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

/// Sets the sign-out fence of the token file at `path` (`reason` for the
/// log), under the file's lock.
pub fn set_fence(path: &Path, next: SignOutFence, reason: &str) {
    watch_stores();
    pharos_content::token::set_fence(path, next, reason);
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
/// the media service. Every exit publishes a fence first, so no early
/// return can skip it: every token at the file's path, or at every path
/// when the path cannot be found. The sign-out operation then narrows it to
/// the token it left, or a store replaces it. A `NotSaved`, the failed
/// lookup included, asks the service to stay signed out.
pub fn forget_token_with(
    path_lookup: impl FnOnce() -> Result<PathBuf, String>,
    clear: impl FnOnce(Option<&Path>),
    notifier: impl FnOnce(ServiceNotice),
) -> SignedOut {
    let path = path_lookup();
    match &path {
        Ok(path) => set_fence(path, SignOutFence::Every, "sign_out_started"),
        Err(_) => fence_every_file("token_path_unknown"),
    }
    let verdict = match path {
        Ok(path) => {
            clear(Some(&path));
            sign_out(&path)
        }
        Err(e) => {
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
        assert_eq!(run_token(&path, "u1", &fence(&path)), Ok("tokB".to_owned()));
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
            run_token(&path, "u1", &fence(&path)).is_err(),
            "the signed-out token B was sent"
        );
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
        set_fence(&path, SignOutFence::for_token(None), "test");
        assert_eq!(fence(&path), SignOutFence::Every);
        pharos_content::token::store(&path, "tokB").unwrap();
        pair(&path, Some("tokB"), "u1").unwrap();
        assert_eq!(
            run_token(&path, "u1", &fence(&path)),
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
        pharos_content::token::set_fence(&path, SignOutFence::Every, "test");
        assert!(take_stale(), "a fence did not mark the cache stale");
    }
}
