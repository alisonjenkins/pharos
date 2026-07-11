//! Group-state persistence hook (Phase B4.3c — zero-downtime deploys).
//!
//! So a watch party survives a rolling deploy, the owning replica writes the
//! group's whole coordination state to a shared store after every mutation.
//! When that replica drains, another replica hydrates the group from the last
//! snapshot and takes over as coordinator (B4.3d).
//!
//! Like [`crate::delivery::Delivery`], this is a fire-and-forget, object-safe
//! trait the server implements over its concrete `SyncGroupStore` — so
//! pharos-sync stays free of any storage dependency. The single-replica /
//! SQLite path passes no persistence at all (its groups never leave the
//! process); only the Postgres multi-replica path wires it.

use crate::messages::GroupId;

/// Persist / drop a group's serialized snapshot. Fire-and-forget: the impl
/// spawns the actual (async) store write, so the group actor never blocks on
/// I/O. `epoch_unix_ms` is stored alongside the blob because a replica taking
/// over must reuse the same wall-clock time base (see [`crate::group`]).
pub trait GroupPersistence: Send + Sync + 'static {
    fn persist(&self, group_id: GroupId, epoch_unix_ms: u64, state_json: String);
    fn remove(&self, group_id: GroupId);
}
