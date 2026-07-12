//! Session hub — bridges Jellyfin's **HTTP** SyncPlay commands to the
//! **WebSocket** member sinks the group engine broadcasts to.
//!
//! Stock jellyfin-web drives SyncPlay by POSTing `/SyncPlay/{New,Join,Pause,
//! Seek,SetNewQueue,…}` (identified by the client's `deviceId`) and only
//! *receives* commands/updates over `/socket`. The group engine, however, keys
//! members by a per-socket [`MemberId`] and owns only the socket's `ServerMsg`
//! sink. This hub is the missing map: `deviceId → {member_id, sink, group}`.
//!
//! - The `/socket` task [`register`](SessionHub::register)s on connect and
//!   [`deregister`](SessionHub::deregister)s on disconnect.
//! - An HTTP handler [`resolve`](SessionHub::resolve)s the caller's `deviceId`
//!   to drive the engine (create/join a group, send a `GroupMsg`), using the
//!   session's own sink for `AddMember`.
//!
//! Backed by a `DashMap` so concurrent socket connects/disconnects and HTTP
//! command handlers across actix workers don't contend on a single lock.

use crate::group::GroupHandle;
use crate::messages::{MemberId, ServerMsg};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Fixed UUIDv5 namespace for deriving a device's [`MemberId`] from its
/// `deviceId`. DETERMINISTIC on purpose (B24): the persisted group snapshot
/// stores members by `MemberId`, so a member id that survives a process
/// restart is what lets a reconnecting device be recognised as an existing
/// member of a persisted group and re-attached — a random per-process id
/// orphaned every membership on deploy.
const MEMBER_ID_NS: uuid::Uuid = uuid::Uuid::from_u128(0x9e1b_52fa_7c44_4bd0_a3ce_8d6f_1b2a_4c33);

/// The stable member id for a device: same device (`deviceId`) → same id,
/// across reconnects AND process restarts.
pub fn member_id_for_device(device_id: &str) -> MemberId {
    MemberId(uuid::Uuid::new_v5(&MEMBER_ID_NS, device_id.as_bytes()))
}

struct SessionEntry {
    /// Stable per-device member id — kept ACROSS socket reconnects so a
    /// reconnecting client stays the same group member (the engine keys members
    /// by this). Minted once, on the device's first `/socket`.
    member_id: MemberId,
    name: String,
    sink: mpsc::Sender<ServerMsg>,
    /// The group this session currently belongs to, if any. Set by the HTTP
    /// New/Join handler (via [`attach_group`](SessionHub::attach_group)),
    /// cleared on Leave. Deliberately SURVIVES a socket disconnect so a
    /// reconnect re-attaches instead of orphaning the membership.
    group: Option<GroupHandle>,
    /// Connection generation — bumped on every `/socket` (re)connect for this
    /// device. A disconnecting socket captures its generation and only tears the
    /// membership down if no newer socket has connected since (see
    /// [`remove_if_current_gen`](SessionHub::remove_if_current_gen)).
    conn_gen: u64,
}

/// A session resolved for an HTTP SyncPlay command handler. Clones out of the
/// hub so the handler never holds the map shard lock across an `await`.
pub struct ResolvedSession {
    pub member_id: MemberId,
    pub name: String,
    pub sink: mpsc::Sender<ServerMsg>,
    pub group: Option<GroupHandle>,
}

/// Outcome of a `/socket` (re)connect registering with the hub.
pub struct Registered {
    /// The device's stable member id (new on first connect, reused on reconnect).
    pub member_id: MemberId,
    /// The group the device is already a member of, if this is a reconnect into
    /// an existing group — the socket must refresh the group's sink to itself.
    pub group: Option<GroupHandle>,
    /// This connection's generation, to hand back to
    /// [`remove_if_current_gen`](SessionHub::remove_if_current_gen) on disconnect.
    pub gen: u64,
}

/// Shared `deviceId → session` directory. `Clone` is a cheap `Arc` bump; one
/// instance lives in the server's app-data across all workers.
#[derive(Clone, Default)]
pub struct SessionHub {
    inner: Arc<DashMap<String, SessionEntry>>,
}

impl SessionHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a `/socket` connection. On a device's FIRST connect this derives
    /// its stable member id (deterministic from the `deviceId` — see
    /// [`member_id_for_device`]) and an empty group slot. On a RECONNECT (same
    /// `device_id`) it keeps the member id + group and just refreshes the sink,
    /// returning the existing group so the socket can re-point it at itself —
    /// membership survives socket churn. Always bumps the connection generation.
    pub fn register(
        &self,
        device_id: String,
        name: String,
        sink: mpsc::Sender<ServerMsg>,
    ) -> Registered {
        let member_id = member_id_for_device(&device_id);
        let mut e = self.inner.entry(device_id).or_insert_with(|| SessionEntry {
            member_id,
            name: name.clone(),
            sink: sink.clone(),
            group: None,
            conn_gen: 0,
        });
        e.name = name;
        e.sink = sink;
        e.conn_gen += 1;
        Registered {
            member_id: e.member_id,
            group: e.group.clone(),
            gen: e.conn_gen,
        }
    }

    /// The device's current connection generation, if registered.
    pub fn conn_gen(&self, device_id: &str) -> Option<u64> {
        self.inner.get(device_id).map(|e| e.conn_gen)
    }

    /// Tear down a device's session ONLY if its generation still equals `gen` —
    /// i.e. no newer `/socket` has connected since the disconnect that scheduled
    /// this. Returns the group (for `RemoveMember`) when it actually removes.
    /// A reconnect within the grace window bumps the generation, so this no-ops.
    pub fn remove_if_current_gen(&self, device_id: &str, gen: u64) -> Option<GroupHandle> {
        // `remove_if` avoids a get-then-remove race with a concurrent reconnect.
        self.inner
            .remove_if(device_id, |_, e| e.conn_gen == gen)
            .and_then(|(_, e)| e.group)
    }

    /// Resolve a device to its session for an HTTP command handler. `None` when
    /// no socket has registered for this device yet (the client should have an
    /// open `/socket` before issuing SyncPlay commands).
    pub fn resolve(&self, device_id: &str) -> Option<ResolvedSession> {
        self.inner.get(device_id).map(|e| ResolvedSession {
            member_id: e.member_id,
            name: e.name.clone(),
            sink: e.sink.clone(),
            group: e.group.clone(),
        })
    }

    /// Record the group a session joined. Called *before* `AddMember` so the
    /// group's wall-clock epoch is available to the socket the instant the
    /// first (late-joiner catch-up) command is broadcast.
    pub fn attach_group(&self, device_id: &str, group: GroupHandle) {
        if let Some(mut e) = self.inner.get_mut(device_id) {
            e.group = Some(group);
        }
    }

    /// Clear a session's group on Leave, returning the old handle so the caller
    /// can `RemoveMember`.
    pub fn detach_group(&self, device_id: &str) -> Option<GroupHandle> {
        self.inner
            .get_mut(device_id)
            .and_then(|mut e| e.group.take())
    }

    /// The wall-clock (unix ms) epoch of the device's current group, used by the
    /// socket to convert a `ServerMsg`'s monotonic `at_server_ms` into the
    /// absolute UTC `When` the Jellyfin client schedules against.
    pub fn epoch_of(&self, device_id: &str) -> Option<u64> {
        self.inner
            .get(device_id)
            .and_then(|e| e.group.as_ref().map(|g| g.epoch_unix_ms))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::delivery::{LocalDelivery, MemberSinks};
    use crate::group::GroupHandle;
    use crate::messages::GroupId;
    use std::sync::Arc;

    fn sink() -> mpsc::Sender<ServerMsg> {
        mpsc::channel(8).0
    }

    /// A group handle backed by a throwaway in-process delivery — these tests
    /// exercise the hub's device→member bookkeeping, not message delivery.
    fn handle() -> GroupHandle {
        GroupHandle::spawn(
            GroupId::new(),
            Arc::new(LocalDelivery::new(MemberSinks::new())),
        )
    }

    #[test]
    fn resolve_absent_device_is_none() {
        let hub = SessionHub::new();
        assert!(hub.resolve("nope").is_none());
    }

    #[tokio::test]
    async fn register_resolve_attach_detach_roundtrip() {
        let hub = SessionHub::new();
        let reg = hub.register("devA".into(), "ali".into(), sink());
        let r = hub.resolve("devA").unwrap();
        assert_eq!(r.member_id, reg.member_id);
        assert!(r.group.is_none() && reg.group.is_none());

        let handle = handle();
        hub.attach_group("devA", handle.clone());
        assert_eq!(hub.epoch_of("devA"), Some(handle.epoch_unix_ms));
        assert!(hub.resolve("devA").unwrap().group.is_some());

        let detached = hub.detach_group("devA").unwrap();
        assert_eq!(detached.group_id, handle.group_id);
        assert!(hub.resolve("devA").unwrap().group.is_none());
    }

    #[tokio::test]
    async fn reconnect_keeps_member_id_and_group_and_bumps_gen() {
        let hub = SessionHub::new();
        let first = hub.register("devB".into(), "b".into(), sink());
        let handle = handle();
        hub.attach_group("devB", handle.clone());

        // Reconnect: same member id, same group surfaced, higher generation.
        let again = hub.register("devB".into(), "b".into(), sink());
        assert_eq!(again.member_id, first.member_id, "member id is stable");
        assert_eq!(
            again.group.map(|g| g.group_id),
            Some(handle.group_id),
            "reconnect sees the existing group"
        );
        assert!(again.gen > first.gen, "generation bumped");

        // A stale-generation teardown (from the FIRST socket's disconnect) must
        // NOT remove the reconnected session.
        assert!(hub.remove_if_current_gen("devB", first.gen).is_none());
        assert!(
            hub.resolve("devB").is_some(),
            "reconnected session survives"
        );

        // The current generation's teardown removes it and returns the group.
        let left = hub.remove_if_current_gen("devB", again.gen).unwrap();
        assert_eq!(left.group_id, handle.group_id);
        assert!(hub.resolve("devB").is_none());
    }

    /// B24 — the member id must survive a PROCESS RESTART, not just a socket
    /// reconnect: a fresh hub (new process after a deploy) must derive the
    /// same member id for the same device, or the persisted group roster can
    /// never recognise the returning member.
    #[test]
    fn member_id_is_stable_across_process_restarts() {
        let hub_before = SessionHub::new();
        let a = hub_before.register("devC".into(), "c".into(), sink());
        let hub_after_restart = SessionHub::new();
        let b = hub_after_restart.register("devC".into(), "c".into(), sink());
        assert_eq!(a.member_id, b.member_id, "same device → same member id");
        assert_eq!(a.member_id, member_id_for_device("devC"));
        // Distinct devices still get distinct ids.
        assert_ne!(member_id_for_device("devC"), member_id_for_device("devD"));
    }
}
