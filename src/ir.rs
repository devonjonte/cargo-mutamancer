//! Compiled fingerprints: each function by its optimized LLVM IR, as the
//! coverage build emits it (`--emit=llvm-ir`), so that whatever changes
//! what a function does changes its fingerprint: a constant it uses, a
//! type's layout, a macro's expansion, a `cfg`, an inlined callee.
//!
//! Normalized so that only what the function does remains:
//! - metadata attachments (`!noalias !12`) are dropped: they are hints
//!   ("assume this"), which change no defined behavior;
//! - attribute groups (`#0`) are replaced by their content, which their
//!   numbers stand for module by module;
//! - the per-module suffix `.llvm.N` of promoted names is dropped;
//! - private and internal globals are named by their content.
//!
//! An item's fingerprint covers each of its functions (every instance,
//! every closure, in every binary), the content of the globals they
//! reference, and the bodies of the functions they reference that belong
//! to no item (instances of other crates' generics, shims); a function that
//! belongs to an item is referenced by name, since its own fingerprint
//! covers it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

/// One module's definitions: functions and globals by name, their text
/// normalized but for references to private globals (named by content once
/// every definition is known).
struct Module {
    functions: HashMap<String, String>,
    globals: HashMap<String, (bool, String)>,
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
            // `, !name !N` and `!N` (and `!{...}` inline): dropped.
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

fn parse(path: &Path) -> Module {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut attributes = HashMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("attributes #") {
            if let Some((n, body)) = rest.split_once(" = ") {
                attributes.insert(n.to_string(), body.to_string());
            }
        }
    }
    let mut functions = HashMap::new();
    let mut globals = HashMap::new();
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
            functions.insert(strip_suffix(&name), body);
        } else if line.starts_with('@') {
            let name = references(line).first().cloned().unwrap_or_default();
            let private = line.contains(" = private ") || line.contains(" = internal ");
            globals.insert(strip_suffix(&name), (private, normalize(line.split_once(" = ").map(|x| x.1).unwrap_or(line), &attributes)));
        }
    }
    Module { functions, globals }
}

/// The IR files of the binary `exe`, which a build that started at `since`
/// wrote: each codegen unit's, beside it. A file older than the build is a
/// codegen unit rustc reused without writing its IR, and stops the tool:
/// reading it would read another build.
pub fn files(exe: &Path, since: std::time::SystemTime) -> Vec<std::path::PathBuf> {
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

/// Each item's compiled fingerprint, from every module of every binary,
/// given which function symbols belong to which item.
pub fn fingerprints(modules: &[std::path::PathBuf], owner: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mods: Vec<Module> = modules.iter().map(|p| parse(p)).collect();
    let mut per_item: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    for m in &mods {
        let mut memo: HashMap<String, String> = HashMap::new();
        for name in m.functions.keys() {
            if let Some(item) = owner.get(name) {
                let fp = closure(m, owner, name, &mut memo, &mut BTreeSet::new());
                per_item.entry(item.clone()).or_default().insert((name.clone(), fp));
            }
        }
    }
    per_item
        .into_iter()
        .map(|(item, set)| {
            let mut h = blake3::Hasher::new();
            for (name, fp) in &set {
                h.update(name.as_bytes());
                h.update(&[0]);
                h.update(fp.as_bytes());
                h.update(&[0]);
            }
            (item, h.finalize().to_hex().to_string())
        })
        .collect()
}

/// The hash of `name`'s definition in `m` with what it references: globals
/// by content, functions of no item by their own closure, the rest (other
/// items' functions, other modules' symbols) by name. `seen` breaks cycles.
fn closure(m: &Module, owner: &BTreeMap<String, String>, name: &str, memo: &mut HashMap<String, String>, seen: &mut BTreeSet<String>) -> String {
    if let Some(h) = memo.get(name) {
        return h.clone();
    }
    if !seen.insert(name.to_string()) {
        return format!("cycle:{name}");
    }
    let raw = m.functions.get(name).map(String::as_str).or_else(|| m.globals.get(name).map(|g| g.1.as_str())).unwrap_or("");
    let mut refs: Vec<String> = references(raw).into_iter().map(|r| strip_suffix(&r)).filter(|r| r != name).collect();
    refs.sort();
    refs.dedup();
    // Each reference's part: globals by content, functions of no item by
    // their closure, the rest by name. Private globals are then named in the
    // text by their content, longest names first (so `@a.10` is not caught
    // as `@a.1`).
    let parts: Vec<(String, String)> = refs
        .iter()
        .map(|r| {
            let part =
                if m.globals.contains_key(r) || (m.functions.contains_key(r) && !owner.contains_key(r)) { closure(m, owner, r, memo, seen) } else { r.clone() };
            (r.clone(), part)
        })
        .collect();
    let mut text = raw.to_string();
    let mut private: Vec<&(String, String)> = parts.iter().filter(|(r, _)| m.globals.get(r).is_some_and(|g| g.0)).collect();
    private.sort_by_key(|(r, _)| std::cmp::Reverse(r.len()));
    for (r, part) in &private {
        text = text.replace(&format!("@{r}"), &format!("@private:{part}"));
    }
    let mut h = blake3::Hasher::new();
    h.update(text.as_bytes());
    for (r, part) in &parts {
        if m.globals.get(r).is_some_and(|g| g.0) {
            continue;
        }
        h.update(&[1]);
        h.update(r.as_bytes());
        h.update(&[2]);
        h.update(part.as_bytes());
    }
    seen.remove(name);
    let out = h.finalize().to_hex().to_string();
    memo.insert(name.to_string(), out.clone());
    out
}
