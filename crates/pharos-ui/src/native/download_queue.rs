//! The download queue (ADR 0148): which video downloads next, when a
//! stopped one is tried again, and what each row of the Downloads page says.
//! No I/O: the queue driver (`kept_video_view.rs`) reads the [`Queue`] and
//! the store's records, asks [`next_action`], and does it. The web and the
//! phone run the same rules; their stores only fetch and store bytes.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::kept_video::{fits, KeepVideoRefusal, KeptState, KeptSubtitle, KeptVideo};

/// Where a device keeps the queue (a JSON [`Queue`]).
pub const QUEUE_KEY: &str = "pharos.download_queue";

/// Why a download stopped that a later try can mend. Saved on the record
/// (`KeptVideo.halt`), so a restart knows it too. A stop with no halt is a
/// failure only the viewer can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Halt {
    /// The play session its URLs carry ended (401, 403 or 410): a new one
    /// is negotiated and the stored files are kept.
    SessionEnded,
    /// No answer, an unready segment (404) or a busy server (5xx), past the
    /// store's own retries.
    Unreachable,
    /// The viewer paused it.
    Paused,
    /// A renewed session's stream is not the stored one (another segment
    /// list, another init segment): its files cannot be kept.
    StreamChanged,
}

impl Halt {
    /// The log and metric label: a dashboard contract, kept distinct.
    pub fn label(self) -> &'static str {
        match self {
            Self::SessionEnded => "session_ended",
            Self::Unreachable => "unreachable",
            Self::Paused => "paused",
            Self::StreamChanged => "stream_changed",
        }
    }
}

/// The halt a fetch that failed with `status` (`None`: no answer) earns
/// once the store's retries are spent; `None` for one a retry cannot mend.
pub fn halt_for_status(status: Option<u16>) -> Option<Halt> {
    match status {
        None | Some(404) => Some(Halt::Unreachable),
        Some(401 | 403 | 410) => Some(Halt::SessionEnded),
        Some(s) if (500..600).contains(&s) => Some(Halt::Unreachable),
        Some(_) => None,
    }
}

/// One video the viewer asked for, from the moment it is queued until it is
/// Ready or removed: what the driver needs to negotiate its stream again,
/// with no call for the item's detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub user: String,
    pub item: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub runtime_ms: u64,
    #[serde(default)]
    pub subtitles: Vec<KeptSubtitle>,
    pub quality_bps: u64,
    /// The viewer paused it (before or while it ran).
    #[serde(default)]
    pub paused: bool,
    /// Why it could not start, when no record says so: the server would not
    /// give a downloadable stream. Cleared by Retry.
    #[serde(default)]
    pub failed: Option<String>,
}

impl QueueEntry {
    /// The size it is admitted with before its playlists are read.
    pub fn estimate(&self) -> u64 {
        queued_estimate(self.quality_bps, self.runtime_ms)
    }
}

/// The audio a stream carries beside its capped video, for the estimate.
pub const AUDIO_BPS: u64 = 192_000;

/// The bytes a queued download is expected to need: its video cap plus
/// audio for its runtime. The playlists' own figure replaces it once read.
pub fn queued_estimate(quality_bps: u64, runtime_ms: u64) -> u64 {
    pharos_hls::offline::estimate_bytes(quality_bps.saturating_add(AUDIO_BPS), runtime_ms)
}

/// The viewer's queue, in order. Saved whole on every change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    pub entries: Vec<QueueEntry>,
}

impl Queue {
    /// Reads a saved queue; anything unreadable is an empty queue (the
    /// store's records still list what was started).
    pub fn parse(text: Option<&str>) -> Self {
        text.and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default()
    }

    pub fn find(&self, user: &str, item: &str) -> Option<&QueueEntry> {
        self.entries
            .iter()
            .find(|e| e.user == user && e.item == item)
    }

    fn find_mut(&mut self, user: &str, item: &str) -> Option<&mut QueueEntry> {
        self.entries
            .iter_mut()
            .find(|e| e.user == user && e.item == item)
    }

    /// Adds `entry` at the end, or puts a queued one back in its place
    /// with its new settings (a Retry, a new quality).
    pub fn push(&mut self, entry: QueueEntry) {
        match self.find_mut(&entry.user, &entry.item) {
            Some(e) => *e = entry,
            None => self.entries.push(entry),
        }
    }

    pub fn remove(&mut self, user: &str, item: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !(e.user == user && e.item == item));
        before != self.entries.len()
    }

    pub fn set_paused(&mut self, user: &str, item: &str, paused: bool) -> bool {
        self.find_mut(user, item)
            .map(|e| e.paused = paused)
            .is_some()
    }

    /// Moves `item` one place towards the front (`up`) or the back, among
    /// `user`'s entries. False when it is already at that end.
    pub fn shift(&mut self, user: &str, item: &str, up: bool) -> bool {
        let mine: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].user == user)
            .collect();
        let Some(at) = mine.iter().position(|&i| self.entries[i].item == item) else {
            return false;
        };
        let other = if up {
            at.checked_sub(1)
        } else {
            (at + 1 < mine.len()).then_some(at + 1)
        };
        match other {
            Some(o) => {
                self.entries.swap(mine[at], mine[o]);
                true
            }
            None => false,
        }
    }

    /// Takes in every record of `user`'s that is not Ready and not queued
    /// (a download started before the queue existed), at the end, and drops
    /// the entries whose record is Ready. True when anything changed.
    pub fn reconcile(&mut self, user: &str, records: &[KeptVideo]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| {
            e.user != user
                || !records
                    .iter()
                    .any(|r| r.user == user && r.item == e.item && r.ready())
        });
        let mut changed = before != self.entries.len();
        for r in records.iter().filter(|r| r.user == user && !r.ready()) {
            if self.find(user, &r.item).is_none() {
                self.entries.push(QueueEntry {
                    user: r.user.clone(),
                    item: r.item.clone(),
                    version: r.version.clone(),
                    title: r.title.clone(),
                    kind: r.kind.clone(),
                    runtime_ms: r.runtime_ms,
                    subtitles: r.subtitles.clone(),
                    quality_bps: r.quality_bps,
                    paused: r.halt == Some(Halt::Paused),
                    failed: None,
                });
                changed = true;
            }
        }
        changed
    }
}

/// The waits between tries of one download, seconds; the last repeats.
const BACKOFF_SECS: [u64; 5] = [2, 5, 15, 30, 60];

/// Tries the server answered (a session renewal, a server error) before a
/// download is given up as failed and the queue moves on. Tries that could
/// not reach the server are not counted: a viewer offline for a day keeps
/// the queue.
pub const MAX_ANSWERED_TRIES: u32 = 10;

/// What the driver remembers about one download's tries, in memory only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tries {
    /// Every try since the last progress.
    pub tries: u32,
    /// Of those, the ones the server answered.
    pub answered: u32,
    /// Not tried again before this (milliseconds, the driver's clock).
    pub next_at_ms: u64,
    /// The bytes the record held at the last try: more means progress.
    pub bytes: u64,
}

impl Tries {
    /// Notes a try that stopped; `answered` when the server answered it.
    pub fn failed(&mut self, now_ms: u64, answered: bool) {
        let wait = BACKOFF_SECS
            .get(self.tries as usize)
            .copied()
            .unwrap_or(BACKOFF_SECS[BACKOFF_SECS.len() - 1]);
        self.tries = self.tries.saturating_add(1);
        if answered {
            self.answered = self.answered.saturating_add(1);
        }
        self.next_at_ms = now_ms.saturating_add(wait * 1000);
    }

    /// A download that stored more since the last try starts its count
    /// again.
    pub fn progressed(&mut self, bytes: u64) {
        if bytes > self.bytes {
            *self = Self {
                bytes,
                ..Self::default()
            };
        }
    }

    pub fn given_up(&self) -> bool {
        self.answered >= MAX_ANSWERED_TRIES
    }
}

/// What the driver does next for one user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing: one is running, or nothing is waiting.
    Idle,
    /// Negotiate `item`'s stream and hand it to the store. A store holding
    /// a stopped record of it keeps the files that still match.
    Start { item: String },
    /// Remove `item`'s files and record, then start it afresh: its stream
    /// changed under a renewed session.
    Restart { item: String },
    /// The front of the queue waits until then (milliseconds).
    Wait { item: String, until_ms: u64 },
}

/// The next step for `user`'s queue: one download at a time, in queue
/// order. A paused entry or one that failed for good is passed over; one
/// waiting to be tried again holds the queue, since what stopped it (most
/// often the connection) would stop the next one too.
pub fn next_action(
    queue: &Queue,
    records: &[KeptVideo],
    tries: &HashMap<String, Tries>,
    user: &str,
    now_ms: u64,
) -> Action {
    let running = records
        .iter()
        .any(|r| r.user == user && matches!(r.state, KeptState::Downloading { .. }));
    if running {
        return Action::Idle;
    }
    for e in queue.entries.iter().filter(|e| e.user == user) {
        if e.paused || e.failed.is_some() {
            continue;
        }
        let record = records.iter().find(|r| r.user == user && r.item == e.item);
        let t = tries.get(&e.item).copied().unwrap_or_default();
        let action = match record {
            Some(r) if r.ready() => continue,
            Some(r) => match (&r.state, r.halt) {
                (KeptState::Failed { .. }, Some(Halt::StreamChanged)) => Action::Restart {
                    item: e.item.clone(),
                },
                (KeptState::Failed { .. }, Some(Halt::SessionEnded | Halt::Unreachable))
                    if !t.given_up() =>
                {
                    Action::Start {
                        item: e.item.clone(),
                    }
                }
                // Paused on the record while the entry is not: the viewer
                // resumed it.
                (KeptState::Failed { .. }, Some(Halt::Paused)) => Action::Start {
                    item: e.item.clone(),
                },
                _ => continue,
            },
            None => Action::Start {
                item: e.item.clone(),
            },
        };
        if t.next_at_ms > now_ms {
            return Action::Wait {
                item: e.item.clone(),
                until_ms: t.next_at_ms,
            };
        }
        return action;
    }
    Action::Idle
}

/// Whether a download may begin negotiating now: no other negotiation is
/// running (`starting`) and none of the user's downloads is running. The
/// queue's tick holds to both; a viewer's Retry must too, or it would start
/// beside them. Err names the closed gate for the log, and the item stays
/// due for the tick to start once both are open.
pub fn start_gate(starting: bool, records: &[KeptVideo], user: &str) -> Result<(), &'static str> {
    if starting {
        return Err("another_start_in_progress");
    }
    if records
        .iter()
        .any(|r| r.user == user && matches!(r.state, KeptState::Downloading { .. }))
    {
        return Err("another_download_running");
    }
    Ok(())
}

/// Whether `entry` fits beside everything kept and queued: the downloads
/// cap and the free-space floor hold for what is promised, not only what is
/// on disk. `used` is what the store counts (its own records included).
pub fn admit(
    queue: &Queue,
    records: &[KeptVideo],
    entry: &QueueEntry,
    used: u64,
    cap: u64,
    free: u64,
) -> Result<(), KeepVideoRefusal> {
    let promised: u64 = queue
        .entries
        .iter()
        .filter(|e| e.item != entry.item || e.user != entry.user)
        .filter(|e| !records.iter().any(|r| r.user == e.user && r.item == e.item))
        .map(QueueEntry::estimate)
        .sum();
    // An entry already started has bytes still to write that the free
    // figure does not lack yet: its estimate less what its record holds.
    // They take room only from the free-space floor: the cap's `used`
    // already counts the record's own bytes, and `promised` the rest.
    let outstanding: u64 = queue
        .entries
        .iter()
        .filter(|e| e.item != entry.item || e.user != entry.user)
        .filter_map(|e| {
            records
                .iter()
                .find(|r| r.user == e.user && r.item == e.item)
                .map(|r| e.estimate().saturating_sub(r.bytes()))
        })
        .sum();
    let promised_free = promised.saturating_add(outstanding);
    fits(
        entry.estimate(),
        used.saturating_add(promised),
        cap,
        free.saturating_sub(promised_free),
    )
}

/// What one row of the queue says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowState {
    Downloading {
        percent: u32,
        bytes: u64,
    },
    /// Stopped by something a try can mend; tried again by itself.
    Waiting {
        halt: Halt,
        bytes: u64,
        /// Seconds until the next try; 0 when it is due.
        in_secs: u64,
    },
    Queued,
    Paused {
        percent: u32,
        bytes: u64,
    },
    Failed {
        reason: String,
    },
}

/// One row of the queue on the Downloads page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub item: String,
    pub title: String,
    pub quality_bps: u64,
    /// The size it is expected to reach.
    pub estimate: u64,
    /// 1-based place among the user's entries.
    pub position: usize,
    pub state: RowState,
    pub can_move_up: bool,
    pub can_move_down: bool,
}

fn percent(done: u32, total: u32) -> u32 {
    if total == 0 {
        return 0;
    }
    let p = u64::from(done).saturating_mul(100) / u64::from(total);
    u32::try_from(p.min(100)).unwrap_or(100)
}

/// The rows of `user`'s queue, in order. A running download cannot be
/// moved and nothing moves above it: it keeps the front until it stops.
pub fn rows(
    queue: &Queue,
    records: &[KeptVideo],
    tries: &HashMap<String, Tries>,
    user: &str,
    now_ms: u64,
) -> Vec<Row> {
    let mine: Vec<&QueueEntry> = queue.entries.iter().filter(|e| e.user == user).collect();
    let mut out = Vec::with_capacity(mine.len());
    for (i, e) in mine.iter().enumerate() {
        let record = records.iter().find(|r| r.user == user && r.item == e.item);
        if record.is_some_and(KeptVideo::ready) {
            continue;
        }
        let progress = record.map_or((0, 0), |r| match r.state {
            KeptState::Downloading { done, total, bytes } => (percent(done, total), bytes),
            _ => (0, r.bytes()),
        });
        let t = tries.get(&e.item).copied().unwrap_or_default();
        let in_secs = t.next_at_ms.saturating_sub(now_ms).div_ceil(1000);
        let state = if let Some(reason) = &e.failed {
            RowState::Failed {
                reason: reason.clone(),
            }
        } else {
            match record.map(|r| (&r.state, r.halt)) {
                Some((KeptState::Downloading { .. }, _)) => RowState::Downloading {
                    percent: progress.0,
                    bytes: progress.1,
                },
                _ if e.paused => RowState::Paused {
                    percent: record.map_or(0, stored_percent),
                    bytes: progress.1,
                },
                Some((KeptState::Failed { reason }, None)) => RowState::Failed {
                    reason: reason.clone(),
                },
                Some((KeptState::Failed { reason }, Some(_))) if t.given_up() => RowState::Failed {
                    reason: format!("gave up after {} tries: {reason}", t.tries),
                },
                Some((KeptState::Failed { .. }, Some(halt))) => RowState::Waiting {
                    halt,
                    bytes: progress.1,
                    in_secs,
                },
                None if t.tries > 0 => RowState::Waiting {
                    halt: Halt::Unreachable,
                    bytes: 0,
                    in_secs,
                },
                _ => RowState::Queued,
            }
        };
        let running = matches!(state, RowState::Downloading { .. });
        let above_running = i
            .checked_sub(1)
            .and_then(|p| mine.get(p))
            .and_then(|p| records.iter().find(|r| r.user == user && r.item == p.item))
            .is_some_and(|r| matches!(r.state, KeptState::Downloading { .. }));
        out.push(Row {
            item: e.item.clone(),
            title: e.title.clone(),
            quality_bps: e.quality_bps,
            estimate: record
                .map(|r| r.estimated_bytes)
                .filter(|&b| b > 0)
                .unwrap_or_else(|| e.estimate()),
            position: i + 1,
            state,
            can_move_up: !running && i > 0 && !above_running,
            can_move_down: !running && i + 1 < mine.len(),
        });
    }
    out
}

/// How far a stopped record got, by stored bytes against its estimate.
fn stored_percent(r: &KeptVideo) -> u32 {
    if r.estimated_bytes == 0 {
        return 0;
    }
    let p = r.bytes().saturating_mul(100) / r.estimated_bytes;
    u32::try_from(p.min(100)).unwrap_or(100)
}

/// A waiting row's words: what it waits for and that it carries on alone.
pub fn waiting_line(halt: Halt, in_secs: u64) -> String {
    let what = match halt {
        Halt::SessionEnded => "Reconnecting to the server",
        Halt::Unreachable => "Waiting for a connection",
        Halt::Paused => "Paused",
        Halt::StreamChanged => "Starting again",
    };
    if in_secs == 0 {
        format!("{what} \u{b7} trying now")
    } else {
        format!("{what} \u{b7} tries again in {in_secs} s")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::native::offline_ui::FREE_GUARD_BYTES;

    fn entry(item: &str) -> QueueEntry {
        QueueEntry {
            user: "u".into(),
            item: item.into(),
            version: String::new(),
            title: format!("Invented Film {item}"),
            kind: "movie".into(),
            runtime_ms: 3_600_000,
            subtitles: Vec::new(),
            quality_bps: 2_000_000,
            paused: false,
            failed: None,
        }
    }

    fn record(item: &str, state: KeptState, halt: Option<Halt>) -> KeptVideo {
        KeptVideo {
            item: item.into(),
            version: String::new(),
            user: "u".into(),
            title: format!("Invented Film {item}"),
            kind: "movie".into(),
            runtime_ms: 3_600_000,
            master_url: format!("/videos/{item}/master.m3u8?PlaySessionId=a"),
            audio: None,
            subtitles: Vec::new(),
            files: Vec::new(),
            quality_bps: 2_000_000,
            estimated_bytes: 1_000,
            state,
            position_ms: 0,
            saved_at: String::new(),
            estimate_from_adoption: false,
            stale_on_disk: 0,
            halt,
        }
    }

    fn queue(items: &[&str]) -> Queue {
        Queue {
            entries: items.iter().map(|i| entry(i)).collect(),
        }
    }

    fn failed(halt: Option<Halt>) -> (KeptState, Option<Halt>) {
        (KeptState::Failed { reason: "x".into() }, halt)
    }

    fn downloading() -> KeptState {
        KeptState::Downloading {
            done: 1,
            total: 4,
            bytes: 10,
        }
    }

    #[test]
    fn a_session_end_and_no_answer_are_mended_by_a_try_and_a_refusal_is_not() {
        assert_eq!(halt_for_status(Some(401)), Some(Halt::SessionEnded));
        assert_eq!(halt_for_status(Some(410)), Some(Halt::SessionEnded));
        assert_eq!(halt_for_status(None), Some(Halt::Unreachable));
        assert_eq!(halt_for_status(Some(503)), Some(Halt::Unreachable));
        assert_eq!(halt_for_status(Some(404)), Some(Halt::Unreachable));
        assert_eq!(halt_for_status(Some(400)), None);
    }

    #[test]
    fn halt_labels_are_distinct() {
        let labels = [
            Halt::SessionEnded,
            Halt::Unreachable,
            Halt::Paused,
            Halt::StreamChanged,
        ]
        .map(Halt::label);
        let set: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(set.len(), labels.len());
    }

    /// Disarm check: dropping the `running` guard starts a second download
    /// beside the first.
    #[test]
    fn only_one_download_runs_at_a_time() {
        let q = queue(&["1", "2"]);
        let records = vec![record("1", downloading(), None)];
        assert_eq!(
            next_action(&q, &records, &HashMap::new(), "u", 0),
            Action::Idle
        );
        let records = vec![record("1", KeptState::Ready, None)];
        assert_eq!(
            next_action(&q, &records, &HashMap::new(), "u", 0),
            Action::Start { item: "2".into() }
        );
    }

    #[test]
    fn a_dropped_session_is_started_again_keeping_its_place() {
        let q = queue(&["1", "2"]);
        let (s, h) = failed(Some(Halt::SessionEnded));
        let records = vec![record("1", s, h)];
        assert_eq!(
            next_action(&q, &records, &HashMap::new(), "u", 0),
            Action::Start { item: "1".into() }
        );
    }

    #[test]
    fn a_waiting_download_holds_the_queue_until_its_next_try() {
        let q = queue(&["1", "2"]);
        let (s, h) = failed(Some(Halt::Unreachable));
        let records = vec![record("1", s, h)];
        let mut t = Tries::default();
        t.failed(1_000, false);
        let tries = HashMap::from([("1".to_owned(), t)]);
        assert_eq!(
            next_action(&q, &records, &tries, "u", 1_500),
            Action::Wait {
                item: "1".into(),
                until_ms: 3_000
            }
        );
        assert_eq!(
            next_action(&q, &records, &tries, "u", 3_000),
            Action::Start { item: "1".into() }
        );
    }

    #[test]
    fn paused_and_failed_downloads_are_passed_over() {
        let mut q = queue(&["1", "2", "3"]);
        q.set_paused("u", "1", true);
        let (s, h) = failed(None);
        let records = vec![record("2", s, h)];
        assert_eq!(
            next_action(&q, &records, &HashMap::new(), "u", 0),
            Action::Start { item: "3".into() }
        );
    }

    #[test]
    fn a_changed_stream_is_started_afresh() {
        let q = queue(&["1"]);
        let (s, h) = failed(Some(Halt::StreamChanged));
        assert_eq!(
            next_action(&q, &[record("1", s, h)], &HashMap::new(), "u", 0),
            Action::Restart { item: "1".into() }
        );
    }

    /// Tries the server answered run out; tries that never reached it do
    /// not, so a long time offline keeps the queue.
    #[test]
    fn only_answered_tries_give_a_download_up() {
        let mut t = Tries::default();
        for _ in 0..50 {
            t.failed(0, false);
        }
        assert!(!t.given_up());
        for _ in 0..MAX_ANSWERED_TRIES {
            t.failed(0, true);
        }
        assert!(t.given_up());
        t.progressed(t.bytes + 1);
        assert!(!t.given_up());
        assert_eq!(t.tries, 0);
    }

    #[test]
    fn backoff_grows_and_stops_at_a_minute() {
        let mut t = Tries::default();
        let mut waits = Vec::new();
        for _ in 0..7 {
            t.failed(0, false);
            waits.push(t.next_at_ms / 1000);
        }
        assert_eq!(waits, [2, 5, 15, 30, 60, 60, 60]);
    }

    #[test]
    fn entries_move_among_one_users_own() {
        let mut q = queue(&["1", "2", "3"]);
        q.entries.insert(
            1,
            QueueEntry {
                user: "other".into(),
                ..entry("9")
            },
        );
        assert!(q.shift("u", "3", true));
        let mine: Vec<_> = q
            .entries
            .iter()
            .filter(|e| e.user == "u")
            .map(|e| e.item.as_str())
            .collect();
        assert_eq!(mine, ["1", "3", "2"]);
        assert!(!q.shift("u", "1", true));
        assert!(!q.shift("u", "2", false));
        assert_eq!(q.entries[1].user, "other");
    }

    #[test]
    fn reconcile_adopts_unqueued_downloads_and_drops_ready_ones() {
        let mut q = queue(&["1"]);
        let records = vec![
            record("1", KeptState::Ready, None),
            record("2", downloading(), None),
            record(
                "3",
                KeptState::Failed { reason: "x".into() },
                Some(Halt::Paused),
            ),
        ];
        assert!(q.reconcile("u", &records));
        let items: Vec<_> = q
            .entries
            .iter()
            .map(|e| (e.item.as_str(), e.paused))
            .collect();
        assert_eq!(items, [("2", false), ("3", true)]);
        assert!(!q.reconcile("u", &records));
    }

    #[test]
    fn the_queue_reads_back_what_it_saved_and_shrugs_off_junk() {
        let q = queue(&["1"]);
        let text = serde_json::to_string(&q).unwrap();
        assert_eq!(Queue::parse(Some(&text)), q);
        assert_eq!(Queue::parse(Some("not json")), Queue::default());
        assert_eq!(Queue::parse(None), Queue::default());
    }

    /// Disarm check: leaving the queued entries out of `promised` admits
    /// the third film.
    #[test]
    fn admission_counts_what_is_queued_but_not_started() {
        let q = queue(&["1", "2"]);
        let one = entry("1").estimate();
        let third = entry("3");
        // Room for two films, both already promised.
        let cap = one * 2 + one / 2;
        assert!(admit(&q, &[], &third, 0, cap, u64::MAX).is_err());
        assert!(admit(&Queue::default(), &[], &third, 0, cap, u64::MAX).is_ok());
    }

    /// CR review 6018104500 (download_driver.rs:195-197): a start
    /// waits for the gates the tick keeps.
    #[test]
    fn a_start_keeps_the_ticks_gates() {
        let records = [record("1", downloading(), None)];
        assert_eq!(start_gate(true, &[], "u"), Err("another_start_in_progress"));
        assert_eq!(
            start_gate(false, &records, "u"),
            Err("another_download_running")
        );
        assert_eq!(start_gate(false, &records, "other"), Ok(()));
        assert_eq!(start_gate(false, &[], "u"), Ok(()));
    }

    /// CR review 6018104500 (download_queue.rs:344): a download already
    /// started still has bytes to write; the free-space floor counts them.
    /// Disarm check: leaving `outstanding` out admits the second film.
    #[test]
    fn admission_counts_the_bytes_a_started_download_has_still_to_write() {
        let q = queue(&["1"]);
        let records = [record("1", downloading(), None)];
        let second = entry("2");
        // Room for the second film beside the first's bytes on disk, but
        // not beside what the first has yet to write.
        let free = second.estimate() + FREE_GUARD_BYTES + 10_000;
        assert!(admit(&q, &records, &second, 0, u64::MAX, free).is_err());
        let roomy = free + q.entries[0].estimate();
        assert!(admit(&q, &records, &second, 0, u64::MAX, roomy).is_ok());
    }

    #[test]
    fn rows_say_what_each_download_is_doing() {
        let mut q = queue(&["1", "2", "3", "4", "5"]);
        q.set_paused("u", "4", true);
        q.entries[4].failed = Some("The server chose direct_play".into());
        let (s, h) = failed(Some(Halt::SessionEnded));
        let records = vec![record("1", downloading(), None), record("2", s, h)];
        let mut t = Tries::default();
        t.failed(0, true);
        let tries = HashMap::from([("2".to_owned(), t)]);
        let r = rows(&q, &records, &tries, "u", 1_000);
        assert_eq!(
            r[0].state,
            RowState::Downloading {
                percent: 25,
                bytes: 10
            }
        );
        assert!(!r[0].can_move_up && !r[0].can_move_down);
        assert_eq!(
            r[1].state,
            RowState::Waiting {
                halt: Halt::SessionEnded,
                bytes: 1_000,
                in_secs: 1
            }
        );
        assert!(!r[1].can_move_up, "nothing moves above the running one");
        assert_eq!(r[2].state, RowState::Queued);
        assert!(r[2].can_move_up && r[2].can_move_down);
        assert!(matches!(r[3].state, RowState::Paused { .. }));
        assert!(matches!(r[4].state, RowState::Failed { .. }));
        assert_eq!(
            r.iter().map(|r| r.position).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn a_waiting_line_says_it_carries_on_by_itself() {
        assert_eq!(
            waiting_line(Halt::Unreachable, 30),
            "Waiting for a connection \u{b7} tries again in 30 s"
        );
        assert_eq!(
            waiting_line(Halt::SessionEnded, 0),
            "Reconnecting to the server \u{b7} trying now"
        );
    }
}
