//! The real [`CastDirectory`]: what the cast service knows, made readable
//! synchronously for the `/Sessions` projections.
//!
//! [`CastDirectory`] is a sync trait (it is read while building a response and
//! inside the socket loop) but the service is an actor, so the active casts
//! are mirrored into a small cache. [`RuntimeCastDirectory::refresh`] updates
//! it; [`RuntimeCastDirectory::spawn_refresher`] keeps it current and tells
//! every socket to re-read `Sessions` when it changed, whatever ended or
//! started the cast (owner leave, TV drop, group gone).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::Duration;

use pharos_core::{UserId, UserStore};
use pharos_sync::messages::GroupPlayState;
use pharos_sync::GroupRegistry;
use tokio::sync::broadcast;

use crate::cast_service::CastRuntime;
use crate::cast_sessions::{ActiveCastInfo, CastDirectory};
use crate::config::CastConfig;
use crate::state::{SocketBroadcast, Stores};

/// How often the cache re-reads the service when nothing prompted a refresh.
/// Casts end for reasons no route sees (TV drop, group gone), so the picker
/// would otherwise show a stale owner until the next viewer event.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Owner display names by user, so the refresh tick reads a user once rather
/// than every tick for the life of a cast. Only successful reads are kept (a
/// failed one is retried next tick) and entries for owners with no active cast
/// are dropped, which bounds the map by the number of casts.
#[derive(Default)]
struct OwnerNames {
    names: Mutex<HashMap<UserId, String>>,
}

impl OwnerNames {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<UserId, String>> {
        self.names.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn name_of<F, Fut>(&self, owner: UserId, lookup: F) -> Option<String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Option<String>>,
    {
        if let Some(name) = self.lock().get(&owner) {
            return Some(name.clone());
        }
        let name = lookup().await?;
        self.lock().insert(owner, name.clone());
        Some(name)
    }

    fn retain_only(&self, owners: &[UserId]) {
        self.lock().retain(|owner, _| owners.contains(owner));
    }
}

pub struct RuntimeCastDirectory {
    cfg: CastConfig,
    runtime: CastRuntime,
    stores: Stores,
    /// Bound once the group registry exists (it is built after `AppState`).
    registry: OnceLock<GroupRegistry>,
    owner_names: OwnerNames,
    active: RwLock<Vec<ActiveCastInfo>>,
}

impl RuntimeCastDirectory {
    pub fn new(cfg: CastConfig, runtime: CastRuntime, stores: Stores) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            runtime,
            stores,
            registry: OnceLock::new(),
            owner_names: OwnerNames::default(),
            active: RwLock::new(Vec::new()),
        })
    }

    /// Let the directory read each cast's group for the item it is playing.
    pub fn bind_registry(&self, registry: GroupRegistry) {
        let _ = self.registry.set(registry);
    }

    /// Re-read the service. Returns whether the active casts changed.
    pub async fn refresh(&self) -> bool {
        let Some(service) = self.runtime.service() else {
            return false;
        };
        let mut next = Vec::new();
        let mut live_owners = Vec::new();
        for cast in service.snapshot().await {
            live_owners.push(cast.owner);
            let owner_user_name = self
                .owner_names
                .name_of(cast.owner, || async {
                    match self.stores.get(cast.owner).await {
                        Ok(user) => Some(user.name),
                        Err(e) => {
                            tracing::warn!(
                                tv = %cast.tv, owner = %cast.owner.0, error = %e,
                                "cast: owner not readable; listing the cast without a name"
                            );
                            None
                        }
                    }
                })
                .await
                .unwrap_or_default();
            let (now_playing_item_id, is_paused) = self.group_view(cast.group).await;
            next.push(ActiveCastInfo {
                tv: cast.tv,
                owner_user_id: cast.owner.0.simple().to_string(),
                owner_user_name,
                now_playing_item_id,
                position_ticks: 0,
                is_paused,
            });
        }
        self.owner_names.retain_only(&live_owners);
        let mut guard = self.active.write().unwrap_or_else(PoisonError::into_inner);
        if *guard == next {
            return false;
        }
        *guard = next;
        true
    }

    async fn group_view(&self, group: pharos_sync::messages::GroupId) -> (Option<String>, bool) {
        let Some(registry) = self.registry.get() else {
            return (None, false);
        };
        let handle = match registry.get(group).await {
            Ok(Some(handle)) => handle,
            Ok(None) => return (None, false),
            Err(e) => {
                tracing::warn!(%group, error = %e, "cast: group not readable for the TV session");
                return (None, false);
            }
        };
        match handle.snapshot().await {
            Some(snap) => (
                snap.current_item_id,
                snap.play_state == GroupPlayState::Paused,
            ),
            None => (None, false),
        }
    }

    /// Keep the cache current. A `CastChanged` on the bus (sent by the routes
    /// after a start or stop) refreshes at once; otherwise every
    /// [`REFRESH_INTERVAL`]. Whenever the casts changed, every socket is told
    /// to re-read `Sessions` so the device picker follows.
    pub fn spawn_refresher(self: &Arc<Self>, bus: broadcast::Sender<SocketBroadcast>) {
        let this = Arc::clone(self);
        let mut rx = bus.subscribe();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let prompted = tokio::select! {
                    _ = tick.tick() => false,
                    msg = rx.recv() => match msg {
                        Ok(SocketBroadcast::CastChanged) => true,
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                };
                if this.refresh().await || prompted {
                    let _ = bus.send(SocketBroadcast::SessionsChanged);
                }
            }
        });
    }
}

impl CastDirectory for RuntimeCastDirectory {
    fn config(&self) -> &CastConfig {
        &self.cfg
    }

    fn reachable(&self) -> HashMap<String, bool> {
        self.runtime.reachable_tvs()
    }

    fn active(&self) -> Vec<ActiveCastInfo> {
        self.active
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn tick(names: &OwnerNames, owners: &[UserId], reads: &AtomicUsize) -> Vec<String> {
        let mut out = Vec::new();
        for owner in owners {
            let name = names
                .name_of(*owner, || async {
                    reads.fetch_add(1, Ordering::SeqCst);
                    Some("alice".to_owned())
                })
                .await
                .unwrap_or_default();
            out.push(name);
        }
        names.retain_only(owners);
        out
    }

    /// The refresh tick must not read the user store for a cast it already knows.
    #[tokio::test]
    async fn repeated_ticks_read_each_owner_once() {
        let names = OwnerNames::default();
        let reads = AtomicUsize::new(0);
        let owner = UserId::new();
        for _ in 0..10 {
            assert_eq!(tick(&names, &[owner], &reads).await, ["alice"]);
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_read_is_retried_and_a_departed_owner_is_forgotten() {
        let names = OwnerNames::default();
        let owner = UserId::new();
        let failing = names.name_of(owner, || async { None }).await;
        assert_eq!(failing, None);
        let reads = AtomicUsize::new(0);
        tick(&names, &[owner], &reads).await;
        assert_eq!(reads.load(Ordering::SeqCst), 1, "a failure is not cached");

        tick(&names, &[], &reads).await;
        tick(&names, &[owner], &reads).await;
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "an owner with no cast is evicted"
        );
    }
}
