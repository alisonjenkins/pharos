//! B24 — SyncPlay membership recovery after a process restart / deploy.
//!
//! Group STATE survives a rolling deploy (the owner replica persists a
//! snapshot to `sync_groups` after every mutation and a new owner hydrates
//! from it), but the session hub's `deviceId → group` map is in-memory — so
//! after a restart every reconnecting socket registered with `group: None`,
//! the persisted group was never re-attached, and every subsequent HTTP
//! command was dropped with only a server-side WARN. Live incident: one
//! member pressed Pause (their client pauses locally first), the server
//! dropped it, the other member kept playing — silent desync until the group
//! was recreated by hand.
//!
//! Member ids are deterministic per device (`hub::member_id_for_device`), so
//! the persisted roster still names the reconnecting device: this lookup
//! scans the persisted snapshots for the device's member id and returns the
//! group to re-attach. Called on `/socket` connect when the hub has no group
//! for the device.

use crate::state::Stores;
use pharos_core::SyncGroupStore;
use pharos_sync::group::snapshot_contains_member;
use pharos_sync::messages::{GroupId, MemberId};

/// How far back a persisted snapshot may date (since its last real mutation)
/// and still be RECOVERED into (or listed by `/SyncPlay/List` — B28).
/// Generous enough for a party paused overnight; old enough that a device is
/// never re-attached to last week's leftovers.
pub const RECOVERY_WINDOW_SECS: i64 = 24 * 3600;

/// Snapshots older than this are garbage-collected by the janitor. Kept past
/// the recovery window so an operator can still inspect a recently-dead group.
const PRUNE_CUTOFF_SECS: i64 = 48 * 3600;

/// How often the janitor sweeps.
const JANITOR_INTERVAL_SECS: u64 = 6 * 3600;

pub fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The persisted group `member_id` belongs to, if any. A handful of rows at
/// most (groups are pruned), each a cheap JSON scan — only runs on a socket
/// connect that has no in-memory membership. Snapshots stale past
/// [`RECOVERY_WINDOW_SECS`] are ignored. Store errors resolve to `None`:
/// recovery is best-effort, the fallback (`NotInGroup` on the next command)
/// keeps the client consistent.
pub async fn find_persisted_group(stores: &Stores, member_id: MemberId) -> Option<GroupId> {
    let cutoff = now_unix_secs() - RECOVERY_WINDOW_SECS;
    let rows = stores.list_sync_groups().await.ok()?;
    rows.iter()
        .filter(|r| r.updated_at >= cutoff)
        .find(|r| snapshot_contains_member(&r.state_json, member_id))
        .and_then(|r| uuid::Uuid::parse_str(&r.group_id).ok())
        .map(GroupId)
}

/// B29 — boot reconciliation: hydrate every FRESH persisted snapshot into a
/// live group actor shortly after startup. Without this, a snapshot whose
/// members never reconnect has NO actor — so the T83 ghost prune never runs
/// against it and the "group" haunts the join picker (B28 lists snapshots)
/// until the 48h janitor. Hydrating hands each orphan to the existing
/// lifecycle: members get MEMBER_TTL_MS to reconnect, no-shows are pruned,
/// an emptied group's actor deletes the snapshot — self-cleaning within
/// minutes. Members who DO return were being recovered anyway (B24); this
/// only accelerates the no-show path. `get_or_create` is ownership-aware
/// under Postgres (one replica hydrates; others get remote handles).
pub fn spawn_boot_reconciliation(stores: Stores, registry: pharos_sync::GroupRegistry) {
    tokio::spawn(async move {
        // Retry rounds (B33): during a rolling deploy the OLD pod is still
        // draining (30s grace) when this fires — it holds each group's
        // advisory lock, so the first `get_or_create` loses the ownership
        // election and yields a REMOTE handle (a bus-forwarder to an owner
        // that is about to die). With nothing else touching the group, no
        // replica ever re-attempted ownership: the group ran with NO actor —
        // no ghost prune, no dissolution — and its snapshot squatted in the
        // picker until the 48h janitor. Ownership is detectable from outside
        // the registry: `snapshot()` answers only from a LOCAL actor (a
        // remote handle drops the reply), so retry each round until every
        // fresh group answers — or the rounds run out (~3.5 min, far past
        // any drain window).
        for round in 0u32..10 {
            tokio::time::sleep(std::time::Duration::from_secs(if round == 0 {
                10
            } else {
                20
            }))
            .await;
            let cutoff = now_unix_secs() - RECOVERY_WINDOW_SECS;
            let rows = match stores.list_sync_groups().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "syncplay: boot reconciliation list failed");
                    return;
                }
            };
            let mut unowned = 0usize;
            for row in rows.iter().filter(|r| r.updated_at >= cutoff) {
                let Ok(gid) = uuid::Uuid::parse_str(&row.group_id).map(GroupId) else {
                    continue;
                };
                match registry.get_or_create(gid).await {
                    Ok(h) => {
                        if h.snapshot().await.is_some() {
                            tracing::info!(group = %gid, round, "syncplay: boot reconciliation hydrated persisted group");
                        } else {
                            // Remote handle — another replica (possibly the
                            // draining old pod) still holds the lock.
                            unowned += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(group = %gid, error = %e, "syncplay: boot reconciliation failed");
                        unowned += 1;
                    }
                }
            }
            if unowned == 0 {
                return;
            }
            tracing::info!(
                round,
                unowned,
                "syncplay: boot reconciliation retrying unowned groups"
            );
        }
        tracing::warn!("syncplay: boot reconciliation gave up with unowned groups remaining");
    });
}

/// T83 — periodic GC for `sync_groups` snapshots. A group actor removes its
/// own snapshot when the group empties, but a snapshot orphaned by a crash /
/// kill (no clean empty) previously lived FOREVER: `prune_sync_groups`
/// existed on the store and nothing ever called it.
pub fn spawn_snapshot_janitor(stores: Stores) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(JANITOR_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match stores
                .prune_sync_groups(now_unix_secs() - PRUNE_CUTOFF_SECS)
                .await
            {
                Ok(0) => {}
                Ok(n) => tracing::info!(pruned = n, "syncplay: stale group snapshots pruned"),
                Err(e) => tracing::warn!(error = %e, "syncplay: snapshot prune failed"),
            }
        }
    });
}
