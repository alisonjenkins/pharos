//! The stage (spec 020): one playback for the whole session, shown full on
//! the watch route and as a floating window or a mini bar everywhere else.
//!
//! The stage is the one place the player is mounted. It sits beside the
//! routed screen, not in it, so leaving the watch page changes the stage's
//! view (a class) rather than unmounting the player and its stream.

use dioxus::prelude::*;
use pharos_client::api::ItemRef;

use super::listen::{Icon, ICON_FLOAT, ICON_PIP, ICON_QUEUE_NEXT, QUEUE_PANEL_ID};
use super::platform;
use super::playback::{position_at, MediaState};
use super::route::Route;
use super::watch_view::Control;

/// The stage element, for its size while dragging.
pub const STAGE_ID: &str = "pharos-stage";
/// Where the floating window was dropped, kept for the tab (FR-006).
const SPOT_KEY: &str = "pharos.stage.spot";
/// The view used when the player is not full, kept for the tab.
const VIEW_KEY: &str = "pharos.stage.view";
/// The floating window's width once resized, kept for the tab.
const WIDTH_KEY: &str = "pharos.stage.width";
/// The floating window's drag handle.
const STRIP_CLASS: &str = "pharos-stage-strip";
/// The stage bar's Expand button, where a TV's focus lands as the player
/// goes compact (B1031).
pub const STAGE_EXPAND_ID: &str = "pharos-stage-expand";
/// Whether the D-pad's spatial focus is the only way round the page (a TV):
/// the floating window's strip is then no focus stop, as its arrows would
/// move the window rather than the focus (B1031).
const REMOTE_FOCUS: bool = cfg!(target_os = "android");
/// How far an arrow key moves the floating window, in px (FR-007).
pub const KEY_STEP_PX: f64 = 16.0;
/// The narrowest the floating window resizes to, in px: the strip's
/// buttons with picture-in-picture need about 15rem.
pub const MIN_WIDTH_PX: f64 = 240.0;
/// How a video window's height follows its width: the picture is 16:9.
const VIDEO_ASPECT: f64 = 9.0 / 16.0;

/// Whether leaving a video's full view stops it (the Android TV app).
const STOPS_VIDEO: bool = cfg!(target_os = "android");

/// How the player is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The watch page or the now-playing screen.
    Full,
    /// A small window that can be dragged anywhere on the page.
    Floating,
    /// A bar along the bottom of the shell.
    Mini,
}

impl View {
    pub fn label(self) -> &'static str {
        match self {
            View::Full => "full",
            View::Floating => "floating",
            View::Mini => "mini",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "floating" => Some(View::Floating),
            "mini" => Some(View::Mini),
            _ => None,
        }
    }
}

/// The floating window's place: how far across and down the room left
/// around it it sits, each in 0..=1. As fractions of the room, not of the
/// viewport, a resize can never push the window off the page (FR-006).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spot {
    pub x: f64,
    pub y: f64,
}

impl Default for Spot {
    /// The bottom-right corner.
    fn default() -> Self {
        Spot { x: 1.0, y: 1.0 }
    }
}

impl Spot {
    fn clamped(x: f64, y: f64) -> Self {
        let unit = |v: f64| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        Spot {
            x: unit(x),
            y: unit(y),
        }
    }

    /// The window's top-left, in px, for a viewport and window size. A
    /// window bigger than the viewport pins to the top-left.
    pub fn origin(self, viewport: (f64, f64), window: (f64, f64)) -> (f64, f64) {
        let room = |v: f64, w: f64| (v - w).max(0.0);
        (
            self.x * room(viewport.0, window.0),
            self.y * room(viewport.1, window.1),
        )
    }

    /// The spot for a window whose top-left is at `origin` px.
    pub fn at(origin: (f64, f64), viewport: (f64, f64), window: (f64, f64)) -> Self {
        let frac = |o: f64, v: f64, w: f64| {
            let room = v - w;
            if room > 0.0 {
                o / room
            } else {
                0.0
            }
        };
        Spot::clamped(
            frac(origin.0, viewport.0, window.0),
            frac(origin.1, viewport.1, window.1),
        )
    }

    /// The spot after moving the window by `delta` px from this one.
    pub fn moved(self, delta: (f64, f64), viewport: (f64, f64), window: (f64, f64)) -> Self {
        let (x, y) = self.origin(viewport, window);
        Spot::at((x + delta.0, y + delta.1), viewport, window)
    }

    fn encode(self) -> String {
        format!("{:.4},{:.4}", self.x, self.y)
    }

    fn decode(s: &str) -> Option<Self> {
        let (x, y) = s.split_once(',')?;
        Some(Spot::clamped(
            x.trim().parse().ok()?,
            y.trim().parse().ok()?,
        ))
    }
}

/// The floating window's resize handle: a top corner, over the video (the
/// strip along the bottom carries Close), on the side away from the edge
/// the window sits nearest. A resize keeps the opposite bottom corner put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grip {
    pub left: bool,
}

impl Grip {
    pub fn for_spot(spot: Spot) -> Self {
        Grip {
            left: spot.x >= 0.5,
        }
    }

    pub fn label(self) -> &'static str {
        if self.left {
            "top-left"
        } else {
            "top-right"
        }
    }
}

/// A resize of the floating window: its new width and the spot that keeps
/// the corner opposite `grip` where it was.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resized {
    pub width: f64,
    pub spot: Spot,
}

/// The floating video window `window` px at `spot`, its `grip` moved by
/// `delta` px. The height is tied to the width, so the width follows the
/// pointer along whichever axis it moved further, within [`MIN_WIDTH_PX`]
/// and the viewport's width and height.
pub fn resized(
    spot: Spot,
    grip: Grip,
    delta: (f64, f64),
    viewport: (f64, f64),
    window: (f64, f64),
) -> Resized {
    let along_x = if grip.left { -delta.0 } else { delta.0 };
    let along_y = -delta.1 / VIDEO_ASPECT;
    let grow = if along_y.abs() > along_x.abs() {
        along_y
    } else {
        along_x
    };
    let widest = viewport
        .0
        .min(window.0 + (viewport.1 - window.1) / VIDEO_ASPECT);
    let width = (window.0 + grow)
        .min(widest)
        .max(MIN_WIDTH_PX)
        .min(viewport.0);
    let height = window.1 + (width - window.0) * VIDEO_ASPECT;
    let (x, y) = spot.origin(viewport, window);
    let origin = (
        if grip.left { x + window.0 - width } else { x },
        y + window.1 - height,
    );
    Resized {
        width,
        spot: Spot::at(origin, viewport, (width, height)),
    }
}

fn decode_width(s: &str) -> Option<f64> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|w| w.is_finite() && *w >= MIN_WIDTH_PX)
}

/// What plays and how it is shown.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    /// The item playing; none when nothing is.
    pub item: Option<ItemRef>,
    pub view: View,
    /// The view to use when the player leaves full: floating or mini.
    pub compact: View,
    pub spot: Spot,
    /// The floating window's width in px once resized; none is the default.
    pub width: Option<f64>,
    /// Counts the items played, so each one mounts its own player even when
    /// an item comes round again.
    pub play: u32,
    /// The song handing over to the next one, kept mounted until the next
    /// is ready and, with a crossfade, until it has faded out.
    pub handoff: Option<Handoff>,
    /// Whether the item playing is a film or an episode, as its player
    /// reported on mounting: on the TV, leaving its full view stops it.
    pub video: bool,
}

impl Default for Stage {
    fn default() -> Self {
        Stage {
            item: None,
            view: View::Full,
            compact: View::Floating,
            spot: Spot::default(),
            width: None,
            play: 0,
            handoff: None,
            video: false,
        }
    }
}

/// A song handing the stage over to the next: its player stays mounted
/// beside the next one's, so the screen never drops to a loading state
/// between songs and the two can overlap for a crossfade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handoff {
    pub from: ItemRef,
    /// The [`Stage::play`] the song was mounted as.
    pub from_play: u32,
    /// How long the two overlap; 0 is a cut once the next is ready.
    pub fade_ms: u64,
    /// The next song's playback is negotiated: its screen shows, the
    /// handing-over one is hidden.
    pub ready: bool,
}

impl Stage {
    /// The stage as this tab left it: where the window was and which compact
    /// view it used. Nothing plays after a reload.
    pub fn restore() -> Self {
        let read = |key: &str| platform::session_get(key).unwrap_or_default();
        Stage {
            compact: read(VIEW_KEY)
                .as_deref()
                .and_then(View::parse)
                .unwrap_or(View::Floating),
            spot: read(SPOT_KEY)
                .as_deref()
                .and_then(Spot::decode)
                .unwrap_or_default(),
            width: read(WIDTH_KEY).as_deref().and_then(decode_width),
            ..Stage::default()
        }
        .docked_on_tv()
    }

    /// The Android TV app's compact player is always the bar along the
    /// bottom: a remote cannot drag the floating window off what it covers.
    fn docked_on_tv(mut self) -> Self {
        if cfg!(target_os = "android") {
            self.compact = View::Mini;
        }
        self
    }

    /// Whether something plays outside the full view.
    pub fn compact_shown(&self) -> bool {
        self.item.is_some() && self.view != View::Full
    }

    /// Follow a route change: the watch route shows its item full; leaving
    /// it shrinks the player to the compact view. Returns the view change,
    /// if any, for the log.
    pub fn follow(&mut self, route: &Route) -> Option<View> {
        let before = (self.item, self.view);
        match route {
            Route::Watch(id) => {
                self.set_item(*id);
                self.view = View::Full;
            }
            // The TV stops a video there, as TV apps do: a remote cannot
            // watch one in the bar and use the page at once (Alison, 2026-11-08).
            _ if self.item.is_some() && self.view == View::Full && self.video && STOPS_VIDEO => {
                self.close()
            }
            _ if self.item.is_some() && self.view == View::Full => self.view = self.compact,
            _ => {}
        }
        (before != (self.item, self.view)).then_some(self.view)
    }

    /// Signed in again: sign-in closed the player, so a watch route that
    /// never changed puts its item back. A player already on stage, or any
    /// other route, is left alone.
    pub fn resume(&mut self, route: &Route) -> Option<View> {
        if self.item.is_some() || !matches!(route, Route::Watch(_)) {
            return None;
        }
        self.follow(route)
    }

    /// Whether a navigation to `to` is a play the stage takes in place: a
    /// watch route while the player is compact (FR-001). The route stays
    /// where it is and the stage plays `to`'s item in its current view.
    pub fn takes(&mut self, to: &Route) -> bool {
        let Route::Watch(id) = to else {
            return false;
        };
        if !self.compact_shown() {
            return false;
        }
        self.set_item(*id);
        true
    }

    /// Play `id`. Another item is a new play; a handoff only survives as the
    /// item it was made for leaves.
    fn set_item(&mut self, id: ItemRef) {
        if self.item == Some(id) {
            // Nothing leaves, so nothing hands over.
            if self.handoff.is_some_and(|h| h.from_play == self.play) {
                self.handoff = None;
            }
            return;
        }
        if self.handoff.is_some_and(|h| h.from_play != self.play) {
            // An older handoff gives way to this one.
            self.handoff = None;
        }
        self.item = Some(id);
        self.play = self.play.wrapping_add(1);
    }

    /// The key the playing item's player mounts under: one per play.
    pub fn key(&self) -> Option<String> {
        self.item.map(|id| format!("{id}:{}", self.play))
    }

    /// Mark the playing song (`from`) as handing over to whatever plays next,
    /// overlapping it by `fade_ms`. Ignored unless `from` is playing.
    pub fn hand_off(&mut self, from: ItemRef, fade_ms: u64) {
        if self.item != Some(from) {
            return;
        }
        self.handoff = Some(Handoff {
            from,
            from_play: self.play,
            fade_ms,
            ready: false,
        });
    }

    /// The song handing over, once the next one plays; with its key.
    pub fn outgoing(&self) -> Option<(Handoff, String)> {
        self.handoff
            .filter(|h| h.from_play != self.play)
            .map(|h| (h, format!("{}:{}", h.from, h.from_play)))
    }

    /// The next song's playback is negotiated (`play` is its play): show it.
    /// Without a fade the handing-over song goes at once.
    pub fn ready(&mut self, play: u32) {
        if play != self.play {
            return;
        }
        match self.outgoing() {
            Some((h, _)) if h.fade_ms == 0 => self.handoff = None,
            Some(_) => {
                if let Some(h) = self.handoff.as_mut() {
                    h.ready = true;
                }
            }
            None => {}
        }
    }

    /// The handing-over song mounted as `from_play` has faded out.
    pub fn faded(&mut self, from_play: u32) {
        if self.handoff.is_some_and(|h| h.from_play == from_play) {
            self.handoff = None;
        }
    }

    /// Show the player as `view`. A compact view is remembered for next time.
    pub fn show(&mut self, view: View) {
        self.view = view;
        if view != View::Full {
            self.compact = view;
            store(VIEW_KEY, view.label());
        }
    }

    /// Put the floating window at `spot`.
    pub fn place(&mut self, spot: Spot) {
        self.spot = spot;
    }

    /// Remember the floating window's spot for the tab: at the end of a
    /// drag, not on every move.
    pub fn keep_spot(&self) {
        store(SPOT_KEY, &self.spot.encode());
    }

    /// Resize the floating window.
    pub fn resize(&mut self, to: Resized) {
        self.width = Some(to.width);
        self.spot = to.spot;
    }

    /// Remember the floating window's size and spot for the tab, at the end
    /// of a resize.
    pub fn keep_size(&self) {
        if let Some(w) = self.width {
            store(WIDTH_KEY, &format!("{w:.0}"));
        }
        self.keep_spot();
    }

    /// Stop showing anything.
    pub fn close(&mut self) {
        self.video = false;
        self.item = None;
        self.view = View::Full;
        self.handoff = None;
    }
}

fn store(key: &str, value: &str) {
    if let Err(e) = platform::session_set(key, value) {
        platform::warn(&format!("pharos: stage {key} not stored error={e}"));
    }
}

/// The app's stage, provided by the page root.
#[derive(Clone, Copy)]
pub struct StageCtx(pub Signal<Stage>);

/// What the stage's controls do, beyond the player's own [`Control`]s.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StageAction {
    /// One of the player's controls: play/pause, previous, next.
    Player(Control),
    /// Show the player as this view.
    Show(View),
    /// Stop and close the player.
    Close,
    /// Move the floating window to this spot.
    Place(Spot),
    /// Remember where the floating window is (a drag or key move ended).
    Keep,
    /// Dock or undock the queue beside the compact player (B900/V852).
    Queue,
}

/// Whether the queue is docked beside the compact player, shared by every
/// player on the stage so a song handing over keeps it (B900/V852).
#[derive(Clone, Copy)]
pub struct QueueDock(pub Signal<bool>);

/// What the stage logs when the docked queue opens or closes.
pub fn dock_line(open: bool, view: View, upcoming: usize) -> String {
    format!(
        "pharos: stage queue shown={open} view={} upcoming={upcoming}",
        view.label()
    )
}

/// The compact player's queue button: how many songs wait, and whether the
/// docked queue is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueButton {
    pub count: usize,
    pub open: bool,
}

/// A drag in progress: where the pointer and the window started.
#[derive(Debug, Clone, Copy)]
struct Drag {
    pointer: (f64, f64),
    from: Spot,
}

/// Whether the strip is a focus stop whose arrows move the window: the
/// floating window's, unless the arrows are a TV's only way between
/// controls (B1031).
fn strip_movable(floating: bool, remote_focus: bool) -> bool {
    floating && !remote_focus
}

/// Whether the floating window shows its resize grip: not on a TV, where
/// the grip would be one more focus stop over the picture whose arrows
/// resize the window rather than move the focus (B1031).
pub fn grip_shown(floating: bool) -> bool {
    floating && !REMOTE_FOCUS
}

/// The compact player's chrome: the title strip (the drag handle of the
/// floating window), the transport and a thin progress line. Drawn by the
/// player over its own media element, which stays where it is.
#[component]
pub fn StageBar(
    view: View,
    title: String,
    image: Option<String>,
    media: MediaState,
    runtime_ms: u64,
    /// Offer the browser's picture-in-picture window (a video, where the
    /// browser has one).
    pip: bool,
    previous: Option<ItemRef>,
    next: Option<ItemRef>,
    error: Option<String>,
    on_action: EventHandler<StageAction>,
    /// The position as a signal, when the player keeps it apart from `media`
    /// (B815).
    #[props(default)]
    live_position: Option<Signal<u64>>,
    /// Music's queue button (B900/V852); none for a video.
    #[props(default)]
    queue: Option<QueueButton>,
    /// Whole seconds until the next video plays, while "Up next" counts
    /// down: one line with Cancel in place of the card (038 FR-406).
    #[props(default)]
    up_next_in: Option<u64>,
) -> Element {
    let media = MediaState {
        position_ms: live_position.map_or(media.position_ms, |s| s()),
        ..media
    };
    let mut drag = use_signal(|| None::<Drag>);
    let StageCtx(stage) = use_context::<StageCtx>();
    let spot = stage.read().spot;
    let total = match media.duration_ms {
        0 => runtime_ms,
        d => d,
    };
    #[allow(clippy::cast_precision_loss)]
    let played = if total > 0 {
        (media.position_ms as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let floating = view == View::Floating;
    let room = || (platform::viewport(), platform::element_size(STAGE_ID));
    let place = move |delta: (f64, f64), from: Spot| {
        let (viewport, window) = room();
        from.moved(delta, viewport, window)
    };
    let on_strip_key = move |evt: KeyboardEvent| {
        // Only on the strip itself; a focused button keeps its own keys.
        if !strip_movable(floating, REMOTE_FOCUS) || !platform::key_on_itself(&evt, STRIP_CLASS) {
            return;
        }
        let delta = match evt.key() {
            Key::Named(NamedKey::ArrowLeft) => (-KEY_STEP_PX, 0.0),
            Key::Named(NamedKey::ArrowRight) => (KEY_STEP_PX, 0.0),
            Key::Named(NamedKey::ArrowUp) => (0.0, -KEY_STEP_PX),
            Key::Named(NamedKey::ArrowDown) => (0.0, KEY_STEP_PX),
            _ => return,
        };
        evt.prevent_default();
        on_action.call(StageAction::Place(place(delta, spot)));
        on_action.call(StageAction::Keep);
    };
    let label = if media.paused { "Play" } else { "Pause" };
    rsx! {
        div {
            class: "pharos-stage-bar",
            "data-view": view.label(),
            div {
                class: STRIP_CLASS,
                tabindex: if strip_movable(floating, REMOTE_FOCUS) { "0" } else { "-1" },
                aria_label: if strip_movable(floating, REMOTE_FOCUS) { "Move the player with the arrow keys" },
                onkeydown: on_strip_key,
                onpointerdown: move |evt: PointerEvent| {
                    // The strip's buttons are not handles.
                    if !floating || platform::on_a_button(&evt) {
                        return;
                    }
                    let at = evt.client_coordinates();
                    platform::capture_pointer(&evt);
                    drag.set(Some(Drag { pointer: (at.x, at.y), from: spot }));
                },
                onpointermove: move |evt: PointerEvent| {
                    let Some(d) = *drag.peek() else {
                        return;
                    };
                    let at = evt.client_coordinates();
                    let delta = (at.x - d.pointer.0, at.y - d.pointer.1);
                    on_action.call(StageAction::Place(place(delta, d.from)));
                },
                onpointerup: move |_| {
                    if drag.peek().is_some() {
                        drag.set(None);
                        on_action.call(StageAction::Keep);
                        platform::info(&format!(
                            "pharos: stage spot x={:.3} y={:.3}",
                            spot.x, spot.y
                        ));
                    }
                },
                onpointercancel: move |_| drag.set(None),
                if let Some(src) = image {
                    img { class: "pharos-stage-art", src: platform::art_src(&src), alt: "" }
                }
                div { class: "pharos-stage-text",
                    p { class: "pharos-stage-title", title: "{title}", "{title}" }
                    if let Some(message) = error {
                        p { class: "pharos-stage-error", role: "alert", "{message}" }
                    } else if let Some(seconds) = up_next_in {
                        p { class: "pharos-stage-next",
                            span { role: "timer", "Next in {seconds} s" }
                            " · "
                            button {
                                class: "pharos-stage-next-cancel",
                                r#type: "button",
                                aria_label: "Cancel up next",
                                onclick: move |_| on_action.call(StageAction::Player(Control::CancelNext)),
                                "Cancel"
                            }
                        }
                    }
                }
                div { class: "pharos-stage-transport", role: "group", aria_label: "Transport",
                    button {
                        class: "pharos-stage-button",
                        r#type: "button",
                        aria_label: "Previous",
                        disabled: previous.is_none(),
                        onclick: move |_| {
                            if let Some(id) = previous {
                                on_action.call(StageAction::Player(Control::Play(id)));
                            }
                        },
                        Icon { path: ICON_PREVIOUS }
                    }
                    button {
                        class: "pharos-stage-button pharos-stage-play",
                        r#type: "button",
                        aria_label: label,
                        onclick: move |_| on_action.call(StageAction::Player(Control::TogglePlay)),
                        Icon { path: if media.paused { ICON_PLAY } else { ICON_PAUSE } }
                    }
                    button {
                        class: "pharos-stage-button",
                        r#type: "button",
                        aria_label: "Next",
                        disabled: next.is_none(),
                        onclick: move |_| {
                            if let Some(id) = next {
                                on_action.call(StageAction::Player(Control::Play(id)));
                            }
                        },
                        Icon { path: ICON_NEXT }
                    }
                }
                if let Some(q) = queue {
                    div { class: "pharos-stage-extra",
                        button {
                            class: "pharos-stage-button pharos-stage-queue",
                            r#type: "button",
                            "data-key-action": "queue",
                            aria_label: queue_label(q.count),
                            title: "Show or hide what plays next",
                            aria_pressed: if q.open { "true" } else { "false" },
                            aria_controls: q.open.then_some(QUEUE_PANEL_ID),
                            onclick: move |_| on_action.call(StageAction::Queue),
                            Icon { path: ICON_QUEUE_NEXT }
                            if q.count > 0 {
                                span { class: "pharos-queue-count", aria_hidden: "true", "{q.count}" }
                            }
                        }
                    }
                }
                if pip {
                    div { class: "pharos-stage-extra",
                        button {
                            class: "pharos-stage-button",
                            r#type: "button",
                            aria_label: "Picture in picture",
                            title: "Picture in picture",
                            onclick: move |_| on_action.call(StageAction::Player(Control::PictureInPicture)),
                            Icon { path: ICON_PIP }
                        }
                    }
                }
                div { class: "pharos-stage-views", role: "group", aria_label: "Size",
                    button {
                        class: "pharos-stage-button",
                        r#type: "button",
                        id: STAGE_EXPAND_ID,
                        aria_label: "Expand",
                        "data-key-action": "full-player",
                        title: "Expand to the full screen",
                        onclick: move |_| on_action.call(StageAction::Show(View::Full)),
                        Icon { path: ICON_FULL }
                    }
                    if floating {
                        button {
                            class: "pharos-stage-button",
                            r#type: "button",
                            aria_label: "Mini bar",
                            "data-key-action": "player-size",
                            title: "Shrink to a bar along the bottom",
                            onclick: move |_| on_action.call(StageAction::Show(View::Mini)),
                            Icon { path: ICON_MINI }
                        }
                    } else {
                        button {
                            class: "pharos-stage-button",
                            r#type: "button",
                            aria_label: "Floating window",
                            "data-key-action": "player-size",
                            title: "Float in a window you can move",
                            onclick: move |_| on_action.call(StageAction::Show(View::Floating)),
                            Icon { path: ICON_FLOAT }
                        }
                    }
                    button {
                        class: "pharos-stage-button",
                        r#type: "button",
                        aria_label: "Close",
                        title: "Stop and close",
                        onclick: move |_| on_action.call(StageAction::Close),
                        Icon { path: ICON_CLOSE }
                    }
                }
            }
            // A click or tap seeks there (T038-306). Hidden from a screen
            // reader, which has the full player's seek slider.
            div {
                id: STAGE_PROGRESS_ID,
                class: "pharos-stage-progress",
                aria_hidden: "true",
                onclick: move |evt: MouseEvent| {
                    let width = platform::element_width(STAGE_PROGRESS_ID);
                    if let Some(ms) = position_at(evt.element_coordinates().x, width, total) {
                        platform::info(&format!(
                            "pharos: stage progress seek from {} ms to {ms} ms",
                            media.position_ms
                        ));
                        on_action.call(StageAction::Player(Control::Seek(ms)));
                    }
                },
                span { style: "width: {played:.2}%" }
            }
        }
    }
}

/// The queue button's name, with how many songs wait so a screen reader
/// hears the count the badge shows.
pub fn queue_label(count: usize) -> String {
    match count {
        0 => "Queue".to_owned(),
        1 => "Queue, 1 song".to_owned(),
        n => format!("Queue, {n} songs"),
    }
}

/// The compact player's progress line, which seeks on a click (T038-306).
const STAGE_PROGRESS_ID: &str = "pharos-stage-progress";
pub(super) const ICON_PLAY: &str = "M8 5v14l11-7L8 5Z";
pub(super) const ICON_PAUSE: &str = "M7 5h3.5v14H7zM13.5 5H17v14h-3.5z";
pub(super) const ICON_PREVIOUS: &str = "M6 6h2v12H6V6Zm3.5 6 8.5 6V6l-8.5 6Z";
pub(super) const ICON_NEXT: &str = "M16 6h2v12h-2V6ZM6 18l8.5-6L6 6v12Z";
/// Corners pointing out.
pub const ICON_FULL: &str =
    "M5 5h5v2H7v3H5V5Zm9 0h5v5h-2V7h-3V5ZM5 14h2v3h3v2H5v-5Zm12 3v-3h2v5h-5v-2h3Z";
/// A bar along the bottom of a window.
const ICON_MINI: &str = "M3 5h18v14H3V5Zm2 2v7h14V7H5Zm0 9v1h14v-1H5Z";
/// Close for a compact window whose player has not loaded, or failed to:
/// the stage bar that carries Close comes only with a loaded player.
#[component]
pub fn StageClose(item: ItemRef) -> Element {
    let StageCtx(mut stage) = use_context::<StageCtx>();
    rsx! {
        button {
            class: "pharos-stage-button pharos-stage-close",
            r#type: "button",
            aria_label: "Close",
            title: "Stop and close",
            onclick: move |_| {
                stage.write().close();
                platform::info(&format!(
                    "pharos: stage view=closed item={item} reason=close"
                ));
            },
            Icon { path: ICON_CLOSE }
        }
    }
}

/// A resize in progress: where the pointer and the window started.
#[derive(Debug, Clone, Copy)]
struct Resizing {
    pointer: (f64, f64),
    spot: Spot,
    grip: Grip,
    window: (f64, f64),
}

/// The floating window's resize handle, on the corner [`Grip::for_spot`]
/// picks. Drag it, or focus it and use the arrow keys. Shown for a video
/// only: a song's window is the strip alone.
#[component]
pub fn StageGrip() -> Element {
    let StageCtx(mut stage) = use_context::<StageCtx>();
    let mut resizing = use_signal(|| None::<Resizing>);
    // Mid-resize the grip stays on its corner, though the spot moves.
    let grip = resizing
        .read()
        .map_or_else(|| Grip::for_spot(stage.read().spot), |r| r.grip);
    let start = move |pointer: (f64, f64)| {
        let spot = stage.peek().spot;
        Resizing {
            pointer,
            spot,
            grip: Grip::for_spot(spot),
            window: platform::element_size(STAGE_ID),
        }
    };
    let to = move |r: Resizing, delta: (f64, f64)| {
        resized(r.spot, r.grip, delta, platform::viewport(), r.window)
    };
    let log = move || {
        let s = stage.peek();
        platform::info(&format!(
            "pharos: stage resize width={:.0} x={:.3} y={:.3}",
            s.width.unwrap_or_default(),
            s.spot.x,
            s.spot.y
        ));
    };
    rsx! {
        button {
            class: "pharos-stage-grip",
            r#type: "button",
            "data-grip": grip.label(),
            aria_label: "Resize the player with the arrow keys",
            title: "Drag to resize",
            onkeydown: move |evt: KeyboardEvent| {
                // Every arrow changes the width by one step; up and down
                // move the handle by the height that step gives.
                let rise = KEY_STEP_PX * VIDEO_ASPECT;
                let delta = match evt.key() {
                    Key::Named(NamedKey::ArrowLeft) => (-KEY_STEP_PX, 0.0),
                    Key::Named(NamedKey::ArrowRight) => (KEY_STEP_PX, 0.0),
                    Key::Named(NamedKey::ArrowUp) => (0.0, -rise),
                    Key::Named(NamedKey::ArrowDown) => (0.0, rise),
                    _ => return,
                };
                evt.prevent_default();
                let r = to(start((0.0, 0.0)), delta);
                stage.write().resize(r);
                stage.peek().keep_size();
                log();
            },
            onpointerdown: move |evt: PointerEvent| {
                let at = evt.client_coordinates();
                platform::capture_pointer(&evt);
                resizing.set(Some(start((at.x, at.y))));
            },
            onpointermove: move |evt: PointerEvent| {
                let Some(r) = *resizing.peek() else {
                    return;
                };
                let at = evt.client_coordinates();
                stage.write().resize(to(r, (at.x - r.pointer.0, at.y - r.pointer.1)));
            },
            onpointerup: move |_| {
                if resizing.peek().is_some() {
                    resizing.set(None);
                    stage.peek().keep_size();
                    log();
                }
            },
            onpointercancel: move |_| {
                if resizing.peek().is_some() {
                    resizing.set(None);
                    stage.peek().keep_size();
                }
            },
        }
    }
}

const ICON_CLOSE: &str = "M19 6.4 17.6 5 12 10.6 6.4 5 5 6.4 10.6 12 5 17.6 6.4 19 12 13.4 17.6 19 19 17.6 13.4 12 19 6.4Z";

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: (f64, f64) = (1000.0, 800.0);
    const WINDOW: (f64, f64) = (320.0, 220.0);

    #[test]
    fn the_grip_sits_on_the_top_corner_away_from_the_nearest_edge() {
        assert_eq!(Grip::for_spot(Spot::default()).label(), "top-left");
        assert_eq!(Grip::for_spot(Spot { x: 0.2, y: 0.9 }).label(), "top-right");
    }

    #[test]
    fn a_resize_keeps_the_opposite_corner_in_place() {
        for spot in [
            Spot::default(),
            Spot { x: 0.0, y: 1.0 },
            Spot { x: 0.3, y: 0.7 },
        ] {
            let grip = Grip::for_spot(spot);
            let out = |v: f64, toward_start: bool| if toward_start { -v } else { v };
            let delta = (out(80.0, grip.left), 0.0);
            let r = resized(spot, grip, delta, VIEWPORT, WINDOW);
            assert!((r.width - 400.0).abs() < 1e-9, "{r:?}");
            let height = WINDOW.1 + 80.0 * VIDEO_ASPECT;
            let (x0, y0) = spot.origin(VIEWPORT, WINDOW);
            let (x1, y1) = r.spot.origin(VIEWPORT, (r.width, height));
            let far = |a: f64, size: f64, at_start: bool| if at_start { a + size } else { a };
            assert!(
                (far(x0, WINDOW.0, grip.left) - far(x1, r.width, grip.left)).abs() < 1e-6,
                "{spot:?} x"
            );
            assert!(
                (far(y0, WINDOW.1, true) - far(y1, height, true)).abs() < 1e-6,
                "{spot:?} y"
            );
        }
    }

    #[test]
    fn a_vertical_drag_resizes_through_the_aspect() {
        let grip = Grip::for_spot(Spot::default());
        let r = resized(Spot::default(), grip, (0.0, -90.0), VIEWPORT, WINDOW);
        assert!((r.width - 480.0).abs() < 1e-9, "{r:?}");
    }

    #[test]
    fn a_resize_stays_between_the_minimum_and_the_viewport() {
        let grip = Grip::for_spot(Spot::default());
        let small = resized(Spot::default(), grip, (5000.0, 0.0), VIEWPORT, WINDOW);
        assert!((small.width - MIN_WIDTH_PX).abs() < 1e-9, "{small:?}");
        let big = resized(Spot::default(), grip, (-5000.0, 0.0), VIEWPORT, WINDOW);
        assert!((big.width - VIEWPORT.0).abs() < 1e-9, "{big:?}");
        // On a wide, short viewport the height reaches it first.
        let short = (1600.0, 500.0);
        let big = resized(Spot::default(), grip, (-5000.0, 0.0), short, WINDOW);
        let height = WINDOW.1 + (big.width - WINDOW.0) * VIDEO_ASPECT;
        assert!((height - short.1).abs() < 1e-6, "{big:?}");
    }

    #[test]
    fn a_stored_width_below_the_minimum_or_garbage_is_dropped() {
        assert_eq!(decode_width("480"), Some(480.0));
        assert_eq!(decode_width("12"), None);
        assert_eq!(decode_width("NaN"), None);
        assert_eq!(decode_width("wide"), None);
    }

    #[test]
    fn the_default_spot_is_the_bottom_right_corner() {
        assert_eq!(Spot::default().origin(VIEWPORT, WINDOW), (680.0, 580.0));
    }

    #[test]
    fn a_drag_moves_the_window_by_the_pointer() {
        let spot = Spot { x: 0.5, y: 0.5 }.moved((-100.0, 50.0), VIEWPORT, WINDOW);
        let (x, y) = spot.origin(VIEWPORT, WINDOW);
        assert!((x - 240.0).abs() < 1e-9, "{x}");
        assert!((y - 340.0).abs() < 1e-9, "{y}");
    }

    #[test]
    fn a_drag_past_an_edge_stops_at_it() {
        let spot = Spot::default().moved((5000.0, -5000.0), VIEWPORT, WINDOW);
        assert_eq!(spot, Spot { x: 1.0, y: 0.0 });
    }

    #[test]
    fn a_smaller_viewport_keeps_the_window_wholly_in_view() {
        for x in [0.0, 0.3, 1.0] {
            for y in [0.0, 0.6, 1.0] {
                let (ox, oy) = Spot { x, y }.origin((400.0, 600.0), WINDOW);
                assert!(ox >= 0.0 && ox + WINDOW.0 <= 400.0, "x {ox}");
                assert!(oy >= 0.0 && oy + WINDOW.1 <= 600.0, "y {oy}");
            }
        }
    }

    #[test]
    fn a_window_bigger_than_the_viewport_pins_to_the_top_left() {
        let spot = Spot::at((50.0, 50.0), (300.0, 200.0), WINDOW);
        assert_eq!(spot.origin((300.0, 200.0), WINDOW), (0.0, 0.0));
    }

    #[test]
    fn a_stored_spot_round_trips_and_bad_ones_are_dropped() {
        let spot = Spot { x: 0.25, y: 0.75 };
        assert_eq!(Spot::decode(&spot.encode()), Some(spot));
        assert_eq!(Spot::decode("2,-1"), Some(Spot { x: 1.0, y: 0.0 }));
        assert_eq!(Spot::decode("nope"), None);
        assert_eq!(Spot::decode("NaN,0.5"), Some(Spot { x: 0.0, y: 0.5 }));
    }

    fn playing_full(id: u64) -> Stage {
        let mut stage = Stage::default();
        stage.follow(&Route::Watch(ItemRef::Real(id)));
        stage
    }

    #[test]
    fn the_watch_route_shows_its_item_full() {
        let stage = playing_full(7);
        assert_eq!(stage.item, Some(ItemRef::Real(7)));
        assert_eq!(stage.view, View::Full);
    }

    #[test]
    fn leaving_the_watch_route_shrinks_the_player_to_the_compact_view() {
        let mut stage = playing_full(7);
        assert_eq!(stage.follow(&Route::Home), Some(View::Floating));
        assert_eq!(stage.item, Some(ItemRef::Real(7)));
        stage.show(View::Mini);
        stage.follow(&Route::Watch(ItemRef::Real(7)));
        assert_eq!(stage.follow(&Route::Find), Some(View::Mini));
    }

    #[test]
    fn going_to_manage_keeps_the_player_compact() {
        // Manage is a screen of this app now (B921), not another app.
        let mut stage = playing_full(7);
        assert_eq!(stage.follow(&Route::Manage), Some(stage.compact));
        assert_eq!(stage.item, Some(ItemRef::Real(7)));
    }

    #[test]
    fn a_route_change_with_nothing_playing_changes_nothing() {
        let mut stage = Stage::default();
        assert_eq!(stage.follow(&Route::Home), None);
        assert_eq!(stage, Stage::default());
    }

    #[test]
    fn signing_in_again_on_the_watch_route_puts_its_item_back() {
        let mut stage = playing_full(7);
        stage.close();
        assert_eq!(
            stage.resume(&Route::Watch(ItemRef::Real(7))),
            Some(View::Full)
        );
        assert_eq!(stage.item, Some(ItemRef::Real(7)));
        assert_eq!(
            stage.resume(&Route::Watch(ItemRef::Real(8))),
            None,
            "one on stage stays"
        );
        stage.close();
        assert_eq!(stage.resume(&Route::Home), None);
        assert_eq!(stage.item, None);
    }

    #[test]
    fn a_compact_player_takes_a_watch_navigation_in_place() {
        let mut stage = playing_full(7);
        stage.follow(&Route::Home);
        assert!(stage.takes(&Route::Watch(ItemRef::Real(8))));
        assert_eq!(stage.item, Some(ItemRef::Real(8)));
        assert_eq!(stage.view, View::Floating);
        assert!(!stage.takes(&Route::Home));
    }

    #[test]
    fn a_full_or_empty_stage_lets_a_watch_navigation_through() {
        let mut stage = playing_full(7);
        assert!(!stage.takes(&Route::Watch(ItemRef::Real(8))));
        stage.close();
        assert!(!stage.takes(&Route::Watch(ItemRef::Real(8))));
        assert_eq!(stage.item, None);
    }

    #[test]
    fn a_song_handing_over_stays_until_the_next_is_ready() {
        let mut stage = playing_full(7);
        let first = stage.key();
        stage.hand_off(ItemRef::Real(7), 0);
        assert_eq!(stage.outgoing(), None, "nothing has taken over yet");
        stage.follow(&Route::Watch(ItemRef::Real(8)));
        assert_eq!(
            stage.outgoing().map(|(h, key)| (h.from, key)),
            first.map(|key| (ItemRef::Real(7), key)),
            "7 hands over"
        );
        assert_ne!(stage.key(), stage.outgoing().map(|(_, k)| k));
        stage.ready(stage.play);
        assert_eq!(stage.handoff, None, "a cut drops it once 8 is ready");
    }

    #[test]
    fn a_crossfading_song_stays_until_it_has_faded() {
        let mut stage = playing_full(7);
        stage.hand_off(ItemRef::Real(7), 4000);
        stage.follow(&Route::Watch(ItemRef::Real(8)));
        let Some(from_play) = stage.outgoing().map(|(h, _)| h.from_play) else {
            panic!("7 hands over to 8");
        };
        stage.ready(stage.play.wrapping_sub(1));
        assert!(
            !stage.handoff.is_some_and(|h| h.ready),
            "an older play is not ready"
        );
        stage.ready(stage.play);
        assert!(stage.outgoing().is_some_and(|(h, _)| h.ready));
        stage.faded(from_play);
        assert_eq!(stage.handoff, None);
    }

    #[test]
    fn a_handoff_to_the_same_item_or_a_close_is_dropped() {
        let mut stage = playing_full(7);
        stage.hand_off(ItemRef::Real(7), 0);
        stage.follow(&Route::Watch(ItemRef::Real(7)));
        assert_eq!(stage.handoff, None);
        stage.hand_off(ItemRef::Real(9), 0);
        assert_eq!(stage.handoff, None, "9 is not playing");
        stage.hand_off(ItemRef::Real(7), 0);
        stage.follow(&Route::Watch(ItemRef::Real(8)));
        stage.close();
        assert_eq!(stage.outgoing(), None);
    }

    #[test]
    fn an_item_that_comes_round_again_is_a_new_play() {
        let mut stage = playing_full(7);
        let first = stage.key();
        stage.follow(&Route::Watch(ItemRef::Real(8)));
        stage.follow(&Route::Watch(ItemRef::Real(7)));
        assert_ne!(stage.key(), first);
        let again = stage.key();
        stage.follow(&Route::Watch(ItemRef::Real(7)));
        assert_eq!(stage.key(), again, "the same route mounts nothing new");
    }

    /// 038 FR-406, V1222: the compact player's Up next is one line in its
    /// bar, with Cancel. Disarm check: without the line this fails.
    #[test]
    fn the_compact_bar_counts_down_to_the_next_video_in_one_line() {
        let mut dom = VirtualDom::new_with_props(
            move |()| {
                use_context_provider(|| StageCtx(Signal::new(Stage::default())));
                rsx! {
                    StageBar {
                        view: View::Floating,
                        title: "An episode".to_owned(),
                        image: None,
                        media: MediaState::default(),
                        runtime_ms: 1_320_000,
                        pip: false,
                        previous: None,
                        next: None,
                        error: None,
                        on_action: |_| {},
                        up_next_in: Some(12),
                    }
                }
            },
            (),
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Next in 12 s"), "{html}");
        assert!(html.contains("Cancel"), "{html}");
        assert!(!bar(None).contains("pharos-stage-next"));
    }

    fn bar(queue: Option<QueueButton>) -> String {
        let mut dom = VirtualDom::new_with_props(
            move |()| {
                use_context_provider(|| StageCtx(Signal::new(Stage::default())));
                rsx! {
                    StageBar {
                        view: View::Mini,
                        title: "Teardrop".to_owned(),
                        image: None,
                        media: MediaState::default(),
                        runtime_ms: 330_000,
                        pip: false,
                        previous: None,
                        next: None,
                        error: None,
                        on_action: |_| {},
                        queue,
                    }
                }
            },
            (),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// T038-306: the compact player's progress line is a seek target: it
    /// has an id to measure, and its fill lets the pointer through so the
    /// click lands on the line and its offset is the line's.
    #[test]
    fn the_small_players_progress_line_is_a_seek_target() {
        let html = bar(None);
        assert!(html.contains(r#"id="pharos-stage-progress""#), "{html}");
        let sheet = include_str!("../../assets/pharos.css");
        let fill = sheet
            .split("\n.pharos-stage-progress span {")
            .nth(1)
            .and_then(|r| r.split('}').next())
            .unwrap_or_default();
        assert!(fill.contains("pointer-events: none;"), "{fill}");
    }

    /// B900/V852: music's mini bar has a queue button that counts what
    /// waits and says whether the docked queue is open; a video's has none.
    #[test]
    fn the_mini_bar_offers_the_queue_for_music_only() {
        assert!(!bar(None).contains("pharos-stage-queue"));
        let closed = bar(Some(QueueButton {
            count: 3,
            open: false,
        }));
        assert!(
            closed.contains(r#"aria-label="Queue, 3 songs""#),
            "{closed}"
        );
        assert!(closed.contains(r#"aria-pressed="false""#), "{closed}");
        assert!(
            !closed.contains("aria-controls"),
            "no panel to point at: {closed}"
        );
        let open = bar(Some(QueueButton {
            count: 0,
            open: true,
        }));
        assert!(open.contains(r#"aria-label="Queue""#), "{open}");
        assert!(open.contains(r#"aria-controls="pharos-queue""#), "{open}");
        assert!(
            !open.contains("pharos-queue-count"),
            "no badge for none: {open}"
        );
    }

    /// B1031/V990: the focus a TV's compact player hands over lands on the
    /// stage bar's Expand button, which opens the full player.
    #[test]
    fn the_stage_bar_names_its_expand_button() {
        let html = bar(None);
        let id = format!(r#"id="{STAGE_EXPAND_ID}""#);
        let named = html.split('<').any(|tag| {
            tag.starts_with("button") && tag.contains(r#"aria-label="Expand""#) && tag.contains(&id)
        });
        assert!(named, "{html}");
    }

    /// B1031/V990: on a TV the floating strip is no focus stop, so its
    /// arrows move the focus between the bar's buttons, not the window.
    #[test]
    fn a_remote_never_moves_the_floating_window() {
        assert!(strip_movable(true, false), "a keyboard still moves it");
        assert!(!strip_movable(true, true));
        assert!(!strip_movable(false, false));
        assert!(!strip_movable(false, true));
    }

    /// B1031/V990: a TV's floating window has no resize grip to trap the
    /// D-pad; a keyboard's and a pointer's keep theirs.
    #[test]
    fn only_a_keyboard_or_pointer_gets_the_resize_grip() {
        assert_eq!(grip_shown(true), !REMOTE_FOCUS);
        assert!(!grip_shown(false));
    }

    #[test]
    fn the_queue_button_names_its_count() {
        assert_eq!(queue_label(0), "Queue");
        assert_eq!(queue_label(1), "Queue, 1 song");
        assert_eq!(queue_label(12), "Queue, 12 songs");
    }

    #[test]
    fn the_docked_queue_logs_its_state_the_view_and_the_count() {
        assert_eq!(
            dock_line(true, View::Mini, 4),
            "pharos: stage queue shown=true view=mini upcoming=4"
        );
        assert_eq!(
            dock_line(false, View::Floating, 0),
            "pharos: stage queue shown=false view=floating upcoming=0"
        );
    }
}
