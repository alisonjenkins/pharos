//! The Android TV app's platform module (spec 025, ADR-0061): the same
//! calls as [`web`](super), for the shared `App` running natively in Rust
//! with the system WebView only drawing the DOM. It started as a copy of
//! `host.rs`, so a call the browser gains is a compile error here until it
//! has an Android twin; a stub still marked `android: not yet` answers as
//! the host does.
//!
//! How it reaches the page (spec 025, "How the Android UI reads the DOM"):
//! one long-lived script (`BRIDGE_JS`) sends keys, Back (`popstate`, which
//! an `on_back` guard may take before any route hears it) and the
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

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
    // Read once: nothing in the app writes the file, and artwork asks per image.
    static BASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BASE.get_or_init(|| tv_page::server_base(android::load_server().as_deref()))
        .clone()
}

/// Artwork straight from the server, which serves it without the token
/// (keyed tags, ADR-0073). Through the proxy each image held one of the
/// WebView's request threads for seconds, and the bridge's own calls queued
/// behind them: a D-pad press took up to 45 s to answer.
pub fn art_src(src: &str) -> String {
    tv_page::art_src(&server(), src)
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
    if lock(&CAPS).is_none() {
        probe_capabilities().await;
    }
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
            let api = NativeApi::new(&session);
            spawn_forever(keep_media_token(api.clone()));
            Ok(Open {
                api,
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

/// The TV signs in by Quick Connect too.
pub fn offers_quick_connect() -> bool {
    true
}

/// Sign in by Quick Connect: show each code minted, and answer as a
/// password sign-in does once one is approved.
pub async fn quick_connect(mut show_code: impl FnMut(String) + 'static) -> SignInReply {
    let base = server();
    let call = CallInfo::default();
    match android::quick_connect_sign_in(&base, &call, |code| show_code(code.to_owned())).await {
        Ok(signed) => match android::store_token(&signed.token) {
            Ok(()) => {
                log::info!("pharos: sign_in ok via=quick_connect server={base}");
                SignInReply::Status {
                    status: 204,
                    retry_after: None,
                }
            }
            Err(e) => SignInReply::Failed(format!("could not keep the sign-in: {e}")),
        },
        Err(e) => SignInReply::Failed(e),
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
    lock(&PATHS).current().to_owned()
}

pub fn push(path: &str) {
    lock(&PATHS).push(path);
    run(format!(
        "history.pushState('pharos', '', {});",
        js_str(path)
    ));
}

/// A newer server's reload has nothing to reload: the app is the build
/// installed, so the page offers no Reload.
pub fn can_reload() -> bool {
    false
}

/// The TV's remote sets the volume, so the app stays at full and draws no
/// volume control of its own.
pub fn owns_volume() -> bool {
    false
}

pub fn reload() {}

pub fn replace(path: &str) {
    lock(&PATHS).replace(path);
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
    let can = lock(&PATHS).can_go_back();
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

/// A Back guard, removed on drop.
pub struct BackWatch(u64);

impl Drop for BackWatch {
    fn drop(&mut self) {
        unlisten(self.0);
    }
}

/// A listener for the remote's Back at the first page, removed on drop.
pub struct BackAtRoot(u64);

impl Drop for BackAtRoot {
    fn drop(&mut self) {
        unlisten(self.0);
    }
}

/// Ask `on_back` first when the remote's Back arrives: answering `true`
/// takes it, so the page stays where it is and no `popstate` listener hears
/// it. The player's remote scrub uses this to drop its preview in place
/// (B907) instead of leaving the player.
pub fn on_back(on_back: impl FnMut() -> bool + 'static) -> BackWatch {
    BackWatch(listen(Listener::Back(Box::new(on_back))))
}

/// Call `on_back` when the remote's Back is pressed at the first page. It
/// answers whether it used the press (closing the player, say); when no
/// listener does, the app goes to the background, as Android's own Back
/// would have.
/// A listener for media elements' `timeupdate`s, removed on drop.
pub struct TimeUpdates(u64);

impl Drop for TimeUpdates {
    fn drop(&mut self) {
        unlisten(self.0);
    }
}

/// Call `on_tick` with the element's id on each media `timeupdate`, at most
/// once a second per element. The bridge keeps them from Dioxus, whose
/// events each hold the WebView until Rust answers.
pub fn on_time_update(on_tick: impl FnMut(&str) + 'static) -> TimeUpdates {
    TimeUpdates(listen(Listener::Tick(Box::new(on_tick))))
}

pub fn on_back_at_root(on_back: impl FnMut() -> bool + 'static) -> BackAtRoot {
    BackAtRoot(listen(Listener::Root(Box::new(on_back))))
}

/// Send the app to the background (`Activity.moveTaskToBack`), as Back at
/// the first page does in any Android app.
fn leave_app() {
    let left = (|| -> Result<(), String> {
        let ctx = ndk_context::android_context();
        // SAFETY: tao put the process's JavaVM and the activity in the
        // context at start-up, and both live as long as the app.
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| e.to_string())?;
        let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
        let activity = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };
        env.call_method(&activity, "moveTaskToBack", "(Z)Z", &[true.into()])
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    match left {
        Ok(()) => log::info!("pharos: back at root action=leave"),
        Err(e) => log::warn!("pharos: back at root leave failed error={e}"),
    }
}

/// The WebView answers `maybe` for HLS but does not play it well; hls.js
/// over MSE does (spike 0).
pub fn plays_hls() -> bool {
    false
}

pub fn has_mse() -> bool {
    true
}

/// What the WebView decodes, as `open` probed it, with no container the
/// server can play directly (`tv_page::NO_DIRECT_PLAY`).
///
/// Before the probe answers (or if it failed) this is still HLS-only: an
/// empty container list would tell the server any container direct-plays,
/// and the WebView cannot seek a direct stream through the proxy.
pub fn capabilities() -> Capabilities {
    lock(&CAPS).clone().unwrap_or_else(|| Capabilities {
        containers: vec![tv_page::NO_DIRECT_PLAY.to_owned()],
        ..Capabilities::default()
    })
}

/// Ask the WebView once which codecs MSE and `<video>` take, for
/// `capabilities`.
async fn probe_capabilities() {
    let asked = RefCell::new(std::collections::BTreeSet::new());
    let note = |mime: &str| {
        asked.borrow_mut().insert(mime.to_owned());
        false
    };
    let _ = super::playback::capabilities(note, note);
    let note_true = |mime: &str| {
        asked.borrow_mut().insert(mime.to_owned());
        true
    };
    let _ = super::playback::capabilities(note_true, note_true);
    let mimes: Vec<String> = asked.into_inner().into_iter().collect();
    let script = format!(
        "const v = document.createElement('video'); \
         const out = {{}}; \
         for (const m of {}) {{ \
           const mse = !!(window.MediaSource && MediaSource.isTypeSupported(m)); \
           out[m] = [mse, v.canPlayType(m) !== '']; \
         }} \
         return out;",
        serde_json::to_string(&mimes).unwrap_or_else(|_| "[]".into())
    );
    let answers: HashMap<String, (bool, bool)> = match document::eval(&script).join().await {
        Ok(a) => a,
        Err(e) => {
            log::warn!("pharos: capabilities probe failed error={e}");
            return;
        }
    };
    let answer = |mime: &str| answers.get(mime).copied().unwrap_or_default();
    let mut caps = super::playback::capabilities(|m| answer(m).0 || answer(m).1, |m| answer(m).1);
    caps.containers = vec![tv_page::NO_DIRECT_PLAY.to_owned()];
    log::info!(
        "pharos: capabilities video={} audio={} containers={}",
        caps.video_codecs.join(","),
        caps.audio_codecs.join(","),
        caps.containers.join(",")
    );
    *lock(&CAPS) = Some(caps);
}

/// The media element `id` as its last event left it, and how long ago.
fn media(id: &str) -> Option<(tv_page::Media, f64)> {
    lock(&MEDIA)
        .get(id)
        .map(|(media, at)| (media.clone(), now_ms() - at))
}

pub fn position_ms(video_id: &str) -> u64 {
    media(video_id).map_or(0, |(m, ago)| m.position_ms(ago))
}

pub fn ended(video_id: &str) -> bool {
    media(video_id).is_some_and(|(m, _)| m.ended)
}

pub fn has_data(video_id: &str) -> bool {
    media(video_id).is_some_and(|(m, _)| m.has_data())
}

pub fn media_state(video_id: &str) -> MediaState {
    media(video_id).map_or_else(MediaState::default, |(m, ago)| MediaState {
        position_ms: m.position_ms(ago),
        duration_ms: m.duration_ms(),
        paused: m.paused,
        volume: m.vol,
        muted: m.muted,
        loading: super::playback::is_loading(m.seeking, m.ready, m.paused, m.ended),
        video_width: m.w,
        video_height: m.h,
    })
}

/// Run `body` with `v` bound to the media element `id`, when it exists.
fn on_media(id: &str, body: &str) {
    run(format!(
        "const v = document.getElementById({}); if (v) {{ {body} }}",
        js_str(id)
    ));
}

pub fn autoplay_blocked() -> bool {
    false
}

#[allow(clippy::cast_precision_loss)]
pub fn seek_ms(video_id: &str, ms: u64) {
    // Seen at once, before the element's `seeking` comes back.
    if let Some((media, at)) = lock(&MEDIA).get_mut(video_id) {
        media.pos = ms as f64 / 1000.0;
        *at = now_ms();
    }
    on_media(
        video_id,
        &format!("v.currentTime = {};", ms as f64 / 1000.0),
    );
}

/// Play if paused, else pause. A refused `play()` leaves it paused, which
/// the controls then show.
pub fn toggle_play(video_id: &str) {
    on_media(
        video_id,
        "if (v.paused) { v.play().catch((e) => console.warn('pharos: play() refused', e)); } else { v.pause(); }",
    );
}

pub fn play(video_id: &str) {
    on_media(
        video_id,
        "v.play().catch((e) => console.warn('pharos: play() refused', e));",
    );
}

pub fn pause(video_id: &str) {
    on_media(video_id, "v.pause();");
}

pub fn set_rate(video_id: &str, rate: f64) {
    if rate.is_finite() {
        on_media(video_id, &format!("v.playbackRate = {rate};"));
    }
}

pub fn unix_ms() -> i64 {
    wall_clock_ms() as i64
}

pub fn set_volume(video_id: &str, volume: f64, muted: bool) {
    let volume = if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        1.0
    };
    on_media(
        video_id,
        &format!("v.volume = {volume}; v.muted = {muted};"),
    );
}

/// Puts the text track whose `<track id>` is `track_id` in `mode`, and every
/// other track off. The `<track>` may mount just after the effect that asks,
/// so a missing one is looked for again for up to 2 s.
fn set_text_track(video_id: &str, track_id: Option<&str>, mode: &str) {
    run(format!(
        "const vid = {vid}, want = {want}, mode = {mode};
const apply = (left) => {{
  const v = document.getElementById(vid);
  const tracks = v ? [...v.textTracks] : [];
  if (want !== null && !tracks.some((t) => t.id === want)) {{
    if (left > 0) setTimeout(() => apply(left - 1), 100);
    return;
  }}
  for (const t of tracks) t.mode = t.id === want ? mode : 'disabled';
}};
apply(20);",
        vid = js_str(video_id),
        want = track_id.map_or_else(|| "null".to_owned(), js_str),
        mode = js_str(mode),
    ));
}

/// Shows the text track whose `<track id>` is `track_id`, hiding the rest;
/// `None` hides them all. The WebView draws the cues itself.
pub fn show_text_track(video_id: &str, track_id: Option<&str>) {
    set_text_track(video_id, track_id, "showing");
}

pub fn lift_cues_as_shown(_video_id: &str) {}

pub fn relift_cues(_video_id: &str) {}

/// Loads the text track `track_id` without drawing it, every other off.
pub fn hide_text_track(video_id: &str, track_id: &str) {
    set_text_track(video_id, Some(track_id), "hidden");
}

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

/// Resolves once the video knows its picture size, or after 45 s.
pub async fn video_metadata(video_id: &str) {
    for _ in 0..450 {
        if media(video_id).is_some_and(|(m, _)| m.ready >= 1) {
            return;
        }
        sleep_ms(100).await;
    }
}

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
pub fn video_quality(video_id: &str) -> Option<(u64, u64)> {
    media(video_id).map(|(m, _)| (m.dropped, m.frames))
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
    *lock(&VIEWPORT)
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
    Ok(lock(&SESSION).get(key).cloned())
}

pub fn session_set(key: &str, value: &str) -> Result<(), String> {
    lock(&SESSION).insert(key.to_owned(), value.to_owned());
    Ok(())
}

pub fn toggle_fullscreen(_id: &str) {}

pub fn page_origin() -> String {
    String::new()
}

pub async fn copy_text(_text: &str) -> bool {
    false
}

pub fn exit_page_fullscreen() {}

pub fn leave_fullscreen_on_escape() -> bool {
    false
}

pub fn escape_claimed() -> bool {
    false
}

pub fn pip_supported() -> bool {
    false
}

pub fn toggle_pip(_id: &str) {}

/// No WebXR here (spec 022 US3).
pub async fn vr_supported() -> bool {
    false
}

pub fn enter_vr(
    _id: &str,
    _packing: pharos_client::api::Stereo,
    _screen: (f64, f64),
    _on_select: std::rc::Rc<dyn Fn()>,
) {
}

pub fn exit_vr() {}

pub fn select_field(_id: &str) {}

/// Move the focus to `to` when it is inside `within` (B1031): the compact
/// player's picture hands it to the stage bar.
pub fn focus_out_of(within: &str, to: &str) {
    run(format!(
        "const a = document.activeElement; const w = document.getElementById({}); \
         const t = document.getElementById({}); \
         if (a && w && t && w.contains(a)) t.focus({{preventScroll: true}});",
        js_str(within),
        js_str(to)
    ));
}

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

/// Press `key` on the page's player when the focus is outside it (B618).
/// Decided from where the focus was as the key went down; the player's
/// keydown handler then takes the forwarded key, and the bridge hears it
/// again from inside the player, so it is not forwarded twice.
pub fn forward_to_player(key: &str) -> bool {
    if !tv_page::forwards_to_player(key, *lock(&KEY_AT)) {
        return false;
    }
    run(format!(
        "const p = document.querySelector('#pharos-player, .pharos-listen'); \
         if (p) {{ \
           p.focus({{ preventScroll: true }}); \
           const key = {key}; \
           p.dispatchEvent(new KeyboardEvent('keydown', {{ key, bubbles: true, cancelable: true, \
             shiftKey: key !== key.toLowerCase() }})); \
         }}",
        key = js_str(key)
    ));
    true
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
    static START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);
    START.elapsed().as_secs_f64() * 1000.0
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

/// A browse grid's scroll sentinel, watched by an `IntersectionObserver` in
/// the page (B1028); the observer is disconnected on drop.
pub struct SentinelWatch {
    watch: u64,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for SentinelWatch {
    fn drop(&mut self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
        run(tv_page::unwatch_sentinel_js(self.watch));
    }
}

/// Call `on_visible` each time one of the elements with `ids` comes into
/// view, as the browser's observer does, so a grid loads its next page on
/// scroll instead of only from "Show more".
pub fn watch_sentinel(
    ids: &[&str],
    mut on_visible: impl FnMut() + 'static,
) -> Option<SentinelWatch> {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    static NEXT_SENTINEL: AtomicU64 = AtomicU64::new(0);
    let watch = NEXT_SENTINEL.fetch_add(1, Ordering::Relaxed);
    let script = tv_page::watch_sentinel_js(watch, ids);
    let stopped = std::sync::Arc::new(AtomicBool::new(false));
    let live = stopped.clone();
    spawn_detached(async move {
        let mut seen = document::eval(&script);
        let mut fired = 0u32;
        loop {
            match seen.recv::<bool>().await {
                Ok(true) => {
                    if live.load(Ordering::Relaxed) {
                        continue;
                    }
                    fired = fired.saturating_add(1);
                    log::debug!("pharos: sentinel seen watch={watch} fired={fired}");
                    on_visible();
                }
                Ok(false) => break,
                Err(e) => {
                    log::warn!(
                        "pharos: sentinel watch failed watch={watch} fired={fired} error={e}"
                    );
                    return;
                }
            }
        }
        log::debug!("pharos: sentinel watch ended watch={watch} fired={fired}");
    });
    Some(SentinelWatch { watch, stopped })
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

/// Scrolls the browse tile at `at` to the top of the page and focuses it
/// (B1028). The page answers later, so this says `true`: the grid asks only
/// for a tile it has rendered, and a miss is logged here instead.
pub fn scroll_to_tile(at: u32) -> bool {
    let script = tv_page::scroll_to_tile_js(at);
    spawn_detached(async move {
        match document::eval(&script).join::<bool>().await {
            Ok(true) => log::debug!("pharos: grid jump tile={at} placed=true"),
            Ok(false) => log::warn!("pharos: grid jump tile={at} placed=false reason=not_in_page"),
            Err(e) => log::warn!("pharos: grid jump tile={at} placed=false error={e}"),
        }
    });
    true
}

pub fn scroll_to_top() {
    run(tv_page::SCROLL_TO_TOP_JS.to_owned());
}

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

/// hls.js for every HLS stream; the Rust engine (spec 031) is the web
/// build's, so it hands over to hls.js here. Video never direct-plays
/// (`tv_page::NO_DIRECT_PLAY`), but a song's stream is a plain file, so a
/// direct stream plays from the server with the media token (B1029, V988).
pub async fn engine(kind: Engine) -> Result<Box<dyn PlaybackEngine>, String> {
    match kind {
        Engine::Direct => {
            // `DirectMedia` implements `Drop`, so struct-update syntax
            // cannot move the rest out of a default value (E0509).
            let mut engine = DirectMedia::default();
            engine.token = wait_for_media_token().await?;
            Ok(Box::new(engine))
        }
        Engine::HlsJs | Engine::NativeHls | Engine::RustHls => {
            if kind != Engine::HlsJs {
                log::info!(
                    "pharos: engine picked={} using=hlsjs reason=android",
                    super::playback::engine_label(kind)
                );
            }
            load_hlsjs().await?;
            Ok(Box::new(HlsJs::default()))
        }
        Engine::Unsupported => Err("this TV cannot play HLS".into()),
    }
}

/// hls.js, shipped inside the app. Fetched through the proxy it waited on
/// the server's first answer, and on a slow one (over `PROXY_DEADLINE`) the
/// player was left with no engine and no video.
const HLSJS: &str = include_str!("../../assets/vendor/hls.min.js");

/// Run the bundled hls.js in the page once.
async fn load_hlsjs() -> Result<(), String> {
    let script = format!(
        "if (window.Hls) return true; \
         const s = document.createElement('script'); \
         s.textContent = {}; \
         document.head.append(s); \
         return !!window.Hls;",
        js_str(HLSJS)
    );
    let loaded = match document::eval(&script).join::<bool>().await {
        Ok(true) => Ok(()),
        Ok(false) => Err("hls.js ran but defined no Hls".into()),
        Err(e) => Err(format!("hls.js: {e}")),
    };
    if let Err(e) = &loaded {
        log::warn!("pharos: hls.js load failed error={e}");
    }
    loaded
}

/// An hls.js player in the page, at `window.__pharosHls[id]`. Its errors and
/// fragment loads come back through the bridge; the recovery is chosen here,
/// as on the web. Dropping it destroys the player.
#[derive(Default)]
struct HlsJs {
    video_id: String,
    listener: Option<u64>,
    /// Which attach made the page's player, so a late `Drop` eval cannot
    /// destroy the one a later attach put at the same id.
    generation: u64,
}

/// Attach generations, shared by hls.js and direct players.
static NEXT_HLS: AtomicU64 = AtomicU64::new(1);

impl PlaybackEngine for HlsJs {
    fn attach(
        &mut self,
        video_id: &str,
        url: &str,
        start_ms: u64,
        on_fatal: super::playback::OnFatal,
        on_fragment: super::playback::OnFragment,
    ) -> Result<(), String> {
        let id = video_id.to_owned();
        let budget = RefCell::new(super::playback::RecoveryBudget::new());
        let log_url = url.split('?').next().unwrap_or_default().to_owned();
        let on_event = {
            let id = id.clone();
            move |event: tv_page::HlsEvent| match event {
                tv_page::HlsEvent::Frag {
                    bytes,
                    load_ms,
                    media_ms,
                } => {
                    budget.borrow_mut().progressed();
                    if bytes > 0 && load_ms > 0 && media_ms > 0 {
                        on_fragment(super::playback::FragmentLoad {
                            bytes,
                            load_ms,
                            media_ms,
                        });
                    }
                }
                tv_page::HlsEvent::Error {
                    kind,
                    details,
                    fatal: false,
                } => {
                    log::warn!(
                        "pharos: playback error engine=hlsjs type={kind} details={details} fatal=false url={log_url}"
                    );
                }
                tv_page::HlsEvent::Error { kind, details, .. } => {
                    let mut budget = budget.borrow_mut();
                    let action = budget.on_fatal(&kind, &details);
                    log::warn!(
                        "pharos: playback error engine=hlsjs type={kind} details={details} fatal=true action={} attempt={} url={log_url}",
                        action.label(),
                        budget.attempts(&kind)
                    );
                    let p = format!("window.__pharosHls && window.__pharosHls[{}]", js_str(&id));
                    match action {
                        super::playback::Recovery::StartLoad => {
                            run(format!("const p = {p}; if (p) p.startLoad();"));
                        }
                        super::playback::Recovery::RecoverMedia => {
                            run(format!("const p = {p}; if (p) p.recoverMediaError();"));
                        }
                        super::playback::Recovery::Fail => {
                            on_fatal(format!("the stream failed ({kind}: {details})"));
                        }
                    }
                }
            }
        };
        if let Some(old) = self.listener.take() {
            unlisten(old);
        }
        self.listener = Some(listen(Listener::Hls(id.clone(), Box::new(on_event))));
        self.video_id = id.clone();
        self.generation = NEXT_HLS.fetch_add(1, Ordering::Relaxed);
        // The previous source's state must not answer for this one:
        // `video_metadata` would resolve on its `ready` at once.
        lock(&MEDIA).remove(&id);
        let start = super::playback::hls_start_position(start_ms);
        run(format!(
            r#"
const id = {id};
const v = document.getElementById(id);
if (!v) throw new Error("no <video id=" + id + ">");
window.__pharosHls = window.__pharosHls || {{}};
if (window.__pharosHls[id]) window.__pharosHls[id].destroy();
const token = {token};
// The G08 has 2.4 GB and swaps: keep 10 s behind the playhead (the
// default keeps all it can, ~70 s seen) and at most 60 s or 40 MB ahead.
const h = new Hls({{
  startPosition: {start},
  backBufferLength: 10,
  maxBufferLength: 30,
  maxMaxBufferLength: 60,
  maxBufferSize: 40 * 1000 * 1000,
  xhrSetup: (xhr) => {{ if (token) xhr.setRequestHeader('X-Emby-Token', token); }},
}});
h.__pharosGen = {generation};
window.__pharosHls[id] = h;
const send = (m) => window.__pharosSend && window.__pharosSend(Object.assign({{ t: "hls", id }}, m));
h.on(Hls.Events.ERROR, (_, d) => send({{ ev: "error", kind: String(d.type), details: String(d.details), fatal: !!d.fatal }}));
h.on(Hls.Events.FRAG_LOADED, (_, d) => {{
  if (!d.frag || d.frag.type !== "main") return;
  const st = d.frag.stats || d.stats || {{}};
  const l = st.loading || {{}};
  const from = l.first > 0 ? l.first : l.start;
  send({{ ev: "frag", bytes: Math.max(0, Math.round(st.loaded || 0)),
         load_ms: Math.max(0, Math.round((l.end || 0) - (from || 0))),
         media_ms: Math.max(0, Math.round((d.frag.duration || 0) * 1000)) }});
}});
v.setAttribute({mark}, "hlsjs");
h.loadSource({url});
h.attachMedia(v);
"#,
            id = js_str(&id),
            mark = js_str(super::resume::ENGINE_MARK),
            url = js_str(&direct_url(&server(), url)),
            token = android::token().map_or_else(|| "null".to_owned(), |t| js_str(&t)),
            generation = self.generation,
        ));
        Ok(())
    }
}

/// The Media-scoped token (ADR-0058) a directly played element's `src`
/// carries, kept fresh by [`keep_media_token`] for the open session.
static MEDIA_TOKEN: Mutex<Option<String>> = Mutex::new(None);

/// Which `open` the running [`keep_media_token`] loop belongs to: a reconnect
/// starts a new loop, and the old one, whose session is gone, stops.
static MEDIA_TOKEN_SESSION: AtomicU64 = AtomicU64::new(0);

/// Ask `api` for a media token now and again before each one expires (B1029, V988).
async fn keep_media_token(api: NativeApi) {
    let session = MEDIA_TOKEN_SESSION.fetch_add(1, Ordering::AcqRel) + 1;
    let call = CallInfo::default();
    while MEDIA_TOKEN_SESSION.load(Ordering::Acquire) == session {
        let wait_ms = match api.media_token(&call).await {
            Ok(t) => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let now = wall_clock_ms() as u64;
                let wait = tv_page::media_token_wait_ms(t.expires_at_ms, now);
                log::info!(
                    "pharos: media token outcome=ok expires_in_s={} refresh_in_s={}",
                    t.expires_at_ms.saturating_sub(now) / 1000,
                    wait / 1000
                );
                *lock(&MEDIA_TOKEN) = Some(t.token);
                wait
            }
            Err(e) => {
                log::warn!(
                    "pharos: media token outcome=failed error={e} retry_in_s={}",
                    tv_page::MEDIA_TOKEN_MIN_WAIT_MS / 1000
                );
                tv_page::MEDIA_TOKEN_MIN_WAIT_MS
            }
        };
        // Slept in steps, so a loop a reconnect replaced stops within one.
        let mut left = wait_ms;
        while left > 0 && MEDIA_TOKEN_SESSION.load(Ordering::Acquire) == session {
            let step = left.min(tv_page::MEDIA_TOKEN_MIN_WAIT_MS);
            #[allow(clippy::cast_possible_truncation)]
            sleep_ms(step as u32).await;
            left -= step;
        }
    }
}

/// How long a direct play waits for the first media token after sign-in.
const MEDIA_TOKEN_WAIT_MS: u32 = 5_000;
const MEDIA_TOKEN_POLL_MS: u32 = 100;

/// The current media token, waiting up to [`MEDIA_TOKEN_WAIT_MS`] for the
/// first one.
async fn wait_for_media_token() -> Result<String, String> {
    let mut waited = 0;
    loop {
        if let Some(token) = lock(&MEDIA_TOKEN).clone() {
            return Ok(token);
        }
        if waited >= MEDIA_TOKEN_WAIT_MS {
            log::warn!(
                "pharos: engine picked=direct outcome=failed reason=no_media_token waited_ms={waited}"
            );
            return Err("the server gave this TV no media token to play with".into());
        }
        sleep_ms(MEDIA_TOKEN_POLL_MS).await;
        waited += MEDIA_TOKEN_POLL_MS;
    }
}

/// A song (or any direct stream) on its media element, loaded straight from
/// the server with the media token in its `src` (B1029, V988): the element
/// cannot send the device token, and through the page proxy it could not
/// seek. The element's own `error` comes back through the bridge.
#[derive(Default)]
struct DirectMedia {
    token: String,
    media_id: String,
    listener: Option<u64>,
    /// Which attach set the element's `src`, so a late `Drop` cannot clear
    /// the song a later attach put on the same element.
    generation: u64,
}

impl PlaybackEngine for DirectMedia {
    fn attach(
        &mut self,
        media_id: &str,
        url: &str,
        start_ms: u64,
        on_fatal: super::playback::OnFatal,
        _on_fragment: super::playback::OnFragment,
    ) -> Result<(), String> {
        let id = media_id.to_owned();
        let log_url = url.split('?').next().unwrap_or_default().to_owned();
        let on_error = {
            let log_url = log_url.clone();
            move |code: u16, message: String| {
                log::warn!(
                    "pharos: playback error engine=direct code={code} message={message} action=fail url={log_url}"
                );
                on_fatal(format!(
                    "the TV could not play this stream (code {code}) {message}"
                ));
            }
        };
        if let Some(old) = self.listener.take() {
            unlisten(old);
        }
        self.listener = Some(listen(Listener::MediaError(id.clone(), Box::new(on_error))));
        self.media_id = id.clone();
        self.generation = NEXT_HLS.fetch_add(1, Ordering::Relaxed);
        lock(&MEDIA).remove(&id);
        log::info!(
            "pharos: playback attach engine=direct id={id} start_ms={start_ms} url={log_url}"
        );
        #[allow(clippy::cast_precision_loss)]
        let start_s = start_ms as f64 / 1000.0;
        run(format!(
            r#"
const id = {id};
const v = document.getElementById(id);
if (!v) throw new Error("no media element id=" + id);
if (v.__pharosOnError) v.removeEventListener("error", v.__pharosOnError);
v.__pharosOnError = () => {{
  if (!v.getAttribute("src")) return;
  const e = v.error;
  window.__pharosSend && window.__pharosSend({{ t: "mediaerror", id,
    code: e ? e.code : 0, message: e ? String(e.message || "") : "" }});
}};
v.addEventListener("error", v.__pharosOnError);
const start = {start_s};
if (start > 0) v.addEventListener("loadedmetadata", () => {{ v.currentTime = start; }}, {{ once: true }});
v.setAttribute({mark}, "direct");
v.__pharosGen = {generation};
// Fetched with CORS (the server allows any origin), so the page's
// visualiser can analyse the sound (B1032).
v.crossOrigin = "anonymous";
v.src = {url};
"#,
            id = js_str(&id),
            mark = js_str(super::resume::ENGINE_MARK),
            url = js_str(&tv_page::direct_media_url(&server(), url, &self.token)),
            generation = self.generation,
        ));
        Ok(())
    }
}

impl Drop for DirectMedia {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.take() {
            unlisten(listener);
        }
        if self.media_id.is_empty() {
            return;
        }
        run(format!(
            "const v = document.getElementById({}); \
             if (v && v.__pharosGen === {}) {{ \
             if (v.__pharosOnError) v.removeEventListener('error', v.__pharosOnError); \
             v.pause(); v.removeAttribute('src'); v.load(); }}",
            js_str(&self.media_id),
            self.generation
        ));
    }
}

/// The server's own address for a stream path. hls.js loads the playlists
/// and segments straight from the server, with the token in a header (the
/// server allows the app's origin): through the page proxy each segment
/// held a WebView request thread, a JNI copy of the body and the 8 s proxy
/// deadline.
fn direct_url(base: &str, url: &str) -> String {
    if url.starts_with('/') {
        tv_page::server_url(base, url)
    } else {
        url.to_owned()
    }
}

impl Drop for HlsJs {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.take() {
            unlisten(listener);
        }
        if self.video_id.is_empty() {
            return;
        }
        run(format!(
            "const id = {}; const h = window.__pharosHls && window.__pharosHls[id]; \
             if (h && h.__pharosGen === {}) {{ h.destroy(); delete window.__pharosHls[id]; }}",
            js_str(&self.video_id),
            self.generation
        ));
        // MEDIA is left alone: a newer engine may already own this id, and
        // its attach cleared the entry for itself.
    }
}

pub fn warn(message: &str) {
    log::warn!("{message}");
}

pub fn info(message: &str) {
    log::info!("{message}");
}

/// No server to submit to on the host: client events are dropped.
pub fn install_telemetry(_api: &NativeApi) {}

pub fn report_event(event: ClientEvent) {
    super::report::record_event(&event);
}

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

/// The visualiser module: this crate's `pharos-tv-visualiser` bin, built to
/// wasm with its wasm-bindgen glue by `scripts/tv-visualiser.sh`.
const VISUALISER_JS: &str = include_str!("../../assets/generated/tv-visualiser.js");
const VISUALISER_WASM_B64: &str = include_str!("../../assets/generated/tv-visualiser.wasm.b64");

static NEXT_VISUALISER: AtomicU64 = AtomicU64::new(1);

/// The music visualiser for `style`, drawn by the page (B1032, V992): this
/// app's Rust runs outside the WebView, where the canvas and the sound are.
/// `on_fallback` hears why it cannot draw; dropping it stops it.
pub struct PageVisualiser {
    id: u64,
    listener: u64,
}

pub fn page_visualiser(style: &str, on_fallback: impl FnMut(String) + 'static) -> PageVisualiser {
    let id = NEXT_VISUALISER.fetch_add(1, Ordering::Relaxed);
    let listener = listen(Listener::Visualiser(id, Box::new(on_fallback)));
    let style = style.to_owned();
    log::info!("pharos: visualiser page run={id} style={style} action=start");
    spawn_detached(async move {
        let started = match load_visualiser().await {
            Ok(()) => document::eval(&tv_page::visualiser_start_js(id, &style))
                .join::<bool>()
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        match started {
            Ok(true) => log::info!("pharos: visualiser page run={id} outcome=started"),
            Ok(false) => log::info!("pharos: visualiser page run={id} outcome=stopped_first"),
            Err(e) => {
                log::warn!("pharos: visualiser page run={id} outcome=failed error={e}");
                on_bridge(Bridge::Visualiser {
                    id,
                    reason: crate::render::Fallback::NoWebgl2.label().to_owned(),
                });
            }
        }
    });
    PageVisualiser { id, listener }
}

impl Drop for PageVisualiser {
    fn drop(&mut self) {
        unlisten(self.listener);
        log::info!("pharos: visualiser page run={} action=stop", self.id);
        run(tv_page::visualiser_stop_js(self.id));
    }
}

/// Load the visualiser module into the page once; concurrent callers share
/// the one load.
async fn load_visualiser() -> Result<(), String> {
    let script = format!(
        r#"if (!window.__pharosVisLoad) {{
  window.__pharosVisLoad = (async () => {{
    const s = document.createElement("script");
    s.textContent = {glue} + "
window.__pharosVisInit = wasm_bindgen;";
    document.head.append(s);
    const b = atob({wasm});
    const bytes = new Uint8Array(b.length);
    for (let i = 0; i < b.length; i++) bytes[i] = b.charCodeAt(i);
    await window.__pharosVisInit({{ module_or_path: bytes }});
    window.__pharosVis = window.__pharosVisInit;
    return true;
  }})();
  window.__pharosVisLoad.catch(() => {{ window.__pharosVisLoad = null; }});
}}
return await window.__pharosVisLoad;"#,
        glue = js_str(VISUALISER_JS),
        wasm = js_str(VISUALISER_WASM_B64.trim()),
    );
    let at = now_ms();
    match document::eval(&script).join::<bool>().await {
        Ok(true) => {
            log::info!(
                "pharos: visualiser module outcome=loaded wasm_b64_bytes={} took_ms={:.0}",
                VISUALISER_WASM_B64.len(),
                now_ms() - at
            );
            Ok(())
        }
        Ok(false) => Err("the visualiser module loaded nothing".into()),
        Err(e) => {
            log::warn!("pharos: visualiser module outcome=failed error={e}");
            Err(format!("visualiser module: {e}"))
        }
    }
}

pub fn media_playing(id: &str) -> bool {
    media(id).is_some_and(|(m, _)| m.src && !m.paused)
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

// Process-wide, not thread-local: on Android, Dioxus runs tasks and event
// handlers on more than one thread (seen on the TV), so a thread-local
// seen from a handler is not the one the bridge wrote.
static PATHS: LazyLock<Mutex<Paths>> = LazyLock::new(|| Mutex::new(Paths::new()));
/// The CSS viewport; a TV's WebView is 960 x 540 (spike 0) until the
/// bridge says otherwise.
static VIEWPORT: Mutex<(f64, f64)> = Mutex::new((960.0, 540.0));
static SESSION: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Mutex::default);
/// A registered listener, called outside the registry's lock.
type SharedListener = Arc<Mutex<Owned<Listener>>>;
static LISTENERS: Mutex<Vec<(u64, SharedListener)>> = Mutex::new(Vec::new());
static NEXT_LISTENER: AtomicU64 = AtomicU64::new(0);
static BRIDGED: AtomicBool = AtomicBool::new(false);
/// Per page path, where the arrow keys last left the focus: a selector
/// and which match it was (B712's rule, as on the web).
static LEFT_AT: LazyLock<Mutex<HashMap<String, (String, u32)>>> = LazyLock::new(Mutex::default);
/// Each media element's last pushed state and when it came (`now_ms`).
static MEDIA: LazyLock<Mutex<HashMap<String, (tv_page::Media, f64)>>> =
    LazyLock::new(Mutex::default);
/// Where the focus was as the key being handled went down.
static KEY_AT: Mutex<tv_page::KeyContext> = Mutex::new(tv_page::KeyContext {
    prevented: false,
    player: false,
    in_player: false,
    on_button: false,
});
/// What `probe_capabilities` found.
static CAPS: Mutex<Option<Capabilities>> = Mutex::new(None);

/// A poisoned lock still holds usable state: a panicking handler leaves
/// these maps whole.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A listener's closure holds the UI's `Rc`s, so it is not `Send`. It is
/// only made, called and dropped from Dioxus's own tasks and handlers,
/// which Dioxus runs one at a time even when on different threads; the
/// mutex around it orders those calls.
struct Owned<T>(T);

// SAFETY: see `Owned`: never touched by two threads at once, since every
// access is under its mutex from code Dioxus runs one at a time.
unsafe impl<T> Send for Owned<T> {}

/// The controls the arrow keys move between: the web's, and the player's
/// track pickers too, which a remote has no other way to reach.
const KEY_TARGETS: &str =
    "a[href], button:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex='-1'])";

/// An app-wide key listener: key, ctrl, meta; true takes the key.
type AppKeys = Box<dyn FnMut(&str, bool, bool) -> bool>;

enum Listener {
    App(AppKeys),
    Mode(Box<dyn FnMut(&str) -> bool>),
    Escape(Box<dyn FnMut()>),
    Pop(Box<dyn FnMut(String)>),
    /// Asked first on Back; `true` keeps the page where it is.
    Back(Box<dyn FnMut() -> bool>),
    /// The remote's Back at the first page; true when it used the press.
    Root(Box<dyn FnMut() -> bool>),
    /// hls.js events for the player on one media element.
    Hls(String, Box<dyn FnMut(tv_page::HlsEvent)>),
    /// A media element's `timeupdate`, with its id.
    Tick(Box<dyn FnMut(&str)>),
    /// A directly played media element's `error`, for the element `id`.
    MediaError(String, Box<dyn FnMut(u16, String)>),
    /// Page visualiser run `id` could not draw: the fallback's label.
    Visualiser(u64, Box<dyn FnMut(String)>),
}

fn listen(listener: Listener) -> u64 {
    install_bridge();
    let id = NEXT_LISTENER
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    lock(&LISTENERS).push((id, Arc::new(Mutex::new(Owned(listener)))));
    id
}

fn unlisten(id: u64) {
    lock(&LISTENERS).retain(|(at, _)| *at != id);
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
// Not a <select>: OK opens its list, and the arrows have to move off it.
const inText = (t) => !!t && (t.isContentEditable || /^(INPUT|TEXTAREA)$/.test(t.tagName));
// Space too: the player's mode key toggles play, and the WebView would
// click the focused button as well (it cannot wait for Rust's answer).
const takes = new Set(["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", " "]);
// OK that commits a remote scrub (B907) must not also click the focused
// button, on its first press or while it is held.
// A left or right in the player starts or moves a scrub before its class
// has rendered, so a press inside the settle window (900 ms) counts too.
// The keys the player binds to a 10 s seek (keymap.rs), each of which
// starts a scrub.
const seekKeys = new Set(["ArrowLeft", "ArrowRight", "h", "l"]);
let okCommits = false;
let lastSeekKey = -1e9;
addEventListener("keyup", (e) => { if (e.key === "Enter") okCommits = false; });
const players = '#pharos-player, .pharos-listen';
// A remote has no Space: OK with the focus on the player itself (or on
// nothing) and the media keys play and pause, as Space does. Swapped in
// the capture phase, before any handler hears the remote's key.
addEventListener("keydown", (e) => {
  if (e.repeat || !e.isTrusted) return;
  const player = document.querySelector(players);
  if (!player) return;
  const at = document.activeElement;
  const onPlayer = !at || at === document.body || at === player;
  const v = player.querySelector("video, audio");
  const paused = !v || v.paused;
  const scrubbing = !!document.querySelector(".pharos-seek.is-scrubbing");
  const toggle = (e.key === "Enter" && onPlayer && !scrubbing) || e.key === "MediaPlayPause" ||
    (e.key === "MediaPlay" && paused) || (e.key === "MediaPause" && !paused);
  if (!toggle) return;
  e.preventDefault();
  e.stopImmediatePropagation();
  player.focus({ preventScroll: true });
  player.dispatchEvent(new KeyboardEvent("keydown", { key: " ", bubbles: true, cancelable: true }));
}, true);
// The remote in the video player: Up and Down move between the picture and
// its controls (the remote has its own volume keys), and Left and Right move
// between controls once one has the focus; on the picture itself Left and
// Right still seek. Taken here, before the player's handler hears them.
// Not the volume slider: it would keep Left and Right for itself, and the
// remote has volume keys.
const controlsIn = (player) => [...player.querySelectorAll(
  "button:not([disabled]), select:not([disabled]), a[href]"
)].filter((el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; });
const nearest = (from, els, key) => {
  const f = from.getBoundingClientRect();
  const fx = f.left + f.width / 2, fy = f.top + f.height / 2;
  let best = null, score = Infinity;
  for (const el of els) {
    if (el === from) continue;
    const r = el.getBoundingClientRect();
    const cx = r.left + r.width / 2, cy = r.top + r.height / 2;
    const sameRow = r.top < f.bottom - 0.5 && r.bottom > f.top + 0.5;
    let s = null;
    if (key === "ArrowDown" && r.top >= f.bottom - 0.5) s = (r.top - f.bottom) * 4 + Math.abs(cx - fx);
    if (key === "ArrowUp" && r.bottom <= f.top + 0.5) s = (f.top - r.bottom) * 4 + Math.abs(cx - fx);
    if (key === "ArrowRight" && sameRow && r.left >= f.right - 0.5) s = r.left - f.right + Math.abs(cy - fy);
    if (key === "ArrowLeft" && sameRow && r.right <= f.left + 0.5) s = f.left - r.right + Math.abs(cy - fy);
    if (s !== null && s < score) { score = s; best = el; }
  }
  return best;
};
addEventListener("keydown", (e) => {
  if (!e.isTrusted || !e.key.startsWith("Arrow")) return;
  const player = document.querySelector('#pharos-player');
  const at = document.activeElement;
  if (!player || !at || !player.contains(at)) return;
  const vertical = e.key === "ArrowUp" || e.key === "ArrowDown";
  const onPicture = at === player;
  if (!vertical && onPicture) return;
  e.preventDefault();
  e.stopPropagation();
  // Any key shows the controls; this one does nothing else.
  player.dispatchEvent(new KeyboardEvent("keydown", { key: "Unidentified", bubbles: true }));
  const els = controlsIn(player);
  let to = null;
  if (onPicture) {
    to = player.querySelector(".pharos-control-row button") || els[0] || null;
  } else {
    to = nearest(at, els, e.key);
    if (!to && e.key === "ArrowUp") to = player;
  }
  if (to) to.focus({ preventScroll: true });
}, true);
addEventListener("keydown", (e) => {
  const text = inText(e.target);
  const prevented = e.defaultPrevented;
  if (!text && !e.ctrlKey && !e.metaKey && !document.querySelector('.pharos-invite') && seekKeys.has(e.key) && e.target.closest && e.target.closest('#pharos-player')) lastSeekKey = performance.now();
  if (e.key === "Enter" && !e.repeat) okCommits = !!document.querySelector(".pharos-seek.is-scrubbing") || performance.now() - lastSeekKey < 900;
  if (e.key === "Enter" && okCommits) e.preventDefault();
  // A held arrow repeats (the player's scrub speeds up on it): the WebView
  // must not move the focus on the repeats either. Rust hears the first.
  if (!text && takes.has(e.key)) e.preventDefault();
  if (e.repeat) return;
  const player = document.querySelector(players);
  const at = document.activeElement;
  send({
    t: "key", key: e.key, ctrl: e.ctrlKey, meta: e.metaKey, text, prevented,
    player: !!player, in_player: !!(player && at && player.contains(at)),
    on_button: !!(at && at.matches && at.matches("a[href], button")),
  });
});
// The WebView takes the remote's Back only while it has a page to go back
// to; at the first one Android leaves the app, over an open player too. A
// guard entry under the first page keeps one there, and Rust decides what
// Back at the first page does.
if (!window.__pharosGuard) {
  window.__pharosGuard = true;
  history.replaceState("pharos-guard", "", location.href);
  history.pushState("pharos", "", location.href);
}
addEventListener("popstate", (e) => {
  if (e.state === "pharos-guard") {
    history.pushState("pharos", "", location.href);
    send({ t: "root" });
    return;
  }
  send({ t: "pop", path: location.pathname + location.search });
});
window.__pharosSend = send;
// Media events do not bubble, but a capturing listener on the document
// hears every media element's. `timeupdate` stops here: Dioxus would hold
// the page for each of its four a second, so Rust hears it as a tick: once
// a second for a video, four times for a song, whose karaoke wipe and line
// changes need them (B1030).
const ticked = {};
const media = (e) => {
  const v = e.target;
  if (!(v instanceof HTMLMediaElement) || !v.id) return;
  let tick = false;
  if (e.type === 'timeupdate') {
    e.stopPropagation();
    const now = performance.now();
    const every = v instanceof HTMLVideoElement ? __VIDEO_TICK_MS__ : __AUDIO_TICK_MS__;
    if (now - (ticked[v.id] || 0) < every) return;
    ticked[v.id] = now;
    tick = true;
  }
  const q = v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality() : null;
  send({
    t: "media", id: v.id, tick, pos: v.currentTime,
    dur: Number.isFinite(v.duration) ? v.duration : null,
    paused: v.paused, ended: v.ended, seeking: v.seeking, ready: v.readyState,
    rate: v.playbackRate, vol: v.volume, muted: v.muted,
    w: v.videoWidth || 0, h: v.videoHeight || 0, src: !!v.currentSrc,
    dropped: q ? q.droppedVideoFrames : 0, frames: q ? q.totalVideoFrames : 0,
  });
};
for (const name of ["timeupdate", "play", "playing", "pause", "waiting", "canplay",
  "loadedmetadata", "durationchange", "seeking", "seeked", "ended", "volumechange",
  "ratechange", "emptied"]) {
  document.addEventListener(name, media, true);
}
const size = () => send({ t: "size", w: innerWidth, h: innerHeight });
addEventListener("resize", size);
size();
await new Promise(() => {});
"#;

/// Start the bridge once: the page's history starts on the app's first
/// page, and its keys and Back come into Rust from then on.
fn install_bridge() {
    if BRIDGED.swap(true, Ordering::AcqRel) {
        return;
    }
    let start = current_path();
    spawn_forever(async move {
        let _ = document::eval(&format!(
            "history.replaceState('pharos', '', {});",
            js_str(&start)
        ))
        .await;
        let mut bridge = document::eval(&tv_page::bridge_script(BRIDGE_JS));
        log::info!(
            "pharos: bridge installed video_tick_ms={} audio_tick_ms={}",
            tv_page::VIDEO_TICK_MS,
            tv_page::AUDIO_TICK_MS
        );
        loop {
            match bridge.recv::<serde_json::Value>().await {
                Ok(value) => match tv_page::parse_bridge(&value) {
                    Some(message) => on_bridge(message),
                    None => log::warn!("pharos: bridge message unknown value={value}"),
                },
                Err(e) => {
                    log::warn!("pharos: bridge ended error={e}");
                    BRIDGED.store(false, Ordering::Release);
                    return;
                }
            }
        }
    });
}

fn on_bridge(message: Bridge) {
    let listeners: Vec<_> = lock(&LISTENERS)
        .iter()
        .map(|(_, f)| Arc::clone(f))
        .collect();
    match message {
        Bridge::Key {
            key,
            ctrl,
            meta,
            text,
            at,
        } => {
            *lock(&KEY_AT) = at;
            let mut taken = false;
            for f in &listeners {
                if let Ok(mut f) = f.try_lock() {
                    if let Listener::App(f) = &mut f.0 {
                        taken |= f(&key, ctrl, meta);
                    }
                }
            }
            if taken {
                return;
            }
            if !(text || ctrl || meta || at.prevented) {
                for f in &listeners {
                    if let Ok(mut f) = f.try_lock() {
                        if let Listener::Mode(f) = &mut f.0 {
                            taken |= f(&key);
                        }
                    }
                }
            }
            // As on the web, Escape reaches these from a text field too.
            if taken || at.prevented || key != "Escape" {
                return;
            }
            for f in &listeners {
                if let Ok(mut f) = f.try_lock() {
                    if let Listener::Escape(f) = &mut f.0 {
                        f();
                    }
                }
            }
        }
        Bridge::Root => {
            let mut used = false;
            for f in &listeners {
                if let Ok(mut f) = f.try_lock() {
                    if let Listener::Root(f) = &mut f.0 {
                        used |= f();
                    }
                }
            }
            if !used {
                leave_app();
            }
        }
        Bridge::Pop { path } => {
            let mut kept = false;
            for f in &listeners {
                if let Ok(mut f) = f.try_lock() {
                    if let Listener::Back(f) = &mut f.0 {
                        kept |= f();
                    }
                }
            }
            if kept {
                // The WebView has already gone back an entry: put the page
                // that took Back back on top, so the next Back leaves it.
                let here = current_path();
                log::info!("pharos: back taken by the page path={here} popped_to={path}");
                run(format!(
                    "history.pushState('pharos', '', {});",
                    js_str(&here)
                ));
                return;
            }
            lock(&PATHS).popped_to(&path);
            for f in &listeners {
                if let Ok(mut f) = f.try_lock() {
                    if let Listener::Pop(f) = &mut f.0 {
                        f(path.clone());
                    }
                }
            }
        }
        Bridge::Size { w, h } => *lock(&VIEWPORT) = (w, h),
        Bridge::Media(media) => {
            let at = now_ms();
            let (id, tick) = (media.id.clone(), media.tick);
            lock(&MEDIA).insert(id.clone(), (media, at));
            if tick {
                for f in &listeners {
                    if let Ok(mut f) = f.try_lock() {
                        if let Listener::Tick(f) = &mut f.0 {
                            f(&id);
                        }
                    }
                }
            }
        }
        Bridge::Hls { id, event } => {
            for f in &listeners {
                if let Ok(mut f) = f.try_lock() {
                    if let Listener::Hls(on, f) = &mut f.0 {
                        if *on == id {
                            f(event.clone());
                        }
                    }
                }
            }
        }
        Bridge::Visualiser { id, reason } => {
            log::info!("pharos: visualiser page run={id} outcome=fallback reason={reason}");
            for f in &listeners {
                if let Ok(mut f) = f.try_lock() {
                    if let Listener::Visualiser(on, f) = &mut f.0 {
                        if *on == id {
                            f(reason.clone());
                        }
                    }
                }
            }
        }
        Bridge::MediaError { id, code, message } => {
            for f in &listeners {
                if let Ok(mut f) = f.try_lock() {
                    if let Listener::MediaError(on, f) = &mut f.0 {
                        if *on == id {
                            f(code, message.clone());
                        }
                    }
                }
            }
        }
    }
}

/// The page's answer to a focus move's first eval.
#[derive(serde::Deserialize)]
struct Targets {
    /// The focused control's rect; `None` with the focus on nothing.
    from: Option<super::keymap::Rect>,
    /// The shown controls in the move's scope: rect, and whether it is in
    /// the docked player bar.
    targets: Vec<(super::keymap::Rect, bool)>,
    /// Whether the focused control is in the docked player bar.
    #[serde(default)]
    from_docked: bool,
}

/// Collect the controls in the focused control's scope (an open modal
/// dialog, else its `data-keys` area; First and Last its `data-keys-list`),
/// or report the focus on nothing. `window.__pharosTargets` keeps them for
/// the focusing eval.
fn targets_js(first_or_last: bool) -> String {
    format!(
        r#"
const T = {targets};
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
const docked = (el) => !!el.closest(".pharos-stage.is-mini");
return {{
  from: from ? rect(from) : null,
  from_docked: !!from && docked(from),
  targets: els.map((el) => [rect(el), docked(el)]),
}};
"#,
        targets = js_str(KEY_TARGETS),
        list = first_or_last,
    )
}

/// Focus the `at`th control the last `targets_js` found and return how to
/// find it again on this page: a selector and its nth match.
fn focus_js(at: usize) -> String {
    format!(
        r#"
const el = (window.__pharosTargets || [])[{at}];
if (!el) return ["", 0];
el.focus();
el.scrollIntoView(false);
const q = (v) => v.replace(/\\/g, "\\\\").replace(/'/g, "\\'");
const href = el.getAttribute("href");
const sel = href !== null ? `[href='${{q(href)}}']` : el.id ? `[id='${{q(el.id)}}']` : "";
return [sel, sel ? Math.max([...document.querySelectorAll(sel)].indexOf(el), 0) : 0];
"#
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
        let path = current_path();
        let left = lock(&LEFT_AT).get(&path).cloned();
        document::eval(&arrive_js(left.as_ref()))
            .await
            .map_err(|e| e.to_string())?;
        return Ok(());
    };
    let controls = &found.targets;
    let Some(at) = super::keymap::pick_docked(dir, from, found.from_docked, controls) else {
        return Ok(());
    };
    if controls[at].0 == from {
        return Ok(());
    }
    // Only the control moved to is marked for coming back: marking every
    // one ran a document-wide query per control, hundreds on a home page.
    let (sel, nth): (String, u32) = document::eval(&focus_js(at))
        .join()
        .await
        .map_err(|e| e.to_string())?;
    if !sel.is_empty() {
        let path = current_path();
        let mut m = lock(&LEFT_AT);
        if m.len() >= 64 && !m.contains_key(&path) {
            m.clear();
        }
        m.insert(path, (sel, nth));
    }
    Ok(())
}

// ---- the server proxy ---------------------------------------------------

/// The request headers passed on to the server. Not `Range`: the Android
/// WebView cuts a ranged request's answer out of the whole body itself, so
/// the proxy fetches and answers whole.
const PASSED_ON: [&str; 2] = ["accept", "if-none-match"];
/// How long the proxy waits for the server before answering 503.
///
/// wry blocks the WebView's request thread on our answer for at most 10 s
/// and then panics (`recv_timeout(..).unwrap()` in wry's android module),
/// which wedges the app. A first transcoded segment, a resume point deep
/// into a title in particular, can take the server longer than that. A 503
/// instead is an ordinary load error that hls.js retries.
const PROXY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);
/// The response headers passed back to the page.
const PASSED_BACK: [&str; 4] = ["content-type", "cache-control", "etag", "last-modified"];

fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // reqwest 0.13's default verifier is rustls-platform-verifier, which
        // on Android needs JNI setup this app does not do. Use the shared
        // ring + webpki-roots config the server uses.
        let builder = reqwest::Client::builder().user_agent(tv_page::USER_AGENT);
        let builder = match pharos_api::tls::client_config() {
            Ok(tls) => builder.tls_backend_preconfigured(tls),
            Err(error) => {
                // ring is installed above, so this still builds; the platform
                // verifier it falls back to fails every handshake loudly.
                log::error!("server proxy TLS config failed to build: {error}");
                builder
            }
        };
        builder.build().unwrap_or_default()
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
            let header = |name: &str| {
                request
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            let headers: Vec<(String, String)> = PASSED_ON
                .iter()
                .filter_map(|name| header(name).map(|v| ((*name).to_owned(), v)))
                .collect();
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let fetched = tokio::time::timeout(PROXY_DEADLINE, fetch_server(&path, &headers))
                    .await
                    .unwrap_or_else(|_| Err(String::from("timed out")));
                let response = match fetched {
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
                        log::warn!(
                            "pharos: proxy failed path_len={} ms={} error={e}",
                            path.len(),
                            started.elapsed().as_millis()
                        );
                        dioxus::mobile::wry::http::Response::builder()
                            .status(503)
                            .header("retry-after", "1")
                            .body(e.into_bytes())
                    }
                };
                // Always answer: a dropped responder panics in wry too.
                let response = response.unwrap_or_else(|e| {
                    log::warn!("pharos: proxy response build failed error={e}");
                    let mut fallback = dioxus::mobile::wry::http::Response::new(Vec::new());
                    *fallback.status_mut() = dioxus::mobile::wry::http::StatusCode::BAD_GATEWAY;
                    fallback
                });
                responder.respond(response);
            });
        });
    }
}
