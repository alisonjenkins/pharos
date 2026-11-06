//! The native app: routing and history, the connection lifecycle that
//! drives [`Machine`], and the screens that load data for the pure views.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use futures_util::StreamExt as _;
use pharos_client::api::{
    Detail, Home, Item, ItemKind, ItemRef, NativeApi, NativeGrant, PharosApi as _,
    PlaybackGrant as _, TrackChoice, TrackPicks,
};
use pharos_client::{CallInfo, ClientError};

use super::account::AccountMenu;
use super::account_settings;
use super::activity;
use super::appearance::{AppearanceView, ContrastRow, Edit, EditorView, Notice, ThemeCard};
use super::approve_device;
use super::backdrop::Backdrop;
use super::browse::{self, Neighbours};
use super::diagnostics;
use super::find::{self, Find, DEBOUNCE_MS, RESULT_LIMIT};
use super::group_app::{Together, VideoPlayer};
use super::group_session::{GroupPlayer, GroupView, Ui};
use super::listen::ListenView;
use super::machine::{Event, Machine, Phase};
use super::mixes::{self, ActiveMix, MixCtx};
use super::palette::{self, Row};
use super::platform::{self, OpenError};
use super::playback::{
    choose_engine, crossfade_ms, fade_volume, max_bitrate_label, parse_crossfade,
    parse_max_bitrate, parse_volume_setting, position_at, preview_set, rust_subtitle_url, start_ms,
    styled_subtitle_signals, styled_subtitle_url, subtitle_font_urls, subtitle_switch, up_next_at,
    volume_setting, Engine, LibraryTransition, MediaEvent, MediaState, OnFatal, OnFragment,
    Outgoing, OverlayEffect, PlaybackEngine, ReportClock, SubtitleSize, SubtitleSwitch, UpNext,
    UpNextStep, CROSSFADE_KEY, LIBRARY_TRANSITION_KEY, MAX_BITRATE_KEY, OVERLAY_EFFECT_KEY,
    SUBTITLE_SIZE_KEY, VOLUME_KEY,
};
use super::playback_settings;
use super::privacy;
use super::quality::{self, Watch};
use super::queue::{Queue, QueueAction, QueueCtx, Track, RESTART_AFTER_MS};
use super::resume::{ResumeProbe, ResumeReport};
use super::route::Route;
use super::seen::{self, MemberSeen};
use super::settings::{Section, SettingsLayout};
use super::stage::{
    Handoff, Stage, StageAction, StageBar, StageClose, StageCtx, StageGrip, View, STAGE_ID,
};
use super::subtitles::{track_mode, FallbackReason, RustState, TrackMode};
use super::together::{
    can_watch_together, group_api, GroupClient as _, GroupError, GroupSummary, Groups,
};
use super::together_view::{InviteView, TogetherView, INVITE_LINK_ID};
use super::track_follow;
use super::trail;
use super::views::{
    open_route, reveal_palette_row, ErrorNote, Hits, HomeView, Loading, MarkPlayed, Nav,
    PaletteView, PlayFromStart, SearchBox, Shell, SignInView, StatusView, TitleView, PALETTE_INPUT,
};
use super::watch_view::{
    text_track_id, total_ms, Control, Controls, Skip, UpNextCard, WatchView, OUTGOING_ID,
    PLAYER_ID, SEEK_ID, VIDEO_ID,
};
use super::whats_new::{self, Close, Gate, GateInputs, Pick, Unseen, WhatsNewBadge, WhatsNewView};
use crate::theme::editor::{Draft, EffectField};
use crate::theme::resolve::PaletteRole;
use crate::theme::signal::{self, Source};
use crate::theme::value::{Density, FontStack, Rgba};
use crate::theme::{self, contrast, css, device, doc, format, Catalogue, Theme, ThemeError};
use crate::views::RootApp;

/// The page root: one route per URL, kept in step with history. `/manage`
/// mounts the older REST panes and never opens the native socket.
#[component]
pub fn Root() -> Element {
    let boot = use_hook(boot_theme);
    let themes = Themes {
        catalogue: use_signal(|| boot.catalogue.clone()),
        active: use_signal(|| boot.active.clone()),
        applied: use_signal(|| boot.applied.clone()),
    };
    use_context_provider(|| themes);
    use_hook(move || Rc::new(watch_system_preferences(themes)));
    let mut route = use_signal(|| Route::parse(&platform::current_path()));
    use_hook(|| {
        Rc::new(platform::on_pop_state(move |path| {
            route.set(Route::parse(&path))
        }))
    });
    // The tab names the page, and the item on it once that has loaded.
    let names = use_signal(HashMap::<ItemRef, String>::new);
    use_context_provider(|| PageNames(names));
    use_effect(move || {
        let at = route();
        let names = names.read();
        let name = at.item().and_then(|id| names.get(&id)).map(String::as_str);
        platform::set_title(&at.page_title(name));
    });
    // One player for the session (spec 020): the stage follows the route,
    // and a play while it is compact stays where the listener is.
    let mut stage = use_signal(Stage::restore);
    use_context_provider(|| StageCtx(stage));
    // A play click put the page in fullscreen; the player leaving the full
    // view (Back, the floating window, Close) takes it out again.
    let shown_full = use_memo(move || {
        let s = stage.read();
        s.item.is_some() && s.view == View::Full
    });
    let mut was_full = use_signal(|| false);
    use_effect(move || {
        let full = shown_full();
        if *was_full.peek() && !full {
            platform::exit_page_fullscreen();
        }
        was_full.set(full);
    });
    // Shift+P / Shift+N and the media keys skip tracks from anywhere in
    // the page, for whichever player is on show (B562).
    let skips = use_context_provider(TrackSkipCtx::default);
    use_hook(move || {
        Rc::new(platform::on_track_skip(move |skip, source| {
            // Out of the cell while it runs, so a redraw it causes can set
            // the next one.
            let owner = skips.0.borrow().owner;
            let Some(mut on_skip) = skips.0.borrow_mut().on_skip.take() else {
                platform::info(&format!(
                    "pharos: track skip dir={} source={source} to=none reason=no_player",
                    skip.label()
                ));
                return false;
            };
            let taken = on_skip(skip, source);
            // Back unless its player let go meanwhile.
            let mut target = skips.0.borrow_mut();
            if target.owner == owner {
                target.on_skip.get_or_insert(on_skip);
            }
            taken
        }))
    });
    use_effect(move || {
        let at = route();
        let mut next = stage.peek().clone();
        if let Some(view) = next.follow(&at) {
            platform::info(&format!(
                "pharos: stage view={} item={} reason=nav",
                view.label(),
                next.item.map(|i| i.to_string()).unwrap_or_default()
            ));
            stage.set(next);
        }
    });
    use_context_provider(|| {
        Nav(EventHandler::new(move |to: Route| {
            if plays_in_place(stage, &to) {
                return;
            }
            if *route.peek() != to {
                platform::push(&to.path());
                route.set(to);
            }
        }))
    });
    use_context_provider(|| {
        Replace(EventHandler::new(move |to: Route| {
            if plays_in_place(stage, &to) {
                return;
            }
            platform::replace(&to.path());
            route.set(to);
        }))
    });
    let replace = use_context::<Replace>();
    let mut group_view = use_signal(GroupView::default);
    let group_error = use_signal(|| None::<String>);
    // The store's pushes arrive outside the Dioxus runtime; this task is how
    // they reach the screens.
    let group_ui = use_coroutine(move |mut rx: UnboundedReceiver<Ui>| async move {
        while let Some(ui) = rx.next().await {
            match ui {
                Ui::View(view) => group_view.set(view),
                Ui::Navigate(item) => replace.0.call(Route::Watch(item)),
            }
        }
    });
    use_context_provider(|| Together::new(group_view, group_error, group_ui));
    match route() {
        Route::Manage => rsx! { RootApp {} },
        route => rsx! {
            Backdrop {}
            NativeApp { route }
        },
    }
}

/// A play the compact player takes in place of a navigation to the watch
/// page (spec 020 FR-001): the page stays and the stage plays the item.
fn plays_in_place(mut stage: Signal<Stage>, to: &Route) -> bool {
    if !matches!(to, Route::Watch(_)) || !stage.peek().compact_shown() {
        return false;
    }
    let taken = stage.write().takes(to);
    if taken {
        let s = stage.peek();
        platform::info(&format!(
            "pharos: stage view={} item={} reason=play",
            s.view.label(),
            s.item.map(|i| i.to_string()).unwrap_or_default()
        ));
    }
    taken
}

/// How often, and how many times, to ask again for word times the aligner is
/// still working out (B499): a song takes it a minute or three on a CPU.
const ALIGN_ASK_MS: u32 = 20_000;
const ALIGN_ASKS: u32 = 15;

const PREFERS_DARK: &str = "(prefers-color-scheme: dark)";
const PREFERS_MORE_CONTRAST: &str = "(prefers-contrast: more)";
const IMPORT_ACCEPT: &str = ".toml,application/toml";
const THEME_MIME: &str = "application/toml";
/// The colour shown for a role when neither the draft nor its base resolves.
const UNRESOLVED: Rgba = Rgba {
    r: 0,
    g: 0,
    b: 0,
    a: 255,
};

/// The theme state every screen shares: the named themes, the choice (a
/// name or `auto`) and the name applied now.
#[derive(Clone, Copy)]
pub struct Themes {
    catalogue: Signal<Catalogue>,
    active: Signal<String>,
    applied: Signal<String>,
}

impl Themes {
    /// The theme applied now, resolved from the catalogue; reading it
    /// subscribes the caller to theme changes.
    pub fn applied_theme(&self) -> Option<Theme> {
        let name = self.applied.read();
        self.catalogue.read().resolve(&name).ok().map(|r| r.theme)
    }

    /// Apply the active choice and record the name applied. `auto` resolves
    /// against the system's preferences at this moment.
    fn apply_active(mut self) {
        let active = self.active.peek().clone();
        let (dark, more) = prefers();
        let catalogue = self.catalogue.peek();
        match theme::device::startup(&catalogue, Ok(Some(active)), dark, more) {
            Ok(startup) => {
                log_lines(&startup.lines);
                apply_theme(&startup.theme);
                self.applied.set(startup.theme.name);
            }
            Err(e) => warn_unavailable(&e),
        }
    }

    /// Make `name` the choice, apply it and store the choice.
    fn choose(mut self, name: String) {
        self.active.set(name);
        self.apply_active();
        self.store_active();
    }

    fn store_active(self) {
        store_setting(device::ACTIVE_KEY, &self.active.peek());
    }

    fn store_custom(self) {
        let json = device::custom_json(&self.catalogue.peek());
        store_setting(device::CUSTOM_KEY, &json);
    }

    /// The catalogue entry `name` changed: re-apply it when it is the active
    /// choice, and store the custom themes.
    fn catalogue_changed(self, name: &str) {
        if *self.active.peek() == name {
            self.apply_active();
        }
        self.store_custom();
    }

    /// The system's preferences changed: re-resolve when `auto` is chosen.
    fn follow_system(self) {
        if *self.active.peek() == device::AUTO {
            self.apply_active();
        }
    }
}

/// The device's catalogue and choices at start-up.
#[derive(Clone)]
struct Boot {
    catalogue: Catalogue,
    active: String,
    applied: String,
}

/// Load the custom themes, then apply the device's stored choice before the
/// first native paint, logging which theme and, on a fallback, why.
fn boot_theme() -> Boot {
    platform::apply_subtitle_size(stored_subtitle_size());
    platform::apply_library_transition(stored_library_transition());
    platform::apply_overlay_effect(stored_overlay_effect());
    let mut catalogue = theme::shipped();
    log_lines(&device::load_custom(
        &mut catalogue,
        platform::stored_theme(device::CUSTOM_KEY),
    ));
    let stored = platform::stored_theme(device::ACTIVE_KEY);
    let auto = matches!(&stored, Ok(Some(name)) if name == device::AUTO);
    let (dark, more) = prefers();
    match device::startup(&catalogue, stored, dark, more) {
        Ok(startup) => {
            log_lines(&startup.lines);
            apply_theme(&startup.theme);
            let active = if auto {
                device::AUTO.to_owned()
            } else {
                startup.theme.name.clone()
            };
            Boot {
                catalogue,
                active,
                applied: startup.theme.name,
            }
        }
        Err(e) => {
            warn_unavailable(&e);
            Boot {
                catalogue,
                active: theme::DEFAULT.to_owned(),
                applied: theme::DEFAULT.to_owned(),
            }
        }
    }
}

/// Keep the two system-preference listeners for the page's life; a change
/// re-resolves `auto`.
fn watch_system_preferences(themes: Themes) -> [platform::MediaWatch; 2] {
    [
        platform::on_media_change(PREFERS_DARK, move || themes.follow_system()),
        platform::on_media_change(PREFERS_MORE_CONTRAST, move || themes.follow_system()),
    ]
}

fn prefers() -> (bool, bool) {
    (
        platform::prefers(PREFERS_DARK),
        platform::prefers(PREFERS_MORE_CONTRAST),
    )
}

/// Set the page's tokens in one style write. A failure is logged and the
/// page keeps the tokens it had.
fn apply_theme(theme: &Theme) {
    if let Err(e) = platform::apply_tokens(&css::declarations(theme)) {
        platform::warn(&format!(
            "pharos: theme apply failed name={} error={e}",
            theme.name
        ));
    }
}

fn warn_unavailable(e: &ThemeError) {
    platform::warn(&format!(
        "pharos: theme unavailable reason={} keeping=pharos.css",
        e.label()
    ));
}

fn log_lines(lines: &[device::Line]) {
    for line in lines {
        match line {
            device::Line::Info(l) => platform::info(l),
            device::Line::Warn(l) => {
                platform::warn(l);
                if l.starts_with("pharos: theme fallback") {
                    platform::report_event(pharos_client::api::ClientEvent {
                        kind: pharos_client::api::ClientEventKind::ThemeFallback,
                        level: pharos_client::api::ClientLevel::Warn,
                        route: String::new(),
                        fields: vec![("line", l.clone())],
                    });
                }
            }
        }
    }
}

fn store_setting(key: &str, value: &str) {
    if let Err(e) = platform::store_theme(key, value) {
        platform::warn(&signal::storage_failed(key, &e));
    }
}

thread_local! {
    /// The viewer's last volume and mute this page load, so a player keeps it
    /// even where `localStorage` cannot be written.
    static LAST_VOLUME: Cell<Option<(f64, bool)>> = const { Cell::new(None) };
}

/// The viewer's last volume and mute: this page load's, else the stored one.
fn last_volume() -> Option<(f64, bool, &'static str)> {
    if let Some((volume, muted)) = LAST_VOLUME.get() {
        return Some((volume, muted, "page"));
    }
    let stored = match platform::stored_theme(VOLUME_KEY) {
        Ok(stored) => stored?,
        Err(e) => {
            platform::warn(&format!("pharos: volume storage read failed error={e}"));
            return None;
        }
    };
    match parse_volume_setting(&stored) {
        Some((volume, muted)) => Some((volume, muted, "storage")),
        None => {
            platform::warn(&format!(
                "pharos: volume not restored reason=unreadable stored={stored:?}"
            ));
            None
        }
    }
}

/// Set the new media element to `last`, the viewer's last volume.
fn restore_volume(last: Option<(f64, bool, &'static str)>) {
    if let Some((volume, muted, from)) = last {
        platform::set_volume(VIDEO_ID, volume, muted);
        platform::info(&format!(
            "pharos: volume restored volume={volume:.3} muted={muted} from={from}"
        ));
    }
}

fn remember_volume(volume: f64, muted: bool) {
    LAST_VOLUME.set(Some((volume, muted)));
    if let Err(e) = platform::store_theme(VOLUME_KEY, &volume_setting(volume, muted)) {
        platform::warn(&format!("pharos: volume storage write failed error={e}"));
    }
}

/// The capabilities to negotiate `kind` with: what the browser decodes, and
/// for video the maximum bitrate the viewer chose (0, Auto, unless they set
/// one). The choice limits video only: a song above it still plays as stored.
fn negotiation_capabilities(kind: ItemKind, auto: u64) -> pharos_client::api::Capabilities {
    let chosen = stored_bitrate(MAX_BITRATE_KEY, parse_max_bitrate);
    let caps = with_max_bitrate(
        platform::capabilities(),
        kind,
        quality::effective_cap(chosen, auto),
    );
    platform::info(&format!(
        "pharos: playback max_bitrate={} chosen={} auto={} kind={kind:?}",
        max_bitrate_label(caps.max_bitrate),
        max_bitrate_label(chosen),
        max_bitrate_label(auto)
    ));
    caps
}

/// The bitrate stored under `key`, read by `parse`; 0 when none. The warn
/// text names the key's last segment (`max_bitrate`, `auto_rung`).
fn stored_bitrate(key: &str, parse: fn(&str) -> u64) -> u64 {
    match platform::stored_theme(key) {
        Ok(stored) => stored.as_deref().map_or(0, parse),
        Err(e) => {
            let name = key.rsplit('.').next().unwrap_or(key);
            platform::warn(&format!("pharos: {name} storage read failed error={e}"));
            0
        }
    }
}

fn store_bitrate(key: &str, bps: u64) {
    if let Err(e) = platform::store_theme(key, &bps.to_string()) {
        let name = key.rsplit('.').next().unwrap_or(key);
        platform::warn(&format!("pharos: {name} storage write failed error={e}"));
    }
}

thread_local! {
    /// The subtitle size chosen on this page load. It outranks storage, so a
    /// choice the browser refused to keep still shows and applies until a
    /// reload.
    static SESSION_SUBTITLE_SIZE: Cell<Option<SubtitleSize>> = const { Cell::new(None) };
}

/// The viewer's subtitle size: this page load's choice, else the stored one,
/// else Medium.
fn stored_subtitle_size() -> SubtitleSize {
    if let Some(size) = SESSION_SUBTITLE_SIZE.with(Cell::get) {
        return size;
    }
    match platform::stored_theme(SUBTITLE_SIZE_KEY) {
        Ok(stored) => stored
            .as_deref()
            .map_or(SubtitleSize::Medium, SubtitleSize::parse),
        Err(e) => {
            platform::warn(&format!(
                "pharos: subtitle_size storage read failed error={e}"
            ));
            SubtitleSize::Medium
        }
    }
}

/// Keep `size` for this page load and try to save it on the device. The
/// write's error comes back so the page can say the choice was not kept.
fn store_subtitle_size(size: SubtitleSize) -> Result<(), String> {
    SESSION_SUBTITLE_SIZE.with(|s| s.set(Some(size)));
    platform::store_theme(SUBTITLE_SIZE_KEY, size.as_str()).inspect_err(|e| {
        platform::warn(&format!(
            "pharos: subtitle_size storage write failed error={e}"
        ));
    })
}

thread_local! {
    /// The library transition chosen on this page load; it outranks storage
    /// as [`SESSION_SUBTITLE_SIZE`] does.
    static SESSION_LIBRARY_TRANSITION: Cell<Option<LibraryTransition>> = const { Cell::new(None) };
}

/// The viewer's library card transition: this page load's choice, else the
/// stored one, else Fade.
fn stored_library_transition() -> LibraryTransition {
    if let Some(fx) = SESSION_LIBRARY_TRANSITION.with(Cell::get) {
        return fx;
    }
    match platform::stored_theme(LIBRARY_TRANSITION_KEY) {
        Ok(stored) => stored
            .as_deref()
            .map_or(LibraryTransition::default(), LibraryTransition::parse),
        Err(e) => {
            platform::warn(&format!(
                "pharos: library_transition storage read failed error={e}"
            ));
            LibraryTransition::default()
        }
    }
}

/// Keep `fx` for this page load and try to save it on the device. The
/// write's error comes back so the page can say the choice was not kept.
fn store_library_transition(fx: LibraryTransition) -> Result<(), String> {
    SESSION_LIBRARY_TRANSITION.with(|s| s.set(Some(fx)));
    platform::store_theme(LIBRARY_TRANSITION_KEY, fx.as_str()).inspect_err(|e| {
        platform::warn(&format!(
            "pharos: library_transition storage write failed error={e}"
        ));
    })
}

thread_local! {
    /// The controls effect chosen on this page load; it outranks storage as
    /// [`SESSION_SUBTITLE_SIZE`] does.
    static SESSION_OVERLAY_EFFECT: Cell<Option<OverlayEffect>> = const { Cell::new(None) };
}

/// The viewer's controls effect: this page load's choice, else the stored
/// one, else Fade.
fn stored_overlay_effect() -> OverlayEffect {
    if let Some(fx) = SESSION_OVERLAY_EFFECT.with(Cell::get) {
        return fx;
    }
    match platform::stored_theme(OVERLAY_EFFECT_KEY) {
        Ok(stored) => stored
            .as_deref()
            .map_or(OverlayEffect::default(), OverlayEffect::parse),
        Err(e) => {
            platform::warn(&format!(
                "pharos: overlay_effect storage read failed error={e}"
            ));
            OverlayEffect::default()
        }
    }
}

/// Keep `fx` for this page load and try to save it on the device. The
/// write's error comes back so the page can say the choice was not kept.
fn store_overlay_effect(fx: OverlayEffect) -> Result<(), String> {
    SESSION_OVERLAY_EFFECT.with(|s| s.set(Some(fx)));
    platform::store_theme(OVERLAY_EFFECT_KEY, fx.as_str()).inspect_err(|e| {
        platform::warn(&format!(
            "pharos: overlay_effect storage write failed error={e}"
        ));
    })
}

/// `caps` with the bitrate to negotiate `kind` with, from the viewer's
/// `chosen` limit.
fn with_max_bitrate(
    mut caps: pharos_client::api::Capabilities,
    kind: ItemKind,
    chosen: u64,
) -> pharos_client::api::Capabilities {
    caps.max_bitrate = video_max_bitrate(kind, chosen);
    caps
}

/// The viewer's maximum-quality choice applies to video, never to audio.
fn video_max_bitrate(kind: ItemKind, chosen: u64) -> u64 {
    if kind == ItemKind::Audio {
        0
    } else {
        chosen
    }
}

thread_local! {
    /// The crossfade length this page load, in seconds, once read or set.
    static CROSSFADE_S: Cell<Option<u32>> = const { Cell::new(None) };
}

/// The listener's crossfade length in seconds; 0 (off) unless they chose one.
pub(super) fn crossfade_seconds() -> u32 {
    if let Some(seconds) = CROSSFADE_S.get() {
        return seconds;
    }
    let seconds = match platform::stored_theme(CROSSFADE_KEY) {
        Ok(stored) => stored.as_deref().map_or(0, parse_crossfade),
        Err(e) => {
            platform::warn(&format!("pharos: crossfade storage read failed error={e}"));
            0
        }
    };
    CROSSFADE_S.set(Some(seconds));
    seconds
}

pub(super) fn set_crossfade_seconds(seconds: u32) {
    CROSSFADE_S.set(Some(seconds));
    platform::info(&format!("pharos: crossfade set seconds={seconds}"));
    if let Err(e) = platform::store_theme(CROSSFADE_KEY, &seconds.to_string()) {
        platform::warn(&format!("pharos: crossfade storage write failed error={e}"));
    }
}

/// How often a fade steps the volume; a hidden tab's timers may run slower,
/// which only makes the steps coarser.
const FADE_STEP_MS: u32 = 50;

/// Ramp the media element `id`'s volume from `from` to `to` over `fade_ms`,
/// unless `cancel` is set first. Dropped with the player that spawned it.
async fn fade(id: &'static str, from: f64, to: f64, fade_ms: u64, cancel: Rc<Cell<bool>>) {
    let start = platform::now_ms();
    #[allow(clippy::cast_precision_loss)]
    let fade_ms = fade_ms as f64;
    loop {
        if cancel.get() {
            return;
        }
        let elapsed = platform::now_ms() - start;
        let muted = platform::media_state(id).muted;
        platform::set_volume(id, fade_volume(from, to, elapsed, fade_ms), muted);
        if elapsed >= fade_ms {
            return;
        }
        platform::sleep_ms(FADE_STEP_MS).await;
    }
}

/// How often a resume in flight reads the analyser (B563).
const RESUME_STEP_MS: u32 = 10;

/// A resume in flight, and a count of resumes so a superseded watch stops.
type ResumeSlot = Rc<RefCell<(u64, Option<ResumeProbe>)>>;

fn report_resume(report: &ResumeReport) {
    platform::info(&report.line());
    platform::report_event(report.event());
}

/// Start timing a resume of the media element `id`, before `play()` so its
/// `playing` cannot come first. Returns the resume's number.
fn start_resume(id: &str, slot: &ResumeSlot) -> u64 {
    let (n, superseded) = {
        let mut slot = slot.borrow_mut();
        let superseded = slot.1.as_mut().and_then(ResumeProbe::superseded);
        slot.0 = slot.0.wrapping_add(1);
        slot.1 = Some(ResumeProbe::start(
            platform::now_ms(),
            platform::resume_context(id),
        ));
        (slot.0, superseded)
    };
    if let Some(report) = superseded {
        report_resume(&report);
    }
    n
}

/// Measure resume number `resume` of the media element `id` until it
/// sounds, is paused again, or times out (V473).
async fn watch_resume(id: &'static str, slot: ResumeSlot, resume: u64) {
    loop {
        platform::sleep_ms(RESUME_STEP_MS).await;
        let report = {
            let mut slot = slot.borrow_mut();
            if slot.0 != resume {
                return;
            }
            let Some(probe) = slot.1.as_mut() else {
                return;
            };
            if probe.done() {
                return;
            }
            probe.frame(platform::now_ms(), platform::tap_peak(id))
        };
        if let Some(report) = report {
            report_resume(&report);
            return;
        }
    }
}

fn source_label(name: &str) -> Source {
    if theme::is_shipped(name) {
        Source::Shipped
    } else {
        Source::Local
    }
}

fn unknown_theme(name: &str) {
    platform::warn(&signal::fallback(
        name,
        source_label(name),
        &ThemeError::UnknownTheme {
            name: name.to_owned(),
        },
        "-",
    ));
}

fn bad_field(path: &str, value: &str) -> ThemeError {
    ThemeError::BadValue {
        path: path.to_owned(),
        value: value.to_owned(),
        expected: "a value the editor offers".to_owned(),
    }
}

fn notify(mut notice: Signal<Option<Notice>>, text: String, error: bool) {
    notice.set(Some(Notice { text, error }));
}

/// Store `source` as a custom theme. The outcome's lines are logged; the
/// saved name, or the reason for refusing, comes back.
fn save_theme(mut themes: Themes, source: String, from: Source) -> Result<String, String> {
    let outcome = {
        let mut catalogue = themes.catalogue.write();
        device::save(&mut catalogue, source, from)
    };
    match outcome {
        Ok(saved) => {
            log_lines(&saved.lines);
            Ok(saved.name)
        }
        Err(refusal) => {
            log_lines(&refusal.lines);
            Err(refusal.reason)
        }
    }
}

fn import(themes: Themes, notice: Signal<Option<Notice>>, source: String) {
    match save_theme(themes, source, Source::Import) {
        Ok(name) => {
            themes.catalogue_changed(&name);
            notify(notice, format!("Imported {name}"), false);
        }
        Err(reason) => notify(notice, reason, true),
    }
}

fn import_file(themes: Themes, notice: Signal<Option<Notice>>) {
    spawn(async move {
        match platform::pick_file(IMPORT_ACCEPT, doc::MAX_BYTES).await {
            Ok(Some(text)) => import(themes, notice, text),
            Ok(None) => {}
            Err(e) => {
                platform::warn(&signal::fallback("-", Source::Import, &e, "-"));
                notify(notice, e.to_string(), true);
            }
        }
    });
}

fn export(themes: Themes, notice: Signal<Option<Notice>>, name: &str) {
    let source = themes.catalogue.peek().get_source(name).map(str::to_owned);
    let Some(source) = source else {
        unknown_theme(name);
        return;
    };
    let filename = format!("{}.pharos-theme.toml", theme::slug(name));
    match platform::download(&filename, THEME_MIME, &source) {
        Ok(()) => platform::info(&signal::exported(name, source.len())),
        Err(e) => {
            platform::warn(&signal::fallback(name, source_label(name), &e, "-"));
            notify(notice, e.to_string(), true);
        }
    }
}

fn delete(
    mut themes: Themes,
    mut editing: Signal<Option<Editing>>,
    notice: Signal<Option<Notice>>,
    name: &str,
) {
    if theme::is_shipped(name) {
        platform::warn(&signal::fallback(
            name,
            Source::Shipped,
            &ThemeError::BadValue {
                path: "meta.name".to_owned(),
                value: name.to_owned(),
                expected: "a custom theme".to_owned(),
            },
            "-",
        ));
        return;
    }
    if !themes.catalogue.write().remove(name) {
        unknown_theme(name);
        return;
    }
    platform::info(&signal::deleted(name));
    let editing_it = editing
        .peek()
        .as_ref()
        .is_some_and(|state| state.base == name || state.draft.name() == Some(name));
    if editing_it {
        editing.set(None);
    }
    if *themes.active.peek() == name {
        themes.choose(theme::DEFAULT.to_owned());
    } else if editing_it {
        themes.apply_active();
    }
    themes.store_custom();
    notify(notice, format!("Deleted {name}"), false);
}

fn customise(
    themes: Themes,
    mut editing: Signal<Option<Editing>>,
    mut notice: Signal<Option<Notice>>,
    base: String,
) {
    let name = fresh_name(&themes.catalogue.peek(), &base);
    let draft = Draft::new(&name, &base);
    apply_preview(themes, &draft);
    editing.set(Some(Editing { draft, base }));
    notice.set(None);
}

fn edit_custom(
    themes: Themes,
    mut editing: Signal<Option<Editing>>,
    mut notice: Signal<Option<Notice>>,
    name: &str,
) {
    let source = themes.catalogue.peek().get_source(name).map(str::to_owned);
    let Some(source) = source else {
        unknown_theme(name);
        return;
    };
    match Draft::from_source(&source) {
        Ok(draft) => {
            let base = format::read(&source)
                .ok()
                .and_then(|(doc, _)| doc.extends().map(str::to_owned))
                .unwrap_or_else(|| theme::DEFAULT.to_owned());
            apply_preview(themes, &draft);
            editing.set(Some(Editing { draft, base }));
            notice.set(None);
        }
        Err(e) => {
            platform::warn(&signal::fallback(name, source_label(name), &e, "-"));
            notify(notice, e.to_string(), true);
        }
    }
}

/// Save the draft as a custom theme and make it the choice. A refusal keeps
/// the editor open with its reason.
fn save_editor(
    themes: Themes,
    mut editing: Signal<Option<Editing>>,
    notice: Signal<Option<Notice>>,
) {
    let Some(state) = editing.peek().clone() else {
        return;
    };
    let source = match state.draft.source() {
        Ok(source) => source,
        Err(e) => {
            platform::warn(&signal::fallback(
                state.draft.name().unwrap_or("-"),
                Source::Editor,
                &e,
                "-",
            ));
            notify(notice, e.to_string(), true);
            return;
        }
    };
    match save_theme(themes, source, Source::Editor) {
        Ok(name) => {
            editing.set(None);
            themes.choose(name.clone());
            themes.store_custom();
            notify(notice, format!("Saved {name}"), false);
        }
        Err(reason) => notify(notice, reason, true),
    }
}

/// Apply one editor change to the draft, then preview it live. A change the
/// editor could not have sent is logged and dropped.
fn edit_draft(themes: Themes, mut editing: Signal<Option<Editing>>, edit: Edit) {
    let Some(mut state) = editing.peek().clone() else {
        return;
    };
    match apply_edit(&mut state.draft, edit) {
        Ok(()) => {
            apply_preview(themes, &state.draft);
            editing.set(Some(state));
        }
        Err(e) => platform::warn(&signal::fallback(
            state.draft.name().unwrap_or("-"),
            Source::Editor,
            &e,
            "-",
        )),
    }
}

fn apply_edit(draft: &mut Draft, edit: Edit) -> Result<(), ThemeError> {
    match edit {
        Edit::Name(name) => draft.set_name(&name),
        Edit::Colour(role, hex) => {
            let parsed = PaletteRole::parse(&role).ok_or_else(|| bad_field("palette", &role))?;
            draft.set_colour(parsed, &hex);
        }
        Edit::Density(label) => {
            let density =
                Density::parse(&label).ok_or_else(|| bad_field("shape.density", &label))?;
            draft.set_density(density);
        }
        Edit::UiFont(label) => {
            let font = FontStack::parse(&label).ok_or_else(|| bad_field("type.ui", &label))?;
            draft.set_ui_font(font);
        }
        Edit::Effect(label, value) => {
            let field = effect_field(&label).ok_or_else(|| bad_field("effects", &label))?;
            draft.set_effect(field, value);
        }
    }
    Ok(())
}

fn effect_field(label: &str) -> Option<EffectField> {
    match label {
        "blur" => Some(EffectField::Blur),
        "vignette" => Some(EffectField::Vignette),
        "grain" => Some(EffectField::Grain),
        "glow" => Some(EffectField::Glow),
        _ => None,
    }
}

/// Show the draft's resolved tokens without saving them. A draft that does
/// not resolve keeps the tokens already applied, and logs why.
fn apply_preview(themes: Themes, draft: &Draft) {
    match draft.preview(&themes.catalogue.peek()) {
        Ok(resolved) => apply_theme(&resolved.theme),
        Err(e) => platform::warn(&signal::fallback(
            draft.name().unwrap_or("-"),
            Source::Editor,
            &e,
            &themes.applied.peek(),
        )),
    }
}

fn cancel(
    themes: Themes,
    mut editing: Signal<Option<Editing>>,
    mut notice: Signal<Option<Notice>>,
) {
    editing.set(None);
    notice.set(None);
    themes.apply_active();
}

/// `My <base>`, or the first `My <base> <n>` no custom theme already uses.
fn fresh_name(catalogue: &Catalogue, base: &str) -> String {
    let first = format!("My {base}");
    if catalogue.get_source(&first).is_none() {
        return first;
    }
    (2..)
        .map(|n| format!("My {base} {n}"))
        .find(|name| catalogue.get_source(name).is_none())
        .unwrap_or(first)
}

/// The theme cards for the catalogue. A theme that does not resolve gets no
/// card; its fallback line is logged instead.
fn theme_cards(catalogue: &Catalogue) -> Vec<ThemeCard> {
    let mut cards = Vec::new();
    for name in catalogue.names() {
        match catalogue.resolve(name) {
            Ok(resolved) => cards.push(card(name, &resolved.theme)),
            Err(e) => platform::warn(&signal::fallback(name, source_label(name), &e, "-")),
        }
    }
    cards
}

fn card(name: &str, theme: &Theme) -> ThemeCard {
    let p = &theme.palette;
    ThemeCard {
        name: name.to_owned(),
        shipped: theme::is_shipped(name),
        active: false,
        swatches: [p.bg, p.surface, p.accent, p.fg]
            .iter()
            .map(Rgba::to_css)
            .collect(),
    }
}

/// `#rrggbb`, the only form an `<input type=color>` accepts.
fn hex6(colour: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", colour.r, colour.g, colour.b)
}

/// The raw text the draft holds for `role`, if it sets one.
fn raw_colour(draft: &Draft, role: PaletteRole) -> Option<String> {
    let source = draft.source().ok()?;
    let (mut doc, _) = format::read(&source).ok()?;
    doc.palette.as_mut()?.slot(role).clone()
}

fn contrast_rows(theme: &Theme) -> Vec<ContrastRow> {
    contrast::check(theme)
        .into_iter()
        .map(|r| ContrastRow {
            pair: r.pair.to_string(),
            ratio: r.ratio,
            min: r.min,
            pass: r.pass,
            decorative: r.decorative,
        })
        .collect()
}

fn effects_of(theme: &Theme) -> Vec<(String, f64)> {
    let e = &theme.effects;
    vec![
        ("blur".to_owned(), f64::from(e.blur_px)),
        ("vignette".to_owned(), e.vignette),
        ("grain".to_owned(), e.grain),
        ("glow".to_owned(), e.glow),
    ]
}

/// The editor's view of a draft. Each colour is the draft's resolved colour;
/// a role whose draft value is not a colour shows the base theme's. The
/// preview's error is shown, and its contrast table left empty.
fn editor_view(state: &Editing, catalogue: &Catalogue) -> EditorView {
    let base = catalogue.resolve(&state.base).ok().map(|r| r.theme);
    let preview = state.draft.preview(catalogue);
    let colours = PaletteRole::ALL
        .iter()
        .map(|&role| {
            let colour = match &preview {
                Ok(resolved) => resolved.theme.palette.get(role),
                Err(_) => raw_colour(&state.draft, role)
                    .and_then(|raw| Rgba::parse(&raw))
                    .or_else(|| base.as_ref().map(|t| t.palette.get(role)))
                    .unwrap_or(UNRESOLVED),
            };
            (role.label().to_owned(), hex6(colour))
        })
        .collect();
    let shown: Option<&Theme> = match &preview {
        Ok(resolved) => Some(&resolved.theme),
        Err(_) => base.as_ref(),
    };
    EditorView {
        name: state.draft.name().unwrap_or_default().to_owned(),
        base: state.base.clone(),
        colours,
        density: shown
            .map(|t| t.density.label().to_owned())
            .unwrap_or_default(),
        ui_font: shown.map(|t| t.ui.label().to_owned()).unwrap_or_default(),
        effects: shown.map(effects_of).unwrap_or_default(),
        contrast: preview
            .as_ref()
            .map(|resolved| contrast_rows(&resolved.theme))
            .unwrap_or_default(),
        error: preview.as_ref().err().map(ToString::to_string),
    }
}

/// An edit in progress: the draft and the theme it extends.
#[derive(Clone)]
struct Editing {
    draft: Draft,
    base: String,
}

/// The names of the items pages have loaded, for the tab title. By id, as
/// the screen and the compact player beside it each load their own.
#[derive(Clone, Copy)]
struct PageNames(Signal<HashMap<ItemRef, String>>);

fn use_page_name(id: ItemRef, name: Option<String>) {
    let PageNames(mut names) = use_context::<PageNames>();
    use_effect(use_reactive!(|(id, name)| {
        if let Some(name) = name {
            if names.peek().get(&id) != Some(&name) {
                names.write().insert(id, name);
            }
        }
    }));
}

/// Navigation that replaces the current history entry, for redirects.
#[derive(Clone, Copy)]
struct Replace(EventHandler<Route>);

/// Who answers a page-wide track skip (B562): the player on show. A player
/// takes it over with its own `owner` number, so a song handing over cannot
/// clear what the next song set when it unmounts.
#[derive(Clone, Default)]
struct TrackSkipCtx(Rc<RefCell<SkipTarget>>);

type OnSkip = Box<dyn FnMut(Skip, &'static str) -> bool>;

#[derive(Default)]
struct SkipTarget {
    /// The last owner number handed out.
    issued: u64,
    /// The player answering skips; 0 for none.
    owner: u64,
    on_skip: Option<OnSkip>,
}

/// Let go of the track skips, if player `owner` still holds them.
fn release_track_skips(skips: &TrackSkipCtx, owner: u64) {
    let mut target = skips.0.borrow_mut();
    if target.owner == owner {
        target.owner = 0;
        target.on_skip = None;
        drop(target);
        platform::offer_track_skips(false, false);
    }
}

/// The item Play from start was pressed on: its next negotiation starts at
/// 0 s, whatever position the server saved, and clears it.
#[derive(Clone, Copy)]
struct StartOver(Signal<Option<ItemRef>>);

/// Who is signed in, as the account menu shows them; a screen that changes
/// the avatar restarts it.
#[derive(Clone, Copy)]
pub(super) struct AccountMe(Resource<Option<pharos_client::api::Me>>);

/// What screens share: the signed-in API (none until resumed) and the
/// machine, so a `revoked` answer can send the app to sign-in.
#[derive(Clone, Copy)]
pub(super) struct Ctx {
    pub(super) api: Signal<Option<NativeApi>>,
    pub(super) machine: Signal<Machine>,
    /// Counts stored watched marks; a screen that shows them reads it, so a
    /// mark reloads it.
    pub(super) marks: Signal<u32>,
}

#[component]
fn NativeApp(route: Route) -> Element {
    let mut machine = use_signal(Machine::new);
    let mut api = use_signal(|| None::<NativeApi>);
    let mut task = use_signal(|| None::<dioxus::core::Task>);
    let mut started = use_signal(|| None::<u32>);
    // A page loaded before a deploy reconnects to the newer server, which may
    // send values this build cannot read: it asks to be reloaded.
    let outdated = use_signal(|| false);
    let mut marks = use_signal(|| 0u32);
    use_context_provider(|| Ctx {
        api,
        machine,
        marks,
    });
    // The play queue lives as long as the app, across pages and songs.
    let queue = use_signal(Queue::default);
    use_context_provider(|| QueueCtx(queue));
    // The mix that is on keeps the queue topped up (spec 024, B441).
    let mix = use_signal(|| None::<ActiveMix>);
    use_context_provider(|| MixCtx(mix));
    mixes::use_top_up(api, queue, mix);
    use_context_provider(|| {
        let marking = Rc::new(RefCell::new(std::collections::HashSet::<ItemRef>::new()));
        MarkPlayed(EventHandler::new(move |(id, played): (ItemRef, bool)| {
            let Some(api) = api.peek().clone() else {
                return;
            };
            // One mark per item at a time: a second click before the reload
            // would record a second play.
            if !marking.borrow_mut().insert(id) {
                platform::info(&format!(
                    "pharos: mark {id} played={played} skipped: one in flight"
                ));
                return;
            }
            let marking = Rc::clone(&marking);
            spawn(async move {
                let result = api.set_played(&call("mark", "played"), id, played).await;
                if let Err(e) = settle(machine, result) {
                    platform::warn(&format!("pharos: marking {id} played={played} failed: {e}"));
                }
                marking.borrow_mut().remove(&id);
                // Reload either way: a season that failed part way through
                // has some episodes marked, and the screen shows what is stored.
                marks += 1;
            });
        }))
    });
    let search = Search {
        text: use_signal(String::new),
        find: use_signal(Find::default),
        hits: use_signal(|| Hits::Idle),
    };
    use_context_provider(|| search);
    let mut palette = Palette(use_signal(|| false));
    use_context_provider(|| palette);
    let whats_new_state = use_whats_new_state();
    use_context_provider(|| whats_new_state);
    use_hook(move || {
        Rc::new(platform::on_key(move |key, ctrl, meta| {
            // Signed out there is no palette to open: leave the key alone
            // rather than open one that pops up right after sign-in.
            let toggle = palette::is_toggle(key, ctrl, meta) && api.peek().is_some();
            if toggle {
                let shut = !*palette.0.peek();
                palette.0.set(shut);
            }
            toggle
        }))
    });
    let nav = use_context::<Nav>();
    let mut start_over = use_signal(|| None::<ItemRef>);
    use_context_provider(|| StartOver(start_over));
    use_context_provider(|| {
        PlayFromStart(EventHandler::new(move |id: ItemRef| {
            start_over.set(Some(id));
            nav.0.call(Route::Watch(id));
        }))
    });
    // Signing out forgets the names pages loaded, so the tab title of a
    // signed-out page never shows the last user's item to the next one.
    let PageNames(mut names) = use_context::<PageNames>();
    use_effect(move || {
        if matches!(machine.read().phase(), Phase::SignIn { .. }) && !names.peek().is_empty() {
            names.write().clear();
        }
    });
    // Signing out ends the session, and the player with it; signing in
    // again on the watch route, which never changed, plays its item.
    let StageCtx(mut stage) = use_context::<StageCtx>();
    let at = route.clone();
    use_effect(use_reactive!(|(at,)| match machine.read().phase() {
        Phase::SignIn { .. } if stage.peek().item.is_some() => stage.write().close(),
        Phase::Ready => {
            let mut next = stage.peek().clone();
            if let Some(view) = next.resume(&at) {
                platform::info(&format!(
                    "pharos: stage view={} item={} reason=signin",
                    view.label(),
                    next.item.map(|i| i.to_string()).unwrap_or_default()
                ));
                stage.set(next);
            }
        }
        _ => {}
    }));
    // The library buttons, read again for each signed-in API; a reconnect
    // keeps the last list until the new one arrives.
    let mut libraries = use_signal(Vec::<Item>::new);
    use_effect(move || {
        let Some(api) = api.read().clone() else {
            return;
        };
        spawn(async move {
            if let Ok(page) = api.views(&call("shell", "libraries")).await {
                libraries.set(page.items);
            }
        });
    });
    // The player hides the shell, so a watch page needs no trail.
    let shown = match route {
        Route::Browse(id) | Route::Title(id) => Some(id),
        _ => None,
    };
    let trail = use_resource(use_reactive!(|(shown,)| {
        let api = api.read().clone();
        async move {
            let (Some(api), Some(id)) = (api, shown) else {
                return Vec::new();
            };
            trail::load(&api, &call("shell", "trail"), id)
                .await
                .unwrap_or_default()
        }
    }));

    use_effect(move || {
        let (phase, epoch, delay) = {
            let m = machine.read();
            (m.phase().clone(), m.epoch(), m.delay_ms())
        };
        match phase {
            Phase::Connecting if *started.peek() != Some(epoch) => {
                started.set(Some(epoch));
                if let Some(old) = task.write().take() {
                    old.cancel();
                }
                // A reconnect keeps the old API until the new one resumes,
                // so the screens keep what they loaded.
                task.set(Some(spawn(connect(machine, api, outdated, epoch, delay))));
            }
            Phase::Connecting | Phase::Ready => {}
            _ => {
                if let Some(old) = task.write().take() {
                    old.cancel();
                }
                if api.peek().is_some() {
                    api.set(None);
                    platform::forget_opt_in();
                    search.clear();
                }
            }
        }
    });

    // Who is signed in, for the account menu; asked again on each resume.
    // A failed ask leaves the menu without a name rather than blocking it.
    let me = use_resource(move || {
        let api = api.read().clone();
        async move {
            let api = api?;
            match api.me(&call("shell", "me")).await {
                Ok(me) => Some(me),
                Err(e) => {
                    platform::warn(&format!("account: me failed: {e}"));
                    None
                }
            }
        }
    });

    use_context_provider(|| AccountMe(me));

    // Ready and reconnecting render through this one node, so a dropped
    // socket does not remount the screen (and its playing video).
    if machine.read().shows_screens() {
        let notice = {
            let m = machine.read();
            if m.reconnecting() {
                Some("Connection lost. Reconnecting…".to_owned())
            } else {
                m.notice().map(str::to_owned)
            }
        };
        let me = me.read().clone().flatten();
        return rsx! {
            Shell {
                libraries: libraries(),
                trail: trail.read().clone().unwrap_or_default(),
                account: rsx! {
                    WhatsNewButton {}
                    AccountMenu {
                        me,
                        on_sign_out: move |_| {
                            if machine.peek().signing_out() {
                                return;
                            }
                            machine.write().apply(Event::SignOutSubmitted);
                            spawn(async move {
                                let reply = platform::sign_out().await;
                                machine.write().apply(Event::SignOutAnswered(reply));
                            });
                        },
                    }
                },
                search: rsx! { TopSearch {} },
                if outdated() {
                    p { class: "pharos-error", role: "alert",
                        "Pharos has been updated. Reload to keep using it. "
                        button { class: "pharos-button", onclick: move |_| platform::reload(), "Reload" }
                    }
                }
                if let Some(message) = notice {
                    ErrorNote { message }
                }
                Screen { route: route.clone() }
                StageView {}
                WhatsNew { route }
                if (palette.0)() {
                    PaletteScreen {}
                }
            }
        };
    }
    let phase = machine.read().phase().clone();
    match phase {
        Phase::Connecting | Phase::Ready => rsx! {
            StatusView {
                title: "Connecting…",
                detail: "Opening a connection to the server.",
                action: None,
            }
        },
        Phase::NotEnabled => rsx! {
            StatusView {
                title: "This interface is not enabled",
                detail: "The server has the native API turned off \
                         ([server.native_api].enabled). The existing screens are under Manage.",
                action: Some(("Open Manage".to_string(), EventHandler::new(move |_| nav.0.call(Route::Manage)))),
            }
        },
        Phase::Unreachable { detail } => rsx! {
            StatusView {
                title: "Cannot connect",
                detail,
                action: Some(("Try again".to_string(), EventHandler::new(move |_| machine.write().apply(Event::Retry)))),
            }
        },
        Phase::SignIn { error, busy } => rsx! {
            SignInView {
                error,
                busy,
                on_submit: move |(name, password): (String, String)| {
                    machine.write().apply(Event::SignInSubmitted);
                    spawn(async move {
                        let reply = platform::sign_in(&name, &password).await;
                        machine.write().apply(Event::SignInAnswered(reply));
                    });
                },
            }
        },
    }
}

/// One connection attempt: wait out the backoff, open and resume, then
/// report the socket's close.
async fn connect(
    mut machine: Signal<Machine>,
    mut api: Signal<Option<NativeApi>>,
    mut outdated: Signal<bool>,
    epoch: u32,
    delay_ms: u32,
) {
    platform::sleep_ms(delay_ms).await;
    let event = match platform::open(&call("connect", "resume")).await {
        Ok(mut open) => {
            if open
                .server_schema
                .is_some_and(pharos_client::is_outdated_by)
            {
                platform::warn(&format!(
                    "pharos: server schema {:?} is newer than this page's",
                    open.server_schema
                ));
                outdated.set(true);
            }
            api.set(Some(open.api.clone()));
            platform::install_telemetry(&open.api);
            machine.write().apply(Event::Resumed { epoch });
            let reason = platform::closed(&mut open.conn).await;
            Event::Closed { epoch, reason }
        }
        Err(OpenError::NotOpened(probe)) => Event::NotOpened { epoch, probe },
        Err(OpenError::NotSignedIn) => Event::ResumeRefused { epoch },
        Err(OpenError::Resume(error)) => Event::ResumeFailed { epoch, error },
    };
    machine.write().apply(event);
}

pub(super) fn call(route: &str, action: &str) -> CallInfo {
    CallInfo {
        ui_route: route.into(),
        ui_action: action.into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        device: "web".into(),
        ..CallInfo::default()
    }
}

/// A finished load: the value, or the message to show. A `revoked` answer
/// also sends the app to sign-in.
fn settle<T>(mut machine: Signal<Machine>, result: Result<T, ClientError>) -> Result<T, String> {
    result.map_err(|e| {
        if e.needs_sign_in() {
            machine.write().apply(Event::NeedsSignIn);
        }
        e.to_string()
    })
}

#[component]
fn Screen(route: Route) -> Element {
    match route {
        Route::Home => rsx! { HomeScreen {} },
        Route::Browse(id) => rsx! { BrowseScreen { id } },
        Route::Title(id) => rsx! { TitleScreen { id } },
        // The stage beside the screen draws the player (spec 020).
        Route::Watch(_) => rsx! {},
        Route::SignIn => rsx! { Redirect { to: Route::Home } },
        // `Root` mounts the older panes for this route.
        Route::Manage => rsx! { Redirect { to: Route::Manage } },
        Route::Appearance => {
            rsx! { SettingsLayout { current: Section::Appearance, AppearanceScreen {} } }
        }
        Route::Account => rsx! { SettingsLayout { current: Section::Account, AccountScreen {} } },
        Route::ApproveDevice => {
            rsx! { SettingsLayout { current: Section::Devices, ApproveDeviceScreen {} } }
        }
        Route::Playback => {
            rsx! { SettingsLayout { current: Section::Playback, PlaybackScreen {} } }
        }
        Route::Privacy => rsx! { SettingsLayout { current: Section::Privacy, PrivacyScreen {} } },
        Route::Find => rsx! { FindRedirect {} },
        Route::Together => rsx! { TogetherScreen {} },
        Route::TogetherGroup(id) => rsx! { InviteScreen { id } },
        Route::NotFound(path) => rsx! { ErrorNote { message: format!("Nothing at {path}.") } },
    }
}

/// Where a player sits on the stage.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Slot {
    /// The item playing. `play` is its [`Stage::play`]; `waiting` while the
    /// song before it still shows, until this one's playback is negotiated.
    Current { play: u32, waiting: bool },
    /// The song handing over to it.
    Outgoing(Handoff),
}

impl Slot {
    fn class(self) -> &'static str {
        match self {
            Slot::Current { waiting: false, .. } => "pharos-stage-slot",
            Slot::Current { waiting: true, .. } => "pharos-stage-slot is-waiting",
            Slot::Outgoing(Handoff { ready: false, .. }) => "pharos-stage-slot is-outgoing",
            Slot::Outgoing(Handoff { ready: true, .. }) => {
                "pharos-stage-slot is-outgoing is-retired"
            }
        }
    }

    /// Out of sight, or shown but no longer the listener's to drive.
    fn hidden(self) -> bool {
        !matches!(self, Slot::Current { waiting: false, .. })
    }

    fn media_id(self) -> &'static str {
        match self {
            Slot::Current { .. } => VIDEO_ID,
            Slot::Outgoing(_) => OUTGOING_ID,
        }
    }
}

/// The one player (spec 020): full on the watch route, else a floating
/// window or a mini bar. It sits beside the routed screen, so a route change
/// restyles it rather than unmounting it and its stream.
#[component]
fn StageView() -> Element {
    let StageCtx(stage) = use_context::<StageCtx>();
    let s = stage.read();
    let (Some(id), Some(key)) = (s.item, s.key()) else {
        return rsx! {};
    };
    // The playing item first, so a lookup by element id that both players
    // share (the visualiser's canvas) finds its own.
    let outgoing = s.outgoing();
    let mut slots = vec![(
        key,
        id,
        Slot::Current {
            play: s.play,
            waiting: outgoing.as_ref().is_some_and(|(h, _)| !h.ready),
        },
    )];
    if let Some((handoff, key)) = outgoing {
        slots.push((key, handoff.from, Slot::Outgoing(handoff)));
    }
    let class = format!("pharos-stage is-{}", s.view.label());
    let mut style = format!(
        "--pharos-stage-x: {:.4}; --pharos-stage-y: {:.4};",
        s.spot.x, s.spot.y
    );
    if let Some(w) = s.width {
        style.push_str(&format!(" --pharos-stage-user-w: {w:.0}px;"));
    }
    let floating = s.view == View::Floating;
    rsx! {
        div { id: STAGE_ID, class, style,
            // Another item (the next episode) mounts a new playback rather
            // than re-rendering this one's; a song handing over keeps its own.
            for (key, id, slot) in slots {
                div {
                    key: "{key}",
                    class: slot.class(),
                    "inert": slot.hidden().then_some("true"),
                    WatchScreen { id, slot }
                }
            }
            if floating {
                StageGrip {}
            }
        }
    }
}

/// The one search the top bar's box and the palette share, so a term typed
/// in the bar is still there, with its matches, in the palette.
#[derive(Clone, Copy)]
struct Search {
    text: Signal<String>,
    find: Signal<Find>,
    hits: Signal<Hits>,
}

impl Search {
    /// Each edit starts a debounced search, and only the reply to the newest
    /// edit is kept. The previous term's matches are dropped at once, so a
    /// pick during the debounce cannot open a match for other text.
    fn edit(mut self, ctx: Ctx, value: String) {
        let generation = self.find.write().edit(&value);
        self.text.set(value);
        self.hits.set(Hits::Idle);
        let Some(generation) = generation else {
            return;
        };
        spawn(async move {
            platform::sleep_ms(DEBOUNCE_MS).await;
            if !self.find.peek().is_current(generation) {
                return;
            }
            let Some(api) = ctx.api.peek().clone() else {
                return;
            };
            let term = self.find.peek().term().to_owned();
            let reply = api
                .search(&call("find", "search"), &term, RESULT_LIMIT)
                .await;
            if !self.find.peek().is_current(generation) {
                return;
            }
            match settle(ctx.machine, reply) {
                Ok(page) => self.hits.set(Hits::Found(page)),
                Err(message) => self.hits.set(Hits::Failed(message)),
            }
        });
    }

    /// Forgets the term and its matches, and outdates any search in flight:
    /// a session's results must not reach whoever signs in next.
    fn clear(mut self) {
        let _ = self.find.write().edit("");
        self.text.set(String::new());
        self.hits.set(Hits::Idle);
    }
}

/// Whether the Ctrl+K / Cmd+K palette is open.
#[derive(Clone, Copy)]
struct Palette(Signal<bool>);

/// An old `/find` link: the palette took the find screen's place.
#[component]
fn FindRedirect() -> Element {
    let mut palette = use_context::<Palette>();
    use_hook(move || palette.0.set(true));
    rsx! { Redirect { to: Route::Home } }
}

/// The palette over the page: places to go and what to resume from the home
/// reply, then the shared search's matches once something is typed.
#[component]
fn PaletteScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let search = use_context::<Search>();
    let nav = use_context::<Nav>();
    let mut palette = use_context::<Palette>();
    let mut active = use_signal(|| Some(0_usize));
    let mut home = use_signal(Home::default);
    use_hook(move || {
        spawn(async move {
            let Some(api) = ctx.api.peek().clone() else {
                return;
            };
            match settle(ctx.machine, api.home(&call("palette", "home")).await) {
                Ok(reply) => home.set(reply),
                // Places and search still work without it.
                Err(message) => platform::warn(&format!("palette: home failed: {message}")),
            }
        })
    });
    use_effect(|| platform::focus(PALETTE_INPUT));
    let rows = use_memo(move || {
        let home = home.read();
        let libraries: Vec<_> = home.rails.iter().map(|rail| rail.library.clone()).collect();
        let recent: Vec<_> = home.resume.iter().chain(&home.listening).cloned().collect();
        let hits = search.hits.read();
        let page = match &*hits {
            Hits::Found(page) => Some(page),
            _ => None,
        };
        palette::rows(
            &palette::places(&libraries),
            &recent,
            &search.text.read(),
            page,
        )
    });
    // Results arrive after the highlight was set: keep it on a row that
    // exists, so Enter opens what is highlighted.
    use_effect(move || {
        let len = rows.read().len();
        let now = *active.peek();
        let kept = match now {
            _ if len == 0 => None,
            Some(at) if at < len => Some(at),
            _ => Some(0),
        };
        if kept != now {
            active.set(kept);
        }
    });
    // The arrows can walk past the list's bottom edge; keep the pick in sight.
    use_effect(move || {
        if let Some(at) = active() {
            reveal_palette_row(at);
        }
    });
    let failed = match &*search.hits.read() {
        Hits::Failed(message) => Some(message.clone()),
        _ => None,
    };
    let mut close = move || palette.0.set(false);
    let on_key = move |evt: KeyboardEvent| match evt.key() {
        Key::ArrowDown | Key::ArrowUp => {
            evt.prevent_default();
            let down = evt.key() == Key::ArrowDown;
            let next = find::step(*active.peek(), rows.peek().len(), down);
            active.set(next);
        }
        Key::Enter => {
            evt.prevent_default();
            let picked = active
                .peek()
                .and_then(|at| rows.peek().get(at).map(Row::route));
            if let Some(route) = picked {
                close();
                nav.0.call(route);
            }
        }
        // Closes rather than letting the search input clear itself.
        Key::Escape => {
            evt.prevent_default();
            close();
        }
        _ => {}
    };
    rsx! {
        PaletteView {
            text: (search.text)(),
            rows: rows(),
            active: active(),
            failed,
            on_input: move |value| {
                active.set(Some(0));
                search.edit(ctx, value);
            },
            on_key,
            on_close: move |_| close(),
        }
    }
}

/// The top bar's search box: the shared search's first matches in a list
/// under it, walked with the arrow keys. Enter opens the match the arrows
/// are on, or the palette with all of them.
#[component]
fn TopSearch() -> Element {
    let ctx = use_context::<Ctx>();
    let search = use_context::<Search>();
    let nav = use_context::<Nav>();
    let mut palette = use_context::<Palette>();
    let mut open = use_signal(|| false);
    let mut active = use_signal(|| None::<usize>);
    let suggestions = move || match &*search.hits.peek() {
        Hits::Found(page) => page
            .items
            .iter()
            .take(find::SUGGEST_LIMIT)
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let on_key = move |evt: KeyboardEvent| match evt.key() {
        Key::ArrowDown | Key::ArrowUp => {
            evt.prevent_default();
            open.set(true);
            let down = evt.key() == Key::ArrowDown;
            let next = find::step(*active.peek(), suggestions().len(), down);
            active.set(next);
        }
        Key::Enter => {
            evt.prevent_default();
            let picked = active
                .peek()
                .and_then(|at| suggestions().into_iter().nth(at));
            open.set(false);
            active.set(None);
            match picked {
                Some(item) => nav.0.call(open_route(&item)),
                None if !search.text.peek().trim().is_empty() => palette.0.set(true),
                None => {}
            }
        }
        // A search input clears itself on Escape; the first press only
        // closes the list, and a second, with the list shut, clears.
        Key::Escape if *open.peek() => {
            evt.prevent_default();
            open.set(false);
            active.set(None);
        }
        _ => {}
    };
    rsx! {
        SearchBox {
            text: (search.text)(),
            hits: (search.hits)(),
            open: open(),
            active: active(),
            on_input: move |value| {
                open.set(true);
                active.set(None);
                search.edit(ctx, value);
            },
            hint: format!("Search ({})", platform::palette_keys()),
            on_key,
            on_open: move |_| open.set(true),
            on_close: move |_| {
                open.set(false);
                active.set(None);
            },
            on_all: move |_| palette.0.set(true),
        }
    }
}

/// The group client for the signed-in session, `None` while signed out.
fn group_client(ctx: Ctx) -> Option<Groups> {
    ctx.api.peek().as_ref().map(group_api)
}

/// The groups running now, or why they cannot be listed.
async fn list_groups(api: Option<Groups>) -> Result<Vec<GroupSummary>, GroupError> {
    match api {
        Some(api) => api.list().await,
        None => Err(GroupError("Sign in to watch together".into())),
    }
}

/// Join `id` and go to what the group is watching. A group that has not
/// said yet takes us there when it does (the store navigates on its item).
fn join_group(ctx: Ctx, together: Together, nav: Nav, id: String) {
    let Some(api) = ctx.api.peek().clone() else {
        return;
    };
    let session = together.session(&api);
    let mut error = together.error;
    error.set(None);
    // The server's Item navigates away before the join answers; the join must
    // outlive the page that started it, or the membership never begins.
    spawn_forever(async move {
        match session.join(&id).await {
            Ok(group) => {
                if let Some(item) = group.item {
                    nav.0.call(Route::Watch(item));
                }
            }
            Err(e) => error.set(Some(e.0)),
        }
    });
}

/// The Together page: the groups running now.
#[component]
fn TogetherScreen() -> Element {
    let nav = use_context::<Nav>();
    let ctx = use_context::<Ctx>();
    let together = use_context::<Together>();
    // A failure shown here is not news to the next screen.
    use_drop({
        let mut error = together.error;
        move || error.set(None)
    });
    let api = use_hook(move || group_client(ctx));
    let groups = use_resource({
        let api = api.clone();
        move || list_groups(api.clone())
    });
    let state = groups.read();
    match state.as_ref() {
        None => rsx! { Loading {} },
        Some(Err(e)) => rsx! { ErrorNote { message: e.0.clone() } },
        Some(Ok(groups)) => rsx! {
            if let Some(message) = together.error.read().clone() {
                ErrorNote { message }
            }
            TogetherView {
                groups: groups.clone(),
                on_join: move |id: String| join_group(ctx, together.clone(), nav, id),
            }
        },
    }
}

/// An invite link: join the group if it still runs.
#[component]
fn InviteScreen(id: String) -> Element {
    let nav = use_context::<Nav>();
    let ctx = use_context::<Ctx>();
    let together = use_context::<Together>();
    // A failure shown here is not news to the next screen.
    use_drop({
        let mut error = together.error;
        move || error.set(None)
    });
    let api = use_hook(move || group_client(ctx));
    let groups = use_resource({
        let api = api.clone();
        move || list_groups(api.clone())
    });
    let state = groups.read();
    match state.as_ref() {
        None => rsx! { Loading {} },
        Some(Err(e)) => rsx! { ErrorNote { message: e.0.clone() } },
        Some(Ok(groups)) => {
            let group: Option<GroupSummary> = groups.iter().find(|g| g.id == id).cloned();
            rsx! {
                if let Some(message) = together.error.read().clone() {
                    ErrorNote { message }
                }
                InviteView {
                    group,
                    on_join: move |id: String| join_group(ctx, together.clone(), nav, id),
                    on_start_new: move |_| nav.0.call(Route::Together),
                }
            }
        }
    }
}

/// Replace the current route once mounted (signed in at `/signin`).
#[component]
fn Redirect(to: Route) -> Element {
    let replace = use_context::<Replace>();
    use_effect(move || replace.0.call(to.clone()));
    rsx! { Loading {} }
}

/// The Account page: the change-password form over the native session.
#[component]
fn AccountScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let mut account = use_context::<AccountMe>();
    let mut busy = use_signal(|| false);
    let mut outcome = use_signal(|| None::<account_settings::Outcome>);
    let mut avatar_busy = use_signal(|| false);
    let mut avatar_outcome = use_signal(|| None::<account_settings::Outcome>);
    let mut description_language = use_signal(String::new);
    // Set once the viewer picks a language, so the mount read, if it answers
    // later, does not put the stored (older) value back on screen.
    let mut description_language_picked = use_signal(|| false);
    // Saves run one at a time, so quick picks can't land out of order.
    let mut description_language_saves = use_signal(|| None::<account_settings::SerialPref>);
    let me = account.0.read().clone().flatten();
    let report = move |e: &ClientError, what: &str| {
        if e.needs_sign_in() {
            let mut machine = ctx.machine;
            machine.write().apply(Event::NeedsSignIn);
        }
        platform::warn(&format!("account: {what} failed: {e}"));
    };
    use_hook(move || {
        let Some(api) = ctx.api.peek().clone() else {
            return;
        };
        spawn(async move {
            let key = account_settings::DESCRIPTION_LANGUAGE_PREF;
            match api
                .pref(&call("account", "description_language_read"), key)
                .await
            {
                Ok(_) if *description_language_picked.peek() => {}
                Ok(value) => description_language.set(
                    value
                        .map(|v| String::from_utf8_lossy(&v).into_owned())
                        .unwrap_or_default(),
                ),
                Err(e) => report(&e, "read description language"),
            }
        });
    });
    rsx! {
        account_settings::AccountView {
            me,
            busy: busy(),
            outcome: outcome(),
            avatar_busy: avatar_busy(),
            avatar_outcome: avatar_outcome(),
            on_change_password: move |(current, new): (String, String)| {
                let Some(api) = ctx.api.peek().clone() else {
                    return;
                };
                if *busy.peek() {
                    return;
                }
                busy.set(true);
                outcome.set(None);
                spawn(async move {
                    let result = api
                        .change_password(&call("account", "change_password"), &current, &new)
                        .await;
                    if let Err(e) = &result {
                        report(e, "change password");
                    }
                    outcome.set(Some(result.map_err(|e| account_settings::refusal_message(&e))));
                    busy.set(false);
                });
            },
            on_pick_avatar: move |_| {
                let Some(api) = ctx.api.peek().clone() else {
                    return;
                };
                if *avatar_busy.peek() {
                    return;
                }
                // Busy from the dialog on, so a second click opens no second picker.
                avatar_busy.set(true);
                avatar_outcome.set(None);
                spawn(async move {
                    let picture = match platform::pick_picture(account_settings::MAX_AVATAR_BYTES).await {
                        Ok(Some(picture)) => picture,
                        Ok(None) => {
                            avatar_busy.set(false);
                            return;
                        }
                        Err(e) => {
                            platform::warn(&format!("account: picture not read: {e}"));
                            avatar_outcome.set(Some(Err(format!("Could not use that picture: {e}"))));
                            avatar_busy.set(false);
                            return;
                        }
                    };
                    let result = api.set_avatar(&call("account", "set_avatar"), &picture).await;
                    match &result {
                        Ok(_) => account.0.restart(),
                        Err(e) => report(e, "set avatar"),
                    }
                    avatar_outcome.set(Some(
                        result.map(|_| ()).map_err(|e| account_settings::avatar_refusal_message(&e)),
                    ));
                    avatar_busy.set(false);
                });
            },
            on_remove_avatar: move |_| {
                let Some(api) = ctx.api.peek().clone() else {
                    return;
                };
                if *avatar_busy.peek() {
                    return;
                }
                avatar_busy.set(true);
                avatar_outcome.set(None);
                spawn(async move {
                    let result = api.remove_avatar(&call("account", "remove_avatar")).await;
                    match &result {
                        Ok(()) => account.0.restart(),
                        Err(e) => report(e, "remove avatar"),
                    }
                    avatar_outcome.set(result.err().map(|e| Err(account_settings::avatar_refusal_message(&e))));
                    avatar_busy.set(false);
                });
            },
            description_language: description_language(),
            on_description_language: move |value: String| {
                let Some(api) = ctx.api.peek().clone() else {
                    return;
                };
                description_language_picked.set(true);
                let before = description_language.peek().clone();
                description_language.set(value.clone());
                let Some(mut next) = description_language_saves
                    .write()
                    .get_or_insert_with(|| account_settings::SerialPref::new(before))
                    .pick(value)
                else {
                    return;
                };
                spawn(async move {
                    let key = account_settings::DESCRIPTION_LANGUAGE_PREF;
                    loop {
                        let ok = match api.set_pref(&call("account", "description_language_set"), key, next.as_bytes()).await {
                            Ok(()) => {
                                platform::info(&format!("account: description language set value={next}"));
                                true
                            }
                            Err(e) => {
                                report(&e, "set description language");
                                false
                            }
                        };
                        let mut saves = description_language_saves.write();
                        let Some(saves) = saves.as_mut() else { return };
                        match saves.finished(next, ok) {
                            Some(queued) => next = queued,
                            None => {
                                // Settled: show what the server holds.
                                let saved = saves.saved().to_string();
                                if *description_language.peek() != saved {
                                    description_language.set(saved);
                                }
                                return;
                            }
                        }
                    }
                });
            },
        }
    }
}

/// The Approve a device page: authorizes a Quick Connect code over the native session.
#[component]
fn ApproveDeviceScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let mut busy = use_signal(|| false);
    let mut outcome = use_signal(|| None::<Result<(), String>>);
    let report = move |e: &ClientError, what: &str| {
        if e.needs_sign_in() {
            let mut machine = ctx.machine;
            machine.write().apply(Event::NeedsSignIn);
        }
        platform::warn(&format!("approve_device: {what} failed: {e}"));
    };
    rsx! {
        approve_device::ApproveDeviceView {
            busy: busy(),
            outcome: outcome(),
            on_approve: move |code: String| {
                let Some(api) = ctx.api.peek().clone() else {
                    return;
                };
                if *busy.peek() {
                    return;
                }
                busy.set(true);
                outcome.set(None);
                spawn(async move {
                    let result = api
                        .quick_connect_authorize(&call("approve_device", "quick_connect_authorize"), &code)
                        .await;
                    if let Err(e) = &result {
                        report(e, "quick connect authorize");
                    }
                    outcome.set(Some(result.map_err(|e| approve_device::refusal_message(&e))));
                    busy.set(false);
                });
            },
        }
    }
}

/// The Appearance page: the theme gallery, imports, and the editor, mapped
/// onto the shared theme state.
#[component]
fn AppearanceScreen() -> Element {
    let themes = use_context::<Themes>();
    let mut editing = use_signal(|| None::<Editing>);
    let notice = use_signal(|| None::<Notice>);
    let mut library_transition = use_signal(stored_library_transition);
    let mut overlay_effect = use_signal(stored_overlay_effect);
    let cards = use_memo(move || theme_cards(&themes.catalogue.read()));
    // Leaving the page mid-edit must put the active theme back.
    use_drop(move || {
        if editing.peek().is_some() {
            themes.apply_active();
        }
    });

    let applied = themes.applied.read().clone();
    let mut list: Vec<ThemeCard> = cards.read().clone();
    for card in list.iter_mut() {
        card.active = card.name == applied;
    }
    let auto = *themes.active.read() == device::AUTO;
    let editor = editing
        .read()
        .as_ref()
        .map(|state| editor_view(state, &themes.catalogue.read()));
    let notice_now = notice.read().clone();
    let library_transition_now = *library_transition.read();
    let overlay_effect_now = *overlay_effect.read();

    rsx! {
        AppearanceView {
            themes: list,
            auto,
            editor,
            notice: notice_now,
            library_transition: library_transition_now,
            on_library_transition: move |fx: LibraryTransition| {
                if let Err(e) = store_library_transition(fx) {
                    notify(
                        notice,
                        format!(
                            "Could not save the library transition on this device: {e}. It applies until you reload."
                        ),
                        true,
                    );
                }
                platform::apply_library_transition(fx);
                library_transition.set(fx);
                platform::info(&format!("pharos: library_transition set={}", fx.as_str()));
            },
            overlay_effect: overlay_effect_now,
            on_overlay_effect: move |fx: OverlayEffect| {
                if let Err(e) = store_overlay_effect(fx) {
                    notify(
                        notice,
                        format!(
                            "Could not save the controls effect on this device: {e}. It applies until you reload."
                        ),
                        true,
                    );
                }
                platform::apply_overlay_effect(fx);
                overlay_effect.set(fx);
                platform::info(&format!("pharos: overlay_effect set={}", fx.as_str()));
            },
            on_pick: move |name: String| {
                editing.set(None);
                themes.choose(name.clone());
                notify(notice, format!("Applied {name}"), false);
            },
            on_auto: move |()| {
                editing.set(None);
                themes.choose(device::AUTO.to_owned());
                notify(notice, "Following the system".to_owned(), false);
            },
            on_customise: move |base: String| customise(themes, editing, notice, base),
            on_edit_custom: move |name: String| edit_custom(themes, editing, notice, &name),
            on_delete: move |name: String| delete(themes, editing, notice, &name),
            on_export: move |name: String| export(themes, notice, &name),
            on_import_file: move |()| import_file(themes, notice),
            on_import_paste: move |text: String| import(themes, notice, text),
            on_edit: move |edit: Edit| edit_draft(themes, editing, edit),
            on_save: move |()| save_editor(themes, editing, notice),
            on_cancel: move |()| cancel(themes, editing, notice),
        }
    }
}

/// The Playback page: this device's quality ceiling, subtitle size and
/// crossfade, each kept in local storage.
#[component]
fn PlaybackScreen() -> Element {
    let notice = use_signal(|| None::<Notice>);
    let mut max_quality = use_signal(|| stored_bitrate(MAX_BITRATE_KEY, parse_max_bitrate));
    let mut subtitle_size = use_signal(stored_subtitle_size);
    let mut crossfade = use_signal(crossfade_seconds);
    rsx! {
        playback_settings::PlaybackView {
            notice: notice.read().clone(),
            max_quality: max_quality(),
            on_max_quality: move |bps: u64| {
                store_bitrate(MAX_BITRATE_KEY, bps);
                max_quality.set(bps);
                platform::info(&format!("pharos: max_bitrate set={}", max_bitrate_label(bps)));
            },
            subtitle_size: subtitle_size(),
            on_subtitle_size: move |size: SubtitleSize| {
                if let Err(e) = store_subtitle_size(size) {
                    notify(
                        notice,
                        format!(
                            "Could not save the subtitle size on this device: {e}. It applies until you reload."
                        ),
                        true,
                    );
                }
                platform::apply_subtitle_size(size);
                platform::relift_cues(VIDEO_ID);
                subtitle_size.set(size);
                platform::info(&format!("pharos: subtitle_size set={}", size.as_str()));
            },
            crossfade_s: crossfade(),
            on_crossfade: move |seconds| {
                set_crossfade_seconds(seconds);
                crossfade.set(seconds);
            },
        }
    }
}

/// The Privacy page: the account's diagnostics opt-in, saved on the server.
#[component]
fn PrivacyScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let notice = use_signal(|| None::<Notice>);
    let mut diagnostics = use_signal(platform::opted_in);
    // Bumped when a save fails: the box is keyed on it, so the browser's
    // flipped checkbox is rebuilt from the server value.
    let mut diagnostics_gen = use_signal(|| 0_u32);
    // The box shows the server's choice, not whatever was cached at sign-in.
    use_future(move || async move {
        let api = ctx.api.peek().clone();
        if let Some(api) = api {
            if let Ok(on) = platform::load_opted_in(&api).await {
                diagnostics.set(on);
            }
        }
    });
    rsx! {
        privacy::PrivacyView {
            notice: notice.read().clone(),
            diagnostics: diagnostics(),
            diagnostics_key: diagnostics_gen(),
            on_diagnostics: move |on: bool| {
                let api = ctx.api.peek().clone();
                let server_before = *diagnostics.peek();
                spawn(async move {
                    let saved = match api {
                        Some(api) => platform::set_opted_in(&api, on).await.map(|()| on),
                        None => Err("not signed in".to_owned()),
                    };
                    if let Err(e) = &saved {
                        notify(
                            notice,
                            format!("Could not save the diagnostics choice: {e}"),
                            true,
                        );
                        diagnostics_gen.with_mut(|g| *g = g.wrapping_add(1));
                    }
                    diagnostics.set(diagnostics::shown_after_save(saved, server_before));
                });
            },
        }
    }
}

/// What's new (spec 027), shared by the top bar's badge and the dialog.
#[derive(Clone, Copy)]
struct WhatsNewState {
    /// `None` until read for this sign-in; then the newest id seen, if any.
    seen: Signal<Option<Option<u32>>>,
    open: Signal<bool>,
    /// Closed with Done on this page: the badge goes.
    done: Signal<bool>,
    /// Turned off on this device.
    off: bool,
}

fn use_whats_new_state() -> WhatsNewState {
    let seen = use_signal(|| None);
    let open = use_signal(|| false);
    let done = use_signal(|| false);
    let off = use_hook(|| {
        platform::stored_theme(whats_new::DEVICE_KEY)
            .ok()
            .flatten()
            .is_some_and(|v| v.trim() == whats_new::OFF)
    });
    WhatsNewState {
        seen,
        open,
        done,
        off,
    }
}

/// The top bar's what's-new button: shown while something is unseen and
/// the dialog did not splash, or was put off with Remind me later.
#[component]
fn WhatsNewButton() -> Element {
    let mut state = use_context::<WhatsNewState>();
    let seen = *state.seen.read();
    if !whats_new::badge(seen, state.off, *state.done.read()) {
        return rsx! {};
    }
    let unseen = Unseen::since(seen.flatten());
    rsx! {
        WhatsNewBadge {
            flow: unseen.flow,
            count: unseen.count(),
            on_open: move |()| {
                platform::info("pharos: whats_new opened from=badge");
                state.open.set(true);
            },
        }
    }
}

/// The what's-new dialog (spec 027): reads the newest entry this viewer has
/// been shown once per sign-in. Right after the page loads or the viewer
/// signs in it opens by itself, unless something plays or the watch page is
/// open; then, and after Remind me later, the badge opens it (V495).
#[component]
fn WhatsNew(route: Route) -> Element {
    let ctx = use_context::<Ctx>();
    let StageCtx(stage) = use_context::<StageCtx>();
    let state = use_context::<WhatsNewState>();
    let WhatsNewState {
        mut seen,
        mut open,
        mut done,
        off,
    } = state;
    let mut asked = use_signal(|| false);
    let mut splash_decided = use_signal(|| false);
    let mut page = use_signal(|| 0_usize);
    let mut replay = use_signal(|| 0_u32);
    let mut library_transition = use_signal(stored_library_transition);
    let mut subtitle_size = use_signal(stored_subtitle_size);
    let mut overlay_effect = use_signal(stored_overlay_effect);
    let themes = use_context::<Themes>();
    // Mounted afresh for each sign-in: the last user's state goes.
    use_hook(move || {
        seen.set(None);
        open.set(false);
        done.set(false);
    });
    use_effect(move || {
        let Some(api) = ctx.api.read().clone() else {
            return;
        };
        if off || *asked.peek() {
            return;
        }
        asked.set(true);
        spawn(async move {
            match api
                .pref(&call("whats_new", "seen_read"), whats_new::SEEN_PREF)
                .await
            {
                Ok(stored) => seen.set(Some(whats_new::parse_seen(stored.as_deref()))),
                Err(e) => {
                    // Unknown is not "never seen": a failed read shows nothing
                    // rather than the whole list on every flaky connect.
                    platform::warn(&format!("pharos: whats_new seen read failed error={e}"));
                    seen.set(Some(Some(whats_new::current())));
                }
            }
        });
    });
    let watch_route = matches!(route, Route::Watch(_));
    use_effect(move || {
        let loaded = *seen.read();
        if *splash_decided.peek() || (loaded.is_none() && !off) {
            return;
        }
        splash_decided.set(true);
        let verdict = whats_new::gate(GateInputs {
            seen: loaded,
            off,
            playing: stage.peek().item.is_some(),
            watch_route,
        });
        let seen_label = loaded
            .flatten()
            .map_or("none".to_owned(), |s| s.to_string());
        let flow = Unseen::since(loaded.flatten()).flow;
        platform::info(&format!(
            "pharos: whats_new splash={} flow={} seen={seen_label} current={}",
            verdict.label(),
            flow.label(),
            whats_new::current()
        ));
        if verdict == Gate::Open {
            open.set(true);
        }
    });
    let shown = *open.read() && !off;
    use_effect(use_reactive!(|(shown,)| {
        if shown {
            page.set(0);
            platform::focus_by_id(whats_new::FIRST_BUTTON_ID);
        }
    }));
    if !shown {
        return rsx! {};
    }
    let unseen = Unseen::since(seen.read().flatten());
    let applied = themes.applied.read().clone();
    let mut cards = theme_cards(&themes.catalogue.read());
    for card in cards.iter_mut() {
        card.active = card.name == applied;
    }
    let choices = whats_new::Choices {
        themes: cards,
        theme_auto: *themes.active.read() == device::AUTO,
        subtitle_size: subtitle_size(),
        library_transition: library_transition(),
        overlay_effect: overlay_effect(),
    };
    rsx! {
        WhatsNewView {
            unseen,
            choices,
            page: page(),
            replay: replay(),
            on_page: move |to: usize| {
                page.set(to);
                platform::focus_by_id(whats_new::FIRST_BUTTON_ID);
            },
            on_pick: move |pick: Pick| {
                match pick {
                    Pick::Theme(name) => {
                        platform::info(&format!("pharos: theme set={name} from=whats_new"));
                        themes.choose(name);
                    }
                    Pick::ThemeAuto => {
                        platform::info("pharos: theme set=auto from=whats_new");
                        themes.choose(device::AUTO.to_owned());
                    }
                    Pick::SubtitleSize(size) => {
                        if let Err(e) = store_subtitle_size(size) {
                            platform::warn(&format!(
                                "pharos: whats_new subtitle_size not kept error={e}"
                            ));
                        }
                        platform::apply_subtitle_size(size);
                        subtitle_size.set(size);
                        platform::info(&format!(
                            "pharos: subtitle_size set={} from=whats_new",
                            size.as_str()
                        ));
                    }
                    Pick::LibraryTransition(fx) => {
                        if let Err(e) = store_library_transition(fx) {
                            platform::warn(&format!(
                                "pharos: whats_new library_transition not kept error={e}"
                            ));
                        }
                        platform::apply_library_transition(fx);
                        library_transition.set(fx);
                        platform::info(&format!(
                            "pharos: library_transition set={} from=whats_new",
                            fx.as_str()
                        ));
                    }
                    Pick::OverlayEffect(fx) => {
                        if let Err(e) = store_overlay_effect(fx) {
                            platform::warn(&format!(
                                "pharos: whats_new overlay_effect not kept error={e}"
                            ));
                        }
                        platform::apply_overlay_effect(fx);
                        overlay_effect.set(fx);
                        platform::info(&format!(
                            "pharos: overlay_effect set={} from=whats_new",
                            fx.as_str()
                        ));
                    }
                }
                replay.with_mut(|r| *r = r.wrapping_add(1));
            },
            on_close: move |how: Close| {
                open.set(false);
                let current = whats_new::current();
                platform::info(&format!(
                    "pharos: whats_new closed how={} current={current}",
                    match how {
                        Close::Later => "later",
                        Close::Done => "done",
                    }
                ));
                if how == Close::Later {
                    return;
                }
                done.set(true);
                let Some(api) = ctx.api.peek().clone() else {
                    return;
                };
                spawn(async move {
                    let value = current.to_string();
                    let result = api
                        .set_pref(
                            &call("whats_new", "seen_write"),
                            whats_new::SEEN_PREF,
                            value.as_bytes(),
                        )
                        .await;
                    if let Err(e) = result {
                        platform::warn(&format!(
                            "pharos: whats_new seen write failed seen={current} error={e}"
                        ));
                    }
                });
            },
        }
    }
}

/// Renders a load's state: loading, its error, or `view` of its value.
fn loaded<T>(state: Option<&Result<T, String>>, view: impl FnOnce(&T) -> Element) -> Element {
    match state {
        None => rsx! { Loading {} },
        Some(Err(message)) => rsx! { ErrorNote { message: message.clone() } },
        Some(Ok(value)) => view(value),
    }
}

#[component]
fn HomeScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let home = use_resource(move || {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            Some(settle(ctx.machine, api.home(&call("home", "load")).await))
        }
    });
    let state = home.read();
    loaded(state.as_ref().and_then(Option::as_ref), |home: &Home| {
        rsx! { HomeView { home: home.clone() } }
    })
}

#[component]
fn BrowseScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    // The screen stays mounted when a tile or season tab opens another
    // parent, so the load follows `id` instead of running once.
    let listing = use_resource(use_reactive!(|(id,)| {
        let api = ctx.api.read().clone();
        let _ = (ctx.marks)();
        async move {
            let api = api?;
            let result = browse::load(&api, &call("browse", "load"), id, |line| {
                platform::warn(&line)
            })
            .await;
            Some((id, settle(ctx.machine, result)))
        }
    }));
    let state = listing.read();
    // The last parent's listing shows until this one's loads, under its name.
    let name = state
        .as_ref()
        .and_then(Option::as_ref)
        .filter(|(asked, _)| *asked == id)
        .and_then(|(_, r)| r.as_ref().ok())
        .map(|browse| browse.name(id));
    use_page_name(id, name);
    loaded(
        state
            .as_ref()
            .and_then(Option::as_ref)
            .map(|(_, result)| result),
        |browse: &browse::Browse| {
            rsx! { SeenBrowse { browse: browse.clone() } }
        },
    )
}

/// What the members of the group we are in have seen of `ids`, fetched
/// through the group session. It is fetched again when the group's roster
/// changes (a member joined or left) and when `ids` do (another item). Empty
/// outside a group, and when the answer fails: the screens simply show no
/// marks.
const SEEN_REFRESH_MS: u32 = 30_000;

fn use_group_seen(ids: Vec<ItemRef>) -> Vec<MemberSeen> {
    let together = use_context::<Together>();
    let view = (together.view)();
    let roster: Vec<String> = view.members.iter().map(|m| m.id.clone()).collect();
    let (active, item_changes) = (view.active, view.item_changes);
    // Another member's progress moves without the roster changing: fetch
    // again when the group moves to another item, and every 30 s while the
    // group is active. Not on every update: buffering flips every 2 s.
    let mut tick = use_signal(|| 0u32);
    use_future(move || async move {
        loop {
            platform::sleep_ms(SEEN_REFRESH_MS).await;
            if *tick.peek() == u32::MAX {
                break;
            }
            tick += 1;
        }
    });
    let tick = tick();
    let found = use_resource(use_reactive!(|(
        ids,
        roster,
        active,
        item_changes,
        tick,
    )| {
        let session = together.current();
        async move {
            let _ = (roster, item_changes, tick);
            let (true, Some(session), false) = (active, session, ids.is_empty()) else {
                return Vec::new();
            };
            session.seen(ids).await.unwrap_or_else(|e| {
                platform::warn(&format!("pharos: group seen failed: {}", e.0));
                Vec::new()
            })
        }
    }));
    let seen = found.read().clone().unwrap_or_default();
    seen
}

/// A show page with the group's marks on it.
#[component]
fn SeenBrowse(browse: browse::Browse) -> Element {
    let ids = match &browse {
        browse::Browse::Show(show) => browse::seen_ids(show),
        _ => Vec::new(),
    };
    let seen = use_group_seen(ids);
    rsx! { browse::BrowseLayout { browse, seen } }
}

/// A film or episode page with the group's marks on it.
#[component]
fn SeenTitle(detail: Detail, on_watch_together: EventHandler<(ItemRef, u64)>) -> Element {
    let seen = use_group_seen(vec![detail.item.id]);
    let PlayFromStart(on_play_from_start) = use_context::<PlayFromStart>();
    rsx! { TitleView { detail, on_watch_together, on_play_from_start, seen } }
}

#[component]
fn TitleScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    let nav = use_context::<Nav>();
    let together = use_context::<Together>();
    // A failure shown here is not news to the next screen.
    use_drop({
        let mut error = together.error;
        move || error.set(None)
    });
    // The screen stays mounted when one title page opens another (a search
    // hit, a neighbour), so the load follows `id` instead of running once
    // (B555). The resource keeps its last value while a new id loads, so
    // each result carries the id it was asked for and a stale one shows as
    // loading, never as the previous title's controls.
    let detail = use_resource(use_reactive!(|(id,)| {
        let api = ctx.api.read().clone();
        let _ = (ctx.marks)();
        async move {
            let api = api?;
            Some((
                id,
                settle(ctx.machine, api.detail(&call("title", "load"), id).await),
            ))
        }
    }));
    // Start a group on this title where the viewer chose, then watch it as
    // its member.
    let group_error = together.error;
    let on_watch_together = move |(id, from_ms): (ItemRef, u64)| {
        let Some(api) = ctx.api.peek().clone() else {
            return;
        };
        let session = together.session(&api);
        let mut group_error = group_error;
        group_error.set(None);
        platform::info(&format!("pharos: group create item={id} from_ms={from_ms}"));
        spawn_forever(async move {
            match session.create(id, from_ms).await {
                Ok(_) => nav.0.call(Route::Watch(id)),
                Err(e) => group_error.set(Some(e.0)),
            }
        });
    };
    let state = detail.read();
    let name = state
        .as_ref()
        .and_then(Option::as_ref)
        .filter(|(asked, _)| *asked == id)
        .and_then(|(_, r)| r.as_ref().ok())
        .map(|d| super::views::page_name(&d.item));
    use_page_name(id, name);
    rsx! {
        if let Some(message) = group_error.read().clone() {
            ErrorNote { message }
        }
        {loaded(
            state
                .as_ref()
                .and_then(Option::as_ref)
                .filter(|(asked, _)| *asked == id)
                .map(|(_, result)| result),
            |detail: &Detail| {
                rsx! {
                    SeenTitle {
                        detail: detail.clone(),
                        on_watch_together: on_watch_together.clone(),
                    }
                }
            },
        )}
    }
}

/// A negotiated playback, ready for the player.
struct Playing {
    title: String,
    kind: ItemKind,
    image: Option<String>,
    url: String,
    start_ms: u64,
    engine: Engine,
    /// The server's play session; a new one remounts the player.
    session: String,
    /// Reports and the stop go through this. It dies with the connection,
    /// so a reconnect swaps in a fresh one under the same player.
    grant: RefCell<NativeGrant>,
    /// The API generation `grant` was negotiated on.
    grant_gen: Cell<u32>,
    detail: Detail,
    /// The tracks the server started this playback on.
    audio: Option<u32>,
    subtitle: Option<u32>,
    /// The audio stream this playback was asked for. The server may play
    /// another, so `audio` alone cannot tell a request was already made.
    asked_audio: Option<u32>,
}

impl Playing {
    /// Whether the stream has its subtitle drawn in by the server.
    fn burned(&self) -> bool {
        self.subtitle.is_some_and(|i| {
            self.detail
                .subtitle_tracks
                .iter()
                .any(|t| t.index == i && t.vtt_url.is_none() && t.images_url.is_none())
        })
    }
}

/// The playback props, compared by identity: a new negotiation is a new
/// player.
#[derive(Clone)]
struct PlayingRef(Rc<Playing>);

impl PartialEq for PlayingRef {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

#[component]
fn WatchScreen(id: ItemRef, slot: Slot) -> Element {
    let ctx = use_context::<Ctx>();
    let replace = use_context::<Replace>();
    let StageCtx(mut ctx_stage) = use_context::<StageCtx>();
    // A song that follows one handing over fades in as that one fades out.
    // Taken by the first player mounted, so a renegotiated one starts as is.
    let fade_in = use_hook(|| {
        FadeIn(Rc::new(Cell::new(
            ctx_stage.peek().outgoing().map_or(0, |(h, _)| h.fade_ms),
        )))
    });
    // Where the player was, so a renegotiation after a reconnect or a track
    // change carries on from there rather than from the stored position.
    let last_ms = use_hook(|| Rc::new(Cell::new(0u64)));
    // The tracks asked for; changing them negotiates a new playback.
    let mut picks = use_signal(TrackPicks::default);
    // What the connection has shown about this video, kept across the
    // reloads its own decisions cause; the cap it asks for rides the next
    // negotiation.
    let watch = use_hook(|| {
        let ceiling = stored_bitrate(MAX_BITRATE_KEY, parse_max_bitrate);
        let rung = stored_bitrate(quality::AUTO_RUNG_KEY, quality::parse_rung);
        let w = Watch::new(ceiling, rung, now_ms_u64());
        QualityWatch(Rc::new(RefCell::new(w)))
    });
    let mut auto_cap = use_signal(|| watch.0.borrow().cap());
    let resume_from = Rc::clone(&last_ms);
    // The episodes either side follow the item, not the playback, so a track
    // change that renegotiates does not look them up again.
    let near = use_resource(move || {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            let info = call("watch", "neighbours");
            let result = async {
                let episode = api.detail(&info, id).await?.item;
                // An episode's neighbours are in its series, a track's on its album.
                match episode.kind {
                    ItemKind::Episode => {
                        Ok((browse::neighbours(&api, &info, &episode).await?, None))
                    }
                    ItemKind::Audio => {
                        let (near, rest) =
                            browse::neighbours_and_rest(&api, &info, &episode).await?;
                        Ok((near, Some(rest)))
                    }
                    _ => Ok((Neighbours::default(), None)),
                }
            }
            .await;
            result
                .map_err(|e: ClientError| {
                    platform::warn(&format!("pharos: episode neighbours unavailable: {e}"))
                })
                .ok()
        }
    });
    // A song that starts takes itself out of the queue, or seeds it with the
    // rest of its album (B435/V375); once per song, as the screen remounts
    // per item.
    let queue = try_use_context::<QueueCtx>();
    let start_over = try_use_context::<StartOver>();
    // The item this screen was asked to start over, kept past the press so a
    // renegotiation before the player reports a position still starts at zero.
    let started_over = use_hook(|| Rc::new(Cell::new(None::<ItemRef>)));
    let mix_ctx = try_use_context::<MixCtx>();
    let mut seeded = use_signal(|| false);
    use_effect(move || {
        let read = near.read();
        // Still loading.
        let Some(looked_up) = read.as_ref() else {
            return;
        };
        if *seeded.peek() {
            return;
        }
        seeded.set(true);
        let Some(QueueCtx(mut queue)) = queue else {
            return;
        };
        let rest = match looked_up {
            Some((_, Some(rest))) => Some(rest.as_slice()),
            // Not a song.
            Some((_, None)) => return,
            // The album could not be read; a song played from the queue
            // still leaves it.
            None if queue.peek().expects(id) => None,
            None => return,
        };
        // While a mix is on it decides what follows, not the song's album.
        let mixing = mix_ctx.is_some_and(|m| m.0.peek().is_some());
        let rest = mixes::album_rest(mixing, rest);
        queue.write().started(id, rest, seed());
        let q = queue.peek();
        platform::info(&format!(
            "pharos: queue started item={id} album_rest={} upcoming={} shuffled={}",
            rest.map_or_else(|| "unread".to_owned(), |r| r.len().to_string()),
            q.upcoming().len(),
            q.shuffled()
        ));
    });
    // A reconnect replaces the API but must not renegotiate: that would mount
    // a new player mid-song. Only gaining or losing the API does.
    let connected = use_memo(move || ctx.api.read().is_some());
    // Counts the APIs seen, so a grant can tell whether its socket is the
    // current one.
    let api_count = use_hook(|| Rc::new(Cell::new(0u32)));
    let api_gen = use_memo(move || {
        ctx.api.read();
        api_count.set(api_count.get().wrapping_add(1));
        api_count.get()
    });
    // The API generation the latest negotiation started on.
    let negotiated_gen = use_hook(|| Rc::new(Cell::new(0u32)));
    let negotiated_at = Rc::clone(&negotiated_gen);
    let mut playing = use_resource(move || {
        connected();
        let api = ctx.api.peek().clone();
        let grant_gen = *api_gen.peek();
        negotiated_at.set(grant_gen);
        let resume_from = Rc::clone(&resume_from);
        let started_over = Rc::clone(&started_over);
        let asked = picks();
        let auto = auto_cap();
        // A song Previous went back to starts over (B464/V406), as does an
        // item Play from start was pressed on (B500). The press is spent only
        // by a negotiation that runs, so a load before sign-in keeps it; this
        // screen then remembers it until the player has a position.
        let previous = queue.is_some_and(|QueueCtx(q)| q.peek().plays_from_start(id));
        let pressed = api.is_some()
            && start_over.is_some_and(|StartOver(mut asked)| {
                let hit = *asked.peek() == Some(id);
                if hit {
                    asked.set(None);
                    started_over.set(Some(id));
                }
                hit
            });
        let pressed = pressed || started_over.get() == Some(id);
        let from_start = previous || pressed;
        let reason = if pressed { "start_over" } else { "previous" };
        async move {
            let api = api?;
            // A play outside the full view is the stage's (spec 020 FR-010).
            let info = call("watch", play_action(ctx_stage.peek().view));
            let result = async {
                let detail = api.detail(&info, id).await?;
                let caps = negotiation_capabilities(detail.item.kind, auto);
                let (decision, grant) = api.play(&info, id, &caps, asked).await?;
                Ok::<_, ClientError>((detail, decision, grant))
            }
            .await;
            Some(
                settle(ctx.machine, result).and_then(|(detail, decision, grant)| {
                    let engine = choose_engine(
                        decision.is_hls(),
                        platform::plays_hls(),
                        platform::has_mse(),
                    );
                    platform::info(&format!(
                        "pharos: playback method={} engine={engine:?} block={} downgrade={} audio={:?} subtitle={:?} url={}",
                        decision.method,
                        decision.direct_play_block,
                        decision.downgrade,
                        decision.audio,
                        decision.subtitle,
                        decision.stream_url
                    ));
                    if engine == Engine::Unsupported {
                        return Err(
                            "This title needs HLS, which this browser cannot play.".to_owned()
                        );
                    }
                    let start_ms = match resume_from.get() {
                        0 if from_start => {
                            platform::info(&format!(
                                "pharos: playback from_start item={id} reason={reason} saved_ms={}",
                                detail.item.position_ms
                            ));
                            0
                        }
                        0 => start_ms(detail.item.position_ms, detail.item.played),
                        here => here,
                    };
                    Ok(PlayingRef(Rc::new(Playing {
                        title: super::views::display_name(&detail.item),
                        kind: detail.item.kind,
                        image: detail.item.image.clone(),
                        url: decision.stream_url,
                        start_ms,
                        engine,
                        session: decision.play_session,
                        grant: RefCell::new(grant),
                        grant_gen: Cell::new(grant_gen),
                        detail,
                        audio: decision.audio,
                        subtitle: decision.subtitle,
                        asked_audio: match asked.audio {
                            TrackChoice::Index(i) => Some(i),
                            _ => None,
                        },
                    })))
                }),
            )
        }
    });
    // The next episode's name and still, for the "Up next" card. Only an
    // episode shows one: a song's next track would cost a detail call for
    // nothing.
    let upcoming = use_resource(move || {
        let api = ctx.api.read().clone();
        let episode = matches!(
            playing.read().as_ref(),
            Some(Some(Ok(p))) if p.0.kind == ItemKind::Episode
        );
        let next = near
            .read()
            .as_ref()
            .and_then(|n| n.as_ref())
            .and_then(|(n, _)| n.next)
            .filter(|_| episode);
        async move {
            let (api, next) = (api?, next?);
            api.detail(&call("watch", "up next"), next)
                .await
                .map(|d| d.item)
                .map_err(|e| platform::warn(&format!("pharos: up next unavailable: {e}")))
                .ok()
        }
    });
    // When the grant is from an older API (a reconnect, even one during the
    // first negotiation), the stream keeps going and only the grant is
    // negotiated again.
    use_effect(move || {
        let gen = api_gen();
        let current = playing.read().clone();
        let p = match current {
            Some(Some(Ok(p))) => p,
            // A first negotiation that failed, or is still waiting, on a
            // socket that has since been replaced starts again on the new
            // one, once per connection.
            None | Some(Some(Err(_))) => {
                if negotiated_gen.get() != gen {
                    playing.restart();
                }
                return;
            }
            Some(None) => return,
        };
        if p.0.grant_gen.replace(gen) == gen {
            return;
        }
        let Some(api) = ctx.api.peek().clone() else {
            return;
        };
        let asked = *picks.peek();
        let kind = p.0.kind;
        spawn(async move {
            let info = call("watch", "renew");
            let mut wait_ms = RENEW_FIRST_WAIT_MS;
            for attempt in 1..=RENEW_ATTEMPTS {
                // A newer reconnect has taken over.
                if *api_gen.peek() != gen {
                    return;
                }
                match api.play(&info, id, &negotiation_capabilities(kind, *auto_cap.peek()), asked).await {
                    Ok((decision, grant)) if *api_gen.peek() == gen => {
                        *p.0.grant.borrow_mut() = grant;
                        // The play session id is a stream credential (B256).
                        platform::info(&format!(
                            "pharos: playback grant renewed after reconnect method={} attempt={attempt}",
                            decision.method
                        ));
                        return;
                    }
                    Ok(_) => return,
                    Err(_) if *api_gen.peek() != gen => return,
                    Err(e) if attempt == RENEW_ATTEMPTS => platform::warn(&format!(
                        "pharos: playback grant renewal failed, reports stop attempts={attempt}: {e}"
                    )),
                    Err(e) => {
                        platform::warn(&format!(
                            "pharos: playback grant renewal retry attempt={attempt}: {e}"
                        ));
                        platform::sleep_ms(wait_ms).await;
                        wait_ms *= 2;
                    }
                }
            }
        });
    });
    // Once this playback is settled (or has failed), the song handing over
    // to it can go: the screen shows this one from here on.
    use_effect(move || {
        if !playing.read().as_ref().is_some_and(Option::is_some) {
            return;
        }
        let Slot::Current { play, .. } = slot else {
            return;
        };
        if ctx_stage.peek().outgoing().is_some() {
            ctx_stage.write().ready(play);
        }
    });
    let state = playing.read();
    // The song handing over is not the page's.
    let name = state
        .as_ref()
        .and_then(Option::as_ref)
        .and_then(|r| r.as_ref().ok())
        .filter(|_| matches!(slot, Slot::Current { .. }))
        .map(|p| super::views::page_name(&p.0.detail.item));
    use_page_name(id, name);
    // The stage bar, and its Close, comes with a loaded player; a compact
    // window still loading or failed needs a way out of its own.
    let stranded = ctx_stage.read().view != View::Full
        && !matches!(state.as_ref().and_then(Option::as_ref), Some(Ok(_)));
    let body = loaded(state.as_ref().and_then(Option::as_ref), |p: &PlayingRef| {
        remount_on(
            &p.0.session,
            rsx! {
                Player {
                    playing: p.clone(),
                    last_ms: LastMs(Rc::clone(&last_ms)),
                    near: near.read().as_ref().and_then(|n| n.as_ref()).map(|n| n.0).unwrap_or_default(),
                    upcoming: upcoming.read().clone().flatten(),
                    on_renegotiate: move |next| picks.set(next),
                    // In place of this episode, as the next-episode button does.
                    on_advance: move |next| replace.0.call(Route::Watch(next)),
                    watch: watch.clone(),
                    on_quality: move |cap| {
                        store_bitrate(quality::AUTO_RUNG_KEY, cap);
                        auto_cap.set(cap);
                    },
                    slot,
                    fade_in: fade_in.clone(),
                }
            },
        )
    });
    rsx! {
        {body}
        if stranded {
            StageClose { item: id }
        }
    }
}

/// Mounts `body` afresh whenever `key` changes, dropping its hooks. Dioxus
/// compares keys only between siblings of a list, so a lone keyed node keeps
/// its old hooks and the new props never reach them; a one-item list does.
pub(super) fn remount_on(key: &str, body: Element) -> Element {
    rsx! {
        for key in std::iter::once(key) {
            Fragment { key: "{key}", {body.clone()} }
        }
    }
}

/// Tries at renewing a playback grant after a reconnect, the first retry
/// after [`RENEW_FIRST_WAIT_MS`] and each later one twice as long (1, 2, 4 s):
/// enough to ride out a server that is still starting.
const RENEW_ATTEMPTS: u32 = 4;
const RENEW_FIRST_WAIT_MS: u32 = 1000;

/// How long the screen's first player fades in from silence, in ms, as the
/// song before fades out; taken (left 0) by the player that uses it.
#[derive(Clone)]
struct FadeIn(Rc<Cell<u64>>);

impl PartialEq for FadeIn {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// The connection history of the video on screen, shared with its player.
#[derive(Clone)]
struct QualityWatch(Rc<RefCell<Watch>>);

impl PartialEq for QualityWatch {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// The player's last known position, shared with its screen.
#[derive(Clone)]
struct LastMs(Rc<Cell<u64>>);

impl PartialEq for LastMs {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// The `ui_action` of a play RPC made in `view`.
fn play_action(view: View) -> &'static str {
    match view {
        View::Full => "play",
        View::Floating => "stage_floating",
        View::Mini => "stage_mini",
    }
}

/// The wall clock in unix milliseconds, as the activity log stamps its lines.
fn now_ms_u64() -> u64 {
    u64::try_from(platform::unix_ms()).unwrap_or(0)
}

/// A shuffle seed: a play order needs no more randomness than the clock.
fn seed() -> u64 {
    platform::now_ms().to_bits()
}

fn choice(index: Option<u32>) -> TrackChoice {
    index.map_or(TrackChoice::Off, TrackChoice::Index)
}

/// The link that joins group `id`: the invite route on this server.
fn invite_url(origin: &str, id: &str) -> String {
    format!("{origin}{}", Route::TogetherGroup(id.to_owned()).path())
}

/// Attaches the engine, draws the controls and reports progress: on play,
/// pause and seek, every 10 s while playing, and one stop when the video
/// ends, the player unmounts or the page is hidden. A track change the
/// `<video>` cannot make itself asks the screen for a new playback.
#[component]
fn Player(
    playing: PlayingRef,
    last_ms: LastMs,
    near: Neighbours,
    upcoming: Option<Item>,
    on_renegotiate: EventHandler<TrackPicks>,
    /// Plays the next episode once this one is finished: Up next, or the
    /// video ending.
    on_advance: EventHandler<ItemRef>,
    watch: QualityWatch,
    on_quality: EventHandler<u64>,
    slot: Slot,
    fade_in: FadeIn,
) -> Element {
    let ctx = use_context::<Ctx>();
    let p = playing.0;
    // Handing over: the song is done with, so nothing it does is reported
    // or acted on any more.
    let retiring = use_hook(|| Rc::new(Cell::new(false)));
    // The automatic quality has decided; one decision per video, whichever
    // path (fragment or media event) reaches it first.
    let decided = use_hook(|| Rc::new(Cell::new(false)));
    // The crossfade into the next song has begun (or there is none to make).
    let crossfading = use_hook(|| Rc::new(Cell::new(false)));
    // This song has handed the stage to the next under a crossfade: it plays
    // out until the next is ready, but its end no longer moves the queue.
    let handed_off = use_hook(|| Rc::new(Cell::new(false)));
    let fade_in_cancel = use_hook(|| Rc::new(Cell::new(false)));
    let resume: ResumeSlot = use_hook(|| Rc::new(RefCell::new((0, None))));
    let fade_in_ms = use_hook(|| fade_in.0.take());
    let clock = use_hook(|| Rc::new(RefCell::new(ReportClock::new())));
    let engine = use_hook(|| Rc::new(RefCell::new(None::<Box<dyn PlaybackEngine>>)));
    let nav = use_context::<Nav>();
    let replace = use_context::<Replace>();
    let together = use_context::<Together>();
    let group = (together.view)();
    let watch_seen = use_group_seen(vec![p.detail.item.id]);
    // The session is read when used: a join can make it after this mounted.
    let session = {
        let together = together.clone();
        move || together.current()
    };
    // A group start from this player is on its way; a second click waits.
    let starting = use_hook(|| Rc::new(Cell::new(false)));
    // The group whose invite panel is open: it opens by itself once a group
    // started from here is up, and a group left or replaced closes it.
    let invite_for = use_signal(|| None::<String>);
    // What the last invite copy did, for the invite panel.
    let invite_note = use_signal(|| None::<String>);
    let invite_open = group.active && group.id.is_some() && *invite_for.read() == group.id;
    let invite = group
        .id
        .as_ref()
        .filter(|_| group.active)
        .map(|id| invite_url(&platform::page_origin(), id));
    // Bumped when the toast's time is up, so the page draws without it.
    let toast_expired = use_signal(|| 0u32);
    let _ = toast_expired();
    let timed_toast = use_hook(|| Rc::new(Cell::new(None::<u64>)));
    use_effect(move || {
        let view = (together.view)();
        if let Some(at) = view.log.toast_to_time(timed_toast.get(), now_ms_u64()) {
            timed_toast.set(Some(at));
            let mut expired = toast_expired;
            spawn(async move {
                platform::sleep_ms(u32::try_from(activity::TOAST_MS).unwrap_or(u32::MAX)).await;
                expired += 1;
            });
        }
    });
    let mut failed = use_signal(|| None::<String>);
    let saved_volume = use_hook(last_volume);
    let mut media = use_signal(|| MediaState::at_volume(saved_volume.map(|(v, m, _)| (v, m))));
    let mut hover_ms = use_signal(|| None::<u64>);
    // The subtitle on screen: the server's pick, then the viewer's.
    let mut subtitle = use_signal(|| p.subtitle);
    // The subtitle the viewer switched to in place, which reports carry so
    // the next playback starts on it.
    let switched = use_hook(|| Rc::new(Cell::new(None::<TrackChoice>)));
    let mut up_next = use_signal(UpNext::default);
    let mut up_next_step = use_signal(|| UpNextStep::Hidden);
    let advanced = use_hook(|| Rc::new(Cell::new(false)));
    // An ASS subtitle drawn from its script (ADR-0057), and the pick it is
    // for; its WebVTT stays on screen until the renderer is ready.
    let styled = use_hook(|| Rc::new(RefCell::new(None::<platform::StyledSubtitles>)));
    let styled_for = use_hook(|| Rc::new(Cell::new(0_u64)));
    let mut styled_ready = use_signal(|| false);
    // A plain text subtitle drawn by the Rust renderer (spec 026), and the
    // pick it is for; its `<track>` stays loaded but hidden, and shows if the
    // renderer fails.
    let rust = use_hook(|| Rc::new(RefCell::new(None::<platform::RustSubtitles>)));
    let rust_for = use_hook(|| Rc::new(Cell::new(0_u64)));
    let mut rust_state = use_signal(|| RustState::Off);
    let StageCtx(stage) = use_context::<StageCtx>();
    // The view alone, so a drag (which moves the spot) does not redraw the
    // player.
    let view = use_memo(move || stage.read().view)();
    let own_queue = use_signal(Queue::default);
    let mut queue = try_use_context::<QueueCtx>().map_or(own_queue, |q| q.0);
    let audio = p.kind == ItemKind::Audio;
    let playing_id = p.detail.item.id;
    // A song's next comes from the queue, unless a group decides what plays.
    // A session is kept after its group ends, so ask the group, not it.
    let grouped = group.drives(playing_id);
    let queued = audio && !grouped;
    let next = if queued {
        queue.read().next_after(playing_id, near.next)
    } else {
        near.next
    };
    // Play `to` after this song. The same song again (queued twice) is the
    // same route, which mounts nothing new, so it plays again in place.
    let go = move |to: ItemRef| {
        if queued && to == playing_id {
            // A copy, so `go` is `Fn` and every handler can hold it.
            let mut queue = queue;
            queue.write().started(to, None, seed());
            platform::info(&format!("pharos: queue replay item={to}"));
            platform::seek_ms(VIDEO_ID, 0);
            platform::play(VIDEO_ID);
        } else {
            if queued {
                // A copy, so `go` stays `Fn`. A cut: the song stops once the
                // next is ready, and shows until then.
                let mut stage = stage;
                stage.write().hand_off(playing_id, 0);
            }
            replace.0.call(Route::Watch(to));
        }
    };

    let lyrics = use_resource({
        let (id, kind) = (p.detail.item.id, p.kind);
        move || {
            let api = ctx.api.read().clone();
            async move {
                if kind != ItemKind::Audio {
                    return Ok(None);
                }
                // No connection is no lookup, so not "no lyrics" either (V423).
                let Some(api) = api else {
                    platform::warn("pharos: lyrics unavailable: no native api");
                    return Err(());
                };
                // A failed lookup is not "no lyrics": the chip says which.
                // Karaoke asks for word times (B499, ADR-0066), as the
                // listener left it when the song started.
                let want_words = super::listen::karaoke_on();
                match api.lyrics(&call("watch", "lyrics"), id, want_words).await {
                    Ok(lyrics) => {
                        platform::info(&format!(
                            "pharos: lyrics lines={} synced={}",
                            lyrics.lines.len(),
                            lyrics.synced
                        ));
                        Ok(Some(lyrics))
                    }
                    Err(e) => {
                        platform::warn(&format!("pharos: lyrics unavailable: {e}"));
                        Err(())
                    }
                }
            }
        }
    });

    // Word times the aligner was still working out (B499) arrive on a later
    // ask; the lyrics shown meanwhile keep their estimate. Kept with the
    // song they are for, so they never outlive it on screen.
    let mut aligned = use_signal(|| None::<(ItemRef, pharos_client::api::Lyrics)>);
    use_effect({
        let id = p.detail.item.id;
        move || {
            let pending = matches!(&*lyrics.read(), Some(Ok(Some(l))) if l.words_pending);
            if !pending {
                return;
            }
            let Some(api) = ctx.api.peek().clone() else {
                return;
            };
            spawn(async move {
                for _ in 0..ALIGN_ASKS {
                    platform::sleep_ms(ALIGN_ASK_MS).await;
                    match api.lyrics(&call("watch", "lyrics"), id, true).await {
                        Ok(next) if next.words_pending => {}
                        // The aligner failed or found no words: keep the
                        // estimate already on screen.
                        Ok(next) if next.lines.iter().all(|l| l.words.is_empty()) => {
                            platform::info("pharos: lyrics word times not found");
                            return;
                        }
                        Ok(next) => {
                            platform::info("pharos: lyrics word times arrived");
                            aligned.set(Some((id, next)));
                            return;
                        }
                        Err(e) => {
                            platform::warn(&format!("pharos: lyrics word times unavailable: {e}"));
                            return;
                        }
                    }
                }
                platform::info("pharos: lyrics word times still pending, gave up");
            });
        }
    });

    let stop = {
        let (p, clock, last_ms) = (Rc::clone(&p), Rc::clone(&clock), Rc::clone(&last_ms.0));
        move || {
            if !clock.borrow_mut().stop() {
                return;
            }
            let grant = p.grant.borrow().clone();
            let at = last_ms.get();
            platform::spawn_detached(async move {
                match grant.stop(&call("watch", "stop"), at).await {
                    Ok(()) => {}
                    // A reload or reconnect closed the playback's connection
                    // first; the server ends a playback whose connection drops.
                    Err(e) if e.is_disconnected() => platform::info(&format!(
                        "pharos: playback stop skipped reason=disconnected detail={e}"
                    )),
                    Err(e) => platform::warn(&format!("pharos: playback stop failed: {e}")),
                }
            });
        }
    };
    use_hook({
        let stop = stop.clone();
        move || Rc::new(platform::on_page_hide(stop))
    });
    let retire_stop = stop.clone();
    // The group's track picks arrive from outside the Dioxus runtime; this
    // task switches the player's tracks.
    let group_tracks = use_coroutine({
        let (p, switched) = (Rc::clone(&p), Rc::clone(&switched));
        move |mut rx: UnboundedReceiver<track_follow::TrackPicks>| {
            let (p, switched) = (Rc::clone(&p), Rc::clone(&switched));
            async move {
                while let Some(picks) = rx.next().await {
                    let audio =
                        track_follow::audio_to_renegotiate(picks.audio, p.audio, p.asked_audio);
                    let to = picks.subtitle.filter(|s| *subtitle.peek() != *s);
                    let burn = to.is_some_and(|to| {
                        let track =
                            to.and_then(|i| p.detail.subtitle_tracks.iter().find(|t| t.index == i));
                        subtitle_switch(p.burned(), track) == SubtitleSwitch::Renegotiate
                    });
                    let verdict = if audio.is_some() || burn {
                        "renegotiate"
                    } else if to.is_some() {
                        "in_place"
                    } else {
                        "none"
                    };
                    platform::info(&format!(
                        "pharos: group tracks player verdict={verdict} audio_pick={:?} playing={:?} asked={:?} subtitle_to={to:?} burn={burn}",
                        picks.audio, p.audio, p.asked_audio
                    ));
                    if audio.is_some() || burn {
                        on_renegotiate.call(TrackPicks {
                            audio: audio
                                .or(p.audio)
                                .map_or(TrackChoice::Default, TrackChoice::Index),
                            subtitle: choice(to.unwrap_or(*subtitle.peek())),
                        });
                    } else if let Some(to) = to {
                        switched.set(Some(choice(to)));
                        subtitle.set(to);
                    }
                }
            }
        }
    });
    // The group drives this video while it is on the stage.
    let group_video: Rc<dyn GroupPlayer> = use_hook({
        let together = together.clone();
        let item = p.detail.item.id;
        let (audio, subtitles) = (
            p.detail.audio_tracks.clone(),
            p.detail.subtitle_tracks.clone(),
        );
        move || {
            let video: Rc<dyn GroupPlayer> = Rc::new(VideoPlayer {
                id: VIDEO_ID,
                audio,
                subtitles,
                tracks: group_tracks,
            });
            together.attach(item, Rc::clone(&video));
            video
        }
    });
    use_drop({
        let together = together.clone();
        move || together.detach(&group_video)
    });
    use_drop({
        let (engine, stop) = (Rc::clone(&engine), stop.clone());
        let (styled, styled_for) = (Rc::clone(&styled), Rc::clone(&styled_for));
        let (rust, rust_for) = (Rc::clone(&rust), Rc::clone(&rust_for));
        move || {
            // A renderer still loading must not attach once the player is gone.
            styled_for.set(styled_for.get().wrapping_add(1));
            styled.borrow_mut().take();
            rust_for.set(rust_for.get().wrapping_add(1));
            rust.borrow_mut().take();
            engine.borrow_mut().take();
            stop();
        }
    });
    // Auto quality judges video played by hls.js, the one engine that shows
    // the downloads; a change reloads at the position, as a track change does.
    let auto = p.engine == Engine::HlsJs && p.kind != ItemKind::Audio;
    use_hook(|| {
        if auto {
            watch.0.borrow_mut().restarted(now_ms_u64());
        }
    });
    let cap_changes = use_coroutine(move |mut rx: UnboundedReceiver<u64>| async move {
        while let Some(cap) = rx.next().await {
            on_quality.call(cap);
        }
    });
    let assess: Rc<dyn Fn()> = {
        let (watch, retiring, decided) = (watch.clone(), Rc::clone(&retiring), Rc::clone(&decided));
        Rc::new(move || {
            if !auto || retiring.get() || decided.get() {
                return;
            }
            let Some(c) = watch.0.borrow_mut().evaluate(now_ms_u64()) else {
                return;
            };
            decided.set(true);
            platform::info(&format!(
                "pharos: quality auto from={} to={} reason={} estimate_bps={}",
                c.from_bps,
                c.to_bps,
                c.reason.label(),
                c.estimate_bps
            ));
            cap_changes.send(c.cap);
        })
    };
    // The engines call back from browser events, outside the Dioxus runtime.
    let fatal = use_coroutine({
        let stop = stop.clone();
        move |mut reasons: UnboundedReceiver<String>| {
            let stop = stop.clone();
            async move {
                while let Some(reason) = reasons.next().await {
                    stop();
                    failed.set(Some(format!("Playback failed: {reason}")));
                }
            }
        }
    });
    // Before the stream attaches, so autoplay never starts at full volume.
    use_effect(move || restore_volume(saved_volume));
    use_effect({
        let cancel = Rc::clone(&fade_in_cancel);
        move || {
            if fade_in_ms == 0 {
                return;
            }
            let to = platform::media_state(VIDEO_ID).volume;
            platform::info(&format!(
                "pharos: crossfade in item={playing_id} fade_ms={fade_in_ms} volume={to:.3}"
            ));
            platform::set_volume(VIDEO_ID, 0.0, platform::media_state(VIDEO_ID).muted);
            spawn(fade(VIDEO_ID, 0.0, to, fade_in_ms, Rc::clone(&cancel)));
        }
    });
    use_effect({
        let (p, engine) = (Rc::clone(&p), Rc::clone(&engine));
        let (watch, assess) = (watch.clone(), Rc::clone(&assess));
        let (retiring, decided) = (Rc::clone(&retiring), Rc::clone(&decided));
        move || {
            let (p, engine) = (Rc::clone(&p), Rc::clone(&engine));
            let (watch, assess) = (watch.clone(), Rc::clone(&assess));
            let (retiring, decided) = (Rc::clone(&retiring), Rc::clone(&decided));
            spawn(async move {
                let on_fatal: OnFatal = Rc::new(move |reason| fatal.send(reason));
                let on_fragment: OnFragment = {
                    let (watch, assess) = (watch.clone(), Rc::clone(&assess));
                    Rc::new(move |load| {
                        // Handing over, or already decided: no more downloads count.
                        if retiring.get() || decided.get() {
                            return;
                        }
                        watch
                            .0
                            .borrow_mut()
                            .fragment(load.bytes, load.load_ms, load.media_ms);
                        assess();
                    })
                };
                let attached = match platform::engine(p.engine).await {
                    Ok(mut e) => e
                        .attach(VIDEO_ID, &p.url, p.start_ms, on_fatal, on_fragment)
                        .map(|()| e),
                    Err(e) => Err(e),
                };
                match attached {
                    Ok(e) => *engine.borrow_mut() = Some(e),
                    Err(e) => failed.set(Some(format!("Could not start playback: {e}"))),
                }
            });
        }
    });
    // A burned subtitle is in the picture, not a text track.
    let burned = p.burned();
    // The renderer calls back from worker events, outside the Dioxus runtime.
    let styled_events = use_coroutine({
        let (styled, styled_for) = (Rc::clone(&styled), Rc::clone(&styled_for));
        move |mut events: UnboundedReceiver<(u64, Result<(), String>)>| {
            let (styled, styled_for) = (Rc::clone(&styled), Rc::clone(&styled_for));
            async move {
                while let Some((pick, result)) = events.next().await {
                    if pick != styled_for.get() {
                        continue;
                    }
                    let (line, event) = styled_subtitle_signals(&result);
                    platform::report_event(event);
                    match result {
                        Ok(()) => {
                            platform::info(&line);
                            styled_ready.set(true);
                        }
                        Err(_) => {
                            platform::warn(&line);
                            styled.borrow_mut().take();
                            styled_ready.set(false);
                        }
                    }
                }
            }
        }
    });
    use_effect({
        let (p, styled, styled_for) = (Rc::clone(&p), Rc::clone(&styled), Rc::clone(&styled_for));
        move || {
            let shown = if burned { None } else { subtitle() };
            let pick = styled_for.get().wrapping_add(1);
            styled_for.set(pick);
            styled.borrow_mut().take();
            styled_ready.set(false);
            let script = shown
                .and_then(|i| p.detail.subtitle_tracks.iter().find(|t| t.index == i))
                .and_then(styled_subtitle_url);
            let Some(url) = script else {
                return;
            };
            let fonts = subtitle_font_urls(&p.detail.fonts, shown.unwrap_or_default());
            let (styled, styled_for) = (Rc::clone(&styled), Rc::clone(&styled_for));
            spawn(async move {
                let report: Rc<dyn Fn(Result<(), String>)> =
                    Rc::new(move |result| styled_events.send((pick, result)));
                match platform::styled_subtitles(VIDEO_ID, &url, &fonts, Rc::clone(&report)).await {
                    Ok(s) if styled_for.get() == pick => *styled.borrow_mut() = Some(s),
                    Ok(_) => {}
                    Err(e) => report(Err(e)),
                }
            });
        }
    });
    // The Rust renderer calls back outside the Dioxus runtime too.
    let rust_events = use_coroutine({
        let (rust, rust_for) = (Rc::clone(&rust), Rc::clone(&rust_for));
        move |mut events: UnboundedReceiver<(u64, FallbackReason)>| {
            let (rust, rust_for) = (Rc::clone(&rust), Rc::clone(&rust_for));
            async move {
                while let Some((pick, _reason)) = events.next().await {
                    if pick == rust_for.get() {
                        rust.borrow_mut().take();
                        rust_state.set(RustState::Failed);
                    }
                }
            }
        }
    });
    use_effect({
        let (p, rust, rust_for) = (Rc::clone(&p), Rc::clone(&rust), Rc::clone(&rust_for));
        move || {
            let shown = if burned { None } else { subtitle() };
            let pick = rust_for.get().wrapping_add(1);
            rust_for.set(pick);
            rust.borrow_mut().take();
            let url = shown
                .and_then(|i| p.detail.subtitle_tracks.iter().find(|t| t.index == i))
                .and_then(rust_subtitle_url);
            let Some(url) = url else {
                rust_state.set(RustState::Off);
                return;
            };
            rust_state.set(RustState::Starting);
            let (rust, rust_for) = (Rc::clone(&rust), Rc::clone(&rust_for));
            spawn(async move {
                let report: Rc<dyn Fn(FallbackReason)> =
                    Rc::new(move |reason| rust_events.send((pick, reason)));
                let started = platform::rust_subtitles(VIDEO_ID, &url, report).await;
                if rust_for.get() != pick {
                    return;
                }
                match started {
                    Ok(s) => {
                        *rust.borrow_mut() = Some(s);
                        rust_state.set(RustState::Active);
                    }
                    Err(_) => rust_state.set(RustState::Failed),
                }
            });
        }
    });
    use_effect(move || {
        let shown = if burned || styled_ready() {
            None
        } else {
            subtitle()
        };
        let id = shown.map(text_track_id);
        match (id, track_mode(rust_state())) {
            (Some(id), TrackMode::Hidden) => platform::hide_text_track(VIDEO_ID, &id),
            (id, _) => platform::show_text_track(VIDEO_ID, id.as_deref()),
        }
    });

    // Finishing this episode for the next: every road to the next episode
    // that counts as watching this one to its end comes through here.
    let advance = {
        let (p, clock, advanced) = (Rc::clone(&p), Rc::clone(&clock), Rc::clone(&advanced));
        move |next: ItemRef| {
            if advanced.replace(true) {
                return;
            }
            clock.borrow_mut().stop();
            // A stop at the end is how the server marks an item watched,
            // once, whether or not an earlier stop (page hide) went first.
            let at = total_ms(&media.peek(), p.detail.item.runtime_ms);
            let grant = p.grant.borrow().clone();
            platform::spawn_detached(async move {
                if let Err(e) = grant.stop(&call("watch", "finish"), at).await {
                    platform::warn(&format!("pharos: marking watched failed: {e}"));
                }
            });
            on_advance.call(next);
        }
    };
    // Only an episode moves on by itself.
    // The next episode often arrives after the player mounts, and the media
    // handlers can outlive the render that made them, so they read it here.
    let up_next_to = use_hook(|| Rc::new(Cell::new(None::<ItemRef>)));
    // In a group the group decides what plays next, as for a song's queue.
    up_next_to.set(
        near.next
            .filter(|_| p.kind == ItemKind::Episode && !grouped),
    );

    let on_media = {
        let (advance, up_next_to) = (advance.clone(), Rc::clone(&up_next_to));
        let (p, clock, last_ms, switched) = (
            Rc::clone(&p),
            Rc::clone(&clock),
            Rc::clone(&last_ms.0),
            Rc::clone(&switched),
        );
        let session = session.clone();
        let (retiring, crossfading) = (Rc::clone(&retiring), Rc::clone(&crossfading));
        let handed_off = Rc::clone(&handed_off);
        let (watch, assess) = (watch.clone(), Rc::clone(&assess));
        let resume = Rc::clone(&resume);
        move |event: MediaEvent| {
            if retiring.get() {
                return;
            }
            let resumed = match event {
                MediaEvent::Playing => {
                    let now = platform::now_ms();
                    resume.borrow_mut().1.as_mut().and_then(|p| p.playing(now))
                }
                MediaEvent::Pause => resume.borrow_mut().1.as_mut().and_then(ResumeProbe::paused),
                _ => None,
            };
            if let Some(report) = resumed {
                report_resume(&report);
            }
            match event {
                MediaEvent::Waiting => {
                    watch.0.borrow_mut().stalled(now_ms_u64());
                    assess();
                }
                MediaEvent::Seeked => watch.0.borrow_mut().seeked(),
                _ => {}
            }
            if let Some(session) = session() {
                session.on_media(event);
            }
            // Near the end, with a crossfade chosen, the next song starts
            // under this one as it fades.
            if event == MediaEvent::TimeUpdate && queued && !crossfading.get() {
                let state = platform::media_state(VIDEO_ID);
                let fade = crossfade_ms(crossfade_seconds(), state.duration_ms);
                if let (false, Some(fade)) = (state.paused, fade) {
                    if state.position_ms.saturating_add(fade) >= state.duration_ms {
                        crossfading.set(true);
                        let (to, line) = queue.peek().advance(playing_id, near.next);
                        match to {
                            Some(to) if to != playing_id => {
                                platform::info(&format!(
                                    "pharos: crossfade start item={playing_id} next={to} fade_ms={fade} at_ms={}",
                                    state.position_ms
                                ));
                                platform::info(&line);
                                handed_off.set(true);
                                let mut stage = stage;
                                stage.write().hand_off(playing_id, fade);
                                replace.0.call(Route::Watch(to));
                                return;
                            }
                            // The same song again, or the end of the queue:
                            // it plays out and `Ended` decides.
                            Some(same) => platform::info(&format!(
                                "pharos: crossfade skipped item={playing_id} reason=same_song next={same}"
                            )),
                            None => platform::info(&format!(
                                "pharos: crossfade skipped item={playing_id} reason=no_next"
                            )),
                        }
                    }
                }
            }
            // A song that ends plays what is next; in a group the group
            // decides what plays.
            if event == MediaEvent::Ended && queued && handed_off.get() {
                platform::info(&format!(
                    "pharos: queue advance skipped item={playing_id} reason=handed_off"
                ));
            } else if event == MediaEvent::Ended && queued {
                let (to, line) = queue.peek().advance(playing_id, near.next);
                platform::info(&line);
                if let Some(to) = to {
                    go(to);
                }
            }
            let state = platform::media_state(VIDEO_ID);
            media.set(state);
            if let Some(next) = up_next_to.get() {
                let total = total_ms(&state, p.detail.item.runtime_ms);
                let from = up_next_at(&p.detail.segments, total);
                let step = up_next.write().step(state.position_ms, from, total);
                if let Some(why) = up_next.peek().moves_on(step, event) {
                    platform::info(&format!(
                        "pharos: up next: playing {next} ({why}) at={}ms of={total}ms from={from:?}",
                        state.position_ms
                    ));
                    advance(next);
                    return;
                }
                if *up_next_step.peek() != step {
                    up_next_step.set(step);
                }
            }
            if event == MediaEvent::Changed && !burned {
                // Text tracks load with the metadata; show the picked one,
                // unless its script is drawn instead.
                let shown: Option<u32> = if *styled_ready.peek() {
                    None
                } else {
                    *subtitle.peek()
                };
                let id = shown.map(text_track_id);
                match (id, track_mode(*rust_state.peek())) {
                    (Some(id), TrackMode::Hidden) => platform::hide_text_track(VIDEO_ID, &id),
                    (id, _) => platform::show_text_track(VIDEO_ID, id.as_deref()),
                }
            }
            let at = platform::position_ms(VIDEO_ID);
            if at > 0 {
                last_ms.set(at);
            }
            let Some(send) = clock.borrow_mut().on(platform::now_ms(), event) else {
                return;
            };
            if send == Outgoing::Stop {
                stop();
                return;
            }
            let Outgoing::Report { paused } = send else {
                return;
            };
            let grant = p.grant.borrow().clone();
            let at = last_ms.get();
            let tracks = TrackPicks {
                audio: TrackChoice::Default,
                subtitle: switched.get().unwrap_or_default(),
            };
            spawn(async move {
                let result = grant
                    .report(&call("watch", "report"), at, paused, tracks)
                    .await;
                if let Err(e) = settle(ctx.machine, result) {
                    platform::warn(&format!("pharos: progress report failed: {e}"));
                }
            });
        }
    };

    let on_control = {
        let p = Rc::clone(&p);
        let session = session.clone();
        let watch = watch.clone();
        let (retiring, fade_in_cancel) = (Rc::clone(&retiring), Rc::clone(&fade_in_cancel));
        let (together, starting) = (together.clone(), Rc::clone(&starting));
        let invite_link = invite.clone();
        let group_id = group.id.clone().filter(|_| group.active);
        let (advance, up_next_to) = (advance.clone(), Rc::clone(&up_next_to));
        let resume = Rc::clone(&resume);
        move |control: Control| {
            if retiring.get() {
                return;
            }
            // In a group the viewer's play, pause and seek are the group's.
            let seek = |ms: u64| {
                watch.0.borrow_mut().seeking(now_ms_u64());
                if !session().is_some_and(|s| s.seek(ms)) {
                    platform::seek_ms(VIDEO_ID, ms);
                }
            };
            let state = *media.peek();
            let total = match state.duration_ms {
                0 => p.detail.item.runtime_ms,
                d => d,
            };
            match control {
                Control::TogglePlay => {
                    // A group's play waits on the group, and a first play
                    // on a load; neither is a resume of a paused song.
                    let state = platform::media_state(VIDEO_ID);
                    let in_group = session().is_some_and(|s| s.drives());
                    let resuming = (state.paused && state.position_ms > 0 && !in_group)
                        .then(|| start_resume(VIDEO_ID, &resume));
                    if !session().is_some_and(|s| s.toggle_play()) {
                        platform::toggle_play(VIDEO_ID);
                    }
                    if let Some(n) = resuming {
                        spawn(watch_resume(VIDEO_ID, Rc::clone(&resume), n));
                    }
                }
                // An unknown length (0) clamps nothing rather than everything.
                Control::Seek(ms) if total > 0 => seek(ms.min(total)),
                Control::Seek(ms) => seek(ms),
                Control::SeekAt(x) => {
                    if let Some(ms) = position_at(x, platform::element_width(SEEK_ID), total) {
                        seek(ms);
                    }
                }
                Control::Hover(x) => hover_ms
                    .set(x.and_then(|x| position_at(x, platform::element_width(SEEK_ID), total))),
                Control::Volume { volume, muted } => {
                    fade_in_cancel.set(true);
                    platform::set_volume(VIDEO_ID, volume, muted);
                    remember_volume(volume, muted);
                }
                Control::Fullscreen => platform::toggle_fullscreen(PLAYER_ID),
                Control::PictureInPicture => platform::toggle_pip(VIDEO_ID),
                // The next-episode button while the card counts is Play now.
                Control::Play(id)
                    if Some(id) == up_next_to.get()
                        && matches!(*up_next_step.peek(), UpNextStep::Counting { .. }) =>
                {
                    platform::info(&format!("pharos: up next: playing {id} (next button)"));
                    advance(id);
                }
                // In place of this episode, so Back leaves the show rather than
                // replaying every episode watched.
                Control::Play(id) => go(id),
                Control::PlayNext => {
                    if let Some(next) = up_next_to.get() {
                        platform::info(&format!("pharos: up next: playing {next} (play now)"));
                        advance(next);
                    }
                }
                Control::CancelNext => {
                    up_next.write().cancel();
                    up_next_step.set(UpNextStep::Hidden);
                }
                // Start a group on what plays, where it is now: the video on
                // the stage is the session's from the start, so it plays on.
                Control::StartTogether => {
                    let Some(api) = ctx.api.peek().clone() else {
                        return;
                    };
                    if starting.replace(true) {
                        return;
                    }
                    let id = p.detail.item.id;
                    let session = together.session(&api);
                    let from_ms = platform::position_ms(VIDEO_ID);
                    let mut group_error = together.error;
                    group_error.set(None);
                    platform::info(&format!(
                        "pharos: group create item={id} from_ms={from_ms} reason=player"
                    ));
                    let starting = Rc::clone(&starting);
                    let (mut invite_for, mut note) = (invite_for, invite_note);
                    spawn_forever(async move {
                        match session.create(id, from_ms).await {
                            Ok(made) => {
                                note.set(None);
                                invite_for.set(Some(made.id));
                            }
                            Err(e) => {
                                platform::warn(&format!(
                                    "pharos: group create failed item={id} reason=player error={}",
                                    e.0
                                ));
                                group_error.set(Some(e.0));
                            }
                        }
                        starting.set(false);
                    });
                }
                Control::ShowInvite(open) => {
                    let (mut invite_for, mut note) = (invite_for, invite_note);
                    note.set(None);
                    invite_for.set(if open { group_id.clone() } else { None });
                }
                Control::CopyInvite => {
                    let Some(url) = invite_link.clone() else {
                        return;
                    };
                    let mut note = invite_note;
                    spawn(async move {
                        let copied = platform::copy_text(&url).await;
                        platform::info(&format!("pharos: invite copy copied={copied}"));
                        if !copied {
                            platform::select_field(INVITE_LINK_ID);
                        }
                        note.set(Some(
                            if copied {
                                "Link copied"
                            } else {
                                "Press Ctrl+C to copy the link"
                            }
                            .to_owned(),
                        ));
                    });
                }
                // History first, so a song from a mix or the queue goes back
                // to what played; the album's previous track when there is none.
                Control::Previous => {
                    if queued && queue.peek().stepping_back() {
                        platform::info("pharos: previous ignored reason=stepping_back");
                        return;
                    }
                    let (to, reason) = if state.position_ms > RESTART_AFTER_MS {
                        (None, "past_start")
                    } else if let Some(to) = queued
                        .then(|| {
                            let mut queue = queue;
                            let back = queue.write().back(Track::from(&p.detail.item));
                            back
                        })
                        .flatten()
                    {
                        (Some(to), "history")
                    } else if let Some(to) = near.previous {
                        (Some(to), "album")
                    } else {
                        (None, "nothing_before")
                    };
                    platform::info(&format!(
                        "pharos: previous at_ms={} to={} reason={reason}",
                        state.position_ms,
                        to.map_or_else(|| "restart".to_owned(), |id| id.to_string())
                    ));
                    match to {
                        Some(id) => go(id),
                        None => seek(0),
                    }
                }
                // Leaving the watch page shrinks the player (spec 020).
                Control::Back => {
                    if !platform::go_back() {
                        nav.0.call(Route::Home);
                    }
                }
                Control::Shrink => {
                    // A copy, so the handler stays `Fn`.
                    let mut stage = stage;
                    let compact = stage.peek().compact;
                    stage.write().show(compact);
                    platform::info(&format!(
                        "pharos: stage view={} item={playing_id} reason=button",
                        compact.label()
                    ));
                    if !platform::go_back() {
                        nav.0.call(Route::Home);
                    }
                }
                Control::Audio(index) => {
                    let Some(index) = track_follow::audio_pick_here(p.audio, index, |index| {
                        session().is_some_and(|s| s.pick_audio(index))
                    }) else {
                        return;
                    };
                    on_renegotiate.call(TrackPicks {
                        audio: TrackChoice::Index(index),
                        subtitle: choice(*subtitle.peek()),
                    });
                }
                Control::Subtitle(to) => {
                    let here = track_follow::subtitle_pick_here(*subtitle.peek(), to, |to| {
                        session().is_some_and(|s| s.pick_subtitle(to))
                    });
                    if !here {
                        return;
                    }
                    let track =
                        to.and_then(|i| p.detail.subtitle_tracks.iter().find(|t| t.index == i));
                    match subtitle_switch(p.burned(), track) {
                        SubtitleSwitch::InPlace => {
                            switched.set(Some(choice(to)));
                            subtitle.set(to);
                        }
                        SubtitleSwitch::Renegotiate => on_renegotiate.call(TrackPicks {
                            audio: p.audio.map_or(TrackChoice::Default, TrackChoice::Index),
                            subtitle: choice(to),
                        }),
                    }
                }
            }
        }
    };

    // The player on show answers track skips from anywhere in the page
    // (B562): a song's previous goes through `Control::Previous` like its
    // button, an episode's to the one before, and next is what the button
    // plays.
    let skips = use_context::<TrackSkipCtx>();
    let skip_owner = use_hook(|| {
        let mut target = skips.0.borrow_mut();
        target.issued += 1;
        target.issued
    });
    if !slot.hidden() {
        let previous = near.previous;
        let mut on_control = on_control.clone();
        let on_skip = move |skip: Skip, source: &'static str| {
            let control = match skip {
                Skip::Previous if audio => Some(Control::Previous),
                Skip::Previous => previous.map(Control::Play),
                Skip::Next => next.map(Control::Play),
            };
            let to = match control {
                Some(Control::Play(id)) => id.to_string(),
                Some(_) => "song_previous".to_owned(),
                None => "none".to_owned(),
            };
            platform::info(&format!(
                "pharos: track skip dir={} source={source} item={playing_id} to={to}",
                skip.label()
            ));
            control.map(&mut on_control).is_some()
        };
        {
            let mut target = skips.0.borrow_mut();
            target.owner = skip_owner;
            target.on_skip = Some(Box::new(on_skip));
        }
        platform::offer_track_skips(audio || previous.is_some(), next.is_some());
    } else {
        // Handing over or waiting: this player no longer answers.
        release_track_skips(&skips, skip_owner);
    }
    use_drop({
        let skips = skips.clone();
        move || release_track_skips(&skips, skip_owner)
    });
    let controls = Controls {
        media: MediaState {
            loading: media().loading && failed().is_none(),
            ..media()
        },
        runtime_ms: p.detail.item.runtime_ms,
        chapters: p.detail.chapters.clone(),
        segments: p.detail.segments.clone(),
        preview: preview_set(&p.detail.trickplay).cloned(),
        hover_ms: hover_ms(),
        audio: p.detail.audio_tracks.clone(),
        subtitles: p.detail.subtitle_tracks.clone(),
        audio_pick: p.audio,
        subtitle_pick: subtitle(),
        previous: near.previous,
        next,
        up_next: match (up_next_step(), upcoming.as_ref()) {
            (UpNextStep::Counting { left_ms, of_ms }, next) => {
                Some(UpNextCard::of(next, left_ms, of_ms))
            }
            _ => None,
        },
    };
    let on_leave = {
        let session = session.clone();
        move |()| {
            if let Some(session) = session() {
                spawn(async move { session.leave().await });
            }
        }
    };
    let on_own_tracks = {
        let session = session.clone();
        move |own: bool| {
            if let Some(session) = session() {
                session.follow_tracks(!own);
            }
        }
    };
    let compact = view != View::Full;
    let on_action = {
        let mut on_control = on_control.clone();
        move |action: StageAction| {
            let mut stage = stage;
            match action {
                StageAction::Player(control) => on_control(control),
                StageAction::Show(View::Full) => {
                    stage.write().show(View::Full);
                    platform::info(&format!(
                        "pharos: stage view=full item={playing_id} reason=button"
                    ));
                    nav.0.call(Route::Watch(playing_id));
                }
                StageAction::Show(to) => {
                    stage.write().show(to);
                    platform::info(&format!(
                        "pharos: stage view={} item={playing_id} reason=button",
                        to.label()
                    ));
                }
                StageAction::Close => {
                    stage.write().close();
                    platform::info(&format!(
                        "pharos: stage view=closed item={playing_id} reason=close"
                    ));
                }
                StageAction::Place(to) => stage.write().place(to),
                StageAction::Keep => stage.peek().keep_spot(),
            }
        }
    };
    // Handing over: report this song done and stop it, at once on a cut or
    // as it fades out under the next. A fade waits for the next song's
    // playback to be negotiated, so the two overlap and the old one plays on
    // at full volume meanwhile. Once, as the slot changes.
    if let Slot::Outgoing(handoff) = slot {
        if (handoff.fade_ms == 0 || handoff.ready) && !retiring.replace(true) {
            fade_in_cancel.set(true);
            let (stop, last_ms) = (retire_stop, Rc::clone(&last_ms.0));
            let mut stage = stage;
            let runtime_ms = p.detail.item.runtime_ms;
            spawn(async move {
                let state = platform::media_state(OUTGOING_ID);
                if handoff.fade_ms > 0 {
                    // It plays out under the next song, so it counts as heard.
                    last_ms.set(match state.duration_ms {
                        0 => runtime_ms,
                        d => d,
                    });
                }
                stop();
                if handoff.fade_ms == 0 {
                    platform::pause(OUTGOING_ID);
                    return;
                }
                let never = Rc::new(Cell::new(false));
                fade(OUTGOING_ID, state.volume, 0.0, handoff.fade_ms, never).await;
                platform::pause(OUTGOING_ID);
                platform::info(&format!(
                    "pharos: crossfade out item={playing_id} fade_ms={}",
                    handoff.fade_ms
                ));
                stage.write().faded(handoff.from_play);
            });
        }
    }
    rsx! {
        if compact {
            StageBar {
                view,
                title: p.title.clone(),
                image: p.image.clone(),
                media: media(),
                runtime_ms: p.detail.item.runtime_ms,
                pip: p.kind != ItemKind::Audio && platform::pip_supported(),
                previous: near.previous,
                next,
                error: failed().or_else(|| group.ended.clone()),
                on_action,
            }
        } else {
            if let Some(message) = failed() {
                ErrorNote { message }
            }
            if let Some(message) = group.ended.clone() {
                ErrorNote { message }
            }
            if let Some(message) = together.error.read().clone() {
                ErrorNote { message }
            }
        }
        if p.kind == ItemKind::Audio {
            ListenView {
                // A retired song's screen is hidden; it draws no visualiser.
                compact: compact || matches!(slot, Slot::Outgoing(Handoff { ready: true, .. })),
                media_id: slot.media_id(),
                title: p.title.clone(),
                image: p.image.clone(),
                artists: p.detail.artists.clone(),
                album: p.detail.album.clone(),
                track: p.detail.item.episode,
                year: p.detail.item.year,
                genres: p.detail.genres.clone(),
                runtime_ms: p.detail.item.runtime_ms,
                media: media(),
                previous: near.previous,
                next,
                lyrics: aligned()
                    .filter(|(for_id, _)| *for_id == p.detail.item.id)
                    .map(|(_, l)| l)
                    .or_else(|| lyrics.read().clone().and_then(Result::ok).flatten()),
                lyrics_loading: lyrics.read().is_none(),
                lyrics_failed: matches!(*lyrics.read(), Some(Err(()))),
                playing: queued.then_some(playing_id),
                queue: queue.read().upcoming().to_vec(),
                shuffled: queue.read().shuffled(),
                on_queue: move |action: QueueAction| {
                    // In a group the group picks the song.
                    if matches!(action, QueueAction::Play(_)) && !queued {
                        platform::info("pharos: queue action=play refused reason=group");
                        return;
                    }
                    let play = match action {
                        QueueAction::Play(key) => {
                            queue.peek().upcoming().iter().find(|e| e.key == key).map(|e| e.id)
                        }
                        _ => None,
                    };
                    queue.write().apply(action, seed());
                    platform::info(&format!(
                        "pharos: queue action={} upcoming={}",
                        action.label(),
                        queue.peek().upcoming().len()
                    ));
                    if let Some(id) = play {
                        go(id);
                    }
                },
                on_media,
                on_control,
            }
        } else {
            WatchView {
                title: p.title.clone(),
                controls,
                on_media,
                on_control,
                item_id: p.detail.item.id,
                members: if group.active { group.members.clone() } else { Vec::new() },
                own_tracks: group.own_tracks,
                on_own_tracks,
                activity: group.log.lines(),
                toast: group.log.toast(now_ms_u64()).map(str::to_owned),
                own_track_members: group
                    .members
                    .iter()
                    .filter(|m| m.own_tracks)
                    .map(|m| m.id.clone())
                    .collect::<Vec<_>>(),
                seen: seen::rows(&watch_seen, p.detail.item.id),
                on_leave,
                autoplay: !grouped,
                can_start_together: can_watch_together(p.kind) && !compact,
                invite,
                invite_open,
                invite_note: invite_note(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// An invite link is the invite route on this server, the group id as
    /// the server minted it.
    #[test]
    fn an_invite_link_opens_the_groups_invite_page_here() {
        let url = invite_url("https://pharos.example", "0a1b-2c");
        assert_eq!(url, "https://pharos.example/ui/together/0a1b-2c");
        assert_eq!(
            Route::parse("/ui/together/0a1b-2c"),
            Route::TogetherGroup("0a1b-2c".into())
        );
    }

    #[test]
    fn a_chosen_overlay_effect_outlives_a_storage_that_keeps_nothing() {
        assert_eq!(stored_overlay_effect(), OverlayEffect::Fade);
        store_overlay_effect(OverlayEffect::Burn).unwrap();
        assert_eq!(stored_overlay_effect(), OverlayEffect::Burn);
    }

    /// A subtitle size picked on this page load is what the picker reads back,
    /// even when storage keeps nothing (the host stub stores nothing), so the
    /// picker never falls back to an older value while the new one applies.
    #[test]
    fn a_chosen_library_transition_outlives_a_storage_that_keeps_nothing() {
        assert_eq!(stored_library_transition(), LibraryTransition::Fade);
        store_library_transition(LibraryTransition::Warp).unwrap();
        assert_eq!(stored_library_transition(), LibraryTransition::Warp);
    }

    #[test]
    fn a_chosen_subtitle_size_outlives_a_storage_that_keeps_nothing() {
        assert_eq!(stored_subtitle_size(), SubtitleSize::Medium);
        store_subtitle_size(SubtitleSize::Large).unwrap();
        assert_eq!(stored_subtitle_size(), SubtitleSize::Large);
    }

    /// The maximum-quality choice caps video and leaves a song as stored
    /// (B477/V420).
    #[test]
    fn the_maximum_quality_choice_limits_video_only() {
        assert_eq!(video_max_bitrate(ItemKind::Movie, 2_000_000), 2_000_000);
        assert_eq!(video_max_bitrate(ItemKind::Episode, 1_000_000), 1_000_000);
        assert_eq!(video_max_bitrate(ItemKind::Audio, 2_000_000), 0);
        assert_eq!(video_max_bitrate(ItemKind::Movie, 0), 0);
        let caps = |kind| with_max_bitrate(Default::default(), kind, 2_000_000).max_bitrate;
        assert_eq!(caps(ItemKind::Audio), 0);
        assert_eq!(caps(ItemKind::Movie), 2_000_000);
    }

    /// Counts the mounts of `remount_on`'s body as the key changes.
    fn mounts_across(keys: &[&str]) -> u32 {
        #[component]
        fn Probe(mounts: Rc<Cell<u32>>) -> Element {
            use_hook(|| mounts.set(mounts.get() + 1));
            rsx! {}
        }
        #[derive(Clone)]
        struct Props {
            key: Rc<RefCell<String>>,
            mounts: Rc<Cell<u32>>,
        }
        let props = Props {
            key: Rc::new(RefCell::new(keys[0].to_owned())),
            mounts: Rc::new(Cell::new(0)),
        };
        let mut dom = VirtualDom::new_with_props(
            |p: Props| {
                let key = p.key.borrow().clone();
                remount_on(&key, rsx! { Probe { mounts: Rc::clone(&p.mounts) } })
            },
            props.clone(),
        );
        dom.rebuild_in_place();
        for key in &keys[1..] {
            *props.key.borrow_mut() = (*key).to_owned();
            dom.mark_dirty(ScopeId::APP);
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        props.mounts.get()
    }

    #[test]
    fn a_new_play_session_mounts_a_new_player() {
        assert_eq!(mounts_across(&["a", "b", "c"]), 3);
    }

    #[test]
    fn the_same_play_session_keeps_its_player() {
        assert_eq!(mounts_across(&["a", "a"]), 1);
    }

    /// Spec 020 FR-001 (V400): the stage beside the screen draws the player,
    /// so the watch route itself draws nothing that a route change could
    /// unmount.
    #[test]
    fn the_watch_route_draws_no_player_of_its_own() {
        let mut dom = VirtualDom::new_with_props(
            Screen,
            ScreenProps {
                route: Route::Watch(ItemRef::Real(1)),
            },
        );
        dom.rebuild_in_place();
        assert_eq!(dioxus_ssr::render(&dom), "");
    }

    #[test]
    fn a_play_outside_the_full_view_is_named_for_the_stage() {
        assert_eq!(play_action(View::Full), "play");
        assert_eq!(play_action(View::Floating), "stage_floating");
        assert_eq!(play_action(View::Mini), "stage_mini");
    }

    #[test]
    fn the_app_starts_by_connecting() {
        let mut dom = VirtualDom::new(Root);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Connecting…"), "{html}");
    }

    #[test]
    fn the_device_starts_on_the_default_theme_when_nothing_is_stored() {
        let boot = boot_theme();
        assert_eq!(boot.active, theme::DEFAULT);
        assert_eq!(boot.applied, theme::DEFAULT);
    }

    #[test]
    fn a_theme_that_does_not_resolve_gets_no_card() {
        let mut catalogue = theme::shipped();
        catalogue
            .insert("[meta]\nname = \"Broken\"\nschema = 1\nextends = \"Nowhere\"\n".to_owned())
            .unwrap();
        let cards = theme_cards(&catalogue);
        assert_eq!(cards.len(), 7);
        assert!(cards.iter().all(|c| c.name != "Broken"));
    }

    #[test]
    fn a_card_shows_the_resolved_bg_surface_accent_and_fg() {
        let catalogue = theme::shipped();
        let resolved = catalogue.resolve("Kali Dark").unwrap().theme;
        let card = theme_cards(&catalogue)
            .into_iter()
            .find(|c| c.name == "Kali Dark")
            .unwrap();
        let p = resolved.palette;
        assert_eq!(
            card.swatches,
            [p.bg, p.surface, p.accent, p.fg]
                .iter()
                .map(Rgba::to_css)
                .collect::<Vec<_>>()
        );
        assert!(card.shipped);
    }

    #[test]
    fn a_draft_colour_that_is_not_a_colour_shows_the_base_colour_and_the_error() {
        let catalogue = theme::shipped();
        let base = catalogue.resolve("Kali Dark").unwrap().theme;
        let mut draft = Draft::new("Mine", "Kali Dark");
        draft.set_colour(PaletteRole::Accent, "nope");
        let state = Editing {
            draft,
            base: "Kali Dark".into(),
        };
        let view = editor_view(&state, &catalogue);
        let accent = view
            .colours
            .iter()
            .find(|(role, _)| role == "accent")
            .unwrap();
        assert_eq!(accent.1, hex6(base.palette.accent));
        assert!(
            view.error
                .as_deref()
                .unwrap_or("")
                .contains("palette.accent"),
            "{view:?}"
        );
        assert!(view.contrast.is_empty());
    }

    #[test]
    fn a_valid_draft_colour_shows_its_resolved_hex() {
        let catalogue = theme::shipped();
        let mut draft = Draft::new("Mine", "Kali Dark");
        draft.set_colour(PaletteRole::Accent, "#FF00FF");
        let view = editor_view(
            &Editing {
                draft,
                base: "Kali Dark".into(),
            },
            &catalogue,
        );
        let accent = view
            .colours
            .iter()
            .find(|(role, _)| role == "accent")
            .unwrap();
        assert_eq!(accent.1, "#ff00ff");
        assert_eq!(view.error, None);
        assert!(!view.contrast.is_empty());
    }

    #[test]
    fn editor_changes_land_on_the_draft_and_unknown_fields_are_refused() {
        let mut draft = Draft::new("Mine", "Kali Dark");
        apply_edit(&mut draft, Edit::Colour("accent".into(), "#00ff00".into())).unwrap();
        apply_edit(&mut draft, Edit::Effect("blur".into(), 3.0)).unwrap();
        apply_edit(&mut draft, Edit::Density("compact".into())).unwrap();
        apply_edit(&mut draft, Edit::Name("Renamed".into())).unwrap();
        let resolved = draft.preview(&theme::shipped()).unwrap();
        assert_eq!(resolved.theme.palette.accent.g, 255);
        assert_eq!(resolved.theme.effects.blur_px, 3);
        assert_eq!(resolved.theme.name, "Renamed");
        assert!(apply_edit(&mut draft, Edit::Colour("glitter".into(), "#000000".into())).is_err());
        assert!(apply_edit(&mut draft, Edit::Effect("sparkle".into(), 1.0)).is_err());
    }

    #[test]
    fn a_new_custom_theme_takes_the_first_free_name() {
        let mut catalogue = theme::shipped();
        assert_eq!(fresh_name(&catalogue, "Kali Dark"), "My Kali Dark");
        catalogue
            .insert("[meta]\nname = \"My Kali Dark\"\nschema = 1\n".to_owned())
            .unwrap();
        assert_eq!(fresh_name(&catalogue, "Kali Dark"), "My Kali Dark 2");
    }

    #[test]
    fn a_save_over_the_size_limit_is_refused_with_its_reason() {
        let refusal = device::save(
            &mut theme::shipped(),
            "x".repeat(doc::MAX_BYTES + 1),
            Source::Import,
        )
        .unwrap_err();
        assert!(refusal.reason.contains("limit"), "{}", refusal.reason);
    }

    #[component]
    fn HarnessRoot() -> Element {
        let themes = Themes {
            catalogue: use_signal(theme::shipped),
            active: use_signal(|| theme::DEFAULT.to_owned()),
            applied: use_signal(|| theme::DEFAULT.to_owned()),
        };
        use_context_provider(|| themes);
        let ctx = Ctx {
            api: use_signal(|| None::<NativeApi>),
            machine: use_signal(Machine::new),
            marks: use_signal(|| 0),
        };
        use_context_provider(|| ctx);
        rsx! { AppearanceScreen {} }
    }

    #[test]
    fn the_appearance_screen_renders_seven_cards_with_the_applied_one_marked() {
        let mut dom = VirtualDom::new(HarnessRoot);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert_eq!(
            html.matches("class=\"pharos-theme-card\"").count(),
            7,
            "{html}"
        );
        let marked = html
            .split("<li")
            .find(|chunk| chunk.contains("aria-current=\"true\""))
            .unwrap_or("");
        assert!(marked.contains("data-theme=\"pharos-night\""), "{html}");
        assert!(html.contains("class=\"pharos-swatch\""), "{html}");
    }
}
