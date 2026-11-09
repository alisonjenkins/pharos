//! The Android TV app's platform module (spec 025, ADR-0061): the same
//! calls as [`web`](super), for the shared `App` running natively in Rust
//! with the system WebView only drawing the DOM. It started as a copy of
//! `host.rs`, so a call the browser gains is a compile error here until it
//! has an Android twin; a stub still marked `android: not yet` answers as
//! the host does.
//!
//! How it reaches the page (spec 025, "How the Android UI reads the DOM"):
//! one long-lived script (`BRIDGE_JS`) sends keys, Back (`popstate`) and the
//! viewport into Rust, which keeps them in thread-locals read
//! synchronously; reads made on a key press run as one batched
//! `document::eval`; calls with no answer are a spawned eval. The page's
//! requests to the server (posters, streams, subtitles, the UI's fonts and
//! stylesheet) are answered by `use_server_proxy`, which fetches them with
//! the device token in a header, so no token is ever in a URL. Pure rules
//! live in `tv_page`, which host tests cover.

// Mirrors the browser module's surface; some of it only the browser uses.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use dioxus::core::spawn_forever;
use dioxus::prelude::document;

use super::android::{self, Closed, NetError};
use super::tv_page::{self, Bridge, Paths};

use pharos_client::api::{Capabilities, ClientEvent, NativeApi};
use pharos_client::CallInfo;

use super::machine::{Probe, SignInReply, SignOutReply};
use super::playback::{
    Engine, LibraryTransition, MediaState, OverlayEffect, PlaybackEngine, SubtitleSize,
};

const NO_BROWSER: &str = "the native app needs a browser";

pub struct Connection {
    closed: Closed,
}

pub struct Open {
    pub conn: Connection,
    pub api: NativeApi,
    pub server_schema: Option<u32>,
}

pub enum OpenError {
    NotOpened(Probe),
    NotSignedIn,
    Resume(String),
}

/// The server this app talks to.
pub fn server() -> String {
    tv_page::server_base(android::load_server().as_deref())
}

/// Connect, then resume with the stored device token. No token, or one the
/// server refuses, is `NotSignedIn`: the sign-in form shows.
pub async fn open(call: &CallInfo) -> Result<Open, OpenError> {
    install_bridge();
    let base = server();
    let (client, system, closed) = android::connect(&base).await.map_err(|e| {
        log::warn!("pharos: open failed server={base} error={e}");
        OpenError::NotOpened(Probe::Failed(e))
    })?;
    spawn_forever(system);
    let server_schema = pharos_client::server_schema_version(&client, call)
        .await
        .inspect_err(|e| log::warn!("pharos: serverInfo failed: {e}"))
        .ok();
    let Some(token) = android::load_token() else {
        log::info!("pharos: open server={base} signed_in=false reason=no_token");
        return Err(OpenError::NotSignedIn);
    };
    match android::resume(&client, &closed, call, &token).await {
        Ok(session) => {
            log::info!("pharos: open server={base} signed_in=true");
            Ok(Open {
                api: NativeApi::new(&session),
                conn: Connection { closed },
                server_schema,
            })
        }
        Err(NetError::Client(e)) if e.needs_sign_in() => {
            log::info!("pharos: open server={base} signed_in=false reason=token_refused");
            android::forget_token();
            Err(OpenError::NotSignedIn)
        }
        Err(e) => Err(OpenError::Resume(e.to_string())),
    }
}

/// Why `open`'s socket closed, once it has.
pub async fn closed(conn: &mut Connection) -> String {
    while !conn.closed.get() {
        sleep_ms(500).await;
    }
    "connection closed".into()
}

/// `Bootstrap.login` on a fresh socket (the one `open` made is closed by the
/// server's sign-in deadline while the form shows); the device token it
/// returns is stored for `open` to resume with.
pub async fn sign_in(name: &str, password: &str) -> SignInReply {
    let base = server();
    let (client, system, closed) = match android::connect(&base).await {
        Ok(link) => link,
        Err(e) => return SignInReply::Failed(e),
    };
    spawn_forever(system);
    let call = CallInfo::default();
    match android::login(&client, &closed, &call, name, password).await {
        Ok(token) => match android::store_token(&token) {
            Ok(()) => {
                log::info!("pharos: sign_in ok server={base}");
                SignInReply::Status {
                    status: 204,
                    retry_after: None,
                }
            }
            Err(e) => SignInReply::Failed(format!("could not keep the sign-in: {e}")),
        },
        Err(NetError::Client(pharos_client::ClientError::Api { kind, .. })) => {
            log::info!("pharos: sign_in refused kind={kind}");
            SignInReply::Status {
                status: tv_page::login_status(kind),
                retry_after: None,
            }
        }
        Err(e) => SignInReply::Failed(e.to_string()),
    }
}

/// Forget the device token. The server keeps the session until it lapses
/// or is revoked from another device.
pub async fn sign_out() -> SignOutReply {
    android::forget_token();
    SignOutReply::Status(204)
}

/// GET a same-server path as text, with the device token in a header.
pub async fn get_text(url: &str) -> Result<String, (Option<u16>, String)> {
    let (status, _, body) = fetch_server(url, &[]).await.map_err(|e| (None, e))?;
    if !(200..300).contains(&status) {
        return Err((Some(status), format!("HTTP {status}")));
    }
    String::from_utf8(body).map_err(|e| (None, e.to_string()))
}

pub async fn sleep_ms(ms: u32) {
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(ms))).await;
}

pub fn set_title(_title: &str) {}

pub fn current_path() -> String {
    PATHS.with(|p| p.borrow().current().to_owned())
}

pub fn push(path: &str) {
    PATHS.with(|p| p.borrow_mut().push(path));
    run(format!(
        "history.pushState('pharos', '', {});",
        js_str(path)
    ));
}

/// android: not yet. A newer server's reload has nothing to reload: the app
/// is the build installed.
pub fn reload() {}

pub fn replace(path: &str) {
    PATHS.with(|p| p.borrow_mut().replace(path));
    run(format!(
        "history.replaceState(history.state, '', {});",
        js_str(path)
    ));
}

pub fn on_a_control(_evt: &dioxus::prelude::MouseEvent) -> bool {
    false
}

pub fn swallow_next_context_menu() {}

/// Back to the page before, when the app pushed one; its `popstate` comes
/// through the bridge as the remote's Back does.
pub fn go_back() -> bool {
    let can = PATHS.with(|p| p.borrow().can_go_back());
    if can {
        run("history.back();".into());
    }
    can
}

/// An Escape listener, removed on drop.
pub struct EscapeWatch(u64);

impl Drop for EscapeWatch {
    fn drop(&mut self) {
        unlisten(self.0);
    }
}

pub fn on_escape(on_escape: impl FnMut() + 'static) -> EscapeWatch {
    EscapeWatch(listen(Listener::Escape(Box::new(on_escape))))
}

/// A `popstate` listener, removed on drop.
pub struct PopState(u64);

impl Drop for PopState {
    fn drop(&mut self) {
        unlisten(self.0);
    }
}

pub fn on_pop_state(on_path: impl FnMut(String) + 'static) -> PopState {
    PopState(listen(Listener::Pop(Box::new(on_path))))
}

/// The WebView answers `maybe` for HLS but does not play it well; hls.js
/// over MSE does (spike 0).
pub fn plays_hls() -> bool {
    false
}

pub fn has_mse() -> bool {
    true
}

pub fn capabilities() -> Capabilities {
    Capabilities::default()
}

pub fn position_ms(_video_id: &str) -> u64 {
    0
}

pub fn ended(_video_id: &str) -> bool {
    false
}

pub fn has_data(_video_id: &str) -> bool {
    false
}

pub fn media_state(_video_id: &str) -> MediaState {
    MediaState::default()
}

pub fn seek_ms(_video_id: &str, _ms: u64) {}

pub fn toggle_play(_video_id: &str) {}

pub fn play(_video_id: &str) {}

pub fn pause(_video_id: &str) {}

pub fn set_rate(_video_id: &str, _rate: f64) {}

pub fn unix_ms() -> i64 {
    wall_clock_ms() as i64
}

pub fn set_volume(_video_id: &str, _volume: f64, _muted: bool) {}

pub fn show_text_track(_video_id: &str, _track_id: Option<&str>) {}

pub fn lift_cues_as_shown(_video_id: &str) {}

pub fn relift_cues(_video_id: &str) {}

/// Loads the text track without drawing it; nothing to load on the host.
pub fn hide_text_track(_video_id: &str, _track_id: &str) {}

/// The Rust subtitle driver's handle; the host has no canvas.
pub struct RustSubtitles;

pub async fn rust_subtitles(
    _video_id: &str,
    _vtt_url: &str,
    _report: std::rc::Rc<dyn Fn(super::subtitles::FallbackReason)>,
) -> Result<RustSubtitles, super::subtitles::FallbackReason> {
    Err(super::subtitles::FallbackReason::NoWebgl2)
}

pub fn set_subtitle_controls_visible(_visible: bool) {}

pub async fn video_metadata(_video_id: &str) {}

pub struct StyledSubtitles;

impl StyledSubtitles {
    /// The renderer's frame counters; the host draws nothing.
    pub fn frames(&self) -> Option<super::playback::RendererFrames> {
        None
    }
}

pub async fn styled_subtitles(
    _video_id: &str,
    _url: &str,
    _fonts: &[String],
    _report: std::rc::Rc<dyn Fn(Result<(), String>)>,
) -> Result<StyledSubtitles, String> {
    Err("no renderer on the host".into())
}

/// The video's dropped and total frame counts; the host has no video.
pub fn video_quality(_video_id: &str) -> Option<(u64, u64)> {
    None
}

pub fn page_visible() -> bool {
    true
}

pub fn element_width(_id: &str) -> f64 {
    0.0
}

pub fn element_size(_id: &str) -> (f64, f64) {
    (0.0, 0.0)
}

pub fn viewport() -> (f64, f64) {
    VIEWPORT.with(std::cell::Cell::get)
}

pub fn key_on_itself(_evt: &dioxus::prelude::KeyboardEvent, _class: &str) -> bool {
    false
}

pub fn on_a_button(_evt: &dioxus::prelude::PointerEvent) -> bool {
    false
}

pub fn capture_pointer(_evt: &dioxus::prelude::PointerEvent) {}

/// `sessionStorage`'s twin: kept while the app runs.
pub fn session_get(key: &str) -> Result<Option<String>, String> {
    Ok(SESSION.with(|s| s.borrow().get(key).cloned()))
}

pub fn session_set(key: &str, value: &str) -> Result<(), String> {
    SESSION.with(|s| s.borrow_mut().insert(key.to_owned(), value.to_owned()));
    Ok(())
}

pub fn toggle_fullscreen(_id: &str) {}

pub fn page_origin() -> String {
    String::new()
}

pub async fn copy_text(_text: &str) -> bool {
    false
}

pub fn enter_page_fullscreen() {}

pub fn exit_page_fullscreen() {}

pub fn pip_active() -> bool {
    false
}

pub fn pip_supported() -> bool {
    false
}

pub fn toggle_pip(_id: &str) {}

pub fn select_field(_id: &str) {}

pub fn focus_by_id(id: &str) {
    run(format!(
        "const el = document.getElementById({}); if (el) el.focus();",
        js_str(id)
    ));
}

/// A key listener, removed on drop.
pub struct KeyListener(u64);

impl Drop for KeyListener {
    fn drop(&mut self) {
        unlisten(self.0);
    }
}

/// Every key, text fields included. The page's script has already decided
/// what the WebView does with it (`tv_page::script_takes`), so the answer
/// only stops the mode keys from hearing it, as `preventDefault` does on
/// the web.
pub fn on_key(on_key: impl FnMut(&str, bool, bool) -> bool + 'static) -> KeyListener {
    KeyListener(listen(Listener::App(Box::new(on_key))))
}

/// Plain keys outside a text field that no app-wide listener took.
pub fn on_mode_key(on_key: impl FnMut(&str) -> bool + 'static) -> KeyListener {
    KeyListener(listen(Listener::Mode(Box::new(on_key))))
}

/// Move the focus in `dir` (spec 011 spatial focus). The rects come back in
/// one eval; `keymap::pick` chooses, as on the web, and a second eval
/// focuses. True at once: the page's script already kept the arrow from
/// the WebView's own navigation.
pub fn move_focus(dir: super::keymap::Dir) -> bool {
    spawn_detached(async move {
        if let Err(e) = move_focus_now(dir).await {
            log::warn!("pharos: keys: move failed error={e}");
        }
    });
    true
}

/// Enter on the focused control: the WebView clicks it itself, since the
/// page's script leaves Enter alone.
pub fn activate_focused() -> bool {
    true
}

pub fn press_key_action(_name: &str) -> bool {
    false
}

pub fn step_tab(_step: i8) -> bool {
    false
}

pub fn focus_arrival(id: &str) {
    run(format!(
        "const a = document.activeElement; \
         if (!(a && a.closest('main'))) {{ \
           const el = document.getElementById({}); \
           const t = el && (el.matches({targets}) ? el : el.querySelector({targets})); \
           if (t) t.focus({{preventScroll: true}}); \
         }}",
        js_str(id),
        targets = js_str(KEY_TARGETS),
    ));
}

pub fn focus_key_area(_name: &str) -> bool {
    false
}

pub fn forward_to_player(_key: &str) -> bool {
    false
}

pub fn key_action_shown(_name: &str) -> bool {
    true
}

pub fn key_tabs_shown() -> bool {
    true
}

pub fn key_area_shown(_name: &str) -> bool {
    true
}

pub fn focus_in_area(_name: &str) -> bool {
    false
}

pub fn dialog_open() -> bool {
    false
}

pub struct TrackSkips;

pub fn on_track_skip(
    _on_skip: impl FnMut(super::watch_view::Skip, &'static str) -> bool + 'static,
) -> TrackSkips {
    TrackSkips
}

pub fn offer_track_skips(_previous: bool, _next: bool) {}

/// What the shared UI reads as this device: a TV.
pub fn user_agent() -> String {
    tv_page::USER_AGENT.to_owned()
}

/// Whether the UI runs inside the Android TV app (spec 025).
pub fn android_shell() -> bool {
    cfg!(target_os = "android")
}

pub fn palette_keys() -> &'static str {
    "Ctrl+K"
}

pub fn now_ms() -> f64 {
    thread_local! {
        static START: std::time::Instant = std::time::Instant::now();
    }
    START.with(|start| start.elapsed().as_secs_f64() * 1000.0)
}

pub fn wall_clock_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

/// The hour and minute of `epoch_ms`, in UTC. Only tests render on the
/// host, and they have no locale to follow.
pub fn local_time(epoch_ms: f64) -> Option<String> {
    let minutes = (epoch_ms / 60_000.0) as u64;
    Some(format!("{:02}:{:02}", minutes / 60 % 24, minutes % 60))
}

pub fn spawn_detached(fut: impl std::future::Future<Output = ()> + 'static) {
    spawn_forever(fut);
}

/// The host has no DOM to observe.
pub struct SentinelWatch;

pub fn watch_sentinel(_ids: &[&str], _on_visible: impl FnMut() + 'static) -> Option<SentinelWatch> {
    None
}

pub struct ShiftWheel;

pub fn on_shift_wheel(_on_turn: impl FnMut(f64) + 'static) -> ShiftWheel {
    ShiftWheel
}

pub struct RailGestures;

pub fn rail_gestures() -> RailGestures {
    RailGestures
}

pub fn first_visible_tile() -> Option<u32> {
    None
}

pub fn tile_in_view(_at: u32) -> bool {
    false
}

pub fn scroll_to_tile(_at: u32) -> bool {
    false
}

pub fn scroll_to_top() {}

pub fn tile_top(_at: u32) -> Option<f64> {
    None
}

pub fn stage_arrival(_again: bool) {}

pub fn scroll_tile_to(_at: u32, _top: f64) -> bool {
    false
}

pub fn current_search() -> String {
    String::new()
}

pub struct ScrollWatch;

pub fn on_scroll_settled(_on_settled: impl FnMut() + 'static) -> ScrollWatch {
    ScrollWatch
}

pub fn center_child(_id: &str, _at: u32) -> bool {
    false
}

pub fn reveal(_list: &str, _id: &str) {}

pub struct PageHide;

pub fn on_page_hide(_on_hide: impl FnMut() + 'static) -> PageHide {
    PageHide
}

pub async fn engine(_kind: Engine) -> Result<Box<dyn PlaybackEngine>, String> {
    Err(NO_BROWSER.into())
}

pub fn warn(message: &str) {
    log::warn!("{message}");
}

pub fn info(message: &str) {
    log::info!("{message}");
}

/// No server to submit to on the host: client events are dropped.
pub fn install_telemetry(_api: &NativeApi) {}

pub fn report_event(_event: ClientEvent) {}

/// The host sends no client events, so diagnostics are never opted in.
pub fn opted_in() -> bool {
    false
}

pub fn forget_opt_in() {}

pub async fn load_opted_in(_api: &NativeApi) -> Result<bool, String> {
    Ok(false)
}

pub async fn set_opted_in(_api: &NativeApi, _on: bool) -> Result<(), String> {
    Err("no server to store the choice on".to_owned())
}

/// `localStorage`'s twin: a file in the app's private directory.
pub fn stored_theme(key: &str) -> Result<Option<String>, String> {
    android::load_pref(key)
}

pub fn apply_tokens(declarations: &[(String, String)]) -> Result<(), String> {
    let style: String = declarations
        .iter()
        .map(|(name, value)| format!("{name}: {value}; "))
        .collect();
    set_root_attribute("style", style.trim_end());
    Ok(())
}

pub fn store_theme(key: &str, value: &str) -> Result<(), String> {
    android::store_pref(key, value)
}

pub fn apply_subtitle_size(size: SubtitleSize) {
    set_root_attribute("data-subtitle-size", size.as_str());
}

pub fn apply_device(device: super::playback::DeviceClass) {
    set_root_attribute("data-device", device.label());
}

pub fn apply_library_transition(fx: LibraryTransition) {
    set_root_attribute("data-library-transition", fx.as_str());
}

pub fn apply_overlay_effect(fx: OverlayEffect) {
    set_root_attribute("data-overlay-fx", fx.as_str());
}

/// android: not yet read from the WebView. A TV is dark, wants no less
/// motion than it can draw, and has no fine pointer.
pub fn prefers(query: &str) -> bool {
    query.contains("prefers-color-scheme: dark")
}

pub struct MediaWatch;

pub fn on_media_change(_query: &str, _on_change: impl FnMut() + 'static) -> MediaWatch {
    MediaWatch
}

pub fn download(_filename: &str, _mime: &str, _text: &str) -> Result<(), crate::theme::ThemeError> {
    Ok(())
}

pub async fn pick_file(
    _accept: &str,
    _max_bytes: usize,
) -> Result<Option<String>, crate::theme::ThemeError> {
    Ok(None)
}

pub async fn pick_picture(_max_bytes: usize) -> Result<Option<Vec<u8>>, String> {
    Ok(None)
}

/// No frames on the host: a render loop here waits forever.
pub async fn animation_frame() -> f64 {
    std::future::pending().await
}

pub fn user_activated() -> bool {
    false
}

pub fn theme_palette() -> crate::render::Palette {
    crate::render::Palette::default()
}

pub fn audio_tap(_media_id: &str) -> Result<Box<dyn super::visualiser::AudioTap>, String> {
    Err(NO_BROWSER.into())
}

pub fn media_playing(_id: &str) -> bool {
    false
}

pub fn resume_context(_id: &str) -> super::resume::ResumeContext {
    super::resume::ResumeContext::unavailable()
}

pub fn tap_peak(_id: &str) -> Option<f32> {
    None
}

/// The session's [`super::group_rpc::Groups`], as this TV's member.
pub type Groups = super::group_rpc::Groups;

pub fn groups(api: &NativeApi) -> Groups {
    Groups::new(api, android::group_device())
}

// ---- the bridge ---------------------------------------------------------

thread_local! {
    static PATHS: RefCell<Paths> = RefCell::new(Paths::new());
    /// The CSS viewport; a TV's WebView is 960 x 540 (spike 0) until the
    /// bridge says otherwise.
    static VIEWPORT: std::cell::Cell<(f64, f64)> = const { std::cell::Cell::new((960.0, 540.0)) };
    static SESSION: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    static LISTENERS: RefCell<Vec<(u64, Rc<RefCell<Listener>>)>> = const { RefCell::new(Vec::new()) };
    static NEXT_LISTENER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static BRIDGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Per page path, where the arrow keys last left the focus: a selector
    /// and which match it was (B712's rule, as on the web).
    static LEFT_AT: RefCell<HashMap<String, (String, u32)>> = RefCell::new(HashMap::new());
}

/// The controls the arrow keys move between, as on the web.
const KEY_TARGETS: &str = "a[href], button:not([disabled]), [tabindex]:not([tabindex='-1'])";

/// An app-wide key listener: key, ctrl, meta; true takes the key.
type AppKeys = Box<dyn FnMut(&str, bool, bool) -> bool>;

enum Listener {
    App(AppKeys),
    Mode(Box<dyn FnMut(&str) -> bool>),
    Escape(Box<dyn FnMut()>),
    Pop(Box<dyn FnMut(String)>),
}

fn listen(listener: Listener) -> u64 {
    install_bridge();
    let id = NEXT_LISTENER.with(|n| {
        let id = n.get().wrapping_add(1);
        n.set(id);
        id
    });
    LISTENERS.with(|l| l.borrow_mut().push((id, Rc::new(RefCell::new(listener)))));
    id
}

fn unlisten(id: u64) {
    LISTENERS.with(|l| l.borrow_mut().retain(|(at, _)| *at != id));
}

/// `value` as a JavaScript string literal.
fn js_str(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// Run `script` in the page without waiting for it.
fn run(script: String) {
    spawn_detached(async move {
        if let Err(e) = document::eval(&script).await {
            log::warn!("pharos: eval failed error={e}");
        }
    });
}

fn set_root_attribute(name: &str, value: &str) {
    run(format!(
        "document.documentElement.setAttribute({}, {});",
        js_str(name),
        js_str(value)
    ));
}

/// Keys, Back and the viewport into Rust. The arrows are kept from the
/// WebView's own spatial navigation, which would move the focus a second
/// time, and Space, which a player button would also take as a click; Enter
/// is left to it, so the focused control is clicked.
const BRIDGE_JS: &str = r#"
const send = (v) => dioxus.send(v);
const inText = (t) => !!t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName));
// Space too: the player's mode key toggles play, and the WebView would
// click the focused button as well (it cannot wait for Rust's answer).
const takes = new Set(["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", " "]);
addEventListener("keydown", (e) => {
  if (e.repeat) return;
  const text = inText(e.target);
  if (!text && takes.has(e.key)) e.preventDefault();
  // OK during a remote scrub commits it (B907); the focused button must
  // not also be clicked.
  if (e.key === "Enter" && document.querySelector(".pharos-seek.is-scrubbing")) e.preventDefault();
  send({ t: "key", key: e.key, ctrl: e.ctrlKey, meta: e.metaKey, text });
});
addEventListener("popstate", () => send({ t: "pop", path: location.pathname + location.search }));
const size = () => send({ t: "size", w: innerWidth, h: innerHeight });
addEventListener("resize", size);
size();
await new Promise(() => {});
"#;

/// Start the bridge once: the page's history starts on the app's first
/// page, and its keys and Back come into Rust from then on.
fn install_bridge() {
    if BRIDGED.with(|b| b.replace(true)) {
        return;
    }
    let start = current_path();
    spawn_forever(async move {
        let _ = document::eval(&format!(
            "history.replaceState('pharos', '', {});",
            js_str(&start)
        ))
        .await;
        let mut bridge = document::eval(BRIDGE_JS);
        log::info!("pharos: bridge installed");
        loop {
            match bridge.recv::<serde_json::Value>().await {
                Ok(value) => match tv_page::parse_bridge(&value) {
                    Some(message) => on_bridge(message),
                    None => log::warn!("pharos: bridge message unknown value={value}"),
                },
                Err(e) => {
                    log::warn!("pharos: bridge ended error={e}");
                    BRIDGED.with(|b| b.set(false));
                    return;
                }
            }
        }
    });
}

fn on_bridge(message: Bridge) {
    let listeners: Vec<_> = LISTENERS.with(|l| l.borrow().iter().map(|(_, f)| f.clone()).collect());
    match message {
        Bridge::Key {
            key,
            ctrl,
            meta,
            text,
        } => {
            let mut taken = false;
            for f in &listeners {
                if let Ok(mut f) = f.try_borrow_mut() {
                    if let Listener::App(f) = &mut *f {
                        taken |= f(&key, ctrl, meta);
                    }
                }
            }
            if taken {
                return;
            }
            if !(text || ctrl || meta) {
                for f in &listeners {
                    if let Ok(mut f) = f.try_borrow_mut() {
                        if let Listener::Mode(f) = &mut *f {
                            taken |= f(&key);
                        }
                    }
                }
            }
            // As on the web, Escape reaches these from a text field too.
            if taken || key != "Escape" {
                return;
            }
            for f in &listeners {
                if let Ok(mut f) = f.try_borrow_mut() {
                    if let Listener::Escape(f) = &mut *f {
                        f();
                    }
                }
            }
        }
        Bridge::Pop { path } => {
            PATHS.with(|p| p.borrow_mut().popped_to(&path));
            for f in &listeners {
                if let Ok(mut f) = f.try_borrow_mut() {
                    if let Listener::Pop(f) = &mut *f {
                        f(path.clone());
                    }
                }
            }
        }
        Bridge::Size { w, h } => VIEWPORT.with(|v| v.set((w, h))),
    }
}

/// The page's answer to a focus move's first eval.
#[derive(serde::Deserialize)]
struct Targets {
    /// The focused control's rect; `None` with the focus on nothing.
    from: Option<super::keymap::Rect>,
    /// The shown controls in the move's scope: rect, selector, nth match.
    targets: Vec<(super::keymap::Rect, String, u32)>,
}

/// Collect the controls in the focused control's scope (an open modal
/// dialog, else its `data-keys` area; First and Last its `data-keys-list`),
/// or report the focus on nothing. `window.__pharosTargets` keeps them for
/// the focusing eval.
fn targets_js(first_or_last: bool) -> String {
    format!(
        r#"
const T = {targets};
const mark = (el) => {{
  const q = (v) => v.replace(/\\/g, "\\\\").replace(/'/g, "\\'");
  const href = el.getAttribute("href");
  const sel = href !== null ? `[href='${{q(href)}}']` : el.id ? `[id='${{q(el.id)}}']` : "";
  return [sel, sel ? [...document.querySelectorAll(sel)].indexOf(el) : 0];
}};
const shown = (area) => [...area.querySelectorAll(T)].filter((el) => {{
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0;
}});
const from = document.activeElement && document.activeElement !== document.body ? document.activeElement : null;
let scope = null;
if (from) {{
  scope = ({list} && from.closest("[data-keys-list]")) || from.closest("[aria-modal='true']") || from.closest("[data-keys]");
}}
const els = scope ? shown(scope) : [];
window.__pharosTargets = els;
const rect = (el) => {{ const r = el.getBoundingClientRect(); return [r.left, r.top, r.width, r.height]; }};
return {{
  from: from ? rect(from) : null,
  targets: els.map((el) => {{ const [s, n] = mark(el); return [rect(el), s, Math.max(n, 0)]; }}),
}};
"#,
        targets = js_str(KEY_TARGETS),
        list = first_or_last,
    )
}

/// With the focus on nothing: the control the keys left this page on, when
/// shown, unless a modal dialog is open; else the first control in view in
/// the modal, the page's content, or its key area.
fn arrive_js(left: Option<&(String, u32)>) -> String {
    let (sel, nth) = left.map_or(("", 0), |(s, n)| (s.as_str(), *n));
    format!(
        r#"
const T = {targets};
const visible = (el) => {{ const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; }};
const go = (el) => {{ el.focus(); el.scrollIntoView(false); return true; }};
const modal = document.querySelector("[aria-modal='true']");
const sel = {sel};
if (!modal && sel) {{
  const el = document.querySelectorAll(sel)[{nth}];
  if (el && visible(el)) return go(el);
}}
const inView = (area) => area && [...area.querySelectorAll(T)].find((el) => {{
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0 && r.top + r.height > 0 && r.top < innerHeight;
}});
const first = modal ? inView(modal) : (inView(document.querySelector("[data-keys] main")) || inView(document.querySelector("[data-keys]")));
return first ? go(first) : false;
"#,
        targets = js_str(KEY_TARGETS),
        sel = js_str(sel),
    )
}

async fn move_focus_now(dir: super::keymap::Dir) -> Result<(), String> {
    use super::keymap::Dir;
    let first_or_last = matches!(dir, Dir::First | Dir::Last);
    let found: Targets = document::eval(&targets_js(first_or_last))
        .join()
        .await
        .map_err(|e| e.to_string())?;
    let Some(from) = found.from else {
        let left = LEFT_AT.with(|m| m.borrow().get(&current_path()).cloned());
        document::eval(&arrive_js(left.as_ref()))
            .await
            .map_err(|e| e.to_string())?;
        return Ok(());
    };
    let rects: Vec<_> = found.targets.iter().map(|(r, _, _)| *r).collect();
    let Some(at) = super::keymap::pick(dir, from, &rects) else {
        return Ok(());
    };
    if rects[at] == from {
        return Ok(());
    }
    let (_, sel, nth) = &found.targets[at];
    if !sel.is_empty() {
        let path = current_path();
        LEFT_AT.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() >= 64 && !m.contains_key(&path) {
                m.clear();
            }
            m.insert(path, (sel.clone(), *nth));
        });
    }
    document::eval(&format!(
        "const el = (window.__pharosTargets || [])[{at}]; \
         if (el) {{ el.focus(); el.scrollIntoView(false); }}"
    ))
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---- the server proxy ---------------------------------------------------

/// The request headers passed on to the server.
const PASSED_ON: [&str; 3] = ["range", "accept", "if-none-match"];
/// The response headers passed back to the page.
const PASSED_BACK: [&str; 6] = [
    "content-type",
    "content-range",
    "accept-ranges",
    "cache-control",
    "etag",
    "last-modified",
];

fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .user_agent(tv_page::USER_AGENT)
            .build()
            .unwrap_or_default()
    })
}

/// GET `path_and_query` from the server with the device token in a header.
/// Returns the status, the passed-back headers and the body.
async fn fetch_server(
    path_and_query: &str,
    headers: &[(String, String)],
) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
    let base = server();
    let mut req = http().get(tv_page::server_url(&base, path_and_query));
    if let Some(token) = android::token() {
        req = req.header("X-Emby-Token", token);
    }
    for (name, value) in headers {
        req = req.header(name, value);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let back = PASSED_BACK
        .iter()
        .filter_map(|name| {
            resp.headers()
                .get(*name)
                .and_then(|v| v.to_str().ok())
                .map(|v| ((*name).to_owned(), v.to_owned()))
        })
        .collect();
    let body = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
    Ok((status, back, body))
}

/// Answer the page's requests to the server (`tv_page::proxied_segments`)
/// by fetching them with the token in a header. Call once, in the root
/// component.
pub fn use_server_proxy() {
    for segment in tv_page::proxied_segments() {
        dioxus::mobile::use_asset_handler(segment, move |request, responder| {
            let path = request.uri().path_and_query().map_or_else(
                || request.uri().path().to_owned(),
                |p| p.as_str().to_owned(),
            );
            let headers: Vec<(String, String)> = PASSED_ON
                .iter()
                .filter_map(|name| {
                    request
                        .headers()
                        .get(*name)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| ((*name).to_owned(), v.to_owned()))
                })
                .collect();
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let response = match fetch_server(&path, &headers).await {
                    Ok((status, back, body)) => {
                        let mut builder = dioxus::mobile::wry::http::Response::builder()
                            .status(status)
                            .header("access-control-allow-origin", "*");
                        for (name, value) in back {
                            builder = builder.header(name, value);
                        }
                        log::debug!(
                            "pharos: proxy status={status} bytes={} ms={}",
                            body.len(),
                            started.elapsed().as_millis()
                        );
                        builder.body(body)
                    }
                    Err(e) => {
                        log::warn!("pharos: proxy failed path_len={} error={e}", path.len());
                        dioxus::mobile::wry::http::Response::builder()
                            .status(502)
                            .body(e.into_bytes())
                    }
                };
                match response {
                    Ok(response) => responder.respond(response),
                    Err(e) => log::warn!("pharos: proxy response build failed error={e}"),
                }
            });
        });
    }
}
