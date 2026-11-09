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
use pharos_core::{PersistedSyncGroup, SyncGroupStore};
use pharos_sync::group::snapshot_contains_member;
use pharos_sync::messages::{GroupId, MemberId};

/// How far back a persisted snapshot may date (since its last real mutation)
/// and still be RECOVERED into (or listed by `/SyncPlay/List` — B28).
/// Generous enough for a party paused overnight; old enough that a device is
/// never re-attached to last week's leftovers.
pub const RECOVERY_WINDOW_SECS: i64 = 24 * 3600;

/// The oldest `updated_at` (unix secs) a snapshot may carry and still count as
/// recoverable.
pub fn recovery_cutoff() -> i64 {
    now_unix_secs() - RECOVERY_WINDOW_SECS
}

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

/// Whether `group_id` has a persisted snapshot fresh enough to be recovered
/// (the same window `/SyncPlay/List` offers it in — B28) AND readable (the
/// parse B28 lists it through). A group present only as such a snapshot is
/// still joinable: Join hydrates it. A fresh snapshot that does not parse
/// is no group, so Join answers GroupDoesNotExist instead of letting
/// `get_or_create` fail on it. Store errors count as "no" (best-effort, like
/// [`find_persisted_group`]).
async fn has_usable_snapshot(stores: &Stores, group_id: GroupId) -> bool {
    let row = match stores.get_sync_group(&group_id.to_string()).await {
        Ok(Some(row)) if row.updated_at >= recovery_cutoff() => row,
        Ok(_) => return false,
        Err(e) => {
            tracing::warn!(group = %group_id, error = %e, "syncplay: persisted group snapshot unreadable");
            return false;
        }
    };
    if pharos_sync::group::snapshot_summary(&row.state_json).is_none() {
        tracing::warn!(group = %group_id, "syncplay: fresh persisted group snapshot is corrupt; treating the group as nonexistent");
        return false;
    }
    true
}

/// Where a Join's target group stands.
pub enum GroupLocation {
    /// Running in this process's registry.
    Live(pharos_sync::group::GroupHandle),
    /// Not in this registry, but joinable: a fresh readable snapshot holds
    /// it, or another replica holds its lease (it is live there). Joining
    /// goes through `get_or_create`, which hydrates or builds a remote router.
    Cold,
    /// Neither. A stale id must not spawn a phantom empty group (V582).
    Absent,
}

/// Classify `group_id` for a Join without creating or hydrating anything.
pub async fn locate_group(
    stores: &Stores,
    registry: &pharos_sync::GroupRegistry,
    group_id: GroupId,
) -> Result<GroupLocation, pharos_sync::registry::RegistryError> {
    if let Some(h) = registry.get(group_id).await? {
        // A cached REMOTE handle is not live here: boot reconciliation caches
        // one while the old pod holds the lock, and a join through it sends
        // AddMember to an owner that may be gone. Re-elect through `Cold`,
        // which hydrates if the lock is free and yields the remote handle
        // again if the owner really is alive.
        if registry.is_owned(group_id).await? || !has_usable_snapshot(stores, group_id).await {
            return Ok(GroupLocation::Live(h));
        }
        return Ok(GroupLocation::Cold);
    }
    // A snapshot's `updated_at` only moves on mutation, so a party paused
    // for a day is stale while still live on its owner replica.
    if has_usable_snapshot(stores, group_id).await || registry.held_elsewhere(group_id).await? {
        return Ok(GroupLocation::Cold);
    }
    Ok(GroupLocation::Absent)
}

/// The group `user` may join: `None` when it does not exist OR the user may
/// not see it, so a hidden group cannot be told from an absent one (Jellyfin
/// answers `LibraryAccessDenied` for the latter, which confirms the group
/// exists). The permission check runs before a cold group is hydrated, so a
/// refused probe leaves the registry untouched.
pub(crate) async fn joinable_group(
    state: &crate::state::AppState,
    registry: &pharos_sync::GroupRegistry,
    user: &pharos_core::User,
    group_id: GroupId,
    route: crate::api::jellyfin::syncplay::GroupHiddenRoute,
) -> Result<Option<pharos_sync::group::GroupHandle>, pharos_sync::registry::RegistryError> {
    use crate::api::jellyfin::syncplay::{may_join, may_join_cold};
    match locate_group(&state.stores, registry, group_id).await? {
        GroupLocation::Live(h) => Ok(may_join(state, user, &h, route).await.then_some(h)),
        GroupLocation::Cold => {
            if !may_join_cold(state, user, group_id, route).await {
                return Ok(None);
            }
            registry.get_or_create(group_id).await.map(Some)
        }
        GroupLocation::Absent => Ok(None),
    }
}

/// The persisted group `member_id` belongs to, if any. A handful of rows at
/// most (groups are pruned), each a cheap JSON scan — only runs on a socket
/// connect that has no in-memory membership. Snapshots stale past
/// [`RECOVERY_WINDOW_SECS`] are ignored. Store errors resolve to `None`:
/// recovery is best-effort, the fallback (`NotInGroup` on the next command)
/// keeps the client consistent.
pub async fn find_persisted_group(stores: &Stores, member_id: MemberId) -> Option<GroupId> {
    let cutoff = recovery_cutoff();
    let rows = stores.list_sync_groups().await.ok()?;
    pick_recovery_group(&rows, cutoff, |r| {
        snapshot_contains_member(&r.state_json, member_id)
    })
    .and_then(|r| uuid::Uuid::parse_str(&r.group_id).ok())
    .map(GroupId)
}

/// The fresh snapshot (`updated_at >= cutoff`) that `contains` accepts, newest
/// wins. Several snapshots can name the same deterministic member id (a device
/// that joined two parties, or a stale copy), so the choice must not depend on
/// the order the rows arrive in: the greatest `updated_at` wins, and an exact
/// timestamp tie goes to the smaller group id (the store's list order).
fn pick_recovery_group(
    rows: &[PersistedSyncGroup],
    cutoff: i64,
    contains: impl Fn(&PersistedSyncGroup) -> bool,
) -> Option<&PersistedSyncGroup> {
    rows.iter()
        .filter(|r| r.updated_at >= cutoff && contains(r))
        .max_by(|a, b| {
            a.updated_at
                .cmp(&b.updated_at)
                .then_with(|| b.group_id.cmp(&a.group_id))
        })
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
            let cutoff = recovery_cutoff();
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    fn row(group_id: &str, updated_at: i64) -> PersistedSyncGroup {
        PersistedSyncGroup {
            group_id: group_id.to_string(),
            epoch_unix_ms: 1_000,
            state_json: "{}".into(),
            updated_at,
        }
    }

    /// Rows arrive oldest-first (as an unordered scan would deliver them). Both
    /// snapshots match the member, so the newer one must be chosen, not the
    /// first one scanned.
    #[test]
    fn recovery_picks_most_recent_matching_snapshot() {
        let rows = vec![row("group-old", 100), row("group-new", 900)];
        let got = pick_recovery_group(&rows, 0, |_| true).map(|r| r.group_id.as_str());
        assert_eq!(got, Some("group-new"));
    }

    #[test]
    fn recovery_ignores_newer_snapshot_that_lacks_the_member() {
        let rows = vec![row("group-old", 100), row("group-new", 900)];
        let got = pick_recovery_group(&rows, 0, |r| r.group_id == "group-old")
            .map(|r| r.group_id.as_str());
        assert_eq!(got, Some("group-old"));
    }

    #[test]
    fn recovery_skips_stale_snapshots_below_cutoff() {
        let rows = vec![row("group-stale", 100), row("group-fresh", 500)];
        let got = pick_recovery_group(&rows, 200, |_| true).map(|r| r.group_id.as_str());
        assert_eq!(got, Some("group-fresh"));
    }

    #[test]
    fn recovery_timestamp_tie_is_order_independent() {
        let a = vec![row("group-b", 500), row("group-a", 500)];
        let b = vec![row("group-a", 500), row("group-b", 500)];
        let pick = |rows: &[PersistedSyncGroup]| {
            pick_recovery_group(rows, 0, |_| true).map(|r| r.group_id.clone())
        };
        assert_eq!(pick(&a), Some("group-a".to_string()));
        assert_eq!(pick(&b), Some("group-a".to_string()));
    }

    #[test]
    fn recovery_cutoff_is_one_window_before_now() {
        let before = now_unix_secs() - RECOVERY_WINDOW_SECS;
        let cutoff = recovery_cutoff();
        let after = now_unix_secs() - RECOVERY_WINDOW_SECS;
        assert!((before..=after).contains(&cutoff));
    }

    struct Own {
        map: Arc<Mutex<Option<usize>>>,
        me: usize,
    }
    impl pharos_sync::OwnershipSource for Own {
        fn try_own(&self, _g: GroupId) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
            Box::pin(async move {
                let mut m = self.map.lock().unwrap();
                match *m {
                    Some(o) if o != self.me => false,
                    _ => {
                        *m = Some(self.me);
                        true
                    }
                }
            })
        }
        fn release(&self, _g: GroupId) {}
    }

    #[derive(Clone)]
    struct Snap(Arc<Mutex<Option<String>>>);
    impl pharos_sync::GroupPersistence for Snap {
        fn persist(&self, _g: GroupId, _e: u64, json: String) {
            *self.0.lock().unwrap() = Some(json);
        }
        fn remove(&self, _g: GroupId) {}
    }
    impl pharos_sync::HydrationSource for Snap {
        fn load(
            &self,
            _g: GroupId,
        ) -> Pin<Box<dyn Future<Output = Option<(u64, String)>> + Send + '_>> {
            let got = self.0.lock().unwrap().clone().map(|j| (1_000, j));
            Box::pin(async move { got })
        }
    }

    struct Nowhere;
    impl pharos_sync::CommandSink for Nowhere {
        fn submit(&self, _g: GroupId, _c: pharos_sync::group::RemoteCommand) {}
    }

    /// B1006 — a join before boot reconciliation owns the group. Rounds 0 and
    /// 1 cached a remote handle while the old pod held the lock; the lock then
    /// freed. `locate_group` must send the join through `Cold` (hydrate), not
    /// treat the cached remote handle as live, or its AddMember reaches no
    /// actor.
    /// Disarm check: with the remote handle taken as `Live`, the first
    /// assertion fails.
    #[tokio::test]
    async fn a_join_before_reconciliation_hydrates_instead_of_using_the_remote_handle() {
        use pharos_core::{PersistedSyncGroup, SyncGroupStore};
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let snap = Snap(Arc::new(Mutex::new(None)));
        let gid = GroupId::new();
        let member = pharos_sync::hub::member_id_for_device("dev");
        let (reply, joined) = tokio::sync::oneshot::channel();
        let actor = pharos_sync::group::GroupHandle::spawn_persistent(
            gid,
            1_000,
            Arc::new(pharos_sync::LocalDelivery::new(
                pharos_sync::MemberSinks::new(),
            )),
            Arc::new(snap.clone()),
            None,
        );
        actor
            .tx
            .send(pharos_sync::group::GroupMsg::AddMember {
                user: None,
                member_id: member,
                name: "m".into(),
                reply,
            })
            .await
            .unwrap();
        joined.await.unwrap();
        let state_json = loop {
            if let Some(j) = snap.0.lock().unwrap().clone() {
                break j;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        let now = now_unix_secs();
        stores
            .upsert_sync_group(
                &PersistedSyncGroup {
                    group_id: gid.to_string(),
                    epoch_unix_ms: 1_000,
                    state_json,
                    updated_at: now,
                },
                now,
            )
            .await
            .unwrap();

        // The old pod (replica 2) holds the lock; this replica (1) caches a
        // remote handle, as reconciliation round 0 did.
        let owner = Arc::new(Mutex::new(Some(2usize)));
        let registry = pharos_sync::GroupRegistry::spawn_distributed(
            Arc::new(pharos_sync::LocalDelivery::new(
                pharos_sync::MemberSinks::new(),
            )),
            pharos_sync::Distributed {
                ownership: Arc::new(Own {
                    map: owner.clone(),
                    me: 1,
                }),
                hydration: Arc::new(snap.clone()),
                commands: Arc::new(Nowhere),
                persistence: Arc::new(snap),
            },
        );
        let remote = registry.get_or_create(gid).await.unwrap();
        assert!(remote.snapshot().await.is_none(), "setup: handle is remote");
        *owner.lock().unwrap() = None;

        let GroupLocation::Cold = locate_group(&stores, &registry, gid).await.unwrap() else {
            panic!("a cached remote handle was taken as live");
        };
        let h = registry.get_or_create(gid).await.unwrap();
        let (reply, joined) = tokio::sync::oneshot::channel();
        h.tx.send(pharos_sync::group::GroupMsg::AddMember {
            user: None,
            member_id: member,
            name: "m".into(),
            reply,
        })
        .await
        .unwrap();
        joined.await.unwrap();
        assert_eq!(h.snapshot().await.unwrap().member_count, 1);
    }
}
