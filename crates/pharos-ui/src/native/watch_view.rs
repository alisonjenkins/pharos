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
use super::listen::{Icon, QueuePanel, ICON_FLOAT, ICON_PIP, ICON_QUEUE_NEXT, QUEUE_PANEL_ID};
use super::platform;
use super::playback::{
    clock, cues_at, left_to_watch, percent, segment_at, share, skip_label, thumb_at, track_label,
    ImageCues, MediaEvent, MediaState, SubtitleSize,
};
use super::queue::{Entry, QueueAction};
use super::seen::SeenRow;
use super::stage::STAGE_EXPAND_ID;
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
    let held = invite_open || queue_open;
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
    let _back = use_hook(|| {
        SCRUB_KEYS.then(|| {
            let mut end_scrub = end_scrub;
            std::rc::Rc::new(platform::on_back(move || {
                if scrub.peek().is_none() {
                    return false;
                }
                end_scrub(scrub::End::Cancelled);
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

    rsx! {
        section { class: "pharos-watch",
            div {
                id: PLAYER_ID,
                class,
                tabindex: if picture_keys { 0 } else { -1 },
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
                        match c.seek_card.clone() {
                            Some(card) => rsx! {
                                div { class: "pharos-seek-wait", "data-seek-wait": "true",
                                    p { class: "pharos-seek-wait-title", "{card.title}" }
                                    p { class: "pharos-seek-wait-time", "{card.waited}" }
                                    if let Some(reason) = card.reason {
                                        p { class: "pharos-seek-wait-reason", "{reason}" }
                                    }
                                    button {
                                        class: "pharos-seek-wait-back",
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
                    if let Some(q) = queue.as_ref() {
                        {
                            let (count, open) = (q.entries.len(), q.open);
                            rsx! {
                                button {
                                    id: QUEUE_TOGGLE_ID,
                                    class: "pharos-control pharos-queue-toggle",
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
                ActivityToast { line: toast }
                if let Some(damage) = c.damage.clone() {
                    PlayerDamageNote { damage }
                }
                if let Some(card) = c.up_next.clone() {
                    UpNext { card, on_control }
                } else {
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
                                aria_label: if next_queued { "Next in the queue" } else { "Next episode" },
                                onclick: move |_| on_control.call(Control::Play(id)),
                                "⏭"
                            }
                        }
                        TimeLabels { segments: c.segments.clone(), pos, total, scrub: SCRUB_KEYS.then_some(scrub) }
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
                        ViewModePicker {
                            show: stereo::cropped(c.packing, ViewMode::OneEye).is_some(),
                            mode: c.view_mode,
                            on_pick: move |control| on_control.call(control),
                        }
                        if let Some((packing, screen)) = vr {
                            button {
                                class: "pharos-control pharos-watch-vr",
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
                        QualityPicker {
                            quality: c.quality.clone(),
                            height: media.video_height,
                            on_pick: move |control| on_control.call(control),
                        }
                        // The window-size controls sit together at the end of
                        // the row, smallest to largest: a small window, picture
                        // in picture, then fullscreen (B904).
                        button {
                            class: "pharos-control pharos-shrink",
                            r#type: "button",
                            "data-key-action": "shrink",
                            aria_label: "Shrink",
                            title: "Shrink to a small window",
                            onclick: move |_| on_control.call(Control::Shrink),
                            Icon { path: ICON_FLOAT }
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
fn SeekBar(
    controls: Controls,
    total: u64,
    on_control: EventHandler<Control>,
    /// A remote scrub (B907): the knob and the preview sit at its target
    /// until it is committed. Read here, so a press re-renders the bar alone.
    #[props(default)]
    scrub: Option<Signal<Option<Scrub>>>,
) -> Element {
    let c = &controls;
    let scrub_ms = scrub.and_then(|s| s()).map(|s| s.target_ms);
    let live = Position::of(c).now();
    let at = scrub_ms.unwrap_or(live);
    let played = percent(at, total);
    let class = if scrub_ms.is_some() {
        "pharos-seek is-scrubbing"
    } else {
        "pharos-seek"
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
    let damaged_spans = c
        .damage
        .as_ref()
        .map(|d| d.ranges.clone())
        .unwrap_or_default();
    rsx! {
        div {
            id: SEEK_ID,
            class,
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
            if let Some((at, time, chapter, thumb, damaged)) = hover {
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
                    if damaged {
                        span { class: "pharos-seek-damage-name", "Damaged" }
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
    let wake = try_use_context::<Wake>();
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
        select {
            class: "pharos-track-picker pharos-track-mode",
            aria_label: "Tracks",
            title: "Tracks",
            onkeydown: move |evt| menu_key(evt, wake),
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
/// an anamorphic canvas stretches with the picture. While the picture is
/// cropped to `crop` (B1017), each cue is moved onto it and kept inside it.
#[component]
fn ImageSubtitles(
    url: String,
    picture: (u32, u32),
    crop: Option<FrameRect>,
    pos: Position,
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
                for (cue, at) in cues_at(&cues, pos.now()).map(|cue| {
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
    rsx! {
        select {
            class: "pharos-track-picker",
            aria_label: label,
            title: label,
            onkeydown: move |evt| menu_key(evt, wake),
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
    let wake = try_use_context::<Wake>();
    if !show {
        return rsx! {};
    }
    rsx! {
        select {
            class: "pharos-track-picker pharos-subtitle-size-picker",
            aria_label: "Subtitle size",
            title: "Subtitle size",
            onkeydown: move |evt| menu_key(evt, wake),
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

/// The 3D menu, for a packed version: one eye, or the whole frame for a 3D
/// TV that unpacks it.
#[component]
fn ViewModePicker(show: bool, mode: ViewMode, on_pick: EventHandler<Control>) -> Element {
    let wake = try_use_context::<Wake>();
    if !show {
        return rsx! {};
    }
    rsx! {
        select {
            class: "pharos-track-picker pharos-view-mode-picker",
            aria_label: "3D view",
            title: "3D view",
            onkeydown: move |evt| menu_key(evt, wake),
            onchange: move |evt: FormEvent| {
                on_pick.call(Control::ViewMode(ViewMode::parse(&evt.value())));
                platform::focus_by_id(PLAYER_ID);
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
        select {
            class: "pharos-track-picker pharos-version-picker",
            aria_label: "Version",
            onkeydown: move |evt| menu_key(evt, wake),
            onchange: move |evt: FormEvent| {
                if let Ok(i) = evt.value().parse::<usize>() {
                    on_pick.call(Control::Version(i));
                }
                platform::focus_by_id(PLAYER_ID);
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

/// The quality menu: Auto or a cap, shown as the state in force, with a
/// reset while Auto has lowered itself.
#[component]
fn QualityPicker(quality: QualityView, height: u32, on_pick: EventHandler<Control>) -> Element {
    let wake = try_use_context::<Wake>();
    let title = quality.title();
    rsx! {
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
        let shrink = html.find(r#"aria-label="Shrink""#).expect("shrink");
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
        assert!(!html.contains("pharos-watch-loading-caption"), "no cause, no caption: {html}");
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
