//! What the player is doing, as one value on a `watch` channel, for the
//! phone UI. It mirrors `pharos-ui`'s `PlayerSnapshot` field for field
//! without depending on it; `pharos-phone` converts between the two.
//!
//! Ids here are the server's item ids (`42`, `s7`), as the UI uses them. The
//! car's browse ids carry a `track/` prefix; [`ui_id`] and [`car_id`] map
//! between the two.

use std::sync::OnceLock;

use pharos_content::NodeId;
use tokio::sync::watch;

use super::{STATE_BUFFERING, STATE_ERROR, STATE_PAUSED, STATE_PLAYING, STATE_STOPPED};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    #[default]
    Idle,
    Buffering,
    Playing,
    Paused,
    /// The queue played to its end.
    Ended,
}

impl PlayState {
    pub fn label(self) -> &'static str {
        match self {
            PlayState::Idle => "idle",
            PlayState::Buffering => "buffering",
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
            PlayState::Ended => "ended",
        }
    }

    /// The state for a `PlaybackState.STATE_*` code. STOPPED is `Idle`; the
    /// end of the queue is marked separately with [`mark_ended`].
    pub(crate) fn of_code(code: i32) -> Self {
        match code {
            STATE_PLAYING => PlayState::Playing,
            STATE_BUFFERING => PlayState::Buffering,
            STATE_PAUSED => PlayState::Paused,
            STATE_STOPPED | STATE_ERROR => PlayState::Idle,
            _ => PlayState::Idle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub state: PlayState,
    pub position_ms: u64,
    /// `None` until the length is known.
    pub duration_ms: Option<u64>,
    pub current_id: Option<String>,
    pub queue: Vec<String>,
    /// Index of `current_id` in `queue`.
    pub index: Option<usize>,
    /// The cause of the last playback failure, with the offending value.
    pub error: Option<String>,
}

const TRACK: &str = "track/";

/// The UI's id for car id `id`: a track's item id, anything else unchanged.
pub fn ui_id(id: &str) -> String {
    id.strip_prefix(TRACK).unwrap_or(id).to_owned()
}

/// The car's id for UI id `id`: an item id becomes its `track/` node, an id
/// that is already a node (or a test entry) is kept.
pub fn car_id(id: &str) -> String {
    let track = format!("{TRACK}{id}");
    if NodeId::parse(id).is_err() && NodeId::parse(&track).is_ok() {
        track
    } else {
        id.to_owned()
    }
}

fn channel() -> &'static watch::Sender<Snapshot> {
    static TX: OnceLock<watch::Sender<Snapshot>> = OnceLock::new();
    TX.get_or_init(|| watch::Sender::new(Snapshot::default()))
}

/// A receiver that always holds the latest snapshot.
pub fn subscribe() -> watch::Receiver<Snapshot> {
    channel().subscribe()
}

pub(crate) fn current() -> Snapshot {
    channel().borrow().clone()
}

/// The session published state `code` at `position_ms`, with `error` when
/// it is STATE_ERROR.
pub(crate) fn note_state(code: i32, position_ms: i64, error: Option<&str>) {
    channel().send_if_modified(|s| {
        let before = s.clone();
        s.state = PlayState::of_code(code);
        s.position_ms = u64::try_from(position_ms).unwrap_or(0);
        if code == STATE_ERROR {
            s.error = Some(error.unwrap_or("playback failed").to_owned());
        }
        *s != before
    });
}

/// A failure with no session to show it on, or one the session state does
/// not carry (a play that never reached the player).
pub(crate) fn note_error(cause: &str) {
    channel().send_modify(|s| {
        s.state = PlayState::Idle;
        s.error = Some(cause.to_owned());
    });
}

/// The player moved to `current` (car id) in `queue` (car ids), not yet
/// started; a new track clears the last error.
pub(crate) fn note_track(current: &str, queue: &[String], duration_ms: i64) {
    channel().send_modify(|s| {
        s.queue = queue.iter().map(|id| ui_id(id)).collect();
        s.index = queue.iter().position(|id| id == current);
        s.current_id = Some(ui_id(current));
        s.position_ms = 0;
        s.duration_ms = known(duration_ms);
        s.error = None;
    });
}

/// The queue was replaced around the current track, which keeps playing.
pub(crate) fn note_queue(queue: &[String], index: usize) {
    channel().send_modify(|s| {
        s.queue = queue.iter().map(|id| ui_id(id)).collect();
        s.index = Some(index);
    });
}

pub(crate) fn note_duration(duration_ms: i64) {
    channel().send_if_modified(|s| {
        let d = known(duration_ms);
        let changed = s.duration_ms != d;
        s.duration_ms = d;
        changed
    });
}

pub(crate) fn note_position(position_ms: u64) {
    channel().send_if_modified(|s| {
        let changed = s.position_ms != position_ms;
        s.position_ms = position_ms;
        changed
    });
}

/// The queue played out: the last track stays current, the state is `Ended`.
pub(crate) fn mark_ended() {
    channel().send_modify(|s| s.state = PlayState::Ended);
}

/// A user stop: nothing is loaded any more.
pub(crate) fn clear() {
    channel().send_modify(|s| *s = Snapshot::default());
}

fn known(ms: i64) -> Option<u64> {
    u64::try_from(ms).ok().filter(|&ms| ms > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_codes_map_to_states() {
        assert_eq!(PlayState::of_code(STATE_PLAYING), PlayState::Playing);
        assert_eq!(PlayState::of_code(STATE_BUFFERING), PlayState::Buffering);
        assert_eq!(PlayState::of_code(STATE_PAUSED), PlayState::Paused);
        assert_eq!(PlayState::of_code(STATE_STOPPED), PlayState::Idle);
        assert_eq!(PlayState::of_code(STATE_ERROR), PlayState::Idle);
    }

    #[test]
    fn ids_map_between_the_car_and_the_ui() {
        assert_eq!(ui_id("track/42"), "42");
        assert_eq!(ui_id("track/s7"), "s7");
        assert_eq!(ui_id("tone:440"), "tone:440");
        assert_eq!(car_id("42"), "track/42");
        assert_eq!(car_id("s7"), "track/s7");
        assert_eq!(car_id("track/42"), "track/42");
        assert_eq!(car_id("tone:440"), "tone:440");
    }

    #[test]
    fn a_zero_or_unknown_length_is_none() {
        assert_eq!(known(0), None);
        assert_eq!(known(-1), None);
        assert_eq!(known(1_500), Some(1_500));
    }

    // One test drives the shared channel, so no other test races it.
    #[test]
    fn the_channel_follows_a_track_through_error_end_and_stop() {
        let rx = subscribe();
        let queue = vec!["track/1".to_owned(), "track/2".to_owned()];
        note_track("track/2", &queue, 0);
        let s = rx.borrow().clone();
        assert_eq!(s.current_id.as_deref(), Some("2"));
        assert_eq!(s.queue, ["1", "2"]);
        assert_eq!((s.index, s.duration_ms), (Some(1), None));

        note_duration(180_000);
        note_state(STATE_PLAYING, 4_000, None);
        let s = current();
        assert_eq!(
            (s.state, s.position_ms, s.duration_ms),
            (PlayState::Playing, 4_000, Some(180_000))
        );

        note_state(STATE_ERROR, 0, Some("player error what=1 extra=-1004"));
        assert_eq!(
            current().error.as_deref(),
            Some("player error what=1 extra=-1004")
        );
        note_track("track/1", &queue, 1_000);
        assert_eq!(current().error, None, "a new track clears the error");

        mark_ended();
        let s = current();
        assert_eq!(
            (s.state, s.current_id.as_deref()),
            (PlayState::Ended, Some("1"))
        );
        clear();
        assert_eq!(current(), Snapshot::default());
    }
}
