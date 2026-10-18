//! `cargo mutamancer`: each commit checked for the reach its change
//! cost (version 0.1: the reach map; mutants come in 0.2).
//!
//!     cargo mutamancer init [OPTIONS] [-- CARGO_TEST_ARGS]
//!     cargo mutamancer check [--accept] [OPTIONS] [-- CARGO_TEST_ARGS]
//!     cargo mutamancer look-for-nondeterminism [OPTIONS] [-- CARGO_TEST_ARGS]
//!     cargo mutamancer install-hook [OPTIONS] [-- CARGO_TEST_ARGS]
//!
//! OPTIONS: `-j N` (workers; one per CPU by default), `--toolchain T`
//! (the nightly that builds with branch coverage; `nightly` by default).

mod ir;
mod items;
mod reach;
mod store;

use items::Item;
use reach::{Binary, Build, Side};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// The map's format; a map of another is refused, never read.
const FORMAT: u32 = 1;

/// The reach map of one context: every item's fingerprint, tokens and
/// sides, and every included test's reach.
#[derive(Serialize, Deserialize)]
struct Map {
    format: u32,
    context: String,
    /// Each crate's id and root file (the lib, each integration test),
    /// relative to the package's directory.
    crates: BTreeMap<String, PathBuf>,
    items: BTreeMap<String, MapItem>,
    tests: BTreeMap<String, MapTest>,
}

#[derive(Serialize, Deserialize, Clone)]
struct MapItem {
    fingerprint: String,
    /// Its tokens' short hashes, to match its sides to a later version's.
    tokens: Vec<u32>,
    /// Its sides, by index: what a test can reach in it.
    sides: Vec<Side>,
}

fn map_item(it: &Item, sides: Option<&Vec<Side>>) -> MapItem {
    MapItem { fingerprint: hex(&it.fingerprint), tokens: it.tokens.iter().map(|t| t.hash).collect(), sides: sides.cloned().unwrap_or_default() }
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
    if args.first().map(String::as_str) == Some("mutamancer") {
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
/// it excludes (`[package.metadata.mutamancer] exclude`).
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
        pkg["metadata"]["mutamancer"]["exclude"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    Package {
        dir: Path::new(pkg["manifest_path"].as_str().unwrap()).parent().unwrap().to_path_buf(),
        target: PathBuf::from(v["target_directory"].as_str().unwrap()).join("mutamancer"),
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

/// Every crate's items, keyed `CRATE::path` (CRATE `lib` or `test:NAME`).
fn all_items(dir: &Path, crates: &BTreeMap<String, PathBuf>) -> BTreeMap<String, Item> {
    let mut out = BTreeMap::new();
    for (id, root) in crates {
        for (k, mut it) in items::crate_items(&dir.join(root)) {
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
/// Measure `chosen` tests (by id; all included when None): build, list,
/// run, read. Returns the binaries' crates, every item, every item's sides,
/// and each measured test's reach.
fn measure(
    o: &Options,
    pkg: &Package,
    chosen: Option<&BTreeSet<String>>,
) -> (BTreeMap<String, PathBuf>, BTreeMap<String, Item>, BTreeMap<String, Vec<Side>>, BTreeMap<String, MapTest>) {
    let t = Instant::now();
    let build = Build { toolchain: o.toolchain.clone(), cargo_args: o.cargo_args.clone(), manifest_dir: pkg.dir.clone(), target_dir: pkg.target.join("build") };
    let bins = build.binaries();
    eprintln!("    Built {} test binaries with coverage in {:.1?}", bins.len(), t.elapsed());
    let crates: BTreeMap<String, PathBuf> = bins.iter().map(|b| (b.id.clone(), b.root.strip_prefix(&pkg.dir).unwrap_or(&b.root).to_path_buf())).collect();
    let items = all_items(&pkg.dir, &crates);
    let mut tests = Vec::new();
    for b in &bins {
        for name in reach::list_tests(b, &pkg.target) {
            let id = test_id(b, &name);
            if !excluded(pkg, &id) && chosen.is_none_or(|c| c.contains(&id)) {
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

/// Whether a test (by id) is one `exclude` names: by libtest's name.
fn excluded(pkg: &Package, id: &str) -> bool {
    let name = id.split_once("::").map(|x| x.1).unwrap_or(id);
    pkg.exclude.iter().any(|p| glob(p, name))
}

fn hex(fp: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*fp).to_hex().to_string()
}

/// A map's meta, as stored beside its parts.
#[derive(Serialize, Deserialize)]
struct Meta {
    format: u32,
    /// The source tree the map measures (git's id).
    tree: String,
    context: String,
    crates: BTreeMap<String, PathBuf>,
    /// BLAKE3 of the items and tests, as serialized: what binds the parts
    /// to this meta.
    content: String,
}

fn content_hash(items: &BTreeMap<String, MapItem>, tests: &BTreeMap<String, MapTest>) -> String {
    blake3::hash(&serde_json::to_vec(&(items, tests)).unwrap()).to_hex().to_string()
}

/// Write `map`, the map of source tree `tree`, into git; its tree.
fn save(store: &store::Store, map: &Map, tree: gix::ObjectId) -> gix::ObjectId {
    let meta = Meta {
        format: FORMAT,
        tree: tree.to_string(),
        context: map.context.clone(),
        crates: map.crates.clone(),
        content: content_hash(&map.items, &map.tests),
    };
    store.write(&store::Parts { meta, items: map.items.clone(), tests: map.tests.clone() })
}

/// The map bound to `commit` in `context`: refused unless it measures that
/// commit's tree, in this context, its parts its meta's.
fn load(store: &store::Store, context: &str, commit: gix::ObjectId) -> Map {
    let id = store
        .note(context, commit)
        .unwrap_or_else(|| panic!("commit {commit} has no map in this context: run `cargo mutamancer init` on a clean checkout of it"));
    let parts: store::Parts<Meta, MapItem, MapTest> = store.read(id);
    let m = parts.meta;
    assert_eq!(m.format, FORMAT, "the map of {commit} is of format {}, this tool reads {FORMAT}: run `cargo mutamancer init` again", m.format);
    assert_eq!(m.tree, store.commit_tree(commit).to_string(), "the map bound to {commit} measures another tree");
    assert_eq!(m.context, context, "the map bound to {commit} has another context");
    assert_eq!(m.content, content_hash(&parts.items, &parts.tests), "the map bound to {commit} does not match its content hash");
    Map { format: m.format, context: m.context, crates: m.crates, items: parts.items, tests: parts.tests }
}

/// The source tree being committed: the index's (`git write-tree`). The
/// check measures the working directory, so it stops where the working
/// directory's tracked files differ from the index, or an untracked Rust
/// file could be built.
fn staged_tree(pkg: &Package) -> gix::ObjectId {
    let git = |args: &[&str]| Command::new("git").args(args).current_dir(&pkg.dir).output().expect("running git");
    let unstaged = git(&["diff", "--name-only"]);
    assert!(
        unstaged.stdout.is_empty(),
        "unstaged changes in {}: stage or stash them, so the check measures what is committed",
        String::from_utf8_lossy(&unstaged.stdout).trim()
    );
    let untracked = git(&["ls-files", "--others", "--exclude-standard", "--", "*.rs"]);
    assert!(
        untracked.stdout.is_empty(),
        "untracked Rust files: {}: add or remove them, so the check measures what is committed",
        String::from_utf8_lossy(&untracked.stdout).trim()
    );
    let out = git(&["write-tree"]);
    assert!(out.status.success(), "git write-tree failed");
    gix::ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim().as_bytes()).expect("a tree id")
}

/// Where the pre-commit check names the map it wrote, for the post-commit
/// hook to bind: the map's tree and the source tree it measures.
fn pending(store: &store::Store) -> PathBuf {
    store.git_dir().join("mutamancer-pending")
}

fn main() {
    let o = options();
    let pkg = package();
    match o.command.as_str() {
        "init" => {
            let store = store::Store::open(&pkg.dir);
            let (head, head_tree) = store.head().expect("a commit to bind the first map to");
            assert_eq!(staged_tree(&pkg), head_tree, "the index differs from HEAD: init measures HEAD, on a clean checkout");
            let (crates, items, sides, tests) = measure(&o, &pkg, None);
            // After the build, which writes Cargo.lock where none was.
            let ctx = context(&o, &pkg);
            let items = items.iter().map(|(k, it)| (k.clone(), map_item(it, sides.get(k)))).collect();
            let map = save(&store, &Map { format: FORMAT, context: ctx.clone(), crates, items, tests }, head_tree);
            store.set_note(&ctx, head, map);
            eprintln!("    Bound the map {map} to {head}");
        }
        "check" => {
            let ctx = context(&o, &pkg);
            std::process::exit(check(&o, &pkg, ctx))
        }
        "bind" => bind(&pkg),
        "look-for-nondeterminism" => std::process::exit(look_for_nondeterminism(&o, &pkg, &context(&o, &pkg))),
        "install-hook" => install_hook(&o),
        other => panic!("a command this tool knows: init, check, look-for-nondeterminism, install-hook (not {other:?})"),
    }
}

/// For each side and index: the tests that reach it.
fn reachers(tests: &BTreeMap<String, MapTest>) -> BTreeMap<(String, u32), BTreeSet<String>> {
    let mut by: BTreeMap<(String, u32), BTreeSet<String>> = BTreeMap::new();
    for (id, t) in tests {
        for (k, sides) in &t.reach {
            for s in sides {
                by.entry((k.clone(), *s)).or_default().insert(id.clone());
            }
        }
    }
    by
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// A list of names for a report: up to four, then how many more.
fn few<T: std::fmt::Display>(names: &[T]) -> String {
    let shown: Vec<String> = names.iter().take(4).map(|n| n.to_string()).collect();
    if names.len() > 4 { format!("{} and {} more", shown.join(", "), names.len() - 4) } else { shown.join(", ") }
}

/// Each new side matched to its earlier version's side of the same kind
/// at the same token, the two versions' tokens aligned (Myers' diff): an
/// index into the old sides, or None for a side in new tokens.
fn match_sides(old: &MapItem, new_tokens: &[u32], new_sides: &[Side]) -> Vec<Option<u32>> {
    let mut new_to_old = vec![None; new_tokens.len() + 1];
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, &old.tokens, new_tokens) {
        if let similar::DiffOp::Equal { old_index, new_index, len } = op {
            for i in 0..len {
                new_to_old[new_index + i] = Some(old_index + i);
            }
        }
    }
    // A side starting at the end of the function (no token) maps to the end.
    new_to_old[new_tokens.len()] = Some(old.tokens.len());
    let by_place: BTreeMap<(u32, u8), u32> = old.sides.iter().enumerate().map(|(i, s)| ((s.token, s.kind), i as u32)).collect();
    new_sides.iter().map(|s| new_to_old[s.token as usize].and_then(|t0| by_place.get(&(t0 as u32, s.kind)).copied())).collect()
}

/// Check the source against the map: report each side that lost its last
/// reaching test, and each side of new code no test reaches, each with its
/// cause. Saves the new map when it finds nothing (or with --accept), so a
/// finding stays until it is fixed or accepted. The exit status: 0 when it
/// finds nothing.
fn check(o: &Options, pkg: &Package, ctx: String) -> i32 {
    let t = Instant::now();
    let store = store::Store::open(&pkg.dir);
    let (head, _) = store.head().expect("a commit to check against: run init on the first");
    let old = load(&store, &ctx, head);
    let tree = staged_tree(pkg);
    let now = all_items(&pkg.dir, &old.crates);
    let changed: BTreeSet<String> =
        now.iter().filter(|(k, it)| old.items.get(*k).is_none_or(|m| m.fingerprint != hex(&it.fingerprint))).map(|(k, _)| k.clone()).collect();
    let gone: BTreeSet<String> = old.items.keys().filter(|k| !now.contains_key(*k)).cloned().collect();
    let removed_tests: BTreeSet<String> = old.tests.keys().filter(|id| !now.get(*id).is_some_and(|it| it.is_test) || excluded(pkg, id)).cloned().collect();
    // Code, not tests: a vanished item that was a test is a removed test.
    let code_changed: BTreeSet<&String> =
        changed.iter().chain(&gone).filter(|k| !now.get(*k).is_some_and(|it| it.is_test) && !old.tests.contains_key(*k)).collect();
    // The tests to measure again: those changed or new, and those whose
    // reach includes changed or vanished code.
    let mut affected: BTreeSet<String> = now.iter().filter(|(k, it)| it.is_test && changed.contains(*k) && !excluded(pkg, k)).map(|(k, _)| k.clone()).collect();
    for (id, test) in &old.tests {
        if !removed_tests.contains(id) && test.reach.keys().any(|k| code_changed.contains(k)) {
            affected.insert(id.clone());
        }
    }
    let mut tests: BTreeMap<String, MapTest> =
        old.tests.iter().filter(|(id, _)| !removed_tests.contains(*id) && !affected.contains(*id)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut sides: BTreeMap<String, Vec<Side>> = old.items.iter().map(|(k, m)| (k.clone(), m.sides.clone())).collect();
    let tier = if affected.is_empty() && code_changed.is_empty() { "lookup" } else { "measured" };
    if tier == "measured" {
        let (_, _, new_sides, measured) = measure(o, pkg, Some(&affected));
        sides = new_sides;
        tests.extend(measured);
    }
    let before = reachers(&old.tests);
    let after = reachers(&tests);
    // Why the tests that reached a side reach it no longer: removed, changed
    // themselves, or run through changed code (these functions).
    let cause = |who: &BTreeSet<String>| {
        let removed: Vec<&String> = who.iter().filter(|t| removed_tests.contains(*t)).collect();
        let changed_tests: Vec<&String> = who.iter().filter(|t| !removed_tests.contains(*t) && changed.contains(*t)).collect();
        let others = who.len() - removed.len() - changed_tests.len();
        let mut beneath: BTreeSet<&String> = BTreeSet::new();
        for t in who.iter().filter(|t| !removed_tests.contains(*t) && !changed.contains(*t)) {
            beneath.extend(old.tests[t].reach.keys().filter(|k| code_changed.contains(k)));
        }
        let mut parts = Vec::new();
        if !removed.is_empty() {
            parts.push(format!("it removes {}", few(&removed)));
        }
        if !changed_tests.is_empty() {
            parts.push(format!("it changes {}", few(&changed_tests)));
        }
        if others > 0 {
            parts.push(format!("it changes code the other {others} reaching it ran through: {}", few(&beneath.into_iter().collect::<Vec<_>>())));
        }
        parts.join("; ")
    };
    // Findings grouped by function and cause: (function, kind, cause) and
    // the sides.
    let mut findings: BTreeMap<(String, &str, String), Vec<String>> = BTreeMap::new();
    // Unchanged functions: each side that lost its last reaching test.
    for ((k, s), who) in &before {
        if changed.contains(k) || gone.contains(k) || after.contains_key(&(k.clone(), *s)) {
            continue;
        }
        findings
            .entry((k.clone(), "reached by no test since this change", cause(who)))
            .or_default()
            .push(reach::side_name(&now[k], &old.items[k].sides[*s as usize]));
    }
    // Changed functions: each side matched to its earlier version's by the
    // functions' tokens. A matched side reached before and by no test now has
    // lost its reach; a side of new tokens no test reaches is new code
    // untested; a side unreached before and after is no effect of this
    // change.
    for k in &changed {
        let (Some(it), Some(new_sides)) = (now.get(k), sides.get(k)) else { continue };
        if it.is_test {
            continue;
        }
        let new_tokens: Vec<u32> = it.tokens.iter().map(|t| t.hash).collect();
        let matched = old.items.get(k).map(|m| match_sides(m, &new_tokens, new_sides)).unwrap_or_else(|| vec![None; new_sides.len()]);
        for (s, side) in new_sides.iter().enumerate() {
            if after.contains_key(&(k.clone(), s as u32)) {
                continue;
            }
            match matched[s] {
                Some(s0) => {
                    if let Some(who) = before.get(&(k.clone(), s0)) {
                        findings.entry((k.clone(), "reached by no test since this change to it", cause(who))).or_default().push(reach::side_name(it, side));
                    }
                }
                None => findings.entry((k.clone(), "new code, reached by no test", String::new())).or_default().push(reach::side_name(it, side)),
            }
        }
    }
    let count: usize = findings.values().map(Vec::len).sum();
    for ((k, what, why), sides) in &findings {
        let why = if why.is_empty() { String::new() } else { format!(": {why}") };
        println!("{k}: {} {what}{why}\n    {}", plural(sides.len(), "side"), few(sides));
    }
    eprintln!(
        "    Checked by {tier} in {:.1?}: {} items changed, {} vanished, {} tests removed, {} measured again; {} findings",
        t.elapsed(),
        changed.len(),
        gone.len(),
        removed_tests.len(),
        affected.len(),
        count
    );
    if findings.is_empty() || o.accept {
        let items = now.iter().map(|(k, it)| (k.clone(), map_item(it, sides.get(k)))).collect();
        let map = save(&store, &Map { format: FORMAT, context: ctx.clone(), crates: old.crates, items, tests }, tree);
        std::fs::write(pending(&store), format!("{map} {tree} {}\n", blake3::hash(ctx.as_bytes()).to_hex())).expect("writing the pending map's name");
        0
    } else {
        1
    }
}

/// Bind the commit just made to the map its pre-commit check wrote (the
/// post-commit hook): refused unless the map measures the commit's tree.
fn bind(pkg: &Package) {
    let store = store::Store::open(&pkg.dir);
    let path = pending(&store);
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    std::fs::remove_file(&path).expect("removing the pending map's name");
    let mut words = text.split_whitespace();
    let (map, tree, _) = (words.next().unwrap(), words.next().unwrap(), words.next().unwrap());
    let (head, head_tree) = store.head().expect("the commit just made");
    assert_eq!(head_tree.to_string(), tree, "the commit's tree is not the one its check measured: run `cargo mutamancer check` on it");
    let parts: store::Parts<Meta, MapItem, MapTest> = store.read(gix::ObjectId::from_hex(map.as_bytes()).unwrap());
    store.set_note(&parts.meta.context, head, gix::ObjectId::from_hex(map.as_bytes()).unwrap());
}

/// Measure every included test again, with nothing they depend on
/// changed, and report each whose reach differs from the map's. It can
/// find nondeterminism, never prove its absence.
fn look_for_nondeterminism(o: &Options, pkg: &Package, ctx: &str) -> i32 {
    let store = store::Store::open(&pkg.dir);
    let (head, head_tree) = store.head().expect("a commit");
    assert_eq!(staged_tree(pkg), head_tree, "the index differs from HEAD: look for nondeterminism on a clean checkout");
    let old = load(&store, ctx, head);
    let now = all_items(&pkg.dir, &old.crates);
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

/// Write the repository's hooks: pre-commit runs `check` with this
/// command's options; post-commit binds the new commit to the map the
/// check wrote. An existing hook this tool did not write is left as it is,
/// and the command stops.
fn install_hook(o: &Options) {
    const MARK: &str = "# written by cargo mutamancer install-hook";
    let mut args = vec!["--toolchain".to_string(), o.toolchain.clone()];
    if !o.cargo_args.is_empty() {
        args.push("--".into());
        args.extend(o.cargo_args.iter().cloned());
    }
    for (hook, command) in [("pre-commit", format!("check {}", args.join(" "))), ("post-commit", "bind".to_string())] {
        let out = Command::new("git").args(["rev-parse", "--git-path", &format!("hooks/{hook}")]).output().expect("running git");
        assert!(out.status.success(), "not in a git repository");
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        if let Ok(old) = std::fs::read_to_string(&path) {
            assert!(old.contains(MARK), "{} exists and is not this tool's: add `cargo mutamancer {command}` to it yourself", path.display());
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("#!/bin/sh\n{MARK}\nexec cargo mutamancer {command}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        eprintln!("    Wrote {}", path.display());
    }
}
