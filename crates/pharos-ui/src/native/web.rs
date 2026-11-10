//! Browser glue for the native app: the socket, `/pharos/session`, history
//! and timers. Every result comes back as data for
//! [`Machine`](super::machine::Machine); no state lives here.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use super::diagnostics::{self, Sink};
use super::rails;
use super::watch_view::Skip;
use gloo_net::http::Request;
use pharos_client::api::{Capabilities, ClientEvent, ClientEventKind, ClientLevel, NativeApi};
use pharos_client::browser::{self, Connection};
use pharos_client::CallInfo;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};

use super::machine::{Probe, SignInReply, SignOutReply};
use super::playback::{
    cue_chars_per_line, cue_font_px, cue_width_share, is_loading, lifted_line, Engine, FailureKind,
    Fatal, FragmentLoad, LibraryTransition, MediaState, OnFatal, OnFragment, OverlayEffect,
    PlaybackEngine, Recovery, RecoveryBudget, SubtitleSize,
};

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

pub fn set_title(title: &str) {
    if let Some(document) = web_sys::window().and_then(|w| w.document()) {
        document.set_title(title);
    }
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
/// The browser's Back is history the page does not hold: a guard is never
/// asked, so Back leaves as it always has.
pub struct BackWatch;

pub fn on_back(_on_back: impl FnMut() -> bool + 'static) -> BackWatch {
    BackWatch
}

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

/// A reload loads the newer server's page.
pub fn can_reload() -> bool {
    true
}

/// The page draws its own mute button and volume slider.
pub fn owns_volume() -> bool {
    true
}

/// Quick Connect sign-in is the TV's: a browser has a keyboard.
pub fn offers_quick_connect() -> bool {
    false
}

pub async fn quick_connect(
    _show_code: impl FnMut(String) + 'static,
) -> super::machine::SignInReply {
    super::machine::SignInReply::Failed("Quick Connect sign-in is the TV app's".into())
}

/// Artwork as the page already has it: same-origin with the server.
pub fn art_src(src: &str) -> String {
    src.to_owned()
}

pub struct BackAtRoot;

/// `timeupdate` reaches the player as a Dioxus event here.
pub struct TimeUpdates;

pub fn on_time_update(_on_tick: impl FnMut(&str) + 'static) -> TimeUpdates {
    TimeUpdates
}

/// The remote's Back at the first page (the Android TV app only): a browser
/// leaves the page itself.
pub fn on_back_at_root(_on_back: impl FnMut() -> bool + 'static) -> BackAtRoot {
    BackAtRoot
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
    let mut caps =
        super::playback::capabilities(|mime| (has_mse() && mse(mime)) || can_play(mime), can_play);
    caps.hdr = prefers("(dynamic-range: high)");
    caps
}

pub(super) fn video_element(id: &str) -> Option<web_sys::HtmlMediaElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(id)?
        .dyn_into::<web_sys::HtmlMediaElement>()
        .ok()
}

/// The `<video>`'s dropped and total frame counts since it loaded, from
/// `getVideoPlaybackQuality`; `None` when the element is gone or the browser
/// has no such counters.
pub fn video_quality(video_id: &str) -> Option<(u64, u64)> {
    let video = video_element(video_id)?
        .dyn_into::<web_sys::HtmlVideoElement>()
        .ok()?;
    let quality = video.get_video_playback_quality();
    Some((
        u64::from(quality.dropped_video_frames()),
        u64::from(quality.total_video_frames()),
    ))
}

/// Whether the page is on screen: frames a hidden tab skips say nothing
/// about the renderer.
pub fn page_visible() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .is_some_and(|d| !d.hidden())
}

/// The `<video>`'s current position, 0 when it is gone.
pub fn position_ms(video_id: &str) -> u64 {
    video_element(video_id).map_or(0, |v| seconds_to_ms(v.current_time()))
}

/// Whether the `<video>` played to its end; false when it is gone.
pub fn ended(video_id: &str) -> bool {
    video_element(video_id).is_some_and(|v| v.ended())
}

/// Whether the `<video>` can play at its position now, not mid-seek.
pub fn has_data(video_id: &str) -> bool {
    video_element(video_id).is_some_and(|v| {
        !v.seeking() && v.ready_state() >= web_sys::HtmlMediaElement::HAVE_FUTURE_DATA
    })
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

/// The shape of the picture `video` shows: its decoded frame, one eye of it
/// while its frame carries a 3D crop (`data-packing`, V921), or the picture
/// inside baked-in black bars while it carries a bar crop (`data-crop`,
/// B1017).
pub(super) fn picture_size(video: &web_sys::HtmlMediaElement) -> (u32, u32) {
    picture_of(video).1
}

/// Where the picture `video` shows sits in its decoded frame (fractions),
/// and its shape: the one reader of the frame's `data-packing` and
/// `data-crop`, for [`picture_size`] and the transition snapshot.
pub(super) fn picture_of(
    video: &web_sys::HtmlMediaElement,
) -> (super::stereo::FrameRect, (u32, u32)) {
    let frame = video.parent_element();
    let packing = frame
        .as_ref()
        .and_then(|f| f.get_attribute("data-packing"))
        .and_then(|a| super::stereo::parse_packing(&a));
    let bars = frame
        .as_ref()
        .and_then(|f| f.get_attribute("data-crop"))
        .and_then(|a| super::bars::parse_crop(&a));
    let (w, h) = super::stereo::eye_size(
        packing,
        video_dimension(video, "videoWidth"),
        video_dimension(video, "videoHeight"),
    );
    let rect = bars.unwrap_or_else(|| super::stereo::eye_rect(packing, super::stereo::Eye::Left));
    (rect, super::bars::shown_size(bars, w, h))
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
        wake_tap(&v);
        start(&v, "toggle");
    } else if let Err(e) = v.pause() {
        warn(&format!("pharos: pause() refused: {e:?}"));
    }
}

/// Start the `<video>`. A refused `play()` (autoplay policy) leaves it
/// paused, and is told apart from other failures (038 FR-407).
pub fn play(video_id: &str) {
    if let Some(v) = video_element(video_id) {
        wake_tap(&v);
        start(&v, "play");
    }
}

thread_local! {
    /// The browser refused a `play()` and no gesture has come since: a
    /// retry would be refused too (038 FR-407, V1223).
    static AUTOPLAY_BLOCKED: Cell<bool> = const { Cell::new(false) };
    /// The document listens for the gesture that lifts it.
    static GESTURE_WATCHED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the browser refused to start a video until the viewer does
/// something: the player then shows "Tap to start", and a group stops
/// retrying the start in the background.
pub fn autoplay_blocked() -> bool {
    AUTOPLAY_BLOCKED.with(Cell::get)
}

/// `play()` with its promise read: a `NotAllowedError` is the autoplay
/// policy, logged and reported once per refusal; anything else (an aborted
/// load, a source error) is logged as itself.
fn start(v: &web_sys::HtmlMediaElement, source: &'static str) {
    let promise = match v.play() {
        Ok(p) => p,
        Err(e) => {
            warn(&format!("pharos: play() refused source={source}: {e:?}"));
            return;
        }
    };
    wasm_bindgen_futures::spawn_local(async move {
        match wasm_bindgen_futures::JsFuture::from(promise).await {
            Ok(_) => AUTOPLAY_BLOCKED.with(|b| b.set(false)),
            Err(e) => play_failed(&e, source),
        }
    });
}

fn play_failed(e: &JsValue, source: &'static str) {
    let name = js_sys::Reflect::get(e, &JsValue::from_str("name"))
        .ok()
        .and_then(|n| n.as_string())
        .unwrap_or_default();
    if name != "NotAllowedError" {
        // An AbortError is a newer load or pause overtaking this play.
        info(&format!(
            "pharos: play() did not start source={source} name={name} reason={}",
            js_message(e)
        ));
        return;
    }
    let first = !AUTOPLAY_BLOCKED.with(|b| b.replace(true));
    watch_for_gesture();
    if first {
        warn(&format!(
            "pharos: autoplay refused source={source} reason={}",
            js_message(e)
        ));
        report_event(ClientEvent {
            kind: ClientEventKind::AutoplayRefused,
            level: ClientLevel::Warn,
            route: current_path(),
            fields: vec![("source", source.to_owned())],
        });
    }
}

/// Lifts [`autoplay_blocked`] on the viewer's next tap, click or key, which
/// gives the page the activation a `play()` needs.
fn watch_for_gesture() {
    if GESTURE_WATCHED.with(|w| w.replace(true)) {
        return;
    }
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let lift = Closure::<dyn Fn()>::new(|| {
        if AUTOPLAY_BLOCKED.with(|b| b.replace(false)) {
            info("pharos: autoplay gesture: a tap or key lifted the refusal");
        }
    });
    for kind in ["pointerdown", "keydown"] {
        let _ = doc.add_event_listener_with_callback_and_bool(
            kind,
            lift.as_ref().unchecked_ref(),
            true,
        );
    }
    // Lives as long as the page.
    lift.forget();
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

/// The viewer's speed (T038-305): `playbackRate`, and the default rate a
/// new source starts at, so a quality or audio switch keeps it.
pub fn set_speed(video_id: &str, rate: f64) {
    if let Some(v) = video_element(video_id) {
        v.set_default_playback_rate(rate);
        v.set_playback_rate(rate);
    }
}

/// The media `id`'s `playbackRate`, 1.0 when it is gone.
pub fn speed(video_id: &str) -> f64 {
    video_element(video_id).map_or(1.0, |v| v.playback_rate())
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

thread_local! {
    /// The `<video>` whose track is shown, measured on each lift.
    static CUE_VIDEO: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// One `cuechange` handler for every shown track: it lifts the cues
    /// that load after the track is shown, at the player's size then.
    static ON_CUE_CHANGE: Closure<dyn FnMut(web_sys::Event)> =
        Closure::new(|e: web_sys::Event| {
            if let Some(track) = e.target() {
                let cue_box = CUE_VIDEO.with(|v| cue_box_for(&v.borrow()));
                lift_cues(
                    &js_sys::Reflect::get(&track, &"activeCues".into()).unwrap_or_default(),
                    cue_box,
                );
            }
        });
    /// The non-native player's `change` handler on its `TextTrackList`.
    static ON_TRACKS_CHANGE: std::cell::RefCell<Option<Closure<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
    /// The `ResizeObserver` on the `<video>` in `CUE_VIDEO` and its callback,
    /// which lift the shown cues again for the player's new size.
    static CUE_RESIZE: std::cell::RefCell<Option<CueResize>> =
        const { std::cell::RefCell::new(None) };
}

/// The `<video>` a `ResizeObserver` watches, the observer and the callback
/// it calls.
type CueResize = (JsValue, JsValue, Closure<dyn FnMut()>);

/// Lifts the cues of each track the `<video>` `video_id` shows, for its
/// size now: every cue (`"cues"`), or only those on screen (`"activeCues"`)
/// when the rest will be lifted by `ON_CUE_CHANGE` as they come up.
fn lift_shown_cues(video_id: &str, list: &str) {
    let Some(tracks) = video_element(video_id).and_then(|v| v.text_tracks()) else {
        return;
    };
    let cue_box = cue_box_for(video_id);
    for i in 0..tracks.length() {
        if let Some(t) = tracks.get(i) {
            if t.mode() == web_sys::TextTrackMode::Showing {
                lift_cues(
                    &js_sys::Reflect::get(&t, &list.into()).unwrap_or_default(),
                    cue_box,
                );
            }
        }
    }
}

/// Makes `video_id` the `<video>` whose cues are lifted, and lifts them
/// again whenever it changes size (a window resize, full screen, the mini
/// player), since a cue lifted for one width can wrap at another.
/// `ResizeObserver` is reached through `js_sys::Reflect`, as web-sys keeps it
/// behind a feature this crate does not enable.
fn watch_cue_video(video_id: &str) {
    CUE_VIDEO.with(|v| video_id.clone_into(&mut v.borrow_mut()));
    let Some(video) = video_element(video_id) else {
        return;
    };
    let video: JsValue = video.into();
    // The same id can name a new `<video>` (the player left and opened
    // again), so the observer is kept only while it watches this element.
    let same = CUE_RESIZE.with(|r| {
        r.borrow()
            .as_ref()
            .is_some_and(|(seen, _, _)| *seen == video)
    });
    if same {
        return;
    }
    CUE_RESIZE.with(|r| {
        if let Some((_, observer, _)) = r.borrow_mut().take() {
            if let Ok(disconnect) = js_sys::Reflect::get(&observer, &"disconnect".into()) {
                if let Some(f) = disconnect.dyn_ref::<js_sys::Function>() {
                    let _ = f.call0(&observer);
                }
            }
        }
    });
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(ctor) = js_sys::Reflect::get(&window, &"ResizeObserver".into()) else {
        return;
    };
    let Some(ctor) = ctor.dyn_ref::<js_sys::Function>() else {
        return;
    };
    let id = video_id.to_owned();
    let on_resize = Closure::<dyn FnMut()>::new(move || lift_shown_cues(&id, "activeCues"));
    let Ok(observer) = js_sys::Reflect::construct(ctor, &js_sys::Array::of1(on_resize.as_ref()))
    else {
        return;
    };
    if let Ok(observe) = js_sys::Reflect::get(&observer, &"observe".into()) {
        if let Some(f) = observe.dyn_ref::<js_sys::Function>() {
            let _ = f.call1(&observer, &video);
        }
    }
    CUE_RESIZE.with(|r| *r.borrow_mut() = Some((video, observer, on_resize)));
}

/// Lifts the shown cues of the `<video>` `video_id` again for the font size
/// the Subtitle size choice just set, since a larger size can wrap a cue onto
/// more lines. The `<video>` whose cues were last lifted (the non-native player's
/// `pharos-media-…`) is lifted too when it is not `video_id`.
pub fn relift_cues(video_id: &str) {
    lift_shown_cues(video_id, "cues");
    let watched = CUE_VIDEO.with(|v| v.borrow().clone());
    if !watched.is_empty() && watched != video_id {
        lift_shown_cues(&watched, "cues");
    }
}

/// Lifts the cues of each track the `<video>` `video_id` shows (V455), now
/// and whenever the browser's own caption menu changes the shown track: the
/// non-native player (`views/player.rs`) has no `show_text_track`.
pub fn lift_cues_as_shown(video_id: &str) {
    let Some(tracks) = video_element(video_id).and_then(|v| v.text_tracks()) else {
        return;
    };
    watch_cue_video(video_id);
    let id = video_id.to_owned();
    let list = tracks.clone();
    let lift = move || {
        lift_shown_cues(&id, "cues");
        for i in 0..list.length() {
            if let Some(t) = list.get(i) {
                if t.mode() == web_sys::TextTrackMode::Showing {
                    ON_CUE_CHANGE.with(|f| t.set_oncuechange(Some(f.as_ref().unchecked_ref())));
                }
            }
        }
    };
    lift();
    let on_change = Closure::<dyn FnMut()>::new(lift);
    tracks.set_onchange(Some(on_change.as_ref().unchecked_ref()));
    // The previous player's handler goes once this one replaces it.
    ON_TRACKS_CHANGE.with(|c| *c.borrow_mut() = Some(on_change));
}

/// Shows the text track whose `<track id>` is `track_id`, hiding the rest;
/// `None` hides them all.
pub fn show_text_track(video_id: &str, track_id: Option<&str>) {
    let Some(tracks) = video_element(video_id).and_then(|v| v.text_tracks()) else {
        return;
    };
    watch_cue_video(video_id);
    for i in 0..tracks.length() {
        if let Some(t) = tracks.get(i) {
            let on = track_id.is_some_and(|id| t.id() == id);
            t.set_mode(if on {
                web_sys::TextTrackMode::Showing
            } else {
                web_sys::TextTrackMode::Disabled
            });
            if on {
                lift_cues(
                    &js_sys::Reflect::get(&t, &"cues".into()).unwrap_or_default(),
                    cue_box_for(video_id),
                );
                ON_CUE_CHANGE.with(|f| t.set_oncuechange(Some(f.as_ref().unchecked_ref())));
            }
        }
    }
    // A track only exposes its cues once it is showing or hidden, so the
    // delay is applied after the modes are set.
    shift_native_cues(video_id);
}

/// Loads the text track `track_id` without drawing it: the Rust subtitle
/// renderer draws its cues, and the track stays in the page so its words
/// survive a renderer failure (spec 026 US1). Every other track is off.
pub fn hide_text_track(video_id: &str, track_id: &str) {
    let Some(tracks) = video_element(video_id).and_then(|v| v.text_tracks()) else {
        return;
    };
    for i in 0..tracks.length() {
        if let Some(t) = tracks.get(i) {
            t.set_mode(if t.id() == track_id {
                web_sys::TextTrackMode::Hidden
            } else {
                web_sys::TextTrackMode::Disabled
            });
        }
    }
}

thread_local! {
    /// The viewer's subtitle delay in ms (T038-305), which every renderer
    /// reads: the Rust renderer per frame, JASSUB as its time offset, the
    /// native `<track>` fallback as shifted cue times.
    static SUBTITLE_DELAY_MS: Cell<i64> = const { Cell::new(0) };
    /// The JASSUB instance drawing now, to hand it a new delay.
    static LIVE_JASSUB: RefCell<Option<JsValue>> = const { RefCell::new(None) };
}

/// The viewer's subtitle delay in ms: positive shows subtitles later.
pub fn subtitle_delay_ms() -> i64 {
    SUBTITLE_DELAY_MS.with(Cell::get)
}

/// Show every subtitle renderer's cues `delay_ms` later than the file says.
pub fn set_subtitle_delay(video_id: &str, delay_ms: i64) {
    SUBTITLE_DELAY_MS.with(|d| d.set(delay_ms));
    LIVE_JASSUB.with(|j| {
        if let Some(jassub) = j.borrow().as_ref() {
            set_jassub_delay(jassub, delay_ms);
        }
    });
    shift_native_cues(video_id);
}

/// Re-applies the delay to cues that loaded after it was set: a `<track>`
/// fetches its file on its own, after `show_text_track` ran.
pub fn refresh_subtitle_delay(video_id: &str) {
    shift_native_cues(video_id);
}

/// JASSUB draws the script at `mediaTime + timeOffset`, so a later
/// subtitle is a negative offset.
fn set_jassub_delay(jassub: &JsValue, delay_ms: i64) {
    let _ = js_sys::Reflect::set(
        jassub,
        &JsValue::from_str("timeOffset"),
        &JsValue::from_f64(-(delay_ms as f64) / 1000.0),
    );
}

/// Moves each loaded `<track>` cue to its file time plus the delay. The
/// file time is kept on the cue the first time it is moved, so a new delay
/// replaces the old one instead of adding to it.
fn shift_native_cues(video_id: &str) {
    let Some(tracks) = video_element(video_id).and_then(|v| v.text_tracks()) else {
        return;
    };
    let delay_s = subtitle_delay_ms() as f64 / 1000.0;
    let key = |k: &str| JsValue::from_str(k);
    for i in 0..tracks.length() {
        let Some(t) = tracks.get(i) else { continue };
        let Ok(cues) = js_sys::Reflect::get(&t, &key("cues")) else {
            continue;
        };
        let count = js_sys::Reflect::get(&cues, &key("length"))
            .ok()
            .and_then(|n| n.as_f64())
            .unwrap_or(0.0) as u32;
        for c in 0..count {
            let Ok(cue) = js_sys::Reflect::get_u32(&cues, c) else {
                continue;
            };
            let time = |own: &str, file: &str| -> Option<f64> {
                let kept = js_sys::Reflect::get(&cue, &key(file)).ok()?.as_f64();
                kept.or_else(|| {
                    let t = js_sys::Reflect::get(&cue, &key(own)).ok()?.as_f64()?;
                    let _ = js_sys::Reflect::set(&cue, &key(file), &JsValue::from_f64(t));
                    Some(t)
                })
            };
            let (Some(start), Some(end)) = (
                time("startTime", "pharosFileStart"),
                time("endTime", "pharosFileEnd"),
            ) else {
                continue;
            };
            let _ = js_sys::Reflect::set(
                &cue,
                &key("startTime"),
                &JsValue::from_f64((start + delay_s).max(0.0)),
            );
            let _ = js_sys::Reflect::set(
                &cue,
                &key("endTime"),
                &JsValue::from_f64((end + delay_s).max(0.0)),
            );
        }
    }
}

pub use super::subs_web::{rust_subtitles, RustSubtitles};
pub use super::turn_web::{turn_begin, turn_cancel, turn_expect, turn_open, turn_ready};
pub use super::xr_web::{enter_vr, exit_vr, vr_supported};

/// The shared resize observer for the JASSUB canvases and its callback,
/// kept alive together.
type CanvasObserver = (web_sys::ResizeObserver, Closure<dyn FnMut()>);

thread_local! {
    /// Whether the control bar shows, for the JASSUB canvas lift (B634).
    static STYLED_CONTROLS_VISIBLE: Cell<bool> = const { Cell::new(true) };
    /// Watches each JASSUB canvas so the lift follows a resize or full
    /// screen: it fires after JASSUB refits the canvas, at most once a frame.
    static STYLED_LIFT_OBSERVER: RefCell<Option<CanvasObserver>> = const { RefCell::new(None) };
    /// When the styled script's signs show, and whether one shows now: the
    /// lift waits while one does (B1211).
    static STYLED_SIGNS: RefCell<Vec<(u64, u64)>> = const { RefCell::new(Vec::new()) };
    static STYLED_SIGN_SHOWING: Cell<bool> = const { Cell::new(false) };
}

/// Re-reads whether a sign shows at `t_ms`, and redoes the lift when that
/// changed.
fn follow_styled_signs(t_ms: u64) {
    let showing = STYLED_SIGNS.with(|s| super::subtitles::sign_showing(&s.borrow(), t_ms));
    if STYLED_SIGN_SHOWING.with(|c| c.replace(showing)) != showing {
        lift_styled_subtitles();
    }
}

/// Whether the player's control bar shows. Both subtitle renderers clear it
/// while it does: the Rust one places its text above it, and the JASSUB
/// canvas moves up (`lift_styled_subtitles`). Called when the bar shows or
/// hides, not per pointer move.
pub fn set_subtitle_controls_visible(visible: bool) {
    super::subs_web::set_controls_visible(visible);
    STYLED_CONTROLS_VISIBLE.with(|c| c.set(visible));
    lift_styled_subtitles();
}

/// Has the shared resize observer watch `canvas` (a no-op if it already
/// does), so its lift is redone when JASSUB refits it.
fn observe_styled_canvas(canvas: &web_sys::Element) {
    STYLED_LIFT_OBSERVER.with(|o| {
        let mut o = o.borrow_mut();
        if o.is_none() {
            let cb = Closure::<dyn FnMut()>::new(lift_styled_subtitles);
            if let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
                *o = Some((observer, cb));
            }
        }
        if let Some((observer, _)) = o.as_ref() {
            observer.observe(canvas);
        }
    });
}

/// Moves each JASSUB canvas clear of the control bar while it shows, by
/// setting `--pharos-ass-lift` on its frame; pharos.css turns that into a
/// transform, so the script is not drawn again (B634, V583). Runs when the
/// bar shows or hides, when a renderer gets ready, and when a canvas is
/// resized; the transform itself changes no canvas size, so it does not
/// trigger itself.
fn lift_styled_subtitles() {
    let visible = STYLED_CONTROLS_VISIBLE.with(Cell::get);
    let sign_showing = STYLED_SIGN_SHOWING.with(Cell::get);
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Ok(canvases) = doc.query_selector_all(".pharos-video-frame > canvas.JASSUB") else {
        return;
    };
    for i in 0..canvases.length() {
        let Some(canvas) = canvases
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
        else {
            continue;
        };
        observe_styled_canvas(&canvas);
        let Some(frame) = canvas
            .parent_element()
            .and_then(|f| f.dyn_into::<web_sys::HtmlElement>().ok())
        else {
            continue;
        };
        let player = frame.closest(".pharos-player").ok().flatten();
        let (player_bottom, controls_height) = player
            .as_ref()
            .and_then(|p| {
                let bar = p
                    .query_selector(".pharos-controls")
                    .ok()
                    .flatten()?
                    .dyn_into::<web_sys::HtmlElement>()
                    .ok()?;
                Some((
                    p.get_bounding_client_rect().bottom(),
                    f64::from(bar.offset_height()),
                ))
            })
            .unwrap_or((f64::NAN, 0.0));
        // Layout offsets, not the on-screen box: the canvas's own transform
        // must not feed back into its lift.
        let canvas_bottom = frame.get_bounding_client_rect().top()
            + f64::from(canvas.offset_top() + canvas.offset_height());
        let lift = super::subtitles::styled_lift(
            visible,
            sign_showing,
            canvas_bottom,
            player_bottom,
            controls_height,
        );
        let _ = frame
            .style()
            .set_property("--pharos-ass-lift", &lift.to_string());
    }
}

/// Where the `<video>` `video_id`'s cues are laid out now: the width a cue
/// of `size` 100 spans (the picture's: `.pharos-video` is `object-fit:
/// contain`, so a letterboxed picture is narrower than its element) and the
/// cue font size (`cue_font_px`) from the width CSS resolves `cqw` against
/// (the enclosing `.pharos-player`, else the video; the viewport while the
/// video itself is full screen, as pharos.css sizes it then), the viewport's
/// height and the `--pharos-subtitle-scale` the Subtitle size choice sets.
#[derive(Clone, Copy)]
struct CueBox {
    width: f64,
    font: f64,
}

fn cue_box_for(video_id: &str) -> CueBox {
    let none = CueBox {
        width: 0.0,
        font: 0.0,
    };
    let Some(window) = web_sys::window() else {
        return none;
    };
    let Some(video) = window
        .document()
        .and_then(|d| d.get_element_by_id(video_id))
    else {
        return none;
    };
    // The on-screen box: a 3D crop stretches the element past its eye along
    // one axis, so the narrower fit below is the eye's.
    let rect = video.get_bounding_client_rect();
    let (picture_w, picture_h) = video
        .dyn_ref::<web_sys::HtmlMediaElement>()
        .map_or((0, 0), picture_size);
    let (picture_w, picture_h) = (f64::from(picture_w), f64::from(picture_h));
    let width = if picture_w > 0.0 && picture_h > 0.0 {
        rect.width().min(rect.height() * picture_w / picture_h)
    } else {
        rect.width()
    };
    let height = window
        .inner_height()
        .ok()
        .and_then(|h| h.as_f64())
        .unwrap_or(0.0);
    let full_screen = window
        .document()
        .and_then(|d| d.fullscreen_element())
        .is_some_and(|e| e.id() == video_id);
    let container = if full_screen {
        window
            .inner_width()
            .ok()
            .and_then(|w| w.as_f64())
            .unwrap_or(rect.width())
    } else {
        video
            .closest(".pharos-player")
            .ok()
            .flatten()
            .map_or(rect.width(), |p| p.get_bounding_client_rect().width())
    };
    let scale = window
        .document()
        .and_then(|d| d.document_element())
        .and_then(|e| window.get_computed_style(&e).ok().flatten())
        .and_then(|s| s.get_property_value("--pharos-subtitle-scale").ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(1.0);
    let mini = video
        .closest(".pharos-stage.is-mini")
        .ok()
        .flatten()
        .is_some();
    CueBox {
        width,
        font: cue_font_px(container, height, scale, mini),
    }
}

/// Set `line` on each horizontal cue in the `TextTrackCueList` `cues`
/// still placed by the browser (`line` is `"auto"`) to what `lifted_line`
/// gives for its text wrapped in `cue_box` (narrowed to the share of the
/// video's width its `size`, `position` and `positionAlign` allow,
/// `cue_width_share`), so its last rendered line sits one
/// line above the picture's bottom edge instead of on it (B545). `line` is
/// the standard `VTTCue` setting, so Firefox honours it too, unlike the
/// `::-webkit-media-text-track-*` pseudo-elements.
fn lift_cues(cues: &JsValue, cue_box: CueBox) {
    if cues.is_null() || cues.is_undefined() {
        return;
    }
    let length = js_sys::Reflect::get(cues, &"length".into())
        .ok()
        .and_then(|n| n.as_f64())
        .unwrap_or(0.0) as u32;
    let line = JsValue::from_str("line");
    let vertical = JsValue::from_str("vertical");
    let cue_text = JsValue::from_str("text");
    let region = JsValue::from_str("region");
    let size_key = JsValue::from_str("size");
    let position_key = JsValue::from_str("position");
    let position_align_key = JsValue::from_str("positionAlign");
    let align_key = JsValue::from_str("align");
    let lifted_key = JsValue::from_str("pharosLifted");
    let text = |cue: &JsValue, key: &JsValue| {
        js_sys::Reflect::get(cue, key)
            .ok()
            .and_then(|v| v.as_string())
    };
    for i in 0..length {
        let Ok(cue) = js_sys::Reflect::get_u32(cues, i) else {
            continue;
        };
        // Only horizontal cues: for `vertical:rl`/`lr` a numeric line moves
        // the cue sideways, not up. A cue in a WebVTT region keeps the
        // region's placement (a numeric line would take it out of the
        // region); a browser without regions has no `region` at all.
        let in_region = js_sys::Reflect::get(&cue, &region)
            .ok()
            .is_some_and(|r| !r.is_null() && !r.is_undefined());
        // A cue Pharos lifted before, and still at the line Pharos gave it,
        // is lifted again for the player's size now; one whose author set
        // its `line` (before or since) is left alone.
        let current = js_sys::Reflect::get(&cue, &line)
            .ok()
            .and_then(|v| v.as_f64());
        let ours = js_sys::Reflect::get(&cue, &lifted_key)
            .ok()
            .and_then(|v| v.as_f64())
            .is_some_and(|previous| current == Some(previous));
        if (ours || text(&cue, &line).as_deref() == Some("auto"))
            && text(&cue, &vertical).as_deref() == Some("")
            && !in_region
        {
            let lines = text(&cue, &cue_text).unwrap_or_default();
            let number = |key: &JsValue| {
                js_sys::Reflect::get(&cue, key)
                    .ok()
                    .and_then(|v| v.as_f64())
            };
            let share = cue_width_share(
                number(&size_key).unwrap_or(100.0),
                number(&position_key),
                &text(&cue, &position_align_key).unwrap_or_default(),
                &text(&cue, &align_key).unwrap_or_default(),
            );
            let chars_per_line = cue_chars_per_line(cue_box.width * share / 100.0, cue_box.font);
            // Written only when it moves: most relifted cues keep their line.
            let lifted = lifted_line(&lines, chars_per_line);
            if current != Some(lifted) {
                let _ = js_sys::Reflect::set(&cue, &line, &JsValue::from_f64(lifted));
            }
            let _ = js_sys::Reflect::set(&cue, &lifted_key, &JsValue::from_f64(lifted));
        }
    }
}

/// The rendered width of the element `id`, 0 when it is gone.
/// Where the media `id` has data up to from its play head: the end of the
/// buffered range holding it, 0 when none does or the element is gone.
pub fn buffered_to_ms(id: &str) -> u64 {
    let Some(media) = video_element(id) else {
        return 0;
    };
    let now = media.current_time();
    let buffered = media.buffered();
    (0..buffered.length())
        .filter_map(|i| Some((buffered.start(i).ok()?, buffered.end(i).ok()?)))
        .find(|(start, end)| *start <= now && now <= *end)
        .map_or(0, |(_, end)| seconds_to_ms(end))
}

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

/// Focuses the text field `id` and selects all of it, ready for Ctrl+C.
pub fn select_field(id: &str) {
    let Some(field) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
    else {
        return;
    };
    let _ = field.focus();
    field.select();
}

/// Moves keyboard focus to `to` when it is inside `within` (B1031).
pub fn focus_out_of(within: &str, to: &str) {
    let Some(doc) = document() else {
        return;
    };
    let Some(within) = doc.get_element_by_id(within) else {
        return;
    };
    if doc
        .active_element()
        .is_some_and(|a| within.contains(Some(&*a)))
    {
        focus_by_id(to);
    }
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

/// Whether the page-wide key layer has its own use for Escape: the key
/// help is open, or the page is in browser fullscreen. A panel that closes
/// on Escape gives way to it.
pub fn escape_claimed() -> bool {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    document.fullscreen_element().is_some()
        || document
            .query_selector(".pharos-key-help")
            .ok()
            .flatten()
            .is_some()
}

/// Leaves browser fullscreen when the page is in it, for an Escape the
/// browser passed on rather than keeping (headless Chromium does): that
/// Escape leaves fullscreen and does nothing else (V854).
pub fn leave_fullscreen_on_escape() -> bool {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    if document.fullscreen_element().is_none() {
        return false;
    }
    document.exit_fullscreen();
    info("pharos: fullscreen action=exit target=page reason=escape");
    true
}

/// Leaves fullscreen, or puts the whole page in it. The page, not the player
/// element: a track or quality change replaces the player node, and an
/// element's fullscreen would end with it (B816).
pub fn toggle_fullscreen() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    if document.fullscreen_element().is_some() {
        document.exit_fullscreen();
        return;
    }
    enter_page_fullscreen_for("key");
}

fn enter_page_fullscreen_for(reason: &'static str) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    if document.fullscreen_element().is_some() {
        info("pharos: fullscreen action=enter skipped reason=already-fullscreen");
        return;
    }
    let Some(root) = document.document_element() else {
        return;
    };
    // web-sys drops the promise, and with it the browser's refusal.
    match root.unchecked_into::<FsElement>().request_fullscreen() {
        Ok(promise) => spawn_detached(async move {
            match wasm_bindgen_futures::JsFuture::from(promise).await {
                Ok(_) => info(&format!(
                    "pharos: fullscreen action=enter target=page reason={reason}"
                )),
                Err(e) => warn(&format!(
                    "pharos: fullscreen action=enter target=page refused reason={reason} error={e:?}"
                )),
            }
        }),
        Err(e) => warn(&format!(
            "pharos: fullscreen action=enter target=page refused reason={reason} error={e:?}"
        )),
    }
}

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = Element)]
    type FsElement;
    #[wasm_bindgen(method, catch, js_name = requestFullscreen)]
    fn request_fullscreen(this: &FsElement) -> Result<js_sys::Promise, JsValue>;
}

/// Leaves the page fullscreen the F key or the Fullscreen button started; a
/// fullscreen on another element is left alone.
pub fn exit_page_fullscreen() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let page = document.document_element();
    if page.is_none() || document.fullscreen_element() != page {
        return;
    }
    document.exit_fullscreen();
    info("pharos: fullscreen action=exit target=page reason=player-left-full");
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

/// The page's `scheme://host[:port]`, for links meant for another device.
pub fn page_origin() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}

/// Puts `text` on the clipboard; `false` when the browser refused or has no
/// clipboard (`navigator.clipboard` needs a secure context, so plain-http
/// LAN pages have none).
pub async fn copy_text(text: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let clipboard = js_sys::Reflect::get(&window.navigator(), &JsValue::from_str("clipboard"))
        .ok()
        .filter(|c| !c.is_undefined() && !c.is_null());
    let Some(clipboard) = clipboard else {
        info("pharos: copy outcome=refused reason=no_clipboard");
        return false;
    };
    let Some(write) = js_sys::Reflect::get(&clipboard, &JsValue::from_str("writeText"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
    else {
        info("pharos: copy outcome=refused reason=no_write_text");
        return false;
    };
    let promise = match write.call1(&clipboard, &JsValue::from_str(text)) {
        Ok(p) => js_sys::Promise::from(p),
        Err(e) => {
            warn(&format!(
                "pharos: copy outcome=failed reason=threw error={e:?}"
            ));
            return false;
        }
    };
    match wasm_bindgen_futures::JsFuture::from(promise).await {
        Ok(_) => true,
        Err(e) => {
            warn(&format!(
                "pharos: copy outcome=failed reason=rejected error={e:?}"
            ));
            false
        }
    }
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

pub(super) fn seconds_to_ms(secs: f64) -> u64 {
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

/// Call `on_key` with each plain key pressed outside a text field that no
/// handler nearer the focus took (spec 029): no Ctrl, Alt or Cmd, not a
/// held key's repeat. When it answers true the browser does not act too.
pub fn on_mode_key(mut on_key: impl FnMut(&str) -> bool + 'static) -> KeyListener {
    let listener =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if e.ctrl_key()
                || e.alt_key()
                || e.meta_key()
                || e.repeat()
                || e.default_prevented()
                || in_text_field(&e)
            {
                return;
            }
            if on_key(&e.key()) {
                e.prevent_default();
            }
        });
    if let Some(window) = web_sys::window() {
        let _ =
            window.add_event_listener_with_callback("keydown", listener.as_ref().unchecked_ref());
    }
    KeyListener(listener)
}

/// What a key can move the focus to: enabled buttons and links. Text and
/// choice fields are left out, since their keys type: `/` reaches search.
const KEY_TARGETS: &str = "a[href], button:not([disabled]), [tabindex]:not([tabindex='-1'])";

fn document() -> Option<web_sys::Document> {
    web_sys::window().and_then(|w| w.document())
}

/// The focused element, unless the focus is on nothing (the body).
fn focused() -> Option<web_sys::Element> {
    let doc = document()?;
    let el = doc.active_element()?;
    let body: Option<web_sys::Element> = doc.body().map(Into::into);
    (Some(&el) != body.as_ref()).then_some(el)
}

/// Focus `el` without scrolling the page when `scroll` is false.
fn focus_element(el: &web_sys::Element, scroll: bool) -> bool {
    let options = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &options,
        &JsValue::from_str("preventScroll"),
        &JsValue::from_bool(!scroll),
    );
    let focus = js_sys::Reflect::get(el, &JsValue::from_str("focus"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    match focus.map(|f| f.call1(el, &options)) {
        Some(Ok(_)) => true,
        Some(Err(e)) => {
            warn(&format!("pharos: keys: focus refused: {e:?}"));
            false
        }
        None => false,
    }
}

fn shown_targets(scope: &web_sys::Element) -> Vec<(web_sys::Element, super::keymap::Rect)> {
    let Ok(list) = scope.query_selector_all(KEY_TARGETS) else {
        return Vec::new();
    };
    (0..list.length())
        .filter_map(|i| list.item(i)?.dyn_into::<web_sys::Element>().ok())
        .filter_map(|el| {
            let r = el.get_bounding_client_rect();
            let shown = r.width() > 0.0 && r.height() > 0.0;
            shown.then(|| (el, (r.left(), r.top(), r.width(), r.height())))
        })
        .collect()
}

thread_local! {
    /// Per page path, the control the arrow keys last focused there: its
    /// selector (`href`, else `id`) and which match it was (B712).
    static LEFT_AT: RefCell<HashMap<String, (String, u32)>> = RefCell::new(HashMap::new());
}

/// Pages whose last focus is kept; past this the oldest are forgotten
/// wholesale, so a TV left running for days keeps a small map.
const LEFT_AT_PAGES: usize = 64;

/// `value` inside a single-quoted CSS attribute selector.
fn css_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

/// How a control is found again: a link by its `href`, else by its `id`.
fn focus_mark(el: &web_sys::Element) -> Option<String> {
    el.get_attribute("href")
        .map(|href| format!("[href='{}']", css_quoted(&href)))
        .or_else(|| {
            Some(el.id())
                .filter(|id| !id.is_empty())
                .map(|id| format!("[id='{}']", css_quoted(&id)))
        })
}

/// Keep `el` as where the keys left this page; a control that cannot be
/// found again forgets the page's earlier mark rather than restore a
/// stale one.
fn remember_focus(doc: &web_sys::Document, el: &web_sys::Element) {
    let path = current_path();
    let mark = focus_mark(el).and_then(|mark| {
        let list = doc.query_selector_all(&mark).ok()?;
        let nth = (0..list.length()).find(|&i| {
            list.item(i)
                .is_some_and(|node| node.is_same_node(Some(el.as_ref())))
        })?;
        Some((mark, nth))
    });
    LEFT_AT.with(|m| {
        let mut m = m.borrow_mut();
        match mark {
            Some(mark) => {
                if m.len() >= LEFT_AT_PAGES && !m.contains_key(&path) {
                    m.clear();
                }
                m.insert(path, mark);
            }
            None => {
                m.remove(&path);
            }
        }
    });
}

/// Back on a page with the focus on nothing: focus the control the arrow
/// keys left it on, so a remote's Back returns to the tile it opened.
fn focus_where_left(doc: &web_sys::Document) -> bool {
    let Some((mark, nth)) = LEFT_AT.with(|m| m.borrow().get(&current_path()).cloned()) else {
        return false;
    };
    let Some(el) = doc
        .query_selector_all(&mark)
        .ok()
        .and_then(|list| list.item(nth))
        .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
    else {
        return false;
    };
    let r = el.get_bounding_client_rect();
    if r.width() <= 0.0 || r.height() <= 0.0 || !focus_element(&el, true) {
        return false;
    }
    el.scroll_into_view_with_bool(false);
    true
}

/// With the focus on nothing, focus the first control in view: in an open
/// modal dialog, else in the page's content, else in its key area, so the
/// first press after a page change lands on the page and not its header.
fn focus_first_shown(doc: &web_sys::Document) -> bool {
    let (_, view_h) = viewport();
    let in_view = |area: &web_sys::Element| {
        shown_targets(area)
            .into_iter()
            .find(|(_, r)| r.1 + r.3 > 0.0 && r.1 < view_h)
    };
    let pick = |selector: &str| doc.query_selector(selector).ok().flatten();
    let first = match pick(MODAL) {
        Some(modal) => in_view(&modal),
        None => pick("[data-keys] main")
            .and_then(|main| in_view(&main))
            .or_else(|| pick("[data-keys]").and_then(|area| in_view(&area))),
    };
    first.is_some_and(|(el, _)| focus_element(&el, true))
}

/// An open modal dialog, which keeps the arrow keys' focus inside it.
const MODAL: &str = "[aria-modal='true']";

/// Move the focus in `dir` among the controls of the nearest `data-keys`
/// area around it; First and Last stay inside its `data-keys-list`, when
/// it is in one. With the focus on nothing, focus the area's first
/// control in view. False when there was nowhere to go.
pub fn move_focus(dir: super::keymap::Dir) -> bool {
    use super::keymap::Dir;
    let Some(doc) = document() else {
        return false;
    };
    let Some(from) = focused() else {
        // An open modal dialog wins over where the page was left.
        let modal_open = doc.query_selector(MODAL).ok().flatten().is_some();
        return (!modal_open && focus_where_left(&doc)) || focus_first_shown(&doc);
    };
    let near = |selector: &str| from.closest(selector).ok().flatten();
    // A modal dialog holds the focus: the keys never walk out to the page
    // behind it (B711).
    let area = || near(MODAL).or_else(|| near("[data-keys]"));
    let scope = match dir {
        Dir::First | Dir::Last => near("[data-keys-list]").or_else(area),
        _ => area(),
    };
    let Some(scope) = scope else {
        return false;
    };
    let targets = shown_targets(&scope);
    let r = from.get_bounding_client_rect();
    let rects: Vec<_> = targets.iter().map(|(_, r)| *r).collect();
    match super::keymap::pick(dir, (r.left(), r.top(), r.width(), r.height()), &rects)
        .and_then(|i| targets.get(i))
    {
        Some((el, _)) if *el != from => {
            let moved = focus_element(el, true);
            if moved {
                el.scroll_into_view_with_bool(false);
                remember_focus(&doc, el);
            }
            moved
        }
        _ => false,
    }
}

fn click(el: &web_sys::Element) -> bool {
    match el.dyn_ref::<web_sys::HtmlElement>() {
        Some(html) => {
            html.click();
            true
        }
        None => false,
    }
}

/// Click the focused control, as Enter would.
pub fn activate_focused() -> bool {
    focused().is_some_and(|el| click(&el))
}

/// Click the control marked `data-key-action="<name>"`: the one in the
/// focused `data-key-row` when there is one, else the page's first that is
/// not marked `data-key-row-only`.
pub fn press_key_action(name: &str) -> bool {
    let selector = format!("[data-key-action='{name}']");
    let in_row = focused()
        .and_then(|el| el.closest("[data-key-row]").ok().flatten())
        .and_then(|row| row.query_selector(&selector).ok().flatten());
    let found = in_row.or_else(|| {
        document()?
            .query_selector(&format!("main {selector}:not([data-key-row-only])"))
            .ok()
            .flatten()
    });
    found.is_some_and(|el| click(&el))
}

/// The page's player: the video's, or the music full player's.
const PLAYERS: &str = "#pharos-player, .pharos-listen";

/// Press `key` on the page's player when the focus is outside it, since a
/// player's transport keys are its own handler's (B618). The key is sent
/// after this listener returns, so the layer does not hear it again
/// inside its own call; the player then takes it and the layer, hearing
/// it handled, leaves it. False when there is no player or the focus is
/// already in it, or the key is Space on a focused button or link.
pub fn forward_to_player(key: &str) -> bool {
    let Some(player) = document().and_then(|d| d.query_selector(PLAYERS).ok().flatten()) else {
        return false;
    };
    let at = focused();
    if at
        .as_ref()
        .is_some_and(|el| player.contains(Some(el.unchecked_ref())))
    {
        return false;
    }
    // Space presses a focused button or link, as everywhere.
    if key == " " && at.is_some_and(|el| el.matches("a[href], button").unwrap_or(false)) {
        return false;
    }
    focus_element(&player, false);
    let key = key.to_owned();
    spawn_detached(async move {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(&key);
        init.set_bubbles(true);
        init.set_cancelable(true);
        init.set_shift_key(key.chars().any(char::is_uppercase));
        if let Ok(e) = web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init) {
            let _ = player.dispatch_event(&e);
        }
    });
    true
}

fn shown_in_page(selector: &str) -> bool {
    document()
        .and_then(|d| d.query_selector(selector).ok().flatten())
        .is_some()
}

/// Whether the page has a control a key presses as `name`, where
/// [`press_key_action`] looks for one.
pub fn key_action_shown(name: &str) -> bool {
    shown_in_page(&format!("main [data-key-action='{name}']"))
}

/// Whether the page has tabs `[` and `]` step along.
pub fn key_tabs_shown() -> bool {
    shown_in_page("[data-key-tabs]")
}

/// Whether the page has the `data-key-area="<name>"` area.
pub fn key_area_shown(name: &str) -> bool {
    shown_in_page(&format!("[data-key-area='{name}']"))
}

/// Whether the focus is inside the `data-key-area="<name>"` area.
pub fn focus_in_area(name: &str) -> bool {
    focused()
        .and_then(|el| {
            el.closest(&format!("[data-key-area='{name}']"))
                .ok()
                .flatten()
        })
        .is_some()
}

/// What an open modal dialog does to the page behind it (B825): every
/// element outside it is made `inert`, so Tab, the pointer and screen
/// readers stay inside, and closing it puts the focus back where it was.
/// Hold it for as long as the dialog is mounted; dropping it undoes both.
pub struct ModalHold {
    /// The control that had the focus when the dialog opened.
    opener: Option<web_sys::Element>,
    inerted: Vec<web_sys::Element>,
    root: Option<web_sys::Element>,
}

impl ModalHold {
    /// Notes the focused control. Call it as the dialog first renders,
    /// before it takes the focus.
    pub fn capture() -> Self {
        Self {
            opener: focused(),
            inerted: Vec::new(),
            root: None,
        }
    }

    /// Makes the page outside `root` (a selector for the dialog's outermost
    /// element, its backdrop included so a click there still closes it)
    /// inert. Call it once the dialog is in the page.
    pub fn engage(&mut self, root: &str) {
        if !self.inerted.is_empty() {
            return;
        }
        let Some(found) = document().and_then(|d| d.query_selector(root).ok().flatten()) else {
            warn(&format!("pharos: modal root {root} not in the page"));
            return;
        };
        let mut node = found.clone();
        while let Some(parent) = node.parent_element() {
            let children = parent.children();
            for at in 0..children.length() {
                let Some(sibling) = children.item(at) else {
                    continue;
                };
                // Scripts and styles are not content; one that is inert
                // already (another dialog's doing) is left to its owner.
                if sibling == node
                    || sibling.has_attribute("inert")
                    || matches!(sibling.tag_name().as_str(), "SCRIPT" | "STYLE" | "LINK")
                {
                    continue;
                }
                if sibling.set_attribute("inert", "").is_ok() {
                    self.inerted.push(sibling);
                }
            }
            if parent.tag_name() == "BODY" {
                break;
            }
            node = parent;
        }
        self.root = Some(found);
    }
}

impl Drop for ModalHold {
    fn drop(&mut self) {
        for el in self.inerted.drain(..) {
            let _ = el.remove_attribute("inert");
        }
        // Back to the opener, unless the focus has already gone somewhere
        // on the page the dialog led to.
        let inside = |el: &web_sys::Element| {
            self.root
                .as_ref()
                .is_some_and(|root| root.contains(Some(el)))
        };
        let stranded = focused().is_none_or(|now| inside(&now));
        if let Some(opener) = self.opener.take().filter(|o| o.is_connected()) {
            if stranded {
                focus_element(&opener, false);
            }
        }
    }
}

/// Whether a dialog is open in the page, other than the key help.
pub fn dialog_open() -> bool {
    document()
        .and_then(|d| {
            d.query_selector("[role='dialog']:not(.pharos-key-help), [role='alertdialog']")
                .ok()
                .flatten()
        })
        .is_some()
}

/// Focus the first control shown in the `data-key-area="<name>"` area.
/// False when the area is not in the page.
pub fn focus_key_area(name: &str) -> bool {
    let Some(area) = document().and_then(|d| {
        d.query_selector(&format!("[data-key-area='{name}']"))
            .ok()
            .flatten()
    }) else {
        return false;
    };
    shown_targets(&area)
        .first()
        .is_some_and(|(el, _)| focus_element(el, true))
}

/// Open the tab `step` places from the current one in the page's
/// `data-key-tabs` strip, and focus it.
pub fn step_tab(step: i8) -> bool {
    let Some(strip) = document().and_then(|d| d.query_selector("[data-key-tabs]").ok().flatten())
    else {
        return false;
    };
    let tabs = strip.children();
    let current = (0..tabs.length()).find(|&i| {
        tabs.item(i)
            .is_some_and(|t| t.get_attribute("aria-current").is_some())
    });
    let next = current.and_then(|i| i.checked_add_signed(i32::from(step)));
    match next.and_then(|i| tabs.item(i)) {
        Some(tab) => {
            focus_element(&tab, true);
            click(&tab)
        }
        None => false,
    }
}

/// Focus the element `id` without scrolling, unless the focus is already
/// on something in the page body: a page opened from the keyboard (the
/// search box, a tile, the palette) puts it on its main action, and one
/// opened by Back keeps its scroll.
pub fn focus_arrival(id: &str) {
    if focused().is_some_and(|el| el.closest("main").ok().flatten().is_some()) {
        return;
    }
    let Some(el) = document().and_then(|d| d.get_element_by_id(id)) else {
        return;
    };
    // A row stands for its first control (the album page's first track).
    let target = if el.matches(KEY_TARGETS).unwrap_or(false) {
        Some(el)
    } else {
        el.query_selector(KEY_TARGETS).ok().flatten()
    };
    if let Some(el) = target {
        focus_element(&el, false);
    }
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

/// The browser's user agent string; empty when it will not say.
pub fn user_agent() -> String {
    web_sys::window()
        .and_then(|w| w.navigator().user_agent().ok())
        .unwrap_or_default()
}

/// The web build is never the Android app's shell; that one is the host build.
pub fn android_shell() -> bool {
    false
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

/// Scrolls the page back to its top.
pub fn scroll_to_top() {
    if let Some(window) = web_sys::window() {
        window.scroll_to_with_x_and_y(0.0, 0.0);
    }
}

/// How far the top of the browse tile at `at` is below the top of the
/// viewport, in CSS px; negative when it is scrolled partly out above.
///
/// Measured from layout offsets rather than the bounding rect, so a tile
/// still mid-way through its arrival animation (a transform) reports where
/// it will settle, not where the animation has it this frame.
pub fn tile_top(at: u32) -> Option<f64> {
    let window = web_sys::window()?;
    let tile: web_sys::HtmlElement = grid_tiles()?.item(at)?.dyn_into().ok()?;
    Some(page_offset(tile).1 - window.scroll_y().unwrap_or(0.0))
}

/// Where `el` sits on the page, its left and top in CSS px, from layout
/// offsets, so a tile mid-way through an animated transform reports where
/// it will settle.
fn page_offset(el: web_sys::HtmlElement) -> (f64, f64) {
    let mut node = Some(el);
    let (mut left, mut top) = (0.0, 0.0);
    while let Some(el) = node {
        left += f64::from(el.offset_left());
        top += f64::from(el.offset_top());
        node = el.offset_parent().and_then(|p| p.dyn_into().ok());
    }
    (left, top)
}

/// Places each arriving grid tile for the library transition by where it
/// sits on screen: its row and column (`--fx-row`, `--fx-col`, and their
/// sum `--fx-wave`, B578) and the warp's pull from the middle of the
/// viewport (`--fx-dx`, `--fx-dy`, `--fx-d`, V492). Tiles already placed
/// keep their place unless `again`, so a page landing later never restarts
/// a tile that has arrived; `again` is for a grid scrolled back to where the
/// viewer left it after its tiles were placed.
pub fn stage_arrival(again: bool) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    let Ok(grids) = document.query_selector_all(".pharos-grid") else {
        return;
    };
    for at in 0..grids.length() {
        let Some(grid) = grids
            .item(at)
            .and_then(|g| g.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        let Ok(found) = grid.query_selector_all(":scope > li.pharos-arrive") else {
            continue;
        };
        let tiles: Vec<web_sys::HtmlElement> = (0..found.length())
            .filter_map(|at| found.item(at)?.dyn_into().ok())
            .collect();
        if !tiles.is_empty() && stage_grid(&window, &tiles, again) {
            hold_lift(&window, &grid);
        }
    }
}

/// Holds the hover lift off `grid` until the pointer next moves (B615): a
/// tile arriving under a pointer at rest, as when the viewer comes back to
/// a library, would otherwise lift as it lands, a jump left and up at the
/// end of the transition.
fn hold_lift(window: &web_sys::Window, grid: &web_sys::Element) {
    thread_local! {
        /// The one page-wide `pointermove` listener is installed.
        static INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// Some grid holds its lift, so the next move releases them all.
        static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if let Err(e) = grid.set_attribute("data-lift-held", "") {
        warn(&format!("pharos: lift hold failed error={e:?}"));
        return;
    }
    HELD.set(true);
    if INSTALLED.replace(true) {
        return;
    }
    // One listener for every held grid, found by attribute when the pointer
    // moves, so no grid or closure stays alive after its page goes away on a
    // device that never sends a move (touch only, remote).
    let release = Closure::<dyn FnMut()>::new(|| {
        if !HELD.replace(false) {
            return;
        }
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        if let Ok(held) = doc.query_selector_all("[data-lift-held]") {
            for at in 0..held.length() {
                if let Some(el) = held
                    .item(at)
                    .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
                {
                    let _ = el.remove_attribute("data-lift-held");
                }
            }
        }
    });
    let passive = web_sys::AddEventListenerOptions::new();
    passive.set_passive(true);
    if let Err(e) = window.add_event_listener_with_callback_and_add_event_listener_options(
        "pointermove",
        release.as_ref().unchecked_ref(),
        &passive,
    ) {
        INSTALLED.set(false);
        HELD.set(false);
        let _ = grid.remove_attribute("data-lift-held");
        warn(&format!("pharos: lift hold listener failed error={e:?}"));
        return;
    }
    // Lives for the page, like the other page-wide listeners here.
    release.forget();
}

/// [`stage_arrival`] for one grid's arriving `tiles`, so each grid's wave
/// counts its own rows; `true` when it placed any tile.
fn stage_grid(window: &web_sys::Window, tiles: &[web_sys::HtmlElement], again: bool) -> bool {
    let offsets: Vec<(f64, f64)> = tiles.iter().map(|t| page_offset(t.clone())).collect();
    let scroll = (
        window.scroll_x().unwrap_or(0.0),
        window.scroll_y().unwrap_or(0.0),
    );
    let boxes: Vec<(f64, f64)> = tiles
        .iter()
        .zip(&offsets)
        .map(|(tile, (_, top))| (*top, f64::from(tile.offset_height())))
        .collect();
    let cells = super::browse::arrival_cells(&boxes, scroll.1);
    let px = |v: Result<JsValue, JsValue>| v.ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
    let view = (px(window.inner_width()), px(window.inner_height()));
    let mut placed = false;
    for ((tile, (left, top)), (row, col)) in tiles.iter().zip(offsets).zip(cells) {
        if !again && tile.has_attribute("data-fx") {
            continue;
        }
        placed = true;
        let place = (
            left - scroll.0,
            top - scroll.1,
            f64::from(tile.offset_width()),
            f64::from(tile.offset_height()),
        );
        let (dx, dy, share) = super::browse::arrival_pull(place, view);
        let style = tile.style();
        let set = [
            ("--fx-row", row.to_string()),
            ("--fx-col", col.to_string()),
            ("--fx-wave", (row + col).to_string()),
            ("--fx-dx", format!("{dx:.0}px")),
            ("--fx-dy", format!("{dy:.0}px")),
            ("--fx-d", format!("{share:.3}")),
        ]
        .iter()
        .try_for_each(|(name, value)| style.set_property(name, value))
        .and_then(|()| tile.set_attribute("data-fx", ""));
        // One tile that refuses keeps the rest of the wave in place.
        if let Err(e) = set {
            warn(&format!(
                "pharos: arrival stage failed row={row} col={col} error={e:?}"
            ));
        }
    }
    placed
}

/// Scrolls the page so the browse tile at `at` sits `top` px below the
/// top of the viewport; `false` when it is not in the page yet.
pub fn scroll_tile_to(at: u32, top: f64) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    match tile_top(at) {
        Some(now) => {
            window.scroll_by_with_x_and_y(0.0, now - top);
            true
        }
        None => false,
    }
}

/// The query of the page's URL, `?` included, or empty.
pub fn current_search() -> String {
    web_sys::window()
        .and_then(|w| w.location().search().ok())
        .unwrap_or_default()
}

/// How long the page must stay still before a scroll counts as settled:
/// long enough that a fling reports once, short enough that a click on a
/// tile straight after scrolling still finds it recorded.
const SCROLL_SETTLE_MS: i32 = 150;

/// A window `scroll` listener and its pending settle timer; both go on drop,
/// so a timer cannot fire for a page that has been left. A click anywhere
/// (seen first, in the capture phase) runs a pending settle at once, while
/// the page is still where the viewer left it: following a link inside the
/// delay must not lose the scroll it waited on. A settle still pending on
/// drop is dropped, not run, since by then the next view has moved the page.
pub struct ScrollWatch {
    listener: Closure<dyn FnMut(web_sys::Event)>,
    flush: Closure<dyn FnMut(web_sys::Event)>,
    pending: Rc<Cell<Option<i32>>>,
    _settled: Rc<Closure<dyn FnMut()>>,
}

impl Drop for ScrollWatch {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window.remove_event_listener_with_callback(
                "scroll",
                self.listener.as_ref().unchecked_ref(),
            );
            let _ = window.remove_event_listener_with_callback_and_bool(
                "click",
                self.flush.as_ref().unchecked_ref(),
                true,
            );
            if let Some(timer) = self.pending.take() {
                window.clear_timeout_with_handle(timer);
            }
        }
    }
}

/// Call `on_settled` once the page has stopped scrolling, or straight away
/// on a click while a settle is pending.
pub fn on_scroll_settled(mut on_settled: impl FnMut() + 'static) -> ScrollWatch {
    let pending = Rc::new(Cell::new(None::<i32>));
    let fired = pending.clone();
    let settled = Rc::new(Closure::<dyn FnMut()>::new(move || {
        fired.set(None);
        on_settled();
    }));
    let timer = pending.clone();
    let callback = settled.clone();
    let listener = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        let Some(window) = web_sys::window() else {
            return;
        };
        if let Some(old) = timer.take() {
            window.clear_timeout_with_handle(old);
        }
        timer.set(
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    (*callback).as_ref().unchecked_ref(),
                    SCROLL_SETTLE_MS,
                )
                .ok(),
        );
    });
    let waiting = pending.clone();
    let now = settled.clone();
    let flush = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        let Some(timer) = waiting.take() else {
            return;
        };
        if let Some(window) = web_sys::window() {
            window.clear_timeout_with_handle(timer);
        }
        let settled: &js_sys::Function = (*now).as_ref().unchecked_ref();
        let _ = settled.call0(&JsValue::NULL);
    });
    if let Some(window) = web_sys::window() {
        let options = web_sys::AddEventListenerOptions::new();
        options.set_passive(true);
        let _ = window.add_event_listener_with_callback_and_add_event_listener_options(
            "scroll",
            listener.as_ref().unchecked_ref(),
            &options,
        );
        let _ = window.add_event_listener_with_callback_and_bool(
            "click",
            flush.as_ref().unchecked_ref(),
            true,
        );
    }
    ScrollWatch {
        listener,
        flush,
        pending,
        _settled: settled,
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

/// Room kept clear beside a revealed tab: the focus ring's 3px outline,
/// its 3px offset and the edge of its glow (B1312).
const RING_ROOM: f64 = 12.0;

/// Scrolls the sideways row around the element `selector` names so the
/// element is in sight, moving only that row and never the page: the lit
/// library tab past the edge of a phone (B1313).
pub fn reveal_across(selector: &str) {
    let Some(el) = document().and_then(|d| d.query_selector(selector).ok().flatten()) else {
        return;
    };
    let Some(row) = el.parent_element() else {
        return;
    };
    let (outer, inner) = (
        row.get_bounding_client_rect(),
        el.get_bounding_client_rect(),
    );
    // Leave the TV focus ring's room inside the row's edge (B1312), not
    // just the tab's border.
    let shift = if inner.left() - RING_ROOM < outer.left() {
        inner.left() - RING_ROOM - outer.left()
    } else if inner.right() + RING_ROOM > outer.right() {
        inner.right() + RING_ROOM - outer.right()
    } else {
        return;
    };
    row.set_scroll_left(row.scroll_left() + shift.round() as i32);
}

/// Scrolls the element `list` the least it takes to show its descendant
/// `id` whole; nothing when either is not in the page.
pub fn reveal(list: &str, id: &str) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let (Some(list), Some(row)) = (
        document.get_element_by_id(list),
        document.get_element_by_id(id),
    ) else {
        return;
    };
    let (outer, inner) = (
        list.get_bounding_client_rect(),
        row.get_bounding_client_rect(),
    );
    let shift = if inner.top() < outer.top() {
        inner.top() - outer.top()
    } else if inner.bottom() > outer.bottom() {
        inner.bottom() - outer.bottom()
    } else {
        return;
    };
    list.set_scroll_top(list.scroll_top() + shift.round() as i32);
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

/// `visibilitychange` and `pageshow` listeners; removed on drop.
pub struct PageLifecycle {
    visibility: Closure<dyn FnMut(web_sys::Event)>,
    show: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for PageLifecycle {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window.remove_event_listener_with_callback(
                "pageshow",
                self.show.as_ref().unchecked_ref(),
            );
            if let Some(document) = window.document() {
                let _ = document.remove_event_listener_with_callback(
                    "visibilitychange",
                    self.visibility.as_ref().unchecked_ref(),
                );
            }
        }
    }
}

/// Call `on_hidden` when the page becomes hidden (a mobile or TV browser
/// closing a tab often fires this and no `pagehide`), and `on_restored`
/// when the browser shows a page again from its back/forward cache.
pub fn on_page_lifecycle(
    mut on_hidden: impl FnMut() + 'static,
    mut on_restored: impl FnMut() + 'static,
) -> PageLifecycle {
    let visibility = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        let hidden = web_sys::window()
            .and_then(|w| w.document())
            .is_some_and(|d| d.hidden());
        if hidden {
            on_hidden();
        }
    });
    let show = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
        // `PageTransitionEvent.persisted`, read without the web-sys feature.
        let persisted = js_sys::Reflect::get(&e, &JsValue::from_str("persisted"))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if persisted {
            on_restored();
        }
    });
    if let Some(window) = web_sys::window() {
        let _ = window.add_event_listener_with_callback("pageshow", show.as_ref().unchecked_ref());
        if let Some(document) = window.document() {
            let _ = document.add_event_listener_with_callback(
                "visibilitychange",
                visibility.as_ref().unchecked_ref(),
            );
        }
    }
    PageLifecycle { visibility, show }
}

/// The engine for `kind`, loading hls.js first when it is needed.
pub async fn engine(kind: Engine) -> Result<Box<dyn PlaybackEngine>, String> {
    match kind {
        Engine::Direct | Engine::NativeHls => Ok(Box::new(NativeVideo::default())),
        Engine::HlsJs => {
            load_script(HLSJS_SRC, "Hls").await?;
            Ok(Box::new(HlsJs::default()))
        }
        Engine::RustHls => Ok(Box::new(super::hls_web::RustHls::default())),
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
                let reason =
                    format!("the browser could not play this stream (code {code}) {message}");
                // MEDIA_ERR_DECODE: the bytes arrived and would not decode.
                // MEDIA_ERR_SRC_NOT_SUPPORTED: this browser cannot play it.
                on_fatal(match code {
                    3 => Fatal::media(reason),
                    4 => Fatal::of(FailureKind::Unplayable, reason),
                    _ => Fatal::other(reason),
                });
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
pub(super) const HLSJS_SRC: &str = "/ui/assets/vendor/hls.min.js";

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
pub(super) async fn load_script(src: &str, global: &str) -> Result<(), String> {
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
    #[wasm_bindgen(method, getter, js_class = "JASSUB")]
    fn debug(this: &Jassub) -> JsValue;
    #[wasm_bindgen(method, catch, js_class = "JASSUB", js_name = manualRender)]
    fn manual_render(this: &Jassub, frame: &JsValue, force: bool) -> Result<JsValue, JsValue>;
}

/// Draws the frame a paused `video` shows with `jassub`. JASSUB draws on the
/// video's frame callbacks, which a paused video never fires, so a renderer
/// rebuilt while paused (a size, tier or level change) showed nothing until
/// playback resumed.
async fn draw_paused_frame(
    jassub: &Jassub,
    video: &web_sys::HtmlMediaElement,
    live: &std::cell::Cell<bool>,
) {
    if !video.paused() {
        return;
    }
    let size =
        |k: &str| js_sys::Reflect::get(video, &JsValue::from_str(k)).unwrap_or(JsValue::UNDEFINED);
    let frame = js_sys::Object::new();
    for (k, v) in [
        ("mediaTime", JsValue::from_f64(video.current_time())),
        ("width", size("videoWidth")),
        ("height", size("videoHeight")),
    ] {
        let _ = js_sys::Reflect::set(&frame, &JsValue::from_str(k), &v);
    }
    let draw = || async {
        let p = jassub.manual_render(&frame, true)?;
        wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&p)).await
    };
    // The first draw sizes the canvas. JASSUB's own observer of the new canvas
    // resizes it again in the same frames, which clears it and leaves an
    // unforced redraw that libass answers with "unchanged", so draw once more
    // after it has settled.
    let drawn = match draw().await {
        Ok(_) => {
            animation_frame().await;
            animation_frame().await;
            // Dropped or playing by now: its frame callbacks own the canvas.
            if !live.get() || !video.paused() {
                return;
            }
            draw().await
        }
        Err(e) => Err(e),
    };
    match drawn {
        Ok(_) => info(&format!(
            "pharos: subtitle renderer=jassub paused redraw at={:.3}",
            video.current_time()
        )),
        Err(e) => warn(&format!(
            "pharos: subtitle renderer=jassub paused redraw failed error={}",
            js_message(&e)
        )),
    }
}

/// An ASS/SSA script drawn over the `<video>` by libass in a worker. It
/// lays its canvas right after the video, so the video's parent must hold
/// nothing else Dioxus diffs. Dropping it removes the canvas and the worker.
pub struct StyledSubtitles {
    jassub: Jassub,
    live: Rc<std::cell::Cell<bool>>,
    video: web_sys::HtmlMediaElement,
    on_time: Closure<dyn FnMut()>,
}

impl StyledSubtitles {
    /// JASSUB's own running frame counters: frames it skipped because it was
    /// still drawing the last one, and frames it drew. `None` when its debug
    /// counters are missing, as in a JASSUB without them.
    pub fn frames(&self) -> Option<super::playback::RendererFrames> {
        let debug = self.jassub.debug();
        let count = |key: &str| {
            js_sys::Reflect::get(&debug, &JsValue::from_str(key))
                .ok()?
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n as u64)
        };
        Some(super::playback::RendererFrames {
            skipped: count("droppedFrames")?,
            drawn: count("presentedFrames")?,
        })
    }
}

impl Drop for StyledSubtitles {
    fn drop(&mut self) {
        self.live.set(false);
        let _ = self.jassub.destroy();
        let f = self.on_time.as_ref().unchecked_ref();
        for event in ["timeupdate", "seeked"] {
            let _ = self.video.remove_event_listener_with_callback(event, f);
        }
        STYLED_SIGNS.with(|s| s.borrow_mut().clear());
        follow_styled_signs(0);
    }
}

/// Resolves once `<video id=video_id>` knows its picture size
/// (`HAVE_METADATA`), at once if it already does, or after
/// `STYLED_READY_MS` if it never does. A JASSUB renderer built before then
/// sizes itself to a 0x0 picture and never draws: the player used to build
/// one, fetch its script, then build and fetch again once metadata came.
pub async fn video_metadata(video_id: &str) {
    let Some(video) = video_element(video_id) else {
        return;
    };
    if video.ready_state() >= web_sys::HtmlMediaElement::HAVE_METADATA {
        return;
    }
    let loaded = js_sys::Promise::new(&mut |resolve, _| {
        let once = web_sys::AddEventListenerOptions::new();
        once.set_once(true);
        let _ = video.add_event_listener_with_callback_and_add_event_listener_options(
            "loadedmetadata",
            &resolve,
            &once,
        );
    });
    let loaded = std::pin::pin!(wasm_bindgen_futures::JsFuture::from(loaded));
    let timeout = std::pin::pin!(sleep_ms(STYLED_READY_MS));
    let _ = futures_util::future::select(loaded, timeout).await;
}

/// Starts drawing the script at `url` over `<video id=video_id>`, with the
/// fonts at `fonts` (the file's embedded ones) loaded up front so libass finds
/// a style's `Fontname` among them. While it is not dropped, `report` hears
/// `Ok` once the renderer is ready, or why it failed or did not get ready in
/// time.
pub async fn styled_subtitles(
    video_id: &str,
    url: &str,
    fonts: &[String],
    report: Rc<dyn Fn(Result<(), String>)>,
) -> Result<StyledSubtitles, String> {
    let video = video_element(video_id).ok_or_else(|| format!("no <video id={video_id}>"))?;
    load_script(&format!("{JASSUB_DIR}/jassub.js"), "JASSUB").await?;
    let set = |o: &js_sys::Object, k: &str, v: &JsValue| {
        js_sys::Reflect::set(o, &JsValue::from_str(k), v).map_err(|e| format!("{e:?}"))
    };
    let available = js_sys::Object::new();
    set(
        &available,
        "liberation sans",
        &JsValue::from_str(&format!("{JASSUB_DIR}/default.woff2")),
    )?;
    for (family, file) in super::playback::SUBTITLE_FONT_PACK {
        set(
            &available,
            family,
            &JsValue::from_str(&format!("/ui/assets/fonts/{file}")),
        )?;
    }
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
    set(&options, "availableFonts", &available)?;
    // The file's own fonts, handed to libass before the first frame so it
    // matches `Fontname` against their names; each is the few kilobytes the
    // track draws (026 FR-010), not a whole CJK font.
    set(
        &options,
        "fonts",
        &fonts
            .iter()
            .map(|u| JsValue::from_str(u))
            .collect::<js_sys::Array>(),
    )?;
    set(
        &options,
        "defaultFont",
        &JsValue::from_str("liberation sans"),
    )?;
    // Not the Local Font Access API: it asks the viewer for a permission.
    set(&options, "queryFonts", &JsValue::FALSE)?;
    // Its debug counters say how many frames it skipped while busy, which is
    // how a tier drop tells the renderer's cost from the video's (B667).
    set(&options, "debug", &JsValue::TRUE)?;
    let jassub = Jassub::new(&options).map_err(|e| format!("{e:?}"))?;
    set_jassub_delay(&jassub, subtitle_delay_ms());
    LIVE_JASSUB.with(|j| *j.borrow_mut() = Some(JsValue::clone(&jassub)));
    // Counting only: its default per-frame console line would flood the log.
    let debug = jassub.debug();
    if debug.is_object() {
        let _ = js_sys::Reflect::set(
            &debug,
            &JsValue::from_str("onsubtitleFrameCallback"),
            &JsValue::UNDEFINED,
        );
    }
    let live = Rc::new(std::cell::Cell::new(true));
    STYLED_SIGNS.with(|s| s.borrow_mut().clear());
    follow_styled_signs(0);
    let on_time: Closure<dyn FnMut()> = Closure::new({
        let video = video.clone();
        move || follow_styled_signs((video.current_time().max(0.0) * 1000.0) as u64)
    });
    for event in ["timeupdate", "seeked"] {
        let _ = video.add_event_listener_with_callback(event, on_time.as_ref().unchecked_ref());
    }
    // The signs' times, from the script JASSUB fetched (a cached read).
    spawn_detached({
        let live = Rc::clone(&live);
        let url = url.to_owned();
        let video = video.clone();
        async move {
            match get_text(&url).await {
                Ok(script) if live.get() => {
                    let spans = super::subtitles::sign_spans(&script);
                    web_sys::console::info_1(
                        &format!("pharos: styled script signs={}", spans.len()).into(),
                    );
                    STYLED_SIGNS.with(|s| *s.borrow_mut() = spans);
                    follow_styled_signs((video.current_time().max(0.0) * 1000.0) as u64);
                }
                Ok(_) => {}
                Err((status, e)) => {
                    web_sys::console::warn_1(
                        &format!(
                            "pharos: styled script signs not read (status {status:?}: {e}); the lift ignores signs"
                        )
                        .into(),
                    );
                }
            }
        }
    });
    let ready = wasm_bindgen_futures::JsFuture::from(jassub.ready());
    spawn_detached({
        let live = Rc::clone(&live);
        let drawer: Jassub = JsValue::clone(&jassub).unchecked_into();
        let video = video.clone();
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
                let ready = result.is_ok();
                if ready {
                    // The canvas is new: give it the lift the bar already has.
                    lift_styled_subtitles();
                }
                report(result);
                if ready {
                    draw_paused_frame(&drawer, &video, &live).await;
                }
            }
        }
    });
    Ok(StyledSubtitles {
        jassub,
        live,
        video,
        on_time,
    })
}

/// A thrown value's `message`, or its debug form.
pub(super) fn js_message(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &JsValue::from_str("message"))
        .ok()
        .and_then(|m| m.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}

/// Plays an HLS playlist through hls.js and Media Source Extensions.
#[derive(Default)]
pub(super) struct HlsJs {
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
        let start = super::playback::hls_start_position(start_ms);
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
                // The stream is in trouble: show the player's own state, not the turn.
                super::turn_web::turn_cancel_current("engine-fatal");
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
                    Recovery::Fail => on_fatal(Fatal::from_hls(&kind, &details)),
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
        mark_engine(&video, "hlsjs");
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
    // Kept locally whatever the opt-in: it leaves only inside a pressed report.
    super::report::record_event(&event);
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
    super::subs_web::set_size(size);
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

/// Set `data-device` on `<html>` so a TV gets the ten-foot layout.
pub fn apply_device(device: super::playback::DeviceClass) {
    let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    else {
        return;
    };
    if let Err(e) = root.set_attribute("data-device", device.label()) {
        warn(&format!("pharos: device apply failed error={e:?}"));
    }
}

/// Set `data-library-transition` on `<html>` so a library's cards arrive
/// with `fx`.
pub fn apply_library_transition(fx: LibraryTransition) {
    let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    else {
        return;
    };
    if let Err(e) = root.set_attribute("data-library-transition", fx.as_str()) {
        warn(&format!(
            "pharos: library_transition apply failed error={e:?}"
        ));
    }
}

/// Set `data-overlay-fx` on `<html>` so the player's controls hide with `fx`.
pub fn apply_overlay_effect(fx: OverlayEffect) {
    let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    else {
        return;
    };
    if let Err(e) = root.set_attribute("data-overlay-fx", fx.as_str()) {
        warn(&format!("pharos: overlay_effect apply failed error={e:?}"));
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

/// A routed element's nodes, kept for the element's life: an element is
/// routed once (a second `MediaElementSourceNode` throws), and closing its
/// context would silence it, so a dropped tap leaves them for the next one
/// (B1037, V997). A resume (B563) reads them too.
struct TapNodes {
    media: web_sys::HtmlMediaElement,
    context: web_sys::AudioContext,
    analyser: web_sys::AnalyserNode,
    /// How the last play's wake of `context` went (B735).
    wake: Rc<Cell<super::resume::Wake>>,
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

fn context_state(context: &web_sys::AudioContext) -> String {
    js_sys::Reflect::get(context, &JsValue::from_str("state"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// A routed element plays into a suspended `AudioContext` in silence, so
/// wake the context of `media`'s tap before it plays.
fn wake_tap(media: &web_sys::HtmlMediaElement) {
    use super::resume::Wake;
    let Some((context, wake)) = with_tap(media, |t| (t.context.clone(), Rc::clone(&t.wake))) else {
        return;
    };
    let state = context_state(&context);
    wake.set(Wake::None);
    if super::resume::wakes_context(&state) {
        wake.set(Wake::Pending);
        info(&format!(
            "pharos: resume context was not running state={state}"
        ));
        match context.resume() {
            Err(e) => {
                wake.set(Wake::Refused);
                warn(&format!("pharos: resume context resume() refused: {e:?}"));
            }
            // The promise settles after the play; its outcome is logged on
            // its own, as the report (`context_resumed`) only says a wake
            // was asked for.
            Ok(promise) => wasm_bindgen_futures::spawn_local(async move {
                match wasm_bindgen_futures::JsFuture::from(promise).await {
                    Ok(_) => {
                        wake.set(Wake::Resumed);
                        info(&format!(
                            "pharos: resume context resumed state={}",
                            context_state(&context)
                        ));
                    }
                    Err(e) => {
                        wake.set(Wake::Rejected);
                        warn(&format!(
                            "pharos: resume context resume() rejected: {e:?} state={}",
                            context_state(&context)
                        ));
                    }
                }
            }),
        }
    }
}

/// Wake a suspended tap context on the media element `id` that a play did
/// not wake first (V683): a play from outside the player's controls, such
/// as the browser's own media controls, after the pause rested it.
pub fn wake_media_tap(id: &str) {
    let Some(media) = video_element(id) else {
        return;
    };
    let asleep = with_tap(&media, |t| {
        super::resume::wakes_context(&context_state(&t.context))
    });
    if asleep == Some(true) {
        wake_tap(&media);
    }
}

/// Names the MSE engine attaching to `media`, for [`resume_context`].
pub fn mark_engine(media: &web_sys::HtmlMediaElement, engine: &str) {
    if let Err(e) = media.set_attribute(super::resume::ENGINE_MARK, engine) {
        warn(&format!(
            "pharos: engine mark failed engine={engine} error={e:?}"
        ));
    }
}

/// The media element `id` and its audio path as a resume finds them.
pub fn resume_context(id: &str) -> super::resume::ResumeContext {
    use super::resume::{AudioPath, ResumeContext};
    let Some(media) = video_element(id) else {
        return ResumeContext::unavailable();
    };
    let now = media.current_time();
    let buffered = media.buffered();
    let ahead = (0..buffered.length())
        .filter_map(|i| Some((buffered.start(i).ok()?, buffered.end(i).ok()?)))
        .find(|(start, end)| *start <= now && now <= *end)
        .map_or(0.0, |(_, end)| end - now);
    let src = media.current_src();
    let engine = if src.starts_with("blob:") {
        super::resume::blob_engine(media.get_attribute(super::resume::ENGINE_MARK).as_deref())
    } else if src.split('?').next().is_some_and(|p| p.ends_with(".m3u8")) {
        "native_hls"
    } else {
        "direct"
    };
    let audio = with_tap(&media, |t| AudioPath {
        context_state: context_state(&t.context),
        base_latency_ms: seconds_prop_ms(&t.context, "baseLatency"),
        output_latency_ms: seconds_prop_ms(&t.context, "outputLatency"),
    });
    ResumeContext {
        ready_state: media.ready_state(),
        buffered_ahead_ms: seconds_to_ms(ahead),
        engine,
        muted: media.muted() || media.volume() <= 0.0,
        audio,
        user_agent: user_agent(),
    }
}

/// How the last play's wake of the tap on the media element `id` went
/// (B735); `Wake::None` with no tap.
pub fn tap_wake(id: &str) -> super::resume::Wake {
    video_element(id)
        .and_then(|media| with_tap(&media, |t| t.wake.get()))
        .unwrap_or_default()
}

/// Suspend the running tap context on the media element `id`, letting the
/// output stream close (B734, V683). Returns the context's state before, or
/// `None` with no tap.
pub fn rest_tap(id: &str) -> Option<String> {
    let media = video_element(id)?;
    let context = with_tap(&media, |t| t.context.clone())?;
    let state = context_state(&context);
    if state == "running" {
        if let Err(e) = context.suspend() {
            warn(&format!(
                "pharos: audio output rest suspend() refused: {e:?}"
            ));
        }
    }
    Some(state)
}

/// The largest sample in the tap's latest window on the media element `id`,
/// divided by its volume, or `None` with no tap, no volume, or a context
/// that is not running: a suspended context does not refresh the analyser,
/// whose last window may still hold sound from before the pause.
pub fn tap_peak(id: &str) -> Option<f32> {
    let media = video_element(id)?;
    let volume = media.volume() as f32;
    if media.muted() || volume <= 0.0 {
        return None;
    }
    with_tap(&media, |t| {
        if context_state(&t.context) != "running" {
            return None;
        }
        let mut wave = vec![0.0_f32; t.analyser.fft_size() as usize];
        t.analyser.get_float_time_domain_data(&mut wave);
        Some(wave.iter().fold(0.0_f32, |peak, s| peak.max(s.abs())) / volume)
    })
    .flatten()
}

/// Start a new tap's `context` and log how it went (B1141, V1101): Firefox
/// makes a context suspended and starts it a moment later, and one with no
/// sound output never starts and its `resume()` never settles (B1140), so a
/// start still pending after [`super::resume::TAP_START_REPORT_MS`] is
/// reported stuck.
fn report_tap_start(context: &web_sys::AudioContext) {
    use super::resume::{TapStart, TAP_START_REPORT_MS};
    let made = now_ms();
    let resume = match context.resume() {
        Ok(promise) => promise,
        Err(e) => {
            log_tap_start(&TapStart::Refused {
                error: js_error_name(&e),
            });
            return;
        }
    };
    // One verdict: the resume races a timer that resolves to `undefined`
    // (a settled resume resolves to `undefined` too, so the timer's value is
    // a marker of its own), so a start that settles late is never also
    // reported stuck.
    let stuck = JsValue::from_str("pharos-tap-stuck");
    let timer = {
        let stuck = stuck.clone();
        let ms = i32::try_from(TAP_START_REPORT_MS).unwrap_or(i32::MAX);
        js_sys::Promise::new(&mut |resolve, _| {
            let stuck = stuck.clone();
            let fire = Closure::once_into_js(move || {
                let _ = resolve.call1(&JsValue::NULL, &stuck);
            });
            if let Some(window) = web_sys::window() {
                let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                    fire.unchecked_ref(),
                    ms,
                );
            }
        })
    };
    let race = js_sys::Promise::race(&js_sys::Array::of2(&resume, &timer));
    let context = context.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = wasm_bindgen_futures::JsFuture::from(race).await;
        let state = context_state(&context);
        log_tap_start(&match outcome {
            Ok(v) if v == stuck => TapStart::Stuck { state },
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Ok(_) => TapStart::Started {
                state,
                start_ms: (now_ms() - made).max(0.0) as u64,
            },
            Err(e) => TapStart::Rejected {
                error: js_error_name(&e),
                state,
            },
        });
    });
}

fn log_tap_start(start: &super::resume::TapStart) {
    if start.is_fault() {
        warn(&start.line());
    } else {
        info(&start.line());
    }
}

/// A thrown or rejected value's `name` (`NotAllowedError`), one token for a
/// log field; its debug form when it has none.
fn js_error_name(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &JsValue::from_str("name"))
        .ok()
        .and_then(|n| n.as_string())
        .filter(|n| !n.is_empty() && !n.contains(char::is_whitespace))
        .unwrap_or_else(|| {
            format!("{e:?}")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("_")
        })
}

/// Route the media element `media_id` through an analyser, or hand out the
/// routing it already has: an element is routed once (B1037, V997).
///
/// A cross-origin source without CORS would play silence once routed, so
/// only same-origin and MSE (`blob:`) sources are tapped.
pub fn audio_tap(media_id: &str) -> Result<Box<dyn super::visualiser::AudioTap>, String> {
    let media = video_element(media_id).ok_or_else(|| format!("no media element {media_id}"))?;
    let src = media.current_src();
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default();
    // A source fetched with CORS (the TV app's songs, B1032) can be analysed
    // from another origin; one fetched without it reads as silence.
    let cors = media.cross_origin().as_deref() == Some("anonymous");
    super::resume::routable(&src, &origin, cors)?;
    // The query can carry a media token: never log it.
    let path = src.split('?').next().unwrap_or_default();
    release_detached_taps();
    if let Some((context, analyser)) = with_tap(&media, |t| (t.context.clone(), t.analyser.clone()))
    {
        let state = context_state(&context);
        info(&format!(
            "pharos: audio tap outcome=reused path={path} context_state={state}"
        ));
        let _ = context.resume();
        return Ok(Box::new(WebAudioTap {
            context,
            analyser,
            media,
        }));
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
    report_tap_start(&context);
    info(&format!("pharos: audio tap outcome=routed path={path}"));
    TAPS.with(|taps| {
        taps.borrow_mut().push(TapNodes {
            media: media.clone(),
            context: context.clone(),
            analyser: analyser.clone(),
            wake: Rc::default(),
        })
    });
    Ok(Box::new(WebAudioTap {
        context,
        analyser,
        media,
    }))
}

/// Close the routing of elements gone from the page and paused: nothing
/// can play them again, and each holds an `AudioContext`.
fn release_detached_taps() {
    TAPS.with(|taps| {
        taps.borrow_mut().retain(|t| {
            let spent = !t.media.is_connected() && t.media.paused();
            if spent {
                let _ = t.context.close();
                info("pharos: audio tap outcome=released reason=detached");
            }
            !spent
        })
    });
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

/// The session's [`super::group_rpc::Groups`], as this tab's member.
pub type Groups = super::group_rpc::Groups;

pub fn groups(api: &NativeApi) -> Groups {
    Groups::new(api, group_device())
}
