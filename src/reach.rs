//! Measuring reach: the test binaries built with LLVM's source coverage
//! (branch coverage on), each chosen test run alone in its own process,
//! and its counters read in process against the binary's coverage map.

use crate::items::Item;
use llvm_profparser::coverage::coverage_mapping::read_object_file;
use llvm_profparser::coverage::{Counter, CounterType, ExprKind, Expression, RegionKind};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A test binary: its target's id (`lib`, or `test:NAME` for an
/// integration test), its crate root, and its path.
#[derive(Clone, Debug)]
pub struct Binary {
    pub id: String,
    pub root: PathBuf,
    pub exe: PathBuf,
}

/// The cargo command the tool builds with: the toolchain and the user's
/// arguments to `cargo test`.
pub struct Build {
    pub toolchain: String,
    pub cargo_args: Vec<String>,
    pub manifest_dir: PathBuf,
    pub target_dir: PathBuf,
}

impl Build {
    /// Build every test binary with coverage; each binary of the package.
    pub fn binaries(&self) -> Vec<Binary> {
        let flags = format!("{} -C instrument-coverage -Z coverage-options=branch", std::env::var("RUSTFLAGS").unwrap_or_default());
        let out = Command::new("cargo")
            .arg(format!("+{}", self.toolchain))
            .args(["test", "--no-run", "--message-format=json-render-diagnostics"])
            .args(&self.cargo_args)
            .env("RUSTFLAGS", flags.trim())
            // Instrumented build scripts write profiles when they run: into
            // the tool's directory, never the source tree.
            .env("LLVM_PROFILE_FILE", self.target_dir.join("build-scripts-%p.profraw"))
            .env("CARGO_TARGET_DIR", &self.target_dir)
            .current_dir(&self.manifest_dir)
            .stderr(std::process::Stdio::inherit())
            .output()
            .expect("running cargo");
        assert!(out.status.success(), "cargo test --no-run failed");
        let mut bins = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if v["reason"] != "compiler-artifact" || v["profile"]["test"] != true || v["executable"].is_null() {
                continue;
            }
            let kind = v["target"]["kind"][0].as_str().unwrap_or("");
            let name = v["target"]["name"].as_str().unwrap_or("");
            let id = if kind == "lib" { "lib".to_string() } else { format!("{kind}:{name}") };
            bins.push(Binary { id, root: PathBuf::from(v["target"]["src_path"].as_str().unwrap()), exe: PathBuf::from(v["executable"].as_str().unwrap()) });
        }
        bins
    }
}

/// The tests a binary runs by default (its ignored ones left out). The
/// profile the instrumented binary writes as it lists goes under `dir`.
pub fn list_tests(bin: &Binary, dir: &Path) -> Vec<String> {
    let list = |extra: &[&str]| -> BTreeSet<String> {
        let out = Command::new(&bin.exe)
            .args(["--list", "--format", "terse"])
            .args(extra)
            .env("LLVM_PROFILE_FILE", dir.join("list-%p.profraw"))
            .output()
            .expect("listing tests");
        String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.strip_suffix(": test")).map(str::to_string).collect()
    };
    let ignored = list(&["--ignored"]);
    list(&[]).into_iter().filter(|t| !ignored.contains(t)).collect()
}

/// A function's branch sides and code regions, in source order: what a
/// test can reach in it. In the map each is known by its index here, which
/// layout and formatting leave as they are.
pub type Sides = Vec<Side>;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Side {
    /// For the reports: its line and column, and its kind.
    pub name: String,
    /// 0 code, 1 a branch's true side, 2 its false side.
    pub kind: u8,
    /// The function's token it starts at: what matches it to its
    /// counterpart in another version of the function.
    pub token: u32,
}

/// Where each counted region of a binary falls: for each function record
/// (by its name hash), the counters to read and the item side each
/// lands on.
pub struct Layout {
    exe: PathBuf,
    version: u64,
    /// Each function record: its name hash, its own expressions (records
    /// may share a hash: copies in several codegen units, unused
    /// functions), and where each of its counters lands.
    places: Vec<(u64, Vec<Expression>, Vec<(Counter, String, u32)>)>,
}

/// The item containing `line` of `file` (`by_file` sorted by first line).
fn item_at<'a>(by_file: &'a HashMap<PathBuf, Vec<&'a Item>>, file: &Path, line: usize) -> Option<&'a Item> {
    by_file.get(file)?.iter().rev().find(|it| it.lines.0 <= line && line <= it.lines.1).copied()
}

/// A region's place in its item: line, column, and kind (0 code, 1 a
/// branch's true side, 2 its false side).
type At = (usize, usize, u8);

/// Every binary's coverage map, read once, against the crates' items:
/// each item's sides, numbered once over every binary's regions (a binary
/// may hold only some of a crate's records), and each binary's layout.
pub fn layouts(bins: &[Binary], items: &BTreeMap<String, Item>, version: u64) -> (BTreeMap<String, Sides>, Vec<Layout>) {
    let mut by_file: HashMap<PathBuf, Vec<&Item>> = HashMap::new();
    for it in items.values() {
        by_file.entry(it.file.canonicalize().unwrap_or(it.file.clone())).or_default().push(it);
    }
    for v in by_file.values_mut() {
        v.sort_by_key(|it| it.lines.0);
    }
    let mut regions: BTreeMap<String, BTreeSet<At>> = BTreeMap::new();
    let mut per_bin = Vec::new();
    for bin in bins {
        let info = read_object_file(&bin.exe, version).unwrap_or_else(|e| panic!("reading the coverage map of {}: {e}", bin.exe.display()));
        let mut placed: Vec<(u64, Vec<Expression>, Vec<(Counter, String, At)>)> = Vec::new();
        for f in &info.cov_fun {
            let paths = info.get_files_from_id(f.header.filenames_ref);
            let mut list = Vec::new();
            for r in &f.regions {
                let kinds: Vec<(u8, Counter)> = match r.kind {
                    RegionKind::Code => vec![(0, r.count.clone())],
                    RegionKind::Branch => vec![(1, r.count.clone()), (2, r.false_count.clone())],
                    _ => continue,
                };
                let Some(path) = paths.get(r.file_id) else { continue };
                let path = path.canonicalize().unwrap_or(path.clone());
                let Some(item) = item_at(&by_file, &path, r.loc.line_start) else { continue };
                for (k, c) in kinds {
                    let at = (r.loc.line_start, r.loc.column_start, k);
                    regions.entry(item.key.clone()).or_default().insert(at);
                    list.push((c, item.key.clone(), at));
                }
            }
            if !list.is_empty() {
                placed.push((f.header.name_hash, f.expressions.clone(), list));
            }
        }
        per_bin.push((bin.exe.clone(), placed));
    }
    let index: BTreeMap<&String, BTreeMap<At, u32>> =
        regions.iter().map(|(k, set)| (k, set.iter().enumerate().map(|(i, at)| (*at, i as u32)).collect())).collect();
    let sides = regions
        .iter()
        .map(|(k, set)| {
            let tokens = &items[k].tokens;
            let sides = set
                .iter()
                .map(|&(line, col, kind)| Side {
                    name: format!("{line}:{col} {}", ["code", "branch true", "branch false"][kind as usize]),
                    kind,
                    token: tokens.iter().position(|t| (t.line, t.col) >= (line, col)).unwrap_or(tokens.len()) as u32,
                })
                .collect();
            (k.clone(), sides)
        })
        .collect();
    let layouts = per_bin
        .into_iter()
        .map(|(exe, placed)| Layout {
            exe,
            version,
            places: placed
                .into_iter()
                .map(|(h, e, list)| {
                    (
                        h,
                        e,
                        list.into_iter()
                            .map(|(c, k, at)| {
                                let i = index[&k][&at];
                                (c, k, i)
                            })
                            .collect(),
                    )
                })
                .collect(),
        })
        .collect();
    (sides, layouts)
}

impl Layout {
    /// What one profile reached: for each item, the indices of its sides.
    pub fn reached(&self, profile: &Path) -> BTreeMap<String, BTreeSet<u32>> {
        let prof = llvm_profparser::parse(profile).unwrap_or_else(|e| panic!("reading {}: {e}", profile.display()));
        assert_eq!(prof.version(), Some(self.version), "{}: a profile of another format version", self.exe.display());
        let mut out: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
        for (hash, exprs, list) in &self.places {
            let Some(rec) = prof.find_record_by_hash(*hash) else { continue };
            let counts = &rec.record.counts;
            let mut memo = HashMap::new();
            for (c, key, i) in list {
                if eval(c, counts, exprs, &mut memo) > 0 {
                    out.entry(key.clone()).or_default().insert(*i);
                }
            }
        }
        out
    }
}

/// A counter's value: zero, a raw count, or an expression over others.
fn eval(c: &Counter, counts: &[u64], exprs: &[Expression], memo: &mut HashMap<u64, u64>) -> u64 {
    match c.kind {
        CounterType::Zero => 0,
        CounterType::ProfileInstrumentation => counts.get(c.id as usize).copied().unwrap_or(0),
        CounterType::Expression(kind) => {
            if let Some(v) = memo.get(&c.id) {
                return *v;
            }
            let e = &exprs[c.id as usize];
            let (l, r) = (eval(&e.lhs, counts, exprs, memo), eval(&e.rhs, counts, exprs, memo));
            let v = match kind {
                ExprKind::Add => l + r,
                ExprKind::Subtract => l.saturating_sub(r),
            };
            memo.insert(c.id, v);
            v
        }
    }
}

/// The profile format version `bin` writes: from the profile it writes
/// when it only lists its tests.
pub fn profile_version(bin: &Binary, dir: &Path) -> u64 {
    std::fs::create_dir_all(dir).expect("the profiles' directory");
    let path = dir.join("version.profraw");
    let _ = std::fs::remove_file(&path);
    Command::new(&bin.exe).arg("--list").env("LLVM_PROFILE_FILE", &path).output().expect("running a test binary");
    llvm_profparser::parse(&path).expect("a profile").version().expect("a profile's version")
}

/// Run each of `tests` (binary, test name) alone, `jobs` at a time, each
/// writing its counters under `dir`; the path of each test's profile.
/// A test that fails stops the run: the map needs passing tests.
pub fn run_tests(tests: &[(Binary, String)], manifest_dir: &Path, dir: &Path, jobs: usize) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).expect("the profiles' directory");
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let failed = Mutex::new(Vec::new());
    let tty = std::io::stderr().is_terminal();
    let paths: Vec<PathBuf> = (0..tests.len()).map(|i| dir.join(format!("{i}.profraw"))).collect();
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= tests.len() {
                        break;
                    }
                    let (bin, name) = &tests[i];
                    let _ = std::fs::remove_file(&paths[i]);
                    let out = Command::new(&bin.exe)
                        .args(["--exact", name, "--test-threads", "1", "-q"])
                        .env("LLVM_PROFILE_FILE", &paths[i])
                        .current_dir(manifest_dir)
                        .output()
                        .expect("running a test");
                    if !out.status.success() {
                        failed.lock().unwrap().push(format!("{} {name}", bin.id));
                    }
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if tty {
                        let mut e = std::io::stderr().lock();
                        let _ = write!(e, "\r   Measuring {n}/{} tests", tests.len());
                        let _ = e.flush();
                    }
                }
            });
        }
    });
    if tty && !tests.is_empty() {
        eprintln!();
    }
    let failed = failed.into_inner().unwrap();
    assert!(failed.is_empty(), "tests fail, and the map needs passing tests: {failed:?}");
    paths
}
