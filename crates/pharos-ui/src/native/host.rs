//! Host stand-in for [`web`](super): the same calls, answering as a browser
//! with no server would. Host builds only render the app under test.

// Mirrors the browser module's surface; some of it only the browser uses.
#![allow(dead_code)]

use pharos_client::api::{Capabilities, ClientEvent, NativeApi};
use pharos_client::CallInfo;

use super::machine::{Probe, SignInReply, SignOutReply};
use super::playback::{
    Engine, LibraryTransition, MediaState, OverlayEffect, PlaybackEngine, SubtitleSize,
};

const NO_BROWSER: &str = "the native app needs a browser";

pub struct Connection;

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

pub async fn get_text(_url: &str) -> Result<String, (Option<u16>, String)> {
    Err((None, "no network on the host".into()))
}

pub async fn sleep_ms(_ms: u32) {}

pub fn set_title(_title: &str) {}

pub fn current_path() -> String {
    "/".into()
}

pub fn push(_path: &str) {}

pub fn reload() {}

pub fn replace(_path: &str) {}

pub fn on_a_control(_evt: &dioxus::prelude::MouseEvent) -> bool {
    false
}

pub fn swallow_next_context_menu() {}

pub fn go_back() -> bool {
    false
}

pub struct EscapeWatch;

pub fn on_escape(_on_escape: impl FnMut() + 'static) -> EscapeWatch {
    EscapeWatch
}

pub struct BackWatch;

pub fn on_back(_on_back: impl FnMut() -> bool + 'static) -> BackWatch {
    BackWatch
}

pub struct PopState;

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

pub fn on_pop_state(_on_path: impl FnMut(String) + 'static) -> PopState {
    PopState
}

pub fn plays_hls() -> bool {
    false
}

pub fn has_mse() -> bool {
    false
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

pub fn autoplay_blocked() -> bool {
    false
}

pub fn pause(_video_id: &str) {}

pub fn set_rate(_video_id: &str, _rate: f64) {}

pub fn set_speed(_video_id: &str, _rate: f64) {}

pub fn speed(_video_id: &str) -> f64 {
    1.0
}

pub fn unix_ms() -> i64 {
    0
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

pub fn refresh_subtitle_delay(_video_id: &str) {}
pub fn set_subtitle_delay(_video_id: &str, _delay_ms: i64) {}

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

pub fn buffered_to_ms(_id: &str) -> u64 {
    0
}

pub fn element_width(_id: &str) -> f64 {
    0.0
}

pub fn element_size(_id: &str) -> (f64, f64) {
    (0.0, 0.0)
}

pub fn viewport() -> (f64, f64) {
    (0.0, 0.0)
}

pub fn key_on_itself(_evt: &dioxus::prelude::KeyboardEvent, _class: &str) -> bool {
    false
}

pub fn on_a_button(_evt: &dioxus::prelude::PointerEvent) -> bool {
    false
}

pub fn capture_pointer(_evt: &dioxus::prelude::PointerEvent) {}

pub fn session_get(_key: &str) -> Result<Option<String>, String> {
    Ok(None)
}

pub fn session_set(_key: &str, _value: &str) -> Result<(), String> {
    Ok(())
}

pub fn toggle_fullscreen() {}

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

pub fn focus_by_id(_id: &str) {}

pub fn focus_out_of(_within: &str, _to: &str) {}

pub struct KeyListener;

pub fn on_key(_on_key: impl FnMut(&str, bool, bool) -> bool + 'static) -> KeyListener {
    KeyListener
}

pub fn on_mode_key(_on_key: impl FnMut(&str) -> bool + 'static) -> KeyListener {
    KeyListener
}

pub fn move_focus(_dir: super::keymap::Dir) -> bool {
    false
}

pub fn activate_focused() -> bool {
    false
}

pub fn press_key_action(_name: &str) -> bool {
    false
}

pub fn step_tab(_step: i8) -> bool {
    false
}

pub fn focus_arrival(_id: &str) {}

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

/// There is no page to make inert off the web.
pub struct ModalHold;

impl ModalHold {
    pub fn capture() -> Self {
        Self
    }

    pub fn engage(&mut self, _root: &str) {}
}

pub struct TrackSkips;

pub fn on_track_skip(
    _on_skip: impl FnMut(super::watch_view::Skip, &'static str) -> bool + 'static,
) -> TrackSkips {
    TrackSkips
}

pub fn offer_track_skips(_previous: bool, _next: bool) {}

/// The embedded WebView's user agent; the host build has none.
pub fn user_agent() -> String {
    String::new()
}

/// Whether the UI runs inside the Android TV app (spec 025).
pub fn android_shell() -> bool {
    cfg!(target_os = "android")
}

pub fn palette_keys() -> &'static str {
    "Ctrl+K"
}

pub fn now_ms() -> f64 {
    0.0
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

pub fn spawn_detached(_fut: impl std::future::Future<Output = ()> + 'static) {}

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

pub fn reveal_across(_selector: &str) {}

pub struct PageHide;

pub fn on_page_hide(_on_hide: impl FnMut() + 'static) -> PageHide {
    PageHide
}

pub struct PageLifecycle;

pub fn on_page_lifecycle(
    _on_hidden: impl FnMut() + 'static,
    _on_restored: impl FnMut() + 'static,
) -> PageLifecycle {
    PageLifecycle
}

pub async fn engine(_kind: Engine) -> Result<Box<dyn PlaybackEngine>, String> {
    Err(NO_BROWSER.into())
}

pub fn warn(message: &str) {
    eprintln!("{message}");
}

pub fn info(_message: &str) {}

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

pub fn stored_theme(_key: &str) -> Result<Option<String>, String> {
    Ok(None)
}

pub fn apply_tokens(_declarations: &[(String, String)]) -> Result<(), String> {
    Ok(())
}

pub fn store_theme(_key: &str, _value: &str) -> Result<(), String> {
    Ok(())
}

pub fn apply_subtitle_size(_size: SubtitleSize) {}

pub fn apply_device(_device: super::playback::DeviceClass) {}

pub fn apply_library_transition(_fx: LibraryTransition) {}

/// No layer off the web: no turn begins.
pub fn turn_begin(_item: &str, _dir: super::turn::Direction) -> bool {
    false
}
pub fn turn_expect(_item: &str, _dir: super::turn::Direction) {}
pub fn turn_open(_item: &str) {}
pub fn turn_ready(_item: &str, _image: Option<&str>, _title: &str) {}
pub fn turn_cancel(_item: &str, _reason: &str) {}

pub fn apply_overlay_effect(_fx: OverlayEffect) {}

pub fn prefers(_query: &str) -> bool {
    false
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

pub fn tap_wake(_id: &str) -> super::resume::Wake {
    super::resume::Wake::None
}

pub fn wake_media_tap(_id: &str) {}

pub fn rest_tap(_id: &str) -> Option<String> {
    None
}

/// The host has no server: the in-memory groups.
pub type Groups = super::together::FakeGroups;

pub fn groups(_api: &NativeApi) -> Groups {
    Groups::default()
}
