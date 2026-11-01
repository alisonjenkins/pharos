//! R4: every served method goes through `Instrumented<S>`. capnpc's
//! `pharos_capnp::<iface>::Server` traits are public, so nothing in the type
//! system stops another crate implementing one and skipping the metrics,
//! spans and redaction. This test does: it fails on any `impl … Server for`
//! in a workspace crate that names `pharos_capnp`. Implement
//! `server::<iface>::Handler` instead. Disarm check: adding such an impl to
//! any `crates/*/src` file turns it red.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Whether `source` hand-implements a capnpc `Server` trait.
fn implements_raw_server(source: &str) -> bool {
    source.contains("pharos_capnp")
        && source.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("impl") && l.contains("Server for")
        })
}

#[test]
fn the_scan_recognises_a_raw_server_impl() {
    let raw = "use pharos_api::pharos_capnp::library;\nimpl library::Server for Lib {}\n";
    assert!(implements_raw_server(raw));
    let wrapped =
        "use pharos_api::pharos_capnp::library;\nimpl server::library::Handler for Lib {}\n";
    assert!(!implements_raw_server(wrapped));
}

#[test]
fn no_crate_outside_pharos_api_implements_a_capnp_server_trait() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates dir").flatten() {
        if entry.file_name() != "pharos-api" {
            rust_files(&entry.path().join("src"), &mut files);
            rust_files(&entry.path().join("tests"), &mut files);
        }
    }
    assert!(
        !files.is_empty(),
        "scanned nothing under {}",
        crates.display()
    );
    let offenders: Vec<_> = files
        .iter()
        .filter(|f| std::fs::read_to_string(f).is_ok_and(|s| implements_raw_server(&s)))
        .collect();
    assert!(
        offenders.is_empty(),
        "implement server::<iface>::Handler, not the raw capnp Server trait: {offenders:?}"
    );
}
