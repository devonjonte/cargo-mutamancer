//! A crate's functions, each with its key, its place in the source, and
//! its fingerprint: BLAKE3 of its tokens, so layout and comments change
//! nothing.

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// BLAKE3's derive-key context for a function's fingerprint.
const FINGERPRINT_CONTEXT: &str = "cargo-causal-mutation 2026-10-18 Rust fn token fingerprint v1";

/// One function (a free function, a method, or a trait's default method;
/// closures inside it are part of it).
#[derive(Clone, Debug)]
pub struct Item {
    /// Its path within the crate, `module::Type::name` (libtest's name for a
    /// test); a second function of one path (another `cfg`) gains `#2`.
    pub key: String,
    pub file: PathBuf,
    /// The first and last lines of its source, 1-based.
    pub lines: (usize, usize),
    pub fingerprint: [u8; 32],
    pub is_test: bool,
    /// Its tokens in order (doc attributes left out): each one's line and
    /// column (1-based, as LLVM's coverage counts them) and a short hash,
    /// for matching a changed function's sides to its earlier version's.
    pub tokens: Vec<Token>,
}

#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub line: usize,
    pub col: usize,
    pub hash: u32,
}

/// Every function of the crate whose root file is `root`, by key.
pub fn crate_items(root: &Path) -> BTreeMap<String, Item> {
    let mut out = BTreeMap::new();
    let mut seen = BTreeMap::new();
    let dir = root.parent().expect("a root file in a directory").to_path_buf();
    walk_file(root, &dir, &[], &mut out, &mut seen);
    out
}

/// A module's file: its items, and the files of the modules it declares.
/// `dir` is where its child modules' files are.
fn walk_file(file: &Path, dir: &Path, path: &[String], out: &mut BTreeMap<String, Item>, seen: &mut BTreeMap<String, usize>) {
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
    let ast = syn::parse_file(&text).unwrap_or_else(|e| panic!("parsing {}: {e}", file.display()));
    walk_items(&ast.items, file, dir, path, out, seen);
}

fn walk_items(items: &[syn::Item], file: &Path, dir: &Path, path: &[String], out: &mut BTreeMap<String, Item>, seen: &mut BTreeMap<String, usize>) {
    for item in items {
        match item {
            syn::Item::Fn(f) => add(out, seen, path, None, &f.sig.ident.to_string(), &f.attrs, f.to_token_stream(), file),
            syn::Item::Impl(imp) => {
                let ty = type_name(&imp.self_ty);
                let owner = match &imp.trait_ {
                    Some((_, t, _)) => format!("<impl {} for {ty}>", t.to_token_stream()),
                    None => ty,
                };
                for it in &imp.items {
                    if let syn::ImplItem::Fn(f) = it {
                        add(out, seen, path, Some(&owner), &f.sig.ident.to_string(), &f.attrs, f.to_token_stream(), file);
                    }
                }
            }
            syn::Item::Trait(t) => {
                for it in &t.items {
                    if let syn::TraitItem::Fn(f) = it {
                        if f.default.is_some() {
                            add(out, seen, path, Some(&t.ident.to_string()), &f.sig.ident.to_string(), &f.attrs, f.to_token_stream(), file);
                        }
                    }
                }
            }
            syn::Item::Mod(m) => {
                let name = m.ident.to_string();
                let mut child: Vec<String> = path.to_vec();
                child.push(name.clone());
                match &m.content {
                    Some((_, inner)) => walk_items(inner, file, &dir.join(&name), &child, out, seen),
                    None => {
                        let (child_file, child_dir) = module_file(&m.attrs, file, dir, &name);
                        walk_file(&child_file, &child_dir, &child, out, seen);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The file of `mod name;` declared in `file`, a module whose children
/// live in `dir`, and where that module's own children live. A `#[path]`
/// is relative to the declaring file's directory, and the file it names
/// resolves its children as a `mod.rs` does.
fn module_file(attrs: &[syn::Attribute], file: &Path, dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    for a in attrs {
        if a.path().is_ident("path") {
            if let syn::Meta::NameValue(nv) = &a.meta {
                if let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) = &nv.value {
                    let f = file.parent().unwrap().join(s.value());
                    let child_dir = f.parent().unwrap().to_path_buf();
                    return (f, child_dir);
                }
            }
        }
    }
    let flat = dir.join(format!("{name}.rs"));
    if flat.exists() {
        return (flat, dir.join(name));
    }
    (dir.join(name).join("mod.rs"), dir.join(name))
}

/// A test libtest runs by default: `#[test]`, without `#[ignore]`.
fn is_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("test")) && !attrs.iter().any(|a| a.path().is_ident("ignore"))
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default(),
        other => other.to_token_stream().to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn add(
    out: &mut BTreeMap<String, Item>,
    seen: &mut BTreeMap<String, usize>,
    path: &[String],
    owner: Option<&str>,
    name: &str,
    attrs: &[syn::Attribute],
    tokens: TokenStream,
    file: &Path,
) {
    let mut parts: Vec<&str> = path.iter().map(String::as_str).collect();
    if let Some(o) = owner {
        parts.push(o);
    }
    parts.push(name);
    let base = parts.join("::");
    let n = seen.entry(base.clone()).or_insert(0);
    *n += 1;
    let key = if *n == 1 { base } else { format!("{base}#{n}") };
    let span = tokens.clone().into_iter().next().map(|t| t.span()).unwrap();
    let last = tokens.clone().into_iter().last().map(|t| t.span()).unwrap();
    out.insert(key.clone(), {
        let mut list = Vec::new();
        walk_tokens(tokens, &mut list);
        let mut h = blake3::Hasher::new_derive_key(FINGERPRINT_CONTEXT);
        for (_, bytes) in &list {
            h.update(bytes);
        }
        let tokens = list
            .iter()
            .map(|(at, bytes)| Token { line: at.line, col: at.column + 1, hash: u32::from_le_bytes(blake3::hash(bytes).as_bytes()[..4].try_into().unwrap()) })
            .collect();
        Item {
            key,
            file: file.to_path_buf(),
            lines: (span.start().line, last.end().line),
            fingerprint: *h.finalize().as_bytes(),
            is_test: is_test_attr(attrs),
            tokens,
        }
    });
}

/// A function's tokens in order, doc attributes left out (doctests are
/// tests apart): each token's position and its canonical bytes, which the
/// fingerprint hashes in turn. Each kind of token is tagged, and text is
/// length-prefixed, so no two token streams feed the hash alike.
fn walk_tokens(tokens: TokenStream, out: &mut Vec<(proc_macro2::LineColumn, Vec<u8>)>) {
    let mut iter = tokens.into_iter().peekable();
    while let Some(t) = iter.next() {
        // `#[doc = ...]` (a doc comment): `#` then a bracket group whose
        // first token is `doc`.
        if let TokenTree::Punct(p) = &t {
            if p.as_char() == '#' {
                if let Some(TokenTree::Group(g)) = iter.peek() {
                    if g.delimiter() == Delimiter::Bracket && g.stream().into_iter().next().is_some_and(|f| f.to_string() == "doc") {
                        iter.next();
                        continue;
                    }
                }
            }
        }
        match t {
            TokenTree::Group(g) => {
                out.push((g.span_open().start(), vec![b'(', g.delimiter() as u8]));
                walk_tokens(g.stream(), out);
                out.push((g.span_close().start(), vec![b')']));
            }
            TokenTree::Ident(i) => out.push((i.span().start(), text(b'i', &i.to_string()))),
            TokenTree::Punct(p) => out.push((p.span().start(), vec![b'p', p.as_char() as u8, matches!(p.spacing(), Spacing::Joint) as u8])),
            TokenTree::Literal(l) => out.push((l.span().start(), text(b'l', &l.to_string()))),
        }
    }
}

fn text(tag: u8, s: &str) -> Vec<u8> {
    let mut v = vec![tag];
    v.extend_from_slice(&(s.len() as u64).to_le_bytes());
    v.extend_from_slice(s.as_bytes());
    v
}
