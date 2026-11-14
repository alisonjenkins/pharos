//! The device token in a file the caller names (the app's private files
//! directory). Written whole to a temp file, fsynced and renamed, mode 0600,
//! so a crash never leaves half a token. Never logs the value.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::error::ContentError;

fn err(path: &Path, e: impl std::fmt::Display) -> ContentError {
    ContentError::TokenFile {
        path: path.to_owned(),
        why: e.to_string(),
    }
}

/// The stored token; `None` when the file is absent or empty.
pub fn load(path: &Path) -> Result<Option<String>, ContentError> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t.trim().to_owned()).filter(|t| !t.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(path, e)),
    }
}

pub fn store(path: &Path, token: &str) -> Result<(), ContentError> {
    let dir = path.parent().ok_or_else(|| err(path, "no directory"))?;
    std::fs::create_dir_all(dir).map_err(|e| err(dir, e))?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = Path::new(&tmp_name);
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)?;
        f.write_all(token.as_bytes())?;
        f.sync_all()?;
        if f.metadata()?.permissions().mode() & 0o077 != 0 {
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(tmp);
        err(path, e)
    })
}

/// Remove the token; absent is fine.
pub fn forget(path: &Path) -> Result<(), ContentError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err(path, e)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pharos-auto-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_stored_token_loads_back_private_and_forgets() {
        let d = dir("token");
        let p = d.join("files/device-token");
        assert_eq!(load(&p).unwrap(), None);
        store(&p, "secret-1").unwrap();
        store(&p, "secret-2\n").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("secret-2"));
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        forget(&p).unwrap();
        forget(&p).unwrap();
        assert_eq!(load(&p).unwrap(), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unreadable_path_names_the_path() {
        let d = dir("bad");
        std::fs::create_dir_all(&d).unwrap();
        let e = load(&d).expect_err("a directory is not a token");
        assert!(e.to_string().contains(d.to_str().unwrap()), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
