//! The device token and the user it was issued to, in ONE file.
//!
//! The first line is the token and the optional second is the user's id.
//! `pharos-content` reads the same file for the car and takes the first line
//! only. One file, replaced whole by a temp file and a rename, means a user
//! id can never sit beside another sign-in's token: whatever replaces the
//! token replaces the id with it, and a replacement that fails changes
//! neither. A token with no user id (the older layout kept the id in a
//! sidecar file, which is never trusted) is a token whose user is unknown.
//! Never logs a value.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// What the file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub token: String,
    pub user: Option<String>,
}

/// `None` for no token. A user id is the second line, when there is one.
fn decode(raw: &str) -> Option<Stored> {
    let mut lines = raw.lines();
    let token = lines.next()?.trim().to_owned();
    if token.is_empty() {
        return None;
    }
    let user = lines
        .next()
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty());
    Some(Stored { token, user })
}

fn encode(token: &str, user: Option<&str>) -> String {
    match user {
        Some(u) => format!("{token}\n{u}\n"),
        None => format!("{token}\n"),
    }
}

/// The stored token and its user; `None` when the file is absent or holds no
/// token.
pub fn read(path: &Path) -> Result<Option<Stored>, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(decode(&raw)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Writes the file whole to a temp file of its own (not one any other writer
/// of the directory uses), fsyncs it, and renames it over `path`, so a crash
/// or a failure leaves the old file or the new one, never a mixture.
fn write(path: &Path, token: &str, user: Option<&str>) -> std::io::Result<()> {
    if token.contains('\n') || user.is_some_and(|u| u.contains('\n')) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a value holds a line break",
        ));
    }
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".new");
    let tmp = PathBuf::from(tmp_name);
    let go = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(encode(token, user).as_bytes())?;
        f.sync_all()?;
        if f.metadata()?.permissions().mode() & 0o077 != 0 {
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
    };
    go().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A new sign-in's token. Whoever the file held it for is gone with it: the
/// new token's user is unknown until `store_user`.
pub fn store(path: &Path, token: &str) -> Result<(), String> {
    write(path, token, None).map_err(|e| format!("{}: {e}", path.display()))
}

/// Pairs `user` with the token on file. Refused when there is none: an id
/// with no token to belong to is nobody's.
pub fn store_user(path: &Path, user: &str) -> Result<(), String> {
    match read(path)? {
        Some(s) => {
            write(path, &s.token, Some(user)).map_err(|e| format!("{}: {e}", path.display()))
        }
        None => Err("no token to keep the user with".to_owned()),
    }
}

/// The file the older layout kept the user id in. Never read; removed when
/// found, because the id it holds is not known to belong to the token.
pub fn legacy_user_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".user");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("pharos-token-file-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_token_and_its_user_round_trip_privately() {
        let d = dir("round");
        let p = d.join("device-token");
        assert_eq!(read(&p).unwrap(), None);
        store(&p, "tok").unwrap();
        assert_eq!(read(&p).unwrap().unwrap().user, None);
        store_user(&p, "u1").unwrap();
        assert_eq!(
            read(&p).unwrap(),
            Some(Stored {
                token: "tok".into(),
                user: Some("u1".into())
            })
        );
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }

    #[test]
    fn a_plain_token_file_has_no_user() {
        // The older layout, and what the car's own sign-in writes.
        for raw in ["tok", "tok\n", "tok\n\n", "  tok  \n"] {
            let s = decode(raw).unwrap();
            assert_eq!((s.token.as_str(), s.user), ("tok", None), "{raw:?}");
        }
        assert_eq!(decode(""), None);
        assert_eq!(decode("\nu1\n"), None, "a user with no token is nobody");
    }

    #[test]
    fn a_user_cannot_be_kept_with_no_token() {
        let d = dir("none");
        assert!(store_user(&d.join("device-token"), "u1").is_err());
    }

    #[test]
    fn a_legacy_sidecar_is_never_trusted() {
        let d = dir("legacy");
        let p = d.join("device-token");
        std::fs::write(&p, "tokB").unwrap();
        std::fs::write(legacy_user_path(&p), "A").unwrap();
        assert_eq!(read(&p).unwrap().unwrap().user, None);
    }

    #[test]
    fn a_failed_write_leaves_the_old_pair() {
        let d = dir("failed");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        store_user(&p, "A").unwrap();
        // A line break would corrupt the format: refused, nothing changed.
        assert!(store(&p, "to\nk").is_err());
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
        assert!(!d.join("device-token.new").exists());
    }
}
