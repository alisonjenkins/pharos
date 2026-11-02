//! Host stand-in for [`web`](super): the same calls, answering as a browser
//! with no server would. Host builds only render the app under test.

// Mirrors the browser module's surface; some of it only the browser uses.
#![allow(dead_code)]

use pharos_client::api::{Capabilities, NativeApi};
use pharos_client::CallInfo;

use super::machine::{Probe, SignInReply, SignOutReply};

const NO_BROWSER: &str = "the native app needs a browser";

pub struct Connection;

pub struct Open {
    pub conn: Connection,
    pub api: NativeApi,
}

pub enum OpenError {
    NotOpened(Probe),
    NotSignedIn,
    Resume(String),
}

pub async fn open(_call: &CallInfo) -> Result<Open, OpenError> {
    Err(OpenError::NotOpened(Probe::Failed(NO_BROWSER.into())))
}

pub async fn closed(_conn: &mut Connection) -> String {
    String::new()
}

pub async fn sign_in(_name: &str, _password: &str) -> SignInReply {
    SignInReply::Failed(NO_BROWSER.into())
}

pub async fn sign_out() -> SignOutReply {
    SignOutReply::Failed(NO_BROWSER.into())
}

pub async fn sleep_ms(_ms: u32) {}

pub fn current_path() -> String {
    "/".into()
}

pub fn push(_path: &str) {}

pub fn replace(_path: &str) {}

pub struct PopState;

pub fn on_pop_state(_on_path: impl FnMut(String) + 'static) -> PopState {
    PopState
}

pub fn plays_hls() -> bool {
    false
}

pub fn capabilities() -> Capabilities {
    Capabilities::default()
}
