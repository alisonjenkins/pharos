//! The sign-in files the phone app keeps in its private files directory,
//! read here so the car uses the same sign-in. The app writes the token with
//! [`crate::token`]'s format: trimmed UTF-8, written to a temp file, fsynced
//! and renamed, mode 0600.

use std::path::{Path, PathBuf};

/// The device token, one line.
pub const TOKEN_FILE: &str = "device-token";
/// The server address, `https://host[:port]`.
pub const SERVER_FILE: &str = "server";

pub fn token_path(files_dir: &Path) -> PathBuf {
    files_dir.join(TOKEN_FILE)
}

/// The stored server address with trailing slashes removed. `None` when the
/// file is absent, unreadable, or not an `https://` address, so the caller
/// falls back to its default as the app does.
pub fn load_server(files_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(files_dir.join(SERVER_FILE)).ok()?;
    let s = raw.trim().trim_end_matches('/');
    (s.starts_with("https://") && s.len() > "https://".len()).then(|| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn dir(name: &str) -> std::io::Result<PathBuf> {
        let d = std::env::temp_dir().join(format!("pharos-cred-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d)?;
        Ok(d)
    }

    #[test]
    fn the_server_file_is_trimmed_and_must_be_https() -> std::io::Result<()> {
        let d = dir("server")?;
        assert_eq!(load_server(&d), None, "absent");
        for (raw, want) in [
            (
                "https://pharos.example:8443/\n",
                Some("https://pharos.example:8443"),
            ),
            ("  https://h  ", Some("https://h")),
            ("http://h", None),
            ("https://", None),
            ("", None),
        ] {
            std::fs::write(d.join(SERVER_FILE), raw)?;
            assert_eq!(load_server(&d).as_deref(), want, "{raw:?}");
        }
        Ok(())
    }

    #[test]
    fn a_token_the_app_wrote_loads_and_stays_private() -> Result<(), Box<dyn std::error::Error>> {
        let d = dir("token")?;
        let p = token_path(&d);
        assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("device-token"));
        crate::token::store(&p, "tok-1")?;
        let mode = std::fs::metadata(&p)?.permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::write(&p, "tok-2\n")?;
        assert_eq!(crate::token::load(&p)?.as_deref(), Some("tok-2"));
        Ok(())
    }
}
