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

struct SessionEntry {
    member_id: MemberId,
    name: String,
    sink: mpsc::Sender<ServerMsg>,
    /// The group this session currently belongs to, if any. Set by the HTTP
    /// New/Join handler (via [`attach_group`](SessionHub::attach_group)) and
    /// cleared on Leave/disconnect.
    group: Option<GroupHandle>,
}

/// A session resolved for an HTTP SyncPlay command handler. Clones out of the
/// hub so the handler never holds the map shard lock across an `await`.
pub struct ResolvedSession {
    pub member_id: MemberId,
    pub name: String,
    pub sink: mpsc::Sender<ServerMsg>,
    pub group: Option<GroupHandle>,
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

    /// Register a live socket. Called when a `/socket` WebSocket connects.
    /// Re-registering the same `device_id` (a reconnect) replaces the prior
    /// entry — the old sink is dropped, so its group learns of the departure
    /// only when the old socket task also runs its `deregister` path.
    pub fn register(
        &self,
        device_id: String,
        member_id: MemberId,
        name: String,
        sink: mpsc::Sender<ServerMsg>,
    ) {
        self.inner.insert(
            device_id,
            SessionEntry {
                member_id,
                name,
                sink,
                group: None,
            },
        );
    }

    /// Remove a socket on disconnect, returning any group it belonged to so the
    /// caller can send `RemoveMember`.
    pub fn deregister(&self, device_id: &str) -> Option<GroupHandle> {
        self.inner.remove(device_id).and_then(|(_, e)| e.group)
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
    use crate::group::GroupHandle;
    use crate::messages::GroupId;

    fn sink() -> mpsc::Sender<ServerMsg> {
        mpsc::channel(8).0
    }

    #[test]
    fn resolve_absent_device_is_none() {
        let hub = SessionHub::new();
        assert!(hub.resolve("nope").is_none());
    }

    #[tokio::test]
    async fn register_resolve_attach_detach_roundtrip() {
        let hub = SessionHub::new();
        let mid = MemberId::new();
        hub.register("devA".into(), mid, "ali".into(), sink());
        let r = hub.resolve("devA").unwrap();
        assert_eq!(r.member_id, mid);
        assert!(r.group.is_none());

        let handle = GroupHandle::spawn(GroupId::new());
        hub.attach_group("devA", handle.clone());
        assert_eq!(hub.epoch_of("devA"), Some(handle.epoch_unix_ms));
        assert!(hub.resolve("devA").unwrap().group.is_some());

        let detached = hub.detach_group("devA").unwrap();
        assert_eq!(detached.group_id, handle.group_id);
        assert!(hub.resolve("devA").unwrap().group.is_none());
    }

    #[tokio::test]
    async fn deregister_returns_group_for_cleanup() {
        let hub = SessionHub::new();
        hub.register("devB".into(), MemberId::new(), "b".into(), sink());
        let handle = GroupHandle::spawn(GroupId::new());
        hub.attach_group("devB", handle.clone());
        let left = hub.deregister("devB").unwrap();
        assert_eq!(left.group_id, handle.group_id);
        assert!(hub.resolve("devB").is_none());
    }
}
