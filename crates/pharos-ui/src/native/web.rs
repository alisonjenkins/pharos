//! Browser glue for the native app: the socket, `/pharos/session`, history
//! and timers. Every result comes back as data for
//! [`Machine`](super::machine::Machine); no state lives here.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::diagnostics::{self, Sink};
use super::rails;
use super::watch_view::Skip;
use gloo_net::http::Request;
use pharos_client::api::{Capabilities, ClientEvent, ClientEventKind, ClientLevel, NativeApi};
use pharos_client::browser::{self, Connection};
use pharos_client::group::{GroupUpdate, NativeGroup, NativeMembership};
use pharos_client::{CallInfo, ClientError};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};

use super::machine::{Probe, SignInReply, SignOutReply};
use super::playback::{
    is_loading, Engine, FragmentLoad, MediaState, OnFatal, OnFragment, PlaybackEngine, Recovery,
    RecoveryBudget, SubtitleSize,
};
use super::seen::MemberSeen;
use super::together::{self, ClockReply, GroupClient, GroupError, GroupSink, GroupSummary, Member};
use super::track_follow::{GroupTracksView, TrackChoice};
use pharos_client::api::ItemRef;

const SESSION: &str = "/pharos/session";

/// A signed-in connection. Dropping it closes the socket.
pub struct Open {
    pub conn: Connection,
    pub api: NativeApi,
    /// The server's schemaVersion; `None` when it could not be asked.
    pub server_schema: Option<u32>,
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
    let (server_schema, resumed) = futures_util::future::join(
        pharos_client::server_schema_version(&conn.bootstrap, call),
        pharos_client::resume(&conn.bootstrap, call),
    )
    .await;
    let server_schema = server_schema
        .inspect_err(|e| {
            web_sys::console::warn_1(&format!("pharos: serverInfo failed: {e}").into())
        })
        .ok();
    match resumed {
        Ok(session) => Ok(Open {
            api: NativeApi::new(&session),
            conn,
            server_schema,
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

/// The body of a same-origin GET, or why there is none: the HTTP status
/// when the server answered.
pub async fn get_text(url: &str) -> Result<String, (Option<u16>, String)> {
    let resp = Request::get(url)
        .send()
        .await
        .map_err(|e| (None, e.to_string()))?;
    if !resp.ok() {
        return Err((Some(resp.status()), resp.status_text()));
    }
    resp.text().await.map_err(|e| (None, e.to_string()))
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

/// The `history.state` of every entry the app pushes, which tells an entry
/// with an app page before it from one opened from outside.
const IN_APP: &str = "pharos";

/// `history.pushState` to `path`, so Back returns to the page before.
pub fn push(path: &str) {
    if let Some(history) = web_sys::window().and_then(|w| w.history().ok()) {
        let _ = history.push_state_with_url(&JsValue::from_str(IN_APP), "", Some(path));
    }
}

/// Whether a mouse event started on a control (a button, link, field or
/// slider) rather than on the surface around it. Dioxus dispatches from the
/// app root, so the handler's own element is not the target.
pub fn on_a_control(evt: &dioxus::prelude::MouseEvent) -> bool {
    use dioxus_web::WebEventExt;
    use wasm_bindgen::JsCast;
    evt.try_as_web_event()
        .and_then(|web| web.target())
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .and_then(|el| {
            el.closest("button, a, input, select, textarea, [role=slider], [role=button]")
                .ok()
                .flatten()
        })
        .is_some()
}

thread_local! {
    /// The next `contextmenu` opens no menu, unless a press comes first.
    static SWALLOW_MENU: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Keep the browser's menu off the `contextmenu` that follows this press. A
/// Shift+right press that skips swaps the player before the browser asks for
/// its menu, so the player's own `contextmenu` handler is gone by then
/// (B405). The next press, key or touch drops the guard, so a browser that
/// sends no `contextmenu` (Firefox with Shift held) leaves the next right
/// click, Menu key or long press alone.
pub fn swallow_next_context_menu() {
    thread_local! {
        static INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !INSTALLED.replace(true) {
        let Some(window) = web_sys::window() else {
            INSTALLED.set(false);
            return;
        };
        let on_menu = Closure::<dyn FnMut(web_sys::Event)>::new(|e: web_sys::Event| {
            if SWALLOW_MENU.replace(false) {
                e.prevent_default();
            }
        });
        // In the capture phase, so it runs before the app root sees the
        // press that may arm the guard again.
        let on_input = Closure::<dyn FnMut(web_sys::Event)>::new(|_| SWALLOW_MENU.set(false));
        for (kind, listener) in [
            ("contextmenu", &on_menu),
            ("mousedown", &on_input),
            ("keydown", &on_input),
            ("touchstart", &on_input),
        ] {
            let _ = window.add_event_listener_with_callback_and_bool(
                kind,
                listener.as_ref().unchecked_ref(),
                true,
            );
        }
        // For the page's life, like the app root's own listeners.
        on_menu.forget();
        on_input.forget();
    }
    SWALLOW_MENU.set(true);
}

/// `history.back()`, when the page before this one is the app's: an entry
/// the app pushed. `history.length` would also count other sites' pages.
pub fn go_back() -> bool {
    let Some(history) = web_sys::window().and_then(|w| w.history().ok()) else {
        return false;
    };
    let pushed = history
        .state()
        .ok()
        .and_then(|s| s.as_string())
        .is_some_and(|s| s == IN_APP);
    pushed && history.back().is_ok()
}

/// Load the page again, for a server newer than this build.
pub fn reload() {
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

/// `history.replaceState` to `path`: a redirect, which Back should skip.
pub fn replace(path: &str) {
    if let Some(history) = web_sys::window().and_then(|w| w.history().ok()) {
        // Keeps the entry's state, so a replaced page still knows whether
        // the app pushed it.
        let state = history.state().unwrap_or(JsValue::NULL);
        let _ = history.replace_state_with_url(&state, "", Some(path));
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

/// A window `keydown` listener for Escape, removed on drop.
pub struct EscapeWatch(Closure<dyn FnMut(web_sys::KeyboardEvent)>);

impl Drop for EscapeWatch {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window
                .remove_event_listener_with_callback("keydown", self.0.as_ref().unchecked_ref());
        }
    }
}

/// Call `on_escape` for an Escape pressed anywhere on the page, focus in
/// the app or not, unless a handler nearer the focus already took it.
pub fn on_escape(mut on_escape: impl FnMut() + 'static) -> EscapeWatch {
    let listener =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if e.key() == "Escape" && !e.default_prevented() {
                on_escape();
            }
        });
    if let Some(window) = web_sys::window() {
        let _ =
            window.add_event_listener_with_callback("keydown", listener.as_ref().unchecked_ref());
    }
    EscapeWatch(listener)
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

fn probe_video() -> Option<web_sys::HtmlMediaElement> {
    web_sys::window()?
        .document()?
        .create_element("video")
        .ok()?
        .dyn_into::<web_sys::HtmlMediaElement>()
        .ok()
}

/// Whether `<video>` plays an HLS playlist itself (Safari).
pub fn plays_hls() -> bool {
    probe_video().is_some_and(|v| !v.can_play_type(HLS_MIME).is_empty())
}

const HLS_MIME: &str = "application/vnd.apple.mpegurl";

/// Whether Media Source Extensions exist, which hls.js plays through.
pub fn has_mse() -> bool {
    web_sys::window().is_some_and(|w| {
        ["MediaSource", "ManagedMediaSource"]
            .iter()
            .any(|name| js_sys::Reflect::has(&w, &JsValue::from_str(name)).unwrap_or(false))
    })
}

/// What this browser decodes: a codec counts if MSE or `<video>` accepts it,
/// a container if `<video>` does.
pub fn capabilities() -> Capabilities {
    let video = probe_video();
    let can_play = |mime: &str| {
        video
            .as_ref()
            .is_some_and(|v| !v.can_play_type(mime).is_empty())
    };
    let mse = |mime: &str| web_sys::MediaSource::is_type_supported(mime);
    super::playback::capabilities(|mime| (has_mse() && mse(mime)) || can_play(mime), can_play)
}

fn video_element(id: &str) -> Option<web_sys::HtmlMediaElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(id)?
        .dyn_into::<web_sys::HtmlMediaElement>()
        .ok()
}

/// The `<video>`'s current position, 0 when it is gone.
pub fn position_ms(video_id: &str) -> u64 {
    video_element(video_id).map_or(0, |v| seconds_to_ms(v.current_time()))
}

/// What the controls show of the `<video>`.
pub fn media_state(video_id: &str) -> MediaState {
    video_element(video_id).map_or_else(MediaState::default, |v| MediaState {
        position_ms: seconds_to_ms(v.current_time()),
        duration_ms: seconds_to_ms(v.duration()),
        paused: v.paused(),
        volume: v.volume(),
        muted: v.muted(),
        loading: is_loading(v.seeking(), v.ready_state(), v.paused(), v.ended()),
        video_width: video_dimension(&v, "videoWidth"),
        video_height: video_dimension(&v, "videoHeight"),
    })
}

/// A `<video>`'s `videoWidth` or `videoHeight`, read off the element so the
/// media element binding is all this needs.
fn video_dimension(v: &web_sys::HtmlMediaElement, name: &str) -> u32 {
    js_sys::Reflect::get(v, &JsValue::from_str(name))
        .ok()
        .and_then(|n| n.as_f64())
        .map_or(0, |n| n as u32)
}

pub fn seek_ms(video_id: &str, ms: u64) {
    if let Some(v) = video_element(video_id) {
        // Lossless below 2^53 ms.
        v.set_current_time(ms as f64 / 1000.0);
    }
}

/// Play if paused, else pause. A refused `play()` (autoplay policy) leaves
/// it paused, which the controls then show.
pub fn toggle_play(video_id: &str) {
    let Some(v) = video_element(video_id) else {
        return;
    };
    if v.paused() {
        if let Err(e) = v.play() {
            warn(&format!("pharos: play() refused: {e:?}"));
        }
    } else if let Err(e) = v.pause() {
        warn(&format!("pharos: pause() refused: {e:?}"));
    }
}

/// Start the `<video>`. A refused `play()` (autoplay policy) leaves it paused.
pub fn play(video_id: &str) {
    if let Some(v) = video_element(video_id) {
        if let Err(e) = v.play() {
            warn(&format!("pharos: play() refused: {e:?}"));
        }
    }
}

pub fn pause(video_id: &str) {
    if let Some(v) = video_element(video_id) {
        if let Err(e) = v.pause() {
            warn(&format!("pharos: pause() refused: {e:?}"));
        }
    }
}

/// `playbackRate`: 1.0 is normal speed.
pub fn set_rate(video_id: &str, rate: f64) {
    if let Some(v) = video_element(video_id) {
        v.set_playback_rate(rate);
    }
}

/// Wall-clock unix milliseconds: the base the server's group clock speaks.
pub fn unix_ms() -> i64 {
    js_sys::Date::now() as i64
}

pub fn set_volume(video_id: &str, volume: f64, muted: bool) {
    if let Some(v) = video_element(video_id) {
        v.set_volume(volume.clamp(0.0, 1.0));
        v.set_muted(muted);
    }
}

/// Shows the text track whose `<track id>` is `track_id`, hiding the rest;
/// `None` hides them all.
pub fn show_text_track(video_id: &str, track_id: Option<&str>) {
    let Some(tracks) = video_element(video_id).and_then(|v| v.text_tracks()) else {
        return;
    };
    for i in 0..tracks.length() {
        if let Some(t) = tracks.get(i) {
            let on = track_id.is_some_and(|id| t.id() == id);
            t.set_mode(if on {
                web_sys::TextTrackMode::Showing
            } else {
                web_sys::TextTrackMode::Disabled
            });
        }
    }
}

/// The rendered width of the element `id`, 0 when it is gone.
pub fn element_width(id: &str) -> f64 {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .map_or(0.0, |e| e.get_bounding_client_rect().width())
}

/// The rendered width and height of the element `id`, zeros when it is gone.
pub fn element_size(id: &str) -> (f64, f64) {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .map_or((0.0, 0.0), |e| {
            let rect = e.get_bounding_client_rect();
            (rect.width(), rect.height())
        })
}

/// The viewport's width and height in CSS px, less any scrollbars: the box
/// a fixed element's percentages refer to.
pub fn viewport() -> (f64, f64) {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .map_or((0.0, 0.0), |e| {
            (f64::from(e.client_width()), f64::from(e.client_height()))
        })
}

/// Whether a key event's target is the element carrying `class` itself, not
/// a control inside it.
pub fn key_on_itself(evt: &dioxus::prelude::KeyboardEvent, class: &str) -> bool {
    use dioxus_web::WebEventExt;
    use wasm_bindgen::JsCast;
    evt.try_as_web_event()
        .and_then(|web| web.target())
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .is_some_and(|el| el.class_list().contains(class))
}

/// The element a pointer event started on.
fn pointer_target(evt: &dioxus::prelude::PointerEvent) -> Option<web_sys::Element> {
    use dioxus_web::WebEventExt;
    use wasm_bindgen::JsCast;
    evt.try_as_web_event()
        .and_then(|web| web.target())
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
}

/// Whether a pointer event started on a button rather than the surface
/// around it.
pub fn on_a_button(evt: &dioxus::prelude::PointerEvent) -> bool {
    pointer_target(evt)
        .and_then(|el| el.closest("button, a, input, [role=button]").ok().flatten())
        .is_some()
}

/// Send the rest of this pointer's events to the element it went down on,
/// so a drag keeps going when the pointer outruns the element.
pub fn capture_pointer(evt: &dioxus::prelude::PointerEvent) {
    if let Some(el) = pointer_target(evt) {
        if let Err(e) = el.set_pointer_capture(evt.pointer_id()) {
            warn(&format!("pharos: pointer capture failed: {e:?}"));
        }
    }
}

/// The value under `key` in `sessionStorage`, kept for the tab.
pub fn session_get(key: &str) -> Result<Option<String>, String> {
    storage(Kept::Tab)?
        .get_item(key)
        .map_err(|e| format!("{e:?}"))
}

/// Write `value` under `key` in `sessionStorage`.
pub fn session_set(key: &str, value: &str) -> Result<(), String> {
    storage(Kept::Tab)?
        .set_item(key, value)
        .map_err(|e| format!("{e:?}"))
}

/// How long a stored value lasts.
#[derive(Clone, Copy)]
enum Kept {
    /// `localStorage`: across visits.
    Device,
    /// `sessionStorage`: for the tab.
    Tab,
}

fn storage(kept: Kept) -> Result<web_sys::Storage, String> {
    let window = web_sys::window().ok_or("no window")?;
    let (storage, name) = match kept {
        Kept::Device => (window.local_storage(), "localStorage"),
        Kept::Tab => (window.session_storage(), "sessionStorage"),
    };
    storage
        .map_err(|e| format!("{e:?}"))?
        .ok_or_else(|| format!("{name} is unavailable"))
}

/// Moves keyboard focus to the element `id`, when it is there and focusable.
pub fn focus_by_id(id: &str) {
    use wasm_bindgen::JsCast;
    let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return;
    };
    if let Err(e) = el.focus() {
        warn(&format!("pharos: focus {id} refused: {e:?}"));
    }
}

/// Moves keyboard focus to the element `id`, if it is in the page.
pub fn focus(id: &str) {
    focus_by_id(id);
}

/// Enters fullscreen on the element `id`, or leaves it.
pub fn toggle_fullscreen(id: &str) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let result = if document.fullscreen_element().is_some() {
        document.exit_fullscreen();
        Ok(())
    } else {
        match document.get_element_by_id(id) {
            Some(e) => e.request_fullscreen(),
            None => Ok(()),
        }
    };
    if let Err(e) = result {
        warn(&format!("pharos: fullscreen refused: {e:?}"));
    }
}

// The Picture-in-Picture API is not among the enabled web-sys features.
#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = web_sys::Document, js_name = Document)]
    type PipDocument;
    /// Missing (`None`) in Firefox.
    #[wasm_bindgen(method, getter, js_name = pictureInPictureEnabled)]
    fn picture_in_picture_enabled(this: &PipDocument) -> Option<bool>;
    #[wasm_bindgen(method, getter, js_name = pictureInPictureElement)]
    fn picture_in_picture_element(this: &PipDocument) -> Option<web_sys::Element>;
    #[wasm_bindgen(method, catch, js_name = exitPictureInPicture)]
    fn exit_picture_in_picture(this: &PipDocument) -> Result<js_sys::Promise, JsValue>;

    #[wasm_bindgen(js_name = HTMLVideoElement)]
    type PipVideo;
    #[wasm_bindgen(method, catch, js_name = requestPictureInPicture)]
    fn request_picture_in_picture(this: &PipVideo) -> Result<js_sys::Promise, JsValue>;
}

fn pip_document() -> Option<PipDocument> {
    web_sys::window()
        .and_then(|w| w.document())
        .map(|d| d.unchecked_into())
}

/// Whether the browser can hand a video to its own picture-in-picture
/// window (`document.pictureInPictureEnabled`; missing in Firefox).
pub fn pip_supported() -> bool {
    pip_document().and_then(|d| d.picture_in_picture_enabled()) == Some(true)
}

/// Hands the video `id` to the browser's picture-in-picture window, or
/// takes it back when it is already there.
pub fn toggle_pip(id: &str) {
    let Some(document) = pip_document() else {
        return;
    };
    let (action, call) = if document.picture_in_picture_element().is_some() {
        ("close", document.exit_picture_in_picture())
    } else {
        let Some(video) = document.get_element_by_id(id) else {
            warn(&format!(
                "pharos: pip action=open refused reason=no-video id={id}"
            ));
            return;
        };
        (
            "open",
            video
                .unchecked_into::<PipVideo>()
                .request_picture_in_picture(),
        )
    };
    info(&format!("pharos: pip action={action} id={id}"));
    match call {
        Ok(promise) => spawn_detached(async move {
            if let Err(e) = wasm_bindgen_futures::JsFuture::from(promise).await {
                warn(&format!("pharos: pip action={action} refused reason={e:?}"));
            }
        }),
        Err(e) => warn(&format!("pharos: pip action={action} refused reason={e:?}")),
    }
}

fn seconds_to_ms(secs: f64) -> u64 {
    if secs.is_finite() && secs > 0.0 {
        // Saturating cast; a position past u64 milliseconds is not real.
        (secs * 1000.0) as u64
    } else {
        0
    }
}

/// A monotonic clock for the report cadence.
pub fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map_or(0.0, |p| p.now())
}

/// Milliseconds since the epoch, for times shown on the wall clock.
pub fn wall_clock_ms() -> f64 {
    js_sys::Date::now()
}

thread_local! {
    /// Built once: a formatter costs far more than a format, and the player
    /// asks on every position tick.
    static TIME_FORMAT: Option<js_sys::Intl::DateTimeFormat> = time_format();
}

fn time_format() -> Option<js_sys::Intl::DateTimeFormat> {
    let options = js_sys::Object::new();
    // The locale's own short time: "10:42 PM" in the US, "22:42" in the UK.
    js_sys::Reflect::set(&options, &"timeStyle".into(), &"short".into()).ok()?;
    Some(js_sys::Intl::DateTimeFormat::new(
        &js_sys::Array::new(),
        &options,
    ))
}

/// The hour and minute of `epoch_ms` in the browser's locale.
pub fn local_time(epoch_ms: f64) -> Option<String> {
    TIME_FORMAT.with(|format| {
        let format = format.as_ref()?;
        let date = js_sys::Date::new(&JsValue::from_f64(epoch_ms));
        format.format().call1(format, &date).ok()?.as_string()
    })
}

/// Run `fut` to completion even after the component that started it is
/// gone (a stop sent on unmount).
pub fn spawn_detached(fut: impl std::future::Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(fut);
}

/// An `IntersectionObserver` on one element, disconnected on drop.
pub struct SentinelWatch {
    observer: web_sys::IntersectionObserver,
    _callback: Closure<dyn FnMut(js_sys::Array)>,
}

impl Drop for SentinelWatch {
    fn drop(&mut self) {
        self.observer.disconnect();
    }
}

/// Call `on_visible` whenever any element named in `ids` is in view,
/// including when one is already in view as the watch starts. `None` when
/// none of them is in the page or the browser has no observer.
pub fn watch_sentinel(
    ids: &[&str],
    mut on_visible: impl FnMut() + 'static,
) -> Option<SentinelWatch> {
    let document = web_sys::window()?.document()?;
    let elements: Vec<_> = ids
        .iter()
        .filter_map(|id| document.get_element_by_id(id))
        .collect();
    if elements.is_empty() {
        return None;
    }
    let callback = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
        let seen = entries.iter().any(|e| {
            e.unchecked_into::<web_sys::IntersectionObserverEntry>()
                .is_intersecting()
        });
        if seen {
            on_visible();
        }
    });
    let observer = web_sys::IntersectionObserver::new(callback.as_ref().unchecked_ref()).ok()?;
    for element in &elements {
        observer.observe(element);
    }
    Some(SentinelWatch {
        observer,
        _callback: callback,
    })
}

/// A `wheel` listener that steps through letters while Shift is held;
/// removed on drop.
pub struct ShiftWheel(Closure<dyn FnMut(web_sys::WheelEvent)>);

impl Drop for ShiftWheel {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window
                .remove_event_listener_with_callback("wheel", self.0.as_ref().unchecked_ref());
        }
    }
}

/// Call `on_turn` with each Shift+wheel turn (positive is down), keeping the
/// page from scrolling or zooming on it; a plain wheel is left alone (B295).
/// Browsers turn Shift+wheel into a sideways scroll, so the turn is whichever
/// axis moved.
pub fn on_shift_wheel(mut on_turn: impl FnMut(f64) + 'static) -> ShiftWheel {
    let listener = Closure::<dyn FnMut(web_sys::WheelEvent)>::new(move |e: web_sys::WheelEvent| {
        if !e.shift_key() || e.ctrl_key() || e.meta_key() {
            return;
        }
        e.prevent_default();
        let scale = wheel_scale(&e);
        let turn = if e.delta_y() != 0.0 {
            e.delta_y()
        } else {
            e.delta_x()
        };
        on_turn(turn * scale);
    });
    if let Some(window) = web_sys::window() {
        let options = web_sys::AddEventListenerOptions::new();
        options.set_passive(false);
        let _ = window.add_event_listener_with_callback_and_add_event_listener_options(
            "wheel",
            listener.as_ref().unchecked_ref(),
            &options,
        );
    }
    ShiftWheel(listener)
}

/// Pixels per unit of a wheel event's delta: a line or page delta is ~16 or
/// ~800 px.
fn wheel_scale(e: &web_sys::WheelEvent) -> f64 {
    match e.delta_mode() {
        web_sys::WheelEvent::DOM_DELTA_LINE => 16.0,
        web_sys::WheelEvent::DOM_DELTA_PAGE => 800.0,
        _ => 1.0,
    }
}

/// The rail at or above an event's target.
fn rail_at(e: &web_sys::Event) -> Option<web_sys::HtmlElement> {
    e.target()?
        .dyn_into::<web_sys::Element>()
        .ok()?
        .closest(".pharos-rail-items")
        .ok()??
        .dyn_into::<web_sys::HtmlElement>()
        .ok()
}

/// A primary button held over a rail, and the rail it scrolls.
struct Held {
    rail: web_sys::HtmlElement,
    pointer: i32,
    drag: rails::Drag,
}

#[derive(Default)]
struct RailState {
    held: Option<Held>,
    glide: Option<(web_sys::HtmlElement, rails::Glide)>,
    /// When the last drag ended, so the click its release makes is dropped.
    dragged_at: Option<f64>,
}

type Handler = Box<dyn FnMut(web_sys::Event)>;

/// How long after a drag ends its click is dropped, in ms: the click
/// follows the release within one task, and a later real click must work.
const DRAG_CLICK_MS: f64 = 100.0;

/// Let a dragged rail go: it snaps and selects again.
fn end_drag(held: &Held, pointer: i32) {
    let _ = held.rail.class_list().remove_1("is-dragging");
    let _ = held.rail.release_pointer_capture(pointer);
}

/// The window listeners behind rail gestures; removed on drop.
pub struct RailGestures(Vec<WindowListener>);

/// An event name, whether it listens in the capture phase, and its handler.
struct WindowListener(&'static str, bool, Closure<dyn FnMut(web_sys::Event)>);

impl Drop for RailGestures {
    fn drop(&mut self) {
        let Some(window) = web_sys::window() else {
            return;
        };
        for WindowListener(name, capture, listener) in &self.0 {
            let _ = window.remove_event_listener_with_callback_and_bool(
                name,
                listener.as_ref().unchecked_ref(),
                *capture,
            );
        }
    }
}

/// Scroll rails further on a sideways wheel notch, and with a mouse or pen
/// dragged across them; a click that ends a drag is dropped so it opens
/// nothing (B315). Touch is left to the browser's own panning.
pub fn rail_gestures() -> RailGestures {
    let state = Rc::new(RefCell::new(RailState::default()));
    let reduced = prefers("(prefers-reduced-motion: reduce)");
    let mut listeners: Vec<(&'static str, bool, Handler)> = Vec::new();

    let st = state.clone();
    listeners.push((
        "wheel",
        false,
        Box::new(move |e: web_sys::Event| {
            let Some(e) = e.dyn_ref::<web_sys::WheelEvent>() else {
                return;
            };
            if e.ctrl_key() || e.meta_key() {
                return;
            }
            let Some(rail) = rail_at(e) else {
                return;
            };
            let scale = wheel_scale(e);
            let lines = e.delta_mode() != web_sys::WheelEvent::DOM_DELTA_PIXEL;
            let Some(step) = rails::wheel_step(
                e.delta_x() * scale,
                e.delta_y() * scale,
                e.shift_key(),
                lines,
            ) else {
                return;
            };
            let mut st = st.borrow_mut();
            let last = st
                .glide
                .as_ref()
                .filter(|(r, _)| *r == rail)
                .map(|(_, g)| *g);
            let max = f64::from(rail.scroll_width() - rail.client_width());
            let left = f64::from(rail.scroll_left());
            let Some(glide) = rails::glide(last, e.time_stamp(), left, max, step) else {
                return;
            };
            e.prevent_default();
            let options = web_sys::ScrollToOptions::new();
            options.set_left(glide.to);
            options.set_behavior(if reduced {
                web_sys::ScrollBehavior::Instant
            } else {
                web_sys::ScrollBehavior::Smooth
            });
            rail.scroll_to_with_scroll_to_options(&options);
            st.glide = Some((rail, glide));
        }),
    ));

    let st = state.clone();
    listeners.push((
        "pointerdown",
        false,
        Box::new(move |e: web_sys::Event| {
            let Some(e) = e.dyn_ref::<web_sys::PointerEvent>() else {
                return;
            };
            let mut st = st.borrow_mut();
            st.dragged_at = None;
            if e.button() != 0 || e.pointer_type() == "touch" {
                return;
            }
            let Some(rail) = rail_at(e) else {
                return;
            };
            let left = f64::from(rail.scroll_left());
            st.held = Some(Held {
                rail,
                pointer: e.pointer_id(),
                drag: rails::Drag::new(f64::from(e.client_x()), left),
            });
        }),
    ));

    let st = state.clone();
    listeners.push((
        "pointermove",
        false,
        Box::new(move |e: web_sys::Event| {
            let Some(e) = e.dyn_ref::<web_sys::PointerEvent>() else {
                return;
            };
            let mut st = st.borrow_mut();
            if st.held.as_ref().map(|h| h.pointer) != Some(e.pointer_id()) {
                return;
            }
            // A press let go where its pointerup never reaches the window
            // (outside it before a drag, or over a system overlay).
            if e.buttons() & 1 == 0 {
                if let Some(held) = st.held.take().filter(|h| h.drag.dragging) {
                    end_drag(&held, held.pointer);
                }
                return;
            }
            let st = &mut *st;
            let Some(held) = st.held.as_mut() else {
                return;
            };
            let was_dragging = held.drag.dragging;
            let Some(left) = held.drag.moved(f64::from(e.client_x())) else {
                return;
            };
            if !was_dragging {
                // The drag moves the rail itself, so a glide set before it
                // no longer says where the rail is heading.
                st.glide = None;
                // Capture only now: capturing on press would retarget a plain
                // click away from the poster.
                let _ = held.rail.set_pointer_capture(e.pointer_id());
                let _ = held.rail.class_list().add_1("is-dragging");
                if let Some(selection) =
                    web_sys::window().and_then(|w| w.get_selection().ok().flatten())
                {
                    let _ = selection.remove_all_ranges();
                }
            }
            held.rail.set_scroll_left(left.round() as i32);
            e.prevent_default();
        }),
    ));

    for name in ["pointerup", "pointercancel"] {
        let st = state.clone();
        listeners.push((
            name,
            false,
            Box::new(move |e: web_sys::Event| {
                let Some(e) = e.dyn_ref::<web_sys::PointerEvent>() else {
                    return;
                };
                let mut st = st.borrow_mut();
                if st.held.as_ref().map(|h| h.pointer) != Some(e.pointer_id()) {
                    return;
                }
                let Some(held) = st.held.take() else {
                    return;
                };
                if held.drag.dragging {
                    end_drag(&held, e.pointer_id());
                    st.dragged_at = Some(e.time_stamp());
                }
            }),
        ));
    }

    // Capture on the window runs before Dioxus's own listener at the app
    // root, so a poster link never sees the click.
    let st = state.clone();
    listeners.push((
        "click",
        true,
        Box::new(move |e: web_sys::Event| {
            let Some(at) = st.borrow_mut().dragged_at.take() else {
                return;
            };
            if e.time_stamp() - at < DRAG_CLICK_MS {
                e.prevent_default();
                e.stop_immediate_propagation();
            }
        }),
    ));

    // A pressed link or image would start the browser's own drag of it,
    // which ends the pointer events.
    listeners.push((
        "dragstart",
        false,
        Box::new(move |e: web_sys::Event| {
            if rail_at(&e).is_some() {
                e.prevent_default();
            }
        }),
    ));

    let Some(window) = web_sys::window() else {
        return RailGestures(Vec::new());
    };
    let options = web_sys::AddEventListenerOptions::new();
    options.set_passive(false);
    let held = listeners
        .into_iter()
        .map(|(name, capture, f)| {
            let listener = Closure::<dyn FnMut(web_sys::Event)>::wrap(f);
            options.set_capture(capture);
            let _ = window.add_event_listener_with_callback_and_add_event_listener_options(
                name,
                listener.as_ref().unchecked_ref(),
                &options,
            );
            WindowListener(name, capture, listener)
        })
        .collect();
    RailGestures(held)
}

/// A page-wide `keydown` listener; removed on drop.
pub struct KeyListener(Closure<dyn FnMut(web_sys::KeyboardEvent)>);

impl Drop for KeyListener {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window
                .remove_event_listener_with_callback("keydown", self.0.as_ref().unchecked_ref());
        }
    }
}

/// Call `on_key` with every key pressed anywhere in the page (its key, and
/// whether Ctrl and the meta key are held); when it answers true the press
/// is taken, so the browser does not act on it too. A held key's repeats
/// are not passed on, but a taken press keeps its repeats from the browser
/// as well: a held Ctrl+K must not open Firefox's web search.
pub fn on_key(mut on_key: impl FnMut(&str, bool, bool) -> bool + 'static) -> KeyListener {
    let mut taken = false;
    let listener =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if !e.repeat() {
                taken = on_key(&e.key(), e.ctrl_key(), e.meta_key());
            }
            if taken {
                e.prevent_default();
            }
        });
    if let Some(window) = web_sys::window() {
        let _ =
            window.add_event_listener_with_callback("keydown", listener.as_ref().unchecked_ref());
    }
    KeyListener(listener)
}

/// The page-wide track skips, removed on drop: a window `keydown`
/// listener and the media session's `previoustrack` / `nexttrack` actions.
pub struct TrackSkips {
    keys: Closure<dyn FnMut(web_sys::KeyboardEvent)>,
    _actions: [Closure<dyn FnMut()>; 2],
}

thread_local! {
    /// The media session actions [`on_track_skip`] made, for
    /// [`offer_track_skips`] to set or clear.
    static SKIP_ACTIONS: RefCell<Option<[js_sys::Function; 2]>> = const { RefCell::new(None) };
    /// What [`offer_track_skips`] last offered, so a redraw that changes
    /// nothing does not set the handlers again.
    static SKIPS_OFFERED: Cell<Option<(bool, bool)>> = const { Cell::new(None) };
}

impl Drop for TrackSkips {
    fn drop(&mut self) {
        offer_track_skips(false, false);
        SKIP_ACTIONS.set(None);
        SKIPS_OFFERED.set(None);
        if let Some(window) = web_sys::window() {
            let _ = window.remove_event_listener_with_callback_and_bool(
                "keydown",
                self.keys.as_ref().unchecked_ref(),
                true,
            );
        }
    }
}

/// `navigator.mediaSession`, where the browser has one. web-sys keeps it
/// behind `web_sys_unstable_apis`, so it is reached by name.
fn media_session() -> Option<JsValue> {
    let navigator = web_sys::window()?.navigator();
    js_sys::Reflect::get(&navigator, &JsValue::from_str("mediaSession"))
        .ok()
        .filter(|s| !s.is_undefined() && !s.is_null())
}

/// `mediaSession.setActionHandler(action, handler)`; `None` clears it.
fn set_action_handler(session: &JsValue, action: &str, handler: Option<&js_sys::Function>) {
    let Some(set) = js_sys::Reflect::get(session, &JsValue::from_str("setActionHandler"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
    else {
        return;
    };
    let handler = handler.map_or(JsValue::NULL, |f| f.clone().into());
    // A browser that does not know the action throws; it then has no
    // button for it either.
    if let Err(e) = set.call2(session, &JsValue::from_str(action), &handler) {
        info(&format!("pharos: media session {action} refused: {e:?}"));
    }
}

/// Call `on_skip` for a track skip asked for anywhere in the page (B562):
/// Shift+P / Shift+N with the focus outside a text field, or the media
/// keys and the system's media controls, which reach the page as the media
/// session's `previoustrack` / `nexttrack`. Its answer for a key is
/// whether it took the press. The actions are offered to the system only
/// once [`offer_track_skips`] says there is somewhere to go.
pub fn on_track_skip(on_skip: impl FnMut(Skip, &'static str) -> bool + 'static) -> TrackSkips {
    let on_skip = Rc::new(RefCell::new(on_skip));
    let keys = {
        let on_skip = Rc::clone(&on_skip);
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            // Shift, not Caps Lock; Ctrl+Shift+P is a private window.
            if !e.shift_key()
                || e.ctrl_key()
                || e.alt_key()
                || e.meta_key()
                || e.repeat()
                || e.default_prevented()
            {
                return;
            }
            let skip = match e.key().as_str() {
                "P" | "p" => Skip::Previous,
                "N" | "n" => Skip::Next,
                _ => return,
            };
            if in_text_field(&e) {
                return;
            }
            if (on_skip.borrow_mut())(skip, "keyboard") {
                e.prevent_default();
            }
        })
    };
    let action = |skip: Skip| {
        let on_skip = Rc::clone(&on_skip);
        Closure::<dyn FnMut()>::new(move || {
            (on_skip.borrow_mut())(skip, "media_session");
        })
    };
    let actions = [action(Skip::Previous), action(Skip::Next)];
    SKIP_ACTIONS.set(Some(
        actions
            .each_ref()
            .map(|a| a.as_ref().unchecked_ref::<js_sys::Function>().clone()),
    ));
    // Capture phase: a slider's own keydown handler stops propagation, and
    // Shift+P / Shift+N still skip with the focus on the seek or volume bar.
    if let Some(window) = web_sys::window() {
        let _ = window.add_event_listener_with_callback_and_bool(
            "keydown",
            keys.as_ref().unchecked_ref(),
            true,
        );
    }
    TrackSkips {
        keys,
        _actions: actions,
    }
}

/// Offer the system the previous and next track where there is one, so its
/// media controls grey out a button that would do nothing.
pub fn offer_track_skips(previous: bool, next: bool) {
    if SKIPS_OFFERED.replace(Some((previous, next))) == Some((previous, next)) {
        return;
    }
    let Some(session) = media_session() else {
        return;
    };
    SKIP_ACTIONS.with_borrow(|actions| {
        let [on_previous, on_next] = match actions {
            Some([p, n]) => [Some(p), Some(n)],
            None => [None, None],
        };
        set_action_handler(&session, "previoustrack", on_previous.filter(|_| previous));
        set_action_handler(&session, "nexttrack", on_next.filter(|_| next));
    });
}

/// The key went to a field that takes text, where Shift+N is a capital N;
/// a slider or checkbox keeps the skip.
fn in_text_field(e: &web_sys::KeyboardEvent) -> bool {
    e.target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .and_then(|el| {
            el.closest(
                "input:not([type=range], [type=checkbox], [type=radio], [type=button]), \
                 textarea, select, [contenteditable]:not([contenteditable=false])",
            )
            .ok()
            .flatten()
        })
        .is_some()
}

/// How the palette's shortcut is written on this device: Cmd on Apple's.
pub fn palette_keys() -> &'static str {
    let apple = web_sys::window()
        .and_then(|w| w.navigator().user_agent().ok())
        .is_some_and(|agent| ["Mac", "iPhone", "iPad"].iter().any(|a| agent.contains(a)));
    if apple {
        "⌘K"
    } else {
        "Ctrl+K"
    }
}

/// The tiles of the browse grid, in order.
fn grid_tiles() -> Option<web_sys::HtmlCollection> {
    let grid = web_sys::window()?
        .document()?
        .query_selector(".pharos-browse .pharos-grid")
        .ok()??;
    Some(grid.children())
}

/// The index of the first browse tile whose bottom is below the top of the
/// viewport. Tiles run top to bottom, so a binary search finds it.
pub fn first_visible_tile() -> Option<u32> {
    let tiles = grid_tiles()?;
    let (mut lo, mut hi) = (0, tiles.length());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let bottom = tiles.item(mid)?.get_bounding_client_rect().bottom();
        if bottom <= 0.0 {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    (lo < tiles.length()).then_some(lo)
}

/// Whether the browse tile at `at` is at least partly in the viewport.
pub fn tile_in_view(at: u32) -> bool {
    let Some(height) = web_sys::window()
        .and_then(|w| w.inner_height().ok())
        .and_then(|h| h.as_f64())
    else {
        return false;
    };
    grid_tiles()
        .and_then(|tiles| tiles.item(at))
        .map(|tile| tile.get_bounding_client_rect())
        .is_some_and(|r| r.bottom() > 0.0 && r.top() < height)
}

/// Scrolls the browse tile at `at` to the top of the page; `false` when it
/// is not in the page yet.
pub fn scroll_to_tile(at: u32) -> bool {
    match grid_tiles().and_then(|tiles| tiles.item(at)) {
        Some(tile) => {
            tile.scroll_into_view_with_bool(true);
            true
        }
        None => false,
    }
}

/// Scroll the element `id` so its child at `at` sits in the middle of it.
/// The element must be the children's offset parent (`position: relative`).
/// False when there is no such child yet.
pub fn center_child(id: &str, at: u32) -> bool {
    let Some(panel) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    else {
        return false;
    };
    let Some(child) = panel
        .children()
        .item(at)
        .and_then(|c| c.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return false;
    };
    let middle = child.offset_top() + child.offset_height() / 2;
    panel.set_scroll_top((middle - panel.client_height() / 2).max(0));
    true
}

/// A `pagehide` listener; removed on drop.
pub struct PageHide(Closure<dyn FnMut(web_sys::Event)>);

impl Drop for PageHide {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window
                .remove_event_listener_with_callback("pagehide", self.0.as_ref().unchecked_ref());
        }
    }
}

/// Call `on_hide` when the page is closed or navigated away from.
pub fn on_page_hide(mut on_hide: impl FnMut() + 'static) -> PageHide {
    let listener = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| on_hide());
    if let Some(window) = web_sys::window() {
        let _ =
            window.add_event_listener_with_callback("pagehide", listener.as_ref().unchecked_ref());
    }
    PageHide(listener)
}

/// The engine for `kind`, loading hls.js first when it is needed.
pub async fn engine(kind: Engine) -> Result<Box<dyn PlaybackEngine>, String> {
    match kind {
        Engine::Direct | Engine::NativeHls => Ok(Box::new(NativeVideo::default())),
        Engine::HlsJs => {
            load_script(HLSJS_SRC, "Hls").await?;
            Ok(Box::new(HlsJs::default()))
        }
        Engine::Unsupported => Err("this browser cannot play HLS".into()),
    }
}

/// Plays a URL through the `<video>` element's own `src`.
#[derive(Default)]
struct NativeVideo {
    video: Option<web_sys::HtmlMediaElement>,
    on_error: Option<Closure<dyn FnMut(web_sys::Event)>>,
}

impl PlaybackEngine for NativeVideo {
    fn attach(
        &mut self,
        video_id: &str,
        url: &str,
        start_ms: u64,
        on_fatal: OnFatal,
        _on_fragment: OnFragment,
    ) -> Result<(), String> {
        let video = video_element(video_id).ok_or_else(|| format!("no <video id={video_id}>"))?;
        let on_error = {
            let (target, url) = (video.clone(), url.to_owned());
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                // An error after the src is removed on drop is not playback failing.
                if target.get_attribute("src").is_none() {
                    return;
                }
                let (code, message) = target
                    .error()
                    .map_or((0, String::new()), |e| (e.code(), e.message()));
                warn(&format!(
                    "pharos: playback error engine=native code={code} message={message} action=fail url={url}"
                ));
                report_event(ClientEvent {
                    kind: ClientEventKind::PlaybackError,
                    level: ClientLevel::Error,
                    route: current_path(),
                    fields: vec![
                        ("engine", "native".to_owned()),
                        ("code", code.to_string()),
                        ("message", message.clone()),
                        ("action", "fail".to_owned()),
                    ],
                });
                on_fatal(format!(
                    "the browser could not play this stream (code {code}) {message}"
                ));
            })
        };
        video
            .add_event_listener_with_callback("error", on_error.as_ref().unchecked_ref())
            .map_err(|e| format!("error listener: {e:?}"))?;
        if start_ms > 0 {
            let target = video.clone();
            let seek = Closure::once_into_js(move || {
                #[allow(clippy::cast_precision_loss)]
                target.set_current_time(start_ms as f64 / 1000.0);
            });
            let once = web_sys::AddEventListenerOptions::new();
            once.set_once(true);
            video
                .add_event_listener_with_callback_and_add_event_listener_options(
                    "loadedmetadata",
                    seek.unchecked_ref(),
                    &once,
                )
                .map_err(|e| format!("seek listener: {e:?}"))?;
        }
        video.set_src(url);
        self.video = Some(video);
        self.on_error = Some(on_error);
        Ok(())
    }
}

impl Drop for NativeVideo {
    fn drop(&mut self) {
        if let (Some(video), Some(on_error)) = (self.video.as_ref(), self.on_error.take()) {
            let _ = video
                .remove_event_listener_with_callback("error", on_error.as_ref().unchecked_ref());
        }
        if let Some(video) = self.video.take() {
            let _ = video.pause();
            let _ = video.remove_attribute("src");
            video.load();
        }
    }
}

/// The vendored hls.js (ADR-0035), served from this origin so the CSP's
/// `script-src 'self'` covers it.
const HLSJS_SRC: &str = "/ui/assets/vendor/hls.min.js";

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = Hls)]
    #[derive(Clone)]
    type HlsPlayer;
    #[wasm_bindgen(constructor, js_class = "Hls")]
    fn new(config: &JsValue) -> HlsPlayer;
    #[wasm_bindgen(method, js_class = "Hls", js_name = loadSource)]
    fn load_source(this: &HlsPlayer, url: &str);
    #[wasm_bindgen(method, js_class = "Hls", js_name = attachMedia)]
    fn attach_media(this: &HlsPlayer, media: &web_sys::HtmlMediaElement);
    #[wasm_bindgen(method, js_class = "Hls")]
    fn on(this: &HlsPlayer, event: &str, listener: &js_sys::Function);
    #[wasm_bindgen(method, js_class = "Hls")]
    fn destroy(this: &HlsPlayer);
    #[wasm_bindgen(method, js_class = "Hls", js_name = startLoad)]
    fn start_load(this: &HlsPlayer);
    #[wasm_bindgen(method, js_class = "Hls", js_name = recoverMediaError)]
    fn recover_media_error(this: &HlsPlayer);
}

/// Add the `<script>` at `src` once and wait for it; `global` is the name
/// it defines, which shows it already ran.
async fn load_script(src: &str, global: &str) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    if js_sys::Reflect::has(&window, &JsValue::from_str(global)).unwrap_or(false) {
        return Ok(());
    }
    let document = window.document().ok_or("no document")?;
    let script = document
        .create_element("script")
        .map_err(|e| format!("{e:?}"))?
        .dyn_into::<web_sys::HtmlScriptElement>()
        .map_err(|_| "not a script element")?;
    script.set_src(src);
    let loaded = js_sys::Promise::new(&mut |resolve, reject| {
        script.set_onload(Some(&resolve));
        script.set_onerror(Some(&reject));
    });
    document
        .head()
        .ok_or("no <head>")?
        .append_child(&script)
        .map_err(|e| format!("{e:?}"))?;
    wasm_bindgen_futures::JsFuture::from(loaded)
        .await
        .map_err(|_| format!("could not load {src}"))?;
    Ok(())
}

/// The vendored JASSUB (ADR-0057), served from this origin like hls.js.
const JASSUB_DIR: &str = "/ui/assets/vendor/jassub";
/// How long the renderer may take to load its wasm, fonts and script and get
/// ready to draw before the player gives up on it and keeps the WebVTT; a
/// cold extraction of an embedded track takes the server up to about 30 s.
const STYLED_READY_MS: u32 = 45_000;

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = JASSUB)]
    type Jassub;
    #[wasm_bindgen(catch, constructor, js_class = "JASSUB")]
    fn new(options: &JsValue) -> Result<Jassub, JsValue>;
    #[wasm_bindgen(method, getter, js_class = "JASSUB")]
    fn ready(this: &Jassub) -> js_sys::Promise;
    #[wasm_bindgen(method, js_class = "JASSUB")]
    fn destroy(this: &Jassub) -> js_sys::Promise;
}

/// An ASS/SSA script drawn over the `<video>` by libass in a worker. It
/// lays its canvas right after the video, so the video's parent must hold
/// nothing else Dioxus diffs. Dropping it removes the canvas and the worker.
pub struct StyledSubtitles {
    jassub: Jassub,
    live: Rc<std::cell::Cell<bool>>,
}

impl Drop for StyledSubtitles {
    fn drop(&mut self) {
        self.live.set(false);
        let _ = self.jassub.destroy();
    }
}

/// Starts drawing the script at `url` over `<video id=video_id>`. While it
/// is not dropped, `report` hears `Ok` once the renderer is ready, or why it
/// failed or did not get ready in time.
pub async fn styled_subtitles(
    video_id: &str,
    url: &str,
    report: Rc<dyn Fn(Result<(), String>)>,
) -> Result<StyledSubtitles, String> {
    let video = video_element(video_id).ok_or_else(|| format!("no <video id={video_id}>"))?;
    load_script(&format!("{JASSUB_DIR}/jassub.js"), "JASSUB").await?;
    let set = |o: &js_sys::Object, k: &str, v: &JsValue| {
        js_sys::Reflect::set(o, &JsValue::from_str(k), v).map_err(|e| format!("{e:?}"))
    };
    let fonts = js_sys::Object::new();
    set(
        &fonts,
        "liberation sans",
        &JsValue::from_str(&format!("{JASSUB_DIR}/default.woff2")),
    )?;
    let options = js_sys::Object::new();
    set(&options, "video", &video)?;
    set(&options, "subUrl", &JsValue::from_str(url))?;
    for (key, file) in [
        ("workerUrl", "jassub-worker.js"),
        ("wasmUrl", "jassub-worker.wasm"),
        ("modernWasmUrl", "jassub-worker-modern.wasm"),
    ] {
        set(
            &options,
            key,
            &JsValue::from_str(&format!("{JASSUB_DIR}/{file}")),
        )?;
    }
    set(&options, "availableFonts", &fonts)?;
    set(
        &options,
        "defaultFont",
        &JsValue::from_str("liberation sans"),
    )?;
    // Not the Local Font Access API: it asks the viewer for a permission.
    set(&options, "queryFonts", &JsValue::FALSE)?;
    let jassub = Jassub::new(&options).map_err(|e| format!("{e:?}"))?;
    let live = Rc::new(std::cell::Cell::new(true));
    let ready = wasm_bindgen_futures::JsFuture::from(jassub.ready());
    spawn_detached({
        let live = Rc::clone(&live);
        async move {
            let ready = std::pin::pin!(ready);
            let timeout = std::pin::pin!(sleep_ms(STYLED_READY_MS));
            let result = match futures_util::future::select(ready, timeout).await {
                futures_util::future::Either::Left((Ok(_), _)) => Ok(()),
                futures_util::future::Either::Left((Err(e), _)) => Err(js_message(&e)),
                futures_util::future::Either::Right(_) => {
                    Err(format!("not ready after {STYLED_READY_MS} ms"))
                }
            };
            if live.get() {
                report(result);
            }
        }
    });
    Ok(StyledSubtitles { jassub, live })
}

/// A thrown value's `message`, or its debug form.
fn js_message(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &JsValue::from_str("message"))
        .ok()
        .and_then(|m| m.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}

/// Plays an HLS playlist through hls.js and Media Source Extensions.
#[derive(Default)]
struct HlsJs {
    player: Option<HlsPlayer>,
    _listeners: Vec<Closure<dyn FnMut(JsValue, JsValue)>>,
}

impl PlaybackEngine for HlsJs {
    fn attach(
        &mut self,
        video_id: &str,
        url: &str,
        start_ms: u64,
        on_fatal: OnFatal,
        on_fragment: OnFragment,
    ) -> Result<(), String> {
        let video = video_element(video_id).ok_or_else(|| format!("no <video id={video_id}>"))?;
        let config = js_sys::Object::new();
        #[allow(clippy::cast_precision_loss)]
        let start = if start_ms > 0 {
            start_ms as f64 / 1000.0
        } else {
            -1.0
        };
        js_sys::Reflect::set(&config, &"startPosition".into(), &start.into())
            .map_err(|e| format!("{e:?}"))?;
        let player = HlsPlayer::new(&config);
        let budget = Rc::new(RefCell::new(RecoveryBudget::new()));
        let on_error = {
            let (player, budget, url) = (player.clone(), Rc::clone(&budget), url.to_owned());
            Closure::<dyn FnMut(JsValue, JsValue)>::new(move |_event, data: JsValue| {
                let field = |name: &str| {
                    js_sys::Reflect::get(&data, &name.into())
                        .ok()
                        .and_then(|v| v.as_string().or_else(|| v.as_bool().map(|b| b.to_string())))
                        .unwrap_or_default()
                };
                let (kind, details, fatal) = (field("type"), field("details"), field("fatal"));
                if fatal != "true" {
                    warn(&format!(
                        "pharos: playback error engine=hlsjs type={kind} details={details} fatal=false url={url}"
                    ));
                    report_event(hls_event(ClientLevel::Warn, &kind, &details, "none", 0));
                    return;
                }
                let mut budget = budget.borrow_mut();
                let action = budget.on_fatal(&kind, &details);
                let attempt = budget.attempts(&kind);
                warn(&format!(
                    "pharos: playback error engine=hlsjs type={kind} details={details} fatal=true action={} attempt={attempt} url={url}",
                    action.label(),
                ));
                report_event(hls_event(
                    ClientLevel::Error,
                    &kind,
                    &details,
                    action.label(),
                    attempt,
                ));
                match action {
                    Recovery::StartLoad => player.start_load(),
                    Recovery::RecoverMedia => player.recover_media_error(),
                    Recovery::Fail => on_fatal(format!("the stream failed ({kind}: {details})")),
                }
            })
        };
        let on_loaded = {
            let budget = Rc::clone(&budget);
            Closure::<dyn FnMut(JsValue, JsValue)>::new(move |_, data: JsValue| {
                budget.borrow_mut().progressed();
                if let Some(load) = fragment_load(&data) {
                    on_fragment(load);
                }
            })
        };
        player.on("hlsError", on_error.as_ref().unchecked_ref());
        player.on("hlsFragLoaded", on_loaded.as_ref().unchecked_ref());
        player.load_source(url);
        player.attach_media(&video);
        self.player = Some(player);
        self._listeners = vec![on_error, on_loaded];
        Ok(())
    }
}

/// A number at `path` in the hls.js object `from`, 0 when absent.
fn js_number(from: &JsValue, path: &[&str]) -> f64 {
    let mut at = from.clone();
    for key in path {
        at = js_sys::Reflect::get(&at, &(*key).into()).unwrap_or(JsValue::UNDEFINED);
    }
    at.as_f64().unwrap_or(0.0)
}

/// The download an `hlsFragLoaded` event reports; `None` for audio and
/// subtitle fragments and for ones with no usable timing.
fn fragment_load(data: &JsValue) -> Option<FragmentLoad> {
    let kind = js_sys::Reflect::get(data, &"frag".into())
        .ok()
        .and_then(|f| js_sys::Reflect::get(&f, &"type".into()).ok())
        .and_then(|t| t.as_string());
    if kind.as_deref() != Some("main") {
        return None;
    }
    let bytes = js_number(data, &["stats", "loaded"]);
    let end = js_number(data, &["stats", "loading", "end"]);
    let first = js_number(data, &["stats", "loading", "first"]);
    let start = if first > 0.0 {
        first
    } else {
        js_number(data, &["stats", "loading", "start"])
    };
    let seconds = js_number(data, &["frag", "duration"]);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let load = FragmentLoad {
        bytes: bytes.max(0.0) as u64,
        load_ms: (end - start).max(0.0) as u64,
        media_ms: (seconds * 1000.0).max(0.0) as u64,
    };
    (load.bytes > 0 && load.load_ms > 0 && load.media_ms > 0).then_some(load)
}

impl Drop for HlsJs {
    fn drop(&mut self) {
        if let Some(player) = self.player.take() {
            player.destroy();
        }
    }
}

pub fn warn(message: &str) {
    web_sys::console::warn_1(&message.into());
}

#[derive(Default)]
struct Telemetry {
    sink: Sink<NativeApi>,
    failure_logged: bool,
    /// The signed-in user's opt-in as last read from or written to `Prefs`.
    /// Off until read, and again after sign-out.
    opted_in: bool,
}

thread_local! {
    static TELEMETRY: RefCell<Telemetry> = RefCell::new(Telemetry::default());
}

/// Whether the signed-in user has opted in to sending diagnostics, as
/// last read from the server. Off before sign-in and until the read lands.
pub fn opted_in() -> bool {
    TELEMETRY.with(|t| t.borrow().opted_in)
}

fn remember_opt_in(on: bool) {
    TELEMETRY.with(|t| t.borrow_mut().opted_in = on);
}

/// Forgets the opt-in, so nothing is sent after sign-out until the next
/// user's choice is read.
pub fn forget_opt_in() {
    remember_opt_in(false);
}

fn prefs_call(action: &str) -> CallInfo {
    CallInfo {
        ui_route: "telemetry".into(),
        ui_action: action.into(),
        ..Default::default()
    }
}

/// Reads the user's opt-in from `Prefs` and remembers it.
pub async fn load_opted_in(api: &NativeApi) -> Result<bool, String> {
    let raw = api
        .pref(&prefs_call("opt_in_read"), diagnostics::OPT_IN_PREF)
        .await
        .map_err(|e| e.to_string())?;
    let on = diagnostics::opt_in_from(raw.as_deref());
    remember_opt_in(on);
    Ok(on)
}

/// Writes the user's opt-in to `Prefs`; remembered only once the server
/// has stored it.
pub async fn set_opted_in(api: &NativeApi, on: bool) -> Result<(), String> {
    let value: &[u8] = if on { b"true" } else { b"false" };
    api.set_pref(&prefs_call("opt_in_write"), diagnostics::OPT_IN_PREF, value)
        .await
        .map_err(|e| e.to_string())?;
    remember_opt_in(on);
    Ok(())
}

/// Removes the `localStorage` opt-in older builds wrote. Its value is not
/// read: the choice now lives on the server, and a stored `true` is not a
/// consent the user gave there.
fn remove_legacy_opt_in() {
    let storage = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    if let Some(storage) = storage {
        let _ = storage.remove_item(diagnostics::LEGACY_OPT_IN_KEY);
    }
}

/// Reads the user's opt-in, then sends the session's client events from
/// now on, and the held ones. An unreadable opt-in counts as off.
pub fn install_telemetry(api: &NativeApi) {
    remove_legacy_opt_in();
    let api = api.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let opted = load_opted_in(&api).await.unwrap_or_else(|e| {
            warn(&format!("pharos: diagnostics opt-in unreadable: {e}"));
            false
        });
        let held = TELEMETRY.with(|t| t.borrow_mut().sink.install(api.clone(), opted));
        if !held.is_empty() {
            send(api, held);
        }
    });
}

/// Submits one client event without waiting for it, if opted in. Opted out,
/// nothing is sent or held.
pub fn report_event(event: ClientEvent) {
    let opted = opted_in();
    let submit = TELEMETRY.with(|t| t.borrow_mut().sink.report(opted, event));
    if let Some((api, event)) = submit {
        send(api, vec![event]);
    }
}

fn send(api: NativeApi, events: Vec<ClientEvent>) {
    wasm_bindgen_futures::spawn_local(async move {
        // Checked again here: the opt-in may have been turned off since the event was raised.
        if !opted_in() {
            return;
        }
        if let Err(e) = api.submit_events(&prefs_call("submit"), &events).await {
            // Logged once: a dead socket fails every later batch the same way.
            let first =
                TELEMETRY.with(|t| !std::mem::replace(&mut t.borrow_mut().failure_logged, true));
            if first {
                warn(&format!("pharos: client event submit failed: {e}"));
            }
        }
    });
}

fn hls_event(
    level: ClientLevel,
    kind: &str,
    details: &str,
    action: &str,
    attempt: u32,
) -> ClientEvent {
    ClientEvent {
        kind: ClientEventKind::PlaybackError,
        level,
        route: current_path(),
        fields: vec![
            ("engine", "hlsjs".to_owned()),
            ("type", kind.to_owned()),
            ("details", details.to_owned()),
            ("action", action.to_owned()),
            ("attempt", attempt.to_string()),
        ],
    }
}

pub fn info(message: &str) {
    web_sys::console::info_1(&message.into());
}

/// The theme name stored under `key`. `Err` carries the browser's reason
/// when storage cannot be read at all.
pub fn stored_theme(key: &str) -> Result<Option<String>, String> {
    storage(Kept::Device)?
        .get_item(key)
        .map_err(|e| format!("{e:?}"))
}

/// Set every token on `<html>` in one `style` write, so the page restyles
/// once.
pub fn apply_tokens(declarations: &[(String, String)]) -> Result<(), String> {
    let root = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .ok_or("no document element")?;
    let style: String = declarations
        .iter()
        .map(|(name, value)| format!("{name}: {value}; "))
        .collect();
    root.set_attribute("style", style.trim_end())
        .map_err(|e| format!("{e:?}"))
}

/// Write `value` under `key` in `localStorage`.
pub fn store_theme(key: &str, value: &str) -> Result<(), String> {
    storage(Kept::Device)?
        .set_item(key, value)
        .map_err(|e| format!("{e:?}"))
}

/// Set `data-subtitle-size` on `<html>` so `video::cue` scales to `size`.
pub fn apply_subtitle_size(size: SubtitleSize) {
    let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    else {
        return;
    };
    if let Err(e) = root.set_attribute("data-subtitle-size", size.as_str()) {
        warn(&format!("pharos: subtitle_size apply failed error={e:?}"));
    }
}

/// Whether the media `query` matches now, false when the browser lacks it.
pub fn prefers(query: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.match_media(query).ok().flatten())
        .is_some_and(|list| list.matches())
}

/// A `change` listener on a media query; removed on drop.
pub struct MediaWatch {
    list: Option<web_sys::MediaQueryList>,
    listener: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for MediaWatch {
    fn drop(&mut self) {
        if let Some(list) = &self.list {
            let _ = list.remove_event_listener_with_callback(
                "change",
                self.listener.as_ref().unchecked_ref(),
            );
        }
    }
}

/// Call `on_change` whenever the media `query` starts or stops matching.
pub fn on_media_change(query: &str, mut on_change: impl FnMut() + 'static) -> MediaWatch {
    let listener = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| on_change());
    let list = web_sys::window().and_then(|w| w.match_media(query).ok().flatten());
    match &list {
        Some(list) => {
            let _ =
                list.add_event_listener_with_callback("change", listener.as_ref().unchecked_ref());
        }
        None => warn(&format!(
            "pharos: theme follow-system unavailable query={query}"
        )),
    }
    MediaWatch { list, listener }
}

/// How long a download's object URL lives after the click, so the browser
/// has started reading it.
const REVOKE_AFTER_MS: u32 = 1_000;

/// Save `text` as `filename` through a temporary object URL and `<a download>`.
pub fn download(filename: &str, mime: &str, text: &str) -> Result<(), crate::theme::ThemeError> {
    let storage_error = |message: String| crate::theme::ThemeError::Storage { message };
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| storage_error("no document".into()))?;
    let parts = js_sys::Array::of1(&JsValue::from_str(text));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime);
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options)
        .map_err(|e| storage_error(format!("{e:?}")))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|e| storage_error(format!("{e:?}")))?;
    let anchor = document
        .create_element("a")
        .map_err(|e| storage_error(format!("{e:?}")))?
        .dyn_into::<web_sys::HtmlAnchorElement>()
        .map_err(|_| storage_error("not an anchor element".into()))?;
    anchor.set_href(&url);
    anchor.set_download(filename);
    let body = document.body();
    if let Some(body) = &body {
        body.append_child(&anchor)
            .map_err(|e| storage_error(format!("{e:?}")))?;
    }
    anchor.click();
    if let Some(body) = &body {
        let _ = body.remove_child(&anchor);
    }
    spawn_detached(async move {
        sleep_ms(REVOKE_AFTER_MS).await;
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    Ok(())
}

/// Ask the user for a file matching `accept`, read as text. `Ok(None)` when
/// the picker closes with nothing chosen; a file over `max_bytes` is refused
/// before it is read.
pub async fn pick_file(
    accept: &str,
    max_bytes: usize,
) -> Result<Option<String>, crate::theme::ThemeError> {
    let storage_error = |message: String| crate::theme::ThemeError::Storage { message };
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| storage_error("no document".into()))?;
    let input = document
        .create_element("input")
        .map_err(|e| storage_error(format!("{e:?}")))?
        .dyn_into::<web_sys::HtmlInputElement>()
        .map_err(|_| storage_error("not an input element".into()))?;
    input.set_type("file");
    input.set_accept(accept);
    let body = document.body();
    if let Some(body) = &body {
        body.append_child(&input)
            .map_err(|e| storage_error(format!("{e:?}")))?;
    }
    let settled = js_sys::Promise::new(&mut |resolve, _| {
        // A pick fires `change`, a dismissal fires `cancel`; both end the wait.
        let _ = input.add_event_listener_with_callback("change", &resolve);
        let _ = input.add_event_listener_with_callback("cancel", &resolve);
    });
    input.click();
    let _ = wasm_bindgen_futures::JsFuture::from(settled).await;
    if let Some(body) = &body {
        let _ = body.remove_child(&input);
    }
    let Some(file) = input.files().and_then(|files| files.get(0)) else {
        return Ok(None);
    };
    let size = file.size();
    if size > max_bytes as f64 {
        return Err(crate::theme::ThemeError::TooLarge {
            bytes: size as usize,
        });
    }
    let text = wasm_bindgen_futures::JsFuture::from(file.text())
        .await
        .map_err(|e| storage_error(format!("could not read the file: {e:?}")))?;
    text.as_string()
        .map(Some)
        .ok_or_else(|| storage_error("the file is not text".into()))
}

/// Ask the user for a picture and return it ready to send as an avatar: at
/// most `max_bytes`, scaled down in the browser first when it is bigger, so a
/// phone photo fits one WebSocket message. `Ok(None)` when nothing was
/// chosen. The server checks and re-encodes whatever arrives.
pub async fn pick_picture(max_bytes: usize) -> Result<Option<Vec<u8>>, String> {
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or("no document")?;
    let input = document
        .create_element("input")
        .map_err(|e| format!("{e:?}"))?
        .dyn_into::<web_sys::HtmlInputElement>()
        .map_err(|_| "not an input element")?;
    input.set_type("file");
    input.set_accept("image/png,image/jpeg,image/webp,image/gif");
    let body = document.body();
    if let Some(body) = &body {
        body.append_child(&input).map_err(|e| format!("{e:?}"))?;
    }
    let settled = js_sys::Promise::new(&mut |resolve, _| {
        let _ = input.add_event_listener_with_callback("change", &resolve);
        let _ = input.add_event_listener_with_callback("cancel", &resolve);
    });
    input.click();
    let _ = wasm_bindgen_futures::JsFuture::from(settled).await;
    if let Some(body) = &body {
        let _ = body.remove_child(&input);
    }
    let Some(file) = input.files().and_then(|files| files.get(0)) else {
        return Ok(None);
    };
    if file.size() <= max_bytes as f64 {
        return blob_bytes(&file).await.map(Some);
    }
    shrink_picture(&file, max_bytes).await.map(Some)
}

/// Longest the shorter side of a picture is sent at; the server keeps 512.
const PICTURE_SHORT_SIDE: f64 = 1024.0;
/// Longest the longer side is sent at, for a very wide or tall picture.
const PICTURE_LONG_SIDE: f64 = 2048.0;

/// Draws `file` onto a canvas no bigger than the sides above and encodes it,
/// WebP where the browser can, else PNG, both keeping transparency; JPEG
/// only when neither fits.
async fn shrink_picture(file: &web_sys::File, max_bytes: usize) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let decoded = window
        .create_image_bitmap_with_blob(file)
        .map_err(|e| format!("{e:?}"))?;
    let bitmap: web_sys::ImageBitmap = wasm_bindgen_futures::JsFuture::from(decoded)
        .await
        .map_err(|_| "the browser could not read that picture".to_owned())?
        .dyn_into()
        .map_err(|_| "not an image bitmap")?;
    let (w, h) = (f64::from(bitmap.width()), f64::from(bitmap.height()));
    let scale = (PICTURE_SHORT_SIDE / w.min(h))
        .min(PICTURE_LONG_SIDE / w.max(h))
        .min(1.0);
    let (cw, ch) = ((w * scale).round().max(1.0), (h * scale).round().max(1.0));
    let canvas = window
        .document()
        .ok_or("no document")?
        .create_element("canvas")
        .map_err(|e| format!("{e:?}"))?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(|_| "not a canvas")?;
    canvas.set_width(cw as u32);
    canvas.set_height(ch as u32);
    let ctx = canvas
        .get_context("2d")
        .map_err(|e| format!("{e:?}"))?
        .ok_or("no 2d context")?
        .dyn_into::<web_sys::CanvasRenderingContext2d>()
        .map_err(|_| "not a 2d context")?;
    ctx.draw_image_with_image_bitmap_and_dw_and_dh(&bitmap, 0.0, 0.0, cw, ch)
        .map_err(|e| format!("{e:?}"))?;
    bitmap.close();
    for mime in ["image/webp", "image/png", "image/jpeg"] {
        let Some(blob) = canvas_blob(&canvas, mime).await else {
            continue;
        };
        // A browser that cannot write WebP gives PNG instead: try the next.
        if blob.type_() != mime {
            continue;
        }
        let bytes = blob_bytes(&blob).await?;
        if bytes.len() <= max_bytes {
            return Ok(bytes);
        }
    }
    Err("that picture is too large even scaled down".to_owned())
}

async fn canvas_blob(canvas: &web_sys::HtmlCanvasElement, mime: &str) -> Option<web_sys::Blob> {
    const QUALITY: f64 = 0.9;
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let done = Closure::once_into_js(move |blob: JsValue| {
            let _ = resolve.call1(&JsValue::NULL, &blob);
        });
        let _ = canvas.to_blob_with_type_and_encoder_options(
            done.unchecked_ref(),
            mime,
            &JsValue::from_f64(QUALITY),
        );
    });
    wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .ok()?
        .dyn_into()
        .ok()
}

async fn blob_bytes(blob: &web_sys::Blob) -> Result<Vec<u8>, String> {
    let buffer = wasm_bindgen_futures::JsFuture::from(blob.array_buffer())
        .await
        .map_err(|_| "could not read the picture".to_owned())?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// Resolves on the next animation frame with its timestamp. Browsers stop
/// animation frames in a hidden tab, so a loop over this idles there.
pub async fn animation_frame() -> f64 {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.request_animation_frame(&resolve);
        }
    });
    wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .ok()
        .and_then(|t| t.as_f64())
        .unwrap_or(0.0)
}

/// Whether the page has had a user gesture, so an `AudioContext` starts
/// running rather than suspended. True where the browser cannot say.
pub fn user_activated() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    js_sys::Reflect::get(&window.navigator(), &"userActivation".into())
        .ok()
        .filter(|a| !a.is_undefined())
        .and_then(|a| js_sys::Reflect::get(&a, &"hasBeenActive".into()).ok())
        .map_or(true, |active| active.is_truthy())
}

/// The page's `--px-color-bg`, `-accent` and `-fg`, as the theme set them.
pub fn theme_palette() -> crate::render::Palette {
    let style = web_sys::window().and_then(|w| {
        let root = w.document()?.document_element()?;
        w.get_computed_style(&root).ok().flatten()
    });
    let Some(style) = style else {
        return crate::render::Palette::default();
    };
    let token = |name: &str| style.get_property_value(name).unwrap_or_default();
    crate::render::Palette::from_tokens(
        &token("--px-color-bg"),
        &token("--px-color-accent"),
        &token("--px-color-fg"),
    )
}

/// Analyser frame size: 1024 frequency bins, ~43 ms of samples at 48 kHz.
const FFT_SIZE: u32 = 2048;

/// Web Audio's view of a media element: the element plays through an
/// `AnalyserNode` into the speakers.
struct WebAudioTap {
    context: web_sys::AudioContext,
    analyser: web_sys::AnalyserNode,
    media: web_sys::HtmlMediaElement,
}

impl super::visualiser::AudioTap for WebAudioTap {
    fn read(&mut self, bins: &mut [f32], wave: &mut [f32]) {
        self.analyser.get_float_frequency_data(bins);
        self.analyser.get_float_time_domain_data(wave);
    }

    fn bins(&self) -> usize {
        self.analyser.frequency_bin_count() as usize
    }

    fn sample_rate(&self) -> f32 {
        self.context.sample_rate()
    }

    fn volume(&self) -> f32 {
        if self.media.muted() {
            0.0
        } else {
            self.media.volume() as f32
        }
    }
}

impl Drop for WebAudioTap {
    fn drop(&mut self) {
        TAPS.with(|taps| taps.borrow_mut().retain(|t| t.context != self.context));
        let _ = self.context.close();
    }
}

/// A live tap's nodes, so a resume (B563) can read them without the
/// visualiser that owns the tap.
struct TapNodes {
    media: web_sys::HtmlMediaElement,
    context: web_sys::AudioContext,
    analyser: web_sys::AnalyserNode,
}

thread_local! {
    static TAPS: RefCell<Vec<TapNodes>> = const { RefCell::new(Vec::new()) };
}

fn with_tap<T>(media: &web_sys::HtmlMediaElement, f: impl FnOnce(&TapNodes) -> T) -> Option<T> {
    TAPS.with(|taps| taps.borrow().iter().find(|t| t.media == *media).map(f))
}

/// A number property of `target` in milliseconds, from seconds; `None` where
/// the browser has not got it (Firefox has no `outputLatency` before 132).
fn seconds_prop_ms(target: &JsValue, name: &str) -> Option<f64> {
    js_sys::Reflect::get(target, &JsValue::from_str(name))
        .ok()
        .and_then(|v| v.as_f64())
        .map(|s| s * 1000.0)
}

/// The media element `id` and its audio path as a resume finds them.
pub fn resume_context(id: &str) -> super::resume::ResumeContext {
    use super::resume::{AudioPath, ResumeContext};
    let Some(media) = video_element(id) else {
        return ResumeContext {
            ready_state: 0,
            buffered_ahead_ms: 0,
            engine: "none",
            muted: false,
            audio: None,
        };
    };
    let now = media.current_time();
    let buffered = media.buffered();
    let ahead = (0..buffered.length())
        .filter_map(|i| Some((buffered.start(i).ok()?, buffered.end(i).ok()?)))
        .find(|(start, end)| *start <= now && now <= *end)
        .map_or(0.0, |(_, end)| end - now);
    let src = media.current_src();
    let engine = if src.starts_with("blob:") {
        "hlsjs"
    } else if src.split('?').next().is_some_and(|p| p.ends_with(".m3u8")) {
        "native_hls"
    } else {
        "direct"
    };
    let audio = with_tap(&media, |t| AudioPath {
        context_state: js_sys::Reflect::get(&t.context, &JsValue::from_str("state"))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "unknown".to_owned()),
        base_latency_ms: seconds_prop_ms(&t.context, "baseLatency"),
        output_latency_ms: seconds_prop_ms(&t.context, "outputLatency"),
    });
    ResumeContext {
        ready_state: media.ready_state(),
        buffered_ahead_ms: seconds_to_ms(ahead),
        engine,
        muted: media.muted() || media.volume() <= 0.0,
        audio,
    }
}

/// The largest sample in the tap's latest window on the media element `id`,
/// divided by its volume, or `None` with no tap or no volume.
pub fn tap_peak(id: &str) -> Option<f32> {
    let media = video_element(id)?;
    let volume = media.volume() as f32;
    if media.muted() || volume <= 0.0 {
        return None;
    }
    with_tap(&media, |t| {
        let mut wave = vec![0.0_f32; t.analyser.fft_size() as usize];
        t.analyser.get_float_time_domain_data(&mut wave);
        wave.iter().fold(0.0_f32, |peak, s| peak.max(s.abs())) / volume
    })
}

/// Route the media element `media_id` through an analyser. A media element
/// can be routed once, so the caller keeps the tap for the element's life.
///
/// A cross-origin source without CORS would play silence once routed, so
/// only same-origin and MSE (`blob:`) sources are tapped.
pub fn audio_tap(media_id: &str) -> Result<Box<dyn super::visualiser::AudioTap>, String> {
    let media = video_element(media_id).ok_or_else(|| format!("no media element {media_id}"))?;
    let src = media.current_src();
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default();
    let same_origin =
        src.starts_with("blob:") || web_sys::Url::new(&src).is_ok_and(|u| u.origin() == origin);
    if !same_origin {
        return Err(format!("cross-origin source {src}"));
    }
    // WebKit's native HLS feeds a MediaElementAudioSourceNode silence, so a
    // routed element would go quiet; leave it unrouted and draw idle.
    if src
        .split('?')
        .next()
        .is_some_and(|path| path.ends_with(".m3u8"))
    {
        return Err(format!("native HLS source {src}"));
    }
    let js = |e: JsValue| format!("{e:?}");
    let context = web_sys::AudioContext::new().map_err(js)?;
    let source = context.create_media_element_source(&media).map_err(js)?;
    let analyser = context.create_analyser().map_err(js)?;
    analyser.set_fft_size(FFT_SIZE);
    analyser.set_smoothing_time_constant(0.0);
    source.connect_with_audio_node(&analyser).map_err(js)?;
    analyser
        .connect_with_audio_node(&context.destination())
        .map_err(js)?;
    let _ = context.resume();
    TAPS.with(|taps| {
        taps.borrow_mut().push(TapNodes {
            media: media.clone(),
            context: context.clone(),
            analyser: analyser.clone(),
        })
    });
    Ok(Box::new(WebAudioTap {
        context,
        analyser,
        media,
    }))
}

/// Whether the media element `id` has a source and is playing.
pub fn media_playing(id: &str) -> bool {
    video_element(id).is_some_and(|m| !m.paused() && !m.current_src().is_empty())
}

const GROUP_DEVICE_KEY: &str = "pharos.group.device";

/// This tab's device id for group calls: the server keys a member by user and
/// device, so two tabs of one user need two ids. It lives in `sessionStorage`,
/// so a reload of the tab is the same member (V25) and a new tab is another.
fn group_device() -> String {
    let storage = web_sys::window().and_then(|w| w.session_storage().ok().flatten());
    if let Some(id) = storage
        .as_ref()
        .and_then(|s| s.get_item(GROUP_DEVICE_KEY).ok().flatten())
    {
        return id;
    }
    let id = format!(
        "web-{:x}{:x}",
        (js_sys::Math::random() * 4_294_967_296.0) as u32,
        js_sys::Date::now() as u64
    );
    if let Some(storage) = storage {
        let _ = storage.set_item(GROUP_DEVICE_KEY, &id);
    }
    id
}

/// `Group` and `Membership` over the session: the [`GroupClient`] the Together
/// screens and the group store use. One membership at a time; joining or
/// creating replaces it.
#[derive(Clone)]
pub struct Groups {
    group: NativeGroup,
    member: Rc<RefCell<Option<NativeMembership>>>,
    /// Counts the creates and joins, so an update from an earlier one is not
    /// taken for the current membership's.
    generation: Rc<Cell<u64>>,
    call: CallInfo,
}

pub fn groups(api: &NativeApi) -> Groups {
    Groups {
        group: api.group().clone(),
        member: Rc::default(),
        generation: Rc::default(),
        call: CallInfo {
            ui_route: "together".into(),
            ui_action: "group".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            device: group_device(),
            ..CallInfo::default()
        },
    }
}

/// What a create or join learns from its sink before the call answers.
struct Answer {
    /// The first `joined`.
    first: Rc<RefCell<Option<GroupSummary>>>,
    /// An `ended` came first.
    ended: Rc<Cell<bool>>,
}

/// How long a create or join waits for the group's `joined` update.
const JOINED_WAIT_MS: u32 = 5_000;
const JOINED_POLL_MS: u32 = 20;

fn group_error(e: ClientError) -> GroupError {
    match e {
        ClientError::Api {
            kind: "notFound", ..
        } => GroupError("This group has ended".into()),
        ClientError::Api {
            kind: "notPermitted",
            ..
        } => GroupError("You can't watch together here".into()),
        other => GroupError(other.to_string()),
    }
}

fn ms(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn unms(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn member(m: pharos_client::group::GroupMember) -> Member {
    Member {
        id: m.id,
        name: m.name,
        buffering: m.buffering,
        own_tracks: m.own_tracks,
    }
}

fn tracks(t: &pharos_client::group::GroupTracks) -> GroupTracksView {
    GroupTracksView {
        audio: t.audio,
        subtitle: t.subtitle,
    }
}

/// The member id a command named; the wire's empty text means none.
fn by(id: Option<String>) -> Option<String> {
    id.filter(|id| !id.is_empty())
}

fn summary(g: pharos_client::group::GroupSummary) -> GroupSummary {
    GroupSummary {
        id: g.id,
        name: g.name,
        item: g.item,
        tracks: tracks(&g.tracks),
        members: g.members.into_iter().map(member).collect(),
        you: g.you,
    }
}

/// The store's update for the client's.
fn update(u: GroupUpdate) -> together::GroupUpdate {
    use together::GroupUpdate as U;
    match u {
        GroupUpdate::Joined(g) => U::Joined(summary(g)),
        GroupUpdate::Play {
            at_server_ms,
            position_ms,
            by: who,
        } => U::Play {
            at_server_ms: ms(at_server_ms),
            position_ms,
            by: by(who),
        },
        GroupUpdate::Pause {
            at_server_ms,
            position_ms,
            by: who,
        } => U::Pause {
            at_server_ms: ms(at_server_ms),
            position_ms,
            by: by(who),
        },
        GroupUpdate::Seek {
            at_server_ms,
            position_ms,
            by: who,
        } => U::Seek {
            at_server_ms: ms(at_server_ms),
            position_ms,
            by: by(who),
        },
        GroupUpdate::Member(m) => U::Member(member(m)),
        GroupUpdate::Left(id) => U::Left(id),
        GroupUpdate::Item {
            item,
            playlist_item_id,
            position_ms,
            playing,
        } => U::Item {
            item,
            playlist_item_id,
            position_ms,
            playing,
        },
        GroupUpdate::Tracks(t) => U::Tracks {
            tracks: tracks(&t),
            by: by(Some(t.by)),
        },
        GroupUpdate::Ended => U::Ended,
    }
}

impl Groups {
    fn membership(&self) -> Result<NativeMembership, GroupError> {
        self.member
            .borrow()
            .clone()
            .ok_or_else(|| GroupError("Not in a group".into()))
    }

    /// Leave the membership still held, if any: the server keys a member by
    /// user and device, so a new one made first would be taken for it.
    async fn leave_held(&self) {
        let held = self.member.borrow_mut().take();
        if let Some(m) = held {
            if let Err(e) = m.leave(&self.call).await {
                warn(&format!("pharos: leaving the earlier group failed: {e}"));
            }
        }
    }

    /// Wraps `sink` so the first `joined` is also kept for the caller, which
    /// needs the group's id: `create` and `join` answer with the membership
    /// only. An `ended` drops the membership held for this call, and is
    /// noted in the returned flag for a call it beats to the answer.
    fn tap(&self, sink: GroupSink) -> (impl Fn(GroupUpdate) + 'static, Answer) {
        let first = Rc::new(RefCell::new(None::<GroupSummary>));
        let keep = Rc::clone(&first);
        let ended = Rc::new(Cell::new(false));
        let ended_flag = Rc::clone(&ended);
        let member = Rc::clone(&self.member);
        let generation = Rc::clone(&self.generation);
        generation.set(generation.get() + 1);
        let mine = generation.get();
        (
            move |u: GroupUpdate| {
                let u = update(u);
                if let together::GroupUpdate::Joined(g) = &u {
                    let mut slot = keep.borrow_mut();
                    if slot.is_none() {
                        *slot = Some(g.clone());
                    }
                }
                if u == together::GroupUpdate::Ended {
                    ended_flag.set(true);
                    if generation.get() == mine {
                        member.borrow_mut().take();
                    }
                }
                sink(u);
            },
            Answer { first, ended },
        )
    }

    async fn settled(
        &self,
        made: Result<NativeMembership, ClientError>,
        answer: Answer,
    ) -> Result<GroupSummary, GroupError> {
        let membership = made.map_err(group_error)?;
        if answer.ended.get() {
            return Err(GroupError("This group has ended".into()));
        }
        *self.member.borrow_mut() = Some(membership);
        let mut waited = 0;
        loop {
            if let Some(g) = answer.first.borrow().clone() {
                return Ok(g);
            }
            if waited >= JOINED_WAIT_MS {
                self.leave_held().await;
                return Err(GroupError("The group did not answer".into()));
            }
            sleep_ms(JOINED_POLL_MS).await;
            waited += JOINED_POLL_MS;
        }
    }
}

impl GroupClient for Groups {
    async fn list(&self) -> Result<Vec<GroupSummary>, GroupError> {
        let groups = self.group.list(&self.call).await.map_err(group_error)?;
        Ok(groups.into_iter().map(summary).collect())
    }

    async fn create(
        &self,
        item: ItemRef,
        position_ms: u64,
        sink: GroupSink,
    ) -> Result<GroupSummary, GroupError> {
        self.leave_held().await;
        let (on_update, answer) = self.tap(sink);
        let made = self
            .group
            .create(&self.call, item, position_ms, on_update)
            .await;
        self.settled(made, answer).await
    }

    async fn join(&self, id: &str, sink: GroupSink) -> Result<GroupSummary, GroupError> {
        self.leave_held().await;
        let (on_update, answer) = self.tap(sink);
        let made = self.group.join(&self.call, id, on_update).await;
        self.settled(made, answer).await
    }

    async fn clock(&self, client_ms: i64) -> Result<ClockReply, GroupError> {
        let s = self
            .group
            .clock(&self.call, unms(client_ms))
            .await
            .map_err(group_error)?;
        Ok(ClockReply {
            client_ms_echo: ms(s.client_ms_echo),
            recv_ms: ms(s.recv_ms),
            send_ms: ms(s.send_ms),
        })
    }

    async fn ping(&self) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.ping(&self.call).await.map_err(group_error)
    }

    async fn clock_report(&self, t1: i64, t2: i64, t3: i64, t4: i64) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.clock_report(&self.call, unms(t1), unms(t2), unms(t3), unms(t4))
            .await
            .map_err(group_error)
    }

    async fn unpause(&self) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.unpause(&self.call).await.map_err(group_error)
    }

    async fn pause(&self) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.pause(&self.call).await.map_err(group_error)
    }

    async fn seek(&self, position_ms: u64) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.seek(&self.call, position_ms).await.map_err(group_error)
    }

    async fn buffering(
        &self,
        playlist_item_id: String,
        position_ms: u64,
        buffering: bool,
    ) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.buffering(&self.call, position_ms, buffering, &playlist_item_id)
            .await
            .map_err(group_error)
    }

    async fn ready(
        &self,
        playlist_item_id: String,
        position_ms: u64,
        playing: bool,
    ) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.ready(&self.call, position_ms, playing, &playlist_item_id)
            .await
            .map_err(group_error)
    }

    async fn seen(&self, ids: Vec<ItemRef>) -> Result<Vec<MemberSeen>, GroupError> {
        let m = self.membership()?;
        m.seen(&self.call, &ids).await.map_err(group_error)
    }

    async fn set_tracks(
        &self,
        audio: TrackChoice,
        subtitle: TrackChoice,
        playlist_item_id: String,
    ) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.set_tracks(&self.call, audio, subtitle, &playlist_item_id)
            .await
            .map_err(group_error)
    }

    async fn follow_tracks(&self, follow: bool) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.follow_tracks(&self.call, follow)
            .await
            .map_err(group_error)
    }

    async fn leave(&self) -> Result<(), GroupError> {
        let Some(m) = self.member.borrow_mut().take() else {
            return Ok(());
        };
        m.leave(&self.call).await.map_err(group_error)
    }
}
