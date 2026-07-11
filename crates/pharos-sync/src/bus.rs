//! Cross-replica delivery bus (Phase B4 — zero-downtime deploys).
//!
//! During a rolling deploy two replicas run against one Postgres and a group's
//! members can be split across both. The [`SyncBus`] is the pipe that carries a
//! group command/update from the replica that processes it to every replica
//! that has a member socket for that group — so a `Pause` issued on the new pod
//! reaches a viewer still connected to the old one.
//!
//! The bus is deliberately dumb: it moves opaque `String` payloads over ONE
//! global channel. The sync layer serializes a `BusEnvelope` (group id +
//! target + `ServerMsg`, or a routed command) into that string and filters on
//! receipt — the bus itself is protocol-agnostic, mirroring the "opaque JSON at
//! the boundary" convention the store layer uses. A single global channel (vs
//! per-group LISTEN/UNLISTEN churn) is ample for watch-party traffic and keeps
//! both the Postgres NOTIFY impl and the in-process impl trivial.
//!
//! Two impls:
//! - [`LocalSyncBus`] — an in-process `tokio::broadcast`, used by the
//!   single-replica / SQLite path and by tests.
//! - `PgSyncBus` (in `pharos-store-sqlx`) — Postgres `LISTEN`/`NOTIFY`.

use tokio::sync::broadcast;

/// Default fan-out buffer. SyncPlay command volume is human-scale (a handful of
/// events per group per minute), so a lagging subscriber that overruns this has
/// bigger problems; it reconciles via the group snapshot regardless.
pub const DEFAULT_BUS_CAPACITY: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("sync bus publish failed: {0}")]
    Backend(String),
}

/// A process-spanning publish/subscribe pipe for one global sync channel.
///
/// `publish` broadcasts a payload to every replica (including the publisher).
/// `subscribe` returns a receiver that yields every payload published AFTER the
/// subscribe call — replicas subscribe once at startup and hold the receiver
/// for the process lifetime, so this "no replay" semantics is fine.
pub trait SyncBus: Send + Sync + 'static {
    fn publish(
        &self,
        payload: String,
    ) -> impl std::future::Future<Output = Result<(), BusError>> + Send;

    fn subscribe(&self) -> broadcast::Receiver<String>;
}

/// In-process bus over `tokio::broadcast`. Correct for a single replica (every
/// subscriber shares one process) and the substrate every unit test runs on.
#[derive(Clone)]
pub struct LocalSyncBus {
    tx: broadcast::Sender<String>,
}

impl LocalSyncBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUS_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }
}

impl Default for LocalSyncBus {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncBus for LocalSyncBus {
    async fn publish(&self, payload: String) -> Result<(), BusError> {
        // `send` errors only when there are zero receivers — that is not a
        // failure for a fire-and-forget broadcast (nobody is listening yet),
        // so it is swallowed rather than surfaced.
        let _ = self.tx.send(payload);
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

    #[tokio::test]
    async fn every_subscriber_receives_a_published_payload() {
        let bus = LocalSyncBus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.publish("hello".into()).await.unwrap();
        assert_eq!(a.recv().await.unwrap(), "hello");
        assert_eq!(b.recv().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn subscribe_after_publish_misses_the_earlier_payload() {
        // No-replay semantics: a replica only sees traffic issued after it
        // subscribed (it re-hydrates prior state from the snapshot instead).
        let bus = LocalSyncBus::new();
        bus.publish("early".into()).await.unwrap();
        let mut late = bus.subscribe();
        bus.publish("later".into()).await.unwrap();
        assert_eq!(late.recv().await.unwrap(), "later");
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_not_an_error() {
        let bus = LocalSyncBus::new();
        // Nobody listening — a fire-and-forget publish must still succeed.
        bus.publish("into the void".into()).await.unwrap();
    }
}
