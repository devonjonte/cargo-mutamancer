//! The tool is its own compiler wrapper, so it links rustc's own library:
//! record the toolchain it was built with (it checks the user's matches),
//! and find that library at run time by an rpath to the toolchain.
fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let v = std::process::Command::new(&rustc).arg("-vV").output().expect("running rustc");
    let v = String::from_utf8_lossy(&v.stdout).to_string();
    let commit = v.lines().find_map(|l| l.strip_prefix("commit-hash: ")).unwrap_or("").to_string();
    println!("cargo::rustc-env=MUTAMANCER_RUSTC_COMMIT={commit}");
    let sysroot = std::process::Command::new(&rustc).args(["--print", "sysroot"]).output().expect("running rustc");
    let sysroot = String::from_utf8_lossy(&sysroot.stdout).trim().to_string();
    println!("cargo::rustc-link-arg=-Wl,-rpath,{sysroot}/lib");
}
