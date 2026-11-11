//! The native app: routing and history, the connection lifecycle that
//! drives [`Machine`], and the screens that load data for the pure views.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use pharos_client::api::{
    Detail, Home, Item, ItemKind, ItemRef, NativeApi, NativeGrant, PersonPage, PharosApi as _,
    PlaybackGrant as _, StartTracks, TrackChoice, TrackPicks,
};
use pharos_client::{CallInfo, ClientError};

use super::account::AccountMenu;
use super::account_settings;
use super::activity;
use super::appearance::{
    not_saved, AppearanceView, ContrastRow, Edit, EditorView, Lasts, Notice, ThemeCard,
};
use super::approve_device;
use super::backdrop::{self, Backdrop};
use super::browse::{self, Neighbours};
use super::damage;
use super::diagnostics;
use super::favourites::{self, Favourites, FavouritesView};
use super::find::{self, Find, DEBOUNCE_MS, RESULT_LIMIT};
use super::group_app::{Together, VideoPlayer};
use super::group_session::{GroupPlayer, GroupView, Ui};
use super::listen::{ListenView, QueuePanel};
use super::machine::{Event, Machine, Phase, SignInReply};
use super::mixes::{self, ActiveMix, MixCtx};
use super::palette::{self, Row};
use super::person::PersonView;
use super::platform::{self, OpenError};
use super::playback::{
    choose_engine, crossfade_ms, device_class, engine_verdict, fade_volume, format_tier_drop,
    max_bitrate_label, offered_subtitles, parse_crossfade, parse_max_bitrate, parse_tier_drop,
    parse_volume_setting, pick_mse_engine, position_at, preview_set, restart_styled,
    rust_subtitle_url, shown_font_urls, source_of_styled, start_ms, starting_tier,
    starting_tier_line, styled_subtitle_signals, styled_subtitle_url, styled_track_url,
    subtitle_switch, tier_drop_line, tier_drop_report, tier_spared_line, up_next_at, up_next_pill,
    volume_setting, DeviceClass, EffectsDetail, EffectsLevel, Engine, EngineReason, FailureKind,
    Fatal, LibraryTransition, MediaEvent, MediaState, MseEngine, OnFatal, OnFragment, Outgoing,
    OverlayEffect, PlaybackEngine, RendererFrames, ReportClock, SubtitleSize, SubtitleSwitch, Tier,
    TierDrop, TierVerdict, UpNext, UpNextStep, CROSSFADE_KEY, EFFECTS_DETAIL_KEY,
    EFFECTS_LEVEL_KEY, LIBRARY_TRANSITION_KEY, MAX_BITRATE_KEY, MSE_ENGINE_KEY, OVERLAY_EFFECT_KEY,
    SPEAKER_COLOURS_KEY, SUBTITLE_SIZE_KEY, TIER_DROP_KEY, VOLUME_KEY,
};
use super::playback_settings;
use super::preplay::{self, PlayPicks, TrackChoices};
use super::privacy;
use super::quality::{self, QualityView, Watch};
use super::queue::{
    video_kind, video_next_line, Queue, QueueAction, QueueCtx, Track, VideoQueueCtx,
    RESTART_AFTER_MS,
};
use super::report;
use super::resume::{Item as ResumeItem, ResumeProbe, ResumeReport};
use super::route::Route;
use super::seek_wait::SeekWait;
use super::seen::{self, MemberSeen};
use super::settings::{Section, SettingsLayout};
use super::show_tracks;
use super::stage::{
    Handoff, QueueButton, QueueDock, Stage, StageAction, StageBar, StageClose, StageCtx, StageGrip,
    View, STAGE_ID,
};
use super::stereo::{self, ViewMode};
use super::subtitles::{
    cues_in_video, rows_kept_for, track_mode, FallbackReason, RustState, TrackMode,
};
use super::together::{
    can_watch_together, group_api, GroupClient as _, GroupError, GroupSummary, Groups,
};
use super::together_view::{InAGroup, InviteView, NotesView, TogetherView, INVITE_LINK_ID};
use super::track_follow;
use super::trail;
use super::turn::{self, EpisodeTurn, EPISODE_TURN_KEY};
use super::versions;
use super::views::{
    open_route, reveal_palette_row, Busy, ErrorNote, FirstLoad, FlashNote, GroupEndedNote, Hits,
    HomeView, LoadError, LoadFailure, Loading, MarkFavorite, MarkPlayed, Nav, PaletteView,
    PlayFailure, PlayFromStart, PlaybackFailure, SearchBox, Shell, SignInView, StartFailure,
    StatusView, StoredMark, TitleView, PALETTE_INPUT,
};
use super::watch_view::{
    split_position, text_track_id, total_ms, Control, Controls, Skip, UpNextCard, VideoQueueView,
    WatchView, OUTGOING_ID, SEEK_ID, VIDEO_ID,
};
use super::whats_new::{self, Close, Gate, GateInputs, Pick, Unseen, WhatsNewBadge, WhatsNewView};
use crate::theme::editor::{Draft, EffectField};
use crate::theme::resolve::PaletteRole;
use crate::theme::signal::{self, Source};
use crate::theme::value::{Density, FontStack, Rgba};
use crate::theme::{self, contrast, css, device, doc, format, Catalogue, Theme, ThemeError};
use crate::views::RootApp;

/// The page root: one route per URL, kept in step with history.
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
    // What Home and title pages last showed, and where each page was left,
    // for Back to draw them at once (FR-206).
    let revisit = use_context_provider(super::revisit::Revisit::new);
    use_hook(|| {
        let mut back = revisit.back;
        Rc::new(platform::on_pop_state(move |path| {
            back.set(true);
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
    // The TV remote's Back at the first page closes the player still
    // floating there before it leaves the app.
    use_hook(move || {
        Rc::new(platform::on_back_at_root(move || {
            let Some(item) = stage.peek().item else {
                return false;
            };
            stage.write().close();
            platform::info(&format!(
                "pharos: stage view=closed item={item} reason=back_at_root"
            ));
            true
        }))
    });
    // The F key put the page in fullscreen; the player leaving the full
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
        super::report::note(&format!("route {}", at.path()));
        let mut next = stage.peek().clone();
        let item = next.item;
        if let Some(view) = next.follow(&at) {
            let view = if next.item.is_some() {
                view.label()
            } else {
                "closed"
            };
            platform::info(&format!(
                "pharos: stage view={view} item={} reason=nav",
                item.map(|i| i.to_string()).unwrap_or_default()
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
                turn_on_nav(&route.peek(), &to);
                let (mut places, mut back) = (revisit.places, revisit.back);
                if let Some(y) = platform::scroll_y() {
                    places.write().put(route.peek().path(), y);
                }
                back.set(false);
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
            turn_on_nav(&route.peek(), &to);
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
        let keeper = super::group_app::GroupKeeper::default();
        while let Some(ui) = rx.next().await {
            match ui {
                Ui::View(view) => {
                    keeper.keep(&view);
                    group_view.set(view);
                }
                Ui::Navigate(item) => replace.0.call(Route::Watch(item)),
            }
        }
    });
    use_context_provider(|| Together::new(group_view, group_error, group_ui));
    let route = route();
    rsx! {
        if backdrop::runs_behind(&route) {
            Backdrop {}
        }
        NativeApp { route }
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

/// How often to ask again for word times the aligner is
/// still working out (B499). An ask is a cache read on the server, and every
/// second between the answer being ready and the next ask is a second of
/// guessed fill (B937). The asks go on for as long as the server says the
/// words are pending: a song can wait behind others in the aligner's queue
/// and then take minutes, and giving up after a fixed count left the fill a
/// guess for the whole song (B1163). Leaving the song drops the task and
/// stops the asks.
const ALIGN_ASK_MS: u32 = 5_000;

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

    /// Make `name` the choice, apply it and store the choice. A failed
    /// store is logged; [`Self::choose_kept`] also hands it back.
    fn choose(self, name: String) {
        let _logged = self.choose_kept(name);
    }

    /// [`Self::choose`], answering why the device would not keep the choice
    /// when it would not: the choice still applies to this page.
    fn choose_kept(mut self, name: String) -> Result<(), String> {
        self.active.set(name);
        self.apply_active();
        self.store_active()
    }

    fn store_active(self) -> Result<(), String> {
        kept_setting(device::ACTIVE_KEY, &self.active.peek())
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
    let device = this_device();
    platform::info(&format!(
        "pharos: device class={} android_shell={}",
        device.label(),
        platform::android_shell()
    ));
    platform::apply_device(device);
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

/// The integrity scan's damaged spans, when they describe the file playing:
/// only the primary file is scanned, so another version has none (B1160).
fn scanned_spans(p: &Playing) -> Vec<pharos_client::api::DamageSpan> {
    let primary = p.version.is_empty()
        || p.detail
            .versions
            .iter()
            .any(|v| v.primary && v.id == p.version);
    match (&p.detail.damage, primary) {
        (Some(d), true) => d.ranges.clone(),
        _ => Vec::new(),
    }
}

/// The spans this device skipped in a file before (B1160).
fn stored_skips(item: ItemRef, version: &str) -> Vec<pharos_client::api::DamageSpan> {
    let key = damage::skipped_key(item, version);
    match platform::stored_theme(&key) {
        Ok(stored) => stored
            .as_deref()
            .map_or_else(Vec::new, damage::parse_skipped),
        Err(e) => {
            platform::warn(&signal::storage_failed(&key, &e));
            Vec::new()
        }
    }
}

fn remember_skip(item: ItemRef, version: &str, from_ms: u64, to_ms: u64) {
    let spans = damage::remember_skip(&stored_skips(item, version), from_ms, to_ms);
    store_setting(
        &damage::skipped_key(item, version),
        &damage::format_skipped(&spans),
    );
}

/// The tab's queue from before a reload, if one was read back; it is held
/// until the signed-in user is known. A record that cannot be read is logged
/// with its reason and dropped.
fn restore_queue() -> Option<Queue> {
    let text = match platform::session_get(super::queue::QUEUE_KEY) {
        Ok(Some(text)) => text,
        Ok(None) => return None,
        Err(e) => {
            platform::warn(&format!(
                "pharos: queue restore failed reason=unreadable error={e}"
            ));
            return None;
        }
    };
    match Queue::restore(&text) {
        Ok(queue) => {
            platform::info(&format!(
                "pharos: queue restored upcoming={} shuffled={}",
                queue.upcoming().len(),
                queue.shuffled()
            ));
            Some(queue)
        }
        Err(e) => {
            let reason = match e {
                super::queue::RestoreError::Unbound => "unbound",
                _ => "invalid",
            };
            platform::warn(&format!(
                "pharos: queue restore failed reason={reason} error={e}"
            ));
            None
        }
    }
}

/// Ties the queue to the signed-in user once known; a queue another user
/// left (its stored copy not cleared at their sign-out) is dropped.
fn claim_queue(mut queue: Signal<Queue>, mut held: Signal<Option<Queue>>, user: &str) {
    if let Some(saved) = held.write().take() {
        if queue.write().adopt(saved, user) {
            platform::info(&format!(
                "pharos: queue adopted upcoming={}",
                queue.peek().upcoming().len()
            ));
        } else {
            platform::info("pharos: queue dropped reason=another-user-or-replaced");
        }
        return;
    }
    if queue.peek().owner() == Some(user) {
        return;
    }
    if queue.write().claim(user) {
        platform::info("pharos: queue dropped reason=another-user");
    }
}

/// Keeps the queue for the tab. A failure is logged once per reason, as the
/// queue changes with every song.
fn store_queue(text: &str) {
    thread_local! {
        static LAST_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }
    let result = platform::session_set(super::queue::QUEUE_KEY, text);
    LAST_ERROR.with_borrow_mut(|last| match result {
        Ok(()) => *last = None,
        Err(e) if last.as_deref() != Some(e.as_str()) => {
            platform::warn(&format!("pharos: queue not stored error={e}"));
            *last = Some(e);
        }
        Err(_) => {}
    });
}

/// Stores one device setting, logging a failure.
fn store_setting(key: &str, value: &str) {
    let _logged = kept_setting(key, value);
}

/// [`store_setting`], handing a failure back to say so to the viewer.
fn kept_setting(key: &str, value: &str) -> Result<(), String> {
    platform::store_theme(key, value)
        .inspect_err(|e| platform::warn(&signal::storage_failed(key, e)))
}

thread_local! {
    /// The viewer's playback speed this session (T038-305), which a new
    /// player starts at.
    static LAST_SPEED: Cell<f64> = const { Cell::new(1.0) };
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
    let last = if platform::owns_volume() {
        last
    } else {
        Some((1.0, false, "device"))
    };
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
    // The Rust engine picks the quality itself: a cap auto quality settled on
    // under hls.js would otherwise hold the server's ladder down for good.
    let auto = if hls_engine_adapts() { 0 } else { auto };
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
/// Keeps `size` for this device and draws subtitles at it now, lifting the
/// cues on screen. The size applies even when it could not be kept.
fn set_subtitle_size(size: SubtitleSize) -> Result<(), String> {
    let kept = store_subtitle_size(size);
    platform::apply_subtitle_size(size);
    platform::relift_cues(VIDEO_ID);
    platform::info(&format!("pharos: subtitle_size set={}", size.as_str()));
    kept
}

/// How this device shows a packed 3D version (V922: kept on the device
/// alone); one eye when nothing is kept.
fn stored_view_mode() -> ViewMode {
    match platform::stored_theme(stereo::VIEW_MODE_KEY) {
        Ok(stored) => stored.as_deref().map_or(ViewMode::OneEye, ViewMode::parse),
        Err(e) => {
            platform::warn(&format!(
                "pharos: stereo view storage read failed error={e}"
            ));
            ViewMode::OneEye
        }
    }
}

/// Keeps `mode` on this device. It applies for this player even when the
/// browser refused to keep it.
fn store_view_mode(mode: ViewMode) {
    platform::info(&format!("pharos: stereo view set={}", mode.as_str()));
    if let Err(e) = platform::store_theme(stereo::VIEW_MODE_KEY, mode.as_str()) {
        platform::warn(&format!("pharos: stereo view not kept error={e}"));
    }
}

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

thread_local! {
    /// The subtitle effects level chosen on this page load; it outranks
    /// storage as [`SESSION_SUBTITLE_SIZE`] does.
    static SESSION_EFFECTS_LEVEL: Cell<Option<EffectsLevel>> = const { Cell::new(None) };
}

/// The viewer's subtitle effects level: this page load's choice, else the
/// stored one, else Subtle.
fn stored_effects_level() -> EffectsLevel {
    if let Some(level) = SESSION_EFFECTS_LEVEL.with(Cell::get) {
        return level;
    }
    match platform::stored_theme(EFFECTS_LEVEL_KEY) {
        Ok(stored) => stored
            .as_deref()
            .map_or(EffectsLevel::default(), EffectsLevel::parse),
        Err(e) => {
            platform::warn(&format!(
                "pharos: effects_level storage read failed error={e}"
            ));
            EffectsLevel::default()
        }
    }
}

/// Keep `level` for this page load and try to save it on the device. The
/// write's error comes back so the page can say the choice was not kept.
fn store_effects_level(level: EffectsLevel) -> Result<(), String> {
    SESSION_EFFECTS_LEVEL.with(|s| s.set(Some(level)));
    platform::store_theme(EFFECTS_LEVEL_KEY, level.as_str()).inspect_err(|e| {
        platform::warn(&format!(
            "pharos: effects_level storage write failed error={e}"
        ));
    })
}

thread_local! {
    /// The speaker colours switch set on this page load; it outranks storage
    /// as [`SESSION_EFFECTS_LEVEL`] does.
    static SESSION_SPEAKER_COLOURS: Cell<Option<bool>> = const { Cell::new(None) };
}

/// The viewer's speaker colours switch: this page load's choice, else the
/// stored one, else off.
fn stored_speaker_colours() -> bool {
    if let Some(on) = SESSION_SPEAKER_COLOURS.with(Cell::get) {
        return on;
    }
    match platform::stored_theme(SPEAKER_COLOURS_KEY) {
        Ok(stored) => stored.as_deref() == Some("on"),
        Err(e) => {
            platform::warn(&format!(
                "pharos: speaker_colours storage read failed error={e}"
            ));
            false
        }
    }
}

/// Keep `on` for this page load and try to save it on the device. The
/// write's error comes back so the page can say the choice was not kept.
fn store_speaker_colours(on: bool) -> Result<(), String> {
    SESSION_SPEAKER_COLOURS.with(|s| s.set(Some(on)));
    platform::store_theme(SPEAKER_COLOURS_KEY, if on { "on" } else { "off" }).inspect_err(|e| {
        platform::warn(&format!(
            "pharos: speaker_colours storage write failed error={e}"
        ));
    })
}

thread_local! {
    /// The effects detail chosen on this page load; it outranks storage as
    /// [`SESSION_EFFECTS_LEVEL`] does.
    static SESSION_EFFECTS_DETAIL: Cell<Option<EffectsDetail>> = const { Cell::new(None) };
}

/// The viewer's effects detail: this page load's choice, else the stored
/// one, else Auto.
fn stored_effects_detail() -> EffectsDetail {
    if let Some(detail) = SESSION_EFFECTS_DETAIL.with(Cell::get) {
        return detail;
    }
    match platform::stored_theme(EFFECTS_DETAIL_KEY) {
        Ok(stored) => stored
            .as_deref()
            .map_or(EffectsDetail::default(), EffectsDetail::parse),
        Err(e) => {
            platform::warn(&format!(
                "pharos: effects_detail storage read failed error={e}"
            ));
            EffectsDetail::default()
        }
    }
}

/// Keep `detail` for this page load and try to save it on the device. The
/// write's error comes back so the page can say the choice was not kept.
fn store_effects_detail(detail: EffectsDetail) -> Result<(), String> {
    SESSION_EFFECTS_DETAIL.with(|s| s.set(Some(detail)));
    platform::store_theme(EFFECTS_DETAIL_KEY, detail.as_str()).inspect_err(|e| {
        platform::warn(&format!(
            "pharos: effects_detail storage write failed error={e}"
        ));
    })
}

thread_local! {
    /// The tier this device dropped to on this page load; it outranks storage
    /// as [`SESSION_EFFECTS_LEVEL`] does.
    static SESSION_TIER_DROP: Cell<Option<Tier>> = const { Cell::new(None) };
}

/// The tier this device last dropped to (spec 028 FR-019): this page load's
/// drop, else the stored one, else none.
fn stored_tier_drop() -> Option<Tier> {
    if let Some(tier) = SESSION_TIER_DROP.with(Cell::get) {
        return Some(tier);
    }
    match platform::stored_theme(TIER_DROP_KEY) {
        Ok(stored) => stored
            .as_deref()
            .and_then(|v| parse_tier_drop(v, now_ms_u64())),
        Err(e) => {
            platform::warn(&format!("pharos: tier_drop storage read failed error={e}"));
            None
        }
    }
}

/// Keep `tier` as this device's cap for this page load and try to save it
/// with the time. A cap never rises: the lower of `tier` and the one already
/// held is kept.
fn store_tier_drop(tier: Tier) -> Result<(), String> {
    let keep = stored_tier_drop().map_or(tier, |held| held.min(tier));
    SESSION_TIER_DROP.with(|s| s.set(Some(keep)));
    platform::store_theme(TIER_DROP_KEY, &format_tier_drop(keep, now_ms_u64())).inspect_err(|e| {
        platform::warn(&format!("pharos: tier_drop storage write failed error={e}"));
    })
}

/// Forget this device's tier cap, so the next styled track starts at its own
/// tier again; the frame-drop watch sets a new cap if it still cannot keep
/// up (B1214).
fn clear_tier_drop() -> Result<(), String> {
    SESSION_TIER_DROP.with(|s| s.set(None));
    platform::store_theme(TIER_DROP_KEY, "").inspect_err(|e| {
        platform::warn(&format!("pharos: tier_drop storage clear failed error={e}"));
    })
}

/// The media query for a viewer who asked for less motion.
const REDUCED_MOTION: &str = "(prefers-reduced-motion: reduce)";

/// `(max-width)` below which a coarse-pointer device counts as a phone.
const NARROW_SCREEN: &str = "(max-width: 600px)";

/// What kind of screen this page runs on: a TV gets the ten-foot layout
/// and starts styled subtitles at the TV tier.
pub(crate) fn this_device() -> DeviceClass {
    device_class(
        &platform::user_agent(),
        platform::prefers("(pointer: coarse)"),
        platform::prefers(NARROW_SCREEN),
        platform::android_shell(),
    )
}

/// The tier a styled subtitle track is first fetched at on this device, and
/// the log line saying why.
fn current_starting_tier() -> Tier {
    let device = this_device();
    let reduced_motion = platform::prefers(REDUCED_MOTION);
    let remembered = stored_tier_drop();
    let detail = stored_effects_detail();
    let tier = starting_tier(device, reduced_motion, remembered, detail);
    platform::info(&starting_tier_line(
        tier,
        device,
        reduced_motion,
        remembered,
        detail,
    ));
    tier
}

/// How often the player reads the video's dropped frames while a generated
/// styled track shows; one second is a fine-enough grain for a 10 s window.
const QUALITY_SAMPLE_MS: u32 = 1_000;
/// Samples in a row without renderer counters before warning: the TV page
/// posts them at most once a second, so the first sample after ready can
/// come before any message has carried them.
const UNCOUNTED_WARN_SAMPLES: u32 = 3;

/// Feed the video's frame counters, and the renderer's own when it counts
/// them, to `watch`; a drop is remembered for the device, logged and
/// reported, and the new tier comes back. A window whose dropped frames were
/// the video's own is logged and keeps the tier (B667).
fn on_quality_sample(
    watch: &mut TierDrop,
    renderer: &str,
    now_ms: u64,
    (dropped_total, presented_total): (u64, u64),
    renderer_frames: Option<RendererFrames>,
    counting: bool,
) -> Option<Tier> {
    let drop = match watch.judge(
        now_ms,
        dropped_total,
        presented_total,
        renderer_frames,
        counting,
    ) {
        TierVerdict::Hold => return None,
        TierVerdict::Spared(spared) => {
            platform::info(&tier_spared_line(&spared));
            return None;
        }
        TierVerdict::Drop(drop) => drop,
    };
    platform::info(&tier_drop_line(&drop));
    platform::report_event(tier_drop_report(renderer, &drop));
    // A failed write is already warned about; the drop holds for this load.
    let _ = store_tier_drop(drop.to);
    Some(drop.to)
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

/// Whether an HLS stream on this device plays through an engine that adapts
/// its own quality.
fn hls_engine_adapts() -> bool {
    choose_engine(
        true,
        platform::plays_hls(),
        platform::has_mse(),
        mse_engine_choice().0,
    )
    .adapts_quality()
}

/// This page load's HLS engine and why (V630).
fn mse_engine_choice() -> (MseEngine, EngineReason) {
    let stored = stored_mse_engine_raw();
    pick_mse_engine(&platform::current_search(), stored.as_deref())
}

fn stored_mse_engine_raw() -> Option<String> {
    platform::stored_theme(MSE_ENGINE_KEY).unwrap_or_else(|e| {
        platform::warn(&format!("pharos: engine storage read failed error={e}"));
        None
    })
}

/// The engine this device's Playback setting names; hls.js when unset.
fn stored_mse_engine() -> MseEngine {
    pick_mse_engine("", stored_mse_engine_raw().as_deref()).0
}

thread_local! {
    /// The episode turn choice this page load, once read or set.
    static EPISODE_TURN: Cell<Option<EpisodeTurn>> = const { Cell::new(None) };
}

/// The viewer's episode turn choice; [`EpisodeTurn::Turn`] unless they chose Off.
fn stored_episode_turn() -> EpisodeTurn {
    if let Some(choice) = EPISODE_TURN.get() {
        return choice;
    }
    let choice = match platform::stored_theme(EPISODE_TURN_KEY) {
        Ok(stored) => stored
            .as_deref()
            .map_or_else(EpisodeTurn::default, EpisodeTurn::parse),
        Err(e) => {
            platform::warn(&format!(
                "pharos: episode_turn storage read failed error={e}"
            ));
            EpisodeTurn::default()
        }
    };
    EPISODE_TURN.set(Some(choice));
    choice
}

fn store_episode_turn(choice: EpisodeTurn) -> Result<(), String> {
    EPISODE_TURN.set(Some(choice));
    platform::info(&format!("pharos: episode_turn set={}", choice.as_str()));
    platform::store_theme(EPISODE_TURN_KEY, choice.as_str()).inspect_err(|e| {
        platform::warn(&format!(
            "pharos: episode_turn storage write failed error={e}"
        ));
    })
}

/// A navigation to the watch page: turn from the episode on screen, or get
/// ready to turn onto the one being opened once it is known to be a video.
fn turn_on_nav(from: &Route, to: &Route) {
    let direction = turn::take_direction();
    let Route::Watch(next) = to else {
        return;
    };
    if !turn::wanted(stored_episode_turn(), platform::prefers(REDUCED_MOTION)) {
        return;
    }
    let item = next.to_string();
    match from {
        Route::Watch(now) if now != next => {
            platform::turn_begin(&item, direction);
        }
        Route::Watch(_) => {}
        _ => platform::turn_expect(&item, direction),
    }
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

/// A resume in flight, a count of resumes so a superseded watch stops, and
/// when the song was last paused.
#[derive(Default)]
struct ResumeState {
    count: u64,
    probe: Option<ResumeProbe>,
    /// `now_ms` of the pause the song is in; `None` while it plays.
    paused_at: Option<f64>,
}

type ResumeSlot = Rc<RefCell<ResumeState>>;

/// Log and send `report`, with the wake of the tap on [`VIDEO_ID`] as it
/// stands now (B735).
fn report_resume(report: ResumeReport) {
    let report = report.with_wake(platform::tap_wake(VIDEO_ID));
    platform::info(&report.line());
    platform::report_event(report.event());
}

/// Start timing a resume of `item` on the media element `id`, before
/// `play()` so its `playing` cannot come first. Returns the resume's number.
fn start_resume(id: &str, item: ResumeItem, slot: &ResumeSlot) -> u64 {
    let (n, superseded) = {
        let mut slot = slot.borrow_mut();
        let superseded = slot.probe.as_mut().and_then(ResumeProbe::superseded);
        slot.count = slot.count.wrapping_add(1);
        slot.probe = Some(ResumeProbe::start(
            platform::now_ms(),
            platform::resume_context(id),
            item,
            slot.paused_at,
        ));
        (slot.count, superseded)
    };
    if let Some(report) = superseded {
        report_resume(report);
    }
    n
}

/// Keep the paused song's output warm, then rest it (B734, V683): a tap's
/// running `AudioContext` holds the output stream open through the pause,
/// so the sound server leaves the device awake for the resume. A pause still
/// unbroken after [`super::resume::REST_AFTER_PAUSE_MS`] suspends the
/// context; the play that ends it wakes the context first.
async fn rest_after_pause(slot: ResumeSlot, paused_at: f64) {
    let state = platform::resume_context(VIDEO_ID)
        .audio
        .map_or_else(|| "untapped".to_owned(), |a| a.context_state);
    platform::info(&format!(
        "pharos: audio output warm reason=paused context_state={state} rest_after_ms={}",
        super::resume::REST_AFTER_PAUSE_MS
    ));
    // Lossless: the constant is a whole number of ms well inside u32.
    platform::sleep_ms(super::resume::REST_AFTER_PAUSE_MS as u32).await;
    if slot.borrow().paused_at != Some(paused_at)
        || !super::resume::rests(paused_at, platform::now_ms())
    {
        return;
    }
    match platform::rest_tap(VIDEO_ID) {
        Some(state) => platform::info(&format!(
            "pharos: audio output rest reason=paused_too_long state_before={state}"
        )),
        None => platform::info("pharos: audio output rest reason=untapped state_before=none"),
    }
}

/// Measure resume number `resume` of the media element `id` until it
/// sounds, is paused again, or times out (V473).
async fn watch_resume(id: &'static str, slot: ResumeSlot, resume: u64) {
    loop {
        platform::sleep_ms(RESUME_STEP_MS).await;
        let report = {
            let mut slot = slot.borrow_mut();
            if slot.count != resume {
                return;
            }
            let Some(probe) = slot.probe.as_mut() else {
                return;
            };
            if probe.done() {
                return;
            }
            probe.frame(platform::now_ms(), platform::tap_peak(id))
        };
        if let Some(report) = report {
            report_resume(report);
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

fn notify(notice: Signal<Option<Notice>>, text: String, error: bool) {
    show(notice, Notice { text, error });
}

fn show(mut notice: Signal<Option<Notice>>, line: Notice) {
    notice.set(Some(line));
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
pub(super) struct AccountMe(pub(super) Resource<Option<pharos_client::api::Me>>);

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

/// How long a [`Flash`] line stays up.
const FLASH_MS: u32 = 7000;

/// A line over the screen for an action that has no screen of its own to
/// report on, such as a watched mark or a saved mix that failed (B824).
/// Provided by the app root; the newest line replaces the last and clears
/// itself.
#[derive(Clone, Copy)]
pub(super) struct Flash {
    line: Signal<Option<Notice>>,
    generation: Signal<u32>,
}

impl Flash {
    pub(super) fn say(mut self, text: String, error: bool) {
        let this = self.generation.peek().wrapping_add(1);
        self.generation.set(this);
        self.line.set(Some(Notice { text, error }));
        spawn(async move {
            platform::sleep_ms(FLASH_MS).await;
            if self.generation.try_peek().is_ok_and(|g| *g == this) {
                self.line.set(None);
            }
        });
    }
}

/// Which of an item's marks a [`mark_handler`] stores.
#[derive(Clone, Copy)]
enum Mark {
    Played,
    Favorite,
}

impl Mark {
    fn label(self) -> &'static str {
        match self {
            Mark::Played => "played",
            Mark::Favorite => "favorite",
        }
    }
}

/// Stores one kind of mark for `(item, on, name)`, one per item at a time
/// (a second click before the reload would record a second play), says in
/// the flash line when the server refuses, and reloads the screen either
/// way: a season that failed part way through has some episodes marked, and
/// the screen shows what is stored.
fn mark_handler(
    mark: Mark,
    api: Signal<Option<NativeApi>>,
    machine: Signal<Machine>,
    flash: Flash,
    mut marks: Signal<u32>,
    mut stored: Signal<Option<(ItemRef, bool)>>,
) -> EventHandler<(ItemRef, bool, String)> {
    let marking = Rc::new(RefCell::new(std::collections::HashSet::<ItemRef>::new()));
    EventHandler::new(move |(id, on, name): (ItemRef, bool, String)| {
        let Some(api) = api.peek().clone() else {
            return;
        };
        let what = mark.label();
        if !marking.borrow_mut().insert(id) {
            platform::info(&format!(
                "pharos: mark {id} {what}={on} skipped: one in flight"
            ));
            return;
        }
        let marking = Rc::clone(&marking);
        spawn(async move {
            let call = call("mark", what);
            let result = match mark {
                Mark::Played => api.set_played(&call, id, on).await,
                Mark::Favorite => api.set_favorite(&call, id, on).await,
            };
            match settle(machine, result) {
                Ok(now) => {
                    if matches!(mark, Mark::Played) {
                        stored.set(Some((id, now)));
                    }
                }
                Err(e) => {
                    platform::warn(&format!("pharos: marking {id} {what}={on} failed: {e}"));
                    let line = match mark {
                        Mark::Played => super::views::mark_failure_line(&name, on, &e),
                        Mark::Favorite => super::views::favorite_failure_line(&name, on, &e),
                    };
                    flash.say(line, true);
                }
            }
            marking.borrow_mut().remove(&id);
            marks += 1;
        });
    })
}

#[component]
fn NativeApp(route: Route) -> Element {
    let mut machine = use_signal(Machine::new);
    // The player shows a lost connection itself; the shell under it is out
    // of sight (spec 038 FR-503).
    let connection_lost = use_memo(move || machine.read().reconnecting());
    use_context_provider(|| ConnectionLost(connection_lost));
    let mut api = use_signal(|| None::<NativeApi>);
    let mut task = use_signal(|| None::<dioxus::core::Task>);
    let mut started = use_signal(|| None::<u32>);
    // A connection attempt that has not answered in a while (a tunnel, where
    // the browser still says it is online) lets kept books open meanwhile.
    let mut slow_connect = use_signal(|| false);
    // A page loaded before a deploy reconnects to the newer server, which may
    // send values this build cannot read: it asks to be reloaded.
    // The newer server's schema, when it is newer than this build's.
    let outdated = use_signal(|| None::<u32>);
    // Dismissed for the rest of this run; a restart shows it again.
    let mut update_dismissed = use_signal(|| false);
    let marks = use_signal(|| 0u32);
    // Bumped by a library change the server pushes; marks by a user data one.
    let library_rev = use_signal(|| 0u32);
    use_context_provider(|| super::live::LibraryRev(library_rev));
    let scan = use_signal(crate::views::scan_progress::ScanView::default);
    use_context_provider(|| crate::views::scan_progress::ScanWatch(scan));
    super::live::use_live_updates(api, marks, library_rev, scan);
    let flash = Flash {
        line: use_signal(|| None),
        generation: use_signal(|| 0),
    };
    use_context_provider(|| flash);
    use_context_provider(|| Ctx {
        api,
        machine,
        marks,
    });
    // One manage token per connection, reused each time Manage opens (B920).
    let mut held_manage = use_signal(|| None);
    use_context_provider(|| super::manage::HeldManageToken(held_manage));
    let manage_tab = use_signal(|| None::<crate::views::AdminTab>);
    use_context_provider(|| super::manage::ManageTab(manage_tab));
    let manage_focus = use_signal(|| None::<String>);
    use_context_provider(|| super::manage::ManageFocus(manage_focus));
    let mut flagged_list = use_signal(|| None);
    let mut flagged_asked = use_signal(|| false);
    use_context_provider(|| super::manage::FlaggedShows {
        list: flagged_list,
        asked: flagged_asked,
    });
    use_effect(move || {
        let _ = api.read();
        held_manage.set(None);
        flagged_list.set(None);
        flagged_asked.set(false);
    });
    // A reloaded tab joins the group it was in again (B665): the server kept
    // its seat for the socket-loss grace, and the group's item brings the
    // tab to what the group plays. Read before this page publishes a view.
    let kept_group = use_hook(|| Rc::new(RefCell::new(super::group_app::kept_before_load())));
    let together = use_context::<Together>();
    let together_again = together.clone();
    // Whether this device is in a group, for the bar's Together link (B1361).
    let shell_group_view = together.view;
    use_effect(move || {
        let Some(api) = api.read().clone() else {
            return;
        };
        let kept = kept_group.borrow_mut().take();
        let Some(id) = super::group_app::group_to_rejoin(kept, together.view.peek().active) else {
            return;
        };
        platform::info(&format!("pharos: group rejoin reason=reload group={id}"));
        let session = together.session(&api);
        let mut note = together.note;
        let started = platform::unix_ms();
        spawn_forever(async move {
            use super::group_session::{rejoin_event, RejoinOutcome};
            let joined = session.join(&id).await;
            let waited = platform::unix_ms().saturating_sub(started);
            let (outcome, cause) = match &joined {
                Ok(_) => (RejoinOutcome::Rejoined, None),
                Err(e) if e.is_superseded() => (RejoinOutcome::Superseded, None),
                Err(e) => (RejoinOutcome::Failed, Some(e.0.as_str())),
            };
            platform::report_event(rejoin_event("reload", &id, outcome, 1, waited, cause));
            if outcome != RejoinOutcome::Failed {
                return;
            }
            if let Err(e) = joined {
                platform::warn(&format!(
                    "pharos: group rejoin failed group={id} error={}",
                    e.0
                ));
                note.set(Some(super::group_app::rejoin_failure(&e.0)));
            }
        });
    });
    // Each later API is a new connection: a group membership on the old one
    // is joined again over it.
    let resumed_once = use_hook(|| Rc::new(Cell::new(false)));
    use_effect(move || {
        let Some(api) = api.read().clone() else {
            return;
        };
        if resumed_once.replace(true) {
            together_again.reconnected(&api);
        }
    });
    // Reading saved offline is sent from wherever the user is (spec 036).
    #[cfg(all(feature = "web", target_arch = "wasm32"))]
    super::reader_web::use_pending_flush(api);
    // The play queue lives as long as the app, across pages and songs, and
    // is kept for the tab so a reload keeps it (FR-704).
    // A queue read back is held until the signed-in user is known, so no
    // song of another user's queue shows meanwhile (B1288); the stored copy
    // is not overwritten while it is held.
    let held = use_signal(restore_queue);
    let queue = use_signal(Queue::default);
    use_context_provider(|| QueueCtx(queue));
    use_effect(move || {
        if held.read().is_none() {
            store_queue(&queue.read().save());
        }
    });
    // Films, episodes and music videos queue apart from songs (V853).
    use_context_provider(|| VideoQueueCtx(Signal::new(Queue::default()), Signal::new(false)));
    // The mix that is on keeps the queue topped up (spec 024, B441).
    let mix = use_signal(|| None::<ActiveMix>);
    use_context_provider(|| MixCtx(mix));
    // Bumped when a saved mix is stored, so every list of them reloads.
    use_context_provider(|| mixes::SavedRev(Signal::new(0)));
    // Whether the queue is docked beside the compact player (B900).
    use_context_provider(|| QueueDock(Signal::new(super::listen::dock_shown())));
    mixes::use_top_up(api, queue, mix);
    let stored = use_context_provider(|| StoredMark(Signal::new(None))).0;
    use_context_provider(|| {
        MarkPlayed(mark_handler(
            Mark::Played,
            api,
            machine,
            flash,
            marks,
            stored,
        ))
    });
    use_context_provider(|| {
        MarkFavorite(mark_handler(
            Mark::Favorite,
            api,
            machine,
            flash,
            marks,
            stored,
        ))
    });
    let search = Search {
        text: use_signal(String::new),
        find: use_signal(Find::default),
        hits: use_signal(|| Hits::Idle),
    };
    use_context_provider(|| search);
    let mut palette = Palette(use_signal(|| false));
    use_context_provider(|| palette);
    let mut report_open = ReportOpen(use_signal(|| false));
    use_context_provider(|| report_open);
    super::keys::provide_key_mode();
    let whats_new_state = use_whats_new_state();
    use_context_provider(|| whats_new_state);
    use_hook(move || {
        Rc::new(platform::on_key(move |key, ctrl, meta| {
            // Signed out there is no palette to open: leave the key alone
            // rather than open one that pops up right after sign-in.
            let signed_in = api.peek().is_some();
            if report::is_shortcut(key, ctrl, meta) && signed_in {
                report::note("key ctrl+shift+d: report a problem");
                report_open.0.set(true);
                return true;
            }
            let toggle = palette::is_toggle(key, ctrl, meta) && signed_in;
            if toggle {
                report::note("key ctrl+k: palette");
                let opening = !*palette.0.peek();
                if opening {
                    search.clear();
                }
                palette.0.set(opening);
            }
            toggle
        }))
    });
    let nav = use_context::<Nav>();
    let mut start_over = use_signal(|| None::<ItemRef>);
    use_context_provider(|| StartOver(start_over));
    let play_picks = use_signal(|| None);
    use_context_provider(|| PlayPicks(play_picks));
    use_context_provider(|| {
        PlayFromStart(EventHandler::new(move |id: ItemRef| {
            start_over.set(Some(id));
            nav.0.call(Route::Watch(id));
        }))
    });
    // Signing out forgets the names pages loaded, so the tab title of a
    // signed-out page never shows the last user's item to the next one.
    let PageNames(mut names) = use_context::<PageNames>();
    let mut revisit = try_use_context::<super::revisit::Revisit>();
    use_effect(move || {
        if matches!(machine.read().phase(), Phase::SignIn { .. }) {
            // Every road to sign-in, a revoked session or a refused resume
            // included: the next user's report must not carry this trail.
            report::forget();
            // Nor may Back draw them the last user's Home or titles.
            if let Some(revisit) = revisit.as_mut() {
                revisit.forget();
            }
            if !names.peek().is_empty() {
                names.write().clear();
            }
            // The queue, kept for the tab (B1281), is not the next user's,
            // nor is one still held from before a reload.
            let mut queue = queue;
            if *queue.peek() != Queue::default() {
                queue.set(Queue::default());
            }
            let mut held = held;
            if held.peek().is_some() {
                held.set(None);
            }
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
    let mut libraries_failed = use_signal(|| false);
    use_effect(move || {
        let _ = library_rev();
        let Some(api) = api.read().clone() else {
            return;
        };
        spawn(async move {
            let info = call("shell", "libraries");
            let asked = retry_once("libraries", || api.views(&info)).await;
            match asked {
                Ok(page) => {
                    libraries.set(page.items);
                    libraries_failed.set(false);
                }
                // The last list, if any, stays; with none the bar says so.
                Err(_) => libraries_failed.set(true),
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
            let info = call("shell", "trail");
            retry_once("trail", || trail::load(&api, &info, id))
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
                if kept_any() {
                    spawn(async move {
                        platform::sleep_ms(SLOW_CONNECT_MS).await;
                        let m = machine.peek();
                        if m.epoch() == epoch && *m.phase() == Phase::Connecting {
                            slow_connect.set(true);
                        }
                    });
                }
                if let Some(old) = task.write().take() {
                    old.cancel();
                }
                // A reconnect keeps the old API until the new one resumes,
                // so the screens keep what they loaded.
                task.set(Some(spawn(connect(machine, api, outdated, epoch, delay))));
            }
            Phase::Connecting => {}
            Phase::Ready => {
                if *slow_connect.peek() {
                    slow_connect.set(false);
                }
            }
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
            let info = call("shell", "me");
            match retry_once("me", || api.me(&info)).await {
                Ok(me) => {
                    reconcile_kept_books(&me.id);
                    claim_queue(queue, held, &me.id);
                    Some(me)
                }
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
    // With the native API off there is no session to ask for a manage
    // token, so Manage is the older app with its own sign-in.
    if route == Route::Manage && matches!(machine.read().phase(), Phase::NotEnabled) {
        return rsx! { RootApp {} };
    }
    if machine.read().shows_screens() {
        let notice = {
            let m = machine.read();
            if m.reconnecting() {
                None
            } else {
                m.notice().map(str::to_owned)
            }
        };
        let reconnecting = machine.read().reconnecting();
        let me = me.read().clone().flatten();
        // The update notice waits while the full player is up: it sat over
        // the picture for the whole film, and nothing in it is urgent there.
        let on_player = {
            let stage = stage.read();
            stage.item.is_some() && stage.view == View::Full
        };
        return rsx! {
            Shell {
                in_group: shell_group_view.read().active,
                libraries: libraries(),
                libraries_failed: libraries_failed(),
                trail: trail.read().clone().unwrap_or_default(),
                account: rsx! {
                    WhatsNewButton {}
                    AccountMenu {
                        me,
                        kept_books: move |()| kept_book_count(),
                        on_sign_out: move |_| {
                            if machine.peek().signing_out() {
                                return;
                            }
                            machine.write().apply(Event::SignOutSubmitted);
                            report::forget();
                            spawn(async move {
                                let reply = platform::sign_out().await;
                                machine.write().apply(Event::SignOutAnswered(reply));
                                // Signed out: no kept book stays on the device.
                                // The queue is emptied by the sign-in effect.
                                if matches!(machine.peek().phase(), Phase::SignIn { .. }) {
                                    forget_kept_books("sign-out").await;
                                }
                            });
                        },
                    }
                },
                search: rsx! { TopSearch {} },
                if let Some(server) = outdated().filter(|_| !on_player && !update_dismissed()) {
                    if platform::can_reload() {
                        p { class: "pharos-error", role: "alert",
                            "Pharos has been updated. Reload to keep using it. "
                            button { class: "pharos-button", onclick: move |_| platform::reload(), "Reload" }
                            " "
                            button { class: "pharos-button", onclick: move |_| update_dismissed.set(true), "Dismiss" }
                        }
                    } else {
                        // An installed app has nothing to reload: the
                        // update is a new build.
                        p { class: "pharos-error", role: "alert",
                            {app_outdated_line(pharos_client::SCHEMA_VERSION, server)}
                            " "
                            button { class: "pharos-button", onclick: move |_| update_dismissed.set(true), "Dismiss" }
                        }
                    }
                }
                if reconnecting {
                    p { class: "pharos-error pharos-connection-shell", role: "alert", {CONNECTION_LOST} }
                }
                if let Some(message) = notice {
                    ErrorNote { message }
                }
                if let Some(line) = flash.line.read().clone() {
                    FlashNote { text: line.text, error: line.error }
                }
                Screen { route: route.clone() }
                StageView {}
                super::keys::KeyLayer { route: route.clone(), on_palette: move |_| {
                    search.clear();
                    palette.0.set(true);
                } }
                WhatsNew { route }
                if (palette.0)() {
                    PaletteScreen {}
                }
                if (report_open.0)() {
                    ReportScreen {}
                }
            }
        };
    }
    let phase = machine.read().phase().clone();
    match phase {
        Phase::Connecting | Phase::Ready => {
            let m = machine.read();
            // Cut off from the server, kept books still open (spec 036 US3).
            let cut_off = m.waiting().is_some() || platform_offline() || slow_connect();
            if let Some(offline) = cut_off.then(|| offline_start(&route)).flatten() {
                return offline;
            }
            match m.waiting() {
                Some(why) => rsx! {
                    StatusView {
                        title: "Waiting for the server…",
                        detail: format!("{why} Trying again in {} s.", m.delay_ms().div_ceil(1000)),
                        action: None,
                    }
                },
                None => rsx! {
                    StatusView {
                        title: "Connecting…",
                        detail: "Opening a connection to the server.",
                        action: None,
                    }
                },
            }
        }
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
                after: platform::offers_quick_connect().then(|| rsx! {
                    QuickConnectSignIn {
                        on_signed_in: move |reply| machine.write().apply(Event::SignInAnswered(reply)),
                    }
                }),
            }
        },
    }
}

/// A Quick Connect code to approve from a signed-in device (Settings,
/// Approve a device), for a remote that makes typing a password slow. It
/// mints a code on mounting, replaces one that expires, and signs in once
/// the code is approved; leaving the page stops it.
#[component]
fn QuickConnectSignIn(on_signed_in: EventHandler<SignInReply>) -> Element {
    let mut code = use_signal(|| None::<String>);
    let _sign_in = use_future(move || async move {
        let reply = platform::quick_connect(move |minted| code.set(Some(minted))).await;
        on_signed_in.call(reply);
    });
    rsx! {
        section { class: "pharos-login pharos-quick-connect", aria_label: "Quick Connect",
            h2 { "Or use Quick Connect" }
            match code() {
                Some(code) => rsx! {
                    p { class: "pharos-quick-connect-code", aria_live: "polite", "{code}" }
                    p { class: "pharos-muted",
                        "On a signed-in device, open Settings, then Approve a device, and enter this code."
                    }
                },
                None => rsx! { p { class: "pharos-muted", "Getting a code…" } },
            }
        }
    }
}

/// One connection attempt: wait out the backoff, open and resume, then
/// report the socket's close.
async fn connect(
    mut machine: Signal<Machine>,
    mut api: Signal<Option<NativeApi>>,
    mut outdated: Signal<Option<u32>>,
    epoch: u32,
    delay_ms: u32,
) {
    platform::sleep_ms(delay_ms).await;
    let event = match platform::open(&call("connect", "resume")).await {
        Ok(mut open) => {
            if let Some(server) = open
                .server_schema
                .filter(|&v| pharos_client::is_outdated_by(v))
            {
                platform::warn(&format!(
                    "pharos: server schema {server} is newer than this page's {}",
                    pharos_client::SCHEMA_VERSION
                ));
                outdated.set(Some(server));
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
/// As [`settle`], for a play: the failure keeps its kind and trace id for
/// the panel's hint and details line (038 FR-403/404).
fn settle_start<T>(
    mut machine: Signal<Machine>,
    result: Result<T, ClientError>,
) -> Result<T, StartFailure> {
    result.map_err(|e| {
        if e.needs_sign_in() {
            machine.write().apply(Event::NeedsSignIn);
        }
        StartFailure::from_client(&e)
    })
}

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
        // Remounted per title so the version pick is read for each one.
        Route::Title(id) => remount_on(&id.to_string(), rsx! { TitleScreen { id } }),
        Route::Person(id) => rsx! { PersonScreen { id } },
        Route::Favourites => rsx! { FavouritesScreen {} },
        // The stage beside the screen draws the player (spec 020).
        Route::Watch(_) => rsx! {},
        Route::Read(id) => remount_on(&id.to_string(), reader_screen(id)),
        Route::SignIn => rsx! { Redirect { to: Route::Home } },
        Route::Manage => rsx! { super::manage::ManageScreen {} },
        Route::Appearance => {
            rsx! { SettingsLayout { current: Section::Appearance, AppearanceScreen {} } }
        }
        Route::Account => rsx! { SettingsLayout { current: Section::Account, AccountScreen {} } },
        Route::ApproveDevice => {
            rsx! {
                SettingsLayout { current: Section::Devices,
                    ApproveDeviceScreen {}
                    MyDevicesScreen {}
                }
            }
        }
        Route::Playback => {
            rsx! { SettingsLayout { current: Section::Playback, PlaybackScreen {} } }
        }
        Route::Privacy => rsx! { SettingsLayout { current: Section::Privacy, PrivacyScreen {} } },
        Route::Offline => rsx! { SettingsLayout { current: Section::Offline, {on_this_device()} } },
        Route::Find => rsx! { FindScreen {} },
        Route::Together => rsx! { TogetherScreen {} },
        Route::TogetherGroup(id) => rsx! { InviteScreen { id } },
        Route::NotFound(path) => rsx! { NotFoundView { path } },
    }
}

/// Deletes every kept book and its record (spec 036 FR-005).
#[cfg(all(feature = "web", target_arch = "wasm32"))]
async fn forget_kept_books(why: &str) {
    super::kept_web::clear_all(why).await;
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
async fn forget_kept_books(_why: &str) {}

/// How many books are kept on this device, for Sign out's warning; `None`
/// when the record cannot be read.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
fn kept_book_count() -> Option<usize> {
    super::kept_web::count()
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
fn kept_book_count() -> Option<usize> {
    Some(0)
}

/// Drops kept books that another user kept, once the signed-in user is known.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
fn reconcile_kept_books(user: &str) {
    super::kept_web::reconcile(user);
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
fn reconcile_kept_books(_user: &str) {}

/// Settings, Offline: the books kept on this device. The cache is the
/// browser's, so the other builds say so.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
fn on_this_device() -> Element {
    rsx! { super::kept_web::OnThisDevice {} }
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
fn on_this_device() -> Element {
    rsx! {
        section { class: "pharos-settings-page",
            h2 { "On this device" }
            p { class: "pharos-settings-lede", "Keeping books to read offline works in the browser app." }
        }
    }
}

/// How long the first connection may take before kept books open without it.
/// Short, because a phone in a tunnel reports itself online.
const SLOW_CONNECT_MS: u32 = 3_000;

/// Whether any book is kept on this device: one `localStorage` read, made
/// only when a connection attempt starts.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
fn kept_any() -> bool {
    super::kept_web::read().is_some()
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
fn kept_any() -> bool {
    false
}

/// The app with no connection and books kept on the device: the list, or
/// the reader when a kept book was asked for. `None` when nothing is kept,
/// so the usual waiting page shows. Browser only; the cache is the browser's.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
fn offline_start(route: &Route) -> Option<Element> {
    let store = super::kept_web::read()?;
    Some(match route {
        Route::Read(id) if store.find(&id.to_string()).is_some() => {
            remount_on(&id.to_string(), reader_screen(*id))
        }
        _ => rsx! { super::kept_web::OfflineHome {} },
    })
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
fn offline_start(_route: &Route) -> Option<Element> {
    None
}

#[cfg(all(feature = "web", target_arch = "wasm32"))]
fn platform_offline() -> bool {
    super::kept_web::is_offline()
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
fn platform_offline() -> bool {
    false
}

/// The reader draws with `quire-web`, which only the browser build has.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
fn reader_screen(id: ItemRef) -> Element {
    rsx! { super::reader_web::ReaderScreen { id } }
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
fn reader_screen(_id: ItemRef) -> Element {
    rsx! { ErrorNote { message: "Books open in the browser app." } }
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

type OnMedia = Box<dyn FnMut(MediaEvent)>;

/// The player's media handler, for `timeupdate`s that arrive outside
/// Dioxus events (`platform::on_time_update`).
#[derive(Default)]
struct Ticks {
    on_media: RefCell<Option<OnMedia>>,
    media_id: Cell<&'static str>,
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
            if super::stage::grip_shown(floating) {
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
        let Some(generation) = generation else {
            self.hits.set(Hits::Idle);
            return;
        };
        // Not "no matches" until the reply says so (B826).
        self.hits.set(Hits::Searching);
        spawn(async move {
            platform::sleep_ms(DEBOUNCE_MS).await;
            if !self.find.peek().is_current(generation) {
                return;
            }
            let Some(api) = ctx.api.peek().clone() else {
                self.hits.set(Hits::Idle);
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

/// Whether the "Report a problem" sheet is open (016 US5).
#[derive(Clone, Copy)]
struct ReportOpen(Signal<bool>);

/// The sheet over the page. What it will send is captured and rendered once,
/// when it opens, and Send posts those very bytes.
#[component]
fn ReportScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let mut open = use_context::<ReportOpen>().0;
    let preview = use_hook(|| report::render(&report::ReportContext::capture()));
    let mut note = use_signal(String::new);
    let mut sending = use_signal(|| false);
    let mut result = use_signal(|| None::<Result<String, String>>);
    rsx! {
        report::ReportView {
            open: true,
            note: note(),
            preview: preview.clone(),
            sending: sending(),
            result: result(),
            on_note: move |text: String| note.set(text),
            on_send: move |_| {
                if *sending.peek() {
                    return;
                }
                let Some(api) = ctx.api.peek().clone() else {
                    result.set(Some(Err("not signed in".to_owned())));
                    return;
                };
                sending.set(true);
                let bytes = preview.clone().into_bytes();
                let text = note.peek().clone();
                spawn(async move {
                    let size = bytes.len();
                    let sent = api
                        .report_problem(&call("telemetry", "report"), &text, &bytes)
                        .await;
                    let outcome = settle(ctx.machine, sent);
                    match &outcome {
                        Ok(id) => platform::info(&format!("pharos: report sent id={id} bytes={size}")),
                        Err(e) => platform::warn(&format!("pharos: report failed bytes={size} error={e}")),
                    }
                    sending.set(false);
                    result.set(Some(outcome));
                });
            },
            on_close: move |_| open.set(false),
            on_copy: move |id: String| {
                spawn(async move {
                    if !platform::copy_text(&id).await {
                        platform::warn(&format!("pharos: report id {id} not copied: clipboard refused"));
                    }
                });
            },
        }
    }
}

/// The search page at `/find?q=` (FR-207). It shares the top bar's search,
/// takes its term from the URL on arrival, and keeps the URL in step as the
/// term changes, so Back from a match returns to these results.
#[component]
fn FindScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let search = use_context::<Search>();
    use_hook(move || {
        let term = find::term_of(&platform::current_search());
        // The URL is the page's state: no term starts it empty, rather than
        // writing the last search back into the URL. Back to the same term
        // keeps the matches it already has.
        if term.trim().is_empty() {
            if !search.text.peek().is_empty() {
                search.clear();
            }
        } else if search.find.peek().term() != term.trim() {
            search.edit(ctx, term);
        }
    });
    use_effect(|| platform::focus_by_id(super::views::FIND_INPUT));
    use_effect(move || {
        let query = find::find_query(&(search.text)());
        let here = Route::parse(&platform::current_path()) == Route::Find;
        if here && platform::current_search() != query {
            platform::replace(&format!("{}{query}", Route::Find.path()));
        }
    });
    rsx! {
        super::views::FindView {
            text: (search.text)(),
            hits: (search.hits)(),
            on_input: move |value| search.edit(ctx, value),
        }
    }
}

/// Opens the search page on `term` (FR-207).
fn open_find(nav: Nav, term: &str) {
    nav.0.call(Route::Find);
    platform::replace(&format!("{}{}", Route::Find.path(), find::find_query(term)));
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
    // Selected, so typing replaces a term the palette opens on.
    use_effect(|| platform::select_field(PALETTE_INPUT));
    let kept = move || {
        platform::stored_theme(palette::SEARCHES_KEY)
            .unwrap_or_else(|e| {
                platform::warn(&format!("palette: past searches unreadable: {e}"));
                None
            })
            .unwrap_or_default()
    };
    let searches = use_signal(move || palette::searches(&kept()));
    // A match opened from a term keeps the term, as a past search to pick.
    let remember = move || {
        let term = search.text.peek().trim().to_owned();
        let matched = matches!(&*search.hits.peek(), Hits::Found(page) if !page.items.is_empty());
        if term.is_empty() || !matched {
            return;
        }
        if let Err(e) =
            platform::store_theme(palette::SEARCHES_KEY, &palette::remember(&kept(), &term))
        {
            platform::warn(&format!("palette: search not kept: {e}"));
        }
    };
    let account = try_use_context::<AccountMe>();
    let admin =
        move || account.is_some_and(|me| me.0.read().clone().flatten().is_some_and(|m| m.is_admin));
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
            &searches.read(),
            &palette::places(&libraries, admin()),
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
    let mut report_open = use_context::<ReportOpen>().0;
    let mut run_command = move |command: palette::Command| {
        report::note(&format!("palette: {}", command.label()));
        close();
        match command {
            palette::Command::ReportProblem => report_open.set(true),
        }
    };
    let on_key = move |evt: KeyboardEvent| match evt.key() {
        Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::ArrowUp) => {
            evt.prevent_default();
            let down = evt.key() == Key::Named(NamedKey::ArrowDown);
            let next = find::step(*active.peek(), rows.peek().len(), down);
            active.set(next);
        }
        Key::Named(NamedKey::Enter) => {
            evt.prevent_default();
            let picked = active.peek().and_then(|at| rows.peek().get(at).cloned());
            match picked {
                Some(Row::Search(term)) => {
                    active.set(Some(0));
                    search.edit(ctx, term);
                }
                Some(Row::Command(command)) => run_command(command),
                Some(row) => {
                    if let Some(route) = row.route() {
                        remember();
                        close();
                        nav.0.call(route);
                    }
                }
                None => {}
            }
        }
        // Closes rather than letting the search input clear itself.
        Key::Named(NamedKey::Escape) => {
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
            searching: matches!(&*search.hits.read(), Hits::Searching),
            on_input: move |value| {
                active.set(Some(0));
                search.edit(ctx, value);
            },
            on_key,
            on_pick: move |_| {
                remember();
                close();
            },
            on_close: move |_| close(),
            on_command: run_command,
        }
    }
}

/// What an installed app (the TV's) says when the server is newer than
/// it: both versions, and how to update (FR-610). There is nothing to
/// reload: the update is a new build.
fn app_outdated_line(app: u32, server: u32) -> String {
    format!(
        "The Pharos server has been updated (version {server}); this app is version {app}. \
         Some things may not work until you install the latest Pharos TV app."
    )
}

/// Whether the top bar offers a Search button in place of the search box:
/// on the Android TV app (FR-602).
const TV_SEARCH_BUTTON: bool = cfg!(target_os = "android");

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
    // A TV has no keyboard to type into the box from and no Ctrl+K: the bar
    // holds a Search button that opens the palette, its input focused, where
    // the remote's keyboard comes up (FR-602).
    if TV_SEARCH_BUTTON {
        return rsx! {
            button {
                class: "pharos-search-open",
                r#type: "button",
                onclick: move |_| {
                    platform::info("pharos: palette opened via=banner");
                    palette.0.set(true);
                },
                "Search"
            }
        };
    }
    let on_key = move |evt: KeyboardEvent| match evt.key() {
        Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::ArrowUp) => {
            evt.prevent_default();
            open.set(true);
            let down = evt.key() == Key::Named(NamedKey::ArrowDown);
            let next = find::step(*active.peek(), suggestions().len(), down);
            active.set(next);
        }
        Key::Named(NamedKey::Enter) => {
            evt.prevent_default();
            let picked = active
                .peek()
                .and_then(|at| suggestions().into_iter().nth(at));
            open.set(false);
            active.set(None);
            match picked {
                Some(item) => nav.0.call(open_route(&item)),
                None if !search.text.peek().trim().is_empty() => {
                    open_find(nav, &search.text.peek());
                }
                None => {}
            }
        }
        // A search input clears itself on Escape; the first press only
        // closes the list, and a second, with the list shut, clears.
        Key::Named(NamedKey::Escape) if *open.peek() => {
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
            on_all: move |_| {
                open.set(false);
                open_find(nav, &search.text.peek());
            },
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

/// Leave the group this device is in, from outside the player (B1361). The
/// leave outlives the page, like a join.
fn leave_group(together: Together) {
    // Not filtered on `active`: mid-rejoin the session is briefly inactive
    // while the page still shows the group, and `leave` is what cancels that
    // rejoin. A click there must not be dropped.
    let Some(session) = together.current() else {
        platform::info("pharos: group leave skipped from=together cause=no_session");
        return;
    };
    let mut error = together.error;
    error.set(None);
    platform::info("pharos: group leave from=together");
    // The session logs a refused leave itself; the device is out of the
    // group either way, and the server's own prune finishes the job.
    spawn_forever(async move { session.leave().await });
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
    // The client is read per request, so Try again after a reconnect asks
    // over the new connection.
    let mut groups = use_resource(move || list_groups(group_client(ctx)));
    super::live::use_group_list_updates(ctx.api, groups);
    // What each group is watching, looked up once the list is in.
    let watching = use_resource(move || {
        let listed = groups.read().clone();
        let native = ctx.api.peek().clone();
        async move {
            let (Some(Ok(listed)), Some(native)) = (listed, native) else {
                return Vec::new();
            };
            group_titles(&native, &listed).await
        }
    });
    // The group this device is in, so the page can offer Leave group (B1361).
    let current = {
        let view = (together.view)();
        view.id.clone().filter(|_| view.active)
    };
    // While the list loads or fails, the way out of the group still shows.
    let leave_now = current.is_some().then(|| {
        let together = together.clone();
        rsx! { InAGroup { on_leave: move |()| leave_group(together.clone()) } }
    });
    let state = groups.read();
    match state.as_ref() {
        None => rsx! {
            {leave_now}
            Loading {}
        },
        Some(Err(e)) => {
            platform::warn(&format!("pharos: together list failed error={}", e.0));
            rsx! {
                {leave_now}
                LoadFailure {
                    error: LoadError {
                        headline: "We couldn't list the groups watching together.".to_owned(),
                        raw: e.0.clone(),
                        trace_id: None,
                    },
                    on_retry: move |()| groups.restart(),
                }
            }
        }
        Some(Ok(listed)) => rsx! {
            if let Some(message) = together.error.read().clone() {
                ErrorNote { message }
            }
            TogetherView {
                groups: listed.clone(),
                on_join: {
                    let together = together.clone();
                    move |id: String| join_group(ctx, together.clone(), nav, id)
                },
                watching: watching.read().clone().unwrap_or_default(),
                current: current.clone(),
                on_leave: move |()| leave_group(together.clone()),
            }
        },
    }
}

/// Each listed group's item as the viewer reads it ("Show · S1 · E2"), by
/// group id. A lookup that fails leaves that group's title out.
async fn group_titles(api: &NativeApi, groups: &[GroupSummary]) -> Vec<(String, String)> {
    let mut titles = Vec::new();
    for group in groups {
        let Some(item) = group.item else {
            continue;
        };
        match api.detail(&call("together", "list"), item).await {
            Ok(detail) => {
                let (title, line) = super::views::watch_heading(&detail.item);
                let text = match line {
                    Some(line) => format!("{title} · {line}"),
                    None => title,
                };
                titles.push((group.id.clone(), text));
            }
            Err(e) => platform::warn(&format!(
                "pharos: together title lookup failed group={} error={e}",
                group.id
            )),
        }
    }
    titles
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
    let mut description_language_failed = use_signal(|| None::<String>);
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
            description_language_failed: description_language_failed(),
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
                                description_language_failed.set(None);
                                true
                            }
                            Err(e) => {
                                report(&e, "set description language");
                                description_language_failed.set(Some(e.to_string()));
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

/// Settings → Devices, "Your devices": loads the user's devices and saves a
/// device's sound formats on each toggle, one save per device at a time.
#[component]
fn MyDevicesScreen() -> Element {
    use super::my_devices::{with_saved, MyDevicesState, MyDevicesView};
    let ctx = use_context::<Ctx>();
    let mut state = use_signal(|| MyDevicesState::Loading);
    let mut saving = use_signal(std::collections::BTreeSet::<String>::new);
    let mut problem = use_signal(|| None::<String>);
    // Counts connections. A call on a closed socket can hang instead of
    // failing, so a new connection drops every save still in flight: its
    // device can be changed again, and a late answer from the old one is
    // ignored.
    let mut connection = use_signal(|| 0u64);
    let report = move |e: &ClientError, what: &str| {
        if e.needs_sign_in() {
            let mut machine = ctx.machine;
            machine.write().apply(Event::NeedsSignIn);
        }
        platform::warn(&format!("my_devices: {what} failed: {e}"));
    };
    // Loads again on a new connection, which may be another account.
    use_effect(move || {
        let api = ctx.api.read().clone();
        *connection.write() += 1;
        let dropped = std::mem::take(&mut *saving.write());
        if !dropped.is_empty() {
            platform::warn(&format!(
                "my_devices: connection changed with {} save(s) in flight; dropped: {dropped:?}",
                dropped.len()
            ));
            problem.set(Some(
                "The connection dropped before the sound formats saved. Try again.".to_owned(),
            ));
        }
        let Some(api) = api else {
            return;
        };
        // The old list's toggles would save on the new connection and then
        // be overwritten by this load's older snapshot; hide them meanwhile.
        state.set(MyDevicesState::Loading);
        let loaded_on = *connection.peek();
        spawn(async move {
            let result = api.my_devices(&call("my_devices", "my_devices")).await;
            if *connection.peek() != loaded_on {
                platform::warn(
                    "my_devices: device list answered after the connection changed; ignored",
                );
                return;
            }
            match result {
                Ok(devices) => state.set(MyDevicesState::Loaded(devices)),
                Err(e) => {
                    report(&e, "my devices");
                    state.set(MyDevicesState::Failed(format!(
                        "Couldn't load your devices: {e}"
                    )));
                }
            }
        });
    });
    rsx! {
        MyDevicesView {
            state: state(),
            saving: saving(),
            problem: problem(),
            on_change: move |(device_id, codecs): (String, Vec<String>)| {
                let Some(api) = ctx.api.peek().clone() else {
                    return;
                };
                if !saving.write().insert(device_id.clone()) {
                    return;
                }
                problem.set(None);
                let started_on = *connection.peek();
                spawn(async move {
                    let result = api
                        .set_device_converted_audio(
                            &call("my_devices", "set_device_converted_audio"),
                            &device_id,
                            &codecs,
                        )
                        .await;
                    if *connection.peek() != started_on {
                        platform::warn(&format!(
                            "my_devices: save for {device_id} answered after the connection changed; ignored"
                        ));
                        return;
                    }
                    match result {
                        Ok(()) => {
                            let current = state.peek().clone();
                            if let MyDevicesState::Loaded(devices) = current {
                                state.set(MyDevicesState::Loaded(with_saved(devices, &device_id, codecs)));
                            }
                        }
                        Err(e) => {
                            report(&e, "set device converted audio");
                            problem.set(Some(format!("Couldn't save the sound formats: {e}")));
                        }
                    }
                    saving.write().remove(&device_id);
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
    let mut effects_level = use_signal(stored_effects_level);
    let mut speaker_colours = use_signal(stored_speaker_colours);
    let mut effects_detail = use_signal(stored_effects_detail);
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
    let effects_level_now = *effects_level.read();
    let speaker_colours_now = *speaker_colours.read();
    let effects_detail_now = *effects_detail.read();

    rsx! {
        AppearanceView {
            themes: list,
            auto,
            editor,
            notice: notice_now,
            library_transition: library_transition_now,
            on_library_transition: move |fx: LibraryTransition| {
                if let Err(e) = store_library_transition(fx) {
                    show(notice, not_saved("the library transition", &e, Lasts::UntilReload));
                }
                platform::apply_library_transition(fx);
                library_transition.set(fx);
                platform::info(&format!("pharos: library_transition set={}", fx.as_str()));
            },
            overlay_effect: overlay_effect_now,
            on_overlay_effect: move |fx: OverlayEffect| {
                if let Err(e) = store_overlay_effect(fx) {
                    show(notice, not_saved("the controls effect", &e, Lasts::UntilReload));
                }
                platform::apply_overlay_effect(fx);
                overlay_effect.set(fx);
                platform::info(&format!("pharos: overlay_effect set={}", fx.as_str()));
            },
            effects_level: effects_level_now,
            on_effects_level: move |level: EffectsLevel| {
                if let Err(e) = store_effects_level(level) {
                    show(notice, not_saved("the subtitle effects", &e, Lasts::UntilReload));
                }
                effects_level.set(level);
                platform::info(&format!("pharos: effects_level set={}", level.as_str()));
            },
            speaker_colours: speaker_colours_now,
            on_speaker_colours: move |on: bool| {
                if let Err(e) = store_speaker_colours(on) {
                    show(notice, not_saved("the speaker colours", &e, Lasts::UntilReload));
                }
                speaker_colours.set(on);
                platform::info(&format!("pharos: speaker_colours set={on}"));
            },
            effects_detail: effects_detail_now,
            on_effects_detail: move |detail: EffectsDetail| {
                if let Err(e) = store_effects_detail(detail) {
                    show(notice, not_saved("the effects detail", &e, Lasts::UntilReload));
                }
                // Asking for more is a fresh try: an old drop's cap would
                // hold Rich at the tier that dropped.
                let cleared = detail == EffectsDetail::Rich
                    && stored_tier_drop().is_some()
                    && clear_tier_drop().is_ok();
                effects_detail.set(detail);
                platform::info(&format!(
                    "pharos: effects_detail set={} tier_cap_cleared={cleared}",
                    detail.as_str()
                ));
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

/// One account pref the Playback page edits: the value on screen and the
/// saves that put it on the server.
#[derive(Clone, Copy)]
struct AccountPref {
    value: Signal<String>,
    /// Set once the viewer picks, so the mount read, if it answers later,
    /// does not put the stored (older) value back on screen.
    picked: Signal<bool>,
    /// Saves run one at a time, so quick picks can't land out of order.
    saves: Signal<Option<account_settings::SerialPref>>,
}

/// Read `key` once into a new [`AccountPref`].
fn use_account_pref(
    ctx: Ctx,
    notice: Signal<Option<Notice>>,
    key: &'static str,
    what: &'static str,
) -> AccountPref {
    let mut pref = AccountPref {
        value: use_signal(String::new),
        picked: use_signal(|| false),
        saves: use_signal(|| None),
    };
    use_hook(move || {
        let Some(api) = ctx.api.peek().clone() else {
            return;
        };
        spawn(async move {
            match api.pref(&call("playback", what), key).await {
                Ok(_) if *pref.picked.peek() => {}
                Ok(value) => pref.value.set(
                    value
                        .map(|v| String::from_utf8_lossy(&v).into_owned())
                        .unwrap_or_default(),
                ),
                Err(e) => {
                    if e.needs_sign_in() {
                        let mut machine = ctx.machine;
                        machine.write().apply(Event::NeedsSignIn);
                    }
                    platform::warn(&format!("playback: read {what} failed: {e}"));
                    notify(
                        notice,
                        format!("Could not load your {what} setting: {e}"),
                        true,
                    );
                }
            }
        });
    });
    pref
}

impl AccountPref {
    /// The viewer picked `value`: show it, save it, and on a failed save put
    /// back what the server holds and say so.
    fn pick(
        mut self,
        ctx: Ctx,
        notice: Signal<Option<Notice>>,
        key: &'static str,
        what: &'static str,
        value: String,
    ) {
        let Some(api) = ctx.api.peek().clone() else {
            return;
        };
        self.picked.set(true);
        let before = self.value.peek().clone();
        self.value.set(value.clone());
        let Some(mut next) = self
            .saves
            .write()
            .get_or_insert_with(|| account_settings::SerialPref::new(before))
            .pick(value)
        else {
            return;
        };
        spawn(async move {
            let mut failed = None;
            loop {
                let ok = match api
                    .set_pref(&call("playback", what), key, next.as_bytes())
                    .await
                {
                    Ok(()) => {
                        platform::info(&format!("playback: {what} set value={next}"));
                        true
                    }
                    Err(e) => {
                        if e.needs_sign_in() {
                            let mut machine = ctx.machine;
                            machine.write().apply(Event::NeedsSignIn);
                        }
                        platform::warn(&format!("playback: set {what} failed: {e}"));
                        failed = Some(account_settings::refusal_message(&e));
                        false
                    }
                };
                let mut saves = self.saves.write();
                let Some(saves) = saves.as_mut() else { return };
                match saves.finished(next, ok) {
                    Some(queued) => next = queued,
                    None => {
                        let saved = saves.saved().to_string();
                        if *self.value.peek() != saved {
                            self.value.set(saved);
                        }
                        if let Some(reason) = failed {
                            notify(
                                notice,
                                format!("Could not save your {what} setting: {reason}"),
                                true,
                            );
                        }
                        return;
                    }
                }
            }
        });
    }
}

/// The Playback page: this device's quality ceiling, subtitle size and
/// crossfade, each kept in local storage.
#[component]
fn PlaybackScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let notice = use_signal(|| None::<Notice>);
    let audio_language = use_account_pref(
        ctx,
        notice,
        playback_settings::AUDIO_LANGUAGE_PREF,
        "audio language",
    );
    let subtitle_language = use_account_pref(
        ctx,
        notice,
        playback_settings::SUBTITLE_LANGUAGE_PREF,
        "subtitle language",
    );
    let subtitle_mode = use_account_pref(
        ctx,
        notice,
        playback_settings::SUBTITLE_MODE_PREF,
        "subtitle mode",
    );
    let mut max_quality = use_signal(|| stored_bitrate(MAX_BITRATE_KEY, parse_max_bitrate));
    let mut subtitle_size = use_signal(stored_subtitle_size);
    let mut crossfade = use_signal(crossfade_seconds);
    let mut mse_engine = use_signal(stored_mse_engine);
    let mut episode_turn = use_signal(stored_episode_turn);
    rsx! {
        playback_settings::PlaybackView {
            notice: notice.read().clone(),
            audio_language: audio_language.value.read().clone(),
            on_audio_language: move |value: String| {
                audio_language.pick(ctx, notice, playback_settings::AUDIO_LANGUAGE_PREF, "audio language", value)
            },
            subtitle_language: subtitle_language.value.read().clone(),
            on_subtitle_language: move |value: String| {
                subtitle_language.pick(ctx, notice, playback_settings::SUBTITLE_LANGUAGE_PREF, "subtitle language", value)
            },
            subtitle_mode: subtitle_mode.value.read().clone(),
            on_subtitle_mode: move |value: String| {
                subtitle_mode.pick(ctx, notice, playback_settings::SUBTITLE_MODE_PREF, "subtitle mode", value)
            },
            episode_turn: episode_turn(),
            on_episode_turn: move |choice: EpisodeTurn| {
                if let Err(e) = store_episode_turn(choice) {
                    show(notice, not_saved("the episode transition", &e, Lasts::UntilReload));
                }
                episode_turn.set(choice);
            },
            max_quality: max_quality(),
            on_max_quality: move |bps: u64| {
                store_bitrate(MAX_BITRATE_KEY, bps);
                max_quality.set(bps);
                platform::info(&format!("pharos: max_bitrate set={}", max_bitrate_label(bps)));
            },
            subtitle_size: subtitle_size(),
            on_subtitle_size: move |size: SubtitleSize| {
                if let Err(e) = set_subtitle_size(size) {
                    show(notice, not_saved("the subtitle size", &e, Lasts::UntilReload));
                }
                subtitle_size.set(size);
            },
            crossfade_s: crossfade(),
            on_crossfade: move |seconds| {
                set_crossfade_seconds(seconds);
                crossfade.set(seconds);
            },
            engine: mse_engine(),
            on_engine: move |engine: MseEngine| {
                // Only a saved choice is shown: the engine is read at the next play.
                if let Err(e) = platform::store_theme(MSE_ENGINE_KEY, engine.as_str()) {
                    platform::warn(&format!(
                        "pharos: engine storage write failed engine={} error={e}",
                        engine.as_str()
                    ));
                    notify(
                        notice,
                        format!("Could not save the streaming engine on this device: {e}."),
                        true,
                    );
                    return;
                }
                mse_engine.set(engine);
                platform::info(&format!("pharos: engine set={}", engine.as_str()));
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
    let mut report_open = use_context::<ReportOpen>();
    let mut reports = use_signal(Vec::new);
    let mut reload = use_signal(|| 0_u32);
    // Read again after the sheet closes: a report just sent is in the list.
    use_resource(move || async move {
        let _ = reload();
        if (report_open.0)() {
            return;
        }
        let Some(api) = ctx.api.peek().clone() else {
            return;
        };
        let listed = api
            .list_reports(&call("telemetry", "list_reports"), false)
            .await;
        // A server keeping no reports, or one older than schema 35, is a
        // normal state: an empty list, not an alert. `unavailable` also
        // covers a store that failed to read, so it leaves a warning.
        let older = listed
            .as_ref()
            .err()
            .is_some_and(ClientError::is_unimplemented);
        let unavailable = matches!(
            &listed,
            Err(ClientError::Api {
                kind: "unavailable",
                ..
            })
        );
        match settle(ctx.machine, listed) {
            Ok(rows) => reports.set(rows),
            Err(e) if older || unavailable => {
                if unavailable {
                    platform::warn(&format!("pharos: report list unavailable: {e}"));
                }
                reports.set(Vec::new());
            }
            Err(e) => notify(
                notice,
                format!("Could not load your problem reports: {e}"),
                true,
            ),
        }
    });
    rsx! {
        privacy::PrivacyView {
            reports: reports.read().clone(),
            on_report: move |()| {
                report::note("settings: report a problem");
                report_open.0.set(true);
            },
            on_delete_report: move |id: String| {
                let Some(api) = ctx.api.peek().clone() else {
                    return;
                };
                spawn(async move {
                    let deleted = api.delete_report(&call("telemetry", "delete_report"), &id).await;
                    if let Err(e) = settle(ctx.machine, deleted) {
                        notify(notice, format!("Could not delete report {id}: {e}"), true);
                    }
                    reload.with_mut(|n| *n = n.wrapping_add(1));
                });
            },
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
    // Why the last pick could not be kept on this device (FR-213).
    let mut not_kept = use_signal(|| None::<String>);
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
            not_kept.set(None);
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
            not_kept: not_kept(),
            on_page: move |to: usize| {
                // A failure belongs to the setting that made it, not the next.
                not_kept.set(None);
                page.set(to);
                platform::focus_by_id(whats_new::FIRST_BUTTON_ID);
            },
            on_pick: move |pick: Pick| {
                match pick {
                    Pick::Theme(name) => {
                        platform::info(&format!("pharos: theme set={name} from=whats_new"));
                        not_kept.set(themes.choose_kept(name).err());
                    }
                    Pick::ThemeAuto => {
                        platform::info("pharos: theme set=auto from=whats_new");
                        not_kept.set(themes.choose_kept(device::AUTO.to_owned()).err());
                    }
                    Pick::SubtitleSize(size) => {
                        if let Err(e) = store_subtitle_size(size) {
                            platform::warn(&format!(
                                "pharos: whats_new subtitle_size not kept error={e}"
                            ));
                            not_kept.set(Some(e.to_string()));
                        } else {
                            not_kept.set(None);
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
                            not_kept.set(Some(e.to_string()));
                        } else {
                            not_kept.set(None);
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
                            not_kept.set(Some(e.to_string()));
                        } else {
                            not_kept.set(None);
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

/// Opens "Report a problem" from a playback failure panel, the failure
/// noted first so the report says what the viewer saw and quotes its trace
/// id (038 FR-403/404).
fn report_failure(failure: &PlayFailure) {
    report::note(&failure.crumb());
    if let Some(trace) = &failure.trace_id {
        report::trace(trace);
    }
    platform::info(&format!(
        "pharos: report opened from=playback_failure kind={}",
        failure.kind.label()
    ));
    if let Some(ReportOpen(mut open)) = try_consume_context::<ReportOpen>() {
        open.set(true);
    }
}

/// A path the app has no page for, with the two ways on (FR-210, A23).
#[component]
fn NotFoundView(path: String) -> Element {
    rsx! {
        section { class: "pharos-not-found",
            ErrorNote { message: format!("Nothing at {path}.") }
            nav { class: "pharos-not-found-links", aria_label: "Go to",
                super::views::Link { to: Route::Home, class: "pharos-button", "Home" }
                super::views::Link { to: Route::Find, class: "pharos-button pharos-button-secondary", "Search" }
            }
        }
    }
}

/// How long the shell waits before asking a failed call once more.
const SHELL_RETRY_MS: u32 = 1_000;

/// `ask`, and once more after [`SHELL_RETRY_MS`] if it fails (FR-210, A15):
/// one dropped reply should not leave the shell without its libraries,
/// trail or name. Each failure is logged with its cause.
async fn retry_once<T, E: std::fmt::Display, F: std::future::Future<Output = Result<T, E>>>(
    what: &str,
    mut ask: impl FnMut() -> F,
) -> Result<T, E> {
    let first = ask().await;
    let Err(e) = first else {
        return first;
    };
    platform::info(&format!("pharos: shell {what} failed, asking again: {e}"));
    platform::sleep_ms(SHELL_RETRY_MS).await;
    let again = ask().await;
    if let Err(e) = &again {
        platform::warn(&format!(
            "pharos: shell {what} unavailable after a retry: {e}"
        ));
    }
    again
}

/// Back one page, or home when there is none to go back to.
fn leave_player(nav: Nav) {
    if !platform::go_back() {
        nav.0.call(Route::Home);
    }
}

/// Renders a load's state: loading, its error, or `view` of its value.
fn loaded<T, E: std::fmt::Display>(
    state: Option<&Result<T, E>>,
    view: impl FnOnce(&T) -> Element,
) -> Element {
    match state {
        None => rsx! { Loading {} },
        Some(Err(message)) => rsx! { ErrorNote { message: message.to_string() } },
        Some(Ok(value)) => view(value),
    }
}

/// As [`settle`], for a screen load that offers "Try again": the failure
/// keeps its kind and trace id for the plain sentence and details (B823).
fn settle_load<T>(
    mut machine: Signal<Machine>,
    result: Result<T, ClientError>,
) -> Result<T, LoadError> {
    result.map_err(|e| {
        if e.needs_sign_in() {
            machine.write().apply(Event::NeedsSignIn);
        }
        LoadError::from_client(&e)
    })
}

/// As [`loaded`], with a failure that offers "Try again" through `retry`.
fn loaded_or_retry<T>(
    state: Option<&Result<T, LoadError>>,
    retry: EventHandler<()>,
    view: impl FnOnce(&T) -> Element,
) -> Element {
    match state {
        None => rsx! { Loading {} },
        Some(Err(error)) => rsx! { LoadFailure { error: error.clone(), on_retry: retry } },
        Some(Ok(value)) => view(value),
    }
}

#[component]
fn HomeScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let library = try_use_context::<super::live::LibraryRev>();
    let mut home = use_resource(move || {
        let api = ctx.api.read().clone();
        // Continue Watching and Next Up follow watched marks; the rails
        // follow the library.
        let _ = (ctx.marks)();
        let _ = library.map(|l| (l.0)());
        async move {
            let api = api?;
            Some(settle_load(
                ctx.machine,
                api.home(&call("home", "load")).await,
            ))
        }
    });
    // The last Home shown stands in while this one loads, and a return by
    // Back goes to where it was left (FR-206).
    let revisit = try_use_context::<super::revisit::Revisit>();
    use_effect(move || {
        if let (Some(Some(Ok(loaded))), Some(revisit)) = (&*home.read(), revisit) {
            let mut kept = revisit.home;
            kept.set(Some(loaded.clone()));
        }
    });
    let state = home.read();
    let fresh = state.as_ref().and_then(Option::as_ref);
    let stand_in = match fresh {
        None => revisit.and_then(|r| r.home.peek().clone()).map(Ok),
        Some(_) => None,
    };
    // Placed once content is up: a failure panel would spend the return.
    use_place_on_return(
        Route::Home,
        matches!(fresh.or(stand_in.as_ref()), Some(Ok(_))),
    );
    loaded_or_retry(
        fresh.or(stand_in.as_ref()),
        EventHandler::new(move |()| home.restart()),
        |home: &Home| rsx! { HomeView { home: home.clone() } },
    )
}

/// Once the page at `route` has content (`shown`), scrolls it to where the
/// viewer left it, if they came back to it by Back or Forward (FR-206).
fn use_place_on_return(route: Route, shown: bool) {
    let revisit = try_use_context::<super::revisit::Revisit>();
    // The page last placed: a title screen stays mounted from one title to
    // the next (B555), and each arrival is placed once.
    let mut placed = use_signal(|| None::<String>);
    let path = route.path();
    use_effect(use_reactive!(|(shown, path)| {
        if !shown || placed.peek().as_deref() == Some(path.as_str()) {
            return;
        }
        placed.set(Some(path.clone()));
        if let Some(y) = revisit.and_then(|r| r.place_for(&path)) {
            platform::info(&format!("pharos: revisit place path={path} y={y}"));
            platform::scroll_to_y(y);
        }
    }));
}

#[component]
fn BrowseScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    let library = try_use_context::<super::live::LibraryRev>();
    // The screen stays mounted when a tile or season tab opens another
    // parent, so the load follows `id` instead of running once.
    let mut listing = use_resource(use_reactive!(|(id,)| {
        let api = ctx.api.read().clone();
        let _ = (ctx.marks)();
        let _ = library.map(|l| (l.0)());
        async move {
            let api = api?;
            let result = browse::load(&api, &call("browse", "load"), id, |line| {
                platform::warn(&line)
            })
            .await;
            Some((id, settle_load(ctx.machine, result)))
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
    // The last parent's listing stays up while the next loads (so a tile
    // tap never blanks the page), dimmed and marked busy (B822).
    let busy = state
        .as_ref()
        .and_then(Option::as_ref)
        .is_some_and(|(asked, _)| *asked != id);
    // Keyed by what it shows (`Browse::mount_key`), so another parent's
    // listing mounts afresh once it lands, while a reload of this one or a
    // season tab keeps its place and the last one stays mounted under the
    // busy cue (B1324).
    let shown = state
        .as_ref()
        .and_then(Option::as_ref)
        .map(|(asked, _)| *asked);
    rsx! {
        Busy { busy,
            {loaded_or_retry(
                state.as_ref().and_then(Option::as_ref).map(|(_, result)| result),
                EventHandler::new(move |()| listing.restart()),
                |browse: &browse::Browse| {
                    remount_on(
                        &shown.map(|s| browse.mount_key(s)).unwrap_or_default(),
                        rsx! { SeenBrowse { browse: browse.clone() } },
                    )
                },
            )}
        }
    }
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
fn SeenTitle(
    detail: Detail,
    on_watch_together: EventHandler<(ItemRef, u64)>,
    tracks: Option<Element>,
    version: String,
    on_version: EventHandler<String>,
) -> Element {
    let seen = use_group_seen(vec![detail.item.id]);
    let in_group = (use_context::<Together>().view)().active;
    let PlayFromStart(on_play_from_start) = use_context::<PlayFromStart>();
    let detail_for_keep = detail.clone();
    rsx! {
        TitleView {
            detail,
            on_watch_together,
            on_play_from_start,
            seen,
            in_group,
            tracks,
            version,
            on_version,
            keep: keep_control(&detail_for_keep),
        }
    }
}

/// Keep offline for a readable book. The cache is the browser's, so the
/// other builds have none.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
fn keep_control(detail: &Detail) -> Option<Element> {
    let format = detail.book.as_ref()?.format;
    Some(rsx! {
        super::kept_web::KeepButton {
            id: detail.item.id,
            title: super::views::display_name(&detail.item),
            format,
            place: detail
                .read_position
                .as_ref()
                .map(|p| (p.locator.clone(), p.fraction)),
        }
    })
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
fn keep_control(_detail: &Detail) -> Option<Element> {
    None
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
    let mut detail = use_resource(use_reactive!(|(id,)| {
        let api = ctx.api.read().clone();
        let _ = (ctx.marks)();
        async move {
            let api = api?;
            Some((
                id,
                settle_load(ctx.machine, api.detail(&call("title", "load"), id).await),
            ))
        }
    }));
    // The tracks playback would start on, for the pickers to show, asked
    // only once the title has a choice to offer (a book or a one-track song
    // has none). Without them the pickers start on each list's first entry
    // and still hand on only what the viewer changes.
    let start = use_resource(move || {
        let api = ctx.api.read().clone();
        let offered = detail
            .read()
            .as_ref()
            .and_then(Option::as_ref)
            .filter(|(asked, _)| *asked == id)
            .and_then(|(_, r)| r.as_ref().ok())
            .map(|d| {
                (
                    d.item.kind,
                    d.item.kind.playable()
                        && (d.audio_tracks.len() > 1 || !d.subtitle_tracks.is_empty()),
                )
            });
        async move {
            let api = api?;
            let (kind, true) = offered? else {
                return None;
            };
            let asked = api
                .start_tracks(
                    &call("title", "tracks"),
                    id,
                    &negotiation_capabilities(kind, 0),
                )
                .await;
            let tracks = asked.unwrap_or_else(|e| {
                platform::warn(&format!("pharos: start tracks unavailable: {e}"));
                StartTracks::default()
            });
            Some((id, tracks))
        }
    });
    // Picks made on an earlier visit are not this visit's.
    let mut pending = use_context::<PlayPicks>().0;
    use_effect(use_reactive!(|(id,)| {
        let _ = id;
        pending.set(None);
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
        // In a group, the title's together buttons move that group here
        // (014 US10) rather than leave it for a new one; its item update
        // brings this member to the player.
        if session.play_for_group(id, from_ms) {
            return;
        }
        platform::info(&format!("pharos: group create item={id} from_ms={from_ms}"));
        spawn_forever(async move {
            match session.create(id, from_ms).await {
                Ok(_) => nav.0.call(Route::Watch(id)),
                Err(e) => group_error.set(Some(e.0)),
            }
        });
    };
    let mut picked = use_signal(|| stored_version(id));
    // The title as last shown stands in while it reloads, and a return by
    // Back goes to where it was left (FR-206).
    let revisit = try_use_context::<super::revisit::Revisit>();
    use_effect(move || {
        if let (Some(Some((asked, Ok(loaded)))), Some(revisit)) = (&*detail.read(), revisit) {
            let mut kept = revisit.titles;
            kept.write().put(*asked, loaded.clone());
        }
    });
    let state = detail.read();
    let fresh = state
        .as_ref()
        .and_then(Option::as_ref)
        .filter(|(asked, _)| *asked == id)
        .map(|(_, result)| result);
    let stand_in = match fresh {
        None => revisit
            .and_then(|r| r.titles.peek().get(&id).cloned())
            .map(Ok),
        Some(_) => None,
    };
    let shown = fresh.or(stand_in.as_ref());
    use_place_on_return(Route::Title(id), matches!(shown, Some(Ok(_))));
    let name = shown
        .and_then(|r| r.as_ref().ok())
        .map(|d| super::views::page_name(&d.item));
    use_page_name(id, name);
    rsx! {
        if let Some(message) = group_error.read().clone() {
            ErrorNote { message }
        }
        {loaded_or_retry(
            shown,
            EventHandler::new(move |()| detail.restart()),
            |detail: &Detail| {
                let version = versions::chosen_version(&detail.versions, picked().as_deref()).to_owned();
                let tracks = start
                    .read()
                    .as_ref()
                    .copied()
                    .flatten()
                    .filter(|(asked, _)| *asked == id)
                    // The pickers list the primary's tracks; another
                    // version's indexes name other streams, so it starts on
                    // the user's language rules instead.
                    .filter(|_| version.is_empty())
                    .map(|(_, start)| {
                        // A choice made on another episode of the show
                        // applies here.
                        let series = show_tracks::show_of(detail);
                        let start = series.map_or(start, |s| {
                            let (audio, subtitle) = show_tracks::shown(
                                &show_tracks::load(s),
                                detail,
                                (start.audio, start.subtitle),
                            );
                            StartTracks { audio, subtitle }
                        });
                        rsx! {
                            TrackChoices {
                                key: "{id}",
                                id,
                                audio: detail.audio_tracks.clone(),
                                subtitles: detail.subtitle_tracks.clone(),
                                start,
                                series: series.map(str::to_owned),
                            }
                        }
                    });
                rsx! {
                    SeenTitle {
                        detail: detail.clone(),
                        on_watch_together: on_watch_together.clone(),
                        tracks,
                        version,
                        on_version: move |v: String| {
                            store_setting(&versions::version_key(id), &v);
                            // Picks name the last file's streams; another starts afresh.
                            pending.set(None);
                            picked.set(Some(v));
                        },
                    }
                }
            },
        )}
    }
}

/// Someone in a cast, and what they appear in. Follows `id` like
/// [`TitleScreen`], since one person's page can stay mounted as another opens.
#[component]
fn PersonScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    let mut page = use_resource(use_reactive!(|(id,)| {
        let api = ctx.api.read().clone();
        let _ = (ctx.marks)();
        async move {
            let api = api?;
            Some((
                id,
                settle_load(ctx.machine, api.person(&call("person", "load"), id).await),
            ))
        }
    }));
    let state = page.read();
    let shown = state
        .as_ref()
        .and_then(Option::as_ref)
        .filter(|(asked, _)| *asked == id)
        .map(|(_, result)| result);
    use_page_name(
        id,
        shown.and_then(|r| r.as_ref().ok()).map(|p| p.name.clone()),
    );
    rsx! {
        {loaded_or_retry(
            shown,
            EventHandler::new(move |()| page.restart()),
            |page: &PersonPage| rsx! { PersonView { page: page.clone() } },
        )}
    }
}

/// The viewer's favourites, one query per kind, sent together (B1083). A
/// mark anywhere reloads it.
#[component]
fn FavouritesScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let mut page = use_resource(move || {
        let api = ctx.api.read().clone();
        let _ = (ctx.marks)();
        async move {
            let api = api?;
            let call = call("favourites", "load");
            let loads = favourites::KINDS.map(|(kind, _)| {
                let (api, call) = (&api, &call);
                async move {
                    api.favorites(call, kind, 0, favourites::PER_KIND)
                        .await
                        .map(|page| (kind, page))
                }
            });
            let loaded = async {
                let mut pages = futures_util::future::join_all(loads)
                    .await
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()?;
                // A show or album marked a favourite fills the first page
                // of episodes or songs with its own, which fold into its
                // tile (B1084, B1085).
                let folds = favourites::Folds::of(&pages);
                for (kind, rail) in pages
                    .iter_mut()
                    .filter(|(kind, _)| favourites::folds(*kind))
                {
                    for _ in 0..favourites::MAX_EPISODE_TOP_UPS {
                        if !folds.wants_more(rail) {
                            break;
                        }
                        let start = rail.items.len() as u32;
                        let next = api
                            .favorites(&call, *kind, start, favourites::PER_KIND)
                            .await?;
                        if next.items.is_empty() {
                            break;
                        }
                        rail.items.extend(next.items);
                    }
                }
                Ok(pages)
            };
            Some(settle_load(ctx.machine, loaded.await).map(Favourites::new))
        }
    });
    let state = page.read();
    rsx! {
        {loaded_or_retry(
            state.as_ref().and_then(Option::as_ref),
            EventHandler::new(move |()| page.restart()),
            |favourites: &Favourites| rsx! { FavouritesView { favourites: favourites.clone() } },
        )}
    }
}

/// The version last picked for `title` on this device, if any.
fn stored_version(title: ItemRef) -> Option<String> {
    platform::stored_theme(&versions::version_key(title))
        .map_err(|e| platform::warn(&format!("pharos: version choice unreadable: {e}")))
        .ok()
        .flatten()
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
    /// The version playing, so a renewed grant stays on that file (empty: the primary).
    version: String,
    /// Its streams against the primary's, which the group's picks name.
    track_map: track_follow::TrackMap,
    /// The tracks the server started this playback on.
    audio: Option<u32>,
    subtitle: Option<u32>,
    /// The audio stream this playback was asked for. The server may play
    /// another, so `audio` alone cannot tell a request was already made.
    asked_audio: Option<u32>,
    /// How the server plays it: "direct_play", "direct_stream" or "transcode".
    method: String,
    /// The subtitle this playback was asked for: `Default` left it to the
    /// server, `Off` asked for none.
    asked_subtitle: TrackChoice,
}

impl Playing {
    /// The tracks this playback was negotiated with, for negotiating it again.
    fn asked(&self) -> TrackPicks {
        TrackPicks {
            audio: self
                .asked_audio
                .map_or(TrackChoice::Default, TrackChoice::Index),
            subtitle: self.asked_subtitle,
        }
    }

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
    let together = use_context::<Together>();
    let group_view = together.view;
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
    let was_paused = use_hook(|| Rc::new(Cell::new(false)));
    // A line over the picture while a renegotiation replaces the player; it
    // lives here because the player is remounted under it (B816).
    let notice = use_signal(|| None::<String>);
    let notice_gen = use_hook(|| Rc::new(Cell::new(0u32)));
    let notify: Rc<dyn Fn(String)> = {
        let notice_gen = Rc::clone(&notice_gen);
        Rc::new(move |line: String| {
            let mut notice = notice;
            let this = notice_gen.get().wrapping_add(1);
            notice_gen.set(this);
            notice.set(Some(line));
            let notice_gen = Rc::clone(&notice_gen);
            spawn(async move {
                platform::sleep_ms(NOTICE_MS).await;
                if notice_gen.get() == this {
                    notice.set(None);
                }
            });
        })
    };
    // The tracks asked for, first those the title page handed on; changing
    // them negotiates a new playback.
    let mut pending = use_context::<PlayPicks>().0;
    let mut picks = use_signal(|| preplay::picks_for(id, *pending.peek()));
    // A version picked in the player; until then the device's stored pick.
    let mut version_pick = use_signal(|| None::<String>);
    // Set by a version switch: `picks` then name the primary's streams, for
    // the next negotiation to map onto the new file.
    let picks_in_primary = use_hook(|| Rc::new(Cell::new(false)));
    // The version whose runtime gap was last told, so a renegotiation on the
    // same file does not tell it again.
    let gap_told = use_hook(|| Rc::new(RefCell::new(None::<String>)));
    use_effect(move || pending.set(None));
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
    // The manual maximum as the Playback page keeps it, mirrored for the
    // player's quality control.
    let mut manual_cap = use_signal(|| stored_bitrate(MAX_BITRATE_KEY, parse_max_bitrate));
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
    // A queued video that starts leaves the video queue (V853), unless a
    // group brought it here; the screen mounts once per play.
    let video_queue = try_use_context::<VideoQueueCtx>();
    use_effect(move || {
        let Some(VideoQueueCtx(mut q, _)) = video_queue else {
            return;
        };
        if group_view.peek().drives(id) {
            return;
        }
        if q.write().take(id) {
            platform::info(&format!(
                "pharos: video queue started item={id} upcoming={}",
                q.peek().upcoming().len()
            ));
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
    let gap_notify = Rc::clone(&notify);
    let carry_flag = Rc::clone(&picks_in_primary);
    let mut playing = use_resource(move || {
        connected();
        let api = ctx.api.peek().clone();
        let grant_gen = *api_gen.peek();
        negotiated_at.set(grant_gen);
        let resume_from = Rc::clone(&resume_from);
        let notify = Rc::clone(&gap_notify);
        let picks_in_primary = Rc::clone(&carry_flag);
        let gap_told = Rc::clone(&gap_told);
        let started_over = Rc::clone(&started_over);
        let asked = picks();
        let picked_here = version_pick();
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
        let together = together.clone();
        async move {
            let api = api?;
            // A play outside the full view is the stage's (spec 020 FR-010).
            let info = call("watch", play_action(ctx_stage.peek().view));
            let result = async {
                // The primary first, to learn which versions the title has.
                let primary = api.detail(&info, id).await?;
                if matches!(primary.item.kind, ItemKind::Episode | ItemKind::Movie) {
                    platform::turn_open(&id.to_string());
                } else {
                    platform::turn_cancel(&id.to_string(), "not-video");
                }
                let stored = picked_here.or_else(|| stored_version(id));
                let version =
                    versions::chosen_version(&primary.versions, stored.as_deref()).to_owned();
                if primary.versions.len() > 1 {
                    platform::info(&format!(
                        "pharos: version chosen={version:?} stored={stored:?} of {}",
                        primary.versions.len()
                    ));
                }
                // The chosen file's tracks and fonts are its own, so its detail
                // is fetched once more; the primary's stands when it plays.
                let primary_tracks = (
                    primary.audio_tracks.clone(),
                    primary.subtitle_tracks.clone(),
                );
                let detail = if version.is_empty() {
                    primary
                } else {
                    api.detail_version(&info, id, &version).await?
                };
                let track_map = track_follow::TrackMap::new(
                    (&detail.audio_tracks, &detail.subtitle_tracks),
                    (&primary_tracks.0, &primary_tracks.1),
                );
                let asked = if picks_in_primary.replace(false) {
                    let own = track_map.choices_to_own(asked);
                    platform::info(&format!(
                        "pharos: version tracks carried item={id} version={version:?} primary={asked:?} own={own:?}"
                    ));
                    own
                } else {
                    asked
                };
                // Every start of an episode comes through here: from its
                // title page, the next-episode button, up next and the
                // episode turn alike.
                let asked = show_tracks::show_of(&detail).map_or(asked, |series| {
                    // The group names the primary's streams (ADR-0089).
                    let followed = together.current().and_then(|s| {
                        let audio = track_map.primary_indexes(activity::TrackKind::Audio);
                        let subtitles = track_map.primary_indexes(activity::TrackKind::Subtitles);
                        s.tracks_followed(&audio, &subtitles)
                            .map(|picks| track_map.picks_to_own(picks))
                    });
                    show_tracks::start_picks(
                        followed.as_ref(),
                        &show_tracks::load(series),
                        &detail,
                        asked,
                    )
                });
                let caps = negotiation_capabilities(detail.item.kind, auto);
                let (decision, grant) = api.play(&info, id, &version, &caps, asked).await?;
                Ok::<_, ClientError>((detail, version, decision, grant, asked, track_map))
            }
            .await;
            Some(
                settle_start(ctx.machine, result).and_then(|(detail, version, decision, grant, asked, track_map)| {
                    let (mse_engine, engine_reason) = mse_engine_choice();
                    let picked = choose_engine(
                        decision.is_hls(),
                        platform::plays_hls(),
                        platform::has_mse(),
                        mse_engine,
                    );
                    let verdict = engine_verdict(picked, engine_reason);
                    let engine = verdict.engine;
                    if picked.reports_fragments() {
                        platform::info(&verdict.line());
                        platform::report_event(verdict.report());
                    }
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
                        return Err(StartFailure::new(
                            FailureKind::Unplayable,
                            "This title needs HLS, which this browser cannot play.",
                        ));
                    }
                    // In a group playing this item, the player starts where
                    // the group is: this member's own resume point would land
                    // after the group's catch-up seek and carry it away (B664).
                    let group_start = together.current().and_then(|s| s.start_for(id));
                    if let Some(gap_ms) = versions::runtime_gap_ms(&detail.versions, &version) {
                        let ms = |id: &str| {
                            detail.versions.iter().find(|v| v.id == id).map_or(0, |v| v.duration_ms)
                        };
                        let primary = detail.versions.iter().find(|v| v.primary).map_or("", |v| v.id.as_str());
                        platform::info(&format!(
                            "pharos: version runtime_gap item={id} version={version:?} primary_ms={} version_ms={} gap_ms={gap_ms} grouped={}",
                            ms(primary),
                            ms(&version),
                            group_start.is_some()
                        ));
                        let told = gap_told.borrow().as_deref() == Some(version.as_str());
                        if group_start.is_some() && !told {
                            gap_told.replace(Some(version.clone()));
                            notify(versions::runtime_gap_notice(gap_ms));
                        }
                    }
                    let start_ms = match (resume_from.get(), group_start) {
                        (0, Some(at)) => {
                            platform::info(&format!(
                                "pharos: playback group_start item={id} position_ms={at} saved_ms={}",
                                detail.item.position_ms
                            ));
                            at
                        }
                        (0, None) if from_start => {
                            platform::info(&format!(
                                "pharos: playback from_start item={id} reason={reason} saved_ms={}",
                                detail.item.position_ms
                            ));
                            0
                        }
                        (0, None) => start_ms(detail.item.position_ms, detail.item.played),
                        (here, _) => here,
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
                        version,
                        track_map,
                        audio: decision.audio,
                        subtitle: decision.subtitle,
                        asked_audio: match asked.audio {
                            TrackChoice::Index(i) => Some(i),
                            _ => None,
                        },
                        method: decision.method,
                        asked_subtitle: asked.subtitle,
                    })))
                }),
            )
        }
    });
    // What plays after this video: the video queue's first, unless a group
    // decides what plays (V853).
    let video_next = use_memo(move || {
        let video = matches!(
            playing.read().as_ref(),
            Some(Some(Ok(p))) if video_kind(p.0.kind)
        );
        let grouped = group_view.read().drives(id);
        video_queue
            .filter(|_| video && !grouped)
            .and_then(|VideoQueueCtx(q, _)| q.read().next_besides(id))
    });
    // The decision, whenever its inputs change (V853).
    use_effect(move || {
        let video = matches!(
            playing.read().as_ref(),
            Some(Some(Ok(p))) if video_kind(p.0.kind)
        );
        let episode = near
            .read()
            .as_ref()
            .and_then(|n| n.as_ref())
            .and_then(|(n, _)| n.next);
        if video {
            platform::info(&video_next_line(
                id,
                video_next(),
                episode,
                group_view.read().drives(id),
            ));
        }
    });
    // The next video's name and still, for the "Up next" card: the queued
    // video, else the next episode. A song's next track would cost a detail
    // call for nothing.
    let upcoming = use_resource(move || {
        let api = ctx.api.read().clone();
        let episode = matches!(
            playing.read().as_ref(),
            Some(Some(Ok(p))) if p.0.kind == ItemKind::Episode
        );
        let next = video_next().or_else(|| {
            near.read()
                .as_ref()
                .and_then(|n| n.as_ref())
                .and_then(|(n, _)| n.next)
                .filter(|_| episode)
        });
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
        let asked = p.0.asked();
        let kind = p.0.kind;
        spawn(async move {
            let info = call("watch", "renew");
            let mut wait_ms = RENEW_FIRST_WAIT_MS;
            for attempt in 1..=RENEW_ATTEMPTS {
                // A newer reconnect has taken over.
                if *api_gen.peek() != gen {
                    return;
                }
                match api.play(&info, id, &p.0.version, &negotiation_capabilities(kind, *auto_cap.peek()), asked).await {
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
    // The turn onto this item (V790) follows its load, and stops for an
    // error, a small player or a page left.
    use_effect(move || match playing.read().as_ref() {
        Some(Some(Ok(p))) => {
            platform::turn_ready(&id.to_string(), p.0.image.as_deref(), &p.0.title);
        }
        Some(Some(Err(_))) => platform::turn_cancel(&id.to_string(), "playback-error"),
        _ => {}
    });
    use_effect(move || {
        if ctx_stage.read().view != View::Full {
            platform::turn_cancel(&id.to_string(), "not-full");
        }
    });
    use_drop(move || platform::turn_cancel(&id.to_string(), "left"));
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
    // A negotiation that failed in the full view has no player and no shell
    // to leave by, so it gets the failure panel (B806).
    let start_failure = match state.as_ref().and_then(Option::as_ref) {
        Some(Err(reason)) if ctx_stage.read().view == View::Full => Some(reason.clone()),
        _ => None,
    };
    let nav = use_context::<Nav>();
    use_effect(move || {
        if let Some(Some(Err(reason))) = &*playing.read() {
            platform::warn(&format!(
                "pharos: playback failure shown item={id} kind=negotiate view={} failure={} trace={} reason={reason}",
                ctx_stage.peek().view.label(),
                reason.kind.label(),
                reason.trace_id.as_deref().unwrap_or("none"),
            ));
        }
    });
    // The full view's first load has no player chrome yet, so it carries its
    // own way out (038 FR-408, V1220).
    let first_load =
        ctx_stage.read().view == View::Full && state.as_ref().and_then(Option::as_ref).is_none();
    let body = match start_failure {
        None if first_load => rsx! {
            FirstLoad { on_back: move |()| leave_player(nav) }
        },
        Some(reason) => rsx! {
            PlaybackFailure {
                failure: PlayFailure::start(reason.clone()),
                on_retry: move |()| playing.restart(),
                on_back: move |()| leave_player(nav),
                on_report: move |()| report_failure(&PlayFailure::start(reason.clone())),
            }
        },
        None => loaded(state.as_ref().and_then(Option::as_ref), |p: &PlayingRef| {
            remount_on(
                &p.0.session,
                rsx! {
                    Player {
                        playing: p.clone(),
                        last_ms: LastMs(Rc::clone(&last_ms)),
                        near: {
                            let mut n = near.read().as_ref().and_then(|n| n.as_ref()).map(|n| n.0).unwrap_or_default();
                            n.next = video_next().or(n.next);
                            n
                        },
                        upcoming: upcoming.read().clone().flatten(),
                        on_renegotiate: move |next| picks.set(next),
                        on_version: {
                            let (last_ms, notify) = (Rc::clone(&last_ms), Rc::clone(&notify));
                            let picks_in_primary = Rc::clone(&picks_in_primary);
                            move |to: String| {
                                let (from, carried) = match playing.peek().as_ref() {
                                    Some(Some(Ok(p))) => {
                                        (p.0.version.clone(), p.0.track_map.choices_to_primary(p.0.asked()))
                                    }
                                    _ => {
                                        platform::warn(&format!(
                                            "pharos: version switch dropped item={id} to={to:?} reason=no playback"
                                        ));
                                        return;
                                    }
                                };
                                if to == from {
                                    return;
                                }
                                platform::info(&format!(
                                    "pharos: version switch item={id} from={from:?} to={to:?} at_ms={} grouped={}",
                                    last_ms.get(),
                                    group_view.peek().drives(id)
                                ));
                                store_setting(&versions::version_key(id), &to);
                                notify("Switching version…".to_owned());
                                // The last file's indexes name other streams in this one.
                                picks_in_primary.set(true);
                                picks.set(carried);
                                version_pick.set(Some(to));
                            }
                        },
                        on_notice: {
                            let notify = Rc::clone(&notify);
                            move |line: String| notify(line)
                        },
                        notice: notice(),
                        manual_cap: manual_cap(),
                        auto_cap: auto_cap(),
                        on_quality_pick: {
                            let (watch, notify) = (watch.clone(), Rc::clone(&notify));
                            move |pick: Option<u64>| {
                                let now = now_ms_u64();
                                match pick {
                                    Some(bps) => {
                                        if bps == *manual_cap.peek() {
                                            return;
                                        }
                                        store_bitrate(MAX_BITRATE_KEY, bps);
                                        let kept = {
                                            let mut w = watch.0.borrow_mut();
                                            w.set_ceiling(bps, now);
                                            w.cap()
                                        };
                                        store_bitrate(quality::AUTO_RUNG_KEY, kept);
                                        platform::info(&format!(
                                            "pharos: max_bitrate set={} from=player auto={}",
                                            max_bitrate_label(bps),
                                            max_bitrate_label(kept)
                                        ));
                                        notify(quality_choice_notice(bps));
                                        manual_cap.set(bps);
                                        auto_cap.set(kept);
                                    }
                                    None => {
                                        watch.0.borrow_mut().clear_cap(now);
                                        store_bitrate(quality::AUTO_RUNG_KEY, 0);
                                        platform::info("pharos: auto quality reset from=player");
                                        notify("Resetting Auto quality…".to_owned());
                                        auto_cap.set(0);
                                    }
                                }
                            }
                        },
                        was_paused: WasPaused(Rc::clone(&was_paused)),
                        // In place of this episode, as the next-episode button does.
                        on_advance: move |next| replace.0.call(Route::Watch(next)),
                        watch: watch.clone(),
                        on_quality: {
                            let notify = Rc::clone(&notify);
                            move |cap| {
                                let before = *auto_cap.peek();
                                if before != cap {
                                    notify(quality_notice(before, cap));
                                }
                                store_bitrate(quality::AUTO_RUNG_KEY, cap);
                                auto_cap.set(cap);
                            }
                        },
                        slot,
                        fade_in: fade_in.clone(),
                    }
                },
            )
        }),
    };
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

/// Whether the viewer had paused the player, shared with its screen so a
/// renegotiation that remounts the player does not start a paused video
/// (B816).
#[derive(Clone)]
struct WasPaused(Rc<Cell<bool>>);

impl PartialEq for WasPaused {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// Whether a freshly mounted video starts by itself: not in a group, whose
/// play it waits for, and not when the viewer had paused the one it replaces.
fn autoplay_after(grouped: bool, was_paused: bool) -> bool {
    !grouped && !was_paused
}

/// How long a "Switching audio…" style notice stays up.
const NOTICE_MS: u32 = 4000;

/// The line shown while a quality pick renegotiates the playback.
fn quality_choice_notice(manual: u64) -> String {
    if manual == 0 {
        "Quality: Auto".to_owned()
    } else {
        format!("Quality: up to {} Mbps", manual / 1_000_000)
    }
}

/// The line shown while a track change renegotiates the playback.
fn track_notice(audio: bool) -> String {
    if audio {
        "Switching audio…"
    } else {
        "Switching subtitles…"
    }
    .to_owned()
}

/// The line shown while the quality step renegotiates from `from` to `to` bps.
fn quality_notice(from: u64, to: u64) -> String {
    let verb = if to < from { "Lowering" } else { "Raising" };
    format!("{verb} quality to {} Mbps", to / 1_000_000)
}

/// The `ui_action` of a play RPC made in `view`.
fn play_action(view: View) -> &'static str {
    match view {
        View::Full => "play",
        View::Floating => "stage_floating",
        View::Mini => "stage_mini",
    }
}

/// A Leave on its way to the server, awaitable by more than one caller.
type LeaveInFlight =
    futures_util::future::Shared<futures_util::future::LocalBoxFuture<'static, ()>>;

/// Whether the connection to the server is down and being reopened.
#[derive(Clone, Copy)]
pub struct ConnectionLost(pub Memo<bool>);

const CONNECTION_LOST: &str = "Connection lost. Reconnecting…";

/// How far past a note's deadline its redraw is timed, so the redraw finds it
/// expired even when the browser fires the timer a little early.
const TIMER_SLACK_MS: u64 = 50;

/// Whether `text` still shows, `for_ms` after it first did. Redraws once
/// when its time is up.
fn use_timed_note(text: Option<&str>, for_ms: u64) -> bool {
    let timer = use_hook(|| Rc::new(RefCell::new(activity::NoteTimer::default())));
    let expired = use_signal(|| 0u32);
    let _ = expired();
    let shown = timer.borrow_mut().show(text, now_ms_u64(), for_ms);
    if shown.arm {
        let mut expired = expired;
        spawn(async move {
            // Past the deadline: a timer can fire before the wall clock agrees.
            platform::sleep_ms(
                u32::try_from(for_ms.saturating_add(TIMER_SLACK_MS)).unwrap_or(u32::MAX),
            )
            .await;
            expired += 1;
        });
    }
    shown.visible
}

/// The pill naming who the group waits on, then "ready" for a moment.
fn use_wait_pill(waiting: &[String], in_group: bool) -> Option<activity::WaitPill> {
    let tracker = use_hook(|| Rc::new(RefCell::new(activity::WaitTracker::default())));
    let expired = use_signal(|| 0u32);
    let _ = expired();
    if !in_group {
        *tracker.borrow_mut() = activity::WaitTracker::default();
        return None;
    }
    let (pill, arm) = tracker.borrow_mut().pill(waiting, now_ms_u64());
    if arm {
        let mut expired = expired;
        spawn(async move {
            platform::sleep_ms(
                u32::try_from(activity::READY_MS.saturating_add(TIMER_SLACK_MS))
                    .unwrap_or(u32::MAX),
            )
            .await;
            expired += 1;
        });
    }
    pill
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

/// The tracks a progress report names. A track the viewer asked for is
/// named as it plays, so the next play starts on it; a track left to the
/// server stays `Default`, so the server's language rules keep deciding. An
/// in-place subtitle switch is what plays now, so it wins over the ask.
fn reported_tracks(
    asked_audio: Option<u32>,
    audio: Option<u32>,
    asked_subtitle: TrackChoice,
    subtitle: Option<u32>,
    switched: Option<TrackChoice>,
) -> TrackPicks {
    let audio = match (asked_audio, audio) {
        (Some(_), Some(playing)) => TrackChoice::Index(playing),
        _ => TrackChoice::Default,
    };
    let asked = match (asked_subtitle, subtitle) {
        (TrackChoice::Off, _) => TrackChoice::Off,
        (TrackChoice::Index(_), Some(playing)) => TrackChoice::Index(playing),
        _ => TrackChoice::Default,
    };
    TrackPicks {
        audio,
        subtitle: switched.unwrap_or(asked),
    }
}

/// The link that joins group `id`: the invite route on this server.
fn invite_url(origin: &str, id: &str) -> String {
    format!("{origin}{}", Route::TogetherGroup(id.to_owned()).path())
}

/// How long after the group changes entry this player waits for events from
/// the old end before its Up next starts over without a seek (B1027).
const UP_NEXT_RESET_SETTLE_MS: f64 = 2_000.0;

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
    /// The viewer picked another version in the player (spec 034 US3).
    on_version: EventHandler<String>,
    /// A line to show while the renegotiation replaces this player.
    on_notice: EventHandler<String>,
    /// The line the screen is showing over the picture, if any.
    notice: Option<String>,
    /// The viewer's manual maximum and the cap Auto settled on, in bps (0 is
    /// none), for the quality control.
    manual_cap: u64,
    auto_cap: u64,
    /// The viewer picked a maximum (`Some`, 0 is Auto) or reset Auto (`None`)
    /// in the player.
    on_quality_pick: EventHandler<Option<u64>>,
    was_paused: WasPaused,
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
    let resume: ResumeSlot = use_hook(ResumeSlot::default);
    let fade_in_ms = use_hook(|| fade_in.0.take());
    let clock = use_hook(|| Rc::new(RefCell::new(ReportClock::new())));
    let engine = use_hook(|| Rc::new(RefCell::new(None::<Box<dyn PlaybackEngine>>)));
    let nav = use_context::<Nav>();
    let replace = use_context::<Replace>();
    let together = use_context::<Together>();
    let group_view = together.view;
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
    // A group that ended or could not be rejoined is news for a moment, then
    // out of the picture's way (FR-501).
    let group_status = group.ended.clone().or_else(|| together.note.read().clone());
    let status_shown = use_timed_note(group_status.as_deref(), activity::NOTE_MS);
    let wait_pill = use_wait_pill(&group.waiting, group.active);
    let connection_lost =
        try_use_context::<ConnectionLost>().is_some_and(|ConnectionLost(lost)| lost());
    // The group just left, while Leave can be undone (FR-505).
    let left_group = use_signal(|| None::<String>);
    // The leave in flight, which an Undo waits out so the old leave cannot
    // land after the new join and end it.
    let leaving = use_hook(|| Rc::new(RefCell::new(None::<LeaveInFlight>)));
    // A new membership ends the chance to undo the last leave.
    use_effect(move || {
        let mut left_group = left_group;
        if (together.view)().active && left_group.peek().is_some() {
            left_group.set(None);
        }
    });
    let undo_shown = use_timed_note(
        left_group.read().as_deref().filter(|_| !group.active),
        activity::UNDO_LEAVE_MS,
    );
    // Taken at mount: the video this one replaces was paused, or not.
    let start_paused = use_hook(|| was_paused.0.get());
    let mut sub_size = use_signal(stored_subtitle_size);
    let mut view_mode = use_signal(stored_view_mode);
    let vr = use_resource(platform::vr_supported);
    let mut failed = use_signal(|| None::<PlayFailure>);
    // A seek still waiting on its first frame, and a clock that ticks once a
    // second while one does, for the card that says so (B1196).
    let mut seek_wait = use_signal(|| None::<SeekWait>);
    let mut seek_tick = use_signal(|| 0u64);
    use_future(move || async move {
        loop {
            platform::sleep_ms(1_000).await;
            if seek_wait.peek().is_some() {
                seek_tick.set(now_ms_u64());
            }
        }
    });
    // Where the media broke, for the skip past it to remember (B1160).
    let broke_at = use_hook(|| Rc::new(Cell::new(0u64)));
    let saved_volume = use_hook(last_volume);
    let mut media = use_signal(|| MediaState::at_volume(saved_volume.map(|(v, m, _)| (v, m))));
    // The position changes every tick and the rest of the state seldom. The
    // player reads `media_rest` and hands `position` to the leaves that show
    // it, so a tick does not rebuild the player (B815).
    let mut position = use_signal(|| 0u64);
    let mut buffered = use_signal(|| 0u64);
    // The viewer's playback speed (T038-305), 1.0 being normal. It lasts
    // the session, so the next episode plays at the speed the viewer chose.
    let mut speed = use_signal(|| LAST_SPEED.with(Cell::get));
    // The viewer's subtitle delay in ms (T038-305). A new player starts
    // at none, as a delay fits one file.
    let mut subtitle_delay = use_signal(|| {
        platform::set_subtitle_delay(VIDEO_ID, 0);
        0i64
    });
    let media_rest = use_memo(move || split_position(media()).0);
    // What a wait under the spinner is for (038 FR-401): whether this video
    // has played yet, and, once a wait passes `wait::HINT_AFTER_MS`, the
    // server's word on whether it is queued for an encoder (FR-402).
    let started = use_signal(|| false);
    let mut queue_hint = use_signal(|| None::<pharos_client::api::QueueHint>);
    // Read when present: the stage can mount before the signed-in shell has
    // provided it.
    let account = try_use_context::<AccountMe>();
    let wait_logged = use_hook(|| Rc::new(RefCell::new(String::new())));
    // The first `canplay` has checked that autoplay started (038 FR-407),
    // and whether the browser refused it and waits for a tap.
    let autoplay_checked = use_hook(|| Rc::new(Cell::new(false)));
    let mut autoplay_refused = use_signal(|| false);
    // How "Up next" was last shown, for its decision line.
    let up_next_mode = use_hook(|| Rc::new(Cell::new(None::<&'static str>)));
    let crop_rows_kept = use_hook(|| Rc::new(Cell::new(None::<Option<&'static str>>)));
    // Its own loop: a queue hint the server is slow to answer must not keep
    // Tap to start hidden (038 FR-407).
    use_future(move || async move {
        loop {
            platform::sleep_ms(1_000).await;
            let refused = platform::autoplay_blocked() && media.peek().paused;
            if *autoplay_refused.peek() != refused {
                autoplay_refused.set(refused);
            }
        }
    });
    use_future({
        let p = Rc::clone(&p);
        move || {
            let p = Rc::clone(&p);
            async move {
                // When this wait began, when the server was last asked, and
                // whether it cannot answer (an older server) for this wait.
                let (mut since, mut asked_at, mut gave_up) = (None::<u64>, 0u64, false);
                loop {
                    platform::sleep_ms(1_000).await;
                    let now = now_ms_u64();
                    if !(media.peek().loading && failed.peek().is_none()) {
                        (since, gave_up) = (None, false);
                        if queue_hint.peek().is_some() {
                            queue_hint.set(None);
                        }
                        continue;
                    }
                    let began = *since.get_or_insert(now);
                    if gave_up
                        || now.saturating_sub(began) < super::wait::HINT_AFTER_MS
                        || now.saturating_sub(asked_at) < super::wait::HINT_EVERY_MS
                    {
                        continue;
                    }
                    asked_at = now;
                    let grant = p.grant.borrow().clone();
                    match grant.queue_hint(&call("watch", "queue hint")).await {
                        // Set only on a change: each set rebuilds the player.
                        Ok(hint) if media.peek().loading => {
                            if *queue_hint.peek() != Some(hint) {
                                queue_hint.set(Some(hint));
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            gave_up = true;
                            platform::warn(&format!(
                                "pharos: queue hint failed item={} waited_ms={} reason={e}",
                                p.detail.item.id,
                                now.saturating_sub(began)
                            ));
                        }
                    }
                }
            }
        }
    });
    let mut hover_ms = use_signal(|| None::<u64>);
    // The subtitle on screen: the server's pick, then the viewer's.
    let mut subtitle = use_signal(|| p.subtitle);
    // The subtitle the viewer switched to in place, which reports carry so
    // the next playback starts on it.
    let switched = use_hook(|| Rc::new(Cell::new(None::<TrackChoice>)));
    // The Appearance page is another route, so the level cannot change while
    // this player is mounted: it is read once, at mount.
    let effects = use_signal(stored_effects_level);
    let speakers = use_signal(stored_speaker_colours);
    // The generated styled track needs Effects on: with it Off, the viewer
    // moves to the plain track it is made from, else to no subtitle.
    use_effect({
        let (p, switched) = (Rc::clone(&p), Rc::clone(&switched));
        move || {
            if effects() != EffectsLevel::Off {
                return;
            }
            let tracks = &p.detail.subtitle_tracks;
            // Read, not peeked: a pick made later (a group-watch switch skips
            // the picker's filter) is moved off the styled track too.
            let Some(shown) = subtitle() else {
                return;
            };
            if !tracks
                .iter()
                .any(|t| t.index == shown && t.styled_url.is_some())
            {
                return;
            }
            let to = source_of_styled(tracks, shown);
            switched.set(Some(choice(to)));
            subtitle.set(to);
        }
    });
    let mut up_next = use_signal(UpNext::default);
    let mut up_next_step = use_signal(|| UpNextStep::Hidden);
    // The group's queue entry the countdown belongs to: a group moving to
    // another entry of the same item keeps this player, not its Cancel.
    let up_next_entry = use_hook(|| Rc::new(RefCell::new(String::new())));
    // A new entry's countdown starts over only once the player has left the
    // old end, so events from there cannot move the group on (B1027). Holds
    // when the group changed entry.
    let up_next_reset = use_hook(|| Rc::new(Cell::new(None::<f64>)));
    let advanced = use_hook(|| Rc::new(Cell::new(false)));
    // An ASS subtitle drawn from its script (ADR-0057), and the pick it is
    // for; its WebVTT stays on screen until the renderer is ready.
    let styled = use_hook(|| Rc::new(RefCell::new(None::<platform::StyledSubtitles>)));
    let styled_for = use_hook(|| Rc::new(Cell::new(0_u64)));
    // The script URL the renderer was last started for: a re-run that
    // resolves to the same script (the effect runs again once playback
    // starts) keeps the renderer instead of fetching the script a second
    // time. Cleared when the renderer fails, so the next run retries.
    let styled_url_for = use_hook(|| Rc::new(RefCell::new(None::<String>)));
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
    let video_queue = try_use_context::<VideoQueueCtx>();
    let audio = p.kind == ItemKind::Audio;
    // Whether Back from the full view stops this play on the TV.
    let current = matches!(slot, Slot::Current { .. });
    use_effect(move || {
        let mut stage = stage;
        if current && stage.peek().video == audio {
            stage.write().video = !audio;
        }
    });
    let playing_id = p.detail.item.id;
    // A player that gives up must show why, not sit under the turn (V790).
    use_effect(move || {
        if failed.read().is_some() {
            platform::turn_cancel(&playing_id.to_string(), "player-failed");
        }
    });
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
    // When the server expects them by, set on each answer that says so.
    let mut words_eta_at = use_signal(|| None::<(ItemRef, f64)>);
    // The one word-times poll; a refreshed lookup (a reconnect) replaces it,
    // so a late answer to the old one never overwrites the newer deadline.
    let mut align_poll = use_signal(|| None::<dioxus::core::Task>);
    use_effect({
        let id = p.detail.item.id;
        move || {
            let pending = match &*lyrics.read() {
                Some(Ok(Some(l))) if l.words_pending => Some(l.words_eta_ms),
                _ => None,
            };
            if let Some(old) = align_poll.write().take() {
                old.cancel();
            }
            let Some(first_eta) = pending else {
                return;
            };
            words_eta_at
                .set(super::lyrics::eta_deadline(first_eta, platform::now_ms()).map(|at| (id, at)));
            let Some(api) = ctx.api.peek().clone() else {
                return;
            };
            align_poll.set(Some(spawn(async move {
                loop {
                    platform::sleep_ms(ALIGN_ASK_MS).await;
                    match api.lyrics(&call("watch", "lyrics"), id, true).await {
                        Ok(next) if next.words_pending => {
                            words_eta_at.set(
                                super::lyrics::eta_deadline(next.words_eta_ms, platform::now_ms())
                                    .map(|at| (id, at)),
                            );
                        }
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
            })));
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
    // A tab that goes hidden reports where it is, since closing it often
    // fires no pagehide; a page restored from the back/forward cache reports
    // again (B809).
    use_hook({
        let (p, clock, last_ms, switched) = (
            Rc::clone(&p),
            Rc::clone(&clock),
            Rc::clone(&last_ms.0),
            Rc::clone(&switched),
        );
        let retiring = Rc::clone(&retiring);
        let restored_clock = Rc::clone(&clock);
        move || {
            Rc::new(platform::on_page_lifecycle(
                move || {
                    if retiring.get() {
                        return;
                    }
                    let here = platform::position_ms(VIDEO_ID);
                    if here > 0 {
                        last_ms.set(here);
                    }
                    let Some(Outgoing::Report { paused }) =
                        clock.borrow_mut().on_hidden(platform::now_ms())
                    else {
                        return;
                    };
                    let grant = p.grant.borrow().clone();
                    let at = last_ms.get();
                    let tracks = TrackPicks {
                        audio: TrackChoice::Default,
                        subtitle: switched.get().unwrap_or_default(),
                    };
                    platform::info(&format!(
                        "pharos: progress report reason=hidden item={playing_id} at_ms={at} paused={paused}"
                    ));
                    platform::spawn_detached(async move {
                        if let Err(e) = grant
                            .report(&call("watch", "report"), at, paused, tracks)
                            .await
                        {
                            platform::warn(&format!(
                                "pharos: progress report failed reason=hidden: {e}"
                            ));
                        }
                    });
                },
                move || {
                    let was_stopped = restored_clock.borrow_mut().restore();
                    platform::info(&format!(
                        "pharos: progress reports restored reason=pageshow item={playing_id} was_stopped={was_stopped}"
                    ));
                },
            ))
        }
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
                        on_notice.call(track_notice(audio.is_some()));
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
        let (map, version) = (p.track_map.clone(), p.version.clone());
        move || {
            let video: Rc<dyn GroupPlayer> = Rc::new(VideoPlayer {
                id: VIDEO_ID,
                audio,
                subtitles,
                map,
                version,
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
    // Auto quality judges video played by an MSE engine, the only kind that
    // sees the downloads; a change reloads at the position, as a track change does.
    let auto = p.engine.reports_fragments() && p.kind != ItemKind::Audio;
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
        let (p, last_ms, broke_at) = (Rc::clone(&p), Rc::clone(&last_ms.0), Rc::clone(&broke_at));
        move |mut reasons: UnboundedReceiver<Fatal>| {
            let stop = stop.clone();
            let (p, last_ms, broke_at) = (Rc::clone(&p), Rc::clone(&last_ms), Rc::clone(&broke_at));
            async move {
                while let Some(fatal) = reasons.next().await {
                    stop();
                    let kind = fatal.kind;
                    let reason = fatal.reason;
                    // Music keeps its own screen, and a group plays one
                    // timeline for all: neither is offered a skip.
                    let skippable = kind == FailureKind::Damaged
                        && p.kind != ItemKind::Audio
                        && !group_view.peek().drives(playing_id);
                    // Where the element stopped; a failure before the first
                    // position tick has not reached `last_ms` yet, and one
                    // before the start seek is at the start.
                    let at = [platform::position_ms(VIDEO_ID), last_ms.get(), p.start_ms]
                        .into_iter()
                        .find(|&ms| ms > 0)
                        .unwrap_or(0);
                    broke_at.set(at);
                    let skip = skippable
                        .then(|| {
                            damage::skip_target(
                                at,
                                p.detail.item.runtime_ms,
                                &scanned_spans(&p),
                                &stored_skips(playing_id, &p.version),
                            )
                        })
                        .flatten();
                    platform::warn(&format!(
                        "pharos: playback failure shown item={playing_id} kind=stream media={} failure={} at_ms={at} skip_to_ms={} source={} reason={reason}",
                        kind == FailureKind::Damaged,
                        kind.label(),
                        skip.map_or(0, |(to, _)| to),
                        skip.map_or("none", |(_, s)| s.label()),
                    ));
                    // Music and groups are offered no skip, but a damaged
                    // file is still named as one (038 FR-403).
                    failed.set(Some(match kind {
                        FailureKind::Damaged => {
                            PlayFailure::damaged(reason, skip.map(|(to, _)| to))
                        }
                        kind => PlayFailure::stream(kind, reason),
                    }));
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
                let adapts = p.engine.adapts_quality();
                let on_fragment: OnFragment = {
                    let (watch, assess) = (watch.clone(), Rc::clone(&assess));
                    Rc::new(move |load| {
                        // Handing over, or already decided: no more downloads
                        // count. An engine that adapts itself needs no help.
                        if adapts || retiring.get() || decided.get() {
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
                    Err(e) => {
                        platform::warn(&format!(
                            "pharos: playback failure shown item={playing_id} kind=start reason={e}"
                        ));
                        // The engine would not attach to this browser.
                        failed.set(Some(PlayFailure::start(StartFailure::new(
                            FailureKind::Unplayable,
                            e,
                        ))));
                    }
                }
            });
        }
    });
    // A burned subtitle is in the picture, not a text track.
    let burned = p.burned();
    // The renderer calls back from worker events, outside the Dioxus runtime.
    let styled_events = use_coroutine({
        let (styled, styled_for, styled_url_for) = (
            Rc::clone(&styled),
            Rc::clone(&styled_for),
            Rc::clone(&styled_url_for),
        );
        move |mut events: UnboundedReceiver<(u64, Result<(), String>)>| {
            let (styled, styled_for, styled_url_for) = (
                Rc::clone(&styled),
                Rc::clone(&styled_for),
                Rc::clone(&styled_url_for),
            );
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
                            styled_url_for.replace(None);
                            styled_ready.set(false);
                        }
                    }
                }
            }
        }
    });
    // The tier the generated styled track is fetched at: the device's, then
    // lower when the video drops frames (T028-21).
    let mut tier = use_signal(current_starting_tier);
    let tier_watch = use_hook(|| Rc::new(RefCell::new(TierDrop::new(*tier.peek()))));
    // Reduced motion lowers the starting tier; a change reloads the script
    // at the new one.
    use_hook({
        let tier_watch = Rc::clone(&tier_watch);
        move || {
            Rc::new(platform::on_media_change(REDUCED_MOTION, move || {
                let now = current_starting_tier();
                if *tier.peek() != now {
                    *tier_watch.borrow_mut() = TierDrop::new(now);
                    tier.set(now);
                }
            }))
        }
    });
    use_effect({
        let (p, styled, styled_for, styled_url_for) = (
            Rc::clone(&p),
            Rc::clone(&styled),
            Rc::clone(&styled_for),
            Rc::clone(&styled_url_for),
        );
        move || {
            let shown = if burned { None } else { subtitle() };
            // Read before any return, so a tier drop, a level change or a
            // size change reloads the script in place.
            let (tier_now, level_now, size_now) = (tier(), effects(), sub_size());
            let speakers_now = speakers();
            let track = shown.and_then(|i| p.detail.subtitle_tracks.iter().find(|t| t.index == i));
            let generated = track
                .and_then(|t| styled_track_url(t, tier_now, level_now, size_now, speakers_now));
            // A generated track at Off draws as the plain track beside it,
            // never from its source's raw script.
            let script = generated.clone().or_else(|| {
                track
                    .filter(|t| t.styled_url.is_none())
                    .and_then(styled_subtitle_url)
            });
            if !restart_styled(styled_url_for.borrow().as_deref(), script.as_deref()) {
                return;
            }
            styled_url_for.replace(script.clone());
            let pick = styled_for.get().wrapping_add(1);
            styled_for.set(pick);
            styled.borrow_mut().take();
            styled_ready.set(false);
            let Some(url) = script else {
                return;
            };
            if generated.is_some() {
                platform::info(&format!(
                    "pharos: subtitle styled url={url} tier={} effects={}",
                    tier_now.label(),
                    level_now.as_str()
                ));
            }
            let fonts = shown_font_urls(
                &p.detail.fonts,
                &p.detail.subtitle_tracks,
                shown.unwrap_or_default(),
            );
            let (styled, styled_for) = (Rc::clone(&styled), Rc::clone(&styled_for));
            spawn(async move {
                // Built before the video knows its size, the renderer never
                // draws; and a pick replaced while waiting fetches nothing.
                platform::video_metadata(VIDEO_ID).await;
                if styled_for.get() != pick {
                    return;
                }
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
    let sampling = use_hook(|| Rc::new(Cell::new(0_u64)));
    use_drop({
        let sampling = Rc::clone(&sampling);
        move || sampling.set(sampling.get().wrapping_add(1))
    });
    use_effect({
        let (p, tier_watch, sampling, styled) = (
            Rc::clone(&p),
            Rc::clone(&tier_watch),
            Rc::clone(&sampling),
            Rc::clone(&styled),
        );
        move || {
            let run = sampling.get().wrapping_add(1);
            sampling.set(run);
            let generated = !burned
                && subtitle()
                    .and_then(|i| p.detail.subtitle_tracks.iter().find(|t| t.index == i))
                    .is_some_and(|t| t.styled_url.is_some());
            if !(generated && styled_ready()) {
                return;
            }
            let (tier_watch, sampling, styled) = (
                Rc::clone(&tier_watch),
                Rc::clone(&sampling),
                Rc::clone(&styled),
            );
            spawn(async move {
                let mut uncounted_samples = 0_u32;
                while sampling.get() == run {
                    platform::sleep_ms(QUALITY_SAMPLE_MS).await;
                    if sampling.get() != run {
                        break;
                    }
                    let Some((dropped, presented)) = platform::video_quality(VIDEO_ID) else {
                        break;
                    };
                    let counting = platform::page_visible() && {
                        let m = media.peek();
                        !m.paused && !m.loading
                    };
                    let (live, frames) = styled
                        .borrow()
                        .as_ref()
                        .map_or((false, None), |s| (true, s.frames()));
                    uncounted_samples = if live && frames.is_none() {
                        uncounted_samples.saturating_add(1)
                    } else {
                        0
                    };
                    if uncounted_samples == UNCOUNTED_WARN_SAMPLES {
                        // Without the counters every video drop is charged to
                        // the effects again (B667), so say so once.
                        platform::warn(
                            "pharos: subtitle renderer frames uncounted renderer=jassub",
                        );
                    }
                    let lower = on_quality_sample(
                        &mut tier_watch.borrow_mut(),
                        "jassub",
                        now_ms_u64(),
                        (dropped, presented),
                        frames,
                        counting,
                    );
                    if let Some(lower) = lower {
                        tier.set(lower);
                        break;
                    }
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
        let session = session.clone();
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
            // In a group the group moves on, and its item brings every
            // member there; two members' up next moving it at once move it
            // once (014 US9).
            if session().is_some_and(|s| s.move_to(next, 0)) {
                return;
            }
            on_advance.call(next);
        }
    };
    // Only an episode moves on by itself.
    // The next episode often arrives after the player mounts, and the media
    // handlers can outlive the render that made them, so they read it here.
    let up_next_to = use_hook(|| Rc::new(Cell::new(None::<ItemRef>)));
    // In a group, moving on moves the group (`advance`).
    // A film's next is only ever a queued video (V853).
    up_next_to.set(near.next.filter(|_| video_kind(p.kind)));

    let on_media = {
        let (advance, up_next_to) = (advance.clone(), Rc::clone(&up_next_to));
        let (up_next_entry, up_next_reset) = (Rc::clone(&up_next_entry), Rc::clone(&up_next_reset));
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
        let music = p.kind == ItemKind::Audio;
        let was_paused = was_paused.clone();
        let autoplays = autoplay_after(grouped, start_paused);
        let autoplay_checked = Rc::clone(&autoplay_checked);
        move |event: MediaEvent| {
            if retiring.get() {
                return;
            }
            // The `autoplay` attribute fails silently when the browser
            // refuses it; a `play()` of our own at the first `canplay` reads
            // the refusal, so the player can say "Tap to start" (038 FR-407).
            if matches!(event, MediaEvent::CanPlay)
                && autoplays
                && !*started.peek()
                && !autoplay_checked.replace(true)
                && platform::media_state(VIDEO_ID).paused
            {
                platform::play(VIDEO_ID);
            }
            let resumed = match event {
                MediaEvent::Playing => {
                    if !*started.peek() {
                        let mut started = started;
                        started.set(true);
                    }
                    was_paused.0.set(false);
                    let now = platform::now_ms();
                    let mut slot = resume.borrow_mut();
                    slot.paused_at = None;
                    slot.probe.as_mut().and_then(|p| p.playing(now))
                }
                MediaEvent::Pause => {
                    was_paused.0.set(true);
                    let now = platform::now_ms();
                    let mut slot = resume.borrow_mut();
                    // Each pause restarts the clock: a play that never reached
                    // `playing` (refused, or stalled) did not end the pause
                    // the listener sees, but the newer pause is what they
                    // last did. An earlier rest timer sees the newer start
                    // and stands down.
                    slot.paused_at = Some(now);
                    // Only a song is tapped; a video has no context to rest.
                    if music {
                        spawn(rest_after_pause(Rc::clone(&resume), now));
                    }
                    slot.probe.as_mut().and_then(ResumeProbe::paused)
                }
                _ => None,
            };
            if let Some(report) = resumed {
                report_resume(report);
            }
            match event {
                MediaEvent::Waiting => {
                    watch.0.borrow_mut().stalled(now_ms_u64());
                    assess();
                }
                MediaEvent::Seeked => watch.0.borrow_mut().seeked(),
                _ => {}
            }
            // Whatever the seek waited on is over once the element lands,
            // plays or ends; a wait left standing would put its card over a
            // later, unrelated stall (B1196).
            if matches!(
                event,
                MediaEvent::Seeked | MediaEvent::Playing | MediaEvent::Ended
            ) {
                if let Some(w) = seek_wait.take() {
                    let waited = now_ms_u64().saturating_sub(w.started_ms);
                    if waited >= super::seek_wait::SHOW_AFTER_MS {
                        platform::info(&format!(
                            "pharos: seek wait: over event={event:?} to={} from={} waited_ms={waited}",
                            w.to_ms, w.from_ms
                        ));
                    }
                }
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
            if *position.peek() != state.position_ms {
                position.set(state.position_ms);
            }
            let to = platform::buffered_to_ms(VIDEO_ID);
            if *buffered.peek() != to {
                buffered.set(to);
            }
            // A new source or element starts at its default rate; the
            // viewer's speed carries over, except in a group, whose drift
            // correction owns the rate.
            let want = *speed.peek();
            if event == MediaEvent::Playing
                && !music
                && want != 1.0
                && !session().is_some_and(|s| s.drives())
                && platform::speed(VIDEO_ID) != want
            {
                platform::set_speed(VIDEO_ID, want);
            }
            if let Some(next) = up_next_to.get() {
                let total = total_ms(&state, p.detail.item.runtime_ms);
                let from = up_next_at(&p.detail.segments, total);
                // Leaving the group (no entry) keeps this player's Cancel.
                let entry = session().map(|s| s.playlist_item_id()).unwrap_or_default();
                let was = if entry.is_empty() {
                    String::new()
                } else {
                    up_next_entry.replace(entry.clone())
                };
                let now = platform::now_ms();
                if !was.is_empty() && was != entry {
                    up_next_reset.set(Some(now));
                }
                // The group lands on the new entry with a seek; one that
                // starts inside the card window lands there too. A player
                // already where the entry starts gets no seek: events queued
                // at the old end are long gone after a moment.
                let before_end = from.map_or(true, |f| state.position_ms < f.at_ms);
                let waited = up_next_reset.get().map(|since| now - since);
                let settled = waited.is_some_and(|ms| ms >= UP_NEXT_RESET_SETTLE_MS);
                if waited.is_some() && (before_end || event == MediaEvent::Seeked || settled) {
                    up_next_reset.set(None);
                    platform::info(&format!(
                        "pharos: up next: reset reason=group_entry entry={entry} at={}ms on={event:?} waited={:.0}ms",
                        state.position_ms,
                        waited.unwrap_or_default()
                    ));
                    up_next.set(UpNext::default());
                }
                // Another member cancelled it: this episode plays to its end
                // here too, rather than this player moving the group on.
                if !up_next.peek().is_cancelled() && session().is_some_and(|s| s.next_held()) {
                    platform::info(&format!(
                        "pharos: up next: cancelled reason=group at={}ms of={total}ms from={from:?}",
                        state.position_ms
                    ));
                    up_next.write().cancel();
                }
                // Still at the old entry's end: the group has already moved
                // on, so no countdown or end from there may move it again.
                if up_next_reset.get().is_none() {
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
            let tracks = reported_tracks(
                p.asked_audio,
                p.audio,
                p.asked_subtitle,
                p.subtitle,
                switched.get(),
            );
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
    // On the TV `timeupdate` comes through the page bridge, at most once a
    // second, instead of as a Dioxus event: each Dioxus event holds the
    // WebView until Rust answers, about 40 ms four times a second.
    let ticks = use_hook(|| Rc::new(Ticks::default()));
    *ticks.on_media.borrow_mut() = Some(Box::new(on_media.clone()));
    ticks.media_id.set(slot.media_id());
    use_hook({
        let ticks = Rc::clone(&ticks);
        move || {
            Rc::new(platform::on_time_update(move |id| {
                if id == ticks.media_id.get() {
                    if let Some(on_media) = ticks.on_media.borrow_mut().as_mut() {
                        on_media(MediaEvent::TimeUpdate);
                    }
                }
            }))
        }
    });

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
            // A menu pick as the primary's stream, which the group names
            // (ADR-0089); one the primary lacks stays on this player.
            let to_group = |kind: activity::TrackKind, own: u32| {
                let mapped = p.track_map.to_primary(kind, own);
                if mapped.is_none() {
                    platform::info(&format!(
                        "pharos: group tracks unmapped kind={kind:?} index={own} version={:?} dir=send",
                        p.version
                    ));
                }
                mapped
            };
            // In a group the viewer's play, pause and seek are the group's.
            let mut seek = |ms: u64| {
                watch.0.borrow_mut().seeking(now_ms_u64());
                if !session().is_some_and(|s| s.seek(ms)) {
                    // A group's seek is the group's to wait on; this one is
                    // the viewer's, with a way back if it is slow (B1196).
                    let earlier = *seek_wait.peek();
                    let at = *position.peek();
                    seek_wait.set(Some(SeekWait::begin(earlier, at, ms, now_ms_u64())));
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
                    let resuming =
                        (state.paused && state.position_ms > 0 && !in_group).then(|| {
                            let item = match p.kind {
                                ItemKind::Audio => ResumeItem::Music,
                                _ => ResumeItem::Video,
                            };
                            start_resume(VIDEO_ID, item, &resume)
                        });
                    // Read before the group acts: the browser's own flag
                    // lifts on pointerdown, before this click arrives.
                    let refused = *autoplay_refused.peek();
                    if !session().is_some_and(|s| s.toggle_play()) {
                        platform::toggle_play(VIDEO_ID);
                    } else if refused {
                        // The group's unpause lands after this gesture ends,
                        // and some browsers (WebKit) only let a play() made
                        // inside the gesture through; start the local player
                        // now (038 FR-407, V1223).
                        platform::play(VIDEO_ID);
                    }
                    if let Some(n) = resuming {
                        spawn(watch_resume(VIDEO_ID, Rc::clone(&resume), n));
                    }
                }
                // An unknown length (0) clamps nothing rather than everything.
                Control::Seek(ms) if total > 0 => seek(ms.min(total)),
                Control::Seek(ms) => seek(ms),
                Control::Hover(x) => hover_ms
                    .set(x.and_then(|x| position_at(x, platform::element_width(SEEK_ID), total))),
                // Without a volume control the device's own volume rules.
                Control::Volume { .. } if !platform::owns_volume() => {}
                Control::Volume { volume, muted } => {
                    fade_in_cancel.set(true);
                    platform::set_volume(VIDEO_ID, volume, muted);
                    remember_volume(volume, muted);
                }
                Control::Fullscreen => platform::toggle_fullscreen(),
                Control::SubtitleSize(size) => {
                    if let Err(e) = set_subtitle_size(size) {
                        platform::warn(&format!("pharos: subtitle_size not kept error={e}"));
                    }
                    sub_size.set(size);
                }
                Control::ViewMode(mode) => {
                    store_view_mode(mode);
                    view_mode.set(mode);
                }
                Control::Quality(bps) => on_quality_pick.call(Some(bps)),
                Control::SubtitleDelay(ms) => {
                    platform::info(&format!(
                        "pharos: subtitle delay from={} ms to={ms} ms",
                        *subtitle_delay.peek()
                    ));
                    subtitle_delay.set(ms);
                    platform::set_subtitle_delay(VIDEO_ID, ms);
                }
                Control::Speed(rate) => {
                    if session().is_some_and(|s| s.drives()) {
                        platform::info(&format!(
                            "pharos: speed unchanged to={rate} reason=in_group"
                        ));
                    } else {
                        platform::info(&format!("pharos: speed from={} to={rate}", *speed.peek()));
                        speed.set(rate);
                        LAST_SPEED.with(|s| s.set(rate));
                        platform::set_speed(VIDEO_ID, rate);
                    }
                }
                Control::Version(i) => {
                    if let Some(v) = p.detail.versions.get(i) {
                        on_version.call(v.id.clone());
                    }
                }
                Control::ResetAutoQuality => on_quality_pick.call(None),
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
                // replaying every episode watched. On the group's player it
                // moves the group, whose item brings this member too (014 US9).
                Control::Play(id) => {
                    if !session().is_some_and(|s| s.move_to(id, 0)) {
                        if near.previous == Some(id) {
                            turn::going_back();
                        }
                        go(id);
                    }
                }
                Control::PlayNext => {
                    if let Some(next) = up_next_to.get() {
                        platform::info(&format!("pharos: up next: playing {next} (play now)"));
                        advance(next);
                    }
                }
                Control::ReturnFromSeek(ms) => {
                    platform::info(&format!("pharos: seek wait: going back to={ms}"));
                    seek_wait.set(None);
                    watch.0.borrow_mut().seeking(now_ms_u64());
                    platform::seek_ms(VIDEO_ID, ms);
                }
                Control::CancelNext => {
                    let shared = session().is_some_and(|s| s.hold_next());
                    platform::info(&format!(
                        "pharos: up next: cancelled reason=viewer shared={shared}"
                    ));
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
                Control::ShowQueue(open) => {
                    let Some(VideoQueueCtx(q, mut shown)) = video_queue else {
                        return;
                    };
                    shown.set(open);
                    platform::info(&format!(
                        "pharos: video queue shown={open} upcoming={}",
                        q.peek().upcoming().len()
                    ));
                }
                Control::Queue(action) => {
                    let Some(VideoQueueCtx(mut q, _)) = video_queue else {
                        return;
                    };
                    let play = match action {
                        QueueAction::Play(key) => q
                            .peek()
                            .upcoming()
                            .iter()
                            .find(|e| e.key == key)
                            .map(|e| e.id),
                        _ => None,
                    };
                    q.write().apply(action, seed());
                    platform::info(&format!(
                        "pharos: video queue action={} upcoming={}",
                        action.label(),
                        q.peek().upcoming().len()
                    ));
                    if let Some(id) = play {
                        go(id);
                    }
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
                        Some(id) => {
                            turn::going_back();
                            go(id);
                        }
                        None => seek(0),
                    }
                }
                // Leaving the watch page shrinks the player (spec 020).
                Control::Back => leave_player(nav),
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
                    if let Some(series) = show_tracks::show_of(&p.detail) {
                        let tracks = &p.detail.audio_tracks;
                        show_tracks::keep(series, |s| show_tracks::chose_audio(s, tracks, index));
                    }
                    let Some(index) = track_follow::audio_pick_here(p.audio, index, |index| {
                        to_group(activity::TrackKind::Audio, index)
                            .is_some_and(|i| session().is_some_and(|s| s.pick_audio(i)))
                    }) else {
                        return;
                    };
                    on_notice.call(track_notice(true));
                    on_renegotiate.call(TrackPicks {
                        audio: TrackChoice::Index(index),
                        subtitle: choice(*subtitle.peek()),
                    });
                }
                Control::Subtitle(to) => {
                    if let Some(series) = show_tracks::show_of(&p.detail) {
                        let tracks = &p.detail.subtitle_tracks;
                        show_tracks::keep(series, |s| show_tracks::chose_subtitle(s, tracks, to));
                    }
                    let here = track_follow::subtitle_pick_here(*subtitle.peek(), to, |to| {
                        let to = match to {
                            None => Some(None),
                            Some(i) => to_group(activity::TrackKind::Subtitles, i).map(Some),
                        };
                        to.is_some_and(|to| session().is_some_and(|s| s.pick_subtitle(to)))
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
                        SubtitleSwitch::Renegotiate => {
                            on_notice.call(track_notice(false));
                            on_renegotiate.call(TrackPicks {
                                audio: p.audio.map_or(TrackChoice::Default, TrackChoice::Index),
                                subtitle: choice(to),
                            });
                        }
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
    let me = account.and_then(|a| {
        a.0.read()
            .as_ref()
            .and_then(Option::as_ref)
            .map(|m| (m.id.clone(), m.name.clone()))
    });
    let members_waiting: Vec<String> = match me {
        Some((id, name)) if group.drives(playing_id) => group
            .members
            .iter()
            .filter(|m| m.buffering && m.id != id && m.name != name)
            .map(|m| m.name.clone())
            .collect(),
        _ => Vec::new(),
    };
    // The group can pause a ready player while another member loads; that
    // is a wait too, and says whom it is for (038 FR-401). Only while paused:
    // a member catching up behind a player that still plays is not a wait.
    let rest = media_rest();
    let waiting =
        (rest.loading || (rest.paused && !members_waiting.is_empty())) && failed().is_none();
    let wait_caption = if waiting {
        let facts = super::wait::WaitFacts {
            started: started(),
            seeking_to: seek_wait().map(|w| w.to_ms),
            hint: queue_hint(),
            members_waiting: &members_waiting,
        };
        let cause = super::wait::cause(&facts);
        let caption = cause.caption();
        // The decision line, once per caption a wait shows.
        if *wait_logged.borrow() != caption {
            platform::info(&super::wait::line(&playing_id.to_string(), &cause, &facts));
            caption.clone_into(&mut wait_logged.borrow_mut());
        }
        caption
    } else {
        wait_logged.borrow_mut().clear();
        String::new()
    };
    let controls = Controls {
        media: MediaState {
            loading: waiting,
            ..media_rest()
        },
        wait_caption,
        tap_to_start: autoplay_refused(),
        runtime_ms: p.detail.item.runtime_ms,
        chapters: p.detail.chapters.clone(),
        segments: p.detail.segments.clone(),
        damage: p.detail.damage.clone(),
        preview: preview_set(&p.detail.trickplay).cloned(),
        hover_ms: hover_ms(),
        audio: p.detail.audio_tracks.clone(),
        subtitles: offered_subtitles(&p.detail.subtitle_tracks, effects()),
        audio_pick: p.audio,
        subtitle_pick: subtitle(),
        previous: near.previous,
        next,
        seek_card: seek_wait()
            .filter(|_| media_rest().loading && failed().is_none())
            .and_then(|w| {
                let unindexed = p
                    .detail
                    .damage
                    .as_ref()
                    .is_some_and(|d| d.verdict == "unindexed");
                w.card(seek_tick(), unindexed)
            }),
        up_next: match (up_next_step(), upcoming.as_ref()) {
            (UpNextStep::Counting { left_ms, of_ms }, next) => {
                // Only the card's own item: a group's next is not the queue's.
                let next = next.filter(|n| Some(n.id) == up_next_to.get());
                let mut card = UpNextCard::of(next, left_ms, of_ms);
                if next.is_none() && p.kind != ItemKind::Episode {
                    "Next".clone_into(&mut card.name);
                }
                let credits = up_next_at(
                    &p.detail.segments,
                    total_ms(&media_rest(), p.detail.item.runtime_ms),
                )
                .is_some_and(|from| from.credits);
                card.pill = up_next_pill(credits, left_ms);
                // The decision line, once per way it is shown (038 FR-405/406).
                let mode = match (view, card.pill) {
                    (View::Full, true) => "pill",
                    (View::Full, false) => "card",
                    _ => "line",
                };
                if up_next_mode.get() != Some(mode) {
                    up_next_mode.set(Some(mode));
                    platform::info(&format!(
                        "pharos: up next: shown item={playing_id} mode={mode} credits={credits} left_ms={left_ms} of_ms={of_ms}"
                    ));
                }
                Some(card)
            }
            _ => {
                up_next_mode.set(None);
                None
            }
        },
        live_position: Some(position),
        live_buffered: Some(buffered),
        subtitle_size: sub_size(),
        packing: versions::playing_stereo(&p.detail.versions, &p.version),
        // A burned subtitle or the browser's own cues may sit in a letterbox
        // bar, so those stay (V977, B1215).
        picture: versions::playing_picture(&p.detail.versions, &p.version)
            .map(|crop| {
                let text_picked = subtitle().is_some_and(|i| {
                    p.detail
                        .subtitle_tracks
                        .iter()
                        .any(|t| t.index == i && t.vtt_url.is_some() && t.images_url.is_none())
                });
                let cues = cues_in_video(text_picked, styled_ready(), rust_state());
                let kept = rows_kept_for(p.burned(), cues);
                if crop_rows_kept.get() != Some(kept) {
                    crop_rows_kept.set(Some(kept));
                    platform::info(&format!(
                        "pharos: picture crop item={playing_id} rows_kept={}",
                        kept.unwrap_or("no")
                    ));
                }
                if kept.is_some() {
                    crop.keeping_rows()
                } else {
                    crop
                }
            })
            .filter(|crop| crop.crops()),
        view_mode: view_mode(),
        vr: vr().unwrap_or(false),
        versions: p
            .detail
            .versions
            .iter()
            .map(|v| (v.id.clone(), versions::version_label(v)))
            .collect(),
        version: p.version.clone(),
        speed: speed(),
        subtitle_delay_ms: subtitle_delay(),
        quality: QualityView {
            manual: manual_cap,
            auto: auto_cap,
            method: p.method.clone(),
        },
    };
    let on_leave = {
        let session = session.clone();
        let id = group.id.clone().filter(|_| group.active);
        let mut left_group = left_group;
        let leaving = Rc::clone(&leaving);
        move |()| {
            if let Some(session) = session() {
                left_group.set(id.clone());
                let left = async move { session.leave().await }.boxed_local().shared();
                *leaving.borrow_mut() = Some(left.clone());
                spawn(left);
            }
        }
    };
    let on_undo_leave = {
        let together = together.clone();
        let mut left_group = left_group;
        let leaving = Rc::clone(&leaving);
        move |()| {
            let Some(id) = left_group.take() else {
                return;
            };
            let Some(api) = ctx.api.peek().clone() else {
                return;
            };
            let session = together.session(&api);
            let mut note = together.note;
            note.set(None);
            platform::info(&format!("pharos: group leave undo group={id}"));
            let left = leaving.borrow_mut().take();
            spawn_forever(async move {
                if let Some(left) = left {
                    left.await;
                }
                match session.join(&id).await {
                    Ok(_) => platform::info(&format!(
                        "pharos: group leave undone group={id} outcome=rejoined"
                    )),
                    Err(e) => {
                        platform::warn(&format!(
                            "pharos: group leave undo failed group={id} error={}",
                            e.0
                        ));
                        note.set(Some(super::group_app::rejoin_failure(&e.0)));
                    }
                }
            });
        }
    };
    let notes = NotesView {
        connection: connection_lost.then(|| CONNECTION_LOST.to_owned()),
        wait: wait_pill,
        status: group_status.filter(|_| status_shown),
        undo: undo_shown,
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
    // The queue docked beside the compact player (B900/V852), shared by the
    // stage's players so a song handing over keeps it open.
    let own_dock = use_signal(|| false);
    let mut dock = try_use_context::<QueueDock>().map_or(own_dock, |d| d.0);
    // Exactly one slot is in sight: the playing one, or the outgoing one
    // while the next is negotiated. Only it docks a queue.
    let in_sight = matches!(
        slot,
        Slot::Current { waiting: false, .. } | Slot::Outgoing(Handoff { ready: false, .. })
    );
    // Which side the floating window is on, so the dock takes the other.
    let window_right = use_memo(move || stage.read().spot.x > 0.5)();
    // And which half, so the dock keeps to the same edge (the top holds the
    // page's navigation).
    let window_low = use_memo(move || stage.read().spot.y > 0.5)();
    let on_queue = EventHandler::new(move |action: QueueAction| {
        // In a group the group picks the song.
        if matches!(action, QueueAction::Play(_)) && !queued {
            platform::info("pharos: queue action=play refused reason=group");
            return;
        }
        let play = match action {
            QueueAction::Play(key) => queue
                .peek()
                .upcoming()
                .iter()
                .find(|e| e.key == key)
                .map(|e| e.id),
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
    });
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
                StageAction::Queue => {
                    let open = !*dock.peek();
                    dock.set(open);
                    super::listen::store_dock(open);
                    platform::info(&super::stage::dock_line(
                        open,
                        view,
                        queue.peek().upcoming().len(),
                    ));
                }
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
    let retry_audio = p.audio;
    let heading = super::views::watch_heading(&p.detail.item);
    rsx! {
        if compact {
            StageBar {
                view,
                title: p.title.clone(),
                image: p.image.clone(),
                media: media_rest(),
                live_position: Some(position),
                runtime_ms: p.detail.item.runtime_ms,
                pip: p.kind != ItemKind::Audio && platform::pip_supported(),
                previous: near.previous,
                next,
                error: failed().map(|f| f.line()).or_else(|| group.ended.clone()),
                on_action,
                queue: audio.then(|| QueueButton {
                    count: queue.read().upcoming().len(),
                    open: dock(),
                }),
                up_next_in: controls.up_next.as_ref().map(|card| card.seconds),
            }
            // Only the slot in sight: a song handing over shows one queue.
            if audio && dock() && in_sight {
                div {
                    class: "pharos-queue-dock",
                    "data-view": view.label(),
                    "data-side": if view == View::Floating && window_right { "left" } else { "right" },
                    "data-edge": if view == View::Floating && window_low { "bottom" } else { "top" },
                    QueuePanel {
                        queue: queue.read().upcoming().to_vec(),
                        shuffled: queue.read().shuffled(),
                        on_queue,
                        playing: queued.then_some(playing_id),
                    }
                }
            }
        } else {
            if let Some(failure) = failed() {
                PlaybackFailure {
                    failure: failure.clone(),
                    on_report: move |()| report_failure(&failure),
                    on_retry: {
                        let last_ms = Rc::clone(&last_ms.0);
                        move |()| {
                            platform::info(&format!(
                                "pharos: playback retry item={playing_id} at_ms={}",
                                last_ms.get()
                            ));
                            on_renegotiate.call(TrackPicks {
                                audio: retry_audio.map_or(TrackChoice::Default, TrackChoice::Index),
                                subtitle: choice(*subtitle.peek()),
                            });
                        }
                    },
                    on_back: move |()| leave_player(nav),
                    // The renegotiation carries on from where `last_ms` says,
                    // as Try again does; the element that broke cannot seek.
                    on_skip: {
                        let (last_ms, broke_at) = (Rc::clone(&last_ms.0), Rc::clone(&broke_at));
                        let was_paused = was_paused.clone();
                        let version = p.version.clone();
                        move |to_ms: u64| {
                            let from_ms = broke_at.get();
                            // The viewer asked to carry on: the next player
                            // plays, whatever the broken one was left at.
                            was_paused.0.set(false);
                            remember_skip(playing_id, &version, from_ms, to_ms);
                            platform::info(&format!(
                                "pharos: playback skip item={playing_id} from_ms={from_ms} to_ms={to_ms}"
                            ));
                            last_ms.set(to_ms);
                            on_renegotiate.call(TrackPicks {
                                audio: retry_audio.map_or(TrackChoice::Default, TrackChoice::Index),
                                subtitle: choice(*subtitle.peek()),
                            });
                        }
                    },
                }
            }
            // A video's player stacks this with its other group notes.
            if p.kind == ItemKind::Audio {
                if let Some(line) = notes.connection.clone() {
                    p { class: "pharos-status-note pharos-connection-lost", role: "status", "{line}" }
                }
                if let Some(message) = notes.status.clone() {
                    GroupEndedNote { message }
                }
            }
            if let Some(message) = together.error.read().clone() {
                ErrorNote { message }
            }
        }
        if p.kind == ItemKind::Audio {
            ListenView {
                // A retired song's screen is hidden; it draws no visualiser.
                compact: compact || matches!(slot, Slot::Outgoing(Handoff { ready: true, .. })),
                in_sight,
                media_id: slot.media_id(),
                title: p.title.clone(),
                image: p.image.clone(),
                artists: p.detail.artists.clone(),
                album: p.detail.album.clone(),
                track: p.detail.item.episode,
                year: p.detail.item.year,
                genres: p.detail.genres.clone(),
                runtime_ms: p.detail.item.runtime_ms,
                media: media_rest(),
                live_position: Some(position),
                previous: near.previous,
                next,
                lyrics: aligned()
                    .filter(|(for_id, _)| *for_id == p.detail.item.id)
                    .map(|(_, l)| l)
                    .or_else(|| lyrics.read().clone().and_then(Result::ok).flatten()),
                lyrics_loading: lyrics.read().is_none(),
                lyrics_failed: matches!(*lyrics.read(), Some(Err(()))),
                words_eta_at: words_eta_at()
                    .filter(|(for_id, _)| *for_id == p.detail.item.id)
                    .map(|(_, at)| at),
                playing: queued.then_some(playing_id),
                queue: queue.read().upcoming().to_vec(),
                shuffled: queue.read().shuffled(),
                on_queue,
                on_media,
                on_control,
            }
        } else {
            WatchView {
                compact,
                title: heading.0,
                title_detail: heading.1,
                controls,
                on_media,
                on_control,
                item_id: p.detail.item.id,
                members: if group.active { group.members.clone() } else { Vec::new() },
                own_tracks: group.own_tracks,
                on_own_tracks,
                activity: group.log.lines(),
                toast: group.log.toast(now_ms_u64()).map(str::to_owned).or(notice),
                notes,
                on_undo_leave,
                own_track_members: group
                    .members
                    .iter()
                    .filter(|m| m.own_tracks)
                    .map(|m| m.id.clone())
                    .collect::<Vec<_>>(),
                seen: seen::rows(&watch_seen, p.detail.item.id),
                on_leave,
                autoplay: autoplay_after(grouped, start_paused),
                can_start_together: can_watch_together(p.kind) && !compact,
                invite,
                invite_open,
                invite_note: invite_note(),
                queue: video_queue.filter(|_| !grouped).map(|VideoQueueCtx(q, shown)| {
                    let q = q.read();
                    VideoQueueView {
                        entries: q.upcoming().to_vec(),
                        shuffled: q.shuffled(),
                        open: shown() && !compact,
                    }
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_apps_outdated_notice_names_both_versions_and_the_update() {
        let line = app_outdated_line(41, 43);
        assert!(line.contains("version 43"), "{line}");
        assert!(line.contains("this app is version 41"), "{line}");
        assert!(line.contains("install the latest Pharos TV app"), "{line}");
    }

    #[test]
    fn a_remount_keeps_a_paused_video_paused() {
        assert!(autoplay_after(false, false));
        assert!(!autoplay_after(false, true), "paused stays paused");
        assert!(!autoplay_after(true, false), "a group waits for its play");
        assert!(!autoplay_after(true, true));
    }

    #[test]
    fn a_quality_pick_names_the_new_limit() {
        assert_eq!(quality_choice_notice(0), "Quality: Auto");
        assert_eq!(quality_choice_notice(4_000_000), "Quality: up to 4 Mbps");
    }

    #[test]
    fn the_notices_name_what_is_changing() {
        assert_eq!(track_notice(true), "Switching audio…");
        assert_eq!(track_notice(false), "Switching subtitles…");
        assert_eq!(
            quality_notice(4_000_000, 2_000_000),
            "Lowering quality to 2 Mbps"
        );
        assert_eq!(
            quality_notice(2_000_000, 8_000_000),
            "Raising quality to 8 Mbps"
        );
    }

    /// An audio track the viewer did not ask for is left to the server's
    /// rules, so the report says nothing about it.
    #[test]
    fn an_audio_track_left_to_the_server_is_reported_as_default() {
        let picks = reported_tracks(None, Some(2), TrackChoice::Default, None, None);
        assert_eq!(picks.audio, TrackChoice::Default);
    }

    /// The audio the viewer asked for is named as it plays, so the next
    /// play starts on the same track.
    #[test]
    fn an_asked_audio_track_is_reported_as_it_plays() {
        let picks = reported_tracks(Some(3), Some(3), TrackChoice::Default, None, None);
        assert_eq!(picks.audio, TrackChoice::Index(3));
    }

    /// An asked audio the server did not start on has nothing to name, so
    /// the report leaves it to the server.
    #[test]
    fn an_asked_audio_the_server_did_not_start_is_default() {
        let picks = reported_tracks(Some(3), None, TrackChoice::Default, None, None);
        assert_eq!(picks.audio, TrackChoice::Default);
    }

    /// Subtitles asked off are reported off, so the next play stays off.
    #[test]
    fn subtitles_asked_off_are_reported_off() {
        let picks = reported_tracks(None, None, TrackChoice::Off, None, None);
        assert_eq!(picks.subtitle, TrackChoice::Off);
    }

    /// A subtitle asked for by index is named as it plays.
    #[test]
    fn an_asked_subtitle_is_reported_as_it_plays() {
        let picks = reported_tracks(None, None, TrackChoice::Index(4), Some(4), None);
        assert_eq!(picks.subtitle, TrackChoice::Index(4));
    }

    /// A subtitle the viewer did not ask for is left to the server, even
    /// when the server started on one.
    #[test]
    fn a_subtitle_left_to_the_server_is_reported_as_default() {
        let picks = reported_tracks(None, None, TrackChoice::Default, Some(2), None);
        assert_eq!(picks.subtitle, TrackChoice::Default);
    }

    /// A subtitle switched to in place is what plays now, so it beats the
    /// subtitle the playback was asked for, including switching off.
    #[test]
    fn an_in_place_switch_beats_the_asked_subtitle() {
        let switched = reported_tracks(
            None,
            None,
            TrackChoice::Index(4),
            Some(4),
            Some(TrackChoice::Index(7)),
        );
        assert_eq!(switched.subtitle, TrackChoice::Index(7));
        let off = reported_tracks(
            None,
            None,
            TrackChoice::Index(4),
            Some(4),
            Some(TrackChoice::Off),
        );
        assert_eq!(off.subtitle, TrackChoice::Off);
    }

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
    #[cfg(not(target_os = "android"))]
    fn the_host_starts_on_the_desktop_tier() {
        assert_eq!(current_starting_tier(), Tier::Desktop);
    }

    #[test]
    fn a_cleared_tier_cap_lets_the_device_start_at_its_own_tier_again() {
        store_tier_drop(Tier::Low).unwrap();
        assert_eq!(stored_tier_drop(), Some(Tier::Low));
        clear_tier_drop().unwrap();
        assert_eq!(stored_tier_drop(), None);
    }

    #[test]
    fn a_chosen_effects_detail_outlives_a_storage_that_keeps_nothing() {
        assert_eq!(stored_effects_detail(), EffectsDetail::Auto);
        store_effects_detail(EffectsDetail::Rich).unwrap();
        assert_eq!(stored_effects_detail(), EffectsDetail::Rich);
    }

    #[test]
    fn a_chosen_speaker_colours_switch_outlives_a_storage_that_keeps_nothing() {
        assert!(!stored_speaker_colours());
        store_speaker_colours(true).unwrap();
        assert!(stored_speaker_colours());
    }

    #[test]
    fn a_chosen_effects_level_outlives_a_storage_that_keeps_nothing() {
        assert_eq!(stored_effects_level(), EffectsLevel::Subtle);
        store_effects_level(EffectsLevel::Full).unwrap();
        assert_eq!(stored_effects_level(), EffectsLevel::Full);
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

    /// FR-206: while Home loads, the Home last shown stands in; with none
    /// kept the page says it is loading.
    #[test]
    fn home_draws_the_last_home_shown_while_it_loads() {
        #[component]
        fn Harness(kept: bool) -> Element {
            use_context_provider(|| crate::native::views::Nav(EventHandler::new(|_: Route| {})));
            use_context_provider(|| Ctx {
                api: Signal::new(None::<NativeApi>),
                machine: Signal::new(Machine::new()),
                marks: Signal::new(0),
            });
            let revisit = use_context_provider(super::super::revisit::Revisit::new);
            if kept {
                let film = pharos_client::api::Item {
                    id: ItemRef::Real(7),
                    kind: pharos_client::api::ItemKind::Movie,
                    name: "An Invented Film".to_owned(),
                    year: None,
                    runtime_ms: 600_000,
                    image: None,
                    played: false,
                    favorite: false,
                    position_ms: 60_000,
                    series_name: None,
                    season: None,
                    episode: None,
                    parent: None,
                    library_kind: None,
                    imdb_rating: None,
                    critic_rating: None,
                    artist: None,
                    stereo: None,
                    damaged: false,
                    new_children: 0,
                };
                let mut home = revisit.home;
                home.set(Some(Home {
                    resume: vec![film],
                    ..Home::default()
                }));
            }
            rsx! { HomeScreen {} }
        }
        let html = |kept| {
            let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { kept });
            dom.rebuild_in_place();
            dioxus_ssr::render(&dom)
        };
        let shown = html(true);
        assert!(shown.contains("An Invented Film"), "{shown}");
        let bare = html(false);
        assert!(!bare.contains("An Invented Film"), "{bare}");
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

    #[test]
    fn a_dropped_tier_outlives_a_storage_that_keeps_nothing() {
        assert_eq!(stored_tier_drop(), None);
        store_tier_drop(Tier::Tv).unwrap();
        assert_eq!(stored_tier_drop(), Some(Tier::Tv));
    }

    #[test]
    fn the_starting_tier_is_capped_at_the_dropped_one() {
        // The host is a Desktop device, so only the cap can make this Tv.
        store_tier_drop(Tier::Tv).unwrap();
        assert_eq!(current_starting_tier(), Tier::Tv);
    }

    #[test]
    fn a_later_drop_never_raises_the_stored_cap() {
        store_tier_drop(Tier::Low).unwrap();
        store_tier_drop(Tier::Tv).unwrap();
        assert_eq!(stored_tier_drop(), Some(Tier::Low));
    }

    #[test]
    fn a_bad_stream_drops_the_tier_once_and_remembers_it() {
        let mut watch = TierDrop::new(Tier::Desktop);
        let drops: Vec<Tier> = (0..=25u64)
            .filter_map(|sec| {
                on_quality_sample(
                    &mut watch,
                    "jassub",
                    sec * 1000,
                    (sec * 30, sec * 60),
                    None,
                    true,
                )
            })
            .collect();
        assert_eq!(drops, [Tier::Tv]);
        assert_eq!(stored_tier_drop(), Some(Tier::Tv));
    }

    #[test]
    fn a_hidden_page_never_drops_the_tier() {
        let mut watch = TierDrop::new(Tier::Desktop);
        for sec in 0..=60u64 {
            assert_eq!(
                on_quality_sample(
                    &mut watch,
                    "jassub",
                    sec * 1000,
                    (sec * 30, sec * 60),
                    None,
                    false
                ),
                None
            );
        }
        assert_eq!(stored_tier_drop(), None);
    }
}
