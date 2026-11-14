//! `pharos-classgen [--lib <name>] <out-dir>`: writes the Android entry-point
//! classes as `<out-dir>/<internal/name>.class`, ready for d8. `--lib` names
//! the native library they load (default `main`, dx's `libmain.so`).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: pharos-classgen [--lib <name>] <out-dir>";

struct Args {
    lib: String,
    out: PathBuf,
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Args, String> {
    let mut lib = pharos_classgen::DEFAULT_LIB.to_owned();
    let mut out = None;
    while let Some(arg) = args.next() {
        if arg == "--lib" {
            let name = args.next().ok_or("--lib needs a library name")?;
            lib = name
                .into_string()
                .map_err(|n| format!("--lib {n:?} is not UTF-8"))?;
            if lib.is_empty() {
                return Err("--lib needs a library name".into());
            }
        } else if out.is_none() {
            out = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unexpected argument {arg:?}"));
        }
    }
    Ok(Args {
        lib,
        out: out.ok_or("missing <out-dir>")?,
    })
}

fn main() -> ExitCode {
    let args = match parse(std::env::args_os().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("pharos-classgen: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    for spec in pharos_classgen::android_auto_classes(&args.lib) {
        let bytes = match pharos_classgen::generate(&spec) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("pharos-classgen: {}: {e}", spec.name);
                return ExitCode::FAILURE;
            }
        };
        let path = args.out.join(format!("{}.class", spec.name));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse(list.iter().map(OsString::from))
    }

    #[test]
    fn lib_defaults_to_main() -> Result<(), String> {
        let a = args(&["out"])?;
        assert_eq!((a.lib.as_str(), a.out), ("main", PathBuf::from("out")));
        Ok(())
    }

    #[test]
    fn lib_can_be_named_on_either_side_of_the_out_dir() -> Result<(), String> {
        assert_eq!(
            args(&["--lib", "pharos_android", "out"])?.lib,
            "pharos_android"
        );
        assert_eq!(args(&["out", "--lib", "x"])?.lib, "x");
        Ok(())
    }

    #[test]
    fn bad_arguments_are_refused() {
        assert!(args(&[]).is_err());
        assert!(args(&["out", "--lib"]).is_err());
        assert!(args(&["--lib", "", "out"]).is_err());
        assert!(args(&["a", "b"]).is_err());
    }
}
