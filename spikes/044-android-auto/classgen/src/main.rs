//! `pharos-classgen <out-dir>`: writes the spike's stub classes as
//! `<out-dir>/<internal/name>.class`, ready for d8.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(out) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: pharos-classgen <out-dir>");
        return ExitCode::from(2);
    };
    for spec in pharos_classgen::android_auto_classes() {
        let bytes = match pharos_classgen::generate(&spec) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("pharos-classgen: {}: {e}", spec.name);
                return ExitCode::FAILURE;
            }
        };
        let path = out.join(format!("{}.class", spec.name));
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("pharos-classgen: create {}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        }
        if let Err(e) = std::fs::write(&path, &bytes) {
            eprintln!("pharos-classgen: write {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!("{} ({} bytes)", path.display(), bytes.len());
    }
    ExitCode::SUCCESS
}
