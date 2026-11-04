//! The watch page as a pure component: the `<video>` and the controls drawn
//! over it (013 US2, US3). What the controls show comes in as [`Controls`];
//! what the viewer does goes out as [`Control`], which the player turns into
//! calls on the `<video>` or a new playback.

use dioxus::html::geometry::WheelDelta;
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use pharos_client::api::{Chapter, Item, ItemRef, Segment, Track, Trickplay};

use super::listen::{Icon, ICON_FLOAT, ICON_PIP};
use super::platform;
use super::playback::{
    clock, left_to_watch, percent, segment_at, skip_label, thumb_at, track_label, MediaEvent,
    MediaState, UP_NEXT_COUNTDOWN_MS,
};
use super::seen::SeenRow;
use super::together::Member;
use super::together_view::{ActivityToast, MemberStrip};
use super::views::{ends_at, episode_label, use_ends_at_refresh};

/// The `<video>` the playback engine attaches to.
pub const VIDEO_ID: &str = "pharos-video";
/// The `<audio>` of a song handing over to the next, which takes
/// [`VIDEO_ID`].
pub const OUTGOING_ID: &str = "pharos-video-out";
/// The element that goes fullscreen: the video with its controls.
pub const PLAYER_ID: &str = "pharos-player";
/// The seek bar, whose width turns a pointer offset into a position.
pub const SEEK_ID: &str = "pharos-seek";

/// How far the arrow keys and skip buttons seek.
pub(super) const SEEK_STEP_MS: u64 = 10_000;
/// How far the up and down keys move the volume.
const VOLUME_STEP: f64 = 0.1;
/// How often a playing picture checks for a still pointer.
const IDLE_TICK_MS: u32 = 500;
/// Still ticks before the chrome fades: 2 to 2.5 s after the last movement,
/// about what modern players wait, so the picture has the screen.
const IDLE_TICKS: u32 = 4;
/// How far one wheel notch moves the volume, or seeks with Shift held.
const WHEEL_VOLUME_STEP: f64 = 0.05;
const WHEEL_SEEK_MS: u64 = 5_000;
/// Wheel travel that makes one notch. A mouse notch is about this far; a
/// trackpad sends many small deltas, which add up instead of each counting.
const WHEEL_NOTCH_PX: f64 = 100.0;
/// A line of wheel travel (`deltaMode` 1, as Firefox reports a mouse).
const WHEEL_LINE_PX: f64 = 40.0;

/// Everything the controls show.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Controls {
    pub media: MediaState,
    /// The item's runtime, for the seek bar until the `<video>` knows its
    /// duration.
    pub runtime_ms: u64,
    pub chapters: Vec<Chapter>,
    pub segments: Vec<Segment>,
    /// The sprite set the seek-bar preview draws from.
    pub preview: Option<Trickplay>,
    /// Where the pointer is over the seek bar.
    pub hover_ms: Option<u64>,
    pub audio: Vec<Track>,
    pub subtitles: Vec<Track>,
    pub audio_pick: Option<u32>,
    /// The subtitle on screen, rendered or burned; `None` is off.
    pub subtitle_pick: Option<u32>,
    /// The episodes before and after this one, when it is an episode.
    pub previous: Option<ItemRef>,
    pub next: Option<ItemRef>,
    /// The "Up next" card, while it counts down.
    pub up_next: Option<UpNextCard>,
}

/// The next episode, as the "Up next" card shows it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpNextCard {
    pub name: String,
    /// Its season and number, such as "S2 · E1".
    pub number: Option<String>,
    pub image: Option<String>,
    /// Whole seconds until it plays, rounded up.
    pub seconds: u64,
    /// How much of the countdown has run, 0 to 100.
    pub elapsed_pct: f64,
}

impl UpNextCard {
    /// The card for `next`, or a plain one while it is unknown, with
    /// `left_ms` of the countdown to run.
    pub fn of(next: Option<&Item>, left_ms: u64) -> Self {
        let run = UP_NEXT_COUNTDOWN_MS.saturating_sub(left_ms.min(UP_NEXT_COUNTDOWN_MS));
        Self {
            name: next.map_or_else(|| "Next episode".to_owned(), |n| n.name.clone()),
            number: next.and_then(episode_label),
            image: next.and_then(|n| n.image.clone()),
            seconds: left_ms.div_ceil(1_000),
            elapsed_pct: percent(run, UP_NEXT_COUNTDOWN_MS),
        }
    }
}

impl Controls {
    fn total_ms(&self) -> u64 {
        total_ms(&self.media, self.runtime_ms)
    }
}

/// The length the seek bar spans: the element's duration, or the item's
/// runtime until the element knows it.
pub(super) fn total_ms(media: &MediaState, runtime_ms: u64) -> u64 {
    match media.duration_ms {
        0 => runtime_ms,
        d => d,
    }
}

/// What a key does to the transport, on the video and music pages alike:
/// space or k plays and pauses, the arrows seek and set the volume, m mutes.
pub(super) fn transport_key(key: &Key, media: &MediaState) -> Option<Control> {
    let volume = |v: f64| Control::Volume {
        volume: v.clamp(0.0, 1.0),
        muted: false,
    };
    Some(match key {
        Key::Character(k) if k == " " || k == "k" => Control::TogglePlay,
        Key::Character(k) if k == "m" => Control::Volume {
            volume: media.volume,
            muted: !media.muted,
        },
        Key::ArrowLeft => Control::Seek(media.position_ms.saturating_sub(SEEK_STEP_MS)),
        Key::ArrowRight => Control::Seek(media.position_ms.saturating_add(SEEK_STEP_MS)),
        Key::ArrowUp => volume(media.volume + VOLUME_STEP),
        Key::ArrowDown => volume(media.volume - VOLUME_STEP),
        _ => return None,
    })
}

/// What the viewer asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Control {
    TogglePlay,
    Seek(u64),
    /// The pointer is `x` px into the seek bar; `None` when it left.
    Hover(Option<f64>),
    /// A click `x` px into the seek bar.
    SeekAt(f64),
    Volume {
        volume: f64,
        muted: bool,
    },
    Audio(u32),
    Subtitle(Option<u32>),
    Fullscreen,
    /// Hand the video to the browser's picture-in-picture window (spec 020).
    PictureInPicture,
    /// Leave the player for the page before it.
    Back,
    /// Shrink the player to its floating window or mini bar (spec 020).
    Shrink,
    /// Play another item: the previous or next episode.
    Play(ItemRef),
    /// The previous song, or this one from the start (B464/V406).
    Previous,
    /// Play the next episode now, as finishing this one ("Up next").
    PlayNext,
    /// Hide "Up next" and stay on this episode to its end.
    CancelNext,
}

/// The player. The engine gives the `<video>` its stream once mounted;
/// `on_media` hears the events progress reports follow, `on_control` the
/// viewer.
#[component]
pub fn WatchView(
    title: String,
    controls: Controls,
    on_media: EventHandler<MediaEvent>,
    on_control: EventHandler<Control>,
    /// The group watching together, when there is one: shown over the
    /// picture with a Leave button.
    #[props(default)]
    members: Vec<Member>,
    #[props(default)] on_leave: EventHandler<()>,
    /// Each member's mark on this item, for the member strip.
    #[props(default)]
    seen: Vec<SeenRow>,
    /// The item playing, named on the `<video>` as `data-item-id` for tests
    /// that watch several browsers (the sync oracle).
    #[props(default)]
    item_id: Option<ItemRef>,
    /// Whether the video starts by itself. A group member's video waits for
    /// the group's play.
    #[props(default = true)]
    autoplay: bool,
    /// Whether this member is off the group's tracks ("Just me").
    #[props(default)]
    own_tracks: bool,
    /// Follow group (`false`) or Just me (`true`), picked in the track menus.
    #[props(default)]
    on_own_tracks: EventHandler<bool>,
    /// Ids of the group members off the group's tracks, for the strip.
    #[props(default)]
    own_track_members: Vec<String>,
    /// The activity log's lines, for the strip.
    #[props(default)]
    activity: Vec<String>,
    /// The activity line to show over the video now, if any.
    #[props(default)]
    toast: Option<String>,
) -> Element {
    use_ends_at_refresh();
    let in_group = !members.is_empty();
    let c = &controls;
    let total = c.total_ms();
    let media = c.media;
    // The chrome over the picture fades while the pointer rests, as in any
    // modern player: `stirred` records movement since the last tick, and one
    // ticking task runs while the chrome is shown.
    let mut idle = use_signal(|| false);
    let mut stirred = use_signal(|| false);
    let mut ticking = use_signal(|| false);
    // A tap that only brings the hidden chrome back must not also pause; a
    // paused player's chrome is never hidden.
    let mut woke_by_tap = use_signal(|| false);
    let mut wake = move || {
        stirred.set(true);
        if *idle.peek() {
            idle.set(false);
        }
        if *ticking.peek() {
            return;
        }
        ticking.set(true);
        spawn(async move {
            let mut still = 0;
            while still < IDLE_TICKS {
                platform::sleep_ms(IDLE_TICK_MS).await;
                if *stirred.peek() {
                    stirred.set(false);
                    still = 0;
                } else {
                    still += 1;
                }
            }
            ticking.set(false);
            idle.set(true);
        });
    };
    use_hook(wake);
    let class = match (media.paused, idle()) {
        (true, _) => "pharos-player is-paused",
        (false, true) => "pharos-player is-idle",
        (false, false) => "pharos-player",
    };
    let shortcuts = use_shortcuts(media, c.previous, c.next, on_control);
    let on_key = move |evt: KeyboardEvent| {
        wake();
        let key = evt.key();
        let control = shortcuts
            .key(&key, evt.modifiers().shift())
            .or_else(|| match &key {
                Key::Character(k) if k == "f" => Some(Control::Fullscreen),
                key => transport_key(key, &media),
            });
        let Some(control) = control else {
            return;
        };
        evt.prevent_default();
        on_control.call(control);
    };
    let skip = segment_at(&c.segments, media.position_ms);
    let ends = ends_at(left_to_watch(&c.segments, media.position_ms, total));
    let text_tracks: Vec<(String, Track)> = c
        .subtitles
        .iter()
        .filter(|t| t.vtt_url.is_some())
        .map(|t| (text_track_id(t.index), t.clone()))
        .collect();

    rsx! {
        section { class: "pharos-watch",
            div {
                id: PLAYER_ID,
                class,
                tabindex: 0,
                onkeydown: on_key,
                onpointermove: move |_| wake(),
                onpointerdown: move |evt: PointerEvent| {
                    woke_by_tap.set(evt.pointer_type() == "touch" && idle() && !media.paused);
                    wake();
                },
                onwheel: move |evt: WheelEvent| {
                    wake();
                    shortcuts.wheel(&evt);
                },
                // The styled subtitle renderer lays its canvas after the
                // video, so the video sits alone where Dioxus never diffs.
                div { class: "pharos-video-frame",
                    video {
                        id: VIDEO_ID,
                        class: "pharos-video",
                        "data-item-id": item_id.map(|id| id.to_string()),
                        autoplay,
                        playsinline: true,
                        onclick: move |evt: MouseEvent| {
                            if woke_by_tap() {
                                woke_by_tap.set(false);
                            } else if !shortcuts.click(&evt) {
                                on_control.call(Control::TogglePlay);
                            }
                        },
                        oncontextmenu: move |evt: MouseEvent| shortcuts.context_menu(&evt),
                        onmousedown: move |evt: MouseEvent| shortcuts.mouse_down(&evt),
                        onplaying: move |_| on_media.call(MediaEvent::Playing),
                        onpause: move |_| on_media.call(MediaEvent::Pause),
                        onseeking: move |_| on_media.call(MediaEvent::Changed),
                        onseeked: move |_| on_media.call(MediaEvent::Seeked),
                        ontimeupdate: move |_| on_media.call(MediaEvent::TimeUpdate),
                        onended: move |_| on_media.call(MediaEvent::Ended),
                        onvolumechange: move |_| on_media.call(MediaEvent::Changed),
                        ondurationchange: move |_| on_media.call(MediaEvent::Changed),
                        onloadedmetadata: move |_| on_media.call(MediaEvent::Changed),
                        onloadeddata: move |_| on_media.call(MediaEvent::Changed),
                        onwaiting: move |_| on_media.call(MediaEvent::Waiting),
                        onstalled: move |_| on_media.call(MediaEvent::Waiting),
                        oncanplay: move |_| on_media.call(MediaEvent::CanPlay),
                        oncanplaythrough: move |_| on_media.call(MediaEvent::Changed),
                        for (id, t) in text_tracks {
                            track {
                                key: "{id}",
                                id: "{id}",
                                kind: "subtitles",
                                src: t.vtt_url.clone().unwrap_or_default(),
                                srclang: "{t.language}",
                                label: track_label(&t, 0),
                            }
                        }
                    }
                }
                if media.loading {
                    div {
                        class: "pharos-watch-loading",
                        role: "status",
                        aria_live: "polite",
                        "data-loading": "true",
                        span { class: "pharos-watch-spinner", aria_hidden: "true" }
                        span { class: "pharos-watch-loading-label", "Loading" }
                        span { class: "pharos-watch-loading-caption", "Waiting for the server…" }
                    }
                }
                div { class: "pharos-watch-top",
                    button {
                        class: "pharos-control",
                        r#type: "button",
                        aria_label: "Back",
                        onclick: move |_| on_control.call(Control::Back),
                        "←"
                    }
                    h2 { class: "pharos-watch-title", "{title}" }
                    button {
                        class: "pharos-control pharos-shrink",
                        r#type: "button",
                        aria_label: "Shrink",
                        title: "Shrink to a small window",
                        onclick: move |_| on_control.call(Control::Shrink),
                        Icon { path: ICON_FLOAT }
                    }
                    if !members.is_empty() {
                        MemberStrip { members, on_leave, seen, own_tracks: own_track_members, activity }
                    }
                }
                ActivityToast { line: toast }
                if let Some(card) = c.up_next.clone() {
                    UpNext { card, on_control }
                } else if let Some(seg) = skip {
                    button {
                        class: "pharos-skip",
                        r#type: "button",
                        onclick: move |_| on_control.call(Control::Seek(seg.end_ms)),
                        {skip_label(seg.kind)}
                    }
                }
                div { class: "pharos-controls",
                    SeekBar { controls: controls.clone(), total, on_control }
                    div { class: "pharos-control-row",
                        button {
                            class: "pharos-control",
                            r#type: "button",
                            aria_label: if media.paused { "Play" } else { "Pause" },
                            onclick: move |_| on_control.call(Control::TogglePlay),
                            if media.paused { "▶" } else { "❚❚" }
                        }
                        if let Some(id) = c.previous {
                            button {
                                class: "pharos-control",
                                r#type: "button",
                                aria_label: "Previous episode",
                                onclick: move |_| on_control.call(Control::Play(id)),
                                "⏮"
                            }
                        }
                        if let Some(id) = c.next {
                            button {
                                class: "pharos-control",
                                r#type: "button",
                                aria_label: "Next episode",
                                onclick: move |_| on_control.call(Control::Play(id)),
                                "⏭"
                            }
                        }
                        span { class: "pharos-time",
                            "{clock(media.position_ms)} / {clock(total)}"
                        }
                        if let Some(ends) = &ends {
                            span { class: "pharos-time pharos-ends-at", "{ends}" }
                        }
                        Volume { media, on_control }
                        span { class: "pharos-control-spacer" }
                        TrackMode { in_group, own_tracks, on_change: on_own_tracks }
                        TrackPicker {
                            label: "Audio",
                            tracks: c.audio.clone(),
                            pick: c.audio_pick,
                            offer_off: false,
                            on_pick: move |p: Option<u32>| {
                                if let Some(i) = p {
                                    on_control.call(Control::Audio(i));
                                }
                            },
                        }
                        TrackPicker {
                            label: "Subtitles",
                            tracks: c.subtitles.clone(),
                            pick: c.subtitle_pick,
                            offer_off: true,
                            on_pick: move |p| on_control.call(Control::Subtitle(p)),
                        }
                        if platform::pip_supported() {
                            button {
                                class: "pharos-control",
                                r#type: "button",
                                aria_label: "Picture in picture",
                                title: "Picture in picture",
                                onclick: move |_| on_control.call(Control::PictureInPicture),
                                Icon { path: ICON_PIP }
                            }
                        }
                        button {
                            class: "pharos-control",
                            r#type: "button",
                            aria_label: "Fullscreen",
                            onclick: move |_| on_control.call(Control::Fullscreen),
                            "⛶"
                        }
                    }
                }
            }
        }
    }
}

/// The `<track id>` of the subtitle stream `index`.
pub fn text_track_id(index: u32) -> String {
    format!("pharos-sub-{index}")
}

/// The next episode and a countdown to it, over the end of this one.
#[component]
fn UpNext(card: UpNextCard, on_control: EventHandler<Control>) -> Element {
    let s = card.seconds;
    rsx! {
        aside { class: "pharos-up-next", aria_label: "Up next",
            if let Some(src) = card.image.clone() {
                img { class: "pharos-up-next-art", src, alt: "" }
            }
            div { class: "pharos-up-next-text",
                p { class: "pharos-up-next-eyebrow", "Up next" }
                p { class: "pharos-up-next-name", "{card.name}" }
                if let Some(number) = card.number.clone() {
                    p { class: "pharos-up-next-number", "{number}" }
                }
                p { class: "pharos-up-next-count", role: "timer",
                    if s == 1 { "Playing in 1 second" } else { "Playing in {s} seconds" }
                }
                div {
                    class: "pharos-up-next-bar",
                    style: "--pharos-fill: {card.elapsed_pct:.1}%",
                }
                div { class: "pharos-up-next-actions",
                    button {
                        class: "pharos-up-next-play",
                        r#type: "button",
                        onclick: move |_| on_control.call(Control::PlayNext),
                        "Play now"
                    }
                    button {
                        class: "pharos-up-next-cancel",
                        r#type: "button",
                        onclick: move |_| on_control.call(Control::CancelNext),
                        "Cancel"
                    }
                }
            }
        }
    }
}

/// Played and buffered progress, chapter ticks, skippable spans and the
/// trickplay preview under the pointer.
#[component]
fn SeekBar(controls: Controls, total: u64, on_control: EventHandler<Control>) -> Element {
    let c = &controls;
    let played = percent(c.media.position_ms, total);
    let hover = c.hover_ms.map(|ms| {
        let chapter = c
            .chapters
            .iter()
            .rev()
            .find(|ch| ch.start_ms <= ms)
            .map(|ch| ch.name.clone())
            .filter(|n| !n.is_empty());
        let thumb = c.preview.as_ref().and_then(|set| thumb_at(set, ms));
        (percent(ms, total), clock(ms), chapter, thumb)
    });
    rsx! {
        div {
            id: SEEK_ID,
            class: "pharos-seek",
            role: "slider",
            aria_label: "Seek",
            aria_valuemin: 0,
            aria_valuemax: total / 1000,
            aria_valuenow: c.media.position_ms / 1000,
            aria_valuetext: clock(c.media.position_ms),
            onmousemove: move |evt: MouseEvent| {
                on_control.call(Control::Hover(Some(evt.element_coordinates().x)));
            },
            onmouseleave: move |_| on_control.call(Control::Hover(None)),
            onclick: move |evt: MouseEvent| {
                on_control.call(Control::SeekAt(evt.element_coordinates().x));
            },
            div { class: "pharos-seek-track",
                for (i, s) in c.segments.iter().enumerate().filter(|(_, s)| s.end_ms > s.start_ms) {
                    span {
                        key: "{i}",
                        class: "pharos-seek-segment",
                        style: "left: {percent(s.start_ms, total)}%; width: {percent(s.end_ms, total) - percent(s.start_ms, total)}%",
                    }
                }
                span { class: "pharos-seek-played", style: "width: {played}%" }
                for (i, ch) in c.chapters.iter().enumerate().filter(|(_, ch)| ch.start_ms > 0) {
                    span {
                        key: "{i}",
                        class: "pharos-seek-chapter",
                        style: "left: {percent(ch.start_ms, total)}%",
                        title: "{ch.name}",
                    }
                }
            }
            if let Some((at, time, chapter, thumb)) = hover {
                div { class: "pharos-seek-preview", style: "left: {at}%",
                    if let Some(t) = thumb {
                        div {
                            class: "pharos-seek-thumb",
                            style: "width: {t.width}px; height: {t.height}px; background-image: url('{t.url}'); background-position: -{t.x}px -{t.y}px",
                        }
                    }
                    if let Some(name) = chapter {
                        span { class: "pharos-seek-chapter-name", "{name}" }
                    }
                    span { class: "pharos-seek-time", "{time}" }
                }
            }
        }
    }
}

/// The mute button and volume slider. The video page's chrome is the
/// default; the music page passes its own classes and icon.
#[component]
pub(super) fn Volume(
    media: MediaState,
    on_control: EventHandler<Control>,
    #[props(default = "pharos-control")] button_class: &'static str,
    #[props(default = "pharos-volume")] range_class: &'static str,
    /// Replaces the emoji on the mute button.
    #[props(default)]
    icon: Option<Element>,
    /// Draws the slider's fill up to the thumb, for themes that style it.
    #[props(default)]
    filled: bool,
) -> Element {
    let shown = if media.muted { 0.0 } else { media.volume };
    let fill = format!("--pharos-fill: {:.0}%", shown * 100.0);
    rsx! {
        button {
            class: button_class,
            r#type: "button",
            aria_label: if media.muted { "Unmute" } else { "Mute" },
            onclick: move |_| on_control.call(Control::Volume {
                // Unmuting a slider dragged to 0 would stay silent.
                volume: if media.muted && media.volume == 0.0 { 1.0 } else { media.volume },
                muted: !media.muted,
            }),
            match icon {
                Some(icon) => icon,
                None => rsx! { if shown == 0.0 { "🔇" } else { "🔊" } },
            }
        }
        input {
            class: range_class,
            r#type: "range",
            aria_label: "Volume",
            min: 0,
            max: 1,
            step: 0.05,
            value: "{shown}",
            style: if filled { fill },
            // Arrows move the slider, not the player's volume and seek keys.
            onkeydown: move |evt: KeyboardEvent| evt.stop_propagation(),
            oninput: move |evt: FormEvent| {
                if let Ok(volume) = evt.value().parse::<f64>() {
                    on_control.call(Control::Volume { volume, muted: volume == 0.0 });
                }
            },
        }
    }
}

/// "Follow group" or "Just me" for the track menus. Only in a group.
#[component]
fn TrackMode(in_group: bool, own_tracks: bool, on_change: EventHandler<bool>) -> Element {
    if !in_group {
        return rsx! {};
    }
    rsx! {
        select {
            class: "pharos-track-picker pharos-track-mode",
            aria_label: "Tracks",
            title: "Tracks",
            onkeydown: move |evt: KeyboardEvent| evt.stop_propagation(),
            onchange: move |evt: FormEvent| {
                on_change.call(evt.value() == "me");
                platform::focus_by_id(PLAYER_ID);
            },
            option { value: "follow", selected: !own_tracks, "Follow group" }
            option { value: "me", selected: own_tracks, "Just me" }
        }
    }
}

/// A `<select>` of tracks; nothing at all for an item with one audio track
/// or no subtitles.
#[component]
fn TrackPicker(
    label: &'static str,
    tracks: Vec<Track>,
    pick: Option<u32>,
    offer_off: bool,
    on_pick: EventHandler<Option<u32>>,
) -> Element {
    let worth_showing = if offer_off {
        !tracks.is_empty()
    } else {
        tracks.len() > 1
    };
    if !worth_showing {
        return rsx! {};
    }
    rsx! {
        select {
            class: "pharos-track-picker",
            aria_label: label,
            title: label,
            onkeydown: move |evt: KeyboardEvent| evt.stop_propagation(),
            onchange: move |evt: FormEvent| {
                on_pick.call(evt.value().parse::<u32>().ok());
                // The picker swallows keys; once picked, they go to the shortcuts again.
                platform::focus_by_id(PLAYER_ID);
            },
            if offer_off {
                option { value: "off", selected: pick.is_none(), "{label} off" }
            }
            for (i, t) in tracks.iter().enumerate() {
                option {
                    key: "{t.index}",
                    value: "{t.index}",
                    selected: pick == Some(t.index),
                    {track_label(t, i)}
                }
            }
        }
    }
}

/// A neighbour a Shift+click asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Skip {
    Previous,
    Next,
}

/// The skip a mouse button asks for with Shift held: the main button is the
/// previous episode or track, the middle or secondary the next. Firefox
/// keeps Shift+right click for its own menu (B405), so the middle button is
/// the one every browser leaves to the page. A click that started on a
/// control is that control's own, so it skips nothing.
pub fn shift_skip(shift: bool, button: Option<MouseButton>, on_a_control: bool) -> Option<Skip> {
    if !shift || on_a_control {
        return None;
    }
    match button {
        Some(MouseButton::Primary) => Some(Skip::Previous),
        Some(MouseButton::Auxiliary | MouseButton::Secondary) => Some(Skip::Next),
        _ => None,
    }
}

/// The mouse shortcuts both players share: the wheel changes the volume, or
/// seeks with Shift, and Shift+click / Shift+middle or right click play the
/// previous / next item. Hand each handler the matching event of the
/// player's surface.
#[derive(Clone, Copy)]
pub struct Shortcuts {
    media: MediaState,
    previous: Option<ItemRef>,
    next: Option<ItemRef>,
    on_control: EventHandler<Control>,
    /// Wheel travel short of a whole notch, kept for the next event.
    carry: Signal<f64>,
    /// Previous goes through `Control::Previous`, as the song's button does.
    song: bool,
}

pub fn use_shortcuts(
    media: MediaState,
    previous: Option<ItemRef>,
    next: Option<ItemRef>,
    on_control: EventHandler<Control>,
) -> Shortcuts {
    Shortcuts {
        media,
        previous,
        next,
        on_control,
        carry: use_signal(|| 0.0),
        song: false,
    }
}

impl Shortcuts {
    /// For a song, where Previous may start it again (B464/V406).
    pub fn for_song(mut self) -> Self {
        self.song = true;
        self
    }

    pub fn wheel(mut self, evt: &WheelEvent) {
        evt.prevent_default();
        let (notches, rest) = wheel_notches(*self.carry.peek(), evt.delta());
        self.carry.set(rest);
        if let Some(control) = wheel_control(notches, evt.modifiers().shift(), self.media) {
            self.on_control.call(control);
        }
    }

    /// A left click; true when it was a Shift+click this handled.
    pub fn click(self, evt: &MouseEvent) -> bool {
        let skip = shift_skip(
            evt.modifiers().shift(),
            evt.trigger_button(),
            platform::on_a_control(evt),
        );
        if skip != Some(Skip::Previous) {
            return false;
        }
        if self.song {
            self.on_control.call(Control::Previous);
        } else {
            self.play(self.previous);
        }
        true
    }

    /// A Shift+middle or Shift+right press plays the next item. It acts on
    /// the press: Firefox on Linux opens its menu there, and the menu takes
    /// the release.
    pub fn mouse_down(self, evt: &MouseEvent) {
        let skip = shift_skip(
            evt.modifiers().shift(),
            evt.trigger_button(),
            platform::on_a_control(evt),
        );
        if skip != Some(Skip::Next) || self.next.is_none() {
            return;
        }
        // No autoscroll or paste from the middle button.
        evt.prevent_default();
        if evt.trigger_button() == Some(MouseButton::Secondary) {
            platform::swallow_next_context_menu();
        }
        self.play(self.next);
    }

    /// Shift+right click is next, so it opens no menu where the browser lets
    /// the page say so. Firefox shows its menu on Shift+right click and never
    /// tells the page, but the skip still happens on the press.
    pub fn context_menu(self, evt: &MouseEvent) {
        if evt.modifiers().shift() && !platform::on_a_control(evt) {
            evt.prevent_default();
        }
    }

    /// Shift+P / Shift+N.
    pub fn key(self, key: &Key, shift: bool) -> Option<Control> {
        match key {
            Key::Character(k) if shift && k.eq_ignore_ascii_case("p") => {
                if self.song {
                    Some(Control::Previous)
                } else {
                    self.previous.map(Control::Play)
                }
            }
            Key::Character(k) if shift && k.eq_ignore_ascii_case("n") => {
                self.next.map(Control::Play)
            }
            _ => None,
        }
    }

    fn play(self, to: Option<ItemRef>) {
        if let Some(id) = to {
            self.on_control.call(Control::Play(id));
        }
    }
}

/// Adds `delta` to the wheel travel `carried` from earlier events: the
/// whole notches it now makes, up (away from the viewer) counting positive,
/// and the travel left over. Shift turns a wheel sideways on some systems, so
/// the larger axis counts.
pub fn wheel_notches(carried: f64, delta: WheelDelta) -> (i32, f64) {
    let along = |x: f64, y: f64| if y.abs() >= x.abs() { y } else { x };
    let px = match delta {
        WheelDelta::Pixels(v) => along(v.x, v.y),
        WheelDelta::Lines(v) => along(v.x, v.y) * WHEEL_LINE_PX,
        WheelDelta::Pages(v) => along(v.x, v.y) * WHEEL_NOTCH_PX,
    };
    let travel = carried - px;
    let notches = (travel / WHEEL_NOTCH_PX).trunc();
    // A wheel moves at most a few dozen notches between events.
    #[allow(clippy::cast_possible_truncation)]
    let whole = notches.clamp(-1_000.0, 1_000.0) as i32;
    (whole, travel - notches * WHEEL_NOTCH_PX)
}

/// What `notches` of wheel do: change the volume, or with Shift seek.
pub fn wheel_control(notches: i32, shift: bool, media: MediaState) -> Option<Control> {
    if notches == 0 {
        return None;
    }
    if shift {
        let by = u64::from(notches.unsigned_abs()) * WHEEL_SEEK_MS;
        let to = if notches > 0 {
            media.position_ms.saturating_add(by)
        } else {
            media.position_ms.saturating_sub(by)
        };
        return Some(Control::Seek(to));
    }
    Some(Control::Volume {
        volume: (media.volume + f64::from(notches) * WHEEL_VOLUME_STEP).clamp(0.0, 1.0),
        muted: false,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use pharos_client::api::SegmentKind;

    use super::*;

    fn render(controls: Controls) -> String {
        let mut dom = VirtualDom::new_with_props(
            |controls: Controls| {
                rsx! {
                    WatchView {
                        title: "Pilot",
                        controls,
                        on_media: |_| {},
                        on_control: |_| {},
                    }
                }
            },
            controls,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn controls() -> Controls {
        let english = Track {
            index: 3,
            language: "eng".into(),
            vtt_url: Some("/videos/7/subtitles/3/stream.vtt".into()),
            ..Track::default()
        };
        let signs = Track {
            index: 4,
            title: "Signs".into(),
            ..Track::default()
        };
        Controls {
            media: MediaState {
                position_ms: 30_000,
                duration_ms: 1_200_000,
                ..MediaState::default()
            },
            runtime_ms: 1_200_000,
            chapters: vec![
                Chapter {
                    name: "Cold open".into(),
                    start_ms: 0,
                },
                Chapter {
                    name: "Act one".into(),
                    start_ms: 300_000,
                },
            ],
            segments: vec![Segment {
                kind: SegmentKind::Intro,
                start_ms: 10_000,
                end_ms: 70_000,
            }],
            audio: vec![
                Track {
                    index: 1,
                    language: "eng".into(),
                    ..Track::default()
                },
                Track {
                    index: 2,
                    language: "jpn".into(),
                    ..Track::default()
                },
            ],
            subtitles: vec![english, signs],
            audio_pick: Some(2),
            subtitle_pick: Some(3),
            ..Controls::default()
        }
    }

    #[test]
    fn the_player_says_when_the_video_ends_before_the_credits() {
        let mut c = controls();
        assert!(render(c.clone()).contains(">Ends at "));
        c.segments.push(Segment {
            kind: SegmentKind::Outro,
            start_ms: 1_100_000,
            end_ms: 1_200_000,
        });
        let html = render(c.clone());
        assert!(html.contains(" (before credits)<"), "{html}");
        c.media.position_ms = 1_150_000;
        let html = render(c);
        assert!(
            html.contains(">Ends at ") && !html.contains("before credits"),
            "{html}"
        );
    }

    #[test]
    fn the_controls_offer_skip_tracks_and_chapters() {
        let html = render(controls());
        assert!(html.contains("Skip intro"), "{html}");
        assert!(html.contains("0:30 / 20:00"), "{html}");
        // One tick for "Act one" a quarter of the way along; none at 0.
        assert!(html.contains("left: 25%"), "{html}");
        assert_eq!(html.matches("pharos-seek-chapter\"").count(), 1, "{html}");
        // Only the text subtitle becomes a <track>.
        assert!(
            html.contains(r#"id="pharos-sub-3""#) && !html.contains("pharos-sub-4"),
            "{html}"
        );
        assert!(
            html.contains("Subtitles off") && html.contains("Signs"),
            "{html}"
        );
        assert!(html.contains(r#"value="2" selected"#), "{html}");
        // The browser's own controls are gone.
        assert!(!html.contains("controls=true"), "{html}");
    }

    #[test]
    fn keys_drive_the_transport() {
        let media = MediaState {
            position_ms: 30_000,
            volume: 0.5,
            ..MediaState::default()
        };
        let key = |k: Key| transport_key(&k, &media);
        let ch = |c: &str| key(Key::Character(c.into()));
        assert_eq!(ch(" "), Some(Control::TogglePlay));
        assert_eq!(ch("k"), Some(Control::TogglePlay));
        assert_eq!(
            ch("m"),
            Some(Control::Volume {
                volume: 0.5,
                muted: true
            })
        );
        assert_eq!(key(Key::ArrowLeft), Some(Control::Seek(20_000)));
        assert_eq!(key(Key::ArrowRight), Some(Control::Seek(40_000)));
        assert_eq!(
            key(Key::ArrowUp),
            Some(Control::Volume {
                volume: 0.6,
                muted: false
            })
        );
        assert_eq!(ch("x"), None);
        // Seeking back from the start stays at the start.
        let start = MediaState::default();
        assert_eq!(
            transport_key(&Key::ArrowLeft, &start),
            Some(Control::Seek(0))
        );
    }

    #[test]
    fn the_title_and_back_are_drawn_over_the_picture() {
        let html = render(controls());
        let player = html.find(r#"id="pharos-player""#);
        let top = html.find("pharos-watch-top");
        assert!(player.is_some() && top > player, "{html}");
        assert!(html.contains(r#"aria-label="Back""#), "{html}");
        assert!(html.contains("Pilot"), "{html}");
    }

    #[test]
    fn the_video_carries_the_item_id_when_known() {
        let render_with = |item_id: Option<ItemRef>| {
            let mut dom = VirtualDom::new_with_props(
                |item_id: Option<ItemRef>| {
                    rsx! {
                        WatchView {
                            title: "Pilot",
                            controls: Controls::default(),
                            on_media: |_| {},
                            on_control: |_| {},
                            item_id,
                        }
                    }
                },
                item_id,
            );
            dom.rebuild_in_place();
            dioxus_ssr::render(&dom)
        };
        assert!(
            render_with(Some(ItemRef::Real(42))).contains(r#"data-item-id="42""#),
            "item id on the video"
        );
        assert!(!render_with(None).contains("data-item-id"));
    }

    /// A group member's video waits for the group's play; any other starts
    /// by itself.
    #[test]
    fn the_video_autoplays_unless_a_group_holds_it() {
        let render_with = |autoplay: bool| {
            let mut dom = VirtualDom::new_with_props(
                |autoplay: bool| {
                    rsx! {
                        WatchView {
                            title: "Pilot",
                            controls: Controls::default(),
                            on_media: |_| {},
                            on_control: |_| {},
                            autoplay,
                        }
                    }
                },
                autoplay,
            );
            dom.rebuild_in_place();
            dioxus_ssr::render(&dom)
        };
        assert!(render_with(true).contains("autoplay"));
        assert!(!render_with(false).contains("autoplay"));
    }

    fn render_group(in_group: bool, own_tracks: bool, toast: Option<String>) -> String {
        let mut dom = VirtualDom::new_with_props(
            move |(in_group, own_tracks, toast): (bool, bool, Option<String>)| {
                let members = if in_group {
                    vec![Member {
                        id: "a".into(),
                        name: "Ada".into(),
                        buffering: false,
                        own_tracks: false,
                    }]
                } else {
                    vec![]
                };
                rsx! {
                    WatchView {
                        title: "Pilot",
                        controls: controls(),
                        on_media: |_| {},
                        on_control: |_| {},
                        members,
                        own_tracks,
                        toast,
                    }
                }
            },
            (in_group, own_tracks, toast),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_track_menus_offer_follow_or_just_me_only_in_a_group() {
        let alone = render_group(false, false, None);
        assert!(
            !alone.contains("Follow group") && !alone.contains("Just me"),
            "{alone}"
        );
        let following = render_group(true, false, None);
        assert!(
            following.contains("Follow group") && following.contains("Just me"),
            "{following}"
        );
    }

    #[test]
    fn the_selected_choice_follows_own_tracks() {
        let selected = |html: &str, value: &str| {
            let tag = format!(r#"value="{value}""#);
            let after = html.split_once(tag.as_str()).expect("option").1;
            after
                .split('>')
                .next()
                .unwrap_or_default()
                .contains("selected")
        };
        let following = render_group(true, false, None);
        assert!(
            selected(&following, "follow") && !selected(&following, "me"),
            "{following}"
        );
        let own = render_group(true, true, None);
        assert!(selected(&own, "me") && !selected(&own, "follow"), "{own}");
    }

    #[test]
    fn the_toast_is_outside_the_chrome_that_idles() {
        let html = render_group(true, false, Some("Sam paused at 12:04".into()));
        let toast = html
            .find(r#"<div class="pharos-activity-toast""#)
            .expect("toast rendered");
        let top = html.find(r#"class="pharos-watch-top""#).expect("top bar");
        let controls = html.find(r#"class="pharos-controls""#).expect("controls");
        assert!(top < toast && toast < controls, "{html}");
        let between = &html[top..toast];
        assert!(
            between.matches("</div>").count() > between.matches("<div").count(),
            "toast inside the top bar: {html}"
        );
        assert!(!render_group(true, false, None).contains("pharos-activity-toast"));
    }

    #[test]
    fn the_loading_overlay_shows_outside_the_chrome_only_while_loading() {
        assert!(!render(controls()).contains("pharos-watch-loading"));
        let mut c = controls();
        c.media.loading = true;
        let html = render(c);
        let loading = html
            .find(r#"<div class="pharos-watch-loading""#)
            .expect("loading overlay rendered");
        let top = html.find(r#"class="pharos-watch-top""#).expect("top bar");
        assert!(loading < top, "{html}");
        assert!(html.contains(r#"data-loading="true""#), "{html}");
        assert!(html.contains("Loading"), "{html}");
        assert!(html.contains("Waiting for the server…"), "{html}");
    }

    #[test]
    fn episode_buttons_show_only_where_there_is_an_episode() {
        let html = render(controls());
        assert!(!html.contains("Previous episode") && !html.contains("Next episode"));
        let mut c = controls();
        c.next = Some(ItemRef::Real(8));
        let html = render(c);
        assert!(!html.contains("Previous episode"), "{html}");
        assert!(html.contains(r#"aria-label="Next episode""#), "{html}");
    }

    #[test]
    fn a_wheel_notch_steps_the_volume_and_with_shift_seeks() {
        let media = MediaState {
            position_ms: 60_000,
            volume: 0.5,
            ..MediaState::default()
        };
        let (up, _) = wheel_notches(0.0, WheelDelta::pixels(0.0, -100.0, 0.0));
        assert_eq!(up, 1);
        assert_eq!(
            wheel_control(up, false, media),
            Some(Control::Volume {
                volume: 0.55,
                muted: false
            })
        );
        let (down, _) = wheel_notches(0.0, WheelDelta::lines(0.0, 3.0, 0.0));
        assert_eq!(down, -1);
        assert_eq!(
            wheel_control(down, true, media),
            Some(Control::Seek(55_000))
        );
        // Shift turns the wheel sideways on some systems.
        let (sideways, _) = wheel_notches(0.0, WheelDelta::pixels(-100.0, 0.0, 0.0));
        assert_eq!(
            wheel_control(sideways, true, media),
            Some(Control::Seek(65_000))
        );
    }

    #[test]
    fn shift_with_a_mouse_button_skips_except_on_a_control() {
        let primary = Some(MouseButton::Primary);
        let secondary = Some(MouseButton::Secondary);
        assert_eq!(shift_skip(true, primary, false), Some(Skip::Previous));
        assert_eq!(shift_skip(true, secondary, false), Some(Skip::Next));
        assert_eq!(shift_skip(false, primary, false), None);
        assert_eq!(
            shift_skip(true, primary, true),
            None,
            "a control's own click"
        );
        assert_eq!(
            shift_skip(true, Some(MouseButton::Auxiliary), false),
            Some(Skip::Next),
            "the middle button, which no browser keeps for itself"
        );
        assert_eq!(shift_skip(true, Some(MouseButton::Fourth), false), None);
    }

    #[test]
    fn trackpad_travel_adds_up_to_a_notch_and_the_volume_stays_in_range() {
        let (none, rest) = wheel_notches(0.0, WheelDelta::pixels(0.0, -60.0, 0.0));
        assert_eq!(none, 0);
        assert_eq!(wheel_control(none, false, MediaState::default()), None);
        let (one, _) = wheel_notches(rest, WheelDelta::pixels(0.0, -60.0, 0.0));
        assert_eq!(one, 1);
        let loud = MediaState {
            volume: 1.0,
            ..MediaState::default()
        };
        assert_eq!(
            wheel_control(5, false, loud),
            Some(Control::Volume {
                volume: 1.0,
                muted: false
            })
        );
        assert_eq!(
            wheel_control(-3, true, MediaState::default()),
            Some(Control::Seek(0))
        );
    }

    #[test]
    fn up_next_shows_the_next_episode_and_its_countdown_in_place_of_skip() {
        let next = Item {
            id: ItemRef::Real(8),
            kind: pharos_client::api::ItemKind::Episode,
            name: "The Return".into(),
            year: None,
            runtime_ms: 0,
            image: Some("/Items/8/Images/Primary".into()),
            played: false,
            favorite: false,
            position_ms: 0,
            series_name: None,
            season: Some(2),
            episode: Some(1),
            parent: None,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
        };
        let mut c = controls();
        c.up_next = Some(UpNextCard::of(Some(&next), 7_200));
        let html = render(c);
        assert!(html.contains("Up next"), "{html}");
        assert!(html.contains("The Return"), "{html}");
        assert!(html.contains("S2 · E1"), "{html}");
        assert!(html.contains("Playing in 8 seconds"), "{html}");
        assert!(
            html.contains("Play now") && html.contains("Cancel"),
            "{html}"
        );
        assert!(html.contains("--pharos-fill: 52.0%"), "{html}");
        assert!(!html.contains("Skip intro"), "{html}");

        let plain = UpNextCard::of(None, 1_000);
        assert_eq!((plain.name.as_str(), plain.seconds), ("Next episode", 1));
        assert_eq!(plain.image, None);
    }

    #[test]
    fn no_skip_outside_a_segment_and_no_lone_audio_picker() {
        let mut c = controls();
        c.media.position_ms = 80_000;
        c.audio.truncate(1);
        let html = render(c);
        assert!(!html.contains("Skip intro"), "{html}");
        assert!(!html.contains(r#"aria-label="Audio""#), "{html}");
    }

    #[test]
    fn hovering_the_seek_bar_previews_the_thumbnail_and_chapter() {
        let mut c = controls();
        c.preview = Some(Trickplay {
            width: 320,
            height: 180,
            interval_ms: 10_000,
            thumb_count: 120,
            tile_count: 2,
            grid: 10,
            tile_url: "/videos/7/trickplay/320/{tile}.jpg".into(),
        });
        c.hover_ms = Some(310_000);
        let html = render(c);
        // Thumbnail 31: tile 0, row 3, column 1.
        assert!(
            html.contains("/videos/7/trickplay/320/0.jpg")
                && html.contains("background-position: -320px -540px"),
            "{html}"
        );
        assert!(html.contains("Act one") && html.contains("5:10"), "{html}");
    }
}
