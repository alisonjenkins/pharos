//! Browser glue for the native app: the socket, `/pharos/session`, history
//! and timers. Every result comes back as data for
//! [`Machine`](super::machine::Machine); no state lives here.

use gloo_net::http::Request;
use pharos_client::api::{Capabilities, NativeApi};
use pharos_client::browser::{self, Connection};
use pharos_client::CallInfo;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};

use super::machine::{Probe, SignInReply, SignOutReply};

const SESSION: &str = "/pharos/session";

/// A signed-in connection. Dropping it closes the socket.
pub struct Open {
    pub conn: Connection,
    pub api: NativeApi,
}

pub enum OpenError {
    /// The socket never opened; the probe says why.
    NotOpened(Probe),
    /// The server knows no session for this browser.
    NotSignedIn,
    Resume(String),
}

/// Open `/pharos/rpc` on this page's host and resume the session cookie.
pub async fn open(call: &CallInfo) -> Result<Open, OpenError> {
    let conn = match browser::connect(&rpc_url()).await {
        Ok(conn) => conn,
        Err(e) => {
            web_sys::console::warn_1(&format!("pharos: {e}").into());
            return Err(OpenError::NotOpened(probe().await));
        }
    };
    match pharos_client::resume(&conn.bootstrap, call).await {
        Ok(session) => Ok(Open {
            api: NativeApi::new(&session),
            conn,
        }),
        Err(e) if e.needs_sign_in() => Err(OpenError::NotSignedIn),
        Err(e) => Err(OpenError::Resume(e.to_string())),
    }
}

/// The close frame's reason once `open`'s socket closes.
pub async fn closed(conn: &mut Connection) -> String {
    conn.closed().await.reason
}

fn rpc_url() -> String {
    let location = web_sys::window().map(|w| w.location());
    let host = location
        .as_ref()
        .and_then(|l| l.host().ok())
        .unwrap_or_default();
    let scheme = match location.and_then(|l| l.protocol().ok()).as_deref() {
        Some("https:") => "wss",
        _ => "ws",
    };
    format!("{scheme}://{host}/pharos/rpc")
}

/// `GET /pharos/session`: 404 when the native API is off, 405 when it is
/// on. The Accept header keeps the UI's HTML fallback from answering.
async fn probe() -> Probe {
    match Request::get(SESSION)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) => Probe::Status(resp.status()),
        Err(e) => Probe::Failed(e.to_string()),
    }
}

#[derive(serde::Serialize)]
struct SignInBody<'a> {
    name: &'a str,
    password: &'a str,
}

pub async fn sign_in(name: &str, password: &str) -> SignInReply {
    let request = match Request::post(SESSION).json(&SignInBody { name, password }) {
        Ok(request) => request,
        Err(e) => return SignInReply::Failed(e.to_string()),
    };
    match request.send().await {
        Ok(resp) => SignInReply::Status {
            status: resp.status(),
            retry_after: resp
                .headers()
                .get("Retry-After")
                .and_then(|v| v.trim().parse().ok()),
        },
        Err(e) => SignInReply::Failed(e.to_string()),
    }
}

pub async fn sign_out() -> SignOutReply {
    match Request::delete(SESSION).send().await {
        Ok(resp) => SignOutReply::Status(resp.status()),
        Err(e) => SignOutReply::Failed(e.to_string()),
    }
}

pub async fn sleep_ms(ms: u32) {
    let ms = i32::try_from(ms).unwrap_or(i32::MAX);
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

pub fn current_path() -> String {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .unwrap_or_else(|| "/".into())
}

/// `history.pushState` to `path`, so Back returns to the page before.
pub fn push(path: &str) {
    if let Some(history) = web_sys::window().and_then(|w| w.history().ok()) {
        let _ = history.push_state_with_url(&JsValue::NULL, "", Some(path));
    }
}

/// `history.replaceState` to `path`: a redirect, which Back should skip.
pub fn replace(path: &str) {
    if let Some(history) = web_sys::window().and_then(|w| w.history().ok()) {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(path));
    }
}

/// A `popstate` listener; removed on drop.
pub struct PopState(Closure<dyn FnMut(web_sys::Event)>);

impl Drop for PopState {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window
                .remove_event_listener_with_callback("popstate", self.0.as_ref().unchecked_ref());
        }
    }
}

/// Call `on_path` with the new path whenever Back or Forward moves history.
pub fn on_pop_state(mut on_path: impl FnMut(String) + 'static) -> PopState {
    let listener = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| on_path(current_path()));
    if let Some(window) = web_sys::window() {
        let _ =
            window.add_event_listener_with_callback("popstate", listener.as_ref().unchecked_ref());
    }
    PopState(listener)
}

/// Whether `<video>` plays an HLS playlist itself (Safari, recent Chrome).
pub fn plays_hls() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.create_element("video").ok())
        .and_then(|e| e.dyn_into::<web_sys::HtmlMediaElement>().ok())
        .is_some_and(|v| !v.can_play_type("application/vnd.apple.mpegurl").is_empty())
}

/// What a browser `<video>` decodes everywhere pharos targets.
pub fn capabilities() -> Capabilities {
    let own = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect();
    Capabilities {
        video_codecs: own(&["h264", "vp9", "av1"]),
        audio_codecs: own(&["aac", "mp3", "opus", "flac"]),
        containers: own(&["mp4", "m4v", "webm"]),
        max_bitrate: 0,
    }
}
