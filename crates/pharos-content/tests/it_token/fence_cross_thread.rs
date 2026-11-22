//! The in-process token fence across threads, as the app uses it: the
//! sign-out fences the token on the app's thread, and the service's worker
//! (its own `pharos-content` thread) reads through the fence on a reload.
//!
//! This binary links the library without `cfg(test)`, so it runs the
//! production fence. The fence is kept per token file, so the other tests
//! of the group, each with a file of its own, cannot set or lift it.

use std::time::Duration;

use pharos_content::{token, AuthState, Library, Timeouts};

/// CR 4637279773: a token fenced by a sign-out on another thread is refused
/// by the service's reload on its worker thread. The reload reads no token
/// (signed out, no request), where an unfenced token would make it try the
/// server; a different token on file is read and tried.
#[test]
fn a_token_fenced_on_the_app_thread_is_refused_by_the_worker_thread() {
    let _serial = super::serial();
    let scratch = super::Scratch::new("fence-cross-thread");
    let token_path = scratch.0.join("token");
    token::store(&token_path, "fenced-token").unwrap();
    let probe = super::Probe::start();
    let lib = Library::connect_with(
        &probe.url,
        &token_path,
        Timeouts {
            command: Duration::from_secs(3),
            code: Duration::from_millis(800),
        },
    )
    .unwrap();

    // The sign-out could neither remove nor mark the token: the fence goes
    // in from the app's own thread.
    let at = token_path.clone();
    std::thread::Builder::new()
        .name("app-sign-out".into())
        .spawn(move || {
            token::add_fence(&at, &token::Fence::for_token(Some("fenced-token")), "test")
        })
        .unwrap()
        .join()
        .unwrap();

    // The reload runs on the service's worker thread.
    lib.reload_credentials()
        .expect("the reload tried the fenced token against the server");
    assert_eq!(lib.auth_state(), AuthState::SignedOut);
    // CR 4644634535: the fenced reload dialed nothing.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(probe.accepted(), 0, "the fenced reload connected");

    // The control: a different token is read, so the reload tries it (and
    // fails, with nothing listening).
    token::store(&token_path, "replacement-token").unwrap();
    assert!(
        lib.reload_credentials().is_err(),
        "the reload did not read the replacement token"
    );
    assert!(probe.saw(1), "the replacement reload made no connection");
}
