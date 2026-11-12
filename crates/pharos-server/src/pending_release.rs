//! Grants released without a stop, held for a grace period (B1397).
//!
//! A native player's connection can drop under a live stream (an ingress roll)
//! and be back seconds later; the player keeps pulling segments under the same
//! `PlaySessionId` and only negotiates its grant again. Ending the playback the
//! moment the grant is released would 410 those segments. So a released grant
//! waits here for [`DEFAULT_RELEASE_GRACE`]: a negotiate from the same user and
//! device for the same stream adopts it, anything else ends it.
//!
//! The map holds only `Send` data and the verdict channel. Whoever removes a
//! slot from the map decides its fate, so expiry and adoption can never both
//! win; the connection's own task, which owns the (thread-bound) session,
//! receives the verdict and ends the playback.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use pharos_core::{ClientDeviceId, UserId};
use pharos_jellyfin_api::device_profile::Decision;
use tokio::sync::oneshot;

/// How long a released grant waits for its player to come back: covers an
/// ingress roll plus the client's reconnect backoff.
pub const DEFAULT_RELEASE_GRACE: Duration = Duration::from_secs(30);

/// The most released grants held at once; each is a few hundred bytes. A
/// release past it ends at once, as it did before the grace existed.
pub const MAX_PENDING_RELEASES: usize = 128;

/// Who a released grant would be adopted by: the same user on the same device
/// playing the same version of the same item.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReleaseKey {
    pub user: UserId,
    pub device: ClientDeviceId,
    pub item: u64,
    /// The version as the player named it; empty is the primary file.
    pub version: String,
}

/// What makes two negotiations the same stream: the decision (delivery method
/// and transcode targets, bitrate cap included), the subtitles burned in, and
/// the tracks the playback starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamShape {
    pub decision: Decision,
    pub burn_subtitle_indices: BTreeSet<u32>,
    pub audio: Option<u32>,
    pub subtitle: Option<u32>,
}

impl StreamShape {
    /// The first field that differs from `other`, with both values.
    fn mismatch(&self, other: &Self) -> Option<String> {
        if self.decision != other.decision {
            return Some(format!(
                "decision: held {:?}, asked {:?}",
                self.decision, other.decision
            ));
        }
        if self.burn_subtitle_indices != other.burn_subtitle_indices {
            return Some(format!(
                "burn_subtitle_indices: held {:?}, asked {:?}",
                self.burn_subtitle_indices, other.burn_subtitle_indices
            ));
        }
        if self.audio != other.audio {
            return Some(format!(
                "audio: held {:?}, asked {:?}",
                self.audio, other.audio
            ));
        }
        if self.subtitle != other.subtitle {
            return Some(format!(
                "subtitle: held {:?}, asked {:?}",
                self.subtitle, other.subtitle
            ));
        }
        None
    }
}

/// A released grant's playback, as a grant that adopts it needs it.
#[derive(Debug, Clone)]
pub struct Released {
    pub play_session_id: String,
    pub item_wire_id: String,
    pub stream_url: String,
    pub remembers_tracks: bool,
    pub shape: StreamShape,
    pub reported_ms: Option<u64>,
}

/// How a released grant ended up; the `outcome` of
/// `pharos_grant_release_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    Adopted,
    EndedAfterGrace,
    EndedMismatch,
    EndedShutdown,
    /// The table was full, so the release ended at once.
    EndedOverflow,
    /// The session knows no device, so no negotiate could be matched to it.
    EndedNoDevice,
}

impl ReleaseOutcome {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Adopted => "adopted",
            Self::EndedAfterGrace => "ended_after_grace",
            Self::EndedMismatch => "ended_mismatch",
            Self::EndedShutdown => "ended_shutdown",
            Self::EndedOverflow => "ended_overflow",
            Self::EndedNoDevice => "ended_no_device",
        }
    }

    pub fn record(self) {
        metrics::counter!("pharos_grant_release_total", "outcome" => self.label()).increment(1);
    }
}

/// What became of a held release, sent to the task that owns its session.
#[derive(Debug)]
pub enum Verdict {
    Adopted,
    /// End it now, for `reason`.
    Ended {
        outcome: ReleaseOutcome,
        reason: String,
    },
}

struct Slot {
    id: u64,
    released: Released,
    verdict: oneshot::Sender<Verdict>,
}

/// A release held by [`PendingReleases::hold`].
pub struct Held {
    pub id: u64,
    pub verdict: oneshot::Receiver<Verdict>,
}

pub struct PendingReleases {
    grace: Duration,
    next_id: AtomicU64,
    slots: Mutex<HashMap<ReleaseKey, Slot>>,
}

impl PendingReleases {
    pub fn new(grace: Duration) -> Self {
        Self {
            grace,
            next_id: AtomicU64::new(0),
            slots: Mutex::new(HashMap::new()),
        }
    }

    pub fn grace(&self) -> Duration {
        self.grace
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ReleaseKey, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds `released` under `key`. A release already held under the key is
    /// ended: the player has moved on. `Err` hands `released` back when the
    /// table is full, for the caller to end at once.
    pub fn hold(&self, key: ReleaseKey, released: Released) -> Result<Held, Box<Released>> {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut slots = self.lock();
        if slots.len() >= MAX_PENDING_RELEASES && !slots.contains_key(&key) {
            return Err(Box::new(released));
        }
        let slot = Slot {
            id,
            released,
            verdict: tx,
        };
        if let Some(old) = slots.insert(key, slot) {
            // The receiver is gone only if its task already ended.
            let _ = old.verdict.send(Verdict::Ended {
                outcome: ReleaseOutcome::EndedMismatch,
                reason: "superseded: the same device released another grant for the same item"
                    .into(),
            });
        }
        Ok(Held { id, verdict: rx })
    }

    /// Takes slot `id` out when its grace is up. `false` means it was already
    /// taken (adopted, rejected or superseded) and its verdict is on the way.
    pub fn expire(&self, key: &ReleaseKey, id: u64) -> bool {
        let mut slots = self.lock();
        if slots.get(key).is_some_and(|s| s.id == id) {
            slots.remove(key);
            true
        } else {
            false
        }
    }

    /// Takes the release held under `key`, if any. The caller must settle the
    /// claim with [`Claim::adopt`] or [`Claim::reject`]; a claim dropped
    /// unsettled ends the playback.
    pub fn claim(&self, key: &ReleaseKey) -> Option<Claim> {
        let slot = self.lock().remove(key)?;
        Some(Claim { slot: Some(slot) })
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A release taken out of the table by the negotiate that may adopt it.
pub struct Claim {
    slot: Option<Slot>,
}

impl Claim {
    pub fn released(&self) -> Option<&Released> {
        self.slot.as_ref().map(|s| &s.released)
    }

    /// The first field of the held stream that `asked` differs in.
    pub fn mismatch(&self, asked: &StreamShape) -> Option<String> {
        self.released().and_then(|r| r.shape.mismatch(asked))
    }

    /// The playback continues under the held session id.
    pub fn adopt(mut self) -> Option<Released> {
        let slot = self.slot.take()?;
        ReleaseOutcome::Adopted.record();
        tracing::info!(
            play_session_id = %pharos_core::secret_prefix(&slot.released.play_session_id),
            "playback: released grant adopted by a new negotiate, playback continues"
        );
        let _ = slot.verdict.send(Verdict::Adopted);
        Some(slot.released)
    }

    /// The held playback is not the one asked for: end it now.
    pub fn reject(mut self, reason: String) {
        self.end(reason);
    }

    fn end(&mut self, reason: String) {
        if let Some(slot) = self.slot.take() {
            tracing::info!(
                play_session_id = %pharos_core::secret_prefix(&slot.released.play_session_id),
                %reason,
                "playback: released grant not adopted, ending it"
            );
            let _ = slot.verdict.send(Verdict::Ended {
                outcome: ReleaseOutcome::EndedMismatch,
                reason,
            });
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.end("negotiate abandoned the claim".into());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn key(item: u64) -> ReleaseKey {
        ReleaseKey {
            user: UserId(uuid::Uuid::nil()),
            device: ClientDeviceId::new("tv"),
            item,
            version: String::new(),
        }
    }

    fn released(psid: &str) -> Released {
        Released {
            play_session_id: psid.into(),
            item_wire_id: "item".into(),
            stream_url: "/u".into(),
            remembers_tracks: true,
            shape: StreamShape {
                decision: Decision::DirectPlay,
                burn_subtitle_indices: BTreeSet::new(),
                audio: Some(1),
                subtitle: None,
            },
            reported_ms: None,
        }
    }

    #[test]
    fn only_one_of_expiry_and_claim_wins() {
        let table = PendingReleases::new(DEFAULT_RELEASE_GRACE);
        let held = table.hold(key(1), released("a")).ok().unwrap();
        let claim = table.claim(&key(1)).unwrap();
        assert!(
            !table.expire(&key(1), held.id),
            "a claimed slot is not expired"
        );
        assert!(claim.mismatch(&released("a").shape).is_none());
        assert_eq!(claim.adopt().unwrap().play_session_id, "a");
        assert!(table.is_empty());

        let held = table.hold(key(1), released("b")).ok().unwrap();
        assert!(table.expire(&key(1), held.id));
        assert!(table.claim(&key(1)).is_none(), "an expired slot is gone");
    }

    #[test]
    fn the_table_is_bounded_and_a_repeat_key_replaces() {
        let table = PendingReleases::new(DEFAULT_RELEASE_GRACE);
        let mut kept = Vec::new();
        for item in 0..MAX_PENDING_RELEASES as u64 {
            kept.push(table.hold(key(item), released("x")).ok().unwrap());
        }
        assert!(table.hold(key(9_999), released("y")).is_err());
        assert_eq!(table.len(), MAX_PENDING_RELEASES);
        // The same key still replaces when full, ending the old one.
        let mut old = kept.remove(0);
        table.hold(key(0), released("z")).ok().unwrap();
        assert!(matches!(
            old.verdict.try_recv(),
            Ok(Verdict::Ended {
                outcome: ReleaseOutcome::EndedMismatch,
                ..
            })
        ));
        assert_eq!(table.len(), MAX_PENDING_RELEASES);
    }

    #[test]
    fn a_mismatch_names_the_field_and_both_values() {
        let table = PendingReleases::new(DEFAULT_RELEASE_GRACE);
        table.hold(key(1), released("a")).ok().unwrap();
        let claim = table.claim(&key(1)).unwrap();
        let mut asked = released("a").shape;
        asked.audio = Some(2);
        let why = claim.mismatch(&asked).unwrap();
        assert!(why.contains("audio") && why.contains("Some(1)") && why.contains("Some(2)"));
    }
}
