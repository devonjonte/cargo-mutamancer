//! Compiled fingerprints, from the compiler itself: the tool runs as cargo's
//! `RUSTC_WORKSPACE_WRAPPER` during the coverage build, and for each
//! function of the package's crates hashes its optimized MIR with rustc's
//! own stable hashing (spans left out; coverage's bookkeeping stripped,
//! whose source hash moves with the lines), together with the fingerprints
//! of the constants and statics it refers to (which MIR names unevaluated,
//! or as pointers to their allocations), so a constant's value or a type's
//! definition is part of every function that uses it. MIR is rustc's own
//! record of what a function does, before LLVM: no inlining (MIR inlining is
//! off in the build), no codegen partitioning, no linkage.
//!
//! Each function's fingerprint is written with its source file and lines,
//! by which the tool assigns it to its item, and with the foreign symbols it
//! calls (assembly, C), whose content the tool takes from the build
//! scripts' archives.

extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_hashes;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_data_structures::stable_hash::{StableHash, StableHasher};
use rustc_hashes::Hash128;
use rustc_hir::def::DefKind;
use rustc_middle::mir::interpret::{AllocId, GlobalAlloc, Scalar};
use rustc_middle::mir::visit::Visitor;
use rustc_middle::mir::{Body, Const, ConstOperand, ConstValue, Location, Operand, Promoted, StatementKind, Terminator, TerminatorKind};
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::def_id::DefId;
use std::collections::{HashMap, HashSet};

/// The environment variables of the wrapper's protocol: the directory each
/// compilation writes its fingerprints into, and the package's directory
/// (only its crates are fingerprinted).
pub const OUT: &str = "MUTAMANCER_COMPILED_OUT";
pub const PACKAGE: &str = "MUTAMANCER_PACKAGE_DIR";

/// One function's fingerprint, as the wrapper writes it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Entry {
    /// Its source file (as rustc names it, relative to the package) and
    /// first and last lines.
    pub file: String,
    pub lines: (usize, usize),
    pub fingerprint: String,
    /// The foreign symbols it calls (by link name).
    pub foreign: Vec<String>,
}

/// Whether this process was started as the compiler wrapper: cargo passes
/// the compiler's path first.
pub fn is_wrapper() -> bool {
    std::env::var_os(OUT).is_some() && std::env::args().nth(1).is_some_and(|a| a.ends_with("rustc") || a.contains("/rustc"))
}

/// The wrapper: compile as rustc would, fingerprinting the package's crates.
pub fn wrap() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let package = std::env::var(PACKAGE).unwrap_or_default();
    let cwd = std::env::current_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
    // A crate of the package: compiled from within its directory.
    let ours = !package.is_empty() && (cwd == package || cwd.starts_with(&format!("{package}/")));
    if !ours {
        let st = std::process::Command::new(&args[0]).args(&args[1..]).status().expect("running rustc");
        std::process::exit(st.code().unwrap_or(1));
    }
    let name = args.windows(2).find(|w| w[0] == "--crate-name").map(|w| w[1].clone()).unwrap_or_default();
    let id = blake3::hash(args.join("\0").as_bytes()).to_hex()[..16].to_string();
    let out = std::path::Path::new(&std::env::var(OUT).unwrap()).join(format!("{name}-{id}.json"));
    let code = rustc_driver::catch_with_exit_code(|| rustc_driver::run_compiler(&args, &mut Fingerprints(out)));
    std::process::exit(if code == std::process::ExitCode::SUCCESS { 0 } else { 1 });
}

struct Fingerprints(std::path::PathBuf);

impl rustc_driver::Callbacks for Fingerprints {
    fn after_analysis(&mut self, _: &rustc_interface::interface::Compiler, tcx: TyCtxt<'_>) -> rustc_driver::Compilation {
        let mut entries = Vec::new();
        let mut memo = HashMap::new();
        let sm = tcx.sess.source_map();
        for def in tcx.mir_keys(()) {
            let def = def.to_def_id();
            if !matches!(tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn | DefKind::Closure) {
                continue;
            }
            let body = tcx.optimized_mir(def);
            let mut foreign = HashSet::new();
            let fp = fingerprint(tcx, body, &mut memo, &mut HashSet::new(), &mut foreign);
            let span = tcx.def_span(def);
            let (lo, hi) = (sm.lookup_char_pos(span.lo()), sm.lookup_char_pos(span.hi()));
            let file = lo.file.name.prefer_local_unconditionally().to_string();
            let mut foreign: Vec<String> = foreign.into_iter().collect();
            foreign.sort();
            entries.push(Entry { file, lines: (lo.line, hi.line), fingerprint: format!("{fp:032x}"), foreign });
        }
        std::fs::write(&self.0, serde_json::to_vec(&entries).unwrap()).expect("writing the fingerprints");
        rustc_driver::Compilation::Continue
    }
}

/// What a body refers to whose content MIR leaves out: this crate's
/// constants and statics (unevaluated, or pointed to), and foreign symbols.
struct Refs<'tcx> {
    tcx: TyCtxt<'tcx>,
    defs: HashSet<(DefId, Option<Promoted>)>,
    foreign: HashSet<String>,
}

impl<'tcx> Refs<'tcx> {
    /// The statics an allocation points to, through every pointer it holds.
    fn alloc(&mut self, id: AllocId, seen: &mut HashSet<AllocId>) {
        if !seen.insert(id) {
            return;
        }
        match self.tcx.global_alloc(id) {
            GlobalAlloc::Static(def) => {
                self.defs.insert((def, None));
            }
            GlobalAlloc::Memory(mem) => {
                for (_, prov) in mem.inner().provenance().ptrs().iter() {
                    self.alloc(prov.alloc_id(), seen);
                }
            }
            _ => {}
        }
    }
}

impl<'tcx> Visitor<'tcx> for Refs<'tcx> {
    fn visit_const_operand(&mut self, c: &ConstOperand<'tcx>, _: Location) {
        match c.const_ {
            Const::Unevaluated(uv, _) => {
                self.defs.insert((uv.def, uv.promoted));
            }
            Const::Val(v, _) => {
                let mut seen = HashSet::new();
                match v {
                    ConstValue::Scalar(Scalar::Ptr(ptr, _)) => self.alloc(ptr.provenance.alloc_id(), &mut seen),
                    ConstValue::Indirect { alloc_id, .. } | ConstValue::Slice { alloc_id, .. } => self.alloc(alloc_id, &mut seen),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn visit_terminator(&mut self, t: &Terminator<'tcx>, l: Location) {
        if let TerminatorKind::Call { func: Operand::Constant(c), .. } = &t.kind {
            if let ty::FnDef(def, _) = *c.const_.ty().kind() {
                if self.tcx.is_foreign_item(def) {
                    self.foreign.insert(self.tcx.symbol_name(ty::Instance::mono(self.tcx, def)).name.to_string());
                }
            }
        }
        self.super_terminator(t, l);
    }
}

fn body_hash<'tcx>(tcx: TyCtxt<'tcx>, body: &Body<'tcx>) -> Hash128 {
    let mut body = body.clone();
    body.coverage_mir_info = None;
    for bb in body.basic_blocks.as_mut_preserves_cfg().iter_mut() {
        bb.statements.retain(|s| !matches!(s.kind, StatementKind::Coverage(..)));
    }
    tcx.with_stable_hashing_context(|mut hcx| {
        let mut hasher = StableHasher::new();
        hcx.while_hashing_spans(false, |hcx| body.stable_hash(hcx, &mut hasher));
        hasher.finish()
    })
}

/// A body's fingerprint with those of the constants and statics it refers
/// to (`memo` by id; `seen` breaks a cycle between them), and the foreign
/// symbols it calls, into `foreign`.
fn fingerprint<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    memo: &mut HashMap<(DefId, Option<Promoted>), u128>,
    seen: &mut HashSet<(DefId, Option<Promoted>)>,
    foreign: &mut HashSet<String>,
) -> u128 {
    let mut refs = Refs { tcx, defs: HashSet::new(), foreign: HashSet::new() };
    refs.visit_body(body);
    foreign.extend(refs.foreign);
    // In a stable order: by each one's DefPathHash, never its DefId.
    let mut defs: Vec<_> = refs.defs.into_iter().collect();
    defs.sort_by_key(|(d, p)| (tcx.def_path_hash(*d).0.split(), p.map(|p| p.as_u32())));
    let mut parts = vec![body_hash(tcx, body).as_u128()];
    for r in defs {
        let (def, promoted) = r;
        if !def.is_local() {
            continue; // another crate's: the map's context fixes it
        }
        let part = if let Some(p) = memo.get(&r) {
            *p
        } else if !seen.insert(r) {
            0 // a cycle: its members' bodies are hashed where it began
        } else {
            let p = match promoted {
                Some(i) => fingerprint(tcx, &tcx.promoted_mir(def)[i], memo, seen, foreign),
                // A foreign item has no body: its content is its object's.
                None if tcx.is_foreign_item(def) => {
                    foreign.insert(tcx.symbol_name(ty::Instance::mono(tcx, def)).name.to_string());
                    0
                }
                None if matches!(tcx.def_kind(def), DefKind::Const { .. } | DefKind::AssocConst { .. } | DefKind::Static { .. } | DefKind::AnonConst) => {
                    fingerprint(tcx, tcx.mir_for_ctfe(def), memo, seen, foreign)
                }
                None => 0,
            };
            seen.remove(&r);
            memo.insert(r, p);
            p
        };
        parts.push(part);
    }
    let mut h = StableHasher::new();
    std::hash::Hasher::write_usize(&mut h, parts.len());
    for p in &parts {
        std::hash::Hasher::write_u128(&mut h, *p);
    }
    h.finish::<Hash128>().as_u128()
}
