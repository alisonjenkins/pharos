//! Browser glue for the native app: the socket, `/pharos/session`, history
//! and timers. Every result comes back as data for
//! [`Machine`](super::machine::Machine); no state lives here.

use std::cell::RefCell;
use std::rc::Rc;

use super::diagnostics::{self, Sink};
use gloo_net::http::Request;
use pharos_client::api::{Capabilities, ClientEvent, ClientEventKind, ClientLevel, NativeApi};
use pharos_client::browser::{self, Connection};
use pharos_client::group::{GroupUpdate, NativeGroup, NativeMembership};
use pharos_client::{CallInfo, ClientError};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};

use super::machine::{Probe, SignInReply, SignOutReply};
use super::playback::{Engine, MediaState, OnFatal, PlaybackEngine, Recovery, RecoveryBudget};
use super::together::{self, ClockReply, GroupClient, GroupError, GroupSink, GroupSummary, Member};
use pharos_client::api::ItemRef;

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
    })
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
        // A line or page delta is ~16 or ~800 px.
        let scale = match e.delta_mode() {
            web_sys::WheelEvent::DOM_DELTA_LINE => 16.0,
            web_sys::WheelEvent::DOM_DELTA_PAGE => 800.0,
            _ => 1.0,
        };
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
            load_hlsjs().await?;
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

/// Add hls.js's `<script>` once and wait for it.
async fn load_hlsjs() -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    if js_sys::Reflect::has(&window, &JsValue::from_str("Hls")).unwrap_or(false) {
        return Ok(());
    }
    let document = window.document().ok_or("no document")?;
    let script = document
        .create_element("script")
        .map_err(|e| format!("{e:?}"))?
        .dyn_into::<web_sys::HtmlScriptElement>()
        .map_err(|_| "not a script element")?;
    script.set_src(HLSJS_SRC);
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
        .map_err(|_| format!("could not load {HLSJS_SRC}"))?;
    Ok(())
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
        let on_fragment = {
            let budget = Rc::clone(&budget);
            Closure::<dyn FnMut(JsValue, JsValue)>::new(move |_, _| {
                budget.borrow_mut().progressed()
            })
        };
        player.on("hlsError", on_error.as_ref().unchecked_ref());
        player.on("hlsFragLoaded", on_fragment.as_ref().unchecked_ref());
        player.load_source(url);
        player.attach_media(&video);
        self.player = Some(player);
        self._listeners = vec![on_error, on_fragment];
        Ok(())
    }
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
    let window = web_sys::window().ok_or("no window")?;
    let storage = window
        .local_storage()
        .map_err(|e| format!("{e:?}"))?
        .ok_or("localStorage is unavailable")?;
    storage.get_item(key).map_err(|e| format!("{e:?}"))
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
    let window = web_sys::window().ok_or("no window")?;
    let storage = window
        .local_storage()
        .map_err(|e| format!("{e:?}"))?
        .ok_or("localStorage is unavailable")?;
    storage.set_item(key, value).map_err(|e| format!("{e:?}"))
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
    samples: Vec<f32>,
}

impl super::visualiser::AudioTap for WebAudioTap {
    fn read(&mut self, bins: &mut [f32], wave: &mut [f32]) {
        self.analyser.get_float_frequency_data(bins);
        self.analyser.get_float_time_domain_data(&mut self.samples);
        let step = (self.samples.len() / wave.len().max(1)).max(1);
        for (out, sample) in wave.iter_mut().zip(self.samples.iter().step_by(step)) {
            *out = *sample;
        }
    }

    fn bins(&self) -> usize {
        self.analyser.frequency_bin_count() as usize
    }

    fn sample_rate(&self) -> f32 {
        self.context.sample_rate()
    }
}

impl Drop for WebAudioTap {
    fn drop(&mut self) {
        let _ = self.context.close();
    }
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
    Ok(Box::new(WebAudioTap {
        context,
        analyser,
        samples: vec![0.0; FFT_SIZE as usize],
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
    call: CallInfo,
}

pub fn groups(api: &NativeApi) -> Groups {
    Groups {
        group: api.group().clone(),
        member: Rc::default(),
        call: CallInfo {
            ui_route: "together".into(),
            ui_action: "group".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            device: group_device(),
            ..CallInfo::default()
        },
    }
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
    }
}

fn summary(g: pharos_client::group::GroupSummary) -> GroupSummary {
    GroupSummary {
        id: g.id,
        name: g.name,
        item: g.item,
        members: g.members.into_iter().map(member).collect(),
    }
}

/// The store's update for the client's, or `None` for one it cannot use.
fn update(u: GroupUpdate) -> Option<together::GroupUpdate> {
    use together::GroupUpdate as U;
    Some(match u {
        GroupUpdate::Joined(g) => U::Joined(summary(g)),
        GroupUpdate::Play {
            at_server_ms,
            position_ms,
        } => U::Play {
            at_server_ms: ms(at_server_ms),
            position_ms,
        },
        GroupUpdate::Pause {
            at_server_ms,
            position_ms,
        } => U::Pause {
            at_server_ms: ms(at_server_ms),
            position_ms,
        },
        GroupUpdate::Seek {
            at_server_ms,
            position_ms,
        } => U::Seek {
            at_server_ms: ms(at_server_ms),
            position_ms,
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
        GroupUpdate::Ended => U::Ended,
        GroupUpdate::Other => return None,
    })
}

impl Groups {
    fn membership(&self) -> Result<NativeMembership, GroupError> {
        self.member
            .borrow()
            .clone()
            .ok_or_else(|| GroupError("Not in a group".into()))
    }

    /// Wraps `sink` so the first `joined` is also kept for the caller, which
    /// needs the group's id: `create` and `join` answer with the membership
    /// only.
    fn tap(
        sink: GroupSink,
    ) -> (
        impl Fn(GroupUpdate) + 'static,
        Rc<RefCell<Option<GroupSummary>>>,
    ) {
        let first = Rc::new(RefCell::new(None::<GroupSummary>));
        let keep = Rc::clone(&first);
        (
            move |u: GroupUpdate| {
                if let Some(u) = update(u) {
                    if let together::GroupUpdate::Joined(g) = &u {
                        let mut slot = keep.borrow_mut();
                        if slot.is_none() {
                            *slot = Some(g.clone());
                        }
                    }
                    sink(u);
                }
            },
            first,
        )
    }

    async fn settled(
        &self,
        made: Result<NativeMembership, ClientError>,
        first: Rc<RefCell<Option<GroupSummary>>>,
    ) -> Result<GroupSummary, GroupError> {
        let membership = made.map_err(group_error)?;
        *self.member.borrow_mut() = Some(membership);
        let mut waited = 0;
        loop {
            if let Some(g) = first.borrow().clone() {
                return Ok(g);
            }
            if waited >= JOINED_WAIT_MS {
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
        let (on_update, first) = Self::tap(sink);
        let made = self
            .group
            .create(&self.call, item, position_ms, on_update)
            .await;
        self.settled(made, first).await
    }

    async fn join(&self, id: &str, sink: GroupSink) -> Result<GroupSummary, GroupError> {
        let (on_update, first) = Self::tap(sink);
        let made = self.group.join(&self.call, id, on_update).await;
        self.settled(made, first).await
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

    async fn leave(&self) -> Result<(), GroupError> {
        let Some(m) = self.member.borrow_mut().take() else {
            return Ok(());
        };
        m.leave(&self.call).await.map_err(group_error)
    }
}
