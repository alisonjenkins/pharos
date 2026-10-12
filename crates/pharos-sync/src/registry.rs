//! `GroupRegistry` actor — owns `HashMap<GroupId, GroupHandle>` and routes
//! membership requests. WS handlers send via the registry, never touching
//! group state directly (V18).
//!
//! Two modes:
//! - **single-replica** (`spawn`): every group is owned locally; a plain
//!   `LocalDelivery` reaches members. SQLite deployments. Behaviour identical
//!   to the original registry.
//! - **distributed** (`spawn_distributed`): under Postgres, a per-group advisory
//!   lock elects one owner replica. The owner runs the actor (persisting its
//!   snapshot, delivering via the bus); non-owners hold a *remote handle* that
//!   forwards commands to the owner over the bus. A reconnect after the owner
//!   drained re-attempts ownership and hydrates from the last snapshot — the
//!   group survives the deploy.

use super::delivery::Delivery;
use super::distributed::{spawn_remote_handle, Distributed};
use super::group::{GroupHandle, RemoteCommand};
use super::messages::GroupId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc, oneshot};

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry actor dropped")]
    ActorDown,
    #[error("registry reply dropped")]
    ReplyDropped,
}

enum Msg {
    GetOrCreate {
        group_id: GroupId,
        reply: oneshot::Sender<GroupHandle>,
    },
    Get {
        group_id: GroupId,
        reply: oneshot::Sender<Option<GroupHandle>>,
    },
    Create {
        reply: oneshot::Sender<GroupHandle>,
    },
    List {
        reply: oneshot::Sender<Vec<GroupHandle>>,
    },
    /// A command forwarded from a non-owner replica over the bus. Applied only
    /// if THIS replica owns the group's local actor.
    DeliverCommand {
        group_id: GroupId,
        cmd: RemoteCommand,
    },
    IsOwned {
        group_id: GroupId,
        reply: oneshot::Sender<bool>,
    },
    TryLease {
        key: GroupId,
        reply: oneshot::Sender<bool>,
    },
    ReleaseLease {
        key: GroupId,
    },
}

/// The registry actor's mutable state.
struct Actor {
    /// Every group this replica has a handle for — owned (local actor) or
    /// remote (bus router).
    groups: HashMap<GroupId, GroupHandle>,
    /// Groups this replica owns a *local actor* for (holds the advisory lease).
    owned: HashSet<GroupId>,
    /// How owned actors reach members (`LocalDelivery` or `BusDelivery`).
    delivery: Arc<dyn Delivery>,
    /// `Some` in distributed mode: ownership election + hydration + command bus
    /// + snapshot persistence.
    distributed: Option<Distributed>,
    /// Announces a group this replica just took over from a persisted snapshot.
    hydrated: broadcast::Sender<GroupHandle>,
    /// Keys leased through [`GroupRegistry::try_lease`], so asking twice from
    /// the holder does not contend with its own lock.
    leases: HashSet<GroupId>,
}

impl Actor {
    /// A cached handle if it is live AND authoritative. Owned handles are
    /// authoritative (we hold the lease); a cached *remote* handle is NOT — the
    /// owner may have drained — so it is never short-circuited here, forcing a
    /// fresh ownership attempt (the takeover path).
    fn cached_owned(&mut self, group_id: GroupId) -> Option<GroupHandle> {
        if self.owned.contains(&group_id) {
            match self.groups.get(&group_id) {
                Some(h) if !h.tx.is_closed() => return Some(h.clone()),
                _ => self.drop_group(group_id), // owned actor died → release + re-elect
            }
        }
        None
    }

    /// Forget a group, releasing its ownership lease if we held one.
    fn drop_group(&mut self, group_id: GroupId) {
        self.groups.remove(&group_id);
        if self.owned.remove(&group_id) {
            if let Some(d) = &self.distributed {
                d.ownership.release(group_id);
            }
        }
    }

    async fn get_or_create(&mut self, group_id: GroupId) -> GroupHandle {
        if let Some(h) = self.cached_owned(group_id) {
            return h;
        }
        self.spawn_for(group_id).await
    }

    /// Elect + spawn (or reuse a remote router for) a group.
    async fn spawn_for(&mut self, group_id: GroupId) -> GroupHandle {
        let Some(d) = self.distributed.clone() else {
            // Single-replica: always own.
            let h = GroupHandle::spawn(group_id, self.delivery.clone());
            self.owned.insert(group_id);
            self.groups.insert(group_id, h.clone());
            return h;
        };
        if d.ownership.try_own(group_id).await {
            // We own it: hydrate from the last snapshot (or start fresh) and run
            // the actor locally.
            let (epoch, json) = match d.hydration.load(group_id).await {
                Some((e, j)) => (e, Some(j)),
                None => (unix_now_ms(), None),
            };
            let h = GroupHandle::spawn_persistent(
                group_id,
                epoch,
                self.delivery.clone(),
                d.persistence.clone(),
                json.as_deref(),
            );
            self.owned.insert(group_id);
            self.groups.insert(group_id, h.clone());
            if json.is_some() {
                // No subscriber is fine: nothing needs resuming.
                let _ = self.hydrated.send(h.clone());
            }
            h
        } else {
            // Another replica owns it: reuse a live remote router if cached,
            // else build one. Its epoch comes from the persisted snapshot so the
            // socket's When conversion is correct.
            if let Some(h) = self.groups.get(&group_id) {
                if !h.tx.is_closed() && !self.owned.contains(&group_id) {
                    return h.clone();
                }
            }
            let epoch = d
                .hydration
                .load(group_id)
                .await
                .map(|(e, _)| e)
                .unwrap_or_else(unix_now_ms);
            let h = spawn_remote_handle(group_id, epoch, d.commands.clone());
            self.groups.insert(group_id, h.clone());
            h
        }
    }

    fn get(&mut self, group_id: GroupId) -> Option<GroupHandle> {
        match self.groups.get(&group_id) {
            Some(h) if !h.tx.is_closed() => Some(h.clone()),
            Some(_) => {
                self.drop_group(group_id);
                None
            }
            None => None,
        }
    }

    async fn deliver_command(&mut self, group_id: GroupId, cmd: RemoteCommand) {
        // Only the owner's local actor applies a forwarded command.
        if self.owned.contains(&group_id) {
            if let Some(h) = self.cached_owned(group_id) {
                let _ = h.tx.send(cmd.into_group_msg()).await;
            }
        }
    }

    /// Whether this replica runs the group's actor right now.
    fn is_owned(&mut self, group_id: GroupId) -> bool {
        self.cached_owned(group_id).is_some()
    }

    async fn try_lease(&mut self, key: GroupId) -> bool {
        if self.leases.contains(&key) {
            return true;
        }
        let granted = match &self.distributed {
            Some(d) => d.ownership.try_own(key).await,
            None => true,
        };
        if granted {
            self.leases.insert(key);
        }
        granted
    }

    fn release_lease(&mut self, key: GroupId) {
        if self.leases.remove(&key) {
            if let Some(d) = &self.distributed {
                d.ownership.release(key);
            }
        }
    }

    fn list(&mut self) -> Vec<GroupHandle> {
        self.groups.retain(|_, h| !h.tx.is_closed());
        self.groups.values().cloned().collect()
    }
}

#[derive(Clone)]
pub struct GroupRegistry {
    tx: mpsc::Sender<Msg>,
    hydrated: broadcast::Sender<GroupHandle>,
}

/// Groups announced to late subscribers' queues before they lag.
const HYDRATED_BACKLOG: usize = 64;

impl GroupRegistry {
    /// Spawn a single-replica registry. `delivery` reaches members
    /// (`LocalDelivery`). Every group is owned locally; no persistence, no
    /// ownership election. SQLite deployments.
    pub fn spawn(delivery: Arc<dyn Delivery>) -> Self {
        Self::spawn_with(delivery, None)
    }

    /// Spawn a distributed (multi-replica) registry. `delivery` is a
    /// `BusDelivery`; `distributed` injects ownership election, hydration, the
    /// command bus, and snapshot persistence.
    pub fn spawn_distributed(delivery: Arc<dyn Delivery>, distributed: Distributed) -> Self {
        Self::spawn_with(delivery, Some(distributed))
    }

    fn spawn_with(delivery: Arc<dyn Delivery>, distributed: Option<Distributed>) -> Self {
        let (tx, mut rx) = mpsc::channel::<Msg>(64);
        let (hydrated, _) = broadcast::channel(HYDRATED_BACKLOG);
        let announce = hydrated.clone();
        tokio::spawn(async move {
            let mut actor = Actor {
                groups: HashMap::new(),
                owned: HashSet::new(),
                delivery,
                distributed,
                hydrated: announce,
                leases: HashSet::new(),
            };
            while let Some(msg) = rx.recv().await {
                match msg {
                    Msg::GetOrCreate { group_id, reply } => {
                        let h = actor.get_or_create(group_id).await;
                        let _ = reply.send(h);
                    }
                    Msg::Get { group_id, reply } => {
                        let _ = reply.send(actor.get(group_id));
                    }
                    Msg::Create { reply } => {
                        // A brand-new id: `spawn_for` wins ownership immediately
                        // (nobody else holds the lock) and runs it locally.
                        let h = actor.spawn_for(GroupId::new()).await;
                        let _ = reply.send(h);
                    }
                    Msg::List { reply } => {
                        let _ = reply.send(actor.list());
                    }
                    Msg::DeliverCommand { group_id, cmd } => {
                        actor.deliver_command(group_id, cmd).await;
                    }
                    Msg::IsOwned { group_id, reply } => {
                        let _ = reply.send(actor.is_owned(group_id));
                    }
                    Msg::TryLease { key, reply } => {
                        let _ = reply.send(actor.try_lease(key).await);
                    }
                    Msg::ReleaseLease { key } => actor.release_lease(key),
                }
            }
        });
        Self { tx, hydrated }
    }

    /// Groups this replica takes over from a persisted snapshot (a restart, a
    /// failover), each announced once after its actor is running. A group
    /// created fresh is never announced.
    pub fn subscribe_hydrated(&self) -> broadcast::Receiver<GroupHandle> {
        self.hydrated.subscribe()
    }

    /// Whether this replica runs the group's actor (a remote handle, or an
    /// unknown group, is not owned).
    pub async fn is_owned(&self, group_id: GroupId) -> Result<bool, RegistryError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::IsOwned {
                group_id,
                reply: tx,
            })
            .await
            .map_err(|_| RegistryError::ActorDown)?;
        rx.await.map_err(|_| RegistryError::ReplyDropped)
    }

    /// Take the cross-replica lease named `key`, on the same election as group
    /// ownership: at most one replica holds it, asking again as the holder is a
    /// no-op, and a single-replica registry always grants it. `key` is any id
    /// that cannot be a group's, so leases never contend with groups.
    pub async fn try_lease(&self, key: GroupId) -> Result<bool, RegistryError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::TryLease { key, reply: tx })
            .await
            .map_err(|_| RegistryError::ActorDown)?;
        rx.await.map_err(|_| RegistryError::ReplyDropped)
    }

    /// Give a lease back. Idempotent.
    pub async fn release_lease(&self, key: GroupId) -> Result<(), RegistryError> {
        self.tx
            .send(Msg::ReleaseLease { key })
            .await
            .map_err(|_| RegistryError::ActorDown)
    }

    /// Route a bus-forwarded command to this replica's owned actor for the
    /// group (no-op if this replica doesn't own it). Called by the command
    /// ingress.
    pub async fn deliver_command(
        &self,
        group_id: GroupId,
        cmd: RemoteCommand,
    ) -> Result<(), RegistryError> {
        self.tx
            .send(Msg::DeliverCommand { group_id, cmd })
            .await
            .map_err(|_| RegistryError::ActorDown)
    }

    pub async fn get_or_create(&self, group_id: GroupId) -> Result<GroupHandle, RegistryError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::GetOrCreate {
                group_id,
                reply: tx,
            })
            .await
            .map_err(|_| RegistryError::ActorDown)?;
        rx.await.map_err(|_| RegistryError::ReplyDropped)
    }

    pub async fn get(&self, group_id: GroupId) -> Result<Option<GroupHandle>, RegistryError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::Get {
                group_id,
                reply: tx,
            })
            .await
            .map_err(|_| RegistryError::ActorDown)?;
        rx.await.map_err(|_| RegistryError::ReplyDropped)
    }

    pub async fn create(&self) -> Result<GroupHandle, RegistryError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::Create { reply: tx })
            .await
            .map_err(|_| RegistryError::ActorDown)?;
        rx.await.map_err(|_| RegistryError::ReplyDropped)
    }

    /// Snapshot of every live group's `GroupHandle`. Caller can fan
    /// out `snapshot()` calls to render the group-watch list endpoint.
    pub async fn list(&self) -> Result<Vec<GroupHandle>, RegistryError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::List { reply: tx })
            .await
            .map_err(|_| RegistryError::ActorDown)?;
        rx.await.map_err(|_| RegistryError::ReplyDropped)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::delivery::{LocalDelivery, MemberSinks};

    /// A registry wired to an in-process delivery, plus the `MemberSinks` its
    /// groups deliver into (so tests can register a member's socket).
    fn reg() -> (GroupRegistry, MemberSinks) {
        let sinks = MemberSinks::new();
        let r = GroupRegistry::spawn(Arc::new(LocalDelivery::new(sinks.clone())));
        (r, sinks)
    }

    #[tokio::test]
    async fn create_then_get_returns_same_group() {
        let (r, _sinks) = reg();
        let h1 = r.create().await.unwrap();
        let h2 = r.get(h1.group_id).await.unwrap().unwrap();
        assert_eq!(h1.group_id, h2.group_id);
    }

    #[tokio::test]
    async fn get_or_create_idempotent() {
        let (r, _sinks) = reg();
        let id = GroupId::new();
        let h1 = r.get_or_create(id).await.unwrap();
        let h2 = r.get_or_create(id).await.unwrap();
        assert_eq!(h1.group_id, h2.group_id);
    }

    #[tokio::test]
    async fn get_unknown_is_none() {
        let (r, _sinks) = reg();
        let res = r.get(GroupId::new()).await.unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn get_or_create_respawns_after_group_terminates() {
        use crate::group::GroupMsg;
        use crate::messages::MemberId;
        use std::time::Duration;
        use tokio::sync::{mpsc, oneshot};

        let (r, sinks) = reg();
        let id = GroupId::new();
        let h1 = r.get_or_create(id).await.unwrap();

        // Add then remove the sole member → the actor empties + terminates.
        let (sink, _rx) = mpsc::channel(8);
        let mid = MemberId::new();
        sinks.insert(mid, 1, sink);
        let (rtx, rrx) = oneshot::channel();
        h1.tx
            .send(GroupMsg::AddMember {
                member_id: mid,
                name: "x".into(),
                reply: rtx,
            })
            .await
            .unwrap();
        let _ = rrx.await.unwrap();
        h1.tx
            .send(GroupMsg::RemoveMember { member_id: mid })
            .await
            .unwrap();

        // Wait for the actor task to exit (its tx closes).
        for _ in 0..200 {
            if h1.tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(h1.tx.is_closed(), "group actor should have terminated");

        // The bug: registry returned the dead handle so every future join
        // failed. Now it must respawn a LIVE one.
        let h2 = r.get_or_create(id).await.unwrap();
        assert!(!h2.tx.is_closed(), "respawned handle must be live");
        assert_eq!(h2.group_id, id);
    }

    #[tokio::test]
    async fn get_drops_dead_handle() {
        use crate::group::GroupMsg;
        use crate::messages::MemberId;
        use std::time::Duration;
        use tokio::sync::{mpsc, oneshot};

        let (r, sinks) = reg();
        let h1 = r.create().await.unwrap();
        let id = h1.group_id;
        let (sink, _rx) = mpsc::channel(8);
        let mid = MemberId::new();
        sinks.insert(mid, 1, sink);
        let (rtx, rrx) = oneshot::channel();
        h1.tx
            .send(GroupMsg::AddMember {
                member_id: mid,
                name: "x".into(),
                reply: rtx,
            })
            .await
            .unwrap();
        let _ = rrx.await.unwrap();
        h1.tx
            .send(GroupMsg::RemoveMember { member_id: mid })
            .await
            .unwrap();
        for _ in 0..200 {
            if h1.tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // get() must not surface the dead handle.
        assert!(r.get(id).await.unwrap().is_none());
    }
}
