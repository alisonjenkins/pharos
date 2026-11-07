//! Where a playback is: the progress a player reports while it plays and the
//! stop that ends it. Both wire layers report here, so a native player and
//! jellyfin-web move the same resume point, the same Currently Watching row
//! and the same watched mark.

use std::time::Instant;

use pharos_core::{MediaStore, PlaybackProgress, User, UserDataStore, UserId};

use crate::progress_writes::{self, Decision, ProgressKey};
use crate::services::access::{check_item, ItemHidden};
use crate::sessions::SessionEvent;
use crate::state::AppState;
use crate::sync_recovery::now_unix_secs;

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
        return Ok(());
    };
    let position = r.position_ticks;
    // The stop below is this session's final write; a report still
    // held for it is older and must not follow it into the database.
    state
        .progress_writes
        .forget(&ProgressKey::new(user.id, item_id, r.play_session_id));
    let runtime = state
        .stores
        .get(item_id)
        .await
        .ok()
        .and_then(|it| it.probe.run_time_ticks())
        .unwrap_or(0);
    // P36 — threshold configurable via [server].played_threshold_pct
    // (default 90, clamped to [50, 100] at boot).
    let played_complement = 100u64.saturating_sub(state.played_threshold_pct as u64);
    let finished =
        runtime > 0 && position >= runtime.saturating_sub(runtime * played_complement / 100);
    let Ok(mut data) = state.stores.get_user_data(user.id, item_id).await else {
        return Ok(());
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
    if state
        .stores
        .set_user_data(user.id, item_id, data)
        .await
        .is_ok()
    {
        // B36 — full DTO so the just-finished (or resumable)
        // item's card flips its watched tick / resume bar
        // everywhere without a refresh. `runtime` was
        // already fetched above for the finished check.
        announce(state, user, item_id, data, runtime);
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
