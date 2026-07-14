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
    inner: Arc<DashMap<MemberId, SinkEntry>>,
}

/// A member's outbound sink tagged with the connection generation that owns it.
/// `gen` is the hub's per-device `conn_gen` (member_id derives from the device,
/// so it is a strictly-increasing generation for this member). Delivery reads
/// only `sink`; `gen` exists so a stale writer can never clobber a live socket.
struct SinkEntry {
    gen: u64,
    sink: mpsc::Sender<ServerMsg>,
}

impl MemberSinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace, on socket reconnect) a member's sink, generation-
    /// fenced. `gen` is the registering socket's `conn_gen`. A write only lands
    /// when `gen >= the stored generation`, so two sockets for one member racing
    /// their inserts (fast reconnect, or an HTTP New/Join carrying a snapshot
    /// sink resolved just before a reconnect) can NEVER leave the older, dead
    /// sink installed over the newer live one (B56). `>=` (not `>`) keeps a
    /// same-generation re-insert (socket connect + HTTP add for the same
    /// connection) idempotent.
    pub fn insert(&self, member_id: MemberId, gen: u64, sink: mpsc::Sender<ServerMsg>) {
        use dashmap::mapref::entry::Entry;
        match self.inner.entry(member_id) {
            Entry::Occupied(mut o) => {
                if gen >= o.get().gen {
                    *o.get_mut() = SinkEntry { gen, sink };
                }
            }
            Entry::Vacant(v) => {
                v.insert(SinkEntry { gen, sink });
            }
        }
    }

    /// Drop a member's sink (socket closed / member left this replica), only if
    /// `gen` still owns it. A reconnect installs a higher generation, so a stale
    /// disconnect teardown from an older socket no-ops instead of wiping the
    /// live sink the reconnect just registered (B56).
    pub fn remove(&self, member_id: MemberId, gen: u64) {
        self.inner.remove_if(&member_id, |_, e| e.gen == gen);
    }

    /// Whether this replica currently holds `member_id`'s socket. Lets a
    /// bus-backed delivery short-circuit to a direct local send (which has no
    /// payload-size cap, unlike Postgres `NOTIFY`) for same-replica members.
    pub fn contains(&self, member_id: MemberId) -> bool {
        self.inner.contains_key(&member_id)
    }

    /// V19 carried over: a slow/full sink must not block delivery. `try_send`
    /// drops on a full channel; the member reconciles via the next catch-up.
    pub fn send(&self, member_id: MemberId, msg: ServerMsg) {
        if let Some(e) = self.inner.get(&member_id) {
            let _ = e.sink.try_send(msg);
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

    /// The underlying sink table (so a wrapper can test membership locality).
    pub fn sinks(&self) -> &MemberSinks {
        &self.sinks
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
        sinks.insert(mid, 1, tx);
        d.deliver(
            mid,
            ServerMsg::Pause {
                at_server_ms: 5,
                position_ms: 0,
            },
        );
        assert!(matches!(rx.recv().await.unwrap(), ServerMsg::Pause { .. }));
    }

    #[tokio::test]
    async fn deliver_to_unregistered_member_is_a_noop() {
        let sinks = MemberSinks::new();
        let d = LocalDelivery::new(sinks.clone());
        // No sink registered — must not panic, just drop.
        d.deliver(
            MemberId::new(),
            ServerMsg::Pause {
                at_server_ms: 5,
                position_ms: 0,
            },
        );
    }

    #[tokio::test]
    async fn removed_member_stops_receiving() {
        let sinks = MemberSinks::new();
        let d = LocalDelivery::new(sinks.clone());
        let (tx, mut rx) = ch();
        let mid = MemberId::new();
        sinks.insert(mid, 1, tx);
        sinks.remove(mid, 1);
        d.deliver(
            mid,
            ServerMsg::Pause {
                at_server_ms: 1,
                position_ms: 0,
            },
        );
        assert!(rx.try_recv().is_err(), "removed sink receives nothing");
    }

    #[tokio::test]
    async fn reconnect_replaces_the_sink() {
        let sinks = MemberSinks::new();
        let d = LocalDelivery::new(sinks.clone());
        let mid = MemberId::new();
        let (tx1, mut rx1) = ch();
        sinks.insert(mid, 1, tx1);
        // Reconnect: same member, fresh sink.
        let (tx2, mut rx2) = ch();
        sinks.insert(mid, 1, tx2);
        d.deliver(
            mid,
            ServerMsg::Pause {
                at_server_ms: 1,
                position_ms: 0,
            },
        );
        assert!(rx1.try_recv().is_err(), "stale sink must not receive");
        assert!(matches!(rx2.recv().await.unwrap(), ServerMsg::Pause { .. }));
    }

    /// B56 — an OLDER-generation insert (a slow/stale writer: an HTTP New/Join
    /// carrying a snapshot sink resolved just before a reconnect, or the losing
    /// task of two racing connects) must NOT clobber the newer live sink. This
    /// is the wedge: a dead sink installed over a live one silently blackholes
    /// all of a member's SyncPlay traffic until its next reconnect.
    #[tokio::test]
    async fn stale_generation_insert_does_not_clobber_the_live_sink() {
        let sinks = MemberSinks::new();
        let d = LocalDelivery::new(sinks.clone());
        let mid = MemberId::new();
        let (tx_live, mut rx_live) = ch();
        sinks.insert(mid, 2, tx_live); // gen 2 = the reconnected, live socket
        let (tx_stale, mut rx_stale) = ch();
        sinks.insert(mid, 1, tx_stale); // gen 1 = the older writer, arrives late
        d.deliver(
            mid,
            ServerMsg::Pause {
                at_server_ms: 1,
                position_ms: 0,
            },
        );
        assert!(
            rx_stale.try_recv().is_err(),
            "older-gen sink must be rejected"
        );
        assert!(
            matches!(rx_live.recv().await.unwrap(), ServerMsg::Pause { .. }),
            "live newer-gen sink still delivers"
        );
    }

    /// B56 — a stale disconnect teardown (older generation) must NOT remove the
    /// sink a newer reconnect just installed.
    #[tokio::test]
    async fn stale_generation_remove_spares_the_live_sink() {
        let sinks = MemberSinks::new();
        let d = LocalDelivery::new(sinks.clone());
        let mid = MemberId::new();
        let (tx_live, mut rx_live) = ch();
        sinks.insert(mid, 2, tx_live);
        sinks.remove(mid, 1); // the gen-1 socket's late teardown
        d.deliver(
            mid,
            ServerMsg::Pause {
                at_server_ms: 1,
                position_ms: 0,
            },
        );
        assert!(
            matches!(rx_live.recv().await.unwrap(), ServerMsg::Pause { .. }),
            "live sink survives a stale-gen remove"
        );
    }
}
