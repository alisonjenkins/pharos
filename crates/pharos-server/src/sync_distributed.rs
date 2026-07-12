//! Server-side wiring of the distributed SyncPlay coordinator (Phase B4.3d).
//!
//! Implements the pharos-sync injection traits (`OwnershipSource`,
//! `HydrationSource`, `CommandSink`, `GroupPersistence`) over the concrete
//! `Stores` + a Postgres `PgSyncBus`, and builds a distributed `GroupRegistry`
//! plus the two bus ingresses (member delivery + inbound command routing).
//!
//! Postgres-only: SQLite deployments are single-replica and use the plain
//! `GroupRegistry::spawn` + `LocalDelivery` path (see `main.rs`).

use crate::state::Stores;
use pharos_core::SyncGroupStore;
use pharos_store_sqlx::{GroupOwnership, PgSyncBus};
use pharos_sync::bus_delivery::BusMsg;
use pharos_sync::distributed::{
    CommandSink, Distributed, HydrationSource, LoadFuture, OwnFuture, OwnershipSource,
};
use pharos_sync::group::RemoteCommand;
use pharos_sync::messages::GroupId;
use pharos_sync::persistence::GroupPersistence;
use pharos_sync::{BusDelivery, GroupRegistry, MemberSinks, SyncBus};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const NOW_UNIX_SECS_FALLBACK: i64 = 0;

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(NOW_UNIX_SECS_FALLBACK)
}

/// Per-group ownership over the store's advisory lock. Retains each won lease
/// (a live Postgres connection holding the lock) keyed by group so `release`
/// can drop it, freeing the group for another replica.
struct StoreOwnership {
    stores: Stores,
    leases: Mutex<HashMap<GroupId, GroupOwnership>>,
}

impl OwnershipSource for StoreOwnership {
    fn try_own(&self, group_id: GroupId) -> OwnFuture<'_> {
        Box::pin(async move {
            match self
                .stores
                .try_acquire_group_ownership(&group_id.to_string())
                .await
            {
                Ok(Some(lease)) => {
                    if let Ok(mut leases) = self.leases.lock() {
                        leases.insert(group_id, lease);
                    }
                    true
                }
                Ok(None) => false,
                Err(e) => {
                    tracing::warn!(%group_id, error = %e, "group ownership acquire failed");
                    false
                }
            }
        })
    }

    fn release(&self, group_id: GroupId) {
        // Dropping the lease closes its connection, releasing the advisory lock.
        if let Ok(mut leases) = self.leases.lock() {
            leases.remove(&group_id);
        }
    }
}

/// Loads a group's persisted snapshot (`epoch_unix_ms`, `state_json`) for
/// takeover hydration.
struct StoreHydration {
    stores: Stores,
}

impl HydrationSource for StoreHydration {
    fn load(&self, group_id: GroupId) -> LoadFuture<'_> {
        Box::pin(async move {
            match self.stores.get_sync_group(&group_id.to_string()).await {
                Ok(Some(g)) => Some((g.epoch_unix_ms as u64, g.state_json)),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(%group_id, error = %e, "group snapshot load failed");
                    None
                }
            }
        })
    }
}

/// Persists an owned group's snapshot after each mutation (fire-and-forget
/// from the ACTOR's perspective, but strictly ORDERED per group: each write
/// runs behind the previous one via a per-group op chain). Without the
/// ordering, the actor's final persist (the 0-member roster written just
/// before it terminates) raced its own remove — two independently-spawned
/// tasks — and a late upsert RESURRECTED the row as an immortal 0-member
/// orphan in the picker (B31).
struct StorePersistence {
    stores: Stores,
    /// group id → the tail of that group's op chain. Each new op awaits the
    /// previous handle, guaranteeing store-visible ordering per group.
    chains: std::sync::Mutex<std::collections::HashMap<GroupId, tokio::task::JoinHandle<()>>>,
}

impl StorePersistence {
    /// Enqueue `op` behind the group's previous op (if any still runs).
    fn enqueue<F>(&self, group_id: GroupId, op: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut chains = self
            .chains
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = chains.remove(&group_id);
        let handle = tokio::spawn(async move {
            if let Some(p) = prev {
                let _ = p.await;
            }
            op.await;
        });
        chains.insert(group_id, handle);
        // Bound the map: drop entries whose chain already finished.
        chains.retain(|_, h| !h.is_finished());
    }
}

impl GroupPersistence for StorePersistence {
    fn persist(&self, group_id: GroupId, epoch_unix_ms: u64, state_json: String) {
        let stores = self.stores.clone();
        self.enqueue(group_id, async move {
            let persisted = pharos_core::PersistedSyncGroup {
                group_id: group_id.to_string(),
                epoch_unix_ms: epoch_unix_ms as i64,
                state_json,
                updated_at: now_unix_secs(),
            };
            if let Err(e) = stores.upsert_sync_group(&persisted, now_unix_secs()).await {
                tracing::warn!(%group_id, error = %e, "group snapshot persist failed");
            }
        });
    }

    fn remove(&self, group_id: GroupId) {
        let stores = self.stores.clone();
        self.enqueue(group_id, async move {
            if let Err(e) = stores.remove_sync_group(&group_id.to_string()).await {
                tracing::warn!(%group_id, error = %e, "group snapshot remove failed");
            }
        });
    }
}

/// Forwards a non-owner's command to the owner over the bus. Serializes a
/// `BusMsg::Command` and funnels through a single egress task so commands keep
/// their order.
struct BusCommands {
    egress: mpsc::UnboundedSender<String>,
}

impl BusCommands {
    fn new(bus: Arc<PgSyncBus>) -> Self {
        let (egress, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(payload) = rx.recv().await {
                let _ = bus.publish(payload).await;
            }
        });
        Self { egress }
    }
}

impl CommandSink for BusCommands {
    fn submit(&self, group_id: GroupId, cmd: RemoteCommand) {
        if let Ok(payload) = serde_json::to_string(&BusMsg::Command { group_id, cmd }) {
            let _ = self.egress.send(payload);
        }
    }
}

/// Build the distributed `GroupRegistry` for a Postgres deployment: a
/// `PgSyncBus`, a `BusDelivery` for owned actors, the member-delivery ingress,
/// and the inbound-command ingress. Returns the registry the handlers use.
pub async fn build(
    stores: Stores,
    database_url: &str,
    member_sinks: MemberSinks,
) -> Result<GroupRegistry, String> {
    let bus = Arc::new(
        PgSyncBus::connect(database_url)
            .await
            .map_err(|e| format!("sync bus connect: {e}"))?,
    );

    // Outbound: the owner's actors publish per-member messages; deliver every
    // replica's copy into its local sinks.
    pharos_sync::spawn_ingress(bus.as_ref(), member_sinks.clone());

    let delivery = Arc::new(BusDelivery::new(bus.clone(), member_sinks.clone()));
    let distributed = Distributed {
        ownership: Arc::new(StoreOwnership {
            stores: stores.clone(),
            leases: Mutex::new(HashMap::new()),
        }),
        hydration: Arc::new(StoreHydration {
            stores: stores.clone(),
        }),
        commands: Arc::new(BusCommands::new(bus.clone())),
        persistence: Arc::new(StorePersistence {
            stores,
            chains: std::sync::Mutex::new(std::collections::HashMap::new()),
        }),
    };
    let registry = GroupRegistry::spawn_distributed(delivery, distributed);

    // Inbound: apply bus-forwarded commands to whichever groups this replica
    // owns (a no-op for the rest).
    let mut cmd_rx = bus.subscribe();
    let reg = registry.clone();
    tokio::spawn(async move {
        loop {
            match cmd_rx.recv().await {
                Ok(payload) => {
                    if let Ok(BusMsg::Command { group_id, cmd }) = serde_json::from_str(&payload) {
                        let _ = reg.deliver_command(group_id, cmd).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    Ok(registry)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// B31 — persist and remove for the SAME group must apply in call order.
    /// The actor's final mutation persists a 0-member roster and then removes
    /// the row; as two independently-spawned tasks the late upsert could land
    /// AFTER the delete and resurrect the row as an immortal 0-member orphan.
    /// The per-group op chain makes the sequence deterministic.
    #[tokio::test]
    async fn remove_after_persist_never_resurrects_the_row() {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let p = StorePersistence {
            stores: stores.clone(),
            chains: Mutex::new(HashMap::new()),
        };
        for i in 0..200 {
            let gid = GroupId::new();
            // The exact terminal sequence: last-mutation persist, then remove.
            p.persist(gid, 1_000, format!("{{\"iteration\":{i}}}"));
            p.remove(gid);
            // Wait for the chain to drain.
            loop {
                let done = {
                    let chains = p.chains.lock().unwrap();
                    chains.get(&gid).is_none_or(|h| h.is_finished())
                };
                if done {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            let row = stores.get_sync_group(&gid.to_string()).await.unwrap();
            assert!(
                row.is_none(),
                "iteration {i}: remove-after-persist must leave NO row"
            );
        }
    }
}
