//! The content of native code (assembly, C) the package's build scripts
//! compile: each global symbol by the tagged hash of the object defining it.

use std::collections::HashMap;
use std::path::PathBuf;

const NATIVE: &str = "cargo-mutamancer 2026-10-18 native object v1";

/// Each global symbol the archives define, by its object's hash.
pub fn symbols(archives: &[PathBuf]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for a in archives {
        let bytes = std::fs::read(a).unwrap_or_else(|e| panic!("reading {}: {e}", a.display()));
        let archive = object::read::archive::ArchiveFile::parse(&*bytes).unwrap_or_else(|e| panic!("{}: not an archive: {e}", a.display()));
        for member in archive.members() {
            let member = member.expect("an archive member");
            let data = member.data(&*bytes).expect("a member's bytes");
            let mut h = blake3::Hasher::new_derive_key(NATIVE);
            h.update(&(data.len() as u64).to_le_bytes());
            h.update(data);
            let hash = h.finalize().to_hex().to_string();
            if let Ok(file) = object::File::parse(data) {
                use object::{Object, ObjectSymbol};
                for s in file.symbols().filter(|s| s.is_definition() && s.is_global()) {
                    if let Ok(n) = s.name() {
                        // Mach-O names carry a leading underscore rustc's omit.
                        out.insert(n.strip_prefix('_').unwrap_or(n).to_string(), hash.clone());
                        out.insert(n.to_string(), hash.clone());
                    }
                }
            }
        }
    }
    out
}
