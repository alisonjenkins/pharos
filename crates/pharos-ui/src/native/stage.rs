//! The stage (spec 020): one playback for the whole session, shown full on
//! the watch route and as a floating window or a mini bar everywhere else.
//!
//! The stage is the one place the player is mounted. It sits beside the
//! routed screen, not in it, so leaving the watch page changes the stage's
//! view (a class) rather than unmounting the player and its stream.

use dioxus::prelude::*;
use pharos_client::api::ItemRef;

use super::listen::{Icon, ICON_FLOAT, ICON_PIP};
use super::platform;
use super::playback::MediaState;
use super::route::Route;
use super::watch_view::Control;

/// The stage element, for its size while dragging.
pub const STAGE_ID: &str = "pharos-stage";
/// Where the floating window was dropped, kept for the tab (FR-006).
const SPOT_KEY: &str = "pharos.stage.spot";
/// The view used when the player is not full, kept for the tab.
const VIEW_KEY: &str = "pharos.stage.view";
/// The floating window's drag handle.
const STRIP_CLASS: &str = "pharos-stage-strip";
/// How far an arrow key moves the floating window, in px (FR-007).
pub const KEY_STEP_PX: f64 = 16.0;

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

/// What plays and how it is shown.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    /// The item playing; none when nothing is.
    pub item: Option<ItemRef>,
    pub view: View,
    /// The view to use when the player leaves full: floating or mini.
    pub compact: View,
    pub spot: Spot,
}

impl Default for Stage {
    fn default() -> Self {
        Stage {
            item: None,
            view: View::Full,
            compact: View::Floating,
            spot: Spot::default(),
        }
    }
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
            ..Stage::default()
        }
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
                self.item = Some(*id);
                self.view = View::Full;
            }
            // Manage is another app: the player ends there (FR-009).
            Route::Manage => self.close(),
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
        self.item = Some(*id);
        true
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

    /// Stop showing anything.
    pub fn close(&mut self) {
        self.item = None;
        self.view = View::Full;
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
}

/// A drag in progress: where the pointer and the window started.
#[derive(Debug, Clone, Copy)]
struct Drag {
    pointer: (f64, f64),
    from: Spot,
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
) -> Element {
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
        if !floating || !platform::key_on_itself(&evt, STRIP_CLASS) {
            return;
        }
        let delta = match evt.key() {
            Key::ArrowLeft => (-KEY_STEP_PX, 0.0),
            Key::ArrowRight => (KEY_STEP_PX, 0.0),
            Key::ArrowUp => (0.0, -KEY_STEP_PX),
            Key::ArrowDown => (0.0, KEY_STEP_PX),
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
                tabindex: if floating { "0" } else { "-1" },
                aria_label: if floating { "Move the player with the arrow keys" },
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
                    img { class: "pharos-stage-art", src: "{src}", alt: "" }
                }
                div { class: "pharos-stage-text",
                    p { class: "pharos-stage-title", title: "{title}", "{title}" }
                    if let Some(message) = error {
                        p { class: "pharos-stage-error", role: "alert", "{message}" }
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
                        aria_label: "Expand",
                        title: "Expand to the full screen",
                        onclick: move |_| on_action.call(StageAction::Show(View::Full)),
                        Icon { path: ICON_FULL }
                    }
                    if floating {
                        button {
                            class: "pharos-stage-button",
                            r#type: "button",
                            aria_label: "Mini bar",
                            title: "Shrink to a bar along the bottom",
                            onclick: move |_| on_action.call(StageAction::Show(View::Mini)),
                            Icon { path: ICON_MINI }
                        }
                    } else {
                        button {
                            class: "pharos-stage-button",
                            r#type: "button",
                            aria_label: "Floating window",
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
            div { class: "pharos-stage-progress", aria_hidden: "true",
                span { style: "width: {played:.2}%" }
            }
        }
    }
}

const ICON_PLAY: &str = "M8 5v14l11-7L8 5Z";
const ICON_PAUSE: &str = "M7 5h3.5v14H7zM13.5 5H17v14h-3.5z";
const ICON_PREVIOUS: &str = "M6 6h2v12H6V6Zm3.5 6 8.5 6V6l-8.5 6Z";
const ICON_NEXT: &str = "M16 6h2v12h-2V6ZM6 18l8.5-6L6 6v12Z";
/// Corners pointing out.
pub const ICON_FULL: &str =
    "M5 5h5v2H7v3H5V5Zm9 0h5v5h-2V7h-3V5ZM5 14h2v3h3v2H5v-5Zm12 3v-3h2v5h-5v-2h3Z";
/// A bar along the bottom of a window.
const ICON_MINI: &str = "M3 5h18v14H3V5Zm2 2v7h14V7H5Zm0 9v1h14v-1H5Z";
const ICON_CLOSE: &str = "M19 6.4 17.6 5 12 10.6 6.4 5 5 6.4 10.6 12 5 17.6 6.4 19 12 13.4 17.6 19 19 17.6 13.4 12 19 6.4Z";

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: (f64, f64) = (1000.0, 800.0);
    const WINDOW: (f64, f64) = (320.0, 220.0);

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
    fn going_to_manage_ends_the_player() {
        let mut stage = playing_full(7);
        stage.follow(&Route::Home);
        stage.follow(&Route::Manage);
        assert_eq!(stage.item, None);
        assert_eq!(stage.follow(&Route::Home), None);
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
}
