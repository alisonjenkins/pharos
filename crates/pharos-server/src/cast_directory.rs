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
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::time::Duration;

use pharos_core::UserStore;
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

pub struct RuntimeCastDirectory {
    cfg: CastConfig,
    runtime: CastRuntime,
    stores: Stores,
    /// Bound once the group registry exists (it is built after `AppState`).
    registry: OnceLock<GroupRegistry>,
    active: RwLock<Vec<ActiveCastInfo>>,
}

impl RuntimeCastDirectory {
    pub fn new(cfg: CastConfig, runtime: CastRuntime, stores: Stores) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            runtime,
            stores,
            registry: OnceLock::new(),
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
        for cast in service.snapshot().await {
            let owner_user_name = match self.stores.get(cast.owner).await {
                Ok(user) => user.name,
                Err(e) => {
                    tracing::warn!(
                        tv = %cast.tv, owner = %cast.owner.0, error = %e,
                        "cast: owner not readable; listing the cast without a name"
                    );
                    String::new()
                }
            };
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
