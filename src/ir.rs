//! Compiled fingerprints: each function by its optimized LLVM IR, as the
//! coverage build emits it (`--emit=llvm-ir`), so that whatever changes
//! what a function does changes its fingerprint: a constant it uses, a
//! type's layout, a macro's expansion, a `cfg`, an inlined callee, the
//! assembly or C it calls.
//!
//! Each kind of content has its immutable identity: the BLAKE3 hash of its
//! content, under a derive-key context naming the kind. Names remain only
//! where a mutable identity is what to compare: a function that belongs to
//! an item (its own fingerprint covers it, and the reach map reruns the
//! tests that run it), a static (a place, whose state changes: its name and
//! its initial content), and symbols of other crates (the map's context
//! fixes what they resolve to).
//!
//! The IR is normalized so that only what a function does remains:
//! - metadata attachments (`!noalias !12`) are dropped: they are hints
//!   ("assume this"), which change no defined behavior;
//! - attribute groups (`#0`) are replaced by their content, which their
//!   numbers stand for module by module;
//! - the per-module suffix `.llvm.N` of promoted names is dropped.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

const FUNCTION: &str = "cargo-mutamancer 2026-10-18 IR function v1";
const CONSTANT: &str = "cargo-mutamancer 2026-10-18 IR constant v1";
const STATIC: &str = "cargo-mutamancer 2026-10-18 IR static v1";
const CYCLE: &str = "cargo-mutamancer 2026-10-18 IR cycle v1";
const NATIVE: &str = "cargo-mutamancer 2026-10-18 native object v1";
const ITEM: &str = "cargo-mutamancer 2026-10-18 item compiled v1";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Function,
    /// A constant whose address no one may depend on (private or internal):
    /// interchangeable with any of equal content, so named by its content.
    Constant,
    /// Any other global: named, with its content.
    Static,
}

struct Symbol {
    kind: Kind,
    text: String,
}

/// A hasher for one kind of content, its parts length-prefixed so no two
/// sequences of parts feed it alike.
struct Tagged(blake3::Hasher);

impl Tagged {
    fn new(context: &str) -> Tagged {
        Tagged(blake3::Hasher::new_derive_key(context))
    }
    fn part(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }
    fn hex(&self) -> String {
        self.0.finalize().to_hex().to_string()
    }
}

/// The names a piece of IR references: `@name` or `@"quoted name"`.
fn references(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'@' {
            if b.get(i + 1) == Some(&b'"') {
                let end = text[i + 2..].find('"').map(|e| i + 2 + e).unwrap_or(b.len());
                out.push(text[i + 2..end].to_string());
                i = end + 1;
                continue;
            }
            let start = i + 1;
            let mut j = start;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b"._$-".contains(&b[j])) {
                j += 1;
            }
            if j > start {
                out.push(text[start..j].to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// A line normalized: metadata attachments and references dropped,
/// attribute groups by content, `.llvm.N` suffixes dropped.
fn normalize(line: &str, attributes: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(line.len());
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'!' => {
                let mut j = i + 1;
                if b.get(j) == Some(&b'{') {
                    let mut depth = 0;
                    while j < b.len() {
                        if b[j] == b'{' {
                            depth += 1;
                        } else if b[j] == b'}' {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        j += 1;
                    }
                } else {
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || b"._".contains(&b[j])) {
                        j += 1;
                    }
                }
                while out.ends_with(' ') || out.ends_with(',') {
                    out.pop();
                }
                i = j;
            }
            b'#' if b.get(i + 1).is_some_and(|c| c.is_ascii_digit()) => {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                out.push('{');
                out.push_str(attributes.get(&line[i + 1..j]).map(String::as_str).unwrap_or(""));
                out.push('}');
                i = j;
            }
            b'.' if line[i..].starts_with(".llvm.") => {
                let mut j = i + 6;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                i = j;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

fn strip_suffix(name: &str) -> String {
    match name.find(".llvm.") {
        Some(i) if name[i + 6..].bytes().all(|c| c.is_ascii_digit()) => name[..i].to_string(),
        _ => name.to_string(),
    }
}

/// One binary's definitions, over all its codegen units: one table, so a
/// reference resolves wherever its symbol was placed.
pub struct Binary {
    symbols: HashMap<String, Symbol>,
    /// Content of each native symbol (assembly or C the build scripts
    /// compiled): its object's tagged hash.
    native: HashMap<String, String>,
}

impl Binary {
    /// Read `modules` (the binary's IR files) and the native archives its
    /// build scripts wrote (`archives`).
    pub fn read(modules: &[PathBuf], archives: &[PathBuf]) -> Binary {
        let mut symbols = HashMap::new();
        for path in modules {
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            let mut attributes = HashMap::new();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("attributes #") {
                    if let Some((n, body)) = rest.split_once(" = ") {
                        attributes.insert(n.to_string(), body.to_string());
                    }
                }
            }
            let mut lines = text.lines();
            while let Some(line) = lines.next() {
                if line.starts_with("define ") {
                    let name = references(line.split('(').next().unwrap_or("")).last().cloned().unwrap_or_default();
                    let mut body = normalize(line, &attributes);
                    for l in lines.by_ref() {
                        body.push('\n');
                        body.push_str(&normalize(l, &attributes));
                        if l == "}" {
                            break;
                        }
                    }
                    symbols.insert(strip_suffix(&name), Symbol { kind: Kind::Function, text: body });
                } else if line.starts_with('@') {
                    let name = references(line).first().cloned().unwrap_or_default();
                    let (head, init) = line.split_once(" = ").unwrap_or((line, ""));
                    let _ = head;
                    let private = init.starts_with("private ") || init.starts_with("internal ");
                    let constant = init.contains(" constant ") || init.starts_with("constant ");
                    let kind = if private && constant { Kind::Constant } else { Kind::Static };
                    symbols.insert(strip_suffix(&name), Symbol { kind, text: normalize(init, &attributes) });
                }
            }
        }
        let mut native = HashMap::new();
        for a in archives {
            let bytes = std::fs::read(a).unwrap_or_else(|e| panic!("reading {}: {e}", a.display()));
            let archive = object::read::archive::ArchiveFile::parse(&*bytes).unwrap_or_else(|e| panic!("{}: not an archive: {e}", a.display()));
            for member in archive.members() {
                let member = member.expect("an archive member");
                let data = member.data(&*bytes).expect("a member's bytes");
                let hash = Tagged::new(NATIVE).part(data).hex();
                if let Ok(file) = object::File::parse(data) {
                    use object::{Object, ObjectSymbol};
                    for s in file.symbols().filter(|s| s.is_definition() && s.is_global()) {
                        if let Ok(n) = s.name() {
                            // Mach-O names carry a leading underscore the IR omits.
                            native.insert(n.trim_start_matches('_').to_string(), hash.clone());
                            native.insert(n.to_string(), hash.clone());
                        }
                    }
                }
            }
        }
        Binary { symbols, native }
    }
}

/// Each item's compiled fingerprint, from every binary, given which
/// function symbols belong to which item.
pub fn fingerprints(bins: &[Binary], owner: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut per_item: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    for bin in bins {
        let content = contents(bin, owner);
        for (name, s) in &bin.symbols {
            if s.kind == Kind::Function {
                if let Some(item) = owner.get(name) {
                    per_item.entry(item.clone()).or_default().insert((name.clone(), content[name].clone()));
                }
            }
        }
    }
    per_item
        .into_iter()
        .map(|(item, set)| {
            let mut h = Tagged::new(ITEM);
            for (name, fp) in &set {
                h.part(name.as_bytes()).part(fp.as_bytes());
            }
            (item, h.hex())
        })
        .collect()
}

/// What stands for a reference to `r` in a fingerprint: a constant by its
/// content, a function of no item by its content, a native symbol by its
/// object's, and everything else (a function of an item, a static, another
/// crate's symbol) by its name; a static's content joins its name.
fn reference_part(bin: &Binary, owner: &BTreeMap<String, String>, r: &str, content: &HashMap<String, String>) -> String {
    match bin.symbols.get(r) {
        Some(s) if s.kind == Kind::Constant => format!("constant:{}", content[r]),
        Some(s) if s.kind == Kind::Function && !owner.contains_key(r) => format!("function:{}", content[r]),
        Some(s) if s.kind == Kind::Static => format!("static:{r}:{}", content[r]),
        Some(_) => format!("item:{r}"),
        None => match bin.native.get(r) {
            Some(h) => format!("native:{r}:{h}"),
            None => format!("extern:{r}"),
        },
    }
}

/// Every symbol's content hash, computed over the reference graph's
/// strongly connected components (Tarjan), in dependency order: a symbol
/// outside any cycle under its kind's context; the members of a cycle
/// together, under the cycle's.
fn contents(bin: &Binary, owner: &BTreeMap<String, String>) -> HashMap<String, String> {
    let names: Vec<&String> = bin.symbols.keys().collect();
    let index: HashMap<&str, usize> = names.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
    let refs: Vec<Vec<String>> = names
        .iter()
        .map(|n| {
            let mut r: Vec<String> = references(&bin.symbols[*n].text).into_iter().map(|r| strip_suffix(&r)).filter(|r| r != *n).collect();
            r.sort();
            r.dedup();
            r
        })
        .collect();
    // Edges only to symbols whose content stands for them (the rest are
    // named, and end the walk).
    let follows = |r: &str| bin.symbols.get(r).is_some_and(|s| s.kind != Kind::Function || !owner.contains_key(r));
    let edges: Vec<Vec<usize>> = refs.iter().map(|rs| rs.iter().filter(|r| follows(r)).map(|r| index[r.as_str()]).collect()).collect();
    let sccs = tarjan(&edges);
    let mut content: HashMap<String, String> = HashMap::new();
    for scc in sccs {
        let members: BTreeSet<usize> = scc.iter().copied().collect();
        // A member's text with its references named: inside the cycle by
        // the member's rank, outside by their parts.
        let render = |i: usize, rank: &HashMap<usize, usize>, content: &HashMap<String, String>| {
            let mut text = bin.symbols[names[i]].text.clone();
            let mut rs: Vec<&String> = refs[i].iter().collect();
            rs.sort_by_key(|r| std::cmp::Reverse(r.len()));
            for r in rs {
                let part = match index.get(r.as_str()) {
                    Some(j) if members.contains(j) => format!("cycle:{}", rank.get(j).copied().unwrap_or(0)),
                    _ => reference_part(bin, owner, r, content),
                };
                text = text.replace(&format!("@{r}"), &format!("@{{{part}}}"));
            }
            text
        };
        if members.len() == 1 && !edges[scc[0]].contains(&scc[0]) {
            let i = scc[0];
            let text = render(i, &HashMap::new(), &content);
            let s = &bin.symbols[names[i]];
            let ctx = match s.kind {
                Kind::Function => FUNCTION,
                Kind::Constant => CONSTANT,
                Kind::Static => STATIC,
            };
            content.insert(names[i].clone(), Tagged::new(ctx).part(text.as_bytes()).hex());
        } else {
            // Ranks by each member's text with every in-cycle reference
            // alike, ties by name: stable across builds.
            let alike: HashMap<usize, usize> = members.iter().map(|&j| (j, 0)).collect();
            let mut order: Vec<usize> = members.iter().copied().collect();
            order.sort_by_key(|&i| (render(i, &alike, &content), names[i].clone()));
            let rank: HashMap<usize, usize> = order.iter().enumerate().map(|(r, &i)| (i, r)).collect();
            let mut h = Tagged::new(CYCLE);
            for &i in &order {
                h.part(render(i, &rank, &content).as_bytes());
            }
            let hash = h.hex();
            for &i in &order {
                content.insert(names[i].clone(), format!("{hash}:{}", rank[&i]));
            }
        }
    }
    content
}

/// The strongly connected components of a graph, each after every
/// component it reaches (so a component's references are hashed first).
fn tarjan(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = edges.len();
    let (mut index, mut low, mut on) = (vec![usize::MAX; n], vec![0; n], vec![false; n]);
    let (mut stack, mut out, mut next) = (Vec::new(), Vec::new(), 0);
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        // An explicit stack of (node, next edge): deep graphs never
        // overflow the thread's stack.
        let mut work = vec![(root, 0)];
        while let Some(&mut (v, ref mut e)) = work.last_mut() {
            if *e == 0 {
                index[v] = next;
                low[v] = next;
                next += 1;
                stack.push(v);
                on[v] = true;
            }
            if *e < edges[v].len() {
                let w = edges[v][*e];
                *e += 1;
                if index[w] == usize::MAX {
                    work.push((w, 0));
                } else if on[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(u, _)) = work.last() {
                low[u] = low[u].min(low[v]);
            }
            if low[v] == index[v] {
                let mut scc = Vec::new();
                loop {
                    let w = stack.pop().unwrap();
                    on[w] = false;
                    scc.push(w);
                    if w == v {
                        break;
                    }
                }
                out.push(scc);
            }
        }
    }
    out
}

/// The IR files of the binary `exe`, which a build that started at `since`
/// wrote: each codegen unit's, beside it. A file older than the build is a
/// codegen unit rustc reused without writing its IR, and stops the tool:
/// reading it would read another build.
pub fn files(exe: &Path, since: std::time::SystemTime) -> Vec<PathBuf> {
    let dir = exe.parent().expect("a binary's directory");
    let stem = exe.file_name().unwrap().to_string_lossy().to_string();
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .expect("the binary's directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "ll") && p.file_name().unwrap().to_string_lossy().starts_with(&format!("{stem}.")))
        .collect();
    out.sort();
    for p in &out {
        let m = std::fs::metadata(p).and_then(|m| m.modified()).expect("an IR file's time");
        assert!(m >= since, "{}: IR from an earlier build (rustc reused this codegen unit without writing its IR)", p.display());
    }
    assert!(!out.is_empty(), "{}: no IR beside it", exe.display());
    out
}
