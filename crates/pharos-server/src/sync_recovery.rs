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

/// The persisted group `member_id` belongs to, if any. A handful of rows at
/// most (groups are pruned), each a cheap JSON scan — only runs on a socket
/// connect that has no in-memory membership. Store errors resolve to `None`:
/// recovery is best-effort, the fallback (`NotInGroup` on the next command)
/// keeps the client consistent.
pub async fn find_persisted_group(stores: &Stores, member_id: MemberId) -> Option<GroupId> {
    let rows = stores.list_sync_groups().await.ok()?;
    rows.iter()
        .find(|r| snapshot_contains_member(&r.state_json, member_id))
        .and_then(|r| uuid::Uuid::parse_str(&r.group_id).ok())
        .map(GroupId)
}
