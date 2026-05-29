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
                        let handle = groups
                            .entry(group_id)
                            .or_insert_with(|| GroupHandle::spawn(group_id))
                            .clone();
                        // Detect a closed group (tx error on send-ping is overkill;
                        // simplest: rely on Create on next request after a panic).
                        let _ = reply.send(handle);
                    }
                    Msg::Get { group_id, reply } => {
                        let h = groups.get(&group_id).cloned();
                        let _ = reply.send(h);
                    }
                    Msg::Create { reply } => {
                        let id = GroupId::new();
                        let h = GroupHandle::spawn(id);
                        groups.insert(id, h.clone());
                        let _ = reply.send(h);
                    }
                    Msg::List { reply } => {
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
}
