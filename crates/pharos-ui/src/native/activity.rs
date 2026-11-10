//! Who did what in a watch-together group (014 US8) as short lines, and the
//! bounded log the member strip and the toast over the video read. Pure: the
//! caller passes the time, so tests never wait.

use std::collections::VecDeque;

use super::playback::clock;

/// Lines the log keeps.
pub const LOG_LINES: usize = 3;
/// How long the newest line stays on the toast.
pub const TOAST_MS: u64 = 4_000;

/// Which track list a change is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Subtitles,
}

/// What was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Play,
    Pause {
        position_ms: u64,
    },
    Seek {
        position_ms: u64,
    },
    /// Members whose buffering the group is waiting on.
    Waiting {
        names: Vec<String>,
    },
    Joined,
    Left,
    /// The group's pick changed; `label` is `None` when subtitles went off.
    Tracks {
        kind: TrackKind,
        label: Option<String>,
    },
    /// A member left or rejoined the group's tracks.
    OwnTracks {
        on: bool,
    },
    /// The group did not take this member's pick, so only their player changed.
    PickNotShared {
        kind: TrackKind,
    },
    /// A command to the group failed or timed out, so the group did not hear it.
    CommandFailed,
    /// A member cancelled "Up next", so the group stays on this episode.
    HeldNext,
}

/// An event and who caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The member's display name; `None` means the group did it.
    pub by: Option<String>,
    pub action: Action,
}

/// The short line for an event.
pub fn line(event: &Event) -> String {
    let who = event.by.as_deref();
    match &event.action {
        Action::Play => match who {
            Some(name) => format!("{name} pressed play"),
            None => "Everyone's ready, playing".to_owned(),
        },
        Action::Pause { position_ms } => {
            format!("{} paused at {}", subject(who), clock(*position_ms))
        }
        Action::Seek { position_ms } => {
            format!("{} skipped to {}", subject(who), clock(*position_ms))
        }
        Action::Waiting { names } => format!("Waiting for {}", join_names(names)),
        Action::Joined => format!("{} joined", subject(who)),
        Action::Left => format!("{} left", subject(who)),
        Action::Tracks {
            kind: TrackKind::Audio,
            label,
        } => match label {
            Some(l) => format!("{} switched audio to {l}", subject(who)),
            None => format!("{} switched audio", subject(who)),
        },
        Action::Tracks {
            kind: TrackKind::Subtitles,
            label,
        } => match label {
            Some(l) => format!("{} turned subtitles to {l}", subject(who)),
            None => format!("{} turned subtitles off", subject(who)),
        },
        Action::OwnTracks { on: true } => format!("{} is on their own tracks", subject(who)),
        Action::OwnTracks { on: false } => {
            format!("{} is following the group's tracks", subject(who))
        }
        Action::CommandFailed => "Couldn't reach the group. Try again.".to_owned(),
        Action::HeldNext => format!("{} cancelled Up next", subject(who)),
        Action::PickNotShared { kind } => format!(
            "Couldn't change the group's {}; changed yours",
            match kind {
                TrackKind::Audio => "audio",
                TrackKind::Subtitles => "subtitles",
            }
        ),
    }
}

fn subject(who: Option<&str>) -> &str {
    who.unwrap_or("The group")
}

/// "Sam", "Sam and Ali", "Sam, Ali and Bo"; "everyone" for none.
fn join_names(names: &[String]) -> String {
    match names {
        [] => "everyone".to_owned(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// How long a status note over the video (a group ended, a rejoin failed)
/// stays before it gets out of the picture's way (spec 038 FR-501).
pub const NOTE_MS: u64 = 8_000;
/// How long "Everyone's ready" shows once the group stops waiting.
pub const READY_MS: u64 = 2_000;
/// How long Leave can be undone (FR-505).
pub const UNDO_LEAVE_MS: u64 = 5_000;

/// When a note was first shown, so it hides `for_ms` after; a different
/// text starts again.
#[derive(Debug, Default)]
pub struct NoteTimer {
    since: Option<(String, u64)>,
}

/// What [`NoteTimer::show`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteShown {
    pub visible: bool,
    /// The text is new: arm one timer to redraw without it.
    pub arm: bool,
}

impl NoteTimer {
    pub fn show(&mut self, text: Option<&str>, now_ms: u64, for_ms: u64) -> NoteShown {
        let Some(text) = text else {
            self.since = None;
            return NoteShown {
                visible: false,
                arm: false,
            };
        };
        match &self.since {
            Some((shown, at)) if shown == text => NoteShown {
                visible: now_ms.saturating_sub(*at) < for_ms,
                arm: false,
            },
            _ => {
                self.since = Some((text.to_owned(), now_ms));
                NoteShown {
                    visible: true,
                    arm: true,
                }
            }
        }
    }
}

/// The pill over the video while the group waits on someone (FR-502).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitPill {
    Waiting(String),
    Ready,
}

impl WaitPill {
    pub fn text(&self) -> String {
        match self {
            Self::Waiting(names) => names.clone(),
            Self::Ready => "Everyone's ready".to_owned(),
        }
    }
}

/// Holds the pill: the wait while anyone is buffering, then "ready" for
/// [`READY_MS`] once nobody is.
#[derive(Debug, Default)]
pub struct WaitTracker {
    ready_at: Option<u64>,
    waited: bool,
}

impl WaitTracker {
    /// The pill for `waiting` (names) at `now_ms`, and whether a timer must
    /// be armed to take a "ready" pill down.
    pub fn pill(&mut self, waiting: &[String], now_ms: u64) -> (Option<WaitPill>, bool) {
        if !waiting.is_empty() {
            self.waited = true;
            self.ready_at = None;
            let line = line(&Event {
                by: None,
                action: Action::Waiting {
                    names: waiting.to_vec(),
                },
            });
            return (Some(WaitPill::Waiting(line)), false);
        }
        if self.waited {
            self.waited = false;
            self.ready_at = Some(now_ms);
            return (Some(WaitPill::Ready), true);
        }
        match self.ready_at {
            Some(at) if now_ms.saturating_sub(at) < READY_MS => (Some(WaitPill::Ready), false),
            _ => {
                self.ready_at = None;
                (None, false)
            }
        }
    }
}

/// The newest [`LOG_LINES`] lines, each with when it arrived.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityLog {
    lines: VecDeque<(String, u64)>,
}

impl ActivityLog {
    /// Record an event at `now_ms`, dropping the oldest line past the bound.
    pub fn push(&mut self, event: &Event, now_ms: u64) {
        if self.lines.len() >= LOG_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back((line(event), now_ms));
    }

    /// The kept lines, oldest first.
    pub fn lines(&self) -> Vec<String> {
        self.lines.iter().map(|(l, _)| l.clone()).collect()
    }

    /// The newest line while it is under [`TOAST_MS`] old at `now_ms`.
    pub fn toast(&self, now_ms: u64) -> Option<&str> {
        let (text, at) = self.lines.back()?;
        (now_ms.saturating_sub(*at) < TOAST_MS).then_some(text.as_str())
    }

    /// The arrival time of the toast showing at `now_ms` when no timer is yet
    /// armed for it: `armed` is the time of the line the last timer was set
    /// for. One timer per toast line, however often the screen redraws.
    pub fn toast_to_time(&self, armed: Option<u64>, now_ms: u64) -> Option<u64> {
        self.toast(now_ms)?;
        let (_, at) = self.lines.back()?;
        (armed != Some(*at)).then_some(*at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by(name: &str, action: Action) -> Event {
        Event {
            by: Some(name.into()),
            action,
        }
    }

    fn group(action: Action) -> Event {
        Event { by: None, action }
    }

    #[test]
    fn a_note_shows_for_its_time_and_a_new_text_starts_again() {
        let mut timer = NoteTimer::default();
        let first = timer.show(Some("Ended"), 1_000, NOTE_MS);
        assert!(first.visible && first.arm);
        let later = timer.show(Some("Ended"), 1_000 + NOTE_MS - 1, NOTE_MS);
        assert!(later.visible && !later.arm, "one timer per text");
        assert!(!timer.show(Some("Ended"), 1_000 + NOTE_MS, NOTE_MS).visible);
        assert!(timer.show(Some("Other"), 20_000, NOTE_MS).arm);
        assert!(!timer.show(None, 20_001, NOTE_MS).visible);
        assert!(
            timer.show(Some("Other"), 20_002, NOTE_MS).arm,
            "cleared, then back"
        );
    }

    #[test]
    fn the_wait_pill_holds_while_anyone_buffers_then_says_ready() {
        let mut wait = WaitTracker::default();
        assert_eq!(wait.pill(&[], 0), (None, false), "never waited");
        let names = vec!["Sam".to_owned()];
        assert_eq!(
            wait.pill(&names, 1_000),
            (Some(WaitPill::Waiting("Waiting for Sam".into())), false)
        );
        assert_eq!(
            wait.pill(&names, 60_000).0,
            Some(WaitPill::Waiting("Waiting for Sam".into())),
            "no time limit on a wait"
        );
        assert_eq!(wait.pill(&[], 61_000), (Some(WaitPill::Ready), true));
        assert_eq!(
            wait.pill(&[], 61_000 + READY_MS - 1).0,
            Some(WaitPill::Ready)
        );
        assert_eq!(wait.pill(&[], 61_000 + READY_MS), (None, false));
        assert_eq!(WaitPill::Ready.text(), "Everyone's ready");
    }

    #[test]
    fn a_command_the_group_missed_reads_as_a_plain_retry_line() {
        assert_eq!(
            line(&group(Action::CommandFailed)),
            "Couldn't reach the group. Try again."
        );
    }

    #[test]
    fn a_toast_line_is_timed_once_however_often_the_screen_redraws() {
        let mut log = ActivityLog::default();
        assert_eq!(log.toast_to_time(None, 0), None, "nothing to time");
        log.push(&group(Action::Joined), 1_000);
        let armed = log.toast_to_time(None, 1_000);
        assert_eq!(armed, Some(1_000));
        assert_eq!(log.toast_to_time(armed, 2_000), None, "same line");
        log.push(&group(Action::Left), 2_500);
        assert_eq!(log.toast_to_time(armed, 2_500), Some(2_500), "a new line");
        assert_eq!(log.toast_to_time(Some(2_500), 9_000), None, "expired");
    }

    #[test]
    fn every_member_line_has_its_shape() {
        let cases = [
            (by("Sam", Action::Play), "Sam pressed play"),
            (
                by(
                    "Sam",
                    Action::Pause {
                        position_ms: 724_000,
                    },
                ),
                "Sam paused at 12:04",
            ),
            (
                by(
                    "Ali",
                    Action::Seek {
                        position_ms: 2_410_000,
                    },
                ),
                "Ali skipped to 40:10",
            ),
            (
                by(
                    "Ali",
                    Action::Seek {
                        position_ms: 3_723_000,
                    },
                ),
                "Ali skipped to 1:02:03",
            ),
            (by("Sam", Action::Joined), "Sam joined"),
            (by("Sam", Action::Left), "Sam left"),
            (
                by(
                    "Sam",
                    Action::Tracks {
                        kind: TrackKind::Subtitles,
                        label: Some("English".into()),
                    },
                ),
                "Sam turned subtitles to English",
            ),
            (
                by(
                    "Sam",
                    Action::Tracks {
                        kind: TrackKind::Subtitles,
                        label: None,
                    },
                ),
                "Sam turned subtitles off",
            ),
            (
                by(
                    "Sam",
                    Action::Tracks {
                        kind: TrackKind::Audio,
                        label: Some("Japanese".into()),
                    },
                ),
                "Sam switched audio to Japanese",
            ),
            (
                by("Sam", Action::OwnTracks { on: true }),
                "Sam is on their own tracks",
            ),
            (
                by("Sam", Action::OwnTracks { on: false }),
                "Sam is following the group's tracks",
            ),
            (by("Sam", Action::HeldNext), "Sam cancelled Up next"),
        ];
        for (event, want) in cases {
            assert_eq!(line(&event), want, "{event:?}");
        }
    }

    #[test]
    fn a_group_play_says_everyone_is_ready() {
        assert_eq!(line(&group(Action::Play)), "Everyone's ready, playing");
    }

    #[test]
    fn a_group_pause_names_the_group() {
        assert_eq!(
            line(&group(Action::Pause { position_ms: 5_000 })),
            "The group paused at 0:05"
        );
    }

    #[test]
    fn waiting_names_one_two_or_many() {
        let waiting = |names: &[&str]| {
            line(&group(Action::Waiting {
                names: names.iter().map(|n| (*n).to_owned()).collect(),
            }))
        };
        assert_eq!(waiting(&["Sam"]), "Waiting for Sam");
        assert_eq!(waiting(&["Sam", "Ali"]), "Waiting for Sam and Ali");
        assert_eq!(
            waiting(&["Sam", "Ali", "Bo"]),
            "Waiting for Sam, Ali and Bo"
        );
        assert_eq!(waiting(&[]), "Waiting for everyone");
    }

    #[test]
    fn the_log_keeps_only_the_newest_three_lines() {
        let mut log = ActivityLog::default();
        for (i, name) in ["A", "B", "C", "D"].into_iter().enumerate() {
            log.push(&by(name, Action::Joined), i as u64);
        }
        assert_eq!(log.lines(), ["B joined", "C joined", "D joined"]);
    }

    #[test]
    fn the_toast_shows_the_newest_line_for_four_seconds() {
        let mut log = ActivityLog::default();
        assert_eq!(log.toast(0), None);
        log.push(&by("Sam", Action::Left), 10_000);
        log.push(&by("Ali", Action::Joined), 11_000);
        assert_eq!(log.toast(11_000), Some("Ali joined"));
        assert_eq!(log.toast(14_999), Some("Ali joined"));
        assert_eq!(log.toast(15_000), None, "expires at exactly 4 s");
    }

    #[test]
    fn a_new_line_restarts_the_toast() {
        let mut log = ActivityLog::default();
        log.push(&by("Sam", Action::Left), 0);
        assert_eq!(log.toast(5_000), None);
        log.push(&by("Sam", Action::Joined), 5_000);
        assert_eq!(log.toast(6_000), Some("Sam joined"));
    }
}
