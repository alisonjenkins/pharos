//! Postgres `LISTEN`/`NOTIFY` implementation of [`pharos_sync::SyncBus`]
//! (Phase B4 — zero-downtime deploys).
//!
//! Lives in the store crate because it owns the sqlx/Postgres dependency. It
//! is a thin transport: every payload the sync layer publishes becomes a
//! `pg_notify('pharos_sync', payload)`, and a single background task per
//! process `LISTEN`s on that channel and re-broadcasts each notification into a
//! `tokio::broadcast` every local subscriber shares. NOTIFY fans out to every
//! connected backend — including the publisher's own — so a command processed
//! on one replica reaches member sockets on all of them.

use pharos_sync::bus::{BusError, SyncBus};
use sqlx::postgres::{PgListener, PgPoolOptions};
use sqlx::PgPool;
use tokio::sync::broadcast;

/// The single global channel all sync traffic rides. Postgres channel names are
/// identifiers; this fixed one avoids any per-group `LISTEN`/`UNLISTEN` churn.
const CHANNEL: &str = "pharos_sync";

/// Postgres caps a NOTIFY payload at 8000 bytes. We refuse anything close to
/// that so a large `PlayQueue` update can't silently fail mid-transport — the
/// caller falls back to snapshot-based re-hydration instead. In practice watch
/// party payloads are well under this (a season queue is ~2 KiB).
const MAX_PAYLOAD_BYTES: usize = 7000;

/// Backoff between listener reconnect attempts after the LISTEN connection drops.
const RECONNECT_BACKOFF_MS: u64 = 500;

/// Postgres-backed cross-replica bus. `Clone` is cheap (shares the publish pool
/// + the broadcast sender).
#[derive(Clone)]
pub struct PgSyncBus {
    pool: PgPool,
    tx: broadcast::Sender<String>,
}

impl PgSyncBus {
    /// Connect a bus to `url`: opens a small dedicated publish pool and spawns
    /// the background `LISTEN` task. The task reconnects with backoff if its
    /// connection drops, so the bus survives transient Postgres blips.
    pub async fn connect(url: &str) -> Result<Self, BusError> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(url)
            .await
            .map_err(|e| BusError::Backend(format!("bus publish pool connect: {e}")))?;
        let (tx, _rx) = broadcast::channel(pharos_sync::bus::DEFAULT_BUS_CAPACITY);
        spawn_listener(url.to_string(), tx.clone());
        Ok(Self { pool, tx })
    }
}

/// Background task: hold a `LISTEN pharos_sync` connection and forward every
/// notification into `tx`. Rebuilds the listener with backoff on any error, so
/// a dropped connection self-heals rather than silently going deaf.
fn spawn_listener(url: String, tx: broadcast::Sender<String>) {
    tokio::spawn(async move {
        loop {
            match run_listener(&url, &tx).await {
                Ok(()) => {
                    // `run_listener` only returns Ok when every subscriber is
                    // gone (the broadcast sender closed) — nothing left to feed.
                    tracing::debug!("pg sync bus listener stopping: no subscribers remain");
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        backoff_ms = RECONNECT_BACKOFF_MS,
                        "pg sync bus listener dropped; reconnecting"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(RECONNECT_BACKOFF_MS))
                        .await;
                }
            }
        }
    });
}

async fn run_listener(url: &str, tx: &broadcast::Sender<String>) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect(url).await?;
    listener.listen(CHANNEL).await?;
    tracing::info!(channel = CHANNEL, "pg sync bus listening");
    loop {
        let notification = listener.recv().await?;
        // A send error means zero receivers; keep listening — a subscriber may
        // (re)appear. Only stop if the sender itself is closed, which cannot
        // happen while `PgSyncBus` (holder of a `tx` clone) is alive; treat a
        // send error as "no listeners right now" and continue.
        let _ = tx.send(notification.payload().to_string());
    }
}

impl SyncBus for PgSyncBus {
    async fn publish(&self, payload: String) -> Result<(), BusError> {
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(BusError::Backend(format!(
                "payload {} bytes exceeds NOTIFY cap {}",
                payload.len(),
                MAX_PAYLOAD_BYTES
            )));
        }
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(CHANNEL)
            .bind(&payload)
            .execute(&self.pool)
            .await
            .map_err(|e| BusError::Backend(format!("pg_notify: {e}")))?;
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Two independent `PgSyncBus` instances against one database model two
    /// replicas: a payload published on A must arrive at a subscriber on B via
    /// real Postgres NOTIFY. This is the cross-replica delivery B4 relies on.
    #[tokio::test]
    async fn notify_crosses_replicas() {
        let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
            eprintln!("SKIP notify_crosses_replicas: PHAROS_TEST_POSTGRES_URL unset");
            return;
        };
        let replica_a = PgSyncBus::connect(&url).await.unwrap();
        let replica_b = PgSyncBus::connect(&url).await.unwrap();
        let mut on_b = replica_b.subscribe();

        // Give B's LISTEN task a moment to establish before A publishes, else
        // the notification predates the LISTEN and is legitimately missed.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        replica_a
            .publish("cross-replica-hello".into())
            .await
            .unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), on_b.recv())
            .await
            .expect("notification must arrive within 5s")
            .expect("broadcast recv");
        assert_eq!(got, "cross-replica-hello");
    }

    #[tokio::test]
    async fn oversized_payload_is_rejected() {
        let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
            eprintln!("SKIP oversized_payload_is_rejected: PHAROS_TEST_POSTGRES_URL unset");
            return;
        };
        let bus = PgSyncBus::connect(&url).await.unwrap();
        let huge = "x".repeat(MAX_PAYLOAD_BYTES + 1);
        assert!(
            bus.publish(huge).await.is_err(),
            "payload above the NOTIFY cap must be refused, not silently dropped"
        );
    }
}
