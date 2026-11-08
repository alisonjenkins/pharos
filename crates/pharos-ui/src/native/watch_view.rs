//! The watch page as a pure component: the `<video>` and the controls drawn
//! over it (013 US2, US3). What the controls show comes in as [`Controls`];
//! what the viewer does goes out as [`Control`], which the player turns into
//! calls on the `<video>` or a new playback.

use dioxus::html::geometry::WheelDelta;
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use pharos_client::api::{Chapter, Item, ItemRef, Segment, Track, Trickplay};

use super::keymap::{self, Mode, Transport};
use super::keys;
use super::quality::{self, QualityView};

use super::art::Art;
use super::listen::{Icon, ICON_FLOAT, ICON_PIP};
use super::platform;
use super::playback::{
    clock, cues_at, left_to_watch, percent, segment_at, share, skip_label, thumb_at, track_label,
    ImageCues, MediaEvent, MediaState, SubtitleSize,
};
use super::seen::SeenRow;
use super::together::Member;
use super::together_view::{ActivityToast, InvitePanel, MemberStrip};
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

/// Two people: watch together.
const ICON_TOGETHER: &str = "M16 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm-8 0a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm0 2c-2.3 0-7 1.2-7 3.5V19h14v-2.5C15 14.2 10.3 13 8 13Zm8 0c-.3 0-.6 0-1 .1 1.2.8 2 2 2 3.4V19h6v-2.5c0-2.3-4.7-3.5-7-3.5Z";

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
/// How often an image subtitle manifest still being extracted is fetched
/// again; the server rewrites it every 2 s.
const IMAGE_CUES_POLL_MS: u32 = 3_000;
/// Fetch failures with no HTTP answer an image subtitle survives before it
/// gives up; an HTTP error (the extraction failed) ends it at once.
const IMAGE_CUES_RETRIES: u32 = 3;

#[cfg(test)]
thread_local! {
    /// How many times `WatchView`'s body has run, for the render-count test.
    static BODY_RENDERS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

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
    /// The playback position as a signal, when the owner updates it apart
    /// from `media`. The leaves that show the position read it, so a tick
    /// re-renders them alone and not the whole player (B815). Without it
    /// `media.position_ms` is the position.
    pub live_position: Option<Signal<u64>>,
    /// The quality control's state: the manual maximum, the auto cap and how
    /// the server plays the video.
    pub quality: QualityView,
    /// The subtitle text size, as the Playback page keeps it.
    pub subtitle_size: SubtitleSize,
}

/// Where playback is. Reading it with `now` in a component's body
/// subscribes that component to every tick; `peek` reads it for a handler.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    fixed: u64,
    live: Option<Signal<u64>>,
}

impl Position {
    pub fn of(c: &Controls) -> Self {
        Self {
            fixed: c.media.position_ms,
            live: c.live_position,
        }
    }

    pub fn at(ms: u64) -> Self {
        Self {
            fixed: ms,
            live: None,
        }
    }

    /// The position, subscribing the calling component to its changes.
    pub fn now(self) -> u64 {
        self.live.map_or(self.fixed, |s| s())
    }

    /// The position, without subscribing.
    pub fn peek(self) -> u64 {
        self.live.map_or(self.fixed, |s| *s.peek())
    }
}

/// `media` with the position it had when the viewer acted, not when the
/// component last rendered.
pub(super) fn media_at(media: MediaState, pos: Position) -> MediaState {
    MediaState {
        position_ms: pos.peek(),
        ..media
    }
}

/// `media` split into what changes the player's look and the position that
/// changes every tick: the player re-renders on the first only (B815).
pub fn split_position(media: MediaState) -> (MediaState, u64) {
    (
        MediaState {
            position_ms: 0,
            ..media
        },
        media.position_ms,
    )
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
    /// `left_ms` of a countdown `of_ms` long to run.
    pub fn of(next: Option<&Item>, left_ms: u64, of_ms: u64) -> Self {
        let run = of_ms.saturating_sub(left_ms.min(of_ms));
        Self {
            name: next.map_or_else(|| "Next episode".to_owned(), |n| n.name.clone()),
            number: next.and_then(episode_label),
            image: next.and_then(|n| n.image.clone()),
            seconds: left_ms.div_ceil(1_000),
            elapsed_pct: percent(run, of_ms),
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

/// Whether a key is not the player's to take: Ctrl or Cmd held, a shortcut
/// of the browser or the palette; or a key sequence (`g` …) under way,
/// whose next key is the key layer's (B617).
pub(super) fn not_transport(evt: &KeyboardEvent) -> bool {
    let m = evt.modifiers();
    m.ctrl() || m.meta() || keys::sequence_pending()
}

/// What a key does to the transport, on the video and music pages alike
/// (spec 029's Player keys): Space plays and pauses, the arrows or
/// h / l seek, the arrows or + / - set the volume, m mutes, a digit seeks
/// to that tenth of the way in.
pub(super) fn transport_key(key: &Key, media: &MediaState) -> Option<Control> {
    transport_control(keymap::transport(Mode::Player, &key.to_string())?, media)
}

/// The control for a transport key that needs nothing but the media; the
/// video's own keys (fullscreen, tracks) are its page's to answer.
pub(super) fn transport_control(t: Transport, media: &MediaState) -> Option<Control> {
    let volume = |v: f64| Control::Volume {
        volume: v.clamp(0.0, 1.0),
        muted: false,
    };
    Some(match t {
        Transport::TogglePlay => Control::TogglePlay,
        Transport::Mute => Control::Volume {
            volume: media.volume,
            muted: !media.muted,
        },
        Transport::SeekBack => Control::Seek(media.position_ms.saturating_sub(SEEK_STEP_MS)),
        Transport::SeekForward => Control::Seek(media.position_ms.saturating_add(SEEK_STEP_MS)),
        Transport::VolumeUp => volume(media.volume + VOLUME_STEP),
        Transport::VolumeDown => volume(media.volume - VOLUME_STEP),
        Transport::SeekTenths(n) if media.duration_ms > 0 => {
            Control::Seek(media.duration_ms / 10 * u64::from(n))
        }
        Transport::SeekTenths(_)
        | Transport::Fullscreen
        | Transport::NextSubtitle
        | Transport::NextAudio
        | Transport::SkipSegment => return None,
    })
}

/// The seek `s` makes: to the end of the segment whose skip button shows,
/// and nothing when no button shows (B805). "Up next" takes the button's
/// place over the credits, so it leaves `s` unbound too.
pub(super) fn skip_segment_control(
    segment: Option<Segment>,
    up_next_shown: bool,
) -> Option<Control> {
    if up_next_shown {
        return None;
    }
    segment.map(|seg| Control::Seek(seg.end_ms))
}

/// The subtitle after `pick` in `tracks`, then off, then the first again.
pub(super) fn next_subtitle(tracks: &[Track], pick: Option<u32>) -> Option<Control> {
    if tracks.is_empty() {
        return None;
    }
    let at = pick.and_then(|p| tracks.iter().position(|t| t.index == p));
    let next = match at {
        None => tracks.first(),
        Some(i) => tracks.get(i + 1),
    };
    Some(Control::Subtitle(next.map(|t| t.index)))
}

/// The audio track after `pick` in `tracks`, round to the first.
pub(super) fn next_audio(tracks: &[Track], pick: Option<u32>) -> Option<Control> {
    if tracks.len() < 2 {
        return None;
    }
    let at = pick.and_then(|p| tracks.iter().position(|t| t.index == p));
    let next = at.and_then(|i| tracks.get(i + 1)).or(tracks.first())?;
    Some(Control::Audio(next.index))
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
    /// Limit the video to this many bits per second (0 is Auto).
    Quality(u64),
    /// Draw subtitles at this size (the Playback page's choice).
    SubtitleSize(SubtitleSize),
    /// Forget the cap Auto lowered itself to.
    ResetAutoQuality,
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
    /// Start a group on this item at the current position, so others can
    /// join (014).
    StartTogether,
    /// Open (`true`) or close the invite panel.
    ShowInvite(bool),
    /// Put the group's invite link on the clipboard.
    CopyInvite,
}

/// The player. The engine gives the `<video>` its stream once mounted;
/// `on_media` hears the events progress reports follow, `on_control` the
/// viewer.
#[component]
pub fn WatchView(
    title: String,
    /// The line under the title: "S1 · E22 · Future Enemies", a film's year.
    #[props(default)]
    title_detail: Option<String>,
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
    /// Whether this item can start a group: the Watch together button shows
    /// while not in one.
    #[props(default)]
    can_start_together: bool,
    /// The group's invite link, once in a group that has one.
    #[props(default)]
    invite: Option<String>,
    /// Whether the invite panel shows (with `invite`).
    #[props(default)]
    invite_open: bool,
    /// What the last invite copy did, for the invite panel.
    #[props(default)]
    invite_note: Option<String>,
) -> Element {
    #[cfg(test)]
    BODY_RENDERS.with(|n| n.set(n.get() + 1));
    let in_group = !members.is_empty();
    let c = &controls;
    let total = c.total_ms();
    let media = c.media;
    let pos = Position::of(c);
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
    // The open invite panel holds the chrome up.
    let invite_open = invite_open && invite.is_some();
    let class = match (media.paused, idle() && !invite_open) {
        (true, _) => "pharos-player is-paused",
        (false, true) => "pharos-player is-idle",
        (false, false) => "pharos-player",
    };
    // The Rust subtitle renderer lifts its text above the control bar while
    // the bar shows (spec 026 US1 #3): paused, or awake and not idle.
    let controls_shown = media.paused || !(idle() && !invite_open);
    use_effect(use_reactive!(|controls_shown| {
        platform::set_subtitle_controls_visible(controls_shown);
    }));
    let shortcuts = use_shortcuts(media, c.previous, c.next, on_control).following(pos);
    let tracks = (
        c.subtitles.clone(),
        c.subtitle_pick,
        c.audio.clone(),
        c.audio_pick,
    );
    let segments = c.segments.clone();
    let up_next_shown = c.up_next.is_some();
    let on_key = move |evt: KeyboardEvent| {
        wake();
        // Ctrl/Cmd chords are the browser's and the palette's (Ctrl+K):
        // a bare `k` raises the volume, the chord must not. After `g`, `h`
        // goes home, not back 10 s.
        if not_transport(&evt) {
            return;
        }
        // The invite panel is a dialog: its keys are its own, and Escape
        // closes it.
        if invite_open {
            if evt.key() == Key::Escape {
                evt.prevent_default();
                on_control.call(Control::ShowInvite(false));
            }
            return;
        }
        // Shift+P / Shift+N are page-wide (B562), not the player's own.
        let control = match keymap::transport(Mode::Player, &evt.key().to_string()) {
            Some(Transport::Fullscreen) => Some(Control::Fullscreen),
            Some(Transport::NextSubtitle) => next_subtitle(&tracks.0, tracks.1),
            Some(Transport::NextAudio) => next_audio(&tracks.2, tracks.3),
            Some(Transport::SkipSegment) => {
                let from_ms = pos.peek();
                let control = skip_segment_control(segment_at(&segments, from_ms), up_next_shown);
                if let Some(Control::Seek(to_ms)) = control {
                    platform::info(&format!(
                        "pharos: skip by key from {from_ms} ms to {to_ms} ms"
                    ));
                }
                control
            }
            Some(t) => transport_control(t, &media_at(media, pos)),
            None => None,
        };
        let Some(control) = control else {
            return;
        };
        evt.prevent_default();
        on_control.call(control);
    };
    let text_tracks: Vec<(String, Track)> = c
        .subtitles
        .iter()
        .filter(|t| t.vtt_url.is_some())
        .map(|t| (text_track_id(t.index), t.clone()))
        .collect();
    // A list of at most one, so a new track's key mounts a new overlay.
    let image_cues: Option<String> = c
        .subtitles
        .iter()
        .find(|t| Some(t.index) == c.subtitle_pick)
        .and_then(|t| t.images_url.clone());

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
                        // Not `stalled`: Chrome fires it under MSE whenever appends pause
                        // for 3 s, while the video plays on.
                        onwaiting: move |_| on_media.call(MediaEvent::Waiting),
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
                for url in image_cues {
                    ImageSubtitles { key: "{url}", url: url.clone(), media, pos }
                }
                div { class: "pharos-watch-top",
                    button {
                        class: "pharos-control",
                        r#type: "button",
                        aria_label: "Back",
                        onclick: move |_| on_control.call(Control::Back),
                        "←"
                    }
                    div { class: "pharos-watch-heading",
                        h2 { class: "pharos-watch-title", "{title}" }
                        if let Some(detail) = title_detail {
                            p { class: "pharos-watch-title-detail", "{detail}" }
                        }
                    }
                    if can_start_together && !in_group {
                        button {
                            class: "pharos-control pharos-together-start",
                            r#type: "button",
                            title: "Start a group here so others can join",
                            onclick: move |_| on_control.call(Control::StartTogether),
                            Icon { path: ICON_TOGETHER }
                            span { class: "pharos-control-label", "Watch together" }
                        }
                    }
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
                    if in_group && invite.is_some() {
                        button {
                            class: "pharos-control pharos-invite-toggle",
                            r#type: "button",
                            aria_expanded: if invite_open { "true" } else { "false" },
                            onclick: move |_| on_control.call(Control::ShowInvite(!invite_open)),
                            Icon { path: ICON_TOGETHER }
                            span { class: "pharos-control-label", "Invite" }
                        }
                    }
                }
                if invite_open {
                    InvitePanel {
                        url: invite.clone().unwrap_or_default(),
                        note: invite_note,
                        on_copy: move |_| on_control.call(Control::CopyInvite),
                        on_close: move |_| on_control.call(Control::ShowInvite(false)),
                    }
                }
                ActivityToast { line: toast }
                if let Some(card) = c.up_next.clone() {
                    UpNext { card, on_control }
                } else {
                    SkipButton { segments: c.segments.clone(), pos, on_control }
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
                        TimeLabels { segments: c.segments.clone(), pos, total }
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
                        SubtitleSizePicker {
                            show: !c.subtitles.is_empty(),
                            size: c.subtitle_size,
                            on_pick: move |control| on_control.call(control),
                        }
                        QualityPicker {
                            quality: c.quality.clone(),
                            height: media.video_height,
                            on_pick: move |control| on_control.call(control),
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

/// The skip intro/credits button, while the position is in a skippable
/// span. It reads the position itself so a tick re-renders it alone.
#[component]
fn SkipButton(segments: Vec<Segment>, pos: Position, on_control: EventHandler<Control>) -> Element {
    let Some(seg) = segment_at(&segments, pos.now()) else {
        return rsx! {};
    };
    rsx! {
        button {
            class: "pharos-skip",
            r#type: "button",
            onclick: move |_| on_control.call(Control::Seek(seg.end_ms)),
            {skip_label(seg.kind)}
        }
    }
}

/// "12:03 / 45:00" and "Ends at 10:42 PM": the labels that move with the
/// position.
#[component]
fn TimeLabels(segments: Vec<Segment>, pos: Position, total: u64) -> Element {
    use_ends_at_refresh();
    let at = pos.now();
    let ends = ends_at(left_to_watch(&segments, at, total));
    rsx! {
        span { class: "pharos-time", "{clock(at)} / {clock(total)}" }
        if let Some(ends) = &ends {
            span { class: "pharos-time pharos-ends-at", "{ends}" }
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
                Art { src, sizes: "9rem", class: "pharos-up-next-art", lazy: false }
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
    let at = Position::of(c).now();
    let played = percent(at, total);
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
            aria_valuenow: at / 1000,
            aria_valuetext: clock(at),
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
                        // A tick already played dims, so the ones ahead stand out.
                        "data-passed": ch.start_ms <= at,
                    }
                }
            }
            span { class: "pharos-seek-knob", style: "left: {played}%" }
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
            onkeydown: slider_key,
            oninput: move |evt: FormEvent| {
                if let Ok(volume) = evt.value().parse::<f64>() {
                    on_control.call(Control::Volume { volume, muted: volume == 0.0 });
                }
            },
        }
    }
}

/// A key pressed on a range slider: the keys that move it (the arrows,
/// Home, End, Page Up and Page Down) are the slider's, not the player's
/// volume and seek keys; every other key goes on to the player, so `m`
/// still mutes after the slider is dragged (B621).
pub(super) fn slider_key(evt: KeyboardEvent) {
    if moves_a_control(&evt.key()) {
        evt.stop_propagation();
    }
}

/// A key pressed on a track menu: the keys that open it and move along
/// it are the menu's; every other key goes on to the player, so `m` still
/// mutes when the menu was opened and left without a pick (B621).
fn menu_key(evt: KeyboardEvent) {
    let key = evt.key();
    if moves_a_control(&key) || matches!(key, Key::Enter) || key.to_string() == " " {
        evt.stop_propagation();
    }
}

fn moves_a_control(key: &Key) -> bool {
    matches!(
        key,
        Key::ArrowLeft
            | Key::ArrowRight
            | Key::ArrowUp
            | Key::ArrowDown
            | Key::Home
            | Key::End
            | Key::PageUp
            | Key::PageDown
    )
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
            onkeydown: menu_key,
            onchange: move |evt: FormEvent| {
                on_change.call(evt.value() == "me");
                platform::focus_by_id(PLAYER_ID);
            },
            option { value: "follow", selected: !own_tracks, "Follow group" }
            option { value: "me", selected: own_tracks, "Just me" }
        }
    }
}

/// An image subtitle drawn over the picture (spec 019): the cues of the
/// manifest at `url`, fetched again while the server is still extracting
/// them. The canvas box matches the picture's `object-fit: contain` box, so
/// an anamorphic canvas stretches with the picture.
#[component]
fn ImageSubtitles(url: String, media: MediaState, pos: Position) -> Element {
    let mut cues = use_signal(ImageCues::default);
    use_future(move || {
        let url = url.clone();
        async move {
            let mut retries = IMAGE_CUES_RETRIES;
            loop {
                match platform::get_text(&url).await {
                    Ok(body) => match serde_json::from_str::<ImageCues>(&body) {
                        Ok(got) => {
                            let complete = got.complete;
                            cues.set(got);
                            if complete {
                                return;
                            }
                        }
                        Err(e) => {
                            platform::warn(&format!("pharos: image subtitles {url}: {e}"));
                            return;
                        }
                    },
                    Err((status, reason)) => {
                        platform::warn(&format!(
                            "pharos: image subtitles {url}: status={status:?} {reason}"
                        ));
                        if status.is_some() || retries == 0 {
                            return;
                        }
                        retries -= 1;
                    }
                }
                platform::sleep_ms(IMAGE_CUES_POLL_MS).await;
            }
        }
    });
    let cues = cues.read();
    let (w, h) = (cues.width, cues.height);
    if w == 0 || h == 0 {
        return rsx! {};
    }
    // The picture's display aspect; the canvas until the metadata is known.
    let (aw, ah) = match (media.video_width, media.video_height) {
        (0, _) | (_, 0) => (w, h),
        size => size,
    };
    rsx! {
        div { class: "pharos-image-subs",
            div {
                class: "pharos-image-subs-canvas",
                style: "width: min(100cqw, calc(100cqh * {aw} / {ah})); aspect-ratio: {aw} / {ah};",
                for cue in cues_at(&cues, pos.now()) {
                    img {
                        key: "{cue.url}",
                        class: "pharos-image-sub",
                        src: "{cue.url}",
                        alt: "",
                        style: "left: {share(cue.x, w)}%; top: {share(cue.y, h)}%; width: {share(cue.width, w)}%; height: {share(cue.height, h)}%;",
                    }
                }
            }
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
            onkeydown: menu_key,
            onchange: move |evt: FormEvent| {
                on_pick.call(evt.value().parse::<u32>().ok());
                // Once picked, the keys that move the picker go to the player again.
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

/// The subtitle size menu, beside the Subtitles menu: the sizes the Playback
/// page offers.
#[component]
fn SubtitleSizePicker(show: bool, size: SubtitleSize, on_pick: EventHandler<Control>) -> Element {
    if !show {
        return rsx! {};
    }
    rsx! {
        select {
            class: "pharos-track-picker pharos-subtitle-size-picker",
            aria_label: "Subtitle size",
            title: "Subtitle size",
            onkeydown: menu_key,
            onchange: move |evt: FormEvent| {
                on_pick.call(Control::SubtitleSize(SubtitleSize::parse(&evt.value())));
                platform::focus_by_id(PLAYER_ID);
            },
            for each in SubtitleSize::ALL {
                option {
                    key: "{each.as_str()}",
                    value: "{each.as_str()}",
                    selected: each == size,
                    "Size: {each.label()}"
                }
            }
        }
    }
}

/// The quality menu: Auto or a cap, shown as the state in force, with a
/// reset while Auto has lowered itself.
#[component]
fn QualityPicker(quality: QualityView, height: u32, on_pick: EventHandler<Control>) -> Element {
    let title = quality.title();
    rsx! {
        select {
            class: "pharos-track-picker pharos-quality-picker",
            aria_label: "Quality",
            title,
            onkeydown: menu_key,
            onchange: move |evt: FormEvent| {
                let value = evt.value();
                let control = if value == quality::RESET_AUTO {
                    Some(Control::ResetAutoQuality)
                } else {
                    value.parse::<u64>().ok().map(Control::Quality)
                };
                if let Some(control) = control {
                    on_pick.call(control);
                }
                platform::focus_by_id(PLAYER_ID);
            },
            for (value, text) in quality.options(height) {
                option {
                    key: "{value}",
                    value: "{value}",
                    selected: value.parse::<u64>().is_ok_and(|bps| bps == quality.manual),
                    "{text}"
                }
            }
        }
    }
}

/// A neighbour a Shift+click, Shift+P / Shift+N or a media key asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Skip {
    Previous,
    Next,
}

impl Skip {
    pub fn label(self) -> &'static str {
        match self {
            Skip::Previous => "previous",
            Skip::Next => "next",
        }
    }
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
    pos: Position,
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
        pos: Position::at(media.position_ms),
    }
}

impl Shortcuts {
    /// For a song, where Previous may start it again (B464/V406).
    pub fn for_song(mut self) -> Self {
        self.song = true;
        self
    }

    /// Seek relative to the live position, not the one `media` held at the
    /// last render.
    pub fn following(mut self, pos: Position) -> Self {
        self.pos = pos;
        self
    }

    pub fn wheel(mut self, evt: &WheelEvent) {
        evt.prevent_default();
        let (notches, rest) = wheel_notches(*self.carry.peek(), evt.delta());
        self.carry.set(rest);
        if let Some(control) = wheel_control(
            notches,
            evt.modifiers().shift(),
            media_at(self.media, self.pos),
        ) {
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

    thread_local! {
        static LIVE: std::cell::Cell<Option<Signal<u64>>> = const { std::cell::Cell::new(None) };
    }

    /// B815: a position tick re-renders the leaves that show it, not the
    /// player's body.
    #[test]
    fn a_position_tick_does_not_rerun_the_watch_view_body() {
        let mut dom = VirtualDom::new(|| {
            let live = use_signal(|| 0u64);
            LIVE.with(|l| l.set(Some(live)));
            let mut c = controls();
            (c.media, _) = split_position(c.media);
            c.live_position = Some(live);
            rsx! {
                WatchView {
                    title: "Pilot",
                    controls: c,
                    on_media: |_| {},
                    on_control: |_| {},
                }
            }
        });
        dom.rebuild_in_place();
        assert!(dioxus_ssr::render(&dom).contains("0:00 / 20:00"));
        // Let the mount's own state changes settle first.
        let _ = dom.render_immediate_to_vec();
        let renders = BODY_RENDERS.with(std::cell::Cell::get);
        dom.in_runtime(|| {
            let mut live = LIVE.with(std::cell::Cell::get).expect("signal");
            live.set(95_000);
        });
        let _ = dom.render_immediate_to_vec();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("1:35 / 20:00"), "{html}");
        assert_eq!(
            BODY_RENDERS.with(std::cell::Cell::get),
            renders,
            "WatchView's body must not re-run on a position tick"
        );
    }

    #[test]
    fn the_control_row_shows_the_quality_in_force_and_a_reset_when_lowered() {
        let mut c = controls();
        c.media.video_height = 720;
        c.quality = QualityView {
            auto: 2_000_000,
            method: "transcode".into(),
            ..QualityView::default()
        };
        let html = render(c);
        assert!(html.contains("pharos-quality-picker"), "{html}");
        assert!(html.contains("Auto · 720p · Transcode"), "{html}");
        assert!(html.contains("Reset Auto (now 2 Mbps)"), "{html}");
        let plain = render(controls());
        assert!(!plain.contains("Reset Auto"), "{plain}");
    }

    #[test]
    fn the_control_row_offers_the_subtitle_sizes_when_there_are_subtitles() {
        let mut c = controls();
        c.subtitle_size = SubtitleSize::Large;
        let html = render(c.clone());
        assert!(html.contains("pharos-subtitle-size-picker"), "{html}");
        for size in SubtitleSize::ALL {
            assert!(html.contains(&format!("Size: {}", size.label())), "{html}");
        }
        c.subtitles.clear();
        assert!(!render(c).contains("pharos-subtitle-size-picker"));
    }

    #[test]
    fn splitting_the_position_leaves_the_rest_of_the_media_alone() {
        let media = MediaState {
            position_ms: 9_000,
            duration_ms: 20_000,
            paused: false,
            ..MediaState::default()
        };
        let (rest, at) = split_position(media);
        assert_eq!(at, 9_000);
        assert_eq!(rest.position_ms, 0);
        assert_eq!((rest.duration_ms, rest.paused), (20_000, false));
        // Two ticks of one playback are the same to the player.
        let (later, _) = split_position(MediaState {
            position_ms: 9_250,
            ..media
        });
        assert_eq!(rest, later);
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

    /// B569/V482: the knob rides the end of the fill, and a tick the video
    /// has passed is marked so it dims.
    #[test]
    fn the_seek_bar_marks_passed_chapters_and_the_play_head() {
        let mut c = controls();
        c.media.position_ms = 600_000;
        let html = render(c);
        assert!(
            html.contains(r#"class="pharos-seek-knob" style="left: 50%""#),
            "{html}"
        );
        assert!(html.contains("data-passed=true"), "{html}");
        assert!(!html.contains("data-passed=false"), "{html}");
    }

    /// B750: the skip button and "Up next" overlap the top of the
    /// controls, which come later in the page; without a z-index above
    /// theirs the controls' gradient padding took the pointer from the lower
    /// half of the button. The seek preview, inside the controls, still
    /// rises over both while it shows.
    #[test]
    fn the_skip_button_and_up_next_stand_above_the_controls() {
        let sheet = include_str!("../../assets/pharos.css");
        let rule = |selector: &str| {
            sheet
                .split(&format!("\n{selector} {{"))
                .nth(1)
                .and_then(|r| r.split('}').next())
                .unwrap_or_default()
                .to_owned()
        };
        assert!(
            rule(".pharos-skip").contains("z-index: 1;"),
            "{}",
            rule(".pharos-skip")
        );
        assert!(
            rule(".pharos-up-next").contains("z-index: 1;"),
            "{}",
            rule(".pharos-up-next")
        );
        assert!(
            !rule(".pharos-controls").contains("z-index"),
            "{}",
            rule(".pharos-controls")
        );
        assert!(
            rule(".pharos-controls:has(.pharos-seek-preview)").contains("z-index: 2;"),
            "the seek preview must rise over the skip button"
        );
    }

    /// B805: the idle fade hides the top strip and the control bar but not
    /// the skip button, in the plain rule and in every `data-overlay-fx`
    /// variant, or a viewer who is just watching never sees it.
    #[test]
    fn idle_never_hides_the_skip_button() {
        let sheet = include_str!("../../assets/pharos.css");
        for line in sheet.lines().filter(|l| l.contains(".is-idle")) {
            assert!(
                !line.contains(".pharos-skip"),
                "an idle rule hides the skip button: {line}"
            );
        }
        assert!(sheet.contains(".pharos-player.is-idle .pharos-controls {"));
    }

    #[test]
    fn s_skips_only_the_segment_whose_button_shows() {
        let seg = Segment {
            kind: SegmentKind::Intro,
            start_ms: 10_000,
            end_ms: 70_000,
        };
        assert!(matches!(
            skip_segment_control(Some(seg), false),
            Some(Control::Seek(70_000))
        ));
        assert!(skip_segment_control(None, false).is_none());
        assert!(skip_segment_control(Some(seg), true).is_none());
        let media = MediaState::default();
        assert!(transport_control(Transport::SkipSegment, &media).is_none());
    }

    #[test]
    fn the_controls_offer_skip_tracks_and_chapters() {
        let html = render(controls());
        assert!(html.contains("Skip intro"), "{html}");
        assert!(html.contains("0:30 / 20:00"), "{html}");
        // One tick for "Act one" a quarter of the way along; none at 0.
        assert!(html.contains("left: 25%"), "{html}");
        assert_eq!(html.matches("pharos-seek-chapter\"").count(), 1, "{html}");
        // V482: "Act one" lies ahead of 0:30, so its tick is not yet passed.
        assert!(html.contains("data-passed=false"), "{html}");
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
        assert_eq!(
            ch("k"),
            Some(Control::Volume {
                volume: 0.6,
                muted: false
            })
        );
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

    /// Spec 029: the Vim keys mirror the arrows, and a digit seeks to that
    /// tenth of the way in once the duration is known.
    #[test]
    fn vim_and_digit_keys_drive_the_transport() {
        let media = MediaState {
            position_ms: 30_000,
            duration_ms: 100_000,
            volume: 0.5,
            ..MediaState::default()
        };
        let ch = |c: &str| transport_key(&Key::Character(c.into()), &media);
        assert_eq!(ch("h"), Some(Control::Seek(20_000)));
        assert_eq!(ch("l"), Some(Control::Seek(40_000)));
        assert_eq!(
            ch("-"),
            Some(Control::Volume {
                volume: 0.4,
                muted: false
            })
        );
        assert_eq!(
            ch("+"),
            Some(Control::Volume {
                volume: 0.6,
                muted: false
            })
        );
        assert_eq!(ch("0"), Some(Control::Seek(0)));
        assert_eq!(ch("7"), Some(Control::Seek(70_000)));
        let unknown = MediaState::default();
        assert_eq!(transport_key(&Key::Character("5".into()), &unknown), None);
        // The video's own keys are its page's, not the shared transport's.
        assert_eq!(ch("c"), None);
        assert_eq!(ch("a"), None);
    }

    fn tracks(indexes: &[u32]) -> Vec<Track> {
        indexes
            .iter()
            .map(|&index| Track {
                index,
                ..Track::default()
            })
            .collect()
    }

    #[test]
    fn c_steps_through_the_subtitles_and_off() {
        let subs = tracks(&[3, 5]);
        assert_eq!(next_subtitle(&subs, None), Some(Control::Subtitle(Some(3))));
        assert_eq!(
            next_subtitle(&subs, Some(3)),
            Some(Control::Subtitle(Some(5)))
        );
        assert_eq!(next_subtitle(&subs, Some(5)), Some(Control::Subtitle(None)));
        assert_eq!(next_subtitle(&[], None), None);
    }

    #[test]
    fn a_steps_round_the_audio_tracks() {
        let audio = tracks(&[1, 2]);
        assert_eq!(next_audio(&audio, Some(1)), Some(Control::Audio(2)));
        assert_eq!(next_audio(&audio, Some(2)), Some(Control::Audio(1)));
        assert_eq!(next_audio(&audio, None), Some(Control::Audio(1)));
        assert_eq!(next_audio(&audio[..1], Some(1)), None);
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

    /// Renders the player alone or in a group, with Watch together offered
    /// and an invite link when `invite`.
    fn render_together(in_group: bool, invite: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            move |(in_group, invite): (bool, bool)| {
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
                        can_start_together: true,
                        invite: invite.then(|| "http://h/ui/together/g1".to_owned()),
                    }
                }
            },
            (in_group, invite),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn render_together_open() -> String {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                WatchView {
                    title: "Pilot",
                    controls: controls(),
                    on_media: |_| {},
                    on_control: |_| {},
                    members: vec![Member {
                        id: "a".into(),
                        name: "Ada".into(),
                        buffering: false,
                        own_tracks: false,
                    }],
                    invite: Some("http://h/ui/together/g1".to_owned()),
                    invite_open: true,
                }
            }
        });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// V479: the player offers Watch together while alone, and Invite once
    /// in a group.
    #[test]
    fn the_player_offers_watch_together_alone_and_invite_in_a_group() {
        let alone = render_together(false, false);
        assert!(alone.contains("pharos-together-start"), "{alone}");
        assert!(alone.contains(">Watch together<"), "{alone}");
        assert!(!alone.contains("pharos-invite-toggle"), "{alone}");
        let grouped = render_together(true, true);
        assert!(!grouped.contains("pharos-together-start"), "{grouped}");
        assert!(grouped.contains("pharos-invite-toggle"), "{grouped}");
        assert!(grouped.contains(r#"aria-expanded="false""#), "{grouped}");
        // The panel shows only when asked for, never on a join by itself.
        assert!(!grouped.contains("pharos-invite-link"), "{grouped}");
        let open = render_together_open();
        assert!(open.contains("pharos-invite-link"), "{open}");
        assert!(open.contains(r#"aria-expanded="true""#), "{open}");
        // No link yet (the group's id has not come): nothing to invite with.
        assert!(!render_together(true, false).contains("pharos-invite-toggle"));
    }

    /// A song or a player that cannot start a group shows no button.
    #[test]
    fn watch_together_hides_when_the_player_cannot_start_a_group() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                WatchView {
                    title: "Pilot",
                    controls: controls(),
                    on_media: |_| {},
                    on_control: |_| {},
                }
            }
        });
        dom.rebuild_in_place();
        assert!(!dioxus_ssr::render(&dom).contains("pharos-together-start"));
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
            artist: None,
        };
        let mut c = controls();
        c.up_next = Some(UpNextCard::of(Some(&next), 7_200, 15_000));
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

        let plain = UpNextCard::of(None, 1_000, 15_000);
        assert_eq!((plain.name.as_str(), plain.seconds), ("Next episode", 1));
        assert_eq!(plain.image, None);
        // From a guess it counts to the end, and its bar fills over that.
        let long = UpNextCard::of(None, 45_000, 90_000);
        assert_eq!((long.seconds, long.elapsed_pct), (45, 50.0));
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
