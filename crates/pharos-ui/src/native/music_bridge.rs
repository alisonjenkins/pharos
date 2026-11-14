//! Music through the phone's native player (spec 044 T21, plan D4).
//!
//! Where a [`NativeAudio`] port is registered, a song plays in the host's
//! engine instead of an `<audio>` element. The player keeps everything else:
//! the play grant and its progress reports, the stage and its handover
//! (ADR-0052, ADR-0053), the queue, the screen. This module is the seam: it
//! turns the port's snapshots into the media state and events the player
//! already understands, and the player's commands into port calls.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use pharos_client::api::ItemRef;
use tokio::sync::watch;

use super::platform;
use super::playback::{MediaEvent, MediaState};
use super::ports::{NativeAudio, PlayState, PlayerSnapshot};
use super::watch_view::VIDEO_ID;

/// A position that moves further than this between two snapshots, while
/// playing, was a seek and not the playhead's own step. The host publishes
/// about once a second, so ordinary steps stay well under it.
const SEEK_JUMP_MS: u64 = 4_000;

/// A resync this soon after the start does not seek back: the restart costs
/// less than the seek.
const RESYNC_KEEPS_POSITION_AFTER_MS: u64 = 1_500;

/// What the player shows for `snapshot`. Volume is the device's, since the
/// port has no volume of its own.
pub fn media_state(snapshot: &PlayerSnapshot) -> MediaState {
    MediaState {
        position_ms: snapshot.position_ms,
        duration_ms: snapshot.duration_ms.unwrap_or(0),
        paused: matches!(
            snapshot.state,
            PlayState::Idle | PlayState::Paused | PlayState::Ended
        ),
        loading: snapshot.state == PlayState::Buffering,
        ..MediaState::default()
    }
}

/// The songs the host should play after `playing`: the queue's upcoming, minus
/// the head when it is `playing` itself (the queue pops it as the song starts).
pub fn after(playing: ItemRef, upcoming: &[ItemRef]) -> Vec<ItemRef> {
    match upcoming.split_first() {
        Some((head, rest)) if *head == playing => rest.to_vec(),
        _ => upcoming.to_vec(),
    }
}

fn current_of(snapshot: &PlayerSnapshot) -> Option<ItemRef> {
    snapshot.current_id.as_deref().and_then(|s| s.parse().ok())
}

/// The songs the host will play after its current one. Ids that are not items
/// are skipped.
fn host_after(snapshot: &PlayerSnapshot) -> Vec<ItemRef> {
    let from = snapshot
        .index
        .map_or(snapshot.queue.len(), |i| i.saturating_add(1));
    snapshot
        .queue
        .get(from..)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Whether the host's own queue has drifted from the player's: it holds
/// `playing` now but would play something other than `upcoming` next.
pub fn needs_resync(snapshot: &PlayerSnapshot, playing: ItemRef, upcoming: &[ItemRef]) -> bool {
    current_of(snapshot) == Some(playing) && host_after(snapshot) != after(playing, upcoming)
}

/// What the host did that the player must follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Event(MediaEvent),
    /// The host moved to another song by itself: it reached the end of one,
    /// or a notification or the car asked for the next.
    Moved(ItemRef),
    /// Playback failed, with the host's cause.
    Failed(String),
}

/// Turns the host's snapshots into the steps the player takes for one song.
#[derive(Debug, Default)]
pub struct Follow {
    /// The snapshot last seen while the host was on this song.
    seen: Option<PlayerSnapshot>,
}

impl Follow {
    pub fn step(&mut self, playing: ItemRef, snapshot: &PlayerSnapshot) -> Vec<Step> {
        let current = current_of(snapshot);
        if current != Some(playing) {
            // Before the host reaches this song there is nothing to follow;
            // once it has been here and is elsewhere, it moved on or stopped.
            let Some(was) = self.seen.take() else {
                return Vec::new();
            };
            return match current {
                Some(to) => vec![Step::Moved(to)],
                None if !matches!(was.state, PlayState::Idle | PlayState::Ended) => {
                    vec![Step::Event(MediaEvent::Pause)]
                }
                None => Vec::new(),
            };
        }
        let before = self.seen.replace(snapshot.clone()).unwrap_or_default();
        let mut steps = Vec::new();
        if snapshot.duration_ms != before.duration_ms {
            steps.push(Step::Event(MediaEvent::Changed));
        }
        if snapshot.state != before.state {
            steps.push(Step::Event(match snapshot.state {
                PlayState::Playing => MediaEvent::Playing,
                PlayState::Buffering => MediaEvent::Waiting,
                PlayState::Idle | PlayState::Paused => MediaEvent::Pause,
                PlayState::Ended => MediaEvent::Ended,
            }));
        }
        if snapshot.position_ms != before.position_ms {
            let jumped = snapshot.position_ms < before.position_ms
                || snapshot.position_ms.saturating_sub(before.position_ms) > SEEK_JUMP_MS;
            // A moving position is the playhead's step; a still one that
            // moved can only have been sought.
            let seeked = if snapshot.state == PlayState::Playing {
                before.state == PlayState::Playing && jumped
            } else {
                before.state == snapshot.state
            };
            steps.push(Step::Event(if seeked {
                MediaEvent::Seeked
            } else {
                MediaEvent::TimeUpdate
            }));
        }
        if snapshot.error != before.error {
            if let Some(cause) = &snapshot.error {
                steps.push(Step::Failed(cause.clone()));
            }
        }
        steps
    }
}

/// The player's handle on the host for one song.
#[derive(Clone)]
pub struct Music {
    port: Arc<dyn NativeAudio>,
    rx: watch::Receiver<PlayerSnapshot>,
    /// The queue last handed to the host.
    sent: Rc<RefCell<Vec<String>>>,
}

impl Music {
    pub fn new(port: Arc<dyn NativeAudio>) -> Self {
        let rx = port.state();
        Self {
            port,
            rx,
            sent: Rc::default(),
        }
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        self.rx.borrow().clone()
    }

    pub fn receiver(&self) -> watch::Receiver<PlayerSnapshot> {
        self.rx.clone()
    }

    /// The queue last handed to the host, as ids.
    pub fn sent(&self) -> Vec<String> {
        self.sent.borrow().clone()
    }

    /// Start `playing` and what follows it, unless the host is on it already.
    /// Returns whether it asked the host to play.
    pub fn begin(&self, playing: ItemRef, upcoming: &[ItemRef]) -> bool {
        if current_of(&self.snapshot()) == Some(playing) {
            return false;
        }
        self.play_from(playing, upcoming);
        true
    }

    fn play_from(&self, playing: ItemRef, upcoming: &[ItemRef]) {
        let ids: Vec<String> = std::iter::once(playing)
            .chain(after(playing, upcoming))
            .map(|id| id.to_string())
            .collect();
        self.sent.replace(ids.clone());
        self.port.play(ids, 0);
    }

    /// Give the host the queue the player now has, keeping the playhead.
    pub fn resync(&self, playing: ItemRef, upcoming: &[ItemRef]) {
        let at = self.snapshot().position_ms;
        self.play_from(playing, upcoming);
        if at > RESYNC_KEEPS_POSITION_AFTER_MS {
            self.port.seek(at);
        }
    }

    pub fn toggle(&self, playing: ItemRef, upcoming: &[ItemRef]) {
        match self.snapshot().state {
            PlayState::Playing | PlayState::Buffering => self.port.pause(),
            PlayState::Paused => self.port.resume(),
            PlayState::Idle | PlayState::Ended => self.play_from(playing, upcoming),
        }
    }

    pub fn seek(&self, ms: u64) {
        self.port.seek(ms);
    }

    pub fn pause(&self) {
        self.port.pause();
    }

    pub fn resume(&self) {
        self.port.resume();
    }

    pub fn stop(&self) {
        self.port.stop();
    }

    /// Ask the host for the song `to` if it is the one it plays next. `false`
    /// when it is not: the player then plays `to` as a new start.
    pub fn next_if(&self, to: ItemRef) -> bool {
        let is_next = host_after(&self.snapshot()).first() == Some(&to);
        if is_next {
            self.port.next();
        }
        is_next
    }

    /// As [`Music::next_if`], for the song before the current one.
    pub fn previous_if(&self, to: ItemRef) -> bool {
        let snapshot = self.snapshot();
        let before = snapshot
            .index
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| snapshot.queue.get(i))
            .and_then(|s| s.parse::<ItemRef>().ok());
        let is_previous = before == Some(to);
        if is_previous {
            self.port.previous();
        }
        is_previous
    }
}

/// Where the player's media calls go: the DOM element, or the host.
#[derive(Clone, Default)]
pub struct MediaIo {
    music: Option<Music>,
}

impl MediaIo {
    /// The host's engine for a song, when one is registered and the song is
    /// not a group's; otherwise the element, as ever.
    pub fn new(song_on_host: bool, port: Option<Arc<dyn NativeAudio>>) -> Self {
        Self {
            music: port.filter(|_| song_on_host).map(Music::new),
        }
    }

    pub fn music(&self) -> Option<&Music> {
        self.music.as_ref()
    }

    /// `None`: no host, use the element. `Some(None)`: a host, but `id` is not
    /// the playing slot's, so it names nothing there (a handover's retired
    /// song). `Some(Some(m))`: the host.
    fn host_for(&self, id: &str) -> Option<Option<&Music>> {
        self.music.as_ref().map(|m| (id == VIDEO_ID).then_some(m))
    }

    pub fn state(&self, id: &str) -> MediaState {
        match self.host_for(id) {
            None => platform::media_state(id),
            Some(m) => m.map_or_else(MediaState::default, |m| media_state(&m.snapshot())),
        }
    }

    pub fn position_ms(&self, id: &str) -> u64 {
        match self.host_for(id) {
            None => platform::position_ms(id),
            Some(m) => m.map_or(0, |m| m.snapshot().position_ms),
        }
    }

    pub fn seek_ms(&self, id: &str, ms: u64) {
        match self.host_for(id) {
            None => platform::seek_ms(id, ms),
            Some(m) => {
                if let Some(m) = m {
                    m.seek(ms);
                }
            }
        }
    }

    pub fn play(&self, id: &str) {
        match self.host_for(id) {
            None => platform::play(id),
            Some(m) => {
                if let Some(m) = m {
                    m.resume();
                }
            }
        }
    }

    pub fn pause(&self, id: &str) {
        match self.host_for(id) {
            None => platform::pause(id),
            Some(m) => {
                if let Some(m) = m {
                    m.pause();
                }
            }
        }
    }

    pub fn buffered_to_ms(&self, id: &str) -> u64 {
        match self.host_for(id) {
            None => platform::buffered_to_ms(id),
            Some(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::ports::fake::FakeAudio;
    use crate::native::watch_view::OUTGOING_ID;

    fn id(n: u64) -> ItemRef {
        ItemRef::Real(n)
    }

    fn ids(ns: &[u64]) -> Vec<ItemRef> {
        ns.iter().copied().map(id).collect()
    }

    fn music() -> (Arc<FakeAudio>, Music) {
        let fake = Arc::new(FakeAudio::new());
        let port: Arc<dyn NativeAudio> = fake.clone();
        (fake, Music::new(port))
    }

    fn follow(fake: &FakeAudio, f: &mut Follow, playing: u64) -> Vec<Step> {
        f.step(id(playing), &fake.snapshot())
    }

    #[test]
    fn the_snapshot_is_what_the_controls_show() {
        let snap = PlayerSnapshot {
            state: PlayState::Buffering,
            position_ms: 1_000,
            duration_ms: Some(200_000),
            ..PlayerSnapshot::default()
        };
        let m = media_state(&snap);
        assert_eq!((m.position_ms, m.duration_ms), (1_000, 200_000));
        assert!(m.loading && !m.paused, "buffering is a wait, not a pause");
        let paused = media_state(&PlayerSnapshot {
            state: PlayState::Paused,
            ..snap.clone()
        });
        assert!(paused.paused && !paused.loading);
        let playing = media_state(&PlayerSnapshot {
            state: PlayState::Playing,
            ..snap
        });
        assert!(!playing.paused && !playing.loading);
        assert_eq!(media_state(&PlayerSnapshot::default()).duration_ms, 0);
    }

    #[test]
    fn a_queue_head_that_is_the_playing_song_is_not_sent_twice() {
        assert_eq!(after(id(1), &ids(&[1, 2, 3])), ids(&[2, 3]));
        assert_eq!(after(id(1), &ids(&[2, 3])), ids(&[2, 3]));
        assert_eq!(after(id(1), &[]), Vec::new());
    }

    #[test]
    fn begin_hands_the_host_the_song_and_what_follows() {
        let (fake, m) = music();
        assert!(m.begin(id(1), &ids(&[2, 3])));
        let s = fake.snapshot();
        assert_eq!(s.queue, vec!["1", "2", "3"]);
        assert_eq!(s.current_id.as_deref(), Some("1"));
        assert_eq!(s.state, PlayState::Playing);
        assert_eq!(m.sent(), vec!["1", "2", "3"]);
    }

    #[test]
    fn begin_leaves_a_host_that_is_on_the_song_alone() {
        let (fake, m) = music();
        fake.play(vec!["7".into(), "8".into()], 0);
        fake.seek(5_000);
        assert!(!m.begin(id(7), &ids(&[8])));
        assert_eq!(fake.snapshot().position_ms, 5_000, "no restart");
    }

    #[test]
    fn pause_and_play_follow_the_hosts_state() {
        let (fake, m) = music();
        m.begin(id(1), &[]);
        m.toggle(id(1), &[]);
        assert_eq!(fake.snapshot().state, PlayState::Paused);
        m.toggle(id(1), &[]);
        assert_eq!(fake.snapshot().state, PlayState::Playing);
    }

    #[test]
    fn a_song_that_ended_plays_again_from_the_start_on_toggle() {
        let (fake, m) = music();
        m.begin(id(1), &[]);
        fake.next();
        assert_eq!(fake.snapshot().state, PlayState::Ended);
        m.toggle(id(1), &[]);
        let s = fake.snapshot();
        assert_eq!((s.state, s.position_ms), (PlayState::Playing, 0));
    }

    #[test]
    fn a_scrub_seeks_the_host() {
        let (fake, m) = music();
        m.begin(id(1), &[]);
        m.seek(42_000);
        assert_eq!(fake.snapshot().position_ms, 42_000);
    }

    #[test]
    fn next_goes_through_the_host_only_for_the_song_it_plays_next() {
        let (fake, m) = music();
        m.begin(id(1), &ids(&[2, 3]));
        assert!(!m.next_if(id(3)), "not the host's next: a new start");
        assert_eq!(fake.snapshot().current_id.as_deref(), Some("1"));
        assert!(m.next_if(id(2)));
        assert_eq!(fake.snapshot().current_id.as_deref(), Some("2"));
    }

    #[test]
    fn previous_goes_through_the_host_only_for_the_song_before() {
        let (fake, m) = music();
        m.begin(id(1), &ids(&[2, 3]));
        fake.next();
        assert!(!m.previous_if(id(9)));
        assert!(m.previous_if(id(1)));
        assert_eq!(fake.snapshot().current_id.as_deref(), Some("1"));
    }

    #[test]
    fn a_queue_the_host_has_drifted_from_is_resynced_and_the_playhead_kept() {
        let (fake, m) = music();
        m.begin(id(1), &ids(&[2, 3]));
        fake.seek(30_000);
        assert!(!needs_resync(&fake.snapshot(), id(1), &ids(&[2, 3])));
        assert!(needs_resync(&fake.snapshot(), id(1), &ids(&[3, 2])));
        m.resync(id(1), &ids(&[3, 2]));
        let s = fake.snapshot();
        assert_eq!(s.queue, vec!["1", "3", "2"]);
        assert_eq!(s.position_ms, 30_000);
        assert!(!needs_resync(&s, id(1), &ids(&[3, 2])));
    }

    #[test]
    fn a_resync_right_after_the_start_does_not_seek() {
        let (fake, m) = music();
        m.begin(id(1), &ids(&[2]));
        fake.seek(500);
        m.resync(id(1), &ids(&[3]));
        assert_eq!(fake.snapshot().position_ms, 0);
    }

    #[test]
    fn nothing_is_resynced_while_the_host_is_on_another_song() {
        let (fake, m) = music();
        m.begin(id(1), &ids(&[2]));
        assert!(!needs_resync(&fake.snapshot(), id(9), &ids(&[5])));
    }

    #[test]
    fn the_hosts_start_is_a_playing_event_and_its_clock_a_time_update() {
        let (fake, m) = music();
        let mut f = Follow::default();
        assert!(follow(&fake, &mut f, 1).is_empty(), "host on nothing");
        m.begin(id(1), &[]);
        assert_eq!(
            follow(&fake, &mut f, 1),
            vec![Step::Event(MediaEvent::Playing)]
        );
        fake.set_duration(180_000);
        assert_eq!(
            follow(&fake, &mut f, 1),
            vec![Step::Event(MediaEvent::Changed)]
        );
        fake.seek(1_000);
        assert_eq!(
            follow(&fake, &mut f, 1),
            vec![Step::Event(MediaEvent::TimeUpdate)]
        );
    }

    #[test]
    fn a_jump_while_playing_is_a_seek() {
        let (fake, m) = music();
        let mut f = Follow::default();
        m.begin(id(1), &[]);
        fake.set_duration(180_000);
        follow(&fake, &mut f, 1);
        fake.seek(90_000);
        assert_eq!(
            follow(&fake, &mut f, 1),
            vec![Step::Event(MediaEvent::Seeked)]
        );
    }

    #[test]
    fn a_seek_while_paused_is_a_seek() {
        let (fake, m) = music();
        let mut f = Follow::default();
        m.begin(id(1), &[]);
        fake.set_duration(180_000);
        m.pause();
        follow(&fake, &mut f, 1);
        fake.seek(10_000);
        assert_eq!(
            follow(&fake, &mut f, 1),
            vec![Step::Event(MediaEvent::Seeked)]
        );
    }

    #[test]
    fn pause_and_resume_are_events() {
        let (fake, m) = music();
        let mut f = Follow::default();
        m.begin(id(1), &[]);
        follow(&fake, &mut f, 1);
        m.pause();
        assert_eq!(
            follow(&fake, &mut f, 1),
            vec![Step::Event(MediaEvent::Pause)]
        );
        m.resume();
        assert_eq!(
            follow(&fake, &mut f, 1),
            vec![Step::Event(MediaEvent::Playing)]
        );
    }

    #[test]
    fn the_end_of_the_queue_is_an_ended_event() {
        let (fake, m) = music();
        let mut f = Follow::default();
        m.begin(id(1), &[]);
        follow(&fake, &mut f, 1);
        fake.next();
        assert_eq!(
            follow(&fake, &mut f, 1),
            vec![Step::Event(MediaEvent::Ended)]
        );
    }

    #[test]
    fn the_host_moving_on_by_itself_is_followed() {
        let (fake, m) = music();
        let mut f = Follow::default();
        m.begin(id(1), &ids(&[2]));
        follow(&fake, &mut f, 1);
        fake.next();
        assert_eq!(follow(&fake, &mut f, 1), vec![Step::Moved(id(2))]);
        assert!(
            follow(&fake, &mut f, 1).is_empty(),
            "told once; this song has nothing more to follow"
        );
    }

    #[test]
    fn a_host_that_has_not_reached_this_song_is_not_followed() {
        let (fake, m) = music();
        let mut f = Follow::default();
        m.begin(id(1), &[]);
        assert!(follow(&fake, &mut f, 2).is_empty());
    }

    #[test]
    fn a_failure_carries_the_hosts_cause_once() {
        let (fake, m) = music();
        let mut f = Follow::default();
        m.begin(id(1), &[]);
        follow(&fake, &mut f, 1);
        fake.play(vec!["1".into()], 4);
        assert_eq!(
            follow(&fake, &mut f, 1),
            vec![Step::Failed("start index 4 is outside a queue of 1".into())]
        );
        assert!(follow(&fake, &mut f, 1).is_empty());
    }

    #[test]
    fn element_calls_reach_the_host_for_the_playing_slot_only() {
        let (fake, m) = music();
        m.begin(id(1), &[]);
        let io = MediaIo { music: Some(m) };
        io.seek_ms(VIDEO_ID, 9_000);
        assert_eq!(fake.snapshot().position_ms, 9_000);
        assert_eq!(io.position_ms(VIDEO_ID), 9_000);
        io.pause(VIDEO_ID);
        assert_eq!(fake.snapshot().state, PlayState::Paused);
        io.play(VIDEO_ID);
        assert_eq!(fake.snapshot().state, PlayState::Playing);
        // A handover's retired song must not pause the song now playing.
        io.pause(OUTGOING_ID);
        assert_eq!(fake.snapshot().state, PlayState::Playing);
        assert_eq!(io.state(OUTGOING_ID), MediaState::default());
    }

    #[test]
    fn a_group_song_or_no_port_keeps_the_element() {
        let port: Arc<dyn NativeAudio> = Arc::new(FakeAudio::new());
        assert!(MediaIo::new(true, Some(port.clone())).music().is_some());
        assert!(MediaIo::new(false, Some(port)).music().is_none());
        assert!(MediaIo::new(true, None).music().is_none());
    }
}
