//! The watch page as a pure component: the `<video>` and the controls drawn
//! over it (013 US2, US3). What the controls show comes in as [`Controls`];
//! what the viewer does goes out as [`Control`], which the player turns into
//! calls on the `<video>` or a new playback.

use dioxus::html::geometry::WheelDelta;
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use pharos_client::api::{Chapter, Damage, Item, ItemRef, Segment, Stereo, Track, Trickplay};

use super::bars;
use super::keymap::{self, Mode, Transport};
use super::keys;
use super::quality::{self, QualityView};
use super::scrub::{self, Scrub};
use super::seek_wait::SeekCard;
use super::stereo::{self, FrameRect, ViewMode};

use super::art::Art;
use super::damage::{in_span, PlayerDamageNote};
use super::listen::{
    Icon, QueuePanel, ICON_FLOAT, ICON_LEAVE, ICON_MUTED, ICON_PIP, ICON_QUEUE_NEXT, ICON_VOLUME,
    QUEUE_PANEL_ID,
};
use super::platform;
use super::playback::{
    clock, cues_at, left_to_watch, percent, position_at, segment_at, share, skip_label, thumb_at,
    track_label, ImageCues, MediaEvent, MediaState, SubtitleSize,
};
use super::queue::{Entry, QueueAction};
use super::seen::SeenRow;
use super::stage::{ICON_FULL, ICON_NEXT, ICON_PAUSE, ICON_PLAY, ICON_PREVIOUS, STAGE_EXPAND_ID};
use super::together::Member;
use super::together_view::{GroupNotes, InvitePanel, MemberStrip, NotesView};
use super::views::{ends_at, episode_label, use_ends_at_refresh};

/// The settings sheet of a narrow screen, and the button that opens it
/// (T038-302).
pub const SETTINGS_ID: &str = "pharos-player-settings";
const SETTINGS_TOGGLE_ID: &str = "pharos-player-settings-toggle";
/// Sliders: the settings button.
const ICON_SETTINGS: &str = "M3 17v2h6v-2H3zM3 5v2h10V5H3zm10 16v-2h8v-2h-8v-2h-2v6h2zM7 9v2H3v2h4v2h2V9H7zm14 4v-2H11v2h10zm-6-4h2V7h4V5h-4V3h-2v6z";

/// The `<video>` the playback engine attaches to.
pub const VIDEO_ID: &str = "pharos-video";
/// The `<audio>` of a song handing over to the next, which takes
/// [`VIDEO_ID`].
pub const OUTGOING_ID: &str = "pharos-video-out";
/// The element that goes fullscreen: the video with its controls.
pub const PLAYER_ID: &str = "pharos-player";
/// The skip intro/credits button, and Up Next's Play now: on a TV each
/// takes the remote's focus as it appears (B1342).
const SKIP_ID: &str = "pharos-skip";
const UP_NEXT_PLAY_ID: &str = "pharos-up-next-play";
/// Up Next's Cancel, the same id on the pill and the full card: the remote
/// on the pill's Cancel stays on Cancel as the pill grows into the card.
const UP_NEXT_CANCEL_ID: &str = "pharos-up-next-cancel";
/// The seek bar, whose width turns a pointer offset into a position.
pub const SEEK_ID: &str = "pharos-seek";

/// Two people: watch together.
const ICON_TOGETHER: &str = "M16 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm-8 0a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm0 2c-2.3 0-7 1.2-7 3.5V19h14v-2.5C15 14.2 10.3 13 8 13Zm8 0c-.3 0-.6 0-1 .1 1.2.8 2 2 2 3.4V19h6v-2.5c0-2.3-4.7-3.5-7-3.5Z";

/// How far the arrow keys and skip buttons seek.
pub(super) const SEEK_STEP_MS: u64 = 10_000;
/// Whether left and right preview a seek with its thumbnail and commit it
/// on OK or a pause (B907), as a TV remote wants, instead of seeking 10 s
/// at once as a keyboard does.
const SCRUB_KEYS: bool = cfg!(target_os = "android");
/// Whether the D-pad's spatial focus is the only way round the page, as on
/// a TV remote with no Tab or pointer (B1031): the compact player's picture
/// is then no focus stop, so the D-pad reaches the stage bar's buttons.
const REMOTE_FOCUS: bool = cfg!(target_os = "android");
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
    /// What is wrong with the file, when the integrity check found it
    /// damaged: its spans are marked on the seek bar.
    pub damage: Option<Damage>,
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
    /// Where a seek that has waited a while is going, and the way back
    /// (B1196); shown in place of the plain loading caption.
    pub seek_card: Option<SeekCard>,
    /// What the wait under the spinner is for (038 FR-401); empty reads as
    /// the generic "Loading".
    pub wait_caption: String,
    /// The browser refused to start the video: a tap starts it (038 FR-407).
    pub tap_to_start: bool,
    /// The playback position as a signal, when the owner updates it apart
    /// from `media`. The leaves that show the position read it, so a tick
    /// re-renders them alone and not the whole player (B815). Without it
    /// `media.position_ms` is the position.
    pub live_position: Option<Signal<u64>>,
    /// Where the media has data up to from the play head, as a signal the
    /// seek bar alone reads (T038-301); `None` draws no buffered range.
    pub live_buffered: Option<Signal<u64>>,
    /// The quality control's state: the manual maximum, the auto cap and how
    /// the server plays the video.
    pub quality: QualityView,
    /// The subtitle text size, as the Playback page keeps it.
    pub subtitle_size: SubtitleSize,
    /// The playing version's 3D packing, and how this device shows it.
    pub packing: Option<Stereo>,
    pub view_mode: ViewMode,
    /// Where the picture sits in the playing version's frame when the file
    /// has black bars baked into it (B1017): the player crops to it.
    pub picture: Option<bars::Crop>,
    /// Whether this browser can enter immersive VR (V923).
    pub vr: bool,
    /// The item's versions as (id, label), the primary's id empty; the
    /// Version control shows when there is more than one (spec 034 US3).
    pub versions: Vec<(String, String)>,
    /// The version playing.
    pub version: String,
    /// The speed the viewer picked; 0 (the default) is normal speed.
    pub speed: f64,
    /// How much later than the file the subtitles show, in ms.
    pub subtitle_delay_ms: i64,
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
    /// The small pill, not the card: the ending is a guess and more than
    /// the last 15 s are left (038 FR-405).
    pub pill: bool,
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
            pill: false,
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

/// What the remote's Back does in the player, nearest first: it drops an
/// open scrub preview (B907), then cancels Up Next (FR-604), and only then
/// leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackInPlayer {
    DropPreview,
    CancelUpNext,
    Leave,
}

pub(super) fn back_in_player(scrubbing: bool, up_next_shown: bool) -> BackInPlayer {
    if scrubbing {
        BackInPlayer::DropPreview
    } else if up_next_shown {
        BackInPlayer::CancelUpNext
    } else {
        BackInPlayer::Leave
    }
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

/// Whether the picture is a focus stop that takes the transport keys: always
/// in the full player, and in the compact one unless the D-pad's spatial
/// focus is the only way to its stage bar (B1031).
pub(super) fn picture_takes_keys(compact: bool, remote_focus: bool) -> bool {
    !(compact && remote_focus)
}

/// What a key does to a remote scrub (B907): left or right moves the
/// preview, OK or play commits it, Back drops it; any other key, and every
/// key while no preview is under way that is not left or right, passes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScrubKey {
    Press { forward: bool },
    Commit,
    Cancel,
    Pass,
}

pub(super) fn scrub_key(t: Option<Transport>, key: &Key, scrubbing: bool) -> ScrubKey {
    match t {
        Some(Transport::SeekForward) => return ScrubKey::Press { forward: true },
        Some(Transport::SeekBack) => return ScrubKey::Press { forward: false },
        _ => {}
    }
    if !scrubbing {
        return ScrubKey::Pass;
    }
    match key {
        Key::Named(NamedKey::Enter | NamedKey::MediaPlay | NamedKey::MediaPlayPause) => {
            ScrubKey::Commit
        }
        Key::Named(NamedKey::Escape | NamedKey::GoBack | NamedKey::BrowserBack) => ScrubKey::Cancel,
        _ if t == Some(Transport::TogglePlay) => ScrubKey::Commit,
        _ => ScrubKey::Pass,
    }
}

/// A control that moves playback itself, so an open scrub preview is
/// dropped rather than left to seek back to its target (B907).
fn drops_preview(control: &Control) -> bool {
    matches!(control, Control::Seek(_) | Control::ReturnFromSeek(_))
}

/// What the viewer asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Control {
    TogglePlay,
    Seek(u64),
    /// Give up on a seek that is still waiting and go back to where the
    /// viewer was (B1196). Not a [`Control::Seek`]: going back is not a new
    /// wait.
    ReturnFromSeek(u64),
    /// The pointer is `x` px into the seek bar; `None` when it left.
    Hover(Option<f64>),
    Volume {
        volume: f64,
        muted: bool,
    },
    Audio(u32),
    Subtitle(Option<u32>),
    Fullscreen,
    /// Limit the video to this many bits per second (0 is Auto).
    Quality(u64),
    /// Play at this speed, 1.0 being normal (T038-305).
    Speed(f64),
    /// Show subtitles this many ms later (negative: earlier) than the file
    /// says (T038-305).
    SubtitleDelay(i64),
    /// Play the version at this place in [`Controls::versions`] from here
    /// (spec 034 US3).
    Version(usize),
    /// Draw subtitles at this size (the Playback page's choice).
    SubtitleSize(SubtitleSize),
    /// Show a packed 3D version as one eye or the whole frame.
    ViewMode(ViewMode),
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
    /// Open (`true`) or close the video queue (V853).
    ShowQueue(bool),
    /// Something the viewer did in the video queue.
    Queue(QueueAction),
}

/// The video queue as the player shows it (V853): what waits, and whether
/// its panel is open.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoQueueView {
    pub entries: Vec<Entry>,
    pub shuffled: bool,
    pub open: bool,
}

impl VideoQueueView {
    /// Whether `next` is the queue's first, so Next plays from the queue
    /// rather than the next episode.
    pub fn plays_next(&self, next: Option<ItemRef>) -> bool {
        next.is_some() && self.entries.first().map(|e| e.id) == next
    }
}

/// The full player's queue button, for the focus to return to.
const QUEUE_TOGGLE_ID: &str = "pharos-watch-queue-toggle";

/// The queue button's name, with the count its badge shows.
pub fn video_queue_label(count: usize) -> String {
    match count {
        0 => "Queue".to_owned(),
        1 => "Queue, 1 video".to_owned(),
        n => format!("Queue, {n} videos"),
    }
}

/// The player. The engine gives the `<video>` its stream once mounted;
/// `on_media` hears the events progress reports follow, `on_control` the
/// viewer.
#[component]
pub fn WatchView(
    title: String,
    /// The player is the floating window or the mini bar (spec 020), whose
    /// stage bar carries the transport.
    #[props(default)]
    compact: bool,
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
    /// The connection, wait, status and undo notes stacked with the toast.
    #[props(default)]
    notes: NotesView,
    #[props(default)] on_undo_leave: EventHandler<()>,
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
    /// The video queue, where it can be used: not in a group, where the
    /// group picks what plays (V853).
    #[props(default)]
    queue: Option<VideoQueueView>,
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
    // Whether the last press on the player was a finger, the last tap on the
    // picture, and how many taps so far (a later one cancels a waiting
    // single tap): the double-tap seek (T038-303).
    let mut touched = use_signal(|| false);
    let mut last_tap = use_signal(|| None::<Tap>);
    let mut tap_count = use_signal(|| 0u32);
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
    use_context_provider(|| {
        Wake(Callback::new(move |()| {
            let mut wake = wake;
            wake();
        }))
    });
    // The open invite panel holds the chrome up, as the open queue does.
    let invite_open = invite_open && invite.is_some();
    let queue_open = queue.as_ref().is_some_and(|q| q.open);
    // The settings sheet a narrow screen keeps the menus in (T038-302).
    let mut sheet_open = use_signal(|| false);
    // A TV keeps every menu in the sheet (B1338); the remote lands on its
    // first one as it opens, not on the seek bar the D-pad would find.
    use_effect(move || {
        if sheet_open() && REMOTE_FOCUS {
            platform::info("pharos: player settings opened focus=first");
            platform::focus_first_in(SETTINGS_ID);
        }
    });
    let held = invite_open || queue_open || sheet_open();
    let next_queued = queue.as_ref().is_some_and(|q| q.plays_next(c.next));
    let class = match (media.paused, idle() && !held) {
        (true, _) => "pharos-player is-paused",
        (false, true) => "pharos-player is-idle",
        (false, false) => "pharos-player",
    };
    // The Rust subtitle renderer lifts its text above the control bar while
    // the bar shows (spec 026 US1 #3): paused, or awake and not idle.
    let controls_shown = media.paused || !(idle() && !held);
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
    // A remote scrub under way (B907): the seek bar previews its target and
    // the seek happens once, when it ends.
    let mut scrub = use_signal(|| None::<Scrub>);
    // A preview belongs to its item: one still open when the next episode
    // starts must not seek it to the old target.
    let item = (item_id, title.clone(), title_detail.clone());
    use_effect(use_reactive!(|item| {
        let _ = &item;
        let open = *scrub.peek();
        if let Some(s) = open {
            scrub.set(None);
            platform::info(&scrub::end_line(
                &s,
                scrub::End::Cancelled,
                platform::now_ms(),
            ));
        }
    }));
    let end_scrub = move |end: scrub::End| {
        let Some(s) = *scrub.peek() else {
            return;
        };
        scrub.set(None);
        platform::info(&scrub::end_line(&s, end, platform::now_ms()));
        if end != scrub::End::Cancelled {
            on_control.call(Control::Seek(s.target_ms));
        }
    };
    // The WebView app's Back is a history pop, not a key: while a preview
    // is open it drops the preview and stays in the player, as Escape does.
    // Up Next showing, Back cancels it before it leaves the player (FR-604).
    // Read at the press, not from the render the hook was made in.
    let up_next_live = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(false)));
    up_next_live.set(up_next_shown);
    let _back = use_hook(|| {
        SCRUB_KEYS.then(|| {
            let mut end_scrub = end_scrub;
            let up_next = std::rc::Rc::clone(&up_next_live);
            std::rc::Rc::new(platform::on_back(move || {
                match back_in_player(scrub.peek().is_some(), up_next.get()) {
                    BackInPlayer::DropPreview => end_scrub(scrub::End::Cancelled),
                    BackInPlayer::CancelUpNext => {
                        platform::info("pharos: up next cancelled via=back");
                        on_control.call(Control::CancelNext);
                    }
                    BackInPlayer::Leave => return false,
                }
                true
            }))
        })
    });
    // On a TV the compact picture hands the focus to the stage bar's Expand
    // button, and keeps none of the D-pad for itself (B1031): left and right
    // seeking it would leave the bar's buttons out of reach.
    let picture_keys = picture_takes_keys(compact, REMOTE_FOCUS);
    use_effect(use_reactive!(|compact| {
        if !picture_takes_keys(compact, REMOTE_FOCUS) {
            platform::info("pharos: stage remote focus to=expand reason=compact");
            platform::focus_out_of(PLAYER_ID, STAGE_EXPAND_ID);
        }
    }));
    let on_key = move |evt: KeyboardEvent| {
        if !picture_keys {
            return;
        }
        wake();
        // Ctrl/Cmd chords are the browser's and the palette's (Ctrl+K):
        // a bare `k` raises the volume, the chord must not. After `g`, `h`
        // goes home, not back 10 s.
        if not_transport(&evt) {
            return;
        }
        // Escape closes the settings sheet wherever the focus is: on its
        // button, or on the player after a pick.
        if *sheet_open.peek() && evt.key() == Key::Named(NamedKey::Escape) {
            let mut sheet_open = sheet_open;
            evt.prevent_default();
            sheet_open.set(false);
            platform::focus_by_id(SETTINGS_TOGGLE_ID);
            return;
        }
        // The invite panel is a dialog: its keys are its own, and Escape
        // closes it.
        if invite_open {
            if evt.key() == Key::Named(NamedKey::Escape) {
                evt.prevent_default();
                on_control.call(Control::ShowInvite(false));
            }
            return;
        }
        // Shift+P / Shift+N are page-wide (B562), not the player's own.
        let transport = keymap::transport(Mode::Player, &evt.key().to_string());
        if SCRUB_KEYS {
            let mut end_scrub = end_scrub;
            let scrubbing = scrub.peek().is_some();
            match scrub_key(transport, &evt.key(), scrubbing) {
                ScrubKey::Press { forward } => {
                    evt.prevent_default();
                    let now = platform::now_ms();
                    let prev = *scrub.peek();
                    let next = scrub::press(
                        prev,
                        forward,
                        evt.is_auto_repeating(),
                        pos.peek(),
                        total,
                        now,
                    );
                    scrub.set(Some(next));
                    spawn(async move {
                        platform::sleep_ms(scrub::SETTLE_MS).await;
                        if (*scrub.peek()).map(|s| s.last_press) == Some(now) {
                            end_scrub(scrub::End::Settled);
                        }
                    });
                    return;
                }
                ScrubKey::Commit => {
                    evt.prevent_default();
                    evt.stop_propagation();
                    end_scrub(scrub::End::Confirmed);
                    return;
                }
                ScrubKey::Cancel => {
                    evt.prevent_default();
                    evt.stop_propagation();
                    end_scrub(scrub::End::Cancelled);
                    return;
                }
                ScrubKey::Pass => {}
            }
        }
        let control = match transport {
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
        // Another seek (a digit, a skip) wins over an open preview: its
        // settle timer must not seek back to the old target.
        if SCRUB_KEYS && drops_preview(&control) {
            let mut end_scrub = end_scrub;
            end_scrub(scrub::End::Cancelled);
        }
        evt.prevent_default();
        on_control.call(control);
    };
    let text_tracks: Vec<(String, Track)> = c
        .subtitles
        .iter()
        .filter(|t| t.vtt_url.is_some())
        .map(|t| (text_track_id(t.index), t.clone()))
        .collect();
    // A packed 3D version on a flat screen shows one eye (V920); everything
    // that fits to the picture fits to that eye (V921).
    let crop = stereo::cropped(c.packing, c.view_mode);
    // Baked-in black bars are cropped away (B1017, V976), but never from a
    // 3D frame: its eye is the crop, and VR shows the whole frame.
    let cropped_bars = c.picture.filter(|_| c.packing.is_none());
    let bars = cropped_bars.map(|b| b.rect);
    let (eye_w, eye_h) = stereo::eye_size(crop, media.video_width, media.video_height);
    let (eye_w, eye_h) = bars::shown_size(bars, eye_w, eye_h);
    // Until the video knows its size, a cropped picture takes the shape the
    // version's own size gives, not a guess.
    let eye_aspect = (eye_w > 0 && eye_h > 0)
        .then(|| f64::from(eye_w) / f64::from(eye_h))
        .or(cropped_bars.map(|b| b.aspect));
    let video_style = match (crop, bars) {
        (Some(_), _) => eye_aspect.map(|a| format!("--pharos-eye-aspect: {a:.6}")),
        (None, Some(r)) => Some(match eye_aspect {
            Some(a) => format!("--pharos-eye-aspect: {a:.6}; {}", bars::crop_style(r)),
            None => bars::crop_style(r),
        }),
        (None, None) => None,
    };
    // A list of at most one, so a new track's key mounts a new overlay.
    // The headset's trigger plays and pauses through the player, so a group's
    // play stays the group's.
    let xr_select = use_coroutine(move |mut selects: UnboundedReceiver<()>| async move {
        use futures_util::StreamExt as _;
        while selects.next().await.is_some() {
            on_control.call(Control::TogglePlay);
        }
    });
    use_drop(platform::exit_vr);
    let vr =
        c.vr.then_some(c.packing)
            .flatten()
            .filter(|p| stereo::xr_layout(Some(*p)).is_some())
            .map(|packing| {
                let (w, h) = stereo::eye_size(c.packing, media.video_width, media.video_height);
                (
                    packing,
                    (stereo::VR_SCREEN_WIDTH_M, stereo::vr_screen_height(w, h)),
                )
            });
    let image_cues: Option<String> = c
        .subtitles
        .iter()
        .find(|t| Some(t.index) == c.subtitle_pick)
        .and_then(|t| t.images_url.clone());
    // A TV offers the delay only for image subtitles, which it draws itself.
    let offer_delay = c.subtitle_pick.is_some() && (!SCRUB_KEYS || image_cues.is_some());

    rsx! {
        section { class: "pharos-watch",
            div {
                id: PLAYER_ID,
                class,
                tabindex: if picture_keys { 0 } else { -1 },
                onkeydown: on_key,
                onpointermove: move |_| wake(),
                onpointerdown: move |evt: PointerEvent| {
                    let touch = evt.pointer_type() == "touch";
                    touched.set(touch);
                    woke_by_tap.set(touch && idle() && !media.paused);
                    wake();
                },
                onwheel: move |evt: WheelEvent| {
                    wake();
                    shortcuts.wheel_video(&evt, compact);
                },
                // The styled subtitle renderer lays its canvas after the
                // video, so the video sits alone where Dioxus never diffs.
                div {
                    class: "pharos-video-frame",
                    "data-packing": crop.map(stereo::packing_attr),
                    "data-crop": bars.map(bars::crop_attr),
                    video {
                        id: VIDEO_ID,
                        class: "pharos-video",
                        // On the video, not the frame: the frame's style
                        // carries the JASSUB lift the player sets.
                        style: video_style,
                        "data-item-id": item_id.map(|id| id.to_string()),
                        autoplay,
                        playsinline: true,
                        onclick: move |evt: MouseEvent| {
                            if *touched.peek() {
                                let now = platform::now_ms();
                                let zone = tap_zone(
                                    evt.element_coordinates().x,
                                    platform::element_width(VIDEO_ID),
                                );
                                let seek = double_tap(*last_tap.peek(), zone, now);
                                last_tap.set(Some(Tap { at: now, zone }));
                                // A second tap cancels the first's play or pause.
                                let tap = *tap_count.peek() + 1;
                                tap_count.set(tap);
                                if let Some(forward) = seek {
                                    woke_by_tap.set(false);
                                    let t = if forward { Transport::SeekForward } else { Transport::SeekBack };
                                    platform::info(&format!(
                                        "pharos: double tap seek forward={forward} at {} ms",
                                        pos.peek()
                                    ));
                                    if let Some(control) = transport_control(t, &media_at(media, pos)) {
                                        on_control.call(control);
                                    }
                                    return;
                                }
                                if woke_by_tap() {
                                    woke_by_tap.set(false);
                                    return;
                                }
                                if zone == TapZone::Middle {
                                    on_control.call(Control::TogglePlay);
                                    return;
                                }
                                // A side tap waits to see whether a second
                                // one makes it a seek.
                                spawn(async move {
                                    platform::sleep_ms(DOUBLE_TAP_MS as u32).await;
                                    if *tap_count.peek() == tap {
                                        on_control.call(Control::TogglePlay);
                                    }
                                });
                            } else if woke_by_tap() {
                                woke_by_tap.set(false);
                            } else if !shortcuts.click(&evt) {
                                on_control.call(Control::TogglePlay);
                            }
                        },
                        // A mouse double-click toggles fullscreen; a double
                        // tap is the seek above.
                        ondoubleclick: move |_| {
                            if !*touched.peek() {
                                on_control.call(Control::Fullscreen);
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
                                // Its cues arrive after the delay was set.
                                onload: move |_| platform::refresh_subtitle_delay(VIDEO_ID),
                            }
                        }
                    }
                }
                if c.tap_to_start {
                    button {
                        class: "pharos-tap-to-start pharos-button",
                        r#type: "button",
                        "data-tap-to-start": "true",
                        onclick: move |_| on_control.call(Control::TogglePlay),
                        span { class: "pharos-tap-to-start-icon", aria_hidden: "true", "▶" }
                        span { "Tap to start" }
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
                        match c.seek_card.clone() {
                            Some(card) => rsx! {
                                div { class: "pharos-seek-wait", "data-seek-wait": "true",
                                    p { class: "pharos-seek-wait-title", "{card.title}" }
                                    p { class: "pharos-seek-wait-time", "{card.waited}" }
                                    if let Some(reason) = card.reason {
                                        p { class: "pharos-seek-wait-reason", "{reason}" }
                                    }
                                    button {
                                        class: "pharos-seek-wait-back pharos-button is-on-video",
                                        r#type: "button",
                                        onclick: move |_| on_control.call(Control::ReturnFromSeek(card.back_to_ms)),
                                        "{card.back}"
                                    }
                                }
                            },
                            None if !c.wait_caption.is_empty() => rsx! {
                                span { class: "pharos-watch-loading-caption", "{c.wait_caption}" }
                            },
                            None => rsx! {},
                        }
                    }
                }
                for url in image_cues {
                    ImageSubtitles {
                        key: "{url}",
                        url: url.clone(),
                        picture: (eye_w, eye_h),
                        crop: bars,
                        pos,
                        delay_ms: c.subtitle_delay_ms,
                    }
                }
                div { class: "pharos-watch-top",
                    button {
                        class: "pharos-button pharos-button-quiet is-icon",
                        r#type: "button",
                        aria_label: "Back",
                        onclick: move |_| on_control.call(Control::Back),
                        Icon { path: ICON_LEAVE }
                    }
                    div { class: "pharos-watch-heading",
                        h2 { class: "pharos-watch-title", "{title}" }
                        if let Some(detail) = title_detail {
                            p { class: "pharos-watch-title-detail", "{detail}" }
                        }
                    }
                    if can_start_together && !in_group {
                        button {
                            class: "pharos-button pharos-button-quiet pharos-together-start",
                            r#type: "button",
                            title: "Start a group here so others can join",
                            // Its label is hidden on a phone or touch screen.
                            aria_label: "Watch together",
                            onclick: move |_| on_control.call(Control::StartTogether),
                            Icon { path: ICON_TOGETHER }
                            span { class: "pharos-control-label", "Watch together" }
                        }
                    }
                    if let Some(q) = queue.as_ref() {
                        {
                            let (count, open) = (q.entries.len(), q.open);
                            rsx! {
                                button {
                                    id: QUEUE_TOGGLE_ID,
                                    class: "pharos-button pharos-button-quiet is-icon pharos-queue-toggle",
                                    r#type: "button",
                                    "data-key-action": "queue",
                                    title: "Show or hide what plays next",
                                    aria_label: video_queue_label(count),
                                    aria_pressed: if open { "true" } else { "false" },
                                    aria_controls: open.then_some(QUEUE_PANEL_ID),
                                    onclick: move |_| on_control.call(Control::ShowQueue(!open)),
                                    Icon { path: ICON_QUEUE_NEXT }
                                    if count > 0 {
                                        span { class: "pharos-queue-count", aria_hidden: "true", "{count}" }
                                    }
                                }
                            }
                        }
                    }
                    if !members.is_empty() {
                        MemberStrip { members, on_leave, seen, own_tracks: own_track_members, activity }
                    }
                    if in_group && invite.is_some() {
                        button {
                            class: "pharos-button pharos-button-quiet pharos-invite-toggle",
                            aria_label: "Invite",
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
                if let Some(q) = queue.clone().filter(|q| q.open) {
                    div { class: "pharos-watch-queue",
                        QueuePanel {
                            queue: q.entries,
                            shuffled: q.shuffled,
                            videos: true,
                            on_queue: move |action| on_control.call(Control::Queue(action)),
                            // Escape closes the queue, as it does the invite
                            // panel, and puts the focus back on its button.
                            on_escape: move |()| {
                                on_control.call(Control::ShowQueue(false));
                                platform::focus_by_id(QUEUE_TOGGLE_ID);
                            },
                        }
                    }
                }
                GroupNotes {
                    connection: notes.connection.clone(),
                    wait: notes.wait.clone(),
                    status: notes.status.clone(),
                    undo: notes.undo,
                    on_undo: on_undo_leave,
                    toast,
                }
                if let Some(damage) = c.damage.clone() {
                    PlayerDamageNote { damage }
                }
                // The compact player says it in one line in its stage bar
                // (038 FR-406); the picture there is too small for a card.
                if let Some(card) = c.up_next.clone().filter(|_| !compact) {
                    if card.pill {
                        UpNextPill { card, on_control }
                    } else {
                        UpNext { card, on_control }
                    }
                } else if c.up_next.is_none() && !(REMOTE_FOCUS && sheet_open()) {
                    // On a TV the open settings panel stands where the skip
                    // card does; the card waits until it closes (B1338).
                    SkipButton { segments: c.segments.clone(), pos, on_control }
                }
                div { class: "pharos-controls",
                    SeekBar {
                        controls: controls.clone(),
                        total,
                        on_control,
                        scrub: Some(scrub),
                    }
                    div { class: "pharos-control-row",
                        button {
                            class: "pharos-button pharos-button-quiet is-icon",
                            r#type: "button",
                            aria_label: if media.paused { "Play" } else { "Pause" },
                            onclick: move |_| on_control.call(Control::TogglePlay),
                            Icon { path: if media.paused { ICON_PLAY } else { ICON_PAUSE } }
                        }
                        if let Some(id) = c.previous {
                            button {
                                class: "pharos-button pharos-button-quiet is-icon",
                                r#type: "button",
                                aria_label: "Previous episode",
                                onclick: move |_| on_control.call(Control::Play(id)),
                                Icon { path: ICON_PREVIOUS }
                            }
                        }
                        if let Some(id) = c.next {
                            button {
                                class: "pharos-button pharos-button-quiet is-icon",
                                r#type: "button",
                                aria_label: if next_queued { "Next in the queue" } else { "Next episode" },
                                onclick: move |_| on_control.call(Control::Play(id)),
                                Icon { path: ICON_NEXT }
                            }
                        }
                        TimeLabels { segments: c.segments.clone(), pos, total, scrub: Some(scrub) }
                        Volume { media, on_control, button_class: "pharos-button pharos-button-quiet is-icon" }
                        span { class: "pharos-control-spacer" }
                        // Speed and subtitle delay wait behind it; on a narrow
                        // screen or a TV the menus and window-size buttons do
                        // too (B1338).
                        button {
                            id: SETTINGS_TOGGLE_ID,
                            class: "pharos-button pharos-button-quiet is-icon pharos-settings-toggle",
                            r#type: "button",
                            aria_label: "Settings",
                            title: "Speed, subtitle delay and more",
                            aria_expanded: if sheet_open() { "true" } else { "false" },
                            aria_controls: SETTINGS_ID,
                            onclick: move |_| sheet_open.toggle(),
                            Icon { path: ICON_SETTINGS }
                        }
                        div {
                            id: SETTINGS_ID,
                            class: if sheet_open() { "pharos-control-menus is-open" } else { "pharos-control-menus" },
                            onkeydown: move |evt: KeyboardEvent| {
                                if sheet_open() && evt.key() == Key::Named(NamedKey::Escape) {
                                    evt.prevent_default();
                                    evt.stop_propagation();
                                    sheet_open.set(false);
                                    platform::focus_by_id(SETTINGS_TOGGLE_ID);
                                }
                            },
                        TrackMode { in_group, own_tracks, on_change: on_own_tracks }
                        TrackPicker {
                            label: "Audio",
                            tracks: c.audio.clone(),
                            pick: c.audio_pick,
                            offer_off: false,
                            caption: true,
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
                            caption: true,
                            on_pick: move |p| on_control.call(Control::Subtitle(p)),
                        }
                        SubtitleSizePicker {
                            show: !c.subtitles.is_empty(),
                            size: c.subtitle_size,
                            on_pick: move |control| on_control.call(control),
                        }
                        ViewModePicker {
                            show: stereo::cropped(c.packing, ViewMode::OneEye).is_some(),
                            mode: c.view_mode,
                            on_pick: move |control| on_control.call(control),
                        }
                        if let Some((packing, screen)) = vr {
                            button {
                                class: "pharos-button pharos-button-quiet pharos-watch-vr",
                                r#type: "button",
                                title: "Watch in VR",
                                // Called in the click itself: a VR session
                                // needs the click's user activation.
                                onclick: move |_| {
                                    platform::enter_vr(
                                        VIDEO_ID,
                                        packing,
                                        screen,
                                        std::rc::Rc::new(move || xr_select.send(())),
                                    );
                                },
                                "Watch in VR"
                            }
                        }
                        VersionPicker {
                            versions: c.versions.clone(),
                            version: c.version.clone(),
                            on_pick: move |control| on_control.call(control),
                        }
                        // Speed and delay are set now and then, so even a
                        // wide screen keeps them behind the settings button.
                        div { class: "pharos-control-extra",
                        SubtitleDelay {
                            // The TV app's text subtitles are its WebView's
                            // own, which take no delay; image ones do.
                            show: offer_delay,
                            delay_ms: c.subtitle_delay_ms,
                            on_pick: move |control| on_control.call(control),
                        }
                        // The TV app cannot set the rate yet.
                        if !SCRUB_KEYS {
                            SpeedPicker {
                                speed: c.speed,
                                in_group,
                                on_pick: move |control| on_control.call(control),
                            }
                        }
                        }
                        QualityPicker {
                            quality: c.quality.clone(),
                            height: media.video_height,
                            on_pick: move |control| on_control.call(control),
                        }
                        // The window-size controls sit together at the end of
                        // the row, smallest to largest: a small window, picture
                        // in picture, then fullscreen (B904).
                        button {
                            class: "pharos-button pharos-button-quiet is-icon pharos-shrink",
                            r#type: "button",
                            "data-key-action": "shrink",
                            aria_label: "Small window",
                            title: "Shrink to a small window",
                            onclick: move |_| on_control.call(Control::Shrink),
                            Icon { path: ICON_FLOAT }
                            span { class: "pharos-sheet-label", "Small window" }
                        }
                        if platform::pip_supported() {
                            button {
                                class: "pharos-button pharos-button-quiet is-icon",
                                r#type: "button",
                                aria_label: "Picture in picture",
                                title: "Picture in picture",
                                onclick: move |_| on_control.call(Control::PictureInPicture),
                                Icon { path: ICON_PIP }
                                span { class: "pharos-sheet-label", "Picture in picture" }
                            }
                        }
                        }
                        // The TV app always fills the screen (B1339).
                        if platform::fullscreen_supported() {
                            button {
                                class: "pharos-button pharos-button-quiet is-icon",
                                r#type: "button",
                                aria_label: "Fullscreen",
                                onclick: move |_| on_control.call(Control::Fullscreen),
                                Icon { path: ICON_FULL }
                            }
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
    let seg = segment_at(&segments, pos.now());
    // On a TV the button takes the remote's focus as it appears, and gives
    // it back to the picture as it goes (B1342).
    let span = seg.as_ref().map(|s| (s.start_ms, s.end_ms));
    use_effect(use_reactive!(|span| {
        if REMOTE_FOCUS {
            if span.is_some() {
                platform::focus_card(PLAYER_ID, SKIP_ID, "");
            } else {
                platform::refocus_if_lost(PLAYER_ID, false);
            }
        }
    }));
    use_drop(|| {
        if REMOTE_FOCUS {
            platform::refocus_if_lost(PLAYER_ID, false);
        }
    });
    let Some(seg) = seg else {
        return rsx! {};
    };
    rsx! {
        button {
            id: SKIP_ID,
            class: "pharos-skip pharos-button is-on-video",
            r#type: "button",
            onclick: move |_| on_control.call(Control::Seek(seg.end_ms)),
            {skip_label(seg.kind)}
        }
    }
}

/// "12:03 / 45:00" and "Ends at 10:42 PM": the labels that move with the
/// position, or with a remote scrub's target while one is open (B907).
#[component]
fn TimeLabels(
    segments: Vec<Segment>,
    pos: Position,
    total: u64,
    #[props(default)] scrub: Option<Signal<Option<Scrub>>>,
) -> Element {
    use_ends_at_refresh();
    let at = scrub
        .and_then(|s| s())
        .map_or_else(|| pos.now(), |s| s.target_ms);
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

/// "Up next" while the ending is a guess: one small line in the corner
/// that leaves the last scene in view (038 FR-405, V1221).
#[component]
fn UpNextPill(card: UpNextCard, on_control: EventHandler<Control>) -> Element {
    use_card_focus();
    rsx! {
        aside { class: "pharos-up-next-pill", aria_label: "Up next",
            span { class: "pharos-up-next-pill-name", "Up next: {card.name}" }
            button {
                id: UP_NEXT_PLAY_ID,
                class: "pharos-up-next-pill-play pharos-button",
                r#type: "button",
                onclick: move |_| on_control.call(Control::PlayNext),
                "Play now"
            }
            button {
                id: UP_NEXT_CANCEL_ID,
                class: "pharos-up-next-pill-cancel pharos-button is-on-video",
                r#type: "button",
                aria_label: "Cancel up next",
                onclick: move |_| on_control.call(Control::CancelNext),
                "Cancel"
            }
        }
    }
}

/// On a TV, Up Next's Play now takes the remote's focus as the card
/// appears (Cancel, if the remote was on the pill's Cancel), and the
/// picture takes it back when the card goes (B1342).
fn use_card_focus() {
    use_effect(|| {
        if REMOTE_FOCUS {
            platform::focus_card(PLAYER_ID, UP_NEXT_PLAY_ID, UP_NEXT_CANCEL_ID);
        }
    });
    use_drop(|| {
        if REMOTE_FOCUS {
            platform::refocus_if_lost(PLAYER_ID, true);
        }
    });
}

/// The next episode and a countdown to it, over the end of this one.
#[component]
fn UpNext(card: UpNextCard, on_control: EventHandler<Control>) -> Element {
    use_card_focus();
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
                        id: UP_NEXT_PLAY_ID,
                        class: "pharos-up-next-play pharos-button",
                        r#type: "button",
                        onclick: move |_| on_control.call(Control::PlayNext),
                        "Play now"
                    }
                    button {
                        id: UP_NEXT_CANCEL_ID,
                        class: "pharos-up-next-cancel pharos-button is-on-video",
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
///
/// A press on the bar, by mouse, pen or touch, starts a preview that follows
/// the pointer (captured, so it keeps following off the bar) and seeks once
/// when it lets go (T038-301): one seek per scrub, so one group command in a
/// group. A cancelled pointer (the browser took the touch for a gesture)
/// drops the preview without seeking.
#[component]
fn SeekBar(
    controls: Controls,
    total: u64,
    on_control: EventHandler<Control>,
    /// A remote scrub (B907) or a pointer drag: the knob and the preview sit
    /// at its target until it is committed. Read here, so a press re-renders
    /// the bar alone.
    #[props(default)]
    scrub: Option<Signal<Option<Scrub>>>,
) -> Element {
    let c = &controls;
    let mut own_scrub = use_signal(|| None::<Scrub>);
    let mut scrub = scrub.unwrap_or(own_scrub);
    // The pointer dragging the bar, while one is.
    let mut dragging = use_signal(|| None::<i32>);
    let scrub_ms = scrub().map(|s| s.target_ms);
    let pos = Position::of(c);
    let live = pos.now();
    let at = scrub_ms.unwrap_or(live);
    let played = percent(at, total);
    let buffered = c
        .live_buffered
        .map(|b| b())
        .filter(|b| *b > live)
        .map(|b| percent(b, total));
    let class = match (scrub_ms.is_some(), dragging().is_some()) {
        (true, true) => "pharos-seek is-dragging",
        (true, false) => "pharos-seek is-scrubbing",
        _ => "pharos-seek",
    };
    let hover = scrub_ms.or(c.hover_ms).map(|ms| {
        let chapter = c
            .chapters
            .iter()
            .rev()
            .find(|ch| ch.start_ms <= ms)
            .map(|ch| ch.name.clone())
            .filter(|n| !n.is_empty());
        let thumb = c.preview.as_ref().and_then(|set| thumb_at(set, ms));
        let damaged = c.damage.as_ref().is_some_and(|d| in_span(&d.ranges, ms));
        (percent(ms, total), clock(ms), chapter, thumb, damaged)
    });
    let preview_at = hover
        .as_ref()
        .map_or_else(String::new, |(at, _, _, thumb, _)| {
            preview_left(
                *at,
                thumb.as_ref().map(|t| t.width),
                class == "pharos-seek is-scrubbing",
            )
        });
    let damaged_spans = c
        .damage
        .as_ref()
        .map(|d| d.ranges.clone())
        .unwrap_or_default();
    let ms_at = move |x: f64| position_at(x, platform::element_width(SEEK_ID), total);
    let mut end_drag = move |end: scrub::End| {
        dragging.set(None);
        let open = *scrub.peek();
        scrub.set(None);
        own_scrub.set(None);
        if let Some(s) = open {
            platform::info(&scrub::end_line(&s, end, platform::now_ms()));
        }
        if let Some(control) = drag_end(open, end) {
            on_control.call(control);
        }
    };
    rsx! {
        div {
            id: SEEK_ID,
            class,
            role: "slider",
            // Focusable, so the arrow keys (the player's own seek steps)
            // reach it from Tab, and a TV's D-pad stops on it (B1330).
            tabindex: 0,
            aria_label: "Seek",
            aria_valuemin: 0,
            aria_valuemax: total / 1000,
            aria_valuenow: at / 1000,
            aria_valuetext: clock(at),
            onpointerdown: move |evt: PointerEvent| {
                // The main button only; a touch or pen press is always it.
                if evt.trigger_button() != Some(MouseButton::Primary) {
                    return;
                }
                let Some(ms) = ms_at(evt.element_coordinates().x) else {
                    return;
                };
                platform::capture_pointer(&evt);
                dragging.set(Some(evt.pointer_id()));
                let input = scrub::Input::of_pointer(&evt.pointer_type());
                scrub.set(Some(scrub::drag(None, input, ms, pos.peek(), platform::now_ms())));
            },
            onpointermove: move |evt: PointerEvent| {
                if *dragging.peek() == Some(evt.pointer_id()) {
                    let Some(ms) = ms_at(evt.element_coordinates().x) else {
                        return;
                    };
                    let prev = *scrub.peek();
                    let input = scrub::Input::of_pointer(&evt.pointer_type());
                    if prev.map(|s| s.target_ms) != Some(ms) {
                        scrub.set(Some(scrub::drag(prev, input, ms, pos.peek(), platform::now_ms())));
                    }
                } else if evt.pointer_type() == "mouse" {
                    on_control.call(Control::Hover(Some(evt.element_coordinates().x)));
                }
            },
            onpointerup: move |evt: PointerEvent| {
                if *dragging.peek() == Some(evt.pointer_id()) {
                    end_drag(scrub::End::Released);
                }
            },
            onpointercancel: move |evt: PointerEvent| {
                if *dragging.peek() == Some(evt.pointer_id()) {
                    end_drag(scrub::End::Cancelled);
                }
            },
            onpointerleave: move |evt: PointerEvent| {
                if evt.pointer_type() == "mouse" {
                    on_control.call(Control::Hover(None));
                }
            },
            div { class: "pharos-seek-track",
                for (i, s) in c.segments.iter().enumerate().filter(|(_, s)| s.end_ms > s.start_ms) {
                    span {
                        key: "{i}",
                        class: "pharos-seek-segment",
                        style: "left: {percent(s.start_ms, total)}%; width: {percent(s.end_ms, total) - percent(s.start_ms, total)}%",
                    }
                }
                if let Some(b) = buffered {
                    span { class: "pharos-seek-buffered", style: "width: {b}%" }
                }
                span { class: "pharos-seek-played", style: "width: {played}%" }
                for (i, s) in damaged_spans.iter().enumerate() {
                    span {
                        key: "damage-{i}",
                        class: "pharos-seek-damage",
                        style: "left: {percent(s.start_ms, total)}%; width: {percent(s.end_ms, total) - percent(s.start_ms, total)}%",
                    }
                }
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
            if let Some((_, time, chapter, thumb, damaged)) = hover {
                div { class: "pharos-seek-preview", style: "{preview_at}",
                    if let Some(t) = thumb {
                        div {
                            class: "pharos-seek-thumb",
                            style: "width: {t.width}px; height: {t.height}px; background-image: url('{t.url}'); background-position: -{t.x}px -{t.y}px",
                        }
                    }
                    if let Some(name) = chapter {
                        span { class: "pharos-seek-chapter-name", "{name}" }
                    }
                    if damaged {
                        span { class: "pharos-seek-damage-name", "Damaged" }
                    }
                    span { class: "pharos-seek-time", "{time}" }
                }
            }
        }
    }
}

/// B1277: where the seek preview's centre sits on the bar. It follows the
/// target but stops half its own width short of either end, so a thumbnail
/// near the start or the end of a film stays whole on screen instead of
/// hanging off the edge. A remote scrub draws the thumbnail at full size
/// (B907) and a pointer at half (`zoom: 0.5`), each with a 2px border; with
/// no thumbnail the time label still needs [`PREVIEW_LABEL_HALF_PX`]. The
/// half width also goes out as `--pharos-preview-half`, which caps a long
/// chapter name to the clamped width.
fn preview_left(at_percent: f64, thumb_width: Option<u32>, full_size: bool) -> String {
    let zoom = if full_size { 1.0 } else { 0.5 };
    let half = thumb_width
        .map(|w| (f64::from(w) + 4.0) * zoom / 2.0)
        .unwrap_or(0.0)
        .max(PREVIEW_LABEL_HALF_PX);
    format!(
        "left: clamp({half}px, {at_percent}%, calc(100% - {half}px)); \
         --pharos-preview-half: {half}px"
    )
}

/// Half the width of the widest time label ("10:00:00" at 0.85rem).
const PREVIEW_LABEL_HALF_PX: f64 = 32.0;

/// What a pointer drag on the seek bar does when it ends: one seek to its
/// target when the pointer lets go, nothing when the browser cancelled it.
/// The moves before it seek nothing, so a scrub is one seek, and in a group
/// one group command (T038-301).
fn drag_end(open: Option<Scrub>, end: scrub::End) -> Option<Control> {
    open.filter(|_| end == scrub::End::Released)
        .map(|s| Control::Seek(s.target_ms))
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
    let wake = try_use_context::<Wake>();
    if !platform::owns_volume() {
        return rsx! {};
    }
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
                None => rsx! {
                    Icon { path: if shown == 0.0 { ICON_MUTED } else { ICON_VOLUME } }
                },
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
            onkeydown: move |evt| slider_key(evt, wake),
            oninput: move |evt: FormEvent| {
                if let Ok(volume) = evt.value().parse::<f64>() {
                    on_control.call(Control::Volume { volume, muted: volume == 0.0 });
                }
            },
        }
    }
}

/// Brings the chrome back when a key lands on one of its controls. The
/// controls stop the keys they own from reaching the player, which is
/// where the chrome is woken, so they wake it first (B819).
#[derive(Clone, Copy)]
pub(super) struct Wake(Callback<()>);

/// Wakes, then stops the key from going on to the player when it `taken`.
fn route_key(taken: bool, wake: impl FnOnce(), stop: impl FnOnce()) {
    wake();
    if taken {
        stop();
    }
}

fn wake_with(wake: Option<Wake>) -> impl FnOnce() {
    move || {
        if let Some(Wake(wake)) = wake {
            wake.call(());
        }
    }
}

/// A key pressed on a range slider: the keys that move it (the arrows,
/// Home, End, Page Up and Page Down) are the slider's, not the player's
/// volume and seek keys; every other key goes on to the player, so `m`
/// still mutes after the slider is dragged (B621).
pub(super) fn slider_key(evt: KeyboardEvent, wake: Option<Wake>) {
    route_key(moves_a_control(&evt.key()), wake_with(wake), || {
        evt.stop_propagation()
    });
}

/// Whether a track menu keeps `key`: the keys that open it and move along it.
/// Once a menu is picked, the keys that move it go to the player again. On
/// a TV the menu sits in the open settings sheet, so the remote stays on it
/// (B1342): the picture would take OK while the sheet is still open.
fn after_pick() {
    if !REMOTE_FOCUS {
        platform::focus_by_id(PLAYER_ID);
    }
}

fn menu_takes(key: &Key) -> bool {
    moves_a_control(key) || matches!(key, Key::Named(NamedKey::Enter)) || key.to_string() == " "
}

/// A key pressed on a track menu: the keys that open it and move along
/// it are the menu's; every other key goes on to the player, so `m` still
/// mutes when the menu was opened and left without a pick (B621).
fn menu_key(evt: KeyboardEvent, wake: Option<Wake>) {
    route_key(menu_takes(&evt.key()), wake_with(wake), || {
        evt.stop_propagation();
    });
}

fn moves_a_control(key: &Key) -> bool {
    matches!(
        key,
        Key::Named(NamedKey::ArrowLeft)
            | Key::Named(NamedKey::ArrowRight)
            | Key::Named(NamedKey::ArrowUp)
            | Key::Named(NamedKey::ArrowDown)
            | Key::Named(NamedKey::Home)
            | Key::Named(NamedKey::End)
            | Key::Named(NamedKey::PageUp)
            | Key::Named(NamedKey::PageDown)
    )
}

/// "Follow group" or "Just me" for the track menus. Only in a group.
#[component]
fn TrackMode(in_group: bool, own_tracks: bool, on_change: EventHandler<bool>) -> Element {
    let wake = try_use_context::<Wake>();
    if !in_group {
        return rsx! {};
    }
    rsx! {
        SheetRow { caption: "Tracks",
            select {
                class: "pharos-track-picker pharos-track-mode",
                aria_label: "Tracks",
                title: "Tracks",
                onkeydown: move |evt| menu_key(evt, wake),
                onchange: move |evt: FormEvent| {
                    on_change.call(evt.value() == "me");
                    after_pick();
                },
                option { value: "follow", selected: !own_tracks, "Follow group" }
                option { value: "me", selected: own_tracks, "Just me" }
            }
        }
    }
}

/// An image subtitle drawn over the picture (spec 019): the cues of the
/// manifest at `url`, fetched again while the server is still extracting
/// them. The canvas box matches the picture's `object-fit: contain` box, so
/// an anamorphic canvas stretches with the picture. While the picture is
/// cropped to `crop` (B1017), each cue is moved onto it and kept inside it.
#[component]
fn ImageSubtitles(
    url: String,
    picture: (u32, u32),
    crop: Option<FrameRect>,
    pos: Position,
    /// The viewer's subtitle delay (T038-305).
    #[props(default)]
    delay_ms: i64,
) -> Element {
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
    // The picture's display aspect (one eye's, while cropped); the canvas
    // until the metadata is known.
    let (aw, ah) = match picture {
        (0, _) | (_, 0) => bars::shown_size(crop, w, h),
        size => size,
    };
    rsx! {
        div { class: "pharos-image-subs",
            div {
                class: "pharos-image-subs-canvas",
                style: "width: min(100cqw, calc(100cqh * {aw} / {ah})); aspect-ratio: {aw} / {ah};",
                for (cue, at) in cues_at(&cues, delayed(pos.now(), delay_ms)).map(|cue| {
                    let at = bars::cue_in_picture(
                        FrameRect {
                            x: share(cue.x, w) / 100.0,
                            y: share(cue.y, h) / 100.0,
                            w: share(cue.width, w) / 100.0,
                            h: share(cue.height, h) / 100.0,
                        },
                        crop,
                    );
                    (cue, at)
                }) {
                    img {
                        key: "{cue.url}",
                        class: "pharos-image-sub",
                        src: "{cue.url}",
                        alt: "",
                        style: "left: {at.x * 100.0}%; top: {at.y * 100.0}%; width: {at.w * 100.0}%; height: {at.h * 100.0}%;",
                    }
                }
            }
        }
    }
}

/// A `<select>` of tracks; nothing at all for an item with one audio track
/// or no subtitles.
#[component]
pub(super) fn TrackPicker(
    label: &'static str,
    tracks: Vec<Track>,
    pick: Option<u32>,
    offer_off: bool,
    on_pick: EventHandler<Option<u32>>,
    /// Captions the menu as a row of the player's settings sheet.
    #[props(default)]
    caption: bool,
) -> Element {
    let wake = try_use_context::<Wake>();
    let worth_showing = if offer_off {
        !tracks.is_empty()
    } else {
        tracks.len() > 1
    };
    if !worth_showing {
        return rsx! {};
    }
    let menu = rsx! {
        select {
            class: "pharos-track-picker",
            aria_label: label,
            title: label,
            onkeydown: move |evt| menu_key(evt, wake),
            onchange: move |evt: FormEvent| {
                on_pick.call(evt.value().parse::<u32>().ok());
                after_pick();
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
    };
    if caption {
        rsx! { SheetRow { caption: label, {menu} } }
    } else {
        menu
    }
}

/// One menu of the player's settings sheet, under its caption (B1358). In
/// the wide bar the row draws no box of its own and the caption is hidden,
/// unless `inline` keeps it beside the menu.
#[component]
fn SheetRow(caption: &'static str, #[props(default)] inline: bool, children: Element) -> Element {
    rsx! {
        div { class: "pharos-sheet-row",
            span {
                class: if inline { "pharos-sheet-caption is-inline" } else { "pharos-sheet-caption" },
                aria_hidden: "true",
                "{caption}"
            }
            {children}
        }
    }
}

/// The subtitle size menu, beside the Subtitles menu: the sizes the Playback
/// page offers.
#[component]
fn SubtitleSizePicker(show: bool, size: SubtitleSize, on_pick: EventHandler<Control>) -> Element {
    let wake = try_use_context::<Wake>();
    if !show {
        return rsx! {};
    }
    rsx! {
        SheetRow { caption: "Size", inline: true,
            select {
                class: "pharos-track-picker pharos-subtitle-size-picker",
                aria_label: "Subtitle size",
                title: "Subtitle size",
                onkeydown: move |evt| menu_key(evt, wake),
                onchange: move |evt: FormEvent| {
                    on_pick.call(Control::SubtitleSize(SubtitleSize::parse(&evt.value())));
                    after_pick();
                },
                for each in SubtitleSize::ALL {
                    option {
                        key: "{each.as_str()}",
                        value: "{each.as_str()}",
                        selected: each == size,
                        "{each.label()}"
                    }
                }
            }
        }
    }
}

/// The 3D menu, for a packed version: one eye, or the whole frame for a 3D
/// TV that unpacks it.
#[component]
fn ViewModePicker(show: bool, mode: ViewMode, on_pick: EventHandler<Control>) -> Element {
    let wake = try_use_context::<Wake>();
    if !show {
        return rsx! {};
    }
    rsx! {
        SheetRow { caption: "3D",
            select {
                class: "pharos-track-picker pharos-view-mode-picker",
                aria_label: "3D view",
                title: "3D view",
                onkeydown: move |evt| menu_key(evt, wake),
                onchange: move |evt: FormEvent| {
                    on_pick.call(Control::ViewMode(ViewMode::parse(&evt.value())));
                    after_pick();
                },
                for each in ViewMode::ALL {
                    option {
                        key: "{each.as_str()}",
                        value: "{each.as_str()}",
                        selected: each == mode,
                        "{each.label()}"
                    }
                }
            }
        }
    }
}

/// One step of the subtitle delay, and the most it moves either way.
pub const SUBTITLE_DELAY_STEP_MS: i64 = 100;
pub const SUBTITLE_DELAY_MAX_MS: i64 = 30_000;

/// The subtitle delay after `steps` steps from `delay_ms`, kept in range.
pub fn step_delay(delay_ms: i64, steps: i64) -> i64 {
    (delay_ms + steps * SUBTITLE_DELAY_STEP_MS).clamp(-SUBTITLE_DELAY_MAX_MS, SUBTITLE_DELAY_MAX_MS)
}

/// The file's time a subtitle shown at `at_ms` belongs to, with the
/// subtitles `delay_ms` late: a positive delay shows each cue later.
pub fn delayed(at_ms: u64, delay_ms: i64) -> u64 {
    if delay_ms >= 0 {
        at_ms.saturating_sub(delay_ms.unsigned_abs())
    } else {
        at_ms.saturating_add(delay_ms.unsigned_abs())
    }
}

/// "+0.3 s", "-1.2 s", "0.0 s".
pub fn delay_label(delay_ms: i64) -> String {
    let sign = match delay_ms {
        d if d > 0 => "+",
        d if d < 0 => "-",
        _ => "",
    };
    let tenths = delay_ms.unsigned_abs() / 100;
    format!("{sign}{}.{} s", tenths / 10, tenths % 10)
}

/// The subtitle delay, while a subtitle is on: earlier and later in 0.1 s
/// steps around the delay (T038-305).
#[component]
fn SubtitleDelay(show: bool, delay_ms: i64, on_pick: EventHandler<Control>) -> Element {
    if !show {
        return rsx! {};
    }
    let label = delay_label(delay_ms);
    rsx! {
        SheetRow { caption: "Delay",
            div {
                class: "pharos-subtitle-delay",
                role: "group",
                aria_label: "Subtitle delay",
                button {
                    class: "pharos-control",
                    r#type: "button",
                    aria_label: "Subtitles earlier",
                    title: "Subtitles 0.1 s earlier",
                    onclick: move |_| on_pick.call(Control::SubtitleDelay(step_delay(delay_ms, -1))),
                    "−"
                }
                button {
                    class: "pharos-subtitle-delay-value",
                    r#type: "button",
                    title: "Reset the subtitle delay",
                    aria_label: "Subtitle delay {label}, reset",
                    onclick: move |_| on_pick.call(Control::SubtitleDelay(0)),
                    "{label}"
                }
                button {
                    class: "pharos-control",
                    r#type: "button",
                    aria_label: "Subtitles later",
                    title: "Subtitles 0.1 s later",
                    onclick: move |_| on_pick.call(Control::SubtitleDelay(step_delay(delay_ms, 1))),
                    "+"
                }
            }
        }
    }
}

/// The speeds the speed menu offers.
pub const SPEEDS: [f64; 7] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// The speed menu's label for `speed`, under its "Speed" caption:
/// "Normal", "1.5×".
pub fn speed_label(speed: f64) -> String {
    if speed == 1.0 {
        "Normal".to_owned()
    } else {
        format!("{speed}×")
    }
}

/// The playback speed menu (T038-305). In a group the group's drift
/// correction owns the rate, so the menu shows normal speed and is off.
#[component]
fn SpeedPicker(speed: f64, in_group: bool, on_pick: EventHandler<Control>) -> Element {
    let wake = try_use_context::<Wake>();
    let speed = if in_group || speed <= 0.0 { 1.0 } else { speed };
    rsx! {
        SheetRow { caption: "Speed",
            select {
                class: "pharos-track-picker pharos-speed-picker",
                aria_label: "Speed",
                title: if in_group { "The group keeps everyone at normal speed" } else { "Speed" },
                disabled: in_group,
                onkeydown: move |evt| menu_key(evt, wake),
                onchange: move |evt: FormEvent| {
                    if let Ok(rate) = evt.value().parse::<f64>() {
                        on_pick.call(Control::Speed(rate));
                    }
                    after_pick();
                },
                for each in SPEEDS {
                    option {
                        key: "{each}",
                        value: "{each}",
                        selected: each == speed,
                        {speed_label(each)}
                    }
                }
            }
        }
    }
}

/// The version menu: each file the item plays from, the one playing chosen.
#[component]
fn VersionPicker(
    versions: Vec<(String, String)>,
    version: String,
    on_pick: EventHandler<Control>,
) -> Element {
    let wake = try_use_context::<Wake>();
    if versions.len() < 2 {
        return rsx! {};
    }
    rsx! {
        SheetRow { caption: "Version",
            select {
                class: "pharos-track-picker pharos-version-picker",
                aria_label: "Version",
                onkeydown: move |evt| menu_key(evt, wake),
                onchange: move |evt: FormEvent| {
                    if let Ok(i) = evt.value().parse::<usize>() {
                        on_pick.call(Control::Version(i));
                    }
                    after_pick();
                },
                for (i, (id, label)) in versions.into_iter().enumerate() {
                    option {
                        key: "{id}",
                        value: "{i}",
                        selected: id == version,
                        "{label}"
                    }
                }
            }
        }
    }
}

/// The quality menu: Auto or a cap, shown as the state in force, with a
/// reset while Auto has lowered itself.
#[component]
fn QualityPicker(quality: QualityView, height: u32, on_pick: EventHandler<Control>) -> Element {
    let wake = try_use_context::<Wake>();
    let title = quality.title();
    rsx! {
        SheetRow { caption: "Quality",
            select {
                class: "pharos-track-picker pharos-quality-picker",
                aria_label: "Quality",
                title,
                onkeydown: move |evt| menu_key(evt, wake),
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
                    after_pick();
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

    /// The video player's wheel (T038-303): only the full view takes it, so
    /// a page scrolled under the small player scrolls, and a trackpad's
    /// pixel travel is left to the page rather than summed into volume
    /// steps a two-finger scroll set off by accident.
    pub fn wheel_video(self, evt: &WheelEvent, compact: bool) {
        if compact || trackpad(evt.delta()) {
            return;
        }
        self.wheel(evt);
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

/// Whether a wheel event looks like a trackpad's: pixel travel of less than
/// half a notch, where a mouse wheel moves notches of about
/// [`WHEEL_NOTCH_PX`]. Line and page travel is a mouse's.
pub fn trackpad(delta: WheelDelta) -> bool {
    let WheelDelta::Pixels(v) = delta else {
        return false;
    };
    let along = if v.y.abs() >= v.x.abs() { v.y } else { v.x };
    // Size alone, not a fraction of a pixel: a mouse notch at a browser
    // zoom other than 100% arrives as e.g. 90.909 px, while a trackpad
    // sends many small steps.
    along.abs() < WHEEL_NOTCH_PX / 2.0
}

/// Which third of the picture a tap landed in, for the double-tap seek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapZone {
    Left,
    Middle,
    Right,
}

/// The third of a picture `width` px wide that `x` falls in.
pub fn tap_zone(x: f64, width: f64) -> TapZone {
    if width.is_nan() || width <= 0.0 || !x.is_finite() {
        return TapZone::Middle;
    }
    match x / width {
        f if f < 1.0 / 3.0 => TapZone::Left,
        f if f > 2.0 / 3.0 => TapZone::Right,
        _ => TapZone::Middle,
    }
}

/// A tap on the picture, kept to tell a double tap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tap {
    pub at: f64,
    pub zone: TapZone,
}

/// Taps closer than this are a double tap.
pub const DOUBLE_TAP_MS: f64 = 300.0;

/// What a touch tap on the picture does (T038-303): a second tap within
/// [`DOUBLE_TAP_MS`] on the same side third seeks 10 s that way, and each
/// further quick tap seeks again. `Some(false)` is back, `Some(true)`
/// forward; `None` is a single tap.
pub fn double_tap(prev: Option<Tap>, zone: TapZone, now: f64) -> Option<bool> {
    let prev = prev.filter(|p| p.zone == zone && (now - p.at) <= DOUBLE_TAP_MS)?;
    match prev.zone {
        TapZone::Left => Some(false),
        TapZone::Right => Some(true),
        TapZone::Middle => None,
    }
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
        assert!(
            html.contains(
                r#"<span aria-hidden="true" class="pharos-sheet-caption is-inline">Size</span>"#
            ),
            "the menu keeps its caption beside it in the wide bar (B1358): {html}"
        );
        for size in SubtitleSize::ALL {
            assert!(
                html.contains(&format!(">{}</option>", size.label())),
                "{html}"
            );
        }
        c.subtitles.clear();
        assert!(!render(c).contains("pharos-subtitle-size-picker"));
    }

    #[test]
    fn a_packed_version_is_cropped_to_one_eye_unless_both_eyes_is_picked() {
        let mut c = controls();
        c.media.video_width = 3840;
        c.media.video_height = 1080;
        c.packing = Some(Stereo::FullSideBySide);
        let html = render(c.clone());
        assert!(html.contains(r#"data-packing="fsbs""#), "{html}");
        // The eye is 16:9, not the frame's 32:9 (V920, V921).
        assert!(html.contains("--pharos-eye-aspect: 1.777778"), "{html}");
        assert!(html.contains("pharos-view-mode-picker"), "{html}");
        c.view_mode = ViewMode::BothEyes;
        let html = render(c.clone());
        assert!(!html.contains("data-packing"), "{html}");
        assert!(html.contains("pharos-view-mode-picker"), "{html}");
    }

    #[test]
    fn watch_in_vr_shows_only_for_a_split_packing_in_a_vr_browser() {
        let mut c = controls();
        c.packing = Some(Stereo::HalfTopAndBottom);
        assert!(
            !render(c.clone()).contains("Watch in VR"),
            "no WebXR, no button"
        );
        c.vr = true;
        assert!(render(c.clone()).contains("Watch in VR"));
        c.view_mode = ViewMode::BothEyes;
        assert!(render(c.clone()).contains("Watch in VR"), "any flat view");
        for packing in [None, Some(Stereo::Mvc)] {
            c.packing = packing;
            assert!(!render(c.clone()).contains("Watch in VR"), "{packing:?}");
        }
    }

    fn pillarboxed() -> bars::Crop {
        bars::Crop {
            rect: FrameRect {
                x: 0.125,
                y: 0.0,
                w: 0.75,
                h: 1.0,
            },
            aspect: 4.0 / 3.0,
        }
    }

    #[test]
    fn a_cropped_picture_has_its_shape_before_the_video_knows_its_size() {
        let mut c = controls();
        c.media.video_width = 0;
        c.media.video_height = 0;
        c.picture = Some(pillarboxed());
        let html = render(c);
        assert!(html.contains("--pharos-eye-aspect: 1.333333"), "{html}");
    }

    #[test]
    fn baked_in_black_bars_are_cropped_to_the_picture() {
        let mut c = controls();
        c.media.video_width = 1920;
        c.media.video_height = 1080;
        // Pillarboxed: a 4:3 picture pillarboxed in 1920x1080 (B1017).
        c.picture = Some(pillarboxed());
        let html = render(c.clone());
        assert!(
            html.contains(r#"data-crop="0.125000 0.000000 0.750000 1.000000""#),
            "{html}"
        );
        // The video takes the picture's 4:3 shape, not the frame's 16:9.
        assert!(html.contains("--pharos-eye-aspect: 1.333333"), "{html}");
        assert!(
            html.contains("--pharos-crop-transform: scale(1.333333"),
            "{html}"
        );
        c.picture = None;
        let html = render(c);
        assert!(!html.contains("data-crop"), "{html}");
        assert!(!html.contains("--pharos-crop"), "{html}");
    }

    #[test]
    fn a_3d_frame_is_never_cropped_for_bars() {
        let mut c = controls();
        c.media.video_width = 3840;
        c.media.video_height = 1080;
        c.packing = Some(Stereo::FullSideBySide);
        c.picture = Some(pillarboxed());
        let html = render(c.clone());
        assert!(!html.contains("data-crop"), "{html}");
        assert!(html.contains("--pharos-eye-aspect: 1.777778"), "{html}");
        c.view_mode = ViewMode::BothEyes;
        assert!(!render(c).contains("data-crop"), "the frame is shown whole");
    }

    #[test]
    fn a_2d_or_mvc_version_is_never_cropped_and_offers_no_3d_menu() {
        for packing in [None, Some(Stereo::Mvc)] {
            let mut c = controls();
            c.packing = packing;
            let html = render(c);
            assert!(!html.contains("data-packing"), "{packing:?}: {html}");
            assert!(
                !html.contains("pharos-view-mode-picker"),
                "{packing:?}: {html}"
            );
        }
    }

    #[test]
    fn a_key_a_control_keeps_still_wakes_the_chrome_first() {
        let order = std::cell::RefCell::new(Vec::new());
        route_key(
            true,
            || order.borrow_mut().push("wake"),
            || order.borrow_mut().push("stop"),
        );
        assert_eq!(*order.borrow(), ["wake", "stop"]);
        order.borrow_mut().clear();
        route_key(
            false,
            || order.borrow_mut().push("wake"),
            || order.borrow_mut().push("stop"),
        );
        assert_eq!(*order.borrow(), ["wake"], "a key left alone is not stopped");
    }

    #[test]
    fn a_menu_keeps_its_own_keys_and_a_slider_its_arrows() {
        for key in [
            Key::Named(NamedKey::ArrowDown),
            Key::Named(NamedKey::ArrowUp),
            Key::Named(NamedKey::Enter),
            Key::Character(" ".into()),
        ] {
            assert!(menu_takes(&key), "{key:?}");
        }
        assert!(!menu_takes(&Key::Character("m".into())));
        assert!(moves_a_control(&Key::Named(NamedKey::ArrowLeft)));
        assert!(!moves_a_control(&Key::Named(NamedKey::Enter)));
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

    /// Spec 034 US3: a title with a 3D version can be switched from the player.
    #[test]
    fn the_player_offers_the_version_menu_only_for_more_than_one() {
        let mut c = controls();
        assert!(!render(c.clone()).contains("aria-label=\"Version\""));
        c.versions = vec![
            (String::new(), "1080p · 4.2 GB".into()),
            ("v3d".into(), "3D (half side-by-side) · 1080p".into()),
        ];
        c.version = "v3d".into();
        let html = render(c);
        assert!(html.contains("aria-label=\"Version\""), "{html}");
        assert!(html.contains("value=\"1\" selected"), "{html}");
        assert!(html.contains("3D (half side-by-side)"));
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

    /// B940: a damaged file starts with a short note saying where, its spans
    /// are marked on the seek bar, and the preview names one under the pointer.
    #[test]
    fn a_damaged_file_is_noted_and_its_spans_marked() {
        use pharos_client::api::DamageSpan;
        let clean = render(controls());
        assert!(!clean.contains("pharos-player-damage"), "{clean}");
        assert!(!clean.contains("pharos-seek-damage"), "{clean}");

        let mut c = controls();
        c.damage = Some(Damage {
            verdict: "damaged".into(),
            ranges: vec![DamageSpan {
                start_ms: 300_000,
                end_ms: 600_000,
            }],
            ..Damage::default()
        });
        c.hover_ms = Some(310_000);
        let html = render(c.clone());
        assert!(
            html.contains("This file is damaged at 5:00–10:00. Playback may skip or stop there."),
            "{html}"
        );
        let total = c.runtime_ms;
        let left = percent(300_000, total);
        let width = percent(600_000, total) - left;
        assert!(
            html.contains(&format!(
                r#"class="pharos-seek-damage" style="left: {left}%; width: {width}%""#
            )),
            "{html}"
        );
        assert!(
            html.contains(r#"<span class="pharos-seek-damage-name">Damaged</span>"#),
            "{html}"
        );
        c.hover_ms = Some(900_000);
        assert!(!render(c).contains("pharos-seek-damage-name"));
    }

    /// T038-305: the speed menu.
    #[test]
    fn the_speed_menu_offers_the_speeds_and_is_off_in_a_group() {
        let mut c = controls();
        c.speed = 1.5;
        let html = render(c.clone());
        assert!(
            html.contains(r#"<option value="1.5" selected=true>1.5×</option>"#),
            "{html}"
        );
        assert!(
            html.contains(">Normal</option>") && html.contains(">2×</option>"),
            "{html}"
        );
        assert!(
            html.contains(r#"class="pharos-sheet-caption">Speed</span>"#),
            "{html}"
        );
        let speed_menu = |html: &str| {
            html.split(r#"aria-label="Speed""#)
                .nth(1)
                .and_then(|r| r.split("</select>").next())
                .unwrap_or_default()
                .to_owned()
        };
        assert!(!speed_menu(&html).contains("disabled"), "{html}");
        let mut dom = VirtualDom::new_with_props(
            |()| rsx! { SpeedPicker { speed: 1.5, in_group: true, on_pick: |_| {} } },
            (),
        );
        dom.rebuild_in_place();
        let grouped = dioxus_ssr::render(&dom);
        assert!(grouped.contains("disabled=true"), "{grouped}");
        assert!(
            grouped.contains(r#"<option value="1" selected=true>Normal</option>"#),
            "the group plays at normal speed: {grouped}"
        );
    }

    #[test]
    fn the_subtitle_delay_steps_a_tenth_and_moves_the_cues() {
        assert_eq!(step_delay(0, 1), 100);
        assert_eq!(step_delay(100, -3), -200);
        assert_eq!(step_delay(29_950, 1), 30_000, "kept in range");
        assert_eq!(step_delay(-30_000, -1), -30_000);
        assert_eq!(
            delayed(10_000, 500),
            9_500,
            "late subtitles show an earlier cue"
        );
        assert_eq!(delayed(10_000, -500), 10_500);
        assert_eq!(delayed(200, 500), 0);
        assert_eq!(delay_label(0), "0.0 s");
        assert_eq!(delay_label(300), "+0.3 s");
        assert_eq!(delay_label(-1_200), "-1.2 s");
        let mut c = controls();
        c.subtitle_delay_ms = 300;
        let html = render(c.clone());
        assert!(
            html.contains(r#"class="pharos-sheet-caption">Delay</span>"#)
                && html.contains(">+0.3 s</button>"),
            "{html}"
        );
        c.subtitle_pick = None;
        assert!(
            !render(c).contains("pharos-subtitle-delay"),
            "no subtitle, no delay"
        );
    }

    #[test]
    fn the_player_draws_its_glyphs_as_icons_not_text() {
        let html = render(controls());
        for glyph in ["▶", "❚❚", "⏮", "⏭", "⛶", "🔇", "🔊", "←"] {
            assert!(!html.contains(glyph), "{glyph} in {html}");
        }
        assert!(
            html.contains(ICON_PLAY) && html.contains(ICON_FULL),
            "{html}"
        );
    }

    #[test]
    fn the_menus_wait_behind_a_settings_button_the_sheet_opens() {
        let html = render(controls());
        assert!(
            html.contains(r#"aria-controls="pharos-player-settings""#)
                && html.contains(r#"aria-expanded="false""#)
                && html.contains(r#"class="pharos-control-menus""#),
            "{html}"
        );
        let menus = html
            .split(r#"id="pharos-player-settings""#)
            .nth(1)
            .unwrap_or_default();
        assert!(
            menus.contains(r#"aria-label="Subtitles""#) && menus.contains("Small window"),
            "the menus and the window-size buttons are in the sheet: {menus}"
        );
        let sheet = include_str!("../../assets/pharos.css");
        assert!(sheet.contains(".pharos-control-menus {\n  display: contents;"));
        assert!(sheet.contains(".pharos-control-menus.is-open {"));
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
        let bar = html.split(r#"id="pharos-seek""#).nth(1).unwrap_or_default();
        let open_tag = bar.split('>').next().unwrap_or_default();
        assert!(
            open_tag.contains(r#"tabindex=0"#) || open_tag.contains(r#"tabindex="0""#),
            "the seek bar takes the focus (V1209): {open_tag}"
        );
    }

    #[test]
    fn back_drops_a_preview_then_cancels_up_next_then_leaves() {
        assert_eq!(back_in_player(true, true), BackInPlayer::DropPreview);
        assert_eq!(back_in_player(false, true), BackInPlayer::CancelUpNext);
        assert_eq!(back_in_player(false, false), BackInPlayer::Leave);
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

    /// B1277 / V1240: the preview stops half its drawn width short of each
    /// end of the bar, full size on a remote scrub and half size under a
    /// pointer, and a bare time label still keeps clear of the edge.
    #[test]
    fn the_seek_preview_stays_inside_the_bar_at_either_end() {
        assert_eq!(
            preview_left(0.0, Some(316), true),
            "left: clamp(160px, 0%, calc(100% - 160px)); --pharos-preview-half: 160px"
        );
        assert_eq!(
            preview_left(99.5, Some(316), false),
            "left: clamp(80px, 99.5%, calc(100% - 80px)); --pharos-preview-half: 80px"
        );
        assert_eq!(
            preview_left(50.0, None, true),
            "left: clamp(32px, 50%, calc(100% - 32px)); --pharos-preview-half: 32px"
        );
    }

    /// B1277: on a TV the controls, and so the preview, keep to the
    /// title-safe area the rest of the ten-foot layout uses.
    #[test]
    fn on_a_tv_the_controls_keep_to_the_title_safe_area() {
        let sheet = include_str!("../../assets/pharos.css");
        let rule = sheet
            .split(":root[data-device=\"tv\"] .pharos-controls {")
            .nth(1)
            .and_then(|r| r.split('}').next())
            .unwrap_or_default();
        assert!(rule.contains("padding-inline: 48px;"), "{rule}");
        assert!(rule.contains("padding-bottom: 27px;"), "{rule}");
        let chrome = sheet
            .split(":root[data-device=\"tv\"] .pharos-up-next-pill {")
            .nth(1)
            .and_then(|r| r.split('}').next())
            .unwrap_or_default();
        assert!(chrome.contains("right: 48px;"), "{chrome}");
        assert!(
            sheet.contains(
                ":root[data-device=\"tv\"] .pharos-up-next-pill {\n  max-width: calc(100% - 96px);"
            ),
            "a long next-episode name must stay inside the left inset too"
        );
        assert!(
            sheet.contains(":root[data-device=\"tv\"] .pharos-watch-top {\n  padding: 27px 48px")
        );
        let name = sheet
            .split("\n.pharos-seek-chapter-name {")
            .nth(1)
            .and_then(|r| r.split('}').next())
            .unwrap_or_default();
        assert!(name.contains("var(--pharos-preview-half"), "{name}");
    }

    /// B1338 / V1309: on a TV the player keeps to two rows. The menus wait
    /// in the settings sheet, hidden until it opens, and the open sheet is a
    /// panel inside the title-safe area, not more rows of the bar.
    #[test]
    fn on_a_tv_the_menus_wait_in_the_settings_sheet() {
        let sheet = include_str!("../../assets/pharos.css");
        let rule = |head: &str| {
            sheet
                .split(&format!("\n{head} {{"))
                .nth(1)
                .and_then(|r| r.split('}').next())
                .unwrap_or_default()
                .to_owned()
        };
        let closed = rule(r#":root[data-device="tv"] .pharos-control-menus"#);
        assert!(closed.contains("display: none;"), "{closed}");
        let open = rule(r#":root[data-device="tv"] .pharos-control-menus.is-open"#);
        assert!(open.contains("position: absolute;"), "{open}");
        assert!(open.contains("right: 48px;"), "{open}");
        assert!(open.contains("bottom: 100%;"), "{open}");
        assert!(open.contains("flex-direction: column;"), "{open}");
        let label =
            rule(r#":root[data-device="tv"] .pharos-control-menus.is-open .pharos-sheet-label"#);
        assert!(label.contains("display: inline;"), "{label}");
        // The sheet is one of the overlays the remote's Back closes.
        assert!(super::super::tv_page::OVERLAYS.contains(".pharos-control-menus.is-open"));
    }

    /// B1358 / V1329: a touch screen keeps the menus in the settings sheet
    /// at any width, so a phone on its side keeps one bar, and every menu in
    /// the sheet is a captioned row. A TV keeps its own panel (B1338).
    #[test]
    fn on_a_touch_screen_the_menus_wait_in_the_sheet_at_any_width() {
        let sheet = include_str!("../../assets/pharos.css");
        // The block under `head` that holds `holding`: a media query can head
        // several blocks.
        let block = |head: &str, holding: &str| {
            sheet
                .split(&format!("\n{head} {{\n"))
                .skip(1)
                .filter_map(|r| r.split("\n}\n").next())
                .find(|b| b.contains(holding))
                .unwrap_or_default()
                .to_owned()
        };
        let touch = block(
            "@media (max-width: 40rem), (pointer: coarse)",
            "pharos-control-menus",
        );
        let not_tv = r#":root:not([data-device="tv"])"#;
        assert!(
            touch.contains(&format!(
                "{not_tv} .pharos-control-menus {{\n    display: none;"
            )),
            "{touch}"
        );
        assert!(
            touch.contains(&format!(
                "{not_tv} .pharos-control-menus.is-open {{\n    position: absolute;"
            )),
            "{touch}"
        );
        assert!(
            touch.contains(&format!(
                "{not_tv} .pharos-control-menus.is-open .pharos-sheet-caption {{\n    display: block;"
            )),
            "{touch}"
        );
        let volume = block("@media (pointer: coarse)", ".pharos-volume");
        assert!(
            volume.starts_with("  .pharos-volume {\n    display: none;"),
            "a phone on its side drops the volume slider too: {volume}"
        );
        let side = block(
            "@media (pointer: coarse) and (min-width: 40.0625rem) and (max-height: 32rem)",
            "pharos-control-menus",
        );
        assert!(side.contains("grid-template-columns: 1fr 1fr;"), "{side}");
        let html = render(controls());
        let menus = html
            .split(r#"id="pharos-player-settings""#)
            .nth(1)
            .unwrap_or_default();
        for caption in ["Audio", "Subtitles", "Size", "Delay", "Speed", "Quality"] {
            assert!(
                menus.contains(&format!(r#"pharos-sheet-caption">{caption}</span>"#))
                    || menus.contains(&format!(
                        r#"pharos-sheet-caption is-inline">{caption}</span>"#
                    )),
                "{caption} has its caption: {menus}"
            );
        }
    }

    /// B1359 / V1330: the player's buttons are the family's quiet pills
    /// (ADR 0120), round where they hold an icon, kept white on the video.
    /// Only the subtitle delay's text steps stay plain controls.
    #[test]
    fn the_player_buttons_are_the_family_pills() {
        let html = render(controls());
        let delay = html
            .split(r#"class="pharos-subtitle-delay""#)
            .nth(1)
            .and_then(|r| r.split("</div>").next())
            .unwrap_or_default();
        let rest = html.replacen(delay, "", 1);
        assert!(
            !rest.contains(r#"class="pharos-control""#)
                && !rest.contains(r#"class="pharos-control "#),
            "{rest}"
        );
        for label in [
            "Back",
            "Play",
            "Mute",
            "Settings",
            "Small window",
            "Fullscreen",
        ] {
            assert!(
                rest.contains(&format!(
                    r#"class="pharos-button pharos-button-quiet is-icon" type="button" aria-label="{label}""#
                )) || rest.contains(&format!(r#"aria-label="{label}""#))
                    && rest
                        .split(&format!(r#"aria-label="{label}""#))
                        .next()
                        .and_then(|before| before.rsplit("<button").next())
                        .is_some_and(|tag| tag.contains("pharos-button pharos-button-quiet is-icon")),
                "{label} is a round quiet pill: {rest}"
            );
        }
        let sheet = include_str!("../../assets/pharos.css");
        let on_video = sheet
            .split("\n.pharos-player .pharos-button-quiet {\n")
            .nth(1)
            .and_then(|r| r.split('}').next())
            .unwrap_or_default();
        assert!(on_video.contains("color: inherit;"), "{on_video}");
        assert!(
            sheet.contains(".pharos-player .pharos-button.is-icon {\n  flex-shrink: 0;"),
            "an icon button keeps its touch size on a narrow phone"
        );
    }

    /// B1363/V1334: the buttons drawn over the video take their look from
    /// the family's on-video kind; their own rules only place them. Disarm
    /// check: put `background: rgb(0 0 0 / 0.65);` back on `.pharos-skip`.
    #[test]
    fn on_video_buttons_take_their_look_from_the_family() {
        let sheet = include_str!("../../assets/pharos.css");
        let rule = |selector: &str| {
            sheet
                .split(&format!("\n{selector} {{"))
                .nth(1)
                .and_then(|r| r.split('}').next())
                .unwrap_or_default()
                .to_owned()
        };
        let on_video = rule(".pharos-button.is-on-video");
        assert!(on_video.contains("color: #fff;"), "{on_video}");
        for own in [
            ".pharos-skip",
            ".pharos-up-next-pill button",
            ".pharos-seek-wait-back",
            ".pharos-tap-to-start",
        ] {
            let body = rule(own);
            assert!(!body.is_empty(), "{own} has a rule");
            for look in ["background", "border", "color:", "cursor"] {
                assert!(!body.contains(look), "{own} sets {look}: {body}");
            }
        }
        assert!(!sheet.contains("\n.pharos-up-next-actions button {"));
        assert!(!sheet.contains("\n.pharos-playback-failure-actions button {"));
    }

    /// B838: on a phone the wrapped control row would sit under a button
    /// held at a fixed `bottom`, so the narrow layout moves it to the top.
    #[test]
    fn on_a_phone_the_skip_button_clears_the_wrapped_controls() {
        let sheet = include_str!("../../assets/pharos.css");
        let phone = sheet
            .split("@media (max-width: 40rem) {\n  .pharos-skip {")
            .nth(1)
            .and_then(|r| r.split('}').next())
            .unwrap_or_default();
        assert!(phone.contains("bottom: auto;"), "{phone}");
        assert!(phone.contains("top: calc("), "{phone}");
    }

    /// T038-301, V1208.
    #[test]
    fn a_drag_on_the_seek_bar_seeks_once_when_it_lets_go() {
        let mut open = None;
        let mut seeks = 0;
        for (i, ms) in (0..20u64).map(|i| (i, 10_000 + i * 5_000)) {
            open = Some(scrub::drag(open, scrub::Input::Touch, ms, 3_000, i as f64));
            // A move seeks nothing: only the end does.
            seeks += usize::from(open.is_none());
        }
        assert_eq!(seeks, 0);
        assert_eq!(
            drag_end(open, scrub::End::Released),
            Some(Control::Seek(105_000)),
            "one seek, to where the pointer let go"
        );
        assert_eq!(drag_end(open, scrub::End::Cancelled), None);
        assert_eq!(drag_end(None, scrub::End::Released), None);
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

    /// B1342 / V1313: the skip button carries the id a TV hands the
    /// remote's focus to as it appears; it is one of OK's presses too.
    #[test]
    fn the_skip_button_is_a_focus_target_for_the_remote() {
        let html = render(controls());
        assert!(
            html.contains(r#"<button class="pharos-skip pharos-button is-on-video" type="button" id="pharos-skip">"#),
            "{html}"
        );
        for id in [SKIP_ID, UP_NEXT_PLAY_ID] {
            assert!(
                super::super::tv_page::OK_PRESSES.contains(&format!(".{id}")),
                "{id}"
            );
        }
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
        assert_eq!(
            key(Key::Named(NamedKey::ArrowLeft)),
            Some(Control::Seek(20_000))
        );
        assert_eq!(
            key(Key::Named(NamedKey::ArrowRight)),
            Some(Control::Seek(40_000))
        );
        assert_eq!(
            key(Key::Named(NamedKey::ArrowUp)),
            Some(Control::Volume {
                volume: 0.6,
                muted: false
            })
        );
        assert_eq!(ch("x"), None);
        // Seeking back from the start stays at the start.
        let start = MediaState::default();
        assert_eq!(
            transport_key(&Key::Named(NamedKey::ArrowLeft), &start),
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

    /// V714: Shrink sits with Picture in picture and Fullscreen at the end of
    /// the bottom row, smallest first, not alone in the top bar.
    #[test]
    fn the_window_size_controls_are_grouped_smallest_first() {
        let html = render(controls());
        let top = html.find(r#"class="pharos-watch-top""#).expect("top bar");
        let row = html
            .find(r#"class="pharos-control-row""#)
            .expect("control row");
        let shrink = html.find(r#"aria-label="Small window""#).expect("shrink");
        let fullscreen = html.find(r#"aria-label="Fullscreen""#).expect("fullscreen");
        assert!(top < row && row < shrink && shrink < fullscreen, "{html}");
        let between = &html[shrink + 1..fullscreen];
        let controls_between = between.matches("aria-label=").count();
        // Only Picture in picture may come between them.
        assert!(controls_between <= 1, "{html}");
        if controls_between == 1 {
            assert!(
                between.contains(r#"aria-label="Picture in picture""#),
                "{html}"
            );
        }
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

    fn render_queue(queue: Option<VideoQueueView>, next: Option<ItemRef>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(queue, next): (Option<VideoQueueView>, Option<ItemRef>)| {
                let mut c = controls();
                c.next = next;
                rsx! {
                    WatchView {
                        title: "Pilot",
                        controls: c,
                        on_media: |_| {},
                        on_control: |_| {},
                        queue,
                    }
                }
            },
            (queue, next),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn queued(open: bool) -> VideoQueueView {
        let mut q = crate::native::queue::Queue::default();
        q.add(vec![crate::native::queue::Track {
            id: ItemRef::Real(8),
            name: "Film BB".to_owned(),
            runtime_ms: 10_200_000,
        }]);
        VideoQueueView {
            entries: q.upcoming().to_vec(),
            shuffled: false,
            open,
        }
    }

    /// V853: the player has a queue button counting the queued videos; the
    /// panel shows only once opened, and Next names the queue when it plays
    /// from it. In a group (no queue) there is no button.
    #[test]
    fn the_player_offers_the_video_queue() {
        let none = render_queue(None, None);
        assert!(!none.contains("pharos-queue-toggle"), "{none}");
        let closed = render_queue(Some(queued(false)), Some(ItemRef::Real(8)));
        assert!(
            closed.contains(r#"aria-label="Queue, 1 video""#),
            "{closed}"
        );
        assert!(closed.contains(r#"aria-pressed="false""#), "{closed}");
        assert!(!closed.contains(r#"id="pharos-queue""#), "{closed}");
        assert!(
            closed.contains(r#"aria-label="Next in the queue""#),
            "{closed}"
        );
        let open = render_queue(Some(queued(true)), Some(ItemRef::Real(2)));
        assert!(open.contains(r#"id="pharos-queue""#), "{open}");
        assert!(open.contains("Film BB"), "{open}");
        assert!(open.contains("1 video"), "{open}");
        assert!(
            !open.contains("pharos-mix"),
            "no mix bar for videos: {open}"
        );
        assert!(open.contains(r#"aria-label="Next episode""#), "{open}");
        // An open queue holds the chrome up, as the invite panel does.
        assert!(!open.contains("is-idle"), "{open}");
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
        // A touch screen hides both labels, so each keeps a name (B1359).
        assert!(alone.contains(r#"aria-label="Watch together""#), "{alone}");
        assert!(grouped.contains(r#"aria-label="Invite""#), "{grouped}");
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
        assert!(
            html.contains(r#"<div class="pharos-activity-toast""#),
            "{html}"
        );
        // The toast sits in the group notes, which sit outside the chrome.
        let toast = html
            .find(r#"<div class="pharos-watch-notes""#)
            .expect("notes rendered");
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
        assert!(
            !html.contains("pharos-watch-loading-caption"),
            "no cause, no caption: {html}"
        );
    }

    /// 038 FR-407, V1223: a refused autoplay offers a tap to start.
    #[test]
    fn a_refused_autoplay_offers_tap_to_start() {
        assert!(!render(controls()).contains("Tap to start"));
        let mut c = controls();
        c.tap_to_start = true;
        let html = render(c);
        assert!(html.contains("Tap to start"), "{html}");
        assert!(html.contains(r#"data-tap-to-start="true""#), "{html}");
    }

    /// 038 FR-401, V1218: the caption under the spinner is the wait's cause.
    #[test]
    fn the_loading_caption_names_the_wait() {
        let mut c = controls();
        c.media.loading = true;
        c.wait_caption = "Buffering…".to_owned();
        let html = render(c);
        assert!(html.contains("Buffering…"), "{html}");
        assert!(!html.contains("Waiting for the server…"), "{html}");
    }

    /// B1196/V1156: a slow seek's card replaces the plain caption, with
    /// where it is going, how long it has waited, why, and the way back.
    #[test]
    fn a_slow_seek_shows_where_it_is_going_and_the_way_back() {
        let mut c = controls();
        c.media.loading = true;
        c.seek_card = super::super::seek_wait::SeekWait::begin(None, 4_805_000, 5_730_000, 0)
            .card(12_000, true);
        let html = render(c);
        assert!(html.contains(r#"data-seek-wait="true""#), "{html}");
        assert!(html.contains("Jumping to 1:35:30…"), "{html}");
        assert!(html.contains("Waiting 12 s"), "{html}");
        assert!(html.contains("no seek index"), "{html}");
        assert!(html.contains("Go back to 1:20:05"), "{html}");
        assert!(!html.contains("pharos-watch-loading-caption"), "{html}");
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
    fn the_video_wheel_leaves_a_trackpad_to_the_page() {
        assert!(
            trackpad(WheelDelta::pixels(0.0, -4.0, 0.0)),
            "a trackpad's small steps"
        );
        assert!(
            trackpad(WheelDelta::pixels(0.0, 12.5, 0.0)),
            "a small fractional step"
        );
        assert!(
            !trackpad(WheelDelta::pixels(0.0, -100.0, 0.0)),
            "a mouse notch"
        );
        assert!(
            !trackpad(WheelDelta::pixels(0.0, 90.909, 0.0)),
            "a mouse notch at 110% zoom"
        );
        assert!(
            !trackpad(WheelDelta::pixels(120.0, 0.0, 0.0)),
            "Shift turns it sideways"
        );
        assert!(
            !trackpad(WheelDelta::lines(0.0, 1.0, 0.0)),
            "Firefox's mouse lines"
        );
        assert!(!trackpad(WheelDelta::pages(0.0, 1.0, 0.0)));
    }

    #[test]
    fn a_double_tap_on_a_side_third_seeks_that_way() {
        assert_eq!(tap_zone(50.0, 390.0), TapZone::Left);
        assert_eq!(tap_zone(195.0, 390.0), TapZone::Middle);
        assert_eq!(tap_zone(380.0, 390.0), TapZone::Right);
        assert_eq!(tap_zone(10.0, 0.0), TapZone::Middle, "no width, no side");
        let left = Some(Tap {
            at: 1_000.0,
            zone: TapZone::Left,
        });
        assert_eq!(double_tap(left, TapZone::Left, 1_250.0), Some(false));
        assert_eq!(
            double_tap(
                Some(Tap {
                    at: 0.0,
                    zone: TapZone::Right
                }),
                TapZone::Right,
                300.0
            ),
            Some(true)
        );
        assert_eq!(double_tap(left, TapZone::Left, 1_301.0), None, "too slow");
        assert_eq!(
            double_tap(left, TapZone::Right, 1_100.0),
            None,
            "the other side"
        );
        assert_eq!(
            double_tap(
                Some(Tap {
                    at: 0.0,
                    zone: TapZone::Middle
                }),
                TapZone::Middle,
                100.0
            ),
            None,
            "the middle third does not seek"
        );
        assert_eq!(double_tap(None, TapZone::Left, 0.0), None);
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
            stereo: None,
            damaged: false,
            new_children: 0,
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
        // B1342: Play now carries the id a TV hands the remote's focus to.
        assert!(html.contains(r#"id="pharos-up-next-play""#), "{html}");
        // The pill's Cancel and the card's share an id, so the remote stays
        // on Cancel as the pill grows into the card (B1342).
        assert!(html.contains(r#"id="pharos-up-next-cancel""#), "{html}");

        // 038 FR-405, V1221: over a guessed ending the pill shows instead,
        // and the compact player leaves it to its stage bar (FR-406).
        let mut pill = controls();
        let mut card = UpNextCard::of(Some(&next), 60_000, 90_000);
        card.pill = true;
        pill.up_next = Some(card.clone());
        let html = render(pill);
        assert!(
            html.contains("pharos-up-next-pill") && html.contains("Up next: The Return"),
            "{html}"
        );
        assert!(
            !html.contains("pharos-up-next-art") && !html.contains("Playing in"),
            "{html}"
        );
        assert!(html.contains(r#"id="pharos-up-next-play""#), "{html}");
        // The pill's Cancel and the card's share an id, so the remote stays
        // on Cancel as the pill grows into the card (B1342).
        assert!(html.contains(r#"id="pharos-up-next-cancel""#), "{html}");

        let mut compact = controls();
        compact.up_next = Some(card);
        let mut dom = VirtualDom::new_with_props(
            |controls: Controls| {
                rsx! {
                    WatchView { title: "Pilot", controls, compact: true, on_media: |_| {}, on_control: |_| {} }
                }
            },
            compact,
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(
            !html.contains("pharos-up-next"),
            "the stage bar says it: {html}"
        );

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

    /// B1031/V990: a TV's compact picture takes no keys and is no focus
    /// stop, so the D-pad reaches the mini bar's or floating window's
    /// Expand button instead of seeking. The full player keeps its keys, and
    /// a keyboard keeps them in the compact player too.
    #[test]
    fn a_remote_leaves_the_compact_picture_to_the_stage_bar() {
        assert!(picture_takes_keys(false, true), "full player on a TV");
        assert!(picture_takes_keys(false, false));
        assert!(picture_takes_keys(true, false), "compact with a keyboard");
        assert!(!picture_takes_keys(true, true), "compact on a TV");
    }

    #[test]
    fn remote_keys_drive_a_scrub_and_pass_the_rest_on() {
        let t = |k: &str| keymap::transport(Mode::Player, k);
        let right = Key::Named(NamedKey::ArrowRight);
        let left = Key::Named(NamedKey::ArrowLeft);
        let enter = Key::Named(NamedKey::Enter);
        let back = Key::Named(NamedKey::Escape);
        let space = Key::Character(" ".into());
        assert_eq!(
            scrub_key(t("ArrowRight"), &right, false),
            ScrubKey::Press { forward: true }
        );
        assert_eq!(
            scrub_key(t("ArrowLeft"), &left, true),
            ScrubKey::Press { forward: false }
        );
        // With no preview, OK, play and Back keep their usual jobs.
        for k in [&enter, &back, &space] {
            assert_eq!(
                scrub_key(t(&k.to_string()), k, false),
                ScrubKey::Pass,
                "{k}"
            );
        }
        assert_eq!(scrub_key(t("Enter"), &enter, true), ScrubKey::Commit);
        assert_eq!(scrub_key(t(" "), &space, true), ScrubKey::Commit);
        assert_eq!(scrub_key(t("Escape"), &back, true), ScrubKey::Cancel);
        let m = Key::Character("m".into());
        assert_eq!(scrub_key(t("m"), &m, true), ScrubKey::Pass);
    }

    #[test]
    fn a_scrub_previews_its_target_on_the_seek_bar() {
        let mut c = controls();
        c.media.position_ms = 60_000;
        c.preview = Some(Trickplay {
            width: 320,
            height: 180,
            interval_ms: 10_000,
            thumb_count: 120,
            tile_count: 2,
            grid: 10,
            tile_url: "/videos/7/trickplay/320/{tile}.jpg".into(),
        });
        let mut dom = VirtualDom::new_with_props(
            |controls: Controls| {
                let total = controls.total_ms();
                let scrub =
                    use_signal(|| Some(scrub::press(None, true, false, 300_000, total, 0.0)));
                rsx! {
                    SeekBar { controls, total, on_control: |_| {}, scrub: Some(scrub) }
                }
            },
            c,
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("pharos-seek is-scrubbing"), "{html}");
        assert!(
            html.contains("background-position: -320px -540px") && html.contains("5:10"),
            "the thumbnail and time are the target's, not playback's: {html}"
        );
        assert!(html.contains(r#"aria-valuetext="5:10""#), "{html}");
    }

    #[test]
    fn the_seek_bar_shows_what_is_buffered_ahead_of_the_play_head() {
        let draw = |buffered_ms: u64| {
            let mut c = controls();
            c.media.position_ms = 300_000;
            let mut dom = VirtualDom::new_with_props(
                move |mut controls: Controls| {
                    let total = controls.total_ms();
                    controls.live_buffered = Some(use_signal(|| buffered_ms));
                    rsx! {
                        SeekBar { controls, total, on_control: |_| {} }
                    }
                },
                c,
            );
            dom.rebuild_in_place();
            dioxus_ssr::render(&dom)
        };
        let html = draw(600_000);
        assert!(
            html.contains(r#"class="pharos-seek-buffered" style="width: 50%""#),
            "{html}"
        );
        let html = draw(200_000);
        assert!(
            !html.contains("pharos-seek-buffered"),
            "nothing ahead of the play head draws no range: {html}"
        );
    }

    #[test]
    fn a_scrub_shows_its_target_in_the_time_label() {
        let mut dom = VirtualDom::new(|| {
            let pos = Position::at(60_000);
            let scrub =
                use_signal(|| Some(scrub::press(None, true, false, 300_000, 3_600_000, 0.0)));
            rsx! {
                TimeLabels { segments: Vec::new(), pos, total: 3_600_000, scrub: Some(scrub) }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("5:10 / 1:00:00"), "{html}");
    }

    #[test]
    fn another_seek_drops_an_open_preview() {
        assert!(drops_preview(&Control::Seek(0)));
        assert!(!drops_preview(&Control::TogglePlay));
        assert!(!drops_preview(&Control::Fullscreen));
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
