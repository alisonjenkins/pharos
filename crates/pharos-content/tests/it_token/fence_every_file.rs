//! A sign-out that could not find its token file fences every token in
//! the process. A service reload then stays signed out; a store lifts it.

use std::time::Duration;

use pharos_content::{token, AuthState, Library, Timeouts};

/// CR 4639608778: the phone's sign-out failed to find its token file
/// (no files dir), so it fences every file. The service's reload reads no
/// token and stays signed out; a sign-in's store lifts the fence.
#[test]
fn a_sign_out_that_cannot_find_its_token_file_keeps_the_service_signed_out() {
    let _serial = super::serial();
    let scratch = super::Scratch::new("fence-every-file");
    let token_path = scratch.0.join("token");
    token::store(&token_path, "kept-token").unwrap();
    let lib = Library::connect_with(
        &super::dead_server(),
        &token_path,
        Timeouts {
            command: Duration::from_secs(3),
            code: Duration::from_millis(800),
        },
    )
    .unwrap();
    token::fence_every_file("test_token_path_unknown");
    lib.reload_credentials()
        .expect("the reload tried the fenced token against the server");
    assert_eq!(lib.auth_state(), AuthState::SignedOut);
    // A sign-in's store lifts it: the replacement is read and tried.
    token::store(&token_path, "replacement-token").unwrap();
    assert!(
        lib.reload_credentials().is_err(),
        "the reload did not read the replacement token"
    );
}
