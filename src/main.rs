//! `cargo mutamancer`: each commit checked for exactly what it changed.
//!
//!     cargo mutamancer init [OPTIONS] [-- CARGO_TEST_ARGS]
//!     cargo mutamancer check [--accept] [OPTIONS] [-- CARGO_TEST_ARGS]
//!     cargo mutamancer bind
//!     cargo mutamancer look-for-nondeterminism [OPTIONS] [-- CARGO_TEST_ARGS]
//!     cargo mutamancer install-hook [OPTIONS] [-- CARGO_TEST_ARGS]
//!
//! OPTIONS: `-j N` (workers; one per CPU by default), `--toolchain T`
//! (the nightly that builds with branch coverage; `nightly` by default).

#![feature(rustc_private)]

mod compiled;
mod items;
mod native;
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
const FORMAT: u32 = 2;

/// The map of one source tree in one context.
struct Map {
    context: String,
    /// Each crate's id and root file (the lib, each integration test),
    /// relative to the package's directory.
    crates: BTreeMap<String, PathBuf>,
    /// Each module's fingerprint of its tokens outside functions.
    others: BTreeMap<String, String>,
    items: BTreeMap<String, MapItem>,
    tests: BTreeMap<String, MapTest>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct MapItem {
    /// Of its tokens (layout, comments and docs left out).
    fingerprint: String,
    /// Of its compiled code (ir.rs).
    compiled: String,
    /// Its sides, by index: what a test can reach in it.
    sides: Vec<Side>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct MapTest {
    /// For each item it reached, the indices of the sides it reached.
    reach: BTreeMap<String, Vec<u32>>,
    /// What it read (files, and directories it listed), each by its hash
    /// (read_hash): a path in the package relative to it, others absolute.
    reads: BTreeMap<String, String>,
}

/// A map's meta, as stored beside its parts.
#[derive(Serialize, Deserialize)]
struct Meta {
    format: u32,
    /// The source tree the map measures (git's id).
    tree: String,
    context: String,
    crates: BTreeMap<String, PathBuf>,
    others: BTreeMap<String, String>,
    /// BLAKE3 of the items and tests, as serialized: what binds the parts
    /// to this meta.
    content: String,
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

/// Every crate's items, keyed `CRATE::path` (CRATE `lib` or `test:NAME`),
/// each module's fingerprint outside functions (`CRATE::module`), and every
/// file read.
fn all_items(dir: &Path, crates: &BTreeMap<String, PathBuf>) -> (BTreeMap<String, Item>, BTreeMap<String, String>, BTreeSet<PathBuf>) {
    let (mut items, mut others, mut files) = (BTreeMap::new(), BTreeMap::new(), BTreeSet::new());
    for (id, root) in crates {
        let c = items::crate_items(&dir.join(root));
        for (k, mut it) in c.items {
            let key = format!("{id}::{k}");
            it.key = key.clone();
            files.insert(it.file.strip_prefix(dir).unwrap_or(&it.file).to_path_buf());
            items.insert(key, it);
        }
        for (m, fp) in c.others {
            others.insert(format!("{id}::{m}"), fp);
        }
        files.extend(c.files.iter().map(|f| f.strip_prefix(dir).unwrap_or(f).to_path_buf()));
    }
    (items, others, files)
}

/// The tool is the compiler for the package's crates (compiled.rs), so the
/// user's toolchain must be the one it was built with.
fn toolchain_matches(o: &Options) {
    let out = Command::new("rustc").arg(format!("+{}", o.toolchain)).arg("-vV").output().expect("running rustc");
    let v = String::from_utf8_lossy(&out.stdout);
    let commit = v.lines().find_map(|l| l.strip_prefix("commit-hash: ")).unwrap_or("");
    assert_eq!(
        commit,
        env!("MUTAMANCER_RUSTC_COMMIT"),
        "toolchain {} is not the one this tool was built with: install it with that toolchain (`cargo +{} install cargo-mutamancer`)",
        o.toolchain,
        o.toolchain
    );
}

/// A test's id is its item's key: its crate, then libtest's name.
fn test_id(bin: &Binary, name: &str) -> String {
    format!("{}::{name}", bin.id)
}

/// Whether a test (by id) is one `exclude` names: by libtest's name.
fn excluded(pkg: &Package, id: &str) -> bool {
    let name = id.split_once("::").map(|x| x.1).unwrap_or(id);
    pkg.exclude.iter().any(|p| glob(p, name))
}

fn hex(fp: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*fp).to_hex().to_string()
}

/// One build's results: its binaries, each item, each item's sides and
/// compiled fingerprint, each binary's layout, and its tests.
struct Measured {
    crates: BTreeMap<String, PathBuf>,
    items: BTreeMap<String, Item>,
    others: BTreeMap<String, String>,
    sides: BTreeMap<String, Vec<Side>>,
    compiled: BTreeMap<String, String>,
    layouts: Vec<(PathBuf, reach::Layout)>,
    tests: Vec<(Binary, String)>,
}

/// Each item's compiled fingerprint: the fingerprints the compiler wrapper
/// wrote (compiled.rs) of the functions in it (the item itself, its
/// closures; in every crate it was compiled into), with the content of the
/// native symbols they call.
fn compiled_fingerprints(
    files: &[PathBuf],
    pkg_dir: &Path,
    items: &BTreeMap<String, Item>,
    native: &std::collections::HashMap<String, String>,
) -> BTreeMap<String, String> {
    let mut by_file: BTreeMap<PathBuf, Vec<&Item>> = BTreeMap::new();
    for it in items.values() {
        by_file.entry(it.file.canonicalize().unwrap_or(it.file.clone())).or_default().push(it);
    }
    let mut parts: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for f in files {
        let bytes = std::fs::read(f).unwrap_or_else(|e| panic!("{}: no compiled fingerprints ({e}): was its crate compiled by another wrapper?", f.display()));
        let entries: Vec<compiled::Entry> = serde_json::from_slice(&bytes).expect("fingerprints");
        for en in entries {
            let file = pkg_dir.join(&en.file);
            let file = file.canonicalize().unwrap_or(file);
            // The innermost item containing the function (a closure's own).
            let Some(it) = by_file
                .get(&file)
                .and_then(|v| v.iter().filter(|it| it.lines.0 <= en.lines.0 && en.lines.1 <= it.lines.1).min_by_key(|it| it.lines.1 - it.lines.0))
            else {
                continue;
            };
            let mut part = en.fingerprint.clone();
            for f in &en.foreign {
                part.push_str(&format!(" {f}:{}", native.get(f).map(String::as_str).unwrap_or("extern")));
            }
            parts.entry(it.key.clone()).or_default().insert(part);
        }
    }
    parts
        .into_iter()
        .map(|(k, set)| {
            let mut h = blake3::Hasher::new_derive_key("cargo-mutamancer 2026-10-18 item compiled v1");
            for p in &set {
                h.update(&(p.len() as u64).to_le_bytes());
                h.update(p.as_bytes());
            }
            (k, h.finalize().to_hex().to_string())
        })
        .collect()
}

/// Build the test binaries with coverage, the compiler wrapper writing each
/// function's fingerprint, and read what they hold: sides, compiled
/// fingerprints, the included tests.
fn build(o: &Options, pkg: &Package) -> Measured {
    let t = Instant::now();
    let b = Build {
        toolchain: o.toolchain.clone(),
        cargo_args: o.cargo_args.clone(),
        manifest_dir: pkg.dir.clone(),
        target_dir: pkg.target.join("build"),
        fingerprints: pkg.target.join("fingerprints"),
    };
    let built = b.binaries();
    let crates: BTreeMap<String, PathBuf> = built.bins.iter().map(|b| (b.id.clone(), b.root.strip_prefix(&pkg.dir).unwrap_or(&b.root).to_path_buf())).collect();
    let (items, others, _) = all_items(&pkg.dir, &crates);
    let version = reach::profile_version(&built.bins[0], &pkg.target.join("profiles"));
    let (sides, layouts, _) = reach::layouts(&built.bins, &items, version);
    let compiled = compiled_fingerprints(&built.fingerprints, &pkg.dir, &items, &native::symbols(&built.archives));
    let mut tests = Vec::new();
    for bin in &built.bins {
        for name in reach::list_tests(bin, &pkg.target) {
            if !excluded(pkg, &test_id(bin, &name)) {
                tests.push((bin.clone(), name));
            }
        }
    }
    eprintln!("    Built {} test binaries with coverage in {:.1?}", built.bins.len(), t.elapsed());
    let layouts = built.bins.iter().map(|b| b.exe.clone()).zip(layouts).collect();
    Measured { crates, items, others, sides, compiled, layouts, tests }
}

/// Run `chosen` tests of `m` (by id), each alone, and read their reach.
fn run(o: &Options, pkg: &Package, m: &Measured, chosen: &BTreeSet<String>) -> BTreeMap<String, MapTest> {
    let t = Instant::now();
    let tests: Vec<(Binary, String)> = m.tests.iter().filter(|(b, n)| chosen.contains(&test_id(b, n))).cloned().collect();
    let profiles = reach::run_tests(&tests, &pkg.dir, &pkg.target.join("profiles"), o.jobs, &preload(pkg));
    let by_exe: BTreeMap<&Path, &reach::Layout> = m.layouts.iter().map(|(e, l)| (e.as_path(), l)).collect();
    let mut out = BTreeMap::new();
    for ((bin, name), profile) in tests.iter().zip(&profiles) {
        let r = by_exe[bin.exe.as_path()].reached(profile);
        let reads = test_reads(pkg, &profile.with_extension("reads"));
        out.insert(test_id(bin, name), MapTest { reach: r.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(), reads });
    }
    eprintln!("    Measured {} tests in {:.1?}", tests.len(), t.elapsed());
    out
}

fn map_items(m: &Measured) -> BTreeMap<String, MapItem> {
    m.items
        .iter()
        .map(|(k, it)| {
            (
                k.clone(),
                MapItem {
                    fingerprint: hex(&it.fingerprint),
                    compiled: m.compiled.get(k).cloned().unwrap_or_default(),
                    sides: m.sides.get(k).cloned().unwrap_or_default(),
                },
            )
        })
        .collect()
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
        others: map.others.clone(),
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
    Map { context: m.context, crates: m.crates, others: m.others, items: parts.items, tests: parts.tests }
}

/// What a test read, from the log the preloaded library wrote: each file
/// and directory it read or looked for, but those it wrote itself and its
/// process's own state, by hash.
fn test_reads(pkg: &Package, log: &Path) -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut written = BTreeSet::new();
    let mut read = BTreeSet::new();
    for line in text.lines() {
        let Some((kind, path)) = line.split_once(' ') else { continue };
        // The test process's own state (its /proc, its devices) is no input
        // the tool can hash: it is the determinism the user promises.
        if ["/proc/self/", "/proc/thread-self/", "/dev/"].iter().any(|p| path.starts_with(p)) || path == "/proc/self" {
            continue;
        }
        let abs = if Path::new(path).is_absolute() { PathBuf::from(path) } else { pkg.dir.join(path) };
        let key = match abs.strip_prefix(&pkg.dir) {
            Ok(rel) => format!("./{}", rel.display()),
            Err(_) => abs.display().to_string(),
        };
        match kind {
            "w" => {
                written.insert(key);
            }
            "f" | "d" => {
                if !written.contains(&key) {
                    read.insert(key);
                }
            }
            _ => {}
        }
    }
    read.into_iter().map(|k| (k.clone(), read_hash(pkg, &k))).collect()
}

/// A read's immutable identity: a file by its content, a directory by its
/// listing, an absent path as absent; each under its own derive-key context.
fn read_hash(pkg: &Package, key: &str) -> String {
    let path = match key.strip_prefix("./") {
        Some(rel) => pkg.dir.join(rel),
        None => PathBuf::from(key),
    };
    let tagged = |ctx: &str, parts: &[&[u8]]| {
        let mut h = blake3::Hasher::new_derive_key(ctx);
        for p in parts {
            h.update(&(p.len() as u64).to_le_bytes());
            h.update(p);
        }
        h.finalize().to_hex().to_string()
    };
    if path.is_dir() {
        let mut names: Vec<String> =
            std::fs::read_dir(&path).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
        names.sort();
        let parts: Vec<&[u8]> = names.iter().map(|n| n.as_bytes()).collect();
        tagged("cargo-mutamancer 2026-10-18 read directory v1", &parts)
    } else {
        match std::fs::read(&path) {
            Ok(bytes) => tagged("cargo-mutamancer 2026-10-18 read file v1", &[&bytes]),
            Err(_) => tagged("cargo-mutamancer 2026-10-18 read absent v1", &[]),
        }
    }
}

/// The preloaded library that records what tests read (preload.c), built
/// once by the system's C compiler into the tool's directory. Linux alone,
/// for now (`LD_PRELOAD`); macOS needs `DYLD_INSERT_LIBRARIES` with
/// interposing, not yet written.
fn preload(pkg: &Package) -> PathBuf {
    assert!(cfg!(target_os = "linux"), "recording what tests read is written for Linux alone so far (macOS: issue #6)");
    const SOURCE: &str = include_str!("preload.c");
    let path = pkg.target.join(format!("preload-{}.so", &blake3::hash(SOURCE.as_bytes()).to_hex()[..16]));
    if !path.exists() {
        std::fs::create_dir_all(&pkg.target).unwrap();
        let src = pkg.target.join("preload.c");
        std::fs::write(&src, SOURCE).unwrap();
        let tmp = path.with_extension("so.new");
        let st = Command::new("cc").args(["-shared", "-fPIC", "-O2", "-o"]).arg(&tmp).arg(&src).arg("-ldl").status().expect("running cc");
        assert!(st.success(), "building the library that records reads (cc)");
        std::fs::rename(tmp, &path).unwrap();
    }
    path
}

/// The paths the index changes against `HEAD`.
fn changed_paths(pkg: &Package) -> BTreeSet<PathBuf> {
    let out = Command::new("git").args(["diff", "--cached", "--name-only", "--relative", "HEAD"]).current_dir(&pkg.dir).output().expect("running git");
    String::from_utf8_lossy(&out.stdout).lines().map(PathBuf::from).collect()
}

/// Whether a changed file's effects reach the compiled code, where compiled
/// fingerprints see them: Rust, assembly, C, the manifest.
fn compiled_input(p: &Path) -> bool {
    matches!(p.extension().and_then(|x| x.to_str()), Some("rs" | "S" | "s" | "c" | "h" | "toml" | "lock"))
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
    // Started by cargo as the compiler wrapper: compile, fingerprinting.
    if compiled::is_wrapper() {
        compiled::wrap();
    }
    let o = options();
    // The commands that build: the toolchain must be the tool's own.
    if matches!(o.command.as_str(), "init" | "check" | "look-for-nondeterminism") {
        toolchain_matches(&o);
    }
    let pkg = package();
    match o.command.as_str() {
        "init" => {
            let store = store::Store::open(&pkg.dir);
            let (head, head_tree) = store.head().expect("a commit to bind the first map to");
            assert_eq!(staged_tree(&pkg), head_tree, "the index differs from HEAD: init measures HEAD, on a clean checkout");
            let m = build(&o, &pkg);
            let all: BTreeSet<String> = m.tests.iter().map(|(b, n)| test_id(b, n)).collect();
            let tests = run(&o, &pkg, &m, &all);
            // After the build, which writes Cargo.lock where none was.
            let ctx = context(&o, &pkg);
            let map = Map { context: ctx.clone(), crates: m.crates.clone(), others: m.others.clone(), items: map_items(&m), tests };
            let id = save(&store, &map, head_tree);
            store.set_note(&ctx, head, id);
            eprintln!("    Bound the map {id} to {head}");
        }
        "check" => {
            let ctx = context(&o, &pkg);
            std::process::exit(check(&o, &pkg, ctx))
        }
        "bind" => bind(&pkg),
        "look-for-nondeterminism" => std::process::exit(look_for_nondeterminism(&o, &pkg, &context(&o, &pkg))),
        "install-hook" => install_hook(&o),
        other => panic!("a command this tool knows: init, check, bind, look-for-nondeterminism, install-hook (not {other:?})"),
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

/// Check the commit being made (the index's tree) against its parent's
/// map. What it changed is read exactly: removed tests by lookup alone;
/// anything else by one build, whose compiled fingerprints say which
/// functions changed what they do, and then the runs of exactly the tests
/// that ran one of them (or are new, or changed). Reports each side of an
/// unchanged function that lost its last reaching test, beside its cause.
/// (Changed functions are judged by their mutants: the kill map.) Saves
/// the new map when it finds nothing (or with --accept); the exit status
/// is 0 then.
fn check(o: &Options, pkg: &Package, ctx: String) -> i32 {
    let t = Instant::now();
    let store = store::Store::open(&pkg.dir);
    let (head, _) = store.head().expect("a commit to check against: run init on the first");
    let old = load(&store, &ctx, head);
    let tree = staged_tree(pkg);
    let (now, now_others, sources) = all_items(&pkg.dir, &old.crates);
    let changed: BTreeSet<String> =
        now.iter().filter(|(k, it)| old.items.get(*k).is_none_or(|m| m.fingerprint != hex(&it.fingerprint))).map(|(k, _)| k.clone()).collect();
    let gone: BTreeSet<String> = old.items.keys().filter(|k| !now.contains_key(*k)).cloned().collect();
    let removed_tests: BTreeSet<String> = old.tests.keys().filter(|id| !now.get(*id).is_some_and(|it| it.is_test) || excluded(pkg, id)).cloned().collect();
    let paths = changed_paths(pkg);
    let outside: Vec<&PathBuf> = paths.iter().filter(|p| !sources.contains(*p)).collect();
    // Tests whose recorded reads changed: each read hashed again.
    let mut hashes: BTreeMap<String, String> = BTreeMap::new();
    let read_changed: BTreeSet<String> = old
        .tests
        .iter()
        .filter(|(id, _)| !removed_tests.contains(*id))
        .filter(|(_, t)| t.reads.iter().any(|(path, h)| hashes.entry(path.clone()).or_insert_with(|| read_hash(pkg, path)) != h))
        .map(|(id, _)| id.clone())
        .collect();
    // Changed files outside the crates' sources that no test read and that
    // are no compiled input (documents, say) change nothing.
    let compiled_outside = outside.iter().any(|p| compiled_input(p));
    let only_removals =
        changed.is_empty() && gone.iter().all(|k| removed_tests.contains(k)) && now_others == old.others && !compiled_outside && read_changed.is_empty();
    let mut tests: BTreeMap<String, MapTest> = old.tests.iter().filter(|(id, _)| !removed_tests.contains(*id)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut items = old.items.clone();
    let mut affected = BTreeSet::new();
    let mut ran_through: BTreeSet<String> = BTreeSet::new();
    if !only_removals {
        let m = build(o, pkg);
        let new_items = map_items(&m);
        // Functions whose compiled code changed, appeared, or vanished.
        ran_through = new_items.iter().filter(|(k, v)| old.items.get(*k).is_none_or(|o| o.compiled != v.compiled)).map(|(k, _)| k.clone()).collect();
        ran_through.extend(gone.iter().cloned());
        let included: BTreeSet<String> = m.tests.iter().map(|(b, n)| test_id(b, n)).collect();
        affected = included
            .iter()
            .filter(|id| {
                !old.tests.contains_key(*id)
                    || ran_through.contains(*id)
                    || read_changed.contains(*id)
                    || old.tests[*id].reach.keys().any(|k| ran_through.contains(k))
            })
            .cloned()
            .collect();
        for id in &affected {
            tests.remove(id);
        }
        tests.retain(|id, _| included.contains(id));
        tests.extend(run(o, pkg, &m, &affected));
        items = new_items;
    }
    let before = reachers(&old.tests);
    let after = reachers(&tests);
    // Why the tests that reached a side reach it no longer: removed, or run
    // again because they ran code that changed (these functions).
    let cause = |who: &BTreeSet<String>| {
        let removed: Vec<&String> = who.iter().filter(|t| removed_tests.contains(*t)).collect();
        let mut beneath: BTreeSet<&String> = BTreeSet::new();
        for t in who.iter().filter(|t| !removed_tests.contains(*t)) {
            if ran_through.contains(t) {
                beneath.insert(t);
            }
            beneath.extend(old.tests[t].reach.keys().filter(|k| ran_through.contains(*k) && !now.get(*k).is_some_and(|it| it.is_test)));
        }
        let mut parts = Vec::new();
        if !removed.is_empty() {
            parts.push(format!("it removes {}", few(&removed)));
        }
        if !beneath.is_empty() {
            parts.push(format!("it changes {}", few(&beneath.into_iter().collect::<Vec<_>>())));
        }
        parts.join("; ")
    };
    let mut findings: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for ((k, s), who) in &before {
        if changed.contains(k) || gone.contains(k) || after.contains_key(&(k.clone(), *s)) {
            continue;
        }
        findings.entry((k.clone(), cause(who))).or_default().push(reach::side_name(&now[k], &old.items[k].sides[*s as usize]));
    }
    let count: usize = findings.values().map(Vec::len).sum();
    for ((k, why), sides) in &findings {
        println!("{k}: {} reached by no test since this change: {why}\n    {}", plural(sides.len(), "side"), few(sides));
    }
    eprintln!(
        "    Checked {} in {:.1?}: {} functions changed, {} vanished, {} changed compiled, {} tests removed, {} run again; {} findings",
        if only_removals { "by lookup" } else { "by one build" },
        t.elapsed(),
        changed.len(),
        gone.len(),
        ran_through.len(),
        removed_tests.len(),
        affected.len(),
        count
    );
    if findings.is_empty() || o.accept {
        let map = Map { context: ctx.clone(), crates: old.crates.clone(), others: now_others, items, tests };
        let id = save(&store, &map, tree);
        std::fs::write(pending(&store), format!("{id} {tree}\n")).expect("writing the pending map's name");
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
    let (map, tree) = (words.next().expect("the pending map"), words.next().expect("the tree it measures"));
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
    let m = build(o, pkg);
    assert!(map_items(&m) == old.items, "the build differs from the map's: check the commit first, then look for nondeterminism");
    let all: BTreeSet<String> = m.tests.iter().map(|(b, n)| test_id(b, n)).collect();
    let measured = run(o, pkg, &m, &all);
    let mut differing = 0;
    for (id, t) in &measured {
        if old.tests.get(id).is_some_and(|m| m != t) {
            differing += 1;
            println!("{id}: its reach differs from the map's, with nothing it depends on changed");
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
