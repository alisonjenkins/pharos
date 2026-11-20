//! The in-process token fence across threads, as the app uses it: the
//! sign-out fences the token on the app's thread, and the service's worker
//! (its own `pharos-content` thread) reads through the fence on a reload.
//!
//! This binary links the library without `cfg(test)`, so it runs the
//! production fence. The fence is kept per token file, so the other tests
//! of the group, each with a file of its own, cannot set or lift it.

use std::path::PathBuf;
use std::time::Duration;

use pharos_content::{token, AuthState, Library, Timeouts};

/// A folder of this test's own, removed when it drops (a failed assertion
/// included).
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pharos-content-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An address nothing listens on: a link attempt fails at once.
fn dead_server() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", l.local_addr().unwrap())
}

/// CR 4637279773: a token fenced by a sign-out on another thread is refused
/// by the service's reload on its worker thread. The reload reads no token
/// (signed out, no request), where an unfenced token would make it try the
/// server; a different token on file is read and tried.
#[test]
fn a_token_fenced_on_the_app_thread_is_refused_by_the_worker_thread() {
    let scratch = Scratch::new("fence-cross-thread");
    let token_path = scratch.0.join("token");
    token::store(&token_path, "fenced-token").unwrap();
    let lib = Library::connect_with(
        &dead_server(),
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
        .spawn(move || token::set_fence(&at, token::Fence::for_token(Some("fenced-token")), "test"))
        .unwrap()
        .join()
        .unwrap();

    // The reload runs on the service's worker thread.
    lib.reload_credentials()
        .expect("the reload tried the fenced token against the server");
    assert_eq!(lib.auth_state(), AuthState::SignedOut);

    // The control: a different token is read, so the reload tries it (and
    // fails, with nothing listening).
    token::store(&token_path, "replacement-token").unwrap();
    assert!(
        lib.reload_credentials().is_err(),
        "the reload did not read the replacement token"
    );
}
