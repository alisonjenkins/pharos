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
/// tests decide through this one function. `fence` is the process's
/// sign-out fence ([`SignOutFence`]): the token it names is refused, and
/// one that names none refuses every request before the file is read.
pub fn run_token(path: &Path, owner: &str, fence: &SignOutFence) -> Result<String, String> {
    if *fence == SignOutFence::Every {
        return Err(format!("signed_out_token_not_removed owner={owner}"));
    }
    let stored = read(path).map_err(|e| format!("token_unreadable owner={owner} error={e}"))?;
    let (token, user) = stored.map_or((None, None), |s| (Some(s.token), s.user));
    if token.as_deref().is_some_and(|t| fence.refuses(t)) {
        return Err(format!("signed_out_token_not_removed owner={owner}"));
    }
    super::kept_video::token_for_owner(token.as_deref(), user.as_deref(), owner)
}

/// The phone's sign-out fence: set when a sign-out left its token on file
/// (neither removed nor saved). It names that token by fingerprint, so any
/// other token on file, whoever stored it, is read; with no token to name
/// it refuses every token until the phone stores one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SignOutFence {
    /// No sign-out left a token behind.
    #[default]
    Open,
    /// The token with this fingerprint is refused.
    Token(String),
    /// The token left behind could not be named: every token is refused.
    Every,
}

impl SignOutFence {
    /// The fence for a sign-out that left `held` on file.
    pub fn for_token(held: Option<&str>) -> Self {
        held.map_or(Self::Every, |t| Self::Token(fingerprint(t)))
    }

    /// Whether `token` is refused.
    pub fn refuses(&self, token: &str) -> bool {
        match self {
            Self::Open => false,
            Self::Token(fp) => fingerprint(token) == *fp,
            Self::Every => true,
        }
    }

    /// The verdict word for logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Token(_) => "token",
            Self::Every => "every",
        }
    }
    /// The fence once a store installed the token fingerprinted `stored`
    /// over the one fingerprinted `replaced` (when that could be read).
    /// A fence naming another token stays: it refuses only that token. One
    /// naming the stored token, or none, opens. An unnamed one names the
    /// token the store overwrote, if it could be read and differs, and
    /// opens otherwise: the token it could not name is no longer on file.
    pub fn after_store(&self, stored: &str, replaced: Option<&str>) -> Self {
        match self {
            Self::Open => Self::Open,
            Self::Token(fp) if fp != stored => self.clone(),
            Self::Token(_) => Self::Open,
            Self::Every => match replaced {
                Some(old) if old != stored => Self::Token(old.to_owned()),
                _ => Self::Open,
            },
        }
    }
}

#[cfg(not(test))]
static FENCE: std::sync::Mutex<SignOutFence> = std::sync::Mutex::new(SignOutFence::Open);

#[cfg(test)]
thread_local! {
    /// Per test thread: the store hook runs on the thread that stores.
    static FENCE: std::cell::RefCell<SignOutFence> =
        const { std::cell::RefCell::new(SignOutFence::Open) };
}

fn with_fence<T>(f: impl FnOnce(&mut SignOutFence) -> T) -> T {
    #[cfg(not(test))]
    {
        f(&mut FENCE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }
    #[cfg(test)]
    {
        FENCE.with(|c| f(&mut c.borrow_mut()))
    }
}

/// This process's sign-out fence ([`SignOutFence`]). Its first read
/// registers the store hook too, so a token cached from it is marked
/// stale by any later store.
pub fn fence() -> SignOutFence {
    watch_stores();
    with_fence(|f| f.clone())
}

/// Sets this process's sign-out fence, and makes sure every later token
/// store, the car's QuickConnect included, replaces it ([`after_store`]).
pub fn set_fence(next: SignOutFence) {
    watch_stores();
    log::info!("pharos: token fence verdict={}", next.label());
    with_fence(|f| *f = next);
}

/// Set by every token store: the token a cache holds may be another now.
static STALE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a token store ran since the last call.
pub fn take_stale() -> bool {
    STALE.swap(false, std::sync::atomic::Ordering::AcqRel)
}

/// Registers [`after_store`] with `pharos_content::token`, once.
fn watch_stores() {
    pharos_content::token::on_stored(after_store);
}

/// The store hook: replaces the fence as [`SignOutFence::after_store`]
/// says, and marks a cached token stale. Takes only the fence's lock.
fn after_store(stored: &str, replaced: Option<&str>) {
    let (was, now) = with_fence(|f| {
        let was = f.clone();
        *f = f.after_store(stored, replaced);
        (was, f.clone())
    });
    if was != now {
        log::info!(
            "pharos: token fence verdict={} reason=token_stored was={}",
            now.label(),
            was.label()
        );
    }
    STALE.store(true, std::sync::atomic::Ordering::Release);
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
        let fenced = SignOutFence::for_token(Some("tok"));
        assert_eq!(
            run_token(&path, "u1", &fenced),
            Err("signed_out_token_not_removed owner=u1".to_owned()),
            "a fenced process sent the signed-out token"
        );
        assert_eq!(
            run_token(&path, "u1", &SignOutFence::Open),
            Ok("tok".to_owned())
        );
        assert_eq!(
            run_token(&path, "u1", &SignOutFence::Every),
            Err("signed_out_token_not_removed owner=u1".to_owned())
        );
    }

    /// CR 4637783579: the fence names the token the sign-out left, so a
    /// token stored by any sign-in (the car's QuickConnect writes the file
    /// itself) is read; the left token stays refused. With no token to
    /// name, every token is refused until the phone stores one.
    #[test]
    fn the_sign_out_fence_refuses_only_the_token_it_names() {
        let d = super::super::test_dir::TmpDir::new("run-fence-identity");
        let path = d.path().join("device-token");
        store(&path, "tokA").unwrap();
        pair(&path, Some("tokA"), "u1").unwrap();
        let fence = SignOutFence::for_token(Some("tokA"));
        assert!(fence.refuses("tokA"));
        // Another sign-in's token, stored past the phone's own path.
        pharos_content::token::store(&path, "tokB").unwrap();
        pair(&path, Some("tokB"), "u1").unwrap();
        assert!(!fence.refuses("tokB"));
        assert_eq!(run_token(&path, "u1", &fence), Ok("tokB".to_owned()));
        assert!(SignOutFence::for_token(None).refuses("tokB"));
        assert!(!SignOutFence::Open.refuses("tokB"));
    }

    /// CR 4638522305: a sign-out that could not read its token fences every
    /// token. The car's QuickConnect then stores B straight through
    /// `pharos_content::token::store`: that store replaces the fence, so B
    /// loads, while the token it overwrote (A) stays refused.
    #[test]
    fn a_quickconnect_store_replaces_an_unnamed_fence() {
        let d = super::super::test_dir::TmpDir::new("run-fence-every");
        let path = d.path().join("device-token");
        store(&path, "tokA").unwrap();
        pair(&path, Some("tokA"), "u1").unwrap();
        // forget_token could not read the token: no name for the fence.
        set_fence(SignOutFence::for_token(None));
        assert_eq!(fence(), SignOutFence::Every);
        pharos_content::token::store(&path, "tokB").unwrap();
        pair(&path, Some("tokB"), "u1").unwrap();
        assert_eq!(
            run_token(&path, "u1", &fence()),
            Ok("tokB".to_owned()),
            "the QuickConnect token stayed refused"
        );
        assert!(
            fence().refuses("tokA"),
            "the overwritten token was let back"
        );
        assert!(take_stale(), "the cached token was not marked stale");
        // A fence naming another token stays when a third is stored.
        pharos_content::token::store(&path, "tokC").unwrap();
        assert!(fence().refuses("tokA"));
        assert!(!fence().refuses("tokC"));
    }
}
