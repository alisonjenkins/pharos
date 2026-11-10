//! Where a playback is: the progress a player reports while it plays and the
//! stop that ends it. Both wire layers report here, so a native player and
//! jellyfin-web move the same resume point, the same Currently Watching row
//! and the same watched mark.

use std::time::Instant;

use pharos_core::{MediaKind, MediaStore, PlaybackProgress, User, UserDataStore, UserId};

use crate::progress_writes::{self, Decision, ProgressKey};
use crate::services::access::{check_item, ItemHidden};
use crate::sessions::SessionEvent;
use crate::state::AppState;
use crate::sync_recovery::now_unix_secs;

/// Spec 038 FR-107, ADR-0114: the smallest position kept as a resume point.
/// A play that never got past it (a trailer peek, a wrong click) leaves no
/// trace in Continue Watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinResume {
    /// Video only: this share of the runtime, when it is longer than `secs`.
    pub pct: u32,
    /// Video and songs: at least this many seconds in.
    pub secs: u32,
}

impl Default for MinResume {
    fn default() -> Self {
        Self { pct: 2, secs: 60 }
    }
}

/// Which minimum a position fell under, for the metric labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BelowMinimum {
    Video,
    Song,
}

impl BelowMinimum {
    pub const ALL: [BelowMinimum; 2] = [BelowMinimum::Video, BelowMinimum::Song];

    pub const fn label(self) -> &'static str {
        match self {
            BelowMinimum::Video => "video",
            BelowMinimum::Song => "song",
        }
    }
}

impl MinResume {
    /// No minimum: every position is kept.
    pub const OFF: MinResume = MinResume { pct: 0, secs: 0 };

    /// Whether `position` (ticks) on an item of `kind` and `runtime` (ticks,
    /// 0 when unknown) is too early to keep. Books have no minimum.
    pub fn below(self, kind: MediaKind, runtime: u64, position: u64) -> Option<BelowMinimum> {
        let secs = u64::from(self.secs) * pharos_core::time::TICKS_PER_SECOND;
        let (floor, which) = match kind {
            MediaKind::Movie | MediaKind::Episode => (
                secs.max(runtime / 100 * u64::from(self.pct)),
                BelowMinimum::Video,
            ),
            MediaKind::Audio => (secs, BelowMinimum::Song),
            MediaKind::Book => return None,
        };
        (position < floor).then_some(which)
    }
}

/// What a stop did with the user's play state. The `label()` pairs are a
/// dashboard contract for `pharos_play_stop_total{outcome,reason}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The position was stored as the resume point.
    Saved,
    /// The stop was past the played threshold: marked played, position reset.
    Finished,
    /// The stop named no item, so there was nothing to store.
    NoItem,
    /// The item was removed since it started playing.
    ItemMissing,
    /// The item could not be read, so its runtime is unknown.
    ItemReadFailed,
    /// The user's stored play state could not be read.
    UserDataReadFailed,
    /// The new play state could not be written.
    UserDataWriteFailed,
}

impl StopOutcome {
    pub const ALL: [StopOutcome; 7] = [
        StopOutcome::Saved,
        StopOutcome::Finished,
        StopOutcome::NoItem,
        StopOutcome::ItemMissing,
        StopOutcome::ItemReadFailed,
        StopOutcome::UserDataReadFailed,
        StopOutcome::UserDataWriteFailed,
    ];

    /// `(outcome, reason)` as the metric labels them.
    pub const fn label(self) -> (&'static str, &'static str) {
        match self {
            StopOutcome::Saved => ("saved", "position"),
            StopOutcome::Finished => ("finished", "past_threshold"),
            StopOutcome::NoItem => ("skipped", "no_item"),
            StopOutcome::ItemMissing => ("skipped", "item_missing"),
            StopOutcome::ItemReadFailed => ("store_error", "item_read"),
            StopOutcome::UserDataReadFailed => ("store_error", "user_data_read"),
            StopOutcome::UserDataWriteFailed => ("store_error", "user_data_write"),
        }
    }

    fn record(self) {
        let (outcome, reason) = self.label();
        metrics::counter!("pharos_play_stop_total", "outcome" => outcome, "reason" => reason)
            .increment(1);
    }
}

/// One report from a player.
pub struct Report<'a> {
    pub play_session_id: Option<&'a str>,
    /// The item as the client names it on the wire.
    pub item_wire_id: Option<&'a str>,
    pub position_ticks: u64,
    pub is_paused: bool,
    /// The tracks the player is actually playing. `None` means the report
    /// says nothing about them, which must not erase a stored choice.
    pub audio_stream_index: Option<i32>,
    pub subtitle_stream_index: Option<i32>,
}

/// Why a playback report was not recorded.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// The item is outside the caller's policy (B277). Nothing was written.
    #[error("item withheld from the caller: {0:?}")]
    Withheld(ItemHidden),
    /// The session registry (V18) is gone: nothing about the playback could
    /// be recorded.
    #[error("session registry: {0}")]
    RegistryDown(String),
    /// A play-state read or write on the stop failed (spec 038 FR-108). The
    /// playback has ended; the position was not stored, so a retry can.
    #[error("play state {step}: {source}")]
    Store {
        step: &'static str,
        #[source]
        source: pharos_core::DomainError,
    },
}

/// A player starting an item: the now-playing entry `/Sessions` lists.
pub struct Start<'a> {
    pub play_session_id: String,
    pub user: &'a User,
    pub device_id: pharos_core::ClientDeviceId,
    pub device_name: String,
    pub client: String,
    pub version: String,
    /// The item as the client names it on the wire.
    pub item_wire_id: String,
    pub position_ticks: u64,
    pub supports_remote_control: bool,
}

/// Starts the live session a report then moves, and bumps the item to the
/// front of the trickplay pre-generator. Both wire layers start here, so a
/// native player and jellyfin-web show up in `/Sessions` the same way. The
/// caller has already checked the user may play the item.
pub async fn start(state: &AppState, s: Start<'_>) -> Result<(), ReportError> {
    // The playback-report path is the ground truth for "watched right
    // now": not every client calls PlaybackInfo, and a restarted pod would
    // otherwise never re-learn what is playing. Best-effort; the worker
    // dedupes.
    if let Some(tx) = &state.trickplay_priority {
        if let Some(id) = pharos_jellyfin_api::dto::parse_item_id(&s.item_wire_id) {
            let _ = tx.send(id);
        }
    }
    tracing::info!(
        play_session_id = %pharos_core::secret_prefix(&s.play_session_id),
        user_id = %s.user.id.0.simple(),
        device_id = %s.device_id,
        client = %s.client,
        item = %s.item_wire_id,
        "playback: session started"
    );
    state
        .sessions
        .apply(SessionEvent::Started {
            session_id: s.play_session_id,
            user_id: s.user.id,
            user_name: s.user.name.clone(),
            device_id: s.device_id,
            device_name: s.device_name,
            client: s.client,
            version: s.version,
            item_id: s.item_wire_id,
            position_ticks: s.position_ticks,
            supports_remote_control: s.supports_remote_control,
        })
        .await
        .map_err(|e| ReportError::RegistryDown(e.to_string()))
}

/// Records a progress report: the live session, the sockets' Currently
/// Watching, and (coalesced by `progress_writes`) the resume position.
pub async fn report(state: &AppState, user: &User, r: Report<'_>) -> Result<(), ReportError> {
    let item_wire_id = r.item_wire_id.unwrap_or("");
    let parsed_item_id = pharos_jellyfin_api::dto::parse_item_id(item_wire_id);
    if let Some(id) = parsed_item_id {
        check_item(state, user, id, "playback")
            .await
            .map_err(ReportError::Withheld)?;
    }
    // One read serves both the broadcast's kind and the DTO's runtime.
    let item = match parsed_item_id {
        Some(id) => state.stores.get(id).await.ok(),
        None => None,
    };

    if let Some(session_id) = r.play_session_id {
        state
            .sessions
            .apply(SessionEvent::Progress {
                session_id: session_id.to_owned(),
                item_id: item_wire_id.to_owned(),
                position_ticks: r.position_ticks,
                is_paused: r.is_paused,
            })
            .await
            .map_err(|e| ReportError::RegistryDown(e.to_string()))?;
        // P10 — fan out to /socket subscribers so the Currently
        // Watching sidebar updates live without polling.
        // Resolve the item's kind so the broadcast can stamp the
        // kotlin-REQUIRED `NowPlayingItem.Type` (a Type-less BaseItemDto
        // throws MissingFieldException and crashes native Android TV — B78).
        let item_kind = item.as_ref().map(|m| m.kind);
        state.notify_playback_progress(
            session_id,
            &user.id.0.simple().to_string(),
            item_wire_id,
            item_kind,
            r.position_ticks,
            r.is_paused,
        );
    }

    // T33: persist the resume position so the Resume row picks the
    // item up across sessions. The session-snapshot path above lives
    // and dies with the in-process actor (V18) and won't survive
    // restarts.
    // parse_item_id: clients echo the canonical 32-hex wire id (B15) — a
    // plain decimal parse silently failed here, so resume positions
    // stopped persisting for hex-era clients.
    let Some(item_id) = parsed_item_id else {
        return Ok(());
    };
    // Keep nudging the pre-generator from progress too: covers a server
    // restart mid-episode (session started before this process) and
    // clients that skip /Sessions/Playing. Deduped by the worker, so the
    // every-~10s cadence costs one HashSet probe.
    if let Some(tx) = &state.trickplay_priority {
        let _ = tx.send(item_id);
    }
    let progress = PlaybackProgress {
        position_ticks: r.position_ticks,
        at: now_unix_secs(),
        audio_stream_index: r.audio_stream_index,
        subtitle_stream_index: r.subtitle_stream_index,
        only_if_newer: false,
    };
    let key = ProgressKey::new(user.id, item_id, r.play_session_id);
    // Only the database write is deferred: the session registry and the
    // socket fan-out above already ran for this report.
    let decision = state
        .progress_writes
        .admit(&key, Instant::now(), r.is_paused, progress);
    let Decision::Persist(reason) = decision else {
        progress_writes::count_held();
        return Ok(());
    };
    match progress_writes::persist(&state.stores, &key, progress, reason.label(), false).await {
        Ok(data) => {
            state
                .progress_writes
                .persisted(&key, Instant::now(), &progress);
            // B36 — broadcast the full DTO (with PlayedPercentage
            // from the item's runtime) so other windows' resume
            // bars track the live position.
            let runtime = item
                .as_ref()
                .and_then(|it| it.probe.run_time_ticks())
                .unwrap_or(0);
            announce(state, user, item_id, data, runtime);
        }
        Err(e) => {
            tracing::warn!(error = %e, item = item_id, "playback progress not persisted");
            // Keep it for the background tick to retry.
            state.progress_writes.hold(&key, progress);
        }
    }
    Ok(())
}

/// Ends `owner`'s playback `session_id`: its stream capability and its
/// now-playing entry. It reads and writes nothing of the user's, so it needs
/// no live session: a grant released after its token was revoked still ends
/// what it started.
pub async fn end(
    state: &AppState,
    owner: UserId,
    session_id: &pharos_core::PlaySessionId,
) -> Result<(), ReportError> {
    // B256: the PlaySessionId authorizes streams on its own, so the
    // stop that ends the playback ends the capability too.
    if let Err(e) = state.transcode_sessions.end_held(session_id, owner).await {
        tracing::warn!(error = %e, "playback stop: transcode session end failed");
    }
    state
        .sessions
        .apply(SessionEvent::Stopped {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|e| ReportError::RegistryDown(e.to_string()))
}

/// Records the end of a playback: ends its session and its stream
/// capability, then stores the final position, or marks the item played
/// when the player stopped near its end.
pub async fn stop(state: &AppState, user: &User, r: Report<'_>) -> Result<(), ReportError> {
    if let Some(id) = r
        .item_wire_id
        .and_then(pharos_jellyfin_api::dto::parse_item_id)
    {
        check_item(state, user, id, "playback")
            .await
            .map_err(ReportError::Withheld)?;
    }
    if let Some(session_id) = r.play_session_id {
        end(
            state,
            user.id,
            &pharos_core::PlaySessionId::from(session_id),
        )
        .await?;
    }

    // Persist final UserData. If the client stopped within the last
    // ~10% of the runtime, treat as "finished" — mark played, reset
    // resume position. Otherwise save the position so the Resume row
    // picks the item up later. Without this the Resume row holds
    // every finished item forever (jellyfin-web only sends an
    // explicit /PlayedItems POST on manual mark-played).
    // parse_item_id: clients echo the canonical 32-hex wire id (B15) — a
    // plain decimal parse silently failed here, so resume positions
    // stopped persisting for hex-era clients.
    let Some(item_id) = r
        .item_wire_id
        .and_then(pharos_jellyfin_api::dto::parse_item_id)
    else {
        StopOutcome::NoItem.record();
        return Ok(());
    };
    let position = r.position_ticks;
    let progress_key = ProgressKey::new(user.id, item_id, r.play_session_id);
    let runtime = match state.stores.get(item_id).await {
        Ok(item) => item.probe.run_time_ticks().unwrap_or(0),
        // Removed since it started playing: there is no row to store on.
        Err(pharos_core::DomainError::NotFound(_)) => {
            StopOutcome::ItemMissing.record();
            state.progress_writes.forget(&progress_key);
            return Ok(());
        }
        Err(error) => {
            StopOutcome::ItemReadFailed.record();
            tracing::warn!(%error, item = item_id, position, "playback stop: item read failed, nothing stored");
            return Err(ReportError::Store {
                step: "item read",
                source: error,
            });
        }
    };
    // P36 — threshold configurable via [server].played_threshold_pct
    // (default 90, clamped to [50, 100] at boot).
    let played_complement = 100u64.saturating_sub(state.played_threshold_pct as u64);
    let finished =
        runtime > 0 && position >= runtime.saturating_sub(runtime * played_complement / 100);
    let mut data = match state.stores.get_user_data(user.id, item_id).await {
        Ok(data) => data,
        Err(error) => {
            StopOutcome::UserDataReadFailed.record();
            tracing::warn!(%error, item = item_id, position, "playback stop: user data read failed, nothing stored");
            return Err(ReportError::Store {
                step: "user data read",
                source: error,
            });
        }
    };
    if finished {
        data.played = true;
        data.play_count = data.play_count.saturating_add(1);
        data.last_played_position_ticks = 0;
    } else {
        data.last_played_position_ticks = position;
    }
    data.last_played_at = now_unix_secs();
    if r.audio_stream_index.is_some() {
        data.audio_stream_index = r.audio_stream_index;
    }
    if r.subtitle_stream_index.is_some() {
        data.subtitle_stream_index = r.subtitle_stream_index;
    }
    match state.stores.set_user_data(user.id, item_id, data).await {
        Ok(()) => {
            // The stop is this session's final write; a report still held
            // for it is older. On a failed stop it stays held, so the
            // background flush still saves the last reported position.
            state.progress_writes.forget(&progress_key);
            if finished {
                StopOutcome::Finished.record();
            } else {
                StopOutcome::Saved.record();
            }
            // B36 — full DTO so the just-finished (or resumable)
            // item's card flips its watched tick / resume bar
            // everywhere without a refresh. `runtime` was
            // already fetched above for the finished check.
            announce(state, user, item_id, data, runtime);
        }
        Err(error) => {
            StopOutcome::UserDataWriteFailed.record();
            tracing::warn!(%error, item = item_id, position, finished, "playback stop: user data write failed");
            return Err(ReportError::Store {
                step: "user data write",
                source: error,
            });
        }
    }
    Ok(())
}

fn announce(
    state: &AppState,
    user: &User,
    item_id: u64,
    data: pharos_core::UserItemData,
    runtime: u64,
) {
    let dto =
        pharos_jellyfin_api::dto::UserItemDataDto::from_domain_with_runtime(item_id, data, runtime);
    if let Ok(entry) = serde_json::to_value(&dto) {
        state.notify_user_data_changed(&user.id.0.simple().to_string(), vec![entry]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pharos_core::time::TICKS_PER_SECOND as S;

    #[test]
    fn a_video_needs_the_larger_of_two_percent_and_a_minute() {
        let min = MinResume::default();
        let film = 2 * 60 * 60 * S;
        // 2% of two hours is 144 s, past the minute.
        assert_eq!(
            min.below(MediaKind::Movie, film, 143 * S),
            Some(BelowMinimum::Video)
        );
        assert_eq!(min.below(MediaKind::Movie, film, 144 * S), None);
        let short = 20 * 60 * S;
        // 2% of twenty minutes is 24 s, so the minute holds.
        assert_eq!(
            min.below(MediaKind::Episode, short, 59 * S),
            Some(BelowMinimum::Video)
        );
        assert_eq!(min.below(MediaKind::Episode, short, 60 * S), None);
        assert_eq!(
            min.below(MediaKind::Episode, 0, 59 * S),
            Some(BelowMinimum::Video)
        );
    }

    #[test]
    fn a_song_needs_a_minute_and_a_book_nothing() {
        let min = MinResume::default();
        let song = 4 * 60 * S;
        assert_eq!(
            min.below(MediaKind::Audio, song, 59 * S),
            Some(BelowMinimum::Song)
        );
        assert_eq!(min.below(MediaKind::Audio, song, 60 * S), None);
        assert_eq!(min.below(MediaKind::Book, 0, 0), None);
    }

    #[test]
    fn a_zero_minimum_keeps_every_position() {
        let off = MinResume::OFF;
        assert_eq!(off.below(MediaKind::Movie, 60 * S, 1), None);
        assert_eq!(off.below(MediaKind::Audio, 60 * S, 1), None);
    }

    #[test]
    fn below_minimum_labels_are_distinct() {
        let labels: std::collections::HashSet<_> =
            BelowMinimum::ALL.iter().map(|b| b.label()).collect();
        assert_eq!(labels.len(), BelowMinimum::ALL.len());
    }
}
