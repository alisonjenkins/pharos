//! What's new and first-run setup (spec 027, ADR-0065), one dialog after
//! sign-in in two flows:
//!
//! - **Setup**, for an account that has never seen it: a welcome, then the
//!   [`SETUP`] settings and every setting in [`ENTRIES`], each with a live
//!   demo and its picker.
//! - **What's new**, for everyone else: the settings added since the viewer
//!   last looked, then a few lines on new features and fixes.
//!
//! [`ENTRIES`] is the registry. A change that ships a setting or a change
//! worth telling viewers about appends one entry with the next `id`; the
//! newest id the viewer has been shown is kept on the server under the
//! `whats_new.seen` pref, so a viewer sees each entry once on any device.
//! Accounts that existed when this shipped were given `0` by a migration, so
//! only an account made later has no value and gets setup. The view here is
//! pure (props in, events out) so it renders under `dioxus-ssr`; the app owns
//! loading, the gate and saving.

use dioxus::prelude::*;

use super::appearance::ThemeCard;
use super::platform;
use super::playback::{LibraryTransition, OverlayEffect, SubtitleSize};

/// The pref holding the newest entry id the viewer has been shown.
pub const SEEN_PREF: &str = "whats_new.seen";

/// The device setting (localStorage) that turns the dialog off on this
/// device when it holds [`OFF`]: for kiosks and the browser suite, whose
/// fresh accounts would otherwise meet it on every sign-in.
pub const DEVICE_KEY: &str = "pharos.whats_new";
pub const OFF: &str = "off";

/// Most changelog lines shown at once, newest first, so a viewer who has
/// not looked for a long time is not handed the project's whole history.
pub const MAX_NOTES: usize = 8;

/// A setting the dialog can demo and set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Theme,
    SubtitleSize,
    LibraryTransition,
    ControlsEffect,
}

impl Setting {
    pub fn title(self) -> &'static str {
        match self {
            Setting::Theme => "Pick a theme",
            Setting::SubtitleSize => "Subtitle size",
            Setting::LibraryTransition => "Library card transitions",
            Setting::ControlsEffect => "Controls effect",
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Setting::Theme => "Colours for the whole app. The page changes as you pick.",
            Setting::SubtitleSize => "How big subtitles are drawn over a video.",
            Setting::LibraryTransition => "Pick how a library's cards arrive when you open it.",
            Setting::ControlsEffect => {
                "Pick how the player's controls hide while you watch, and come back when you move."
            }
        }
    }
}

/// The settings a new account is walked through, before any in [`ENTRIES`].
pub const SETUP: &[Setting] = &[Setting::Theme, Setting::SubtitleSize];

/// What an entry tells the viewer about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A new setting: gets a page of its own with a demo and its picker.
    Setting(Setting),
    Feature,
    Fix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// Unique and increasing in [`ENTRIES`] order; never reused.
    pub id: u32,
    pub kind: Kind,
    pub title: &'static str,
    pub text: &'static str,
}

/// Every entry, oldest first. Append only.
pub const ENTRIES: &[Entry] = &[
    Entry {
        id: 1,
        kind: Kind::Setting(Setting::LibraryTransition),
        title: "Library card transitions",
        text: "Pick how a library's cards arrive when you open it.",
    },
    Entry {
        id: 2,
        kind: Kind::Feature,
        title: "Watch together from the player",
        text: "Start a group part way through a film from the player and share its link.",
    },
    Entry {
        id: 3,
        kind: Kind::Feature,
        title: "Chapters on the seek bar",
        text: "The seek bar has a tick at every chapter.",
    },
    Entry {
        id: 4,
        kind: Kind::Feature,
        title: "Descriptions in your language",
        text: "Pick the language of show and episode descriptions on the Account page.",
    },
    Entry {
        id: 5,
        kind: Kind::Feature,
        title: "Libraries remember your place",
        text: "A library keeps its filters and where you were when you come back to it.",
    },
    Entry {
        id: 6,
        kind: Kind::Feature,
        title: "Word-by-word karaoke",
        text: "Lyrics with word times fill in word by word as the song plays.",
    },
    Entry {
        id: 7,
        kind: Kind::Fix,
        title: "Subtitles sit better",
        text: "Subtitles size to the player and sit clear of the bottom edge.",
    },
    Entry {
        id: 8,
        kind: Kind::Fix,
        title: "Track keys work everywhere",
        text:
            "Next and previous track keys work from anywhere in the page, and media keys skip too.",
    },
    Entry {
        id: 9,
        kind: Kind::Fix,
        title: "Up next on time",
        text: "Up next shows well before the end of an episode with no detected credits.",
    },
    Entry {
        id: 10,
        kind: Kind::Setting(Setting::ControlsEffect),
        title: "Controls effect",
        text: "Pick how the player's controls hide while you watch, and come back when you move.",
    },
    Entry {
        id: 11,
        kind: Kind::Feature,
        title: "In progress filter",
        text: "A library's Watched filter lists what you started and didn't finish, with a button on each to mark it watched.",
    },
];

/// The newest entry id: what Done records as seen.
pub fn current() -> u32 {
    ENTRIES.iter().map(|e| e.id).max().unwrap_or(0)
}

/// Which walk the dialog gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flow {
    /// A new account: welcome and set up.
    #[default]
    Setup,
    WhatsNew,
}

impl Flow {
    pub fn label(self) -> &'static str {
        match self {
            Flow::Setup => "setup",
            Flow::WhatsNew => "whats_new",
        }
    }
}

/// One page of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Welcome,
    Setting(Setting),
    Notes,
}

/// What a viewer is shown, from their stored seen id: `None` (a new
/// account) gets [`Flow::Setup`]; `Some(seen)` gets the settings after
/// `seen`, oldest first, and up to [`MAX_NOTES`] feature and fix lines,
/// newest first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Unseen {
    pub flow: Flow,
    pub settings: Vec<Setting>,
    pub notes: Vec<Entry>,
}

impl Unseen {
    pub fn since(seen: Option<u32>) -> Unseen {
        let Some(after) = seen else {
            let mut settings = SETUP.to_vec();
            for e in ENTRIES {
                if let Kind::Setting(s) = e.kind {
                    if !settings.contains(&s) {
                        settings.push(s);
                    }
                }
            }
            return Unseen {
                flow: Flow::Setup,
                settings,
                notes: Vec::new(),
            };
        };
        let fresh = ENTRIES.iter().filter(|e| e.id > after);
        let settings = fresh
            .clone()
            .filter_map(|e| match e.kind {
                Kind::Setting(s) => Some(s),
                _ => None,
            })
            .collect();
        let mut notes: Vec<Entry> = fresh
            .filter(|e| !matches!(e.kind, Kind::Setting(_)))
            .copied()
            .collect();
        notes.reverse();
        notes.truncate(MAX_NOTES);
        Unseen {
            flow: Flow::WhatsNew,
            settings,
            notes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.settings.is_empty() && self.notes.is_empty()
    }

    /// The pages in order: setup opens with a welcome; then one per setting;
    /// then the changelog when it has lines.
    pub fn pages(&self) -> Vec<Page> {
        let welcome = (self.flow == Flow::Setup).then_some(Page::Welcome);
        let notes = (!self.notes.is_empty()).then_some(Page::Notes);
        welcome
            .into_iter()
            .chain(self.settings.iter().copied().map(Page::Setting))
            .chain(notes)
            .collect()
    }

    /// How many things the badge says are new.
    pub fn count(&self) -> usize {
        self.settings.len().saturating_add(self.notes.len())
    }
}

/// The stored `whats_new.seen` value as an id. `None` is an account with no
/// value; an unreadable value counts as `0`, so the viewer gets what's new
/// rather than setup.
pub fn parse_seen(stored: Option<&[u8]>) -> Option<u32> {
    stored.map(|v| {
        std::str::from_utf8(v)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    })
}

/// Whether the dialog opens by itself as a splash, decided once when the
/// seen id has loaded after a page load or sign-in, and why not. When it
/// does not, the top bar's badge ([`badge`]) offers it instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Open,
    /// Turned off on this device ([`DEVICE_KEY`]).
    Off,
    /// The seen id has not loaded yet.
    Loading,
    /// Nothing newer than what the viewer has seen.
    NothingNew,
    /// Something is on the stage (a video or a song): no splash.
    Playing,
    /// The watch page is open: no splash.
    Watching,
}

impl Gate {
    pub fn label(self) -> &'static str {
        match self {
            Gate::Open => "open",
            Gate::Off => "off",
            Gate::Loading => "loading",
            Gate::NothingNew => "nothing_new",
            Gate::Playing => "playing",
            Gate::Watching => "watching",
        }
    }
}

/// The inputs the splash decides on. A page loaded on the watch page, or
/// with something playing, never splashes: the badge waits instead (V495).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateInputs {
    /// `None` while loading; `Some(seen)` once read (`seen` is `None` for a
    /// new account).
    pub seen: Option<Option<u32>>,
    pub off: bool,
    pub playing: bool,
    pub watch_route: bool,
}

pub fn gate(at: GateInputs) -> Gate {
    if at.off {
        return Gate::Off;
    }
    let Some(seen) = at.seen else {
        return Gate::Loading;
    };
    if Unseen::since(seen).is_empty() {
        Gate::NothingNew
    } else if at.playing {
        Gate::Playing
    } else if at.watch_route {
        Gate::Watching
    } else {
        Gate::Open
    }
}

/// Whether the top bar shows the badge: something unseen, not turned off
/// here, and not closed with Done on this page. Later keeps it.
pub fn badge(seen: Option<Option<u32>>, off: bool, done: bool) -> bool {
    !off && !done && seen.is_some_and(|seen| !Unseen::since(seen).is_empty())
}

/// How the viewer left the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Close {
    /// Show it again next time: nothing is recorded.
    Later,
    /// Record every entry as seen.
    Done,
}

/// The settings' current values, for the pickers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Choices {
    /// Every theme, with `active` on the applied one.
    pub themes: Vec<ThemeCard>,
    /// The theme follows the system's light or dark choice.
    pub theme_auto: bool,
    pub subtitle_size: SubtitleSize,
    pub library_transition: LibraryTransition,
    pub overlay_effect: OverlayEffect,
}

/// A pick made on a setting's page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    Theme(String),
    ThemeAuto,
    SubtitleSize(SubtitleSize),
    LibraryTransition(LibraryTransition),
    OverlayEffect(OverlayEffect),
}

/// The top bar's button that opens the dialog when it did not splash, or
/// after Remind me later.
#[component]
pub fn WhatsNewBadge(flow: Flow, count: usize, on_open: EventHandler<()>) -> Element {
    let label = match (flow, count) {
        (Flow::Setup, _) => "Finish setting up Pharos".to_owned(),
        (Flow::WhatsNew, 1) => "What's new: 1 new thing".to_owned(),
        (Flow::WhatsNew, n) => format!("What's new: {n} new things"),
    };
    rsx! {
        button {
            class: "pharos-whats-new-badge",
            r#type: "button",
            title: "{label}",
            aria_label: "{label}",
            onclick: move |_| on_open.call(()),
            svg {
                class: "pharos-whats-new-icon",
                view_box: "0 0 24 24",
                width: "20",
                height: "20",
                "aria-hidden": "true",
                path {
                    fill: "currentColor",
                    d: "M12 2l2.2 6.3L20 10l-5.8 1.7L12 18l-2.2-6.3L4 10l5.8-1.7z\
                        M19 15l.9 2.1L22 18l-2.1.9L19 21l-.9-2.1L16 18l2.1-.9z",
                }
            }
            span { class: "pharos-whats-new-dot", "aria-hidden": "true" }
        }
    }
}

/// The id the dialog's first button carries, focused when it opens so a TV
/// remote and a keyboard land inside it.
pub const FIRST_BUTTON_ID: &str = "pharos-whats-new-next";

#[component]
pub fn WhatsNewView(
    unseen: Unseen,
    choices: Choices,
    /// Which page shows, 0-based; clamped to the last.
    page: usize,
    /// Bumped on each pick, so the demo replays.
    replay: u32,
    on_page: EventHandler<usize>,
    on_pick: EventHandler<Pick>,
    on_close: EventHandler<Close>,
    /// Why the last pick could not be kept on this device (FR-213).
    #[props(default)]
    not_kept: Option<String>,
) -> Element {
    let pages = unseen.pages();
    let last = pages.len().saturating_sub(1);
    let page = page.min(last);
    let shown = pages.get(page).copied().unwrap_or(Page::Notes);
    let step = format!("{} of {}", page.saturating_add(1), pages.len());
    let kicker = match unseen.flow {
        Flow::Setup => "Welcome",
        Flow::WhatsNew => "What's new",
    };
    let finish = match unseen.flow {
        Flow::Setup => "Start watching",
        Flow::WhatsNew => "Done",
    };
    super::views::use_modal_hold(".pharos-whats-new-backdrop");
    rsx! {
        div { class: "pharos-whats-new-backdrop",
            div {
                class: "pharos-whats-new",
                role: "dialog",
                aria_modal: "true",
                aria_labelledby: "pharos-whats-new-title",
                "data-flow": unseen.flow.label(),
                tabindex: "-1",
                onkeydown: move |e: KeyboardEvent| {
                    e.stop_propagation();
                    // A choice's arrows step along its group, as radios do
                    // (FR-213).
                    let step = match e.key() {
                        Key::Named(NamedKey::ArrowRight | NamedKey::ArrowDown) => Some(1),
                        Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp) => Some(-1),
                        _ => None,
                    };
                    if step.is_some_and(platform::step_radio) {
                        e.prevent_default();
                    }
                    if e.key() == Key::Named(NamedKey::Escape) {
                        on_close.call(Close::Later);
                    }
                },
                onclick: move |e: MouseEvent| e.stop_propagation(),
                header { class: "pharos-whats-new-head",
                    p { class: "pharos-whats-new-kicker", "{kicker}" }
                    if pages.len() > 1 {
                        p { class: "pharos-whats-new-step", "{step}" }
                    }
                }
                match shown {
                    Page::Welcome => rsx! {
                        section { class: "pharos-whats-new-page",
                            h2 { id: "pharos-whats-new-title", "Welcome to Pharos" }
                            p {
                                "A few quick choices to make it yours. Each one shows what it does as you pick, \
                                 and you can change any of them later in Settings."
                            }
                        }
                    },
                    Page::Setting(setting) => rsx! {
                        SettingPage { setting, choices, replay, on_pick, not_kept: not_kept.clone() }
                    },
                    Page::Notes => rsx! {
                        NotesPage { notes: unseen.notes.clone() }
                    },
                }
                footer { class: "pharos-whats-new-actions",
                    button {
                        class: "pharos-button pharos-button-secondary pharos-whats-new-later",
                        onclick: move |_| on_close.call(Close::Later),
                        "Remind me later"
                    }
                    if page > 0 {
                        button {
                            class: "pharos-button pharos-button-secondary",
                            onclick: move |_| on_page.call(page.saturating_sub(1)),
                            "Back"
                        }
                    }
                    if page < last {
                        button {
                            id: FIRST_BUTTON_ID,
                            class: "pharos-button",
                            onclick: move |_| on_page.call(page.saturating_add(1)),
                            "Next"
                        }
                    } else {
                        button {
                            id: FIRST_BUTTON_ID,
                            class: "pharos-button",
                            onclick: move |_| on_close.call(Close::Done),
                            "{finish}"
                        }
                    }
                }
            }
        }
    }
}

/// One radio-style choice button.
#[component]
fn Choice(value: String, label: String, checked: bool, on_pick: EventHandler<()>) -> Element {
    rsx! {
        button {
            class: "pharos-button pharos-button-secondary pharos-whats-new-choice",
            role: "radio",
            aria_checked: if checked { "true" } else { "false" },
            "data-choice": "{value}",
            onclick: move |_| on_pick.call(()),
            "{label}"
        }
    }
}

#[component]
fn SettingPage(
    setting: Setting,
    choices: Choices,
    replay: u32,
    on_pick: EventHandler<Pick>,
    #[props(default)] not_kept: Option<String>,
) -> Element {
    rsx! {
        section { class: "pharos-whats-new-page",
            h2 { id: "pharos-whats-new-title", "{setting.title()}" }
            p { "{setting.text()}" }
            match setting {
                Setting::Theme => rsx! {
                    div {
                        class: "pharos-whats-new-choices pharos-whats-new-themes",
                        role: "radiogroup",
                        aria_label: "Theme",
                        Choice {
                            value: "auto",
                            label: "Follow the system",
                            checked: choices.theme_auto,
                            on_pick: move |()| on_pick.call(Pick::ThemeAuto),
                        }
                        for card in choices.themes.clone() {
                            ThemeChoice {
                                key: "{card.name}",
                                checked: card.active && !choices.theme_auto,
                                card: card.clone(),
                                on_pick: move |name: String| on_pick.call(Pick::Theme(name)),
                            }
                        }
                    }
                },
                Setting::SubtitleSize => rsx! {
                    SubtitleDemo { size: choices.subtitle_size }
                    div {
                        class: "pharos-whats-new-choices",
                        role: "radiogroup",
                        aria_label: "Subtitle size",
                        for size in SubtitleSize::ALL {
                            Choice {
                                key: "{size.as_str()}",
                                value: size.as_str(),
                                label: size.label(),
                                checked: size == choices.subtitle_size,
                                on_pick: move |()| on_pick.call(Pick::SubtitleSize(size)),
                            }
                        }
                    }
                },
                Setting::LibraryTransition => rsx! {
                    TransitionDemo { fx: choices.library_transition, replay }
                    div {
                        class: "pharos-whats-new-choices",
                        role: "radiogroup",
                        aria_label: "Library transition",
                        for fx in LibraryTransition::ALL {
                            Choice {
                                key: "{fx.as_str()}",
                                value: fx.as_str(),
                                label: fx.label(),
                                checked: fx == choices.library_transition,
                                on_pick: move |()| on_pick.call(Pick::LibraryTransition(fx)),
                            }
                        }
                    }
                },
                Setting::ControlsEffect => rsx! {
                    ControlsDemo {}
                    div {
                        class: "pharos-whats-new-choices",
                        role: "radiogroup",
                        aria_label: "Controls effect",
                        for fx in OverlayEffect::ALL {
                            Choice {
                                key: "{fx.as_str()}",
                                value: fx.as_str(),
                                label: fx.label(),
                                checked: fx == choices.overlay_effect,
                                on_pick: move |()| on_pick.call(Pick::OverlayEffect(fx)),
                            }
                        }
                    }
                },
            }
            match not_kept {
                // The pick applies now, but the device would not keep it:
                // say so rather than claim it was saved.
                Some(why) => rsx! {
                    p { class: "pharos-whats-new-help pharos-error", role: "alert",
                        "Couldn't keep this on this device ({why}). It applies until the page is closed."
                    }
                },
                None => rsx! {
                    p { class: "pharos-whats-new-help",
                        "Saved on this device. You can change it later in Settings, under Appearance."
                    }
                },
            }
        }
    }
}

/// A theme as a choice: its name over its swatches.
#[component]
fn ThemeChoice(card: ThemeCard, checked: bool, on_pick: EventHandler<String>) -> Element {
    let name = card.name.clone();
    rsx! {
        button {
            class: "pharos-button pharos-button-secondary pharos-whats-new-choice pharos-whats-new-theme",
            role: "radio",
            aria_checked: if checked { "true" } else { "false" },
            "data-choice": "{card.name}",
            onclick: move |_| on_pick.call(name.clone()),
            span { class: "pharos-whats-new-swatches", "aria-hidden": "true",
                for (i, colour) in card.swatches.iter().enumerate() {
                    span { key: "{i}", style: "background: {colour}" }
                }
            }
            "{card.name}"
        }
    }
}

/// A stand-in picture with a subtitle line at `size`, drawn with the
/// player's own scale steps (V497).
#[component]
fn SubtitleDemo(size: SubtitleSize) -> Element {
    rsx! {
        div {
            class: "pharos-whats-new-subs",
            "data-subtitle-size": size.as_str(),
            aria_hidden: "true",
            span { class: "pharos-whats-new-cue", "Subtitles look like this at this size." }
        }
    }
}

/// Tiles in the transition demo: two rows of the widest grid.
const DEMO_TILES: usize = 8;

/// Columns in the transition demo's grid (the sheet's
/// `.pharos-whats-new-demo`).
const DEMO_COLUMNS: usize = 4;

/// The position variables a library's tile gets from `stage_arrival`, for
/// demo tile `at` in its fixed grid, so every effect moves as it does in a
/// library (V492): row, column and their sum for the waves, a scatter for
/// Pop and the rain, and the warp's pull from the grid's middle. The pull is
/// in percent of the tile, which `translate` reads against the tile itself;
/// a tile is 2:3, so a row is 1.5 tile widths.
fn demo_tile_style(at: usize) -> String {
    let (row, col) = (at / DEMO_COLUMNS, at % DEMO_COLUMNS);
    let rows = DEMO_TILES.div_ceil(DEMO_COLUMNS);
    let mid_col = (DEMO_COLUMNS as f64 - 1.0) / 2.0;
    let mid_row = (rows as f64 - 1.0) / 2.0;
    let (dx, dy) = (mid_col - col as f64, mid_row - row as f64);
    let corner = (mid_col + 0.5).hypot((mid_row + 0.5) * 1.5);
    let share = dx.hypot(dy * 1.5) / corner;
    format!(
        "--fx-row: {row}; --fx-col: {col}; --fx-wave: {}; --fx-r: {}; \
         --fx-dx: {:.0}%; --fx-dy: {:.0}%; --fx-d: {share:.3}",
        row + col,
        (at * 7) % 11,
        dx * 100.0,
        dy * 100.0,
    )
}

/// A row of stand-in cards that arrive with `fx`, the way a library's do.
/// The cards carry `.pharos-arrive` inside `.pharos-whats-new-demo`, which
/// the sheet's transition rules include, and the demo's own
/// `data-library-transition` picks the effect (V496). Remounted on every
/// pick, so the animation plays again.
#[component]
fn TransitionDemo(fx: LibraryTransition, replay: u32) -> Element {
    let key = format!("{}-{replay}", fx.as_str());
    let tiles = rsx! {
        ul {
            class: "pharos-whats-new-demo",
            "data-library-transition": fx.as_str(),
            aria_hidden: "true",
            for at in 0..DEMO_TILES {
                li {
                    key: "{at}",
                    class: "pharos-arrive pharos-whats-new-tile",
                    style: demo_tile_style(at),
                }
            }
        }
    };
    super::app::remount_on(&key, tiles)
}

/// How long the controls demo holds each state: long enough to see the
/// controls at rest before the next hide, and every effect is over in 300 ms.
const DEMO_HOLD_MS: u32 = 1_400;

/// A stand-in player whose top strip and control bar hide and come back on
/// a loop. It is a `.pharos-player` with the real chrome classes, so the
/// sheet's controls effect rules (V491), which follow `data-overlay-fx` on
/// `<html>`, run on it exactly as on a video (V500).
#[component]
fn ControlsDemo() -> Element {
    let mut idle = use_signal(|| false);
    use_future(move || async move {
        loop {
            platform::sleep_ms(DEMO_HOLD_MS).await;
            idle.toggle();
        }
    });
    let class = if idle() {
        "pharos-player pharos-whats-new-player is-idle"
    } else {
        "pharos-player pharos-whats-new-player"
    };
    rsx! {
        div { class, aria_hidden: "true",
            div { class: "pharos-watch-top",
                span { class: "pharos-whats-new-player-title", "Episode 1" }
            }
            div { class: "pharos-controls",
                span { class: "pharos-whats-new-player-bar" }
            }
        }
    }
}

/// The changelog page: features first, then fixes, each newest first.
#[component]
fn NotesPage(notes: Vec<Entry>) -> Element {
    let features: Vec<Entry> = notes
        .iter()
        .filter(|e| e.kind == Kind::Feature)
        .copied()
        .collect();
    let fixes: Vec<Entry> = notes
        .iter()
        .filter(|e| e.kind == Kind::Fix)
        .copied()
        .collect();
    rsx! {
        section { class: "pharos-whats-new-page",
            h2 { id: "pharos-whats-new-title", "Also new in Pharos" }
            if !features.is_empty() {
                h3 { "New" }
                ul { class: "pharos-whats-new-notes",
                    for e in features {
                        li { key: "{e.id}", strong { "{e.title}" } " {e.text}" }
                    }
                }
            }
            if !fixes.is_empty() {
                h3 { "Fixed" }
                ul { class: "pharos-whats-new-notes",
                    for e in fixes {
                        li { key: "{e.id}", strong { "{e.title}" } " {e.text}" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn entry_ids_are_unique_and_increasing() {
        for pair in ENTRIES.windows(2) {
            if let [a, b] = pair {
                assert!(a.id < b.id, "{} then {}", a.id, b.id);
            }
        }
        assert_eq!(current(), ENTRIES.last().map(|e| e.id).unwrap_or(0));
    }

    #[test]
    fn a_viewer_who_has_seen_everything_has_nothing_new() {
        assert!(Unseen::since(Some(current())).is_empty());
    }

    #[test]
    fn a_new_account_is_set_up_with_every_setting_once() {
        let unseen = Unseen::since(None);
        assert_eq!(unseen.flow, Flow::Setup);
        assert!(unseen.notes.is_empty());
        for s in SETUP {
            assert!(unseen.settings.contains(s), "{s:?}");
        }
        for e in ENTRIES {
            if let Kind::Setting(s) = e.kind {
                assert_eq!(unseen.settings.iter().filter(|x| **x == s).count(), 1);
            }
        }
        let pages = unseen.pages();
        assert_eq!(pages.first(), Some(&Page::Welcome));
        assert_eq!(pages.len(), unseen.settings.len() + 1);
    }

    #[test]
    fn an_existing_account_sees_new_settings_then_the_newest_notes() {
        let unseen = Unseen::since(Some(0));
        assert_eq!(unseen.flow, Flow::WhatsNew);
        assert_eq!(
            unseen.settings,
            vec![Setting::LibraryTransition, Setting::ControlsEffect]
        );
        assert!(!unseen.settings.contains(&Setting::Theme), "setup only");
        assert!(unseen.notes.len() <= MAX_NOTES);
        for pair in unseen.notes.windows(2) {
            if let [a, b] = pair {
                assert!(a.id > b.id, "notes are newest first");
            }
        }
        let pages = unseen.pages();
        assert_eq!(
            pages.first(),
            Some(&Page::Setting(Setting::LibraryTransition))
        );
        assert_eq!(pages.last(), Some(&Page::Notes));
    }

    #[test]
    fn only_entries_after_the_seen_id_show() {
        let unseen = Unseen::since(Some(1));
        assert_eq!(unseen.settings, vec![Setting::ControlsEffect]);
        assert!(unseen.notes.iter().all(|e| e.id > 1));
        assert!(!unseen.notes.is_empty());
    }

    #[test]
    fn a_stored_seen_value_parses_and_unreadable_counts_as_zero() {
        assert_eq!(parse_seen(Some(b"7")), Some(7));
        assert_eq!(parse_seen(Some(b" 7\n")), Some(7));
        assert_eq!(parse_seen(Some(b"soon")), Some(0));
        assert_eq!(parse_seen(None), None);
    }

    fn inputs() -> GateInputs {
        GateInputs {
            seen: Some(None),
            off: false,
            playing: false,
            watch_route: false,
        }
    }

    #[test]
    fn the_gate_waits_for_playback_and_the_watch_page() {
        assert_eq!(gate(inputs()), Gate::Open);
        assert_eq!(
            gate(GateInputs {
                seen: Some(Some(0)),
                ..inputs()
            }),
            Gate::Open
        );
        assert_eq!(
            gate(GateInputs {
                playing: true,
                ..inputs()
            }),
            Gate::Playing
        );
        assert_eq!(
            gate(GateInputs {
                watch_route: true,
                ..inputs()
            }),
            Gate::Watching
        );
        assert_eq!(
            gate(GateInputs {
                seen: None,
                ..inputs()
            }),
            Gate::Loading
        );
        assert_eq!(
            gate(GateInputs {
                off: true,
                ..inputs()
            }),
            Gate::Off
        );
        assert_eq!(
            gate(GateInputs {
                seen: Some(Some(current())),
                playing: true,
                ..inputs()
            }),
            Gate::NothingNew
        );
    }

    #[test]
    fn the_badge_shows_while_something_is_unseen_until_done() {
        assert!(badge(Some(None), false, false));
        assert!(badge(Some(Some(0)), false, false));
        assert!(!badge(None, false, false), "not before the seen id loads");
        assert!(!badge(Some(None), true, false), "off on this device");
        assert!(!badge(Some(None), false, true), "closed with Done");
        assert!(!badge(Some(Some(current())), false, false), "all seen");
    }

    #[test]
    fn gate_labels_are_distinct() {
        let all = [
            Gate::Open,
            Gate::Off,
            Gate::Loading,
            Gate::NothingNew,
            Gate::Playing,
            Gate::Watching,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|g| g.label()).collect();
        assert_eq!(labels.len(), all.len());
    }

    fn render(body: impl Fn() -> Element + 'static) -> String {
        let body: std::rc::Rc<dyn Fn() -> Element> = std::rc::Rc::new(body);
        let mut dom =
            VirtualDom::new_with_props(|body: std::rc::Rc<dyn Fn() -> Element>| body(), body);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn choices() -> Choices {
        Choices {
            themes: vec![
                ThemeCard {
                    name: "Pharos Night".to_owned(),
                    shipped: true,
                    active: true,
                    swatches: vec!["#000".to_owned()],
                },
                ThemeCard {
                    name: "Pharos Day".to_owned(),
                    shipped: true,
                    active: false,
                    swatches: vec!["#fff".to_owned()],
                },
            ],
            theme_auto: false,
            subtitle_size: SubtitleSize::Large,
            library_transition: LibraryTransition::Warp,
            overlay_effect: OverlayEffect::Burn,
        }
    }

    fn page(seen: Option<u32>, at: usize) -> String {
        render(move || {
            rsx! {
                WhatsNewView {
                    unseen: Unseen::since(seen),
                    choices: choices(),
                    page: at,
                    replay: 0,
                    on_page: |_| {},
                    on_pick: |_| {},
                    on_close: |_| {},
                }
            }
        })
    }

    /// The page index of `want` in `seen`'s walk.
    fn at(seen: Option<u32>, want: Page) -> usize {
        Unseen::since(seen)
            .pages()
            .iter()
            .position(|p| *p == want)
            .unwrap()
    }

    /// FR-213: a pick the device would not keep is not called saved.
    #[test]
    fn a_pick_that_was_not_kept_is_not_called_saved() {
        let at_theme = at(None, Page::Setting(Setting::Theme));
        let html = render(move || {
            rsx! {
                WhatsNewView {
                    unseen: Unseen::since(None),
                    choices: choices(),
                    page: at_theme,
                    replay: 0,
                    on_page: |_| {},
                    on_pick: |_| {},
                    on_close: |_| {},
                    not_kept: Some("storage is full".to_owned()),
                }
            }
        });
        assert!(
            html.contains("keep this on this device (storage is full)"),
            "{html}"
        );
        assert!(!html.contains("Saved on this device"), "{html}");
        let kept = page(None, at_theme);
        assert!(kept.contains("Saved on this device"), "{kept}");
    }

    #[test]
    fn setup_opens_with_a_welcome() {
        let html = page(None, 0);
        assert!(html.contains("Welcome to Pharos"), "{html}");
        assert!(html.contains(">Next<"), "{html}");
        assert!(!html.contains(">Back<"), "{html}");
    }

    #[test]
    fn the_theme_page_marks_the_applied_theme() {
        let html = page(None, at(None, Page::Setting(Setting::Theme)));
        assert!(html.contains("Pick a theme"), "{html}");
        assert_eq!(html.matches("aria-checked=\"true\"").count(), 1, "{html}");
        assert!(
            html.contains("aria-checked=\"true\" data-choice=\"Pharos Night\""),
            "{html}"
        );
    }

    #[test]
    fn the_subtitle_page_demos_the_current_size() {
        let html = page(None, at(None, Page::Setting(Setting::SubtitleSize)));
        assert!(html.contains("data-subtitle-size=\"large\""), "{html}");
        assert!(
            html.contains("aria-checked=\"true\" data-choice=\"large\""),
            "{html}"
        );
    }

    #[test]
    fn the_transition_page_demos_and_marks_the_current_choice() {
        let html = page(Some(0), 0);
        assert!(html.contains("Library card transitions"), "{html}");
        assert!(
            html.contains("data-library-transition=\"warp\""),
            "the demo carries the choice: {html}"
        );
        assert_eq!(html.matches("pharos-arrive").count(), DEMO_TILES, "{html}");
        assert_eq!(html.matches("aria-checked=\"true\"").count(), 1, "{html}");
        assert!(
            html.contains("aria-checked=\"true\" data-choice=\"warp\""),
            "{html}"
        );
    }

    #[test]
    fn each_demo_tile_is_placed_like_a_library_tile() {
        let first = demo_tile_style(0);
        assert!(
            first.contains("--fx-row: 0; --fx-col: 0; --fx-wave: 0;"),
            "{first}"
        );
        assert!(first.contains("--fx-dx: 150%; --fx-dy: 50%;"), "{first}");
        let last = demo_tile_style(DEMO_TILES - 1);
        assert!(
            last.contains("--fx-row: 1; --fx-col: 3; --fx-wave: 4;"),
            "{last}"
        );
        assert!(last.contains("--fx-dx: -150%; --fx-dy: -50%;"), "{last}");
        for at in 0..DEMO_TILES {
            let style = demo_tile_style(at);
            let d: f64 = style
                .rsplit("--fx-d: ")
                .next()
                .and_then(|v| v.parse().ok())
                .expect("a pull share");
            assert!((0.0..=1.0).contains(&d), "{style}");
        }
    }

    #[test]
    fn the_controls_effect_page_demos_on_a_stand_in_player() {
        let html = page(Some(0), at(Some(0), Page::Setting(Setting::ControlsEffect)));
        assert!(html.contains("Controls effect"), "{html}");
        // The sheet's controls effect rules match the demo by these classes.
        for class in [
            "pharos-player pharos-whats-new-player",
            "pharos-watch-top",
            "pharos-controls",
        ] {
            assert!(html.contains(class), "{class} missing: {html}");
        }
        assert_eq!(html.matches("aria-checked=\"true\"").count(), 1, "{html}");
        assert!(
            html.contains("aria-checked=\"true\" data-choice=\"burn\""),
            "{html}"
        );
    }

    #[test]
    fn the_last_page_lists_notes_and_offers_done() {
        let unseen = Unseen::since(Some(0));
        let html = page(Some(0), at(Some(0), Page::Notes));
        assert!(html.contains("Also new in Pharos"), "{html}");
        for e in &unseen.notes {
            assert!(html.contains(e.title), "{} missing: {html}", e.title);
        }
        assert!(html.contains(">Done<"), "{html}");
        assert!(html.contains(">Back<"), "{html}");
        assert!(html.contains("Remind me later"), "{html}");
    }

    #[test]
    fn setup_ends_with_start_watching() {
        let html = page(None, 99);
        assert!(html.contains(">Start watching<"), "{html}");
    }

    #[test]
    fn the_badge_says_what_it_opens() {
        let html = render(|| {
            rsx! {
                WhatsNewBadge { flow: Flow::WhatsNew, count: 3, on_open: |_| {} }
            }
        });
        assert!(html.contains("3 new things\""), "{html}");
        let one = render(|| {
            rsx! {
                WhatsNewBadge { flow: Flow::WhatsNew, count: 1, on_open: |_| {} }
            }
        });
        assert!(one.contains("1 new thing\""), "{one}");
        let setup = render(|| {
            rsx! {
                WhatsNewBadge { flow: Flow::Setup, count: 3, on_open: |_| {} }
            }
        });
        assert!(setup.contains("Finish setting up Pharos"), "{setup}");
    }
}
