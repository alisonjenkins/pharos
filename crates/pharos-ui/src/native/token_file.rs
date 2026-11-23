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
    watch_stores();
    pharos_content::token::read(path).map_err(|e| e.to_string())
}

/// A new sign-in's token. Whoever the file held it for is gone with it: the
/// new token's user is unknown until `store_user`.
pub fn store(path: &Path, token: &str) -> Result<(), String> {
    watch_stores();
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
/// sign-out fence ([`SignOutFence`]), read with the token under the file's
/// lock (`pharos_content::token::read_for_run`): the token it names is
/// refused, and one that names none refuses every request.
pub fn run_token(path: &Path, owner: &str) -> Result<String, String> {
    watch_stores();
    let read = pharos_content::token::read_for_run(path)
        .map_err(|e| format!("token_unreadable owner={owner} error={e}"))?;
    let (token, user) = match read {
        pharos_content::token::RunRead::Fenced => {
            return Err(format!("signed_out_token_not_removed owner={owner}"));
        }
        pharos_content::token::RunRead::NoToken => (None, None),
        pharos_content::token::RunRead::Token(s) => (Some(s.token), s.user),
    };
    super::kept_video::token_for_owner(token.as_deref(), user.as_deref(), owner)
}

/// The phone's sign-out fence is the token file's own
/// ([`pharos_content::token::Fence`]): set by the sign-out operation that
/// left its token on file (neither removed nor marked), under the file's
/// lock, from the token that operation targeted; replaced by every store,
/// the car's QuickConnect included, in the same lock. The media service in
/// this process reads through the same fence.
pub use pharos_content::token::Fence as SignOutFence;

/// The sign-out fence of the token file at `path`. Its first read
/// registers the store hook too, so a token cached from it is marked stale
/// by any later store.
pub fn fence(path: &Path) -> SignOutFence {
    watch_stores();
    pharos_content::token::fence(path)
}

/// Adds `more` to the sign-out fence of the token file at `path`
/// (`reason` for the log), under the file's lock; it never lifts a refusal.
pub fn add_fence(path: &Path, more: &SignOutFence, reason: &str) {
    watch_stores();
    pharos_content::token::add_fence(path, more, reason);
}

/// Fences every token at every path in this process
/// (`pharos_content::token::fence_every_file`): for a sign-out that could
/// not find its token file. The next store replaces it.
pub fn fence_every_file(reason: &str) {
    watch_stores();
    pharos_content::token::fence_every_file(reason);
}

/// Set by every change to what a token read returns (a store, a removal,
/// a revocation marker, a fence), from inside that operation: the token a
/// cache holds may be another now, or none.
static STALE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the token file changed since the last call.
pub fn take_stale() -> bool {
    STALE.swap(false, std::sync::atomic::Ordering::AcqRel)
}

/// Registers [`after_store`] and [`after_change`] with
/// `pharos_content::token`, once.
fn watch_stores() {
    pharos_content::token::on_stored(after_store);
    pharos_content::token::on_changed(after_change);
}

/// The change hook: marks a cached token stale, whatever the change was.
fn after_change(_reason: &str) {
    STALE.store(true, std::sync::atomic::Ordering::Release);
}

/// The store hook: marks a cached token stale. The store replaced the
/// fence itself, under the file's lock.
fn after_store(_stored: &str, _replaced: Option<&str>) {
    STALE.store(true, std::sync::atomic::Ordering::Release);
}

/// Reads the token behind the fence and caches it (`cached` is the caller's
/// cache, under the caller's lock). Never log the value.
pub fn load_into(cached: &mut Option<Option<String>>, path: Option<&Path>) -> Option<String> {
    let fence = path.map(fence).unwrap_or_default();
    // The read goes through the fence itself ([`read`]).
    let token = path
        .and_then(|path| read(path).ok().flatten())
        .map(|s| s.token);
    if token.is_none() && fence != SignOutFence::Open {
        log::info!("pharos: token load verdict=refused reason=signed_out_token_not_removed");
    }
    *cached = Some(token.clone());
    token
}

/// The cached token, read again when nothing is cached or `stale` says the
/// file changed since.
pub fn token_cached(
    cached: &mut Option<Option<String>>,
    path: impl FnOnce() -> Option<PathBuf>,
    stale: impl FnOnce() -> bool,
) -> Option<String> {
    if let Some(token) = &*cached {
        if !stale() {
            return token.clone();
        }
    }
    load_into(cached, path().as_deref())
}

/// What the sign-out's cache clear leaves: nothing known, so the next read
/// goes to the file. Not "no token": a store that landed after the
/// operation released the file's lock (a Quick Connect sign-in, the
/// phone's own) may have cached its token, and the file still holds it.
/// The fence the operation published refuses the token it left.
pub fn invalidate_after_sign_out(cached: &mut Option<Option<String>>) {
    log::info!(
        "pharos: token cache action=invalidate reason=sign_out_clear held={}",
        match cached {
            None => "unknown",
            Some(None) => "absent",
            Some(Some(_)) => "token",
        }
    );
    *cached = None;
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

impl SignedOut {
    /// The sign-in screen's answer for this verdict: 204 only for a
    /// removal or a revocation that is on disk; one whose folder would not
    /// sync warns that the sign-in may come back after a restart.
    pub fn reply(self) -> super::machine::SignOutReply {
        use super::machine::SignOutReply;
        match self {
            Self::Saved | Self::Marked(_) => SignOutReply::Status(204),
            Self::SavedNotDurable(why) => SignOutReply::NotDurable(why),
            Self::NotSaved(why) => SignOutReply::NotSaved(why),
        }
    }
}

/// A sign-out's removal of the token ([`forget`]), as the verdict the app
/// reports: an unlink that failed is kept apart from an unlink whose
/// folder sync failed.
pub fn sign_out(path: &Path) -> SignedOut {
    signed_out(end_session(path, None, None))
}

/// The sign-out the phone runs: every token fenced and the one on file
/// ended in one hold of the file's lock
/// (`pharos_content::token::sign_out_fenced`).
fn sign_out_fenced(path: &Path) -> SignedOut {
    watch_stores();
    signed_out(pharos_content::token::sign_out_fenced(path))
}

/// What a session end that was a sign-out is, as the app reports it.
fn signed_out(ended: Ended) -> SignedOut {
    match ended {
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

/// What a sign-out asks of the media service in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceNotice {
    /// The token could not be ended: stay signed out, do not reload.
    StaySignedOut,
    /// The token file changed: read it again.
    Reload,
}

/// The phone's sign-out (`android::forget_token`), with its platform parts
/// passed in: `path_lookup` finds the token file, `clear` drops what the
/// caller cached (with the path, when there is one), and `notifier` tells
/// the media service.
///
/// Lock order, outermost first: the caller's cache lock (android.rs `TOKEN`,
/// held across a store, a refused-token end or a cached read), then the
/// token file's lock (`pharos_content::token` `FILE`), then the fence
/// table's mutex (`FENCES`, taken under the file's lock when a fence is
/// added or read). The only call made under those is the change hook, which
/// sets an atomic flag and takes no lock. [`sign_out_fenced`] takes the
/// file's lock and then `FENCES`, and holds neither cache lock: `clear` and
/// `notifier` run after it has returned, so `clear` takes the cache lock
/// only with the file's lock released. Nothing takes the cache lock while
/// holding the file's lock or `FENCES`, so there is no inversion. The fence is
/// published before `clear` runs on every exit: every token at the file's
/// path, or at every path when the path cannot be found. The sign-out
/// itself never narrows that fence: when the token can be neither removed
/// nor marked, the operation unions a fence naming it into this one, and
/// the union of `Every` with anything is still `Every`. Only a later
/// confirmed store changes it (`after_store` lifts the stored token's
/// name). A `NotSaved`, the failed lookup included, asks the service to
/// stay signed out.
pub fn forget_token_with(
    path_lookup: impl FnOnce() -> Result<PathBuf, String>,
    clear: impl FnOnce(Option<&Path>),
    notifier: impl FnOnce(ServiceNotice),
) -> SignedOut {
    let path = path_lookup();
    // The fence, the removal or revocation and the choice of what they
    // apply to are one operation under the token file's lock
    // ([`sign_out`]); the caller's cache clear and the service's notice run
    // after it has released the lock, so a store that lands meanwhile (a
    // Quick Connect sign-in) is neither held up by them nor signed out by
    // them. A path that cannot be found fences every file at once.
    let verdict = match path {
        Ok(path) => {
            let verdict = sign_out_fenced(&path);
            clear(Some(&path));
            verdict
        }
        Err(e) => {
            fence_every_file("token_path_unknown");
            clear(None);
            log::warn!(
                "pharos: token forget verdict=not_removed reason=token_path_unknown error={e}"
            );
            SignedOut::NotSaved(e)
        }
    };
    notifier(match verdict {
        SignedOut::NotSaved(_) => ServiceNotice::StaySignedOut,
        _ => ServiceNotice::Reload,
    });
    verdict
}

/// A session end (or refused resume) that could not find the token file:
/// every token at every path is fenced, as a sign-out that could not find
/// it fences them ([`forget_token_with`]), and the end is `Unsafe`, for the
/// caller to ask the media service to stay signed out.
pub fn end_session_path_unknown(error: String) -> Ended {
    fence_every_file("token_path_unknown");
    log::warn!("pharos: token forget verdict=fenced reason=token_path_unknown error={error}");
    Ended::Unsafe {
        removal: error,
        marker: "no token path".to_owned(),
        target: None,
    }
}

/// Ends a session's hold on the token (`pharos_content::token::end_session`):
/// the token goes while it is `seen` (any with `None`), a replacement is
/// kept, and one that will not go is marked revoked for `user`.
pub fn end_session(path: &Path, seen: Option<&str>, user: Option<&str>) -> Ended {
    watch_stores();
    pharos_content::token::end_session(path, seen, user)
}

/// Removes the token and its user only while the token on file is `seen`
/// (the refused one), under the file's lock: a token stored since is kept
/// (`ForgetIf::Changed`).
pub fn forget_if(path: &Path, seen: &str) -> Result<ForgetIf, String> {
    pharos_content::token::forget_if(path, seen).map_err(|e| e.to_string())
}

#[cfg(test)]
mod sign_out_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// CR 4644634569: a sign-out whose folder would not sync is not
    /// answered as a plain 204: the sign-in screen is told it may come
    /// back after a restart. Durable removals and revocations stay 204.
    /// CR 4666307257: a store of B that lands after the sign-out released
    /// the file's lock, then a read that caches B, then the sign-out's cache
    /// clear: B must still be read. The clear leaves the cache unknown, not
    /// "no token".
    #[test]
    fn a_store_between_the_sign_out_and_its_cache_clear_is_not_lost() {
        let d = super::super::test_dir::TmpDir::new("clear-after-store");
        let path = d.path().join("device-token");
        store(&path, "tokA").unwrap();
        assert_eq!(sign_out_fenced(&path), SignedOut::Saved);
        // The phone's own store of B (it caches B under its lock) ...
        store(&path, "tokB").unwrap();
        let mut cached = Some(Some("tokB".to_owned()));
        // ... a read that serves it ...
        let at = || Some(path.clone());
        assert_eq!(token_cached(&mut cached, at, || false), Some("tokB".into()));
        // ... and then the sign-out's clear.
        invalidate_after_sign_out(&mut cached);
        assert_eq!(
            token_cached(&mut cached, at, || false),
            Some("tokB".into()),
            "the clear hid the token a later sign-in stored"
        );
    }

    #[test]
    fn a_sign_out_not_durable_is_not_answered_as_done() {
        use super::super::machine::SignOutReply;
        assert_eq!(SignedOut::Saved.reply(), SignOutReply::Status(204));
        assert_eq!(
            SignedOut::Marked("x".into()).reply(),
            SignOutReply::Status(204)
        );
        assert_eq!(
            SignedOut::SavedNotDurable("sync".into()).reply(),
            SignOutReply::NotDurable("sync".into())
        );
        assert_eq!(
            SignedOut::NotSaved("rm".into()).reply(),
            SignOutReply::NotSaved("rm".into())
        );
    }

    /// A token that cannot be removed is a sign-out not saved, apart from
    /// one removed whose folder could not be synced; a removed one is
    /// saved.
    #[test]
    fn a_token_that_cannot_be_removed_is_a_sign_out_not_saved() {
        let d = super::super::test_dir::TmpDir::new("sign-out");
        let path = d.path().join("device-token");
        store(&path, "tok").unwrap();
        assert_eq!(sign_out(&path), SignedOut::Saved);
        assert!(read(&path).unwrap().is_none());
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
        assert_eq!(run_token(&path, "u1"), Ok("tok".to_owned()));
        add_fence(&path, &SignOutFence::for_token(Some("tok")), "test");
        assert_eq!(
            run_token(&path, "u1"),
            Err("signed_out_token_not_removed owner=u1".to_owned()),
            "a fenced process sent the signed-out token"
        );
        add_fence(&path, &SignOutFence::Every, "test");
        assert_eq!(
            run_token(&path, "u1"),
            Err("signed_out_token_not_removed owner=u1".to_owned())
        );
        // A store after the fence was set: that token is the one read with
        // the fence in force now, and it is sent (body item android.rs:221).
        pharos_content::token::store(&path, "tok2").unwrap();
        pair(&path, Some("tok2"), "u1").unwrap();
        assert_eq!(run_token(&path, "u1"), Ok("tok2".to_owned()));
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
        add_fence(&path, &fence, "test");
        // Another sign-in's token, stored past the phone's own path.
        pharos_content::token::store(&path, "tokB").unwrap();
        pair(&path, Some("tokB"), "u1").unwrap();
        assert!(!fence.refuses("tokB"));
        assert_eq!(run_token(&path, "u1"), Ok("tokB".to_owned()));
        assert!(SignOutFence::for_token(None).refuses("tokB"));
        assert!(!SignOutFence::Open.refuses("tokB"));
    }

    /// CR 4638691172, first ordering: a sign-out that cannot name its
    /// token fences every token inside the operation, so a QuickConnect
    /// store that lands right after it replaces that fence and is read.
    #[cfg(debug_assertions)]
    #[test]
    fn a_store_after_an_unnamed_sign_out_is_read() {
        use pharos_content::token::faults;
        let d = super::super::test_dir::TmpDir::new("sign-out-then-store");
        let path = d.path().join("device-token");
        store(&path, "tokA").unwrap();
        faults::TOKEN_READ.with(|f| f.set(true));
        let out = sign_out(&path);
        faults::TOKEN_READ.with(|f| f.set(false));
        assert!(matches!(out, SignedOut::NotSaved(_)), "{out:?}");
        assert_eq!(fence(&path), SignOutFence::Every);
        pharos_content::token::store(&path, "tokB").unwrap();
        pair(&path, Some("tokB"), "u1").unwrap();
        assert_eq!(run_token(&path, "u1"), Ok("tokB".to_owned()));
    }

    /// CR 4638691172, second ordering: a store of B between the caller's
    /// read of A and the sign-out makes B the token the sign-out targets.
    /// When B can be neither removed nor marked, the fence names B.
    #[cfg(debug_assertions)]
    #[test]
    fn a_sign_out_fences_the_token_it_targeted_not_an_earlier_read() {
        use pharos_content::token::faults;
        let d = super::super::test_dir::TmpDir::new("store-then-sign-out");
        let path = d.path().join("device-token");
        store(&path, "tokA").unwrap();
        let held = read(&path).unwrap().map(|s| s.token);
        pharos_content::token::store(&path, "tokB").unwrap();
        pair(&path, Some("tokB"), "u1").unwrap();
        faults::REMOVE.with(|f| f.set(true));
        faults::MARKER.with(|f| f.set(true));
        let out = sign_out(&path);
        faults::REMOVE.with(|f| f.set(false));
        faults::MARKER.with(|f| f.set(false));
        assert!(matches!(out, SignedOut::NotSaved(_)), "{out:?}");
        assert_eq!(held.as_deref(), Some("tokA"));
        assert_eq!(fence(&path), SignOutFence::for_token(Some("tokB")));
        assert!(
            run_token(&path, "u1").is_err(),
            "the signed-out token B was sent"
        );
    }

    /// A Quick Connect sign-in that stores a token while the sign-out's cache
    /// clear runs lands after the sign-out's one locked operation, so it is
    /// kept: the sign-out ended the token it found, not the one stored
    /// after it.
    #[test]
    fn a_store_during_the_clear_after_a_sign_out_is_kept() {
        let d = super::super::test_dir::TmpDir::new("forget-then-store");
        let path = d.path().join("device-token");
        store(&path, "tokA").unwrap();
        pair(&path, Some("tokA"), "u1").unwrap();
        let p2 = path.clone();
        let out = forget_token_with(
            || Ok(path.clone()),
            move |_| {
                assert!(
                    read(&p2).unwrap().is_none(),
                    "the cache clear ran before the token was removed"
                );
                pharos_content::token::store(&p2, "tokB").unwrap();
            },
            |_| {},
        );
        assert_eq!(out, SignedOut::Saved);
        assert_eq!(
            read(&path).unwrap().map(|s| s.token).as_deref(),
            Some("tokB"),
            "the sign-out removed the token a sign-in stored after it"
        );
        assert!(fence(&path).refuses("tokA"));
        assert!(!fence(&path).refuses("tokB"));
    }

    /// CR 4639608778, 4639798750: a sign-out whose token path cannot be
    /// found goes through `forget_token_with` as the phone's does. It fences
    /// every token file, tells the service to stay signed out, and a later
    /// reload of the service stays signed out. The fence is process-wide,
    /// so the body runs in a child process of its own, never under another
    /// test's token reads.
    #[test]
    fn a_sign_out_with_no_token_path_keeps_the_service_signed_out() {
        const CHILD: &str = "PHAROS_TEST_FORGET_NO_PATH";
        if std::env::var_os(CHILD).is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!(
                        "{}::a_sign_out_with_no_token_path_keeps_the_service_signed_out",
                        module_path!().trim_start_matches("pharos_ui::")
                    ),
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("1 passed"),
                "the child ran nothing: {}",
                String::from_utf8_lossy(&out.stdout)
            );
            return;
        }
        let d = super::super::test_dir::TmpDir::new("forget-no-path");
        let path = d.path().join("device-token");
        store(&path, "kept-token").unwrap();
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        let lib = pharos_content::Library::connect_with(
            &server,
            &path,
            pharos_content::Timeouts {
                command: std::time::Duration::from_secs(3),
                code: std::time::Duration::from_millis(800),
            },
        )
        .unwrap();
        let notices = std::cell::RefCell::new(Vec::new());
        let out = forget_token_with(
            || Err("no files dir".to_owned()),
            |_| {},
            |n| notices.borrow_mut().push(n),
        );
        assert!(matches!(out, SignedOut::NotSaved(_)), "{out:?}");
        assert_eq!(*notices.borrow(), [ServiceNotice::StaySignedOut]);
        lib.reload_credentials()
            .expect("the reload tried the token the sign-out left");
        assert_eq!(lib.auth_state(), pharos_content::AuthState::SignedOut);
    }

    /// CR 4642574404: a refused session's end that could not find the
    /// token file fences every token, as a sign-out that could not find it
    /// does, so no reader loads the refused token meanwhile. The fence is
    /// process-wide, so in a child.
    #[test]
    fn a_session_end_with_no_token_path_fences_every_token() {
        if !in_child(
            "a_session_end_with_no_token_path_fences_every_token",
            "PHAROS_TEST_END_NO_PATH",
        ) {
            return;
        }
        let d = super::super::test_dir::TmpDir::new("end-no-path");
        let path = d.path().join("device-token");
        store(&path, "tokA").unwrap();
        assert_eq!(fence(&path), SignOutFence::Open);
        let ended = end_session_path_unknown("no files dir".to_owned());
        assert!(
            matches!(ended, Ended::Unsafe { target: None, .. }),
            "{ended:?}"
        );
        assert_eq!(
            fence(&path),
            SignOutFence::Every,
            "the refused token stayed readable"
        );
        assert!(read(&path).unwrap().is_none());
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
        add_fence(&path, &SignOutFence::for_token(None), "test");
        assert_eq!(fence(&path), SignOutFence::Every);
        pharos_content::token::store(&path, "tokB").unwrap();
        pair(&path, Some("tokB"), "u1").unwrap();
        assert_eq!(
            run_token(&path, "u1"),
            Ok("tokB".to_owned()),
            "the QuickConnect token stayed refused"
        );
        assert!(
            fence(&path).refuses("tokA"),
            "the overwritten token was let back"
        );
        // A fence naming another token stays when a third is stored.
        pharos_content::token::store(&path, "tokC").unwrap();
        assert!(fence(&path).refuses("tokA"));
        assert!(!fence(&path).refuses("tokC"));
    }

    /// Runs the calling test, named `name` in this module, in a child
    /// process of its own with `env` set: true in the child, which goes on
    /// with the test body; the parent asserts the child passed and returns
    /// false. For tests of process-wide state other tests also change.
    fn in_child(name: &str, env: &str) -> bool {
        if std::env::var_os(env).is_some() {
            return true;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!(
                    "{}::{name}",
                    module_path!().trim_start_matches("pharos_ui::")
                ),
                "--test-threads=1",
            ])
            .env(env, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "the child ran nothing: {stdout}"
        );
        false
    }

    /// CR 4640611429 and the body item on android.rs:198: every change to
    /// what a token read returns marks a cached token stale, from inside
    /// the operation: a store, a session end that removes the token, one
    /// that marks it revoked, and a fence. Process-wide, so in a child.
    #[test]
    fn every_token_file_change_marks_the_cache_stale() {
        if !in_child(
            "every_token_file_change_marks_the_cache_stale",
            "PHAROS_TEST_TOKEN_STALE",
        ) {
            return;
        }
        let d = super::super::test_dir::TmpDir::new("stale-every-change");
        let path = d.path().join("device-token");
        store(&path, "tokA").unwrap();
        let _ = take_stale();
        store(&path, "tokB").unwrap();
        assert!(take_stale(), "a store did not mark the cache stale");
        // The content worker's own end of a refused session, not through
        // this module: the hook is registered once, process-wide.
        let _ = pharos_content::token::end_session(&path, Some("tokB"), None);
        assert!(take_stale(), "a removal did not mark the cache stale");
        // Fault injection exists in debug builds only.
        #[cfg(debug_assertions)]
        {
            store(&path, "tokC").unwrap();
            let _ = take_stale();
            pharos_content::token::faults::REMOVE.with(|f| f.set(true));
            let ended = pharos_content::token::end_session(&path, Some("tokC"), None);
            pharos_content::token::faults::REMOVE.with(|f| f.set(false));
            assert!(matches!(ended, Ended::Marked(_)), "{ended:?}");
            assert!(
                take_stale(),
                "a revocation marker did not mark the cache stale"
            );
        }
        pharos_content::token::add_fence(&path, &SignOutFence::Every, "test");
        assert!(take_stale(), "a fence did not mark the cache stale");
        // CR 4642574367: a store whose rename landed but whose read-back
        // failed marks the cache stale too, even when its fence adds
        // nothing (every token is fenced already); the reload refuses the
        // unconfirmed token.
        #[cfg(debug_assertions)]
        {
            pharos_content::token::faults::READ_BACK.with(|f| f.set(true));
            let stored = pharos_content::token::store(&path, "tokD");
            pharos_content::token::faults::READ_BACK.with(|f| f.set(false));
            assert!(stored.is_err());
            assert!(
                take_stale(),
                "an unconfirmed store did not mark the cache stale"
            );
            assert!(
                read(&path).unwrap().is_none(),
                "the unconfirmed token loads"
            );
            // CR review 5995575621 (token.rs:609): a store whose rename
            // landed but whose folder would not sync marks the cache stale,
            // though its fence (every token) does not move.
            pharos_content::token::faults::DIR_SYNC.with(|f| f.set(true));
            let stored = pharos_content::token::store(&path, "tokE");
            pharos_content::token::faults::DIR_SYNC.with(|f| f.set(false));
            assert!(stored.is_err());
            assert!(
                take_stale(),
                "a store that could not sync its folder did not mark the cache stale"
            );
        }
    }
}
