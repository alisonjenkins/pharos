//! Links OpenBLAS, found by pkg-config (the nix shell and build provide it).
fn main() {
    if pkg_config::Config::new().probe("openblas").is_err() {
        println!("cargo:rustc-link-lib=dylib=openblas");
    }
}
