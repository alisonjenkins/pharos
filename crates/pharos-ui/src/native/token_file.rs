//! The device token and the user it was issued to, in ONE file.
//!
//! `pharos_content::token` owns the file whole (ADR 0145): its two-line
//! format, the process-wide lock every write takes, and the atomic
//! replacement. The car's sign-in writes through the same module, so it can
//! never land between a read and a write here. These are thin calls that
//! give the app's string errors. Never logs a value.

use std::path::{Path, PathBuf};

pub use pharos_content::token::{fingerprint, Ended, ForgetIf, Forgotten, Pairing, Stored};

/// The stored token and its user; `None` when the file is absent or holds no
/// token.
pub fn read(path: &Path) -> Result<Option<Stored>, String> {
    pharos_content::token::read(path).map_err(|e| e.to_string())
}

/// A new sign-in's token. Whoever the file held it for is gone with it: the
/// new token's user is unknown until `store_user`.
pub fn store(path: &Path, token: &str) -> Result<(), String> {
    pharos_content::token::store(path, token).map_err(|e| e.to_string())
}

/// Pairs `user` with the token on file, under the file's lock. Refused when
/// there is none: an id with no token to belong to is nobody's.
pub fn store_user(path: &Path, user: &str) -> Result<(), String> {
    pharos_content::token::store_user(path, user).map_err(|e| e.to_string())
}

/// Pairs `user` with the token on file only while it is `seen`, under the
/// file's lock: a token replaced since is left unpaired.
pub fn store_user_if(path: &Path, seen: &str, user: &str) -> Result<Pairing, String> {
    pharos_content::token::store_user_if(path, seen, user).map_err(|e| e.to_string())
}

/// Pairs `user` with the token on file only while it is `seen`, the token
/// the session that learned `user` was resumed with. Err names why not:
/// `no_token_seen` (the session carries none), `token_changed` (a sign-in
/// replaced it since) or the file's error. The phone and its host tests
/// pair through this one function.
pub fn pair(path: &Path, seen: Option<&str>, user: &str) -> Result<(), String> {
    let seen = seen.ok_or_else(|| "no_token_seen".to_owned())?;
    match store_user_if(path, seen, user)? {
        Pairing::Paired => Ok(()),
        Pairing::TokenChanged => Err("token_changed".to_owned()),
    }
}

/// The device token for a download request of `owner`'s run: the token and
/// its user from one read of the file, the token only while that user is
/// `owner` ([`token_for_owner`](super::kept_video::token_for_owner)). Err
/// names the refusal; nothing is sent. The phone's fetcher and its host
/// tests decide through this one function. `signed_out` is the process's
/// sign-out fence: set while a sign-out left the token on file, it refuses
/// every request before the file is read, until a new sign-in lifts it.
pub fn run_token(path: &Path, owner: &str, signed_out: bool) -> Result<String, String> {
    if signed_out {
        return Err(format!("signed_out_token_not_removed owner={owner}"));
    }
    let stored = read(path).map_err(|e| format!("token_unreadable owner={owner} error={e}"))?;
    let (token, user) = stored.map_or((None, None), |s| (Some(s.token), s.user));
    super::kept_video::token_for_owner(token.as_deref(), user.as_deref(), owner)
}

/// Removes the token and its user, under the file's lock; absent is fine.
/// Signed out once it returns Ok, whether or not the removal is durable yet
/// (`Forgotten::NotDurable` says why not).
pub fn forget(path: &Path) -> Result<Forgotten, String> {
    pharos_content::token::forget(path).map_err(|e| e.to_string())
}

/// What a sign-out's removal of the token left on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignedOut {
    /// Gone, and the removal reached the disk (or there was nothing).
    Saved,
    /// Gone for every reader, but the folder could not be synced: a crash
    /// may bring it back.
    SavedNotDurable(String),
    /// The file could not be removed (why), but a synced revocation marker
    /// names it: nothing resumes from it, here or after a restart.
    Marked(String),
    /// The file could not be removed nor marked revoked: the token is still
    /// on file, and a restart would resume the account with it.
    NotSaved(String),
}

/// A sign-out's removal of the token ([`forget`]), as the verdict the app
/// reports: an unlink that failed is kept apart from an unlink whose
/// folder sync failed.
pub fn sign_out(path: &Path) -> SignedOut {
    match end_session(path, None, None) {
        Ended::Forgotten(Forgotten::Durable) | Ended::Changed => SignedOut::Saved,
        Ended::Forgotten(Forgotten::NotDurable(why)) => {
            log::warn!("pharos: token forget verdict=signed_out_not_durable reason={why}");
            SignedOut::SavedNotDurable(why)
        }
        Ended::Marked(why) => {
            log::warn!(
                "pharos: token forget verdict=marked_revoked reason=token_not_removed error={why}"
            );
            SignedOut::Marked(why)
        }
        Ended::Unsafe {
            removal, marker, ..
        } => {
            log::warn!(
                "pharos: token forget verdict=not_removed reason={removal} marker_error={marker}"
            );
            SignedOut::NotSaved(removal)
        }
    }
}

/// Ends a session's hold on the token (`pharos_content::token::end_session`):
/// the token goes while it is `seen` (any with `None`), a replacement is
/// kept, and one that will not go is marked revoked for `user`.
pub fn end_session(path: &Path, seen: Option<&str>, user: Option<&str>) -> Ended {
    pharos_content::token::end_session(path, seen, user)
}

/// Removes the token and its user only while the token on file is `seen`
/// (the refused one), under the file's lock: a token stored since is kept
/// (`ForgetIf::Changed`).
pub fn forget_if(path: &Path, seen: &str) -> Result<ForgetIf, String> {
    pharos_content::token::forget_if(path, seen).map_err(|e| e.to_string())
}

/// The file the older layout kept the user id in. Never read; removed when
/// found, because the id it holds is not known to belong to the token.
pub fn legacy_user_path(path: &Path) -> PathBuf {
    pharos_content::token::legacy_user_path(path)
}

#[cfg(test)]
mod sign_out_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A token that cannot be removed is a sign-out not saved, apart from
    /// one removed whose folder could not be synced; a removed one is
    /// saved.
    #[test]
    fn a_token_that_cannot_be_removed_is_a_sign_out_not_saved() {
        let d = super::super::test_dir::TmpDir::new("sign-out");
        let path = d.path().join("device-token");
        store(&path, "tok").unwrap();
        assert_eq!(sign_out(&path), SignedOut::Saved);
        assert_eq!(read(&path).unwrap(), None);
        // A folder where the file should be: the unlink fails.
        std::fs::create_dir_all(path.join("held")).unwrap();
        assert!(
            matches!(sign_out(&path), SignedOut::NotSaved(_)),
            "an unlink that failed was reported saved"
        );
    }

    /// A download request after a sign-out that left the token on file is
    /// refused by the fence, even though the file still names the owner;
    /// with the fence lifted the same file serves it.
    #[test]
    fn a_fenced_sign_out_refuses_the_download_token() {
        let d = super::super::test_dir::TmpDir::new("run-fence");
        let path = d.path().join("device-token");
        store(&path, "tok").unwrap();
        pair(&path, Some("tok"), "u1").unwrap();
        assert_eq!(
            run_token(&path, "u1", true),
            Err("signed_out_token_not_removed owner=u1".to_owned()),
            "a fenced process sent the signed-out token"
        );
        assert_eq!(run_token(&path, "u1", false), Ok("tok".to_owned()));
    }
}
