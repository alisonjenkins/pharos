//! Browser glue for the native app: the socket, `/pharos/session`, history
//! and timers. Every result comes back as data for
//! [`Machine`](super::machine::Machine); no state lives here.

use std::cell::RefCell;
use std::rc::Rc;

use super::diagnostics::{self, Sink};
use gloo_net::http::Request;
use pharos_client::api::{Capabilities, ClientEvent, ClientEventKind, ClientLevel, NativeApi};
use pharos_client::browser::{self, Connection};
use pharos_client::CallInfo;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};

use super::machine::{Probe, SignInReply, SignOutReply};
use super::playback::{Engine, OnFatal, PlaybackEngine, Recovery, RecoveryBudget};

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
const HLSJS_SRC: &str = "/assets/vendor/hls.min.js";

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
}

thread_local! {
    static TELEMETRY: RefCell<Telemetry> = RefCell::new(Telemetry::default());
}

/// Whether this browser has opted in to sending diagnostics. Unreadable
/// storage counts as opted out.
pub fn opted_in() -> bool {
    diagnostics::opt_in_from(
        stored_theme(diagnostics::OPT_IN_KEY)
            .ok()
            .flatten()
            .as_deref(),
    )
}

/// Stores this browser's diagnostics opt-in.
pub fn set_opted_in(on: bool) -> Result<(), String> {
    store_theme(diagnostics::OPT_IN_KEY, if on { "true" } else { "false" })
}

/// Sends the session's client events from now on, and the held ones.
pub fn install_telemetry(api: &NativeApi) {
    let opted = opted_in();
    let held = TELEMETRY.with(|t| t.borrow_mut().sink.install(api.clone(), opted));
    if !held.is_empty() {
        send(api.clone(), held);
    }
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
        let call = pharos_client::CallInfo {
            ui_route: "telemetry".into(),
            ui_action: "submit".into(),
            ..Default::default()
        };
        if let Err(e) = api.submit_events(&call, &events).await {
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
