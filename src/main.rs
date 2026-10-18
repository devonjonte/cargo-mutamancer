//! `cargo causal-mutation`: each commit checked for the reach its change
//! cost (version 0.1: the reach map; mutants come in 0.2).
//!
//!     cargo causal-mutation init [OPTIONS] [-- CARGO_TEST_ARGS]
//!     cargo causal-mutation check [--accept] [OPTIONS] [-- CARGO_TEST_ARGS]
//!     cargo causal-mutation look-for-nondeterminism [OPTIONS] [-- CARGO_TEST_ARGS]
//!
//! OPTIONS: `-j N` (workers; one per CPU by default), `--toolchain T`
//! (the nightly that builds with branch coverage; `nightly` by default).

mod items;
mod reach;

use items::Item;
use reach::{Binary, Build};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// The map's format; a map of another is refused, never read.
const FORMAT: u32 = 1;

/// The reach map of one context: every item's fingerprint and sides, and
/// every included test's reach.
#[derive(Serialize, Deserialize)]
struct Map {
    format: u32,
    context: String,
    /// Each crate's id and root file (the lib, each integration test).
    crates: BTreeMap<String, PathBuf>,
    items: BTreeMap<String, MapItem>,
    tests: BTreeMap<String, MapTest>,
}

#[derive(Serialize, Deserialize, Clone)]
struct MapItem {
    fingerprint: String,
    /// Its sides, by index: what a test can reach in it.
    sides: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct MapTest {
    /// For each item it reached, the indices of the sides it reached.
    reach: BTreeMap<String, Vec<u32>>,
}

struct Options {
    command: String,
    jobs: usize,
    toolchain: String,
    accept: bool,
    cargo_args: Vec<String>,
}

fn options() -> Options {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("causal-mutation") {
        args.remove(0);
    }
    let mut o = Options {
        command: String::new(),
        jobs: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        toolchain: "nightly".into(),
        accept: false,
        cargo_args: Vec::new(),
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-j" => o.jobs = it.next().and_then(|n| n.parse().ok()).expect("-j takes a number"),
            "--toolchain" => o.toolchain = it.next().expect("--toolchain takes a name"),
            "--accept" => o.accept = true,
            "--" => o.cargo_args = it.by_ref().collect(),
            c if o.command.is_empty() => o.command = c.to_string(),
            other => panic!("an argument this command does not take: {other}"),
        }
    }
    o
}

/// The package: its manifest directory, target directory, and the tests
/// it excludes (`[package.metadata.causal-mutation] exclude`).
struct Package {
    dir: PathBuf,
    target: PathBuf,
    exclude: Vec<String>,
}

fn package() -> Package {
    let out = Command::new("cargo").args(["metadata", "--no-deps", "--format-version", "1"]).output().expect("running cargo metadata");
    assert!(out.status.success(), "cargo metadata failed");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let pkgs = v["packages"].as_array().unwrap();
    let pkg = pkgs.iter().find(|p| Path::new(p["manifest_path"].as_str().unwrap()).parent().unwrap() == cwd).or(pkgs.first()).expect("a package");
    let exclude =
        pkg["metadata"]["causal-mutation"]["exclude"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    Package {
        dir: Path::new(pkg["manifest_path"].as_str().unwrap()).parent().unwrap().to_path_buf(),
        target: PathBuf::from(v["target_directory"].as_str().unwrap()).join("causal-mutation"),
        exclude,
    }
}

/// `*` matches any run of characters; the rest literally.
fn glob(pattern: &str, s: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == s,
        Some((head, rest)) => s.starts_with(head) && (0..=s.len() - head.len()).any(|i| glob(rest, &s[head.len() + i..])),
    }
}

/// The context a map belongs to: what, beside the source, could change
/// what a test reaches on this machine.
fn context(o: &Options, pkg: &Package) -> String {
    let rustc =
        Command::new("rustc").arg(format!("+{}", o.toolchain)).arg("-vV").output().map(|x| String::from_utf8_lossy(&x.stdout).into_owned()).unwrap_or_default();
    let lock =
        std::fs::read(pkg.dir.join("Cargo.lock")).or_else(|_| std::fs::read(pkg.dir.parent().unwrap_or(&pkg.dir).join("Cargo.lock"))).unwrap_or_default();
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|c| c.lines().find(|l| l.starts_with("Features") || l.starts_with("flags")).map(str::to_string))
        .unwrap_or_default();
    format!(
        "{}Cargo.lock {}\nRUSTFLAGS {}\ncargo test {}\ncpu {cpu}\ncpus {}\n",
        rustc,
        blake3::hash(&lock).to_hex(),
        std::env::var("RUSTFLAGS").unwrap_or_default(),
        o.cargo_args.join(" "),
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    )
}

fn map_path(pkg: &Package, context: &str) -> PathBuf {
    pkg.target.join("maps").join(format!("{}.json", &blake3::hash(context.as_bytes()).to_hex()[..16]))
}

/// Every crate's items, keyed `CRATE::path` (CRATE `lib` or `test:NAME`).
fn all_items(crates: &BTreeMap<String, PathBuf>) -> BTreeMap<String, Item> {
    let mut out = BTreeMap::new();
    for (id, root) in crates {
        for (k, mut it) in items::crate_items(root) {
            let key = format!("{id}::{k}");
            it.key = key.clone();
            out.insert(key, it);
        }
    }
    out
}

/// A test's id is its item's key: its crate, then libtest's name.
fn test_id(bin: &Binary, name: &str) -> String {
    format!("{}::{name}", bin.id)
}

/// Measure `chosen` tests (by id; all included when None): build, list,
/// run, read. Returns the binaries' crates, every item's sides, and each
/// measured test's reach.
fn measure(
    o: &Options,
    pkg: &Package,
    chosen: Option<&BTreeSet<String>>,
) -> (BTreeMap<String, PathBuf>, BTreeMap<String, Item>, BTreeMap<String, Vec<String>>, BTreeMap<String, MapTest>) {
    let t = Instant::now();
    let build = Build { toolchain: o.toolchain.clone(), cargo_args: o.cargo_args.clone(), manifest_dir: pkg.dir.clone(), target_dir: pkg.target.join("build") };
    let bins = build.binaries();
    eprintln!("    Built {} test binaries with coverage in {:.1?}", bins.len(), t.elapsed());
    let crates: BTreeMap<String, PathBuf> = bins.iter().map(|b| (b.id.clone(), b.root.clone())).collect();
    let items = all_items(&crates);
    let mut tests = Vec::new();
    for b in &bins {
        for name in reach::list_tests(b) {
            let id = test_id(b, &name);
            let excluded = pkg.exclude.iter().any(|p| glob(p, &name));
            if !excluded && chosen.is_none_or(|c| c.contains(&id)) {
                tests.push((b.clone(), name));
            }
        }
    }
    let t = Instant::now();
    let profiles = reach::run_tests(&tests, &pkg.dir, &pkg.target.join("profiles"), o.jobs);
    let version = reach::profile_version(&bins[0], &pkg.target.join("profiles"));
    let (sides, layouts) = reach::layouts(&bins, &items, version);
    let by_exe: BTreeMap<&Path, &reach::Layout> = bins.iter().map(|b| b.exe.as_path()).zip(layouts.iter()).collect();
    let mut reached = BTreeMap::new();
    for ((bin, name), profile) in tests.iter().zip(&profiles) {
        let r = by_exe[bin.exe.as_path()].reached(profile);
        reached.insert(test_id(bin, name), MapTest { reach: r.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect() });
    }
    eprintln!("    Measured {} tests in {:.1?}", tests.len(), t.elapsed());
    (crates, items, sides, reached)
}

fn hex(fp: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*fp).to_hex().to_string()
}

fn load(path: &Path) -> Map {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| panic!("no map at {}: run `cargo causal-mutation init` first", path.display()));
    let map: Map = serde_json::from_str(&text).expect("a map");
    assert_eq!(map.format, FORMAT, "{}: a map of format {}, this tool reads {FORMAT}", path.display(), map.format);
    map
}

fn save(path: &Path, map: &Map) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let tmp = path.with_extension("json.new");
    std::fs::write(&tmp, serde_json::to_vec(map).unwrap()).unwrap();
    std::fs::rename(tmp, path).unwrap();
}

fn main() {
    let o = options();
    let pkg = package();
    let ctx = context(&o, &pkg);
    let path = map_path(&pkg, &ctx);
    match o.command.as_str() {
        "init" => {
            let (crates, items, sides, tests) = measure(&o, &pkg, None);
            let items = items
                .iter()
                .map(|(k, it)| (k.clone(), MapItem { fingerprint: hex(&it.fingerprint), sides: sides.get(k).cloned().unwrap_or_default() }))
                .collect();
            save(&path, &Map { format: FORMAT, context: ctx, crates, items, tests });
            eprintln!("    Saved the map: {}", path.display());
        }
        "check" => std::process::exit(check(&o, &pkg, &path, ctx)),
        "look-for-nondeterminism" => std::process::exit(look_for_nondeterminism(&o, &pkg, &path)),
        other => panic!("a command this tool knows: init, check, look-for-nondeterminism (not {other:?})"),
    }
}

/// Check the source against the map: report each side that lost its last
/// reaching test, and each side of changed code no test reaches, each with
/// its cause. Saves the new map when it finds nothing (or with --accept),
/// so a finding stays until it is fixed or accepted. The exit status: 0
/// when it finds nothing.
fn check(o: &Options, pkg: &Package, path: &Path, ctx: String) -> i32 {
    let t = Instant::now();
    let old = load(path);
    let now = all_items(&old.crates);
    let changed: BTreeSet<String> =
        now.iter().filter(|(k, it)| old.items.get(*k).is_none_or(|m| m.fingerprint != hex(&it.fingerprint))).map(|(k, _)| k.clone()).collect();
    let gone: BTreeSet<String> = old.items.keys().filter(|k| !now.contains_key(*k)).cloned().collect();
    let is_included = |id: &str| {
        let name = id.split_once("::").map(|x| x.1).unwrap_or(id);
        !pkg.exclude.iter().any(|p| glob(p, name))
    };
    let removed_tests: BTreeSet<String> = old.tests.keys().filter(|id| !now.get(*id).is_some_and(|it| it.is_test) || !is_included(id)).cloned().collect();
    // Code, not tests: a vanished item that was a test is a removed test.
    let code_changed: BTreeSet<&String> =
        changed.iter().chain(&gone).filter(|k| !now.get(*k).is_some_and(|it| it.is_test) && !old.tests.contains_key(*k)).collect();
    // The tests to measure again: those changed or new, and those whose
    // reach includes changed or vanished code.
    let mut affected: BTreeSet<String> = now.iter().filter(|(k, it)| it.is_test && changed.contains(*k) && is_included(k)).map(|(k, _)| k.clone()).collect();
    for (id, test) in &old.tests {
        if !removed_tests.contains(id) && test.reach.keys().any(|k| code_changed.contains(k)) {
            affected.insert(id.clone());
        }
    }
    let mut tests: BTreeMap<String, MapTest> =
        old.tests.iter().filter(|(id, _)| !removed_tests.contains(*id) && !affected.contains(*id)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut sides: BTreeMap<String, Vec<String>> = old.items.iter().map(|(k, m)| (k.clone(), m.sides.clone())).collect();
    let tier = if affected.is_empty() && code_changed.is_empty() { "lookup" } else { "measured" };
    if tier == "measured" {
        let (_, _, new_sides, measured) = measure(o, pkg, Some(&affected));
        sides = new_sides;
        tests.extend(measured);
    }
    // Who reaches each side, before and after.
    let reachers = |tests: &BTreeMap<String, MapTest>| {
        let mut by: BTreeMap<(String, u32), BTreeSet<String>> = BTreeMap::new();
        for (id, t) in tests {
            for (k, sides) in &t.reach {
                for s in sides {
                    by.entry((k.clone(), *s)).or_default().insert(id.clone());
                }
            }
        }
        by
    };
    let before = reachers(&old.tests);
    let after = reachers(&tests);
    let mut findings = Vec::new();
    // Unchanged code that lost its last reaching test.
    for ((k, s), who) in &before {
        if changed.contains(k) || gone.contains(k) || after.contains_key(&(k.clone(), *s)) {
            continue;
        }
        let side = old.items.get(k).and_then(|m| m.sides.get(*s as usize)).cloned().unwrap_or_default();
        let causes: Vec<String> =
            who.iter().map(|t| if removed_tests.contains(t) { format!("removes {t}") } else { format!("changes {t}, which reaches it no longer") }).collect();
        findings.push(format!("{k}, {side}: reached by no test; this change {}", causes.join(", and ")));
    }
    // Changed code no test reaches.
    for k in &changed {
        if now.get(k).is_some_and(|it| it.is_test) {
            continue;
        }
        for (s, side) in sides.get(k).into_iter().flatten().enumerate() {
            if !after.contains_key(&(k.clone(), s as u32)) {
                findings.push(format!("{k}, {side}: changed code, reached by no test"));
            }
        }
    }
    for f in &findings {
        println!("{f}");
    }
    eprintln!(
        "    Checked by {tier} in {:.1?}: {} items changed, {} vanished, {} tests removed, {} measured again; {} findings",
        t.elapsed(),
        changed.len(),
        gone.len(),
        removed_tests.len(),
        affected.len(),
        findings.len()
    );
    if findings.is_empty() || o.accept {
        let items =
            now.iter().map(|(k, it)| (k.clone(), MapItem { fingerprint: hex(&it.fingerprint), sides: sides.get(k).cloned().unwrap_or_default() })).collect();
        save(path, &Map { format: FORMAT, context: ctx, crates: old.crates, items, tests });
        0
    } else {
        1
    }
}

/// Measure every included test again, with nothing they depend on
/// changed, and report each whose reach differs from the map's. It can
/// find nondeterminism, never prove its absence.
fn look_for_nondeterminism(o: &Options, pkg: &Package, path: &Path) -> i32 {
    let old = load(path);
    let now = all_items(&old.crates);
    let changed = now.iter().filter(|(k, it)| old.items.get(*k).is_none_or(|m| m.fingerprint != hex(&it.fingerprint))).count();
    assert_eq!(changed, 0, "the source differs from the map's: check it first, then look for nondeterminism");
    let (_, _, _, measured) = measure(o, pkg, None);
    let mut differing = 0;
    for (id, t) in &measured {
        if let Some(m) = old.tests.get(id) {
            if m.reach != t.reach {
                differing += 1;
                println!("{id}: its reach differs from the map's, with nothing it depends on changed");
            }
        }
    }
    eprintln!("    {} tests measured again; {differing} differ", measured.len());
    if differing == 0 { 0 } else { 1 }
}
