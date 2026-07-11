//! Member-sink delivery layer (Phase B4 — zero-downtime deploys).
//!
//! The group actor no longer owns member sockets. Instead it hands every
//! outbound `ServerMsg` to a [`Delivery`] handle addressed by a single
//! `MemberId`, and a per-replica [`MemberSinks`] table owns the `mpsc` senders
//! for the sockets connected to THIS process.
//!
//! Broadcasts are expanded by the ACTOR over its own roster into per-member
//! deliveries — never resolved against the sink table. That is deliberate: a
//! joining socket registers its sink *before* the actor processes its
//! `AddMember`, so resolving a broadcast against the sink table would leak
//! commands the actor queued earlier to a member it hasn't admitted yet (and,
//! e.g., start a late joiner playing during a buffer pause). Driving delivery
//! from the roster means a sink receives nothing until the actor adds its
//! member, then everything from its personalised join catch-up onward.
//!
//! The partition property makes cross-replica delivery correct with zero
//! coordination: a member's socket lives on exactly one replica, so delivering
//! each per-member message on whichever replica holds that member reaches it
//! exactly once cluster-wide.
//!
//! Two `Delivery` impls:
//! - [`LocalDelivery`] — writes straight into a [`MemberSinks`]. The
//!   single-replica / SQLite path uses this directly (no bus hop), and the
//!   multi-replica bus ingress task uses it to place received messages into
//!   local sinks.
//! - `BusDelivery` (Phase B4.3d) — publishes to the cross-replica bus instead.

use crate::messages::{MemberId, ServerMsg};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Fire-and-forget delivery of a `ServerMsg` to one member. Sync (never blocks
/// the actor) and object-safe so the actor can hold an `Arc<dyn Delivery>`
/// chosen at wiring time (local sinks vs the cross-replica bus).
pub trait Delivery: Send + Sync + 'static {
    fn deliver(&self, member_id: MemberId, msg: ServerMsg);
}

/// Per-replica registry of the member sockets connected to THIS process. Shared
/// (cheap `Arc` clone) between the socket layer (which registers/removes sinks)
/// and the delivery layer (which reads them).
#[derive(Clone, Default)]
pub struct MemberSinks {
    inner: Arc<DashMap<MemberId, mpsc::Sender<ServerMsg>>>,
}

impl MemberSinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace, on socket reconnect) a member's sink. Replacing is
    /// how a reconnected socket re-points delivery at its fresh channel without
    /// disturbing the actor's roster.
    pub fn insert(&self, member_id: MemberId, sink: mpsc::Sender<ServerMsg>) {
        self.inner.insert(member_id, sink);
    }

    /// Drop a member's sink (socket closed / member left this replica).
    pub fn remove(&self, member_id: MemberId) {
        self.inner.remove(&member_id);
    }

    /// V19 carried over: a slow/full sink must not block delivery. `try_send`
    /// drops on a full channel; the member reconciles via the next catch-up.
    pub fn send(&self, member_id: MemberId, msg: ServerMsg) {
        if let Some(sink) = self.inner.get(&member_id) {
            let _ = sink.try_send(msg);
        }
    }
}

/// Direct in-process delivery into a [`MemberSinks`]. Correct for a single
/// replica (every member IS local) and the substrate the bus ingress uses.
#[derive(Clone)]
pub struct LocalDelivery {
    sinks: MemberSinks,
}

impl LocalDelivery {
    pub fn new(sinks: MemberSinks) -> Self {
        Self { sinks }
    }
}

impl Delivery for LocalDelivery {
    fn deliver(&self, member_id: MemberId, msg: ServerMsg) {
        self.sinks.send(member_id, msg);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ch() -> (mpsc::Sender<ServerMsg>, mpsc::Receiver<ServerMsg>) {
        mpsc::channel(16)
    }

    #[tokio::test]
    async fn deliver_reaches_the_registered_member() {
        let sinks = MemberSinks::new();
        let d = LocalDelivery::new(sinks.clone());
        let (tx, mut rx) = ch();
        let mid = MemberId::new();
        sinks.insert(mid, tx);
        d.deliver(mid, ServerMsg::Pause { at_server_ms: 5 });
        assert!(matches!(rx.recv().await.unwrap(), ServerMsg::Pause { .. }));
    }

    #[tokio::test]
    async fn deliver_to_unregistered_member_is_a_noop() {
        let sinks = MemberSinks::new();
        let d = LocalDelivery::new(sinks.clone());
        // No sink registered — must not panic, just drop.
        d.deliver(MemberId::new(), ServerMsg::Pause { at_server_ms: 5 });
    }

    #[tokio::test]
    async fn removed_member_stops_receiving() {
        let sinks = MemberSinks::new();
        let d = LocalDelivery::new(sinks.clone());
        let (tx, mut rx) = ch();
        let mid = MemberId::new();
        sinks.insert(mid, tx);
        sinks.remove(mid);
        d.deliver(mid, ServerMsg::Pause { at_server_ms: 1 });
        assert!(rx.try_recv().is_err(), "removed sink receives nothing");
    }

    #[tokio::test]
    async fn reconnect_replaces_the_sink() {
        let sinks = MemberSinks::new();
        let d = LocalDelivery::new(sinks.clone());
        let mid = MemberId::new();
        let (tx1, mut rx1) = ch();
        sinks.insert(mid, tx1);
        // Reconnect: same member, fresh sink.
        let (tx2, mut rx2) = ch();
        sinks.insert(mid, tx2);
        d.deliver(mid, ServerMsg::Pause { at_server_ms: 1 });
        assert!(rx1.try_recv().is_err(), "stale sink must not receive");
        assert!(matches!(rx2.recv().await.unwrap(), ServerMsg::Pause { .. }));
    }
}
