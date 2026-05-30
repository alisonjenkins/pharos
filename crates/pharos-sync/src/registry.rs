//! `GroupRegistry` actor — owns `HashMap<GroupId, GroupHandle>` and routes
//! membership requests. WS handlers send via the registry, never touching
//! group state directly (V18).

use super::group::GroupHandle;
use super::messages::GroupId;
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

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
}

#[derive(Clone)]
pub struct GroupRegistry {
    tx: mpsc::Sender<Msg>,
}

impl GroupRegistry {
    pub fn spawn() -> Self {
        let (tx, mut rx) = mpsc::channel::<Msg>(64);
        tokio::spawn(async move {
            let mut groups: HashMap<GroupId, GroupHandle> = HashMap::new();
            while let Some(msg) = rx.recv().await {
                match msg {
                    Msg::GetOrCreate { group_id, reply } => {
                        // A group actor terminates (and closes its tx) when its
                        // last member leaves. A stale, present-but-dead handle
                        // must be respawned, else every future join to a reused
                        // GroupId silently fails (`tx.send` → Err). Treat a
                        // closed handle as absent.
                        let needs_spawn = groups
                            .get(&group_id)
                            .map(|h| h.tx.is_closed())
                            .unwrap_or(true);
                        if needs_spawn {
                            groups.insert(group_id, GroupHandle::spawn(group_id));
                        }
                        let handle = groups.get(&group_id).cloned();
                        // Always Some — we just inserted if needed.
                        if let Some(h) = handle {
                            let _ = reply.send(h);
                        }
                    }
                    Msg::Get { group_id, reply } => {
                        // Don't surface a dead handle. Drop the stale entry so
                        // the map doesn't leak terminated groups.
                        let h = match groups.get(&group_id) {
                            Some(h) if !h.tx.is_closed() => Some(h.clone()),
                            Some(_) => {
                                groups.remove(&group_id);
                                None
                            }
                            None => None,
                        };
                        let _ = reply.send(h);
                    }
                    Msg::Create { reply } => {
                        let id = GroupId::new();
                        let h = GroupHandle::spawn(id);
                        groups.insert(id, h.clone());
                        let _ = reply.send(h);
                    }
                    Msg::List { reply } => {
                        // Prune terminated groups, surface only live ones.
                        groups.retain(|_, h| !h.tx.is_closed());
                        let all: Vec<GroupHandle> = groups.values().cloned().collect();
                        let _ = reply.send(all);
                    }
                }
            }
        });
        Self { tx }
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

    #[tokio::test]
    async fn create_then_get_returns_same_group() {
        let r = GroupRegistry::spawn();
        let h1 = r.create().await.unwrap();
        let h2 = r.get(h1.group_id).await.unwrap().unwrap();
        assert_eq!(h1.group_id, h2.group_id);
    }

    #[tokio::test]
    async fn get_or_create_idempotent() {
        let r = GroupRegistry::spawn();
        let id = GroupId::new();
        let h1 = r.get_or_create(id).await.unwrap();
        let h2 = r.get_or_create(id).await.unwrap();
        assert_eq!(h1.group_id, h2.group_id);
    }

    #[tokio::test]
    async fn get_unknown_is_none() {
        let r = GroupRegistry::spawn();
        let res = r.get(GroupId::new()).await.unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn get_or_create_respawns_after_group_terminates() {
        use crate::group::GroupMsg;
        use crate::messages::MemberId;
        use std::time::Duration;
        use tokio::sync::{mpsc, oneshot};

        let r = GroupRegistry::spawn();
        let id = GroupId::new();
        let h1 = r.get_or_create(id).await.unwrap();

        // Add then remove the sole member → the actor empties + terminates.
        let (sink, _rx) = mpsc::channel(8);
        let mid = MemberId::new();
        let (rtx, rrx) = oneshot::channel();
        h1.tx
            .send(GroupMsg::AddMember {
                member_id: mid,
                name: "x".into(),
                sink,
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

        let r = GroupRegistry::spawn();
        let h1 = r.create().await.unwrap();
        let id = h1.group_id;
        let (sink, _rx) = mpsc::channel(8);
        let mid = MemberId::new();
        let (rtx, rrx) = oneshot::channel();
        h1.tx
            .send(GroupMsg::AddMember {
                member_id: mid,
                name: "x".into(),
                sink,
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
