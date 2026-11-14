//! Links OpenBLAS, found by pkg-config (the nix shell and build provide it),
//! and with the `separate` feature finds the converted separator model.
fn main() {
    if pkg_config::Config::new().probe("openblas").is_err() {
        println!("cargo:rustc-link-lib=dylib=openblas");
    }
    if std::env::var_os("CARGO_FEATURE_SEPARATE").is_some() {
        separator_model();
    }
}

/// `PHAROS_SEPARATOR_MODEL` is the flake's `separatorModel`: `htdemucs.rs`,
/// which `src/separate/htdemucs.rs` includes, and `htdemucs.bpk`, whose
/// length is baked in so a truncated weights file is refused before upload.
fn separator_model() {
    println!("cargo:rerun-if-env-changed=PHAROS_SEPARATOR_MODEL");
    let Some(dir) = std::env::var_os("PHAROS_SEPARATOR_MODEL") else {
        eprintln!("the separate feature needs PHAROS_SEPARATOR_MODEL (nix develop exports it)");
        std::process::exit(1);
    };
    let dir = std::path::PathBuf::from(dir);
    let bpk = dir.join("htdemucs.bpk");
    match std::fs::metadata(&bpk) {
        Ok(m) => println!("cargo:rustc-env=PHAROS_SEPARATOR_BPK_LEN={}", m.len()),
        Err(e) => {
            eprintln!("{}: {e}", bpk.display());
            std::process::exit(1);
        }
    }
    println!(
        "cargo:rerun-if-changed={}",
        dir.join("htdemucs.rs").display()
    );
    println!("cargo:rerun-if-changed={}", bpk.display());
}
