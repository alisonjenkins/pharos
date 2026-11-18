//! The device token file, owned here whole (ADR 0145): the token on the
//! first line and, optionally, the id of the user it was issued to on the
//! second. The car's sign-in and the phone app both go through this module,
//! and every write of the file takes one process-wide lock, so a sign-in can
//! never land between another writer's read and its write (a user id would
//! otherwise sit beside a token it does not belong to, or a new token be
//! replaced by the old one).
//!
//! The file is in a directory the caller names (the app's private files
//! directory), replaced whole by a temp file of its own, fsynced and renamed
//! (mode 0600), with the directory synced after, so a crash leaves the old
//! file or the new one, never a mixture. A token with no user id (the older
//! layout kept the id in a sidecar file, which is never trusted) is a token
//! whose user is unknown. Never logs a value.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::error::ContentError;

/// Held across every replacement and read-modify-write of a token file in
/// this process.
static FILE: Mutex<()> = Mutex::new(());

fn hold() -> MutexGuard<'static, ()> {
    FILE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn err(path: &Path, e: impl std::fmt::Display) -> ContentError {
    ContentError::TokenFile {
        path: path.to_owned(),
        why: e.to_string(),
    }
}

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
/// token. A replacement is a rename, so a read sees one whole file.
pub fn read(path: &Path) -> Result<Option<Stored>, ContentError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(decode(&raw)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(path, e)),
    }
}

/// The stored token alone; `None` when the file is absent or empty.
pub fn load(path: &Path) -> Result<Option<String>, ContentError> {
    Ok(read(path)?.map(|s| s.token))
}

/// A new sign-in's token, the file replaced whole. Whoever the file held it
/// for is gone with it: the new token's user is unknown until `store_user`.
/// Surrounding whitespace is not part of a token.
pub fn store(path: &Path, token: &str) -> Result<(), ContentError> {
    let _held = hold();
    write(path, token.trim(), None).map_err(|e| err(path, e))
}

/// Pairs `user` with the token on file, read and written under the lock.
/// Refused when there is none: an id with no token to belong to is nobody's.
pub fn store_user(path: &Path, user: &str) -> Result<(), ContentError> {
    let _held = hold();
    let stored = read(path)?;
    #[cfg(test)]
    tests::between_read_and_write();
    match stored {
        Some(s) => write(path, &s.token, Some(user)).map_err(|e| err(path, e)),
        None => Err(err(path, "no token to keep the user with")),
    }
}

/// What [`store_user_if`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pairing {
    /// The user is now paired with the token.
    Paired,
    /// The token on file is not the one the user was learned with: nothing
    /// was written, and the token stays unpaired.
    TokenChanged,
}

/// Pairs `user` with the token on file only while that token is `seen`, the
/// one the request that named `user` was sent with. The compare and the
/// write are one step under the file's lock. A different token (a new
/// sign-in landed in between) is left as it is, unpaired: its user is not
/// known, so a check that needs one fails closed. No token is refused, as
/// in [`store_user`].
pub fn store_user_if(path: &Path, seen: &str, user: &str) -> Result<Pairing, ContentError> {
    let _held = hold();
    match read(path)? {
        Some(s) if s.token == seen.trim() => write(path, &s.token, Some(user))
            .map(|()| Pairing::Paired)
            .map_err(|e| err(path, e)),
        Some(_) => Ok(Pairing::TokenChanged),
        None => Err(err(path, "no token to keep the user with")),
    }
}

/// What a [`forget`] that removed the file left on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forgotten {
    /// Gone for good: the removal reached the disk, or there was nothing.
    Durable,
    /// Gone, so signed out now, but the folder's entry could not be made
    /// durable: a crash may bring the token back. Why, for the caller to
    /// report.
    NotDurable(String),
}

/// Removes the token and its user; absent is fine. Once removed the token
/// is gone for every reader, whatever the folder sync after it does.
pub fn forget(path: &Path) -> Result<Forgotten, ContentError> {
    let _held = hold();
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Forgotten::Durable),
        Err(e) => return Err(err(path, e)),
    }
    let Some(dir) = path.parent() else {
        return Ok(Forgotten::NotDurable("no directory".to_owned()));
    };
    Ok(match durable_dir(dir) {
        Ok(()) => Forgotten::Durable,
        Err(e) => {
            log::warn!(
                "token file removed but not synced dir={} reason={e} verdict=signed_out_not_durable",
                dir.display()
            );
            Forgotten::NotDurable(e.to_string())
        }
    })
}

/// Syncs `dir`, once more if the first sync fails; EINVAL or ENOTSUP (a
/// file system that cannot sync a folder) counts as done, logged. Err with
/// the second failure.
fn durable_dir(dir: &Path) -> std::io::Result<()> {
    let Err(first) = sync_dir(dir) else {
        return Ok(());
    };
    match sync_dir(dir) {
        Ok(()) => {
            log::info!("token file dir sync verdict=retried first_error={first}");
            Ok(())
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
            ) =>
        {
            log::info!("token file dir sync verdict=unsupported error={e}");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// The file the older layout kept the user id in. Never read; removed when
/// found, because the id it holds is not known to belong to the token.
pub fn legacy_user_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".user");
    PathBuf::from(name)
}

/// Writes the file whole to a temp file of its own (not one any other writer
/// of the directory uses), fsyncs it, renames it over `path` and syncs the
/// directory. Called with the lock held.
///
/// Once the rename has installed the new file it is the file: every reader
/// sees it, so it is the active token. A directory sync that fails after
/// that only leaves the rename less durable (a crash may bring the old file
/// back); it is logged with its reason and the write succeeds, so the
/// caller's memory and hooks follow the file. A failure before the install
/// changes nothing and is an error.
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
    })?;
    if let Err(e) = sync_dir(dir) {
        log::warn!(
            "token file installed but not synced dir={} reason={e}",
            dir.display()
        );
    }
    Ok(())
}

/// Flushes the directory entry of a rename to disk.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::FAIL_DIR_SYNC.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("directory sync failed"));
    }
    #[cfg(test)]
    if let Some(kind) = tests::DIR_SYNC_ERRORS.with(|q| {
        let mut q = q.borrow_mut();
        (!q.is_empty()).then(|| q.remove(0))
    }) {
        return Err(std::io::Error::new(kind, "directory sync failed"));
    }
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::cell::RefCell;

    use super::*;

    thread_local! {
        /// While set, syncing the directory after a rename fails.
        pub(super) static FAIL_DIR_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The next directory syncs fail with these, one each.
        pub(super) static DIR_SYNC_ERRORS: RefCell<Vec<std::io::ErrorKind>> = const { RefCell::new(Vec::new()) };
        /// Runs inside `store_user`, between its read and its write.
        static BETWEEN: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
    }

    pub(super) fn between_read_and_write() {
        BETWEEN.with(|b| {
            if let Some(f) = b.borrow().as_ref() {
                f();
            }
        });
    }

    /// A folder of the test's own, removed when the test ends.
    fn dir(tag: &str) -> crate::testdir::TmpDir {
        crate::testdir::TmpDir::new(&format!("token-{tag}"))
    }

    /// A sign-in that lands while a user id is being paired with the old
    /// token waits for the pairing: the new token survives, with no user
    /// (its own is not known yet), never the old token or the old user.
    #[test]
    fn a_sign_in_during_a_user_pairing_keeps_the_new_token() {
        let d = dir("interleave");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        let sign_in: std::sync::Arc<Mutex<Option<std::thread::JoinHandle<()>>>> =
            std::sync::Arc::default();
        {
            let (p, sign_in) = (p.clone(), sign_in.clone());
            BETWEEN.with(|b| {
                *b.borrow_mut() = Some(Box::new(move || {
                    let p = p.clone();
                    let t = std::thread::spawn(move || store(&p, "tokB").unwrap());
                    // Long enough for an unlocked sign-in to finish first.
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    *sign_in.lock().unwrap() = Some(t);
                }));
            });
        }
        store_user(&p, "A").unwrap();
        BETWEEN.with(|b| *b.borrow_mut() = None);
        sign_in.lock().unwrap().take().unwrap().join().unwrap();
        assert_eq!(
            read(&p).unwrap(),
            Some(Stored {
                token: "tokB".into(),
                user: None
            }),
            "the sign-in was overwritten by the pairing"
        );
    }

    /// A new token stored between the request that named the user and the
    /// pairing (another account's Quick Connect approved meanwhile) is not
    /// paired with that user: the pairing is refused and the new token stays
    /// with no user.
    #[test]
    fn a_pairing_for_a_replaced_token_is_refused() {
        let d = dir("pair-if");
        let p = d.join("device-token");
        store(&p, "tokA").unwrap();
        assert_eq!(store_user_if(&p, "tokA", "A").unwrap(), Pairing::Paired);
        assert_eq!(read(&p).unwrap().unwrap().user.as_deref(), Some("A"));
        store(&p, "tokB").unwrap();
        assert_eq!(
            read(&p).unwrap().unwrap().user,
            None,
            "a new token kept the old user"
        );
        assert_eq!(
            store_user_if(&p, "tokA", "A").unwrap(),
            Pairing::TokenChanged
        );
        assert_eq!(
            read(&p).unwrap(),
            Some(Stored {
                token: "tokB".into(),
                user: None
            }),
            "the new token was paired with the old user"
        );
    }

    /// A rename whose directory entry cannot be flushed has still installed
    /// the new file: the write succeeds (the caller's memory and hooks then
    /// follow the file) and the lost durability is logged with its reason.
    #[test]
    fn a_failed_directory_sync_after_the_install_keeps_the_new_token() {
        let d = dir("dirsync");
        let path = d.join("device-token");
        store(&path, "old").unwrap();
        crate::testlog::start();
        FAIL_DIR_SYNC.with(|f| f.set(true));
        let stored = store(&path, "new");
        FAIL_DIR_SYNC.with(|f| f.set(false));
        assert!(stored.is_ok(), "{stored:?}");
        assert_eq!(load(&path).unwrap().as_deref(), Some("new"));
        let lines = crate::testlog::lines();
        assert!(
            lines.iter().any(|l| l.contains("installed but not synced")
                && l.contains("reason=directory sync failed")),
            "{lines:?}"
        );
    }

    /// A sign-out's removal is synced to the folder: a failed sync is tried
    /// once more, EINVAL or ENOTSUP counts as done, and a sync that still
    /// fails leaves the token gone (signed out) and reports why.
    #[test]
    fn a_forgotten_token_is_synced_and_a_failed_sync_is_reported() {
        use std::io::ErrorKind::{InvalidInput, Other, Unsupported};
        let d = dir("forget-sync");
        let p = d.join("device-token");
        for (errors, durable) in [
            (vec![], true),
            (vec![Other], true),
            (vec![InvalidInput, InvalidInput], true),
            (vec![Other, Unsupported], true),
            (vec![Other, Other], false),
        ] {
            store(&p, "tok").unwrap();
            crate::testlog::start();
            DIR_SYNC_ERRORS.with(|q| *q.borrow_mut() = errors.clone());
            let forgot = forget(&p).unwrap();
            let left = DIR_SYNC_ERRORS.with(|q| q.borrow().len());
            DIR_SYNC_ERRORS.with(|q| q.borrow_mut().clear());
            assert_eq!(read(&p).unwrap(), None, "the token outlived the sign-out");
            assert_eq!(left, 0, "{errors:?}: not every sync was tried");
            if durable {
                assert_eq!(forgot, Forgotten::Durable, "{errors:?}");
            } else {
                assert!(
                    matches!(&forgot, Forgotten::NotDurable(why) if why.contains("directory sync failed")),
                    "{forgot:?}"
                );
                let lines = crate::testlog::lines();
                assert!(
                    lines
                        .iter()
                        .any(|l| l.contains("token file removed but not synced")
                            && l.contains("verdict=signed_out_not_durable")),
                    "{lines:?}"
                );
            }
        }
    }

    #[test]
    fn the_token_and_its_user_round_trip_privately_and_forget() {
        let d = dir("round");
        let p = d.join("files/device-token");
        assert_eq!(read(&p).unwrap(), None);
        store(&p, "secret-1").unwrap();
        store(&p, "secret-2\n").unwrap();
        assert_eq!(load(&p).unwrap().as_deref(), Some("secret-2"));
        assert_eq!(read(&p).unwrap().unwrap().user, None);
        store_user(&p, "u1").unwrap();
        assert_eq!(
            read(&p).unwrap(),
            Some(Stored {
                token: "secret-2".into(),
                user: Some("u1".into())
            })
        );
        assert_eq!(load(&p).unwrap().as_deref(), Some("secret-2"));
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        forget(&p).unwrap();
        forget(&p).unwrap();
        assert_eq!(load(&p).unwrap(), None);
    }

    #[test]
    fn a_plain_token_file_has_no_user() {
        // The older layout, and a file written by hand.
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

    #[test]
    fn an_unreadable_path_names_the_path() {
        let d = dir("bad");
        let e = load(&d).expect_err("a directory is not a token");
        assert!(e.to_string().contains(d.to_str().unwrap()), "{e}");
    }
}
