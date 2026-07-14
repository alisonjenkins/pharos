//! Cross-replica delivery over the [`SyncBus`] (Phase B4.3d).
//!
//! [`BusDelivery`] is the [`Delivery`] the owner replica's group actor uses
//! under Postgres: instead of writing straight into local sinks, it publishes
//! each per-member message onto the bus. Every replica runs a [`spawn_ingress`]
//! task that receives those envelopes and hands them to a [`LocalDelivery`] over
//! its OWN [`MemberSinks`]. Because a member's sink lives on exactly one replica
//! (the partition property), each member is delivered to exactly once
//! cluster-wide — no dedup, no coordination.
//!
//! Ordering is preserved: `deliver` is fire-and-forget but funnels through a
//! single egress channel drained by one task, so `Play` can't overtake the
//! `Pause` issued before it.

use crate::bus::SyncBus;
use crate::delivery::{Delivery, LocalDelivery, MemberSinks};
use crate::group::RemoteCommand;
use crate::messages::{GroupId, MemberId, ServerMsg};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Wire envelope carried on the bus: outbound per-member delivery (owner → all)
/// and inbound command routing (non-owner → owner).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BusMsg {
    /// Owner → every replica: deliver `msg` to `member_id` wherever it is
    /// connected. Replicas without that member ignore it.
    Deliver { member_id: MemberId, msg: ServerMsg },
    /// A non-owner replica → the owner: apply `cmd` to group `group_id`. Only
    /// the replica that owns the group acts on it; others ignore it.
    Command {
        group_id: GroupId,
        cmd: RemoteCommand,
    },
}

/// A [`Delivery`] that delivers to same-replica members directly and publishes
/// only cross-replica messages to the bus. Cheap to clone (shares the egress
/// channel + the sink table).
#[derive(Clone)]
pub struct BusDelivery {
    egress: mpsc::UnboundedSender<String>,
    /// The sockets connected to THIS replica. A member found here is delivered
    /// to directly — bypassing the bus, so it is NOT subject to the Postgres
    /// `NOTIFY` 8000-byte payload cap that silently drops a large `PlayQueue`.
    local: LocalDelivery,
}

impl BusDelivery {
    /// Build a bus-backed delivery over `bus`, spawning the egress task that
    /// serializes envelopes and publishes them in order. `sinks` is this
    /// replica's member-sink table, used to short-circuit same-replica
    /// deliveries to a direct local send.
    pub fn new<B: SyncBus>(bus: Arc<B>, sinks: MemberSinks) -> Self {
        let (egress, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(payload) = rx.recv().await {
                // A publish failure (e.g. payload over the NOTIFY cap, or a
                // transient DB blip) drops this one message; the member
                // reconciles via the next catch-up. Best-effort, same as the
                // V19 `try_send` drop philosophy — keep draining.
                let _ = bus.publish(payload).await;
            }
        });
        Self {
            egress,
            local: LocalDelivery::new(sinks),
        }
    }
}

impl Delivery for BusDelivery {
    fn deliver(&self, member_id: MemberId, msg: ServerMsg) {
        // Local-first: a member's socket lives on exactly one replica (the
        // partition property), so if it is HERE, deliver straight into its sink.
        // This both avoids a needless bus round-trip AND — critically — sidesteps
        // the Postgres NOTIFY payload cap that would otherwise silently drop a
        // whole-season `PlayQueue`. Only genuinely-remote members go on the bus.
        if self.local.sinks().contains(member_id) {
            self.local.deliver(member_id, msg);
            return;
        }
        let env = BusMsg::Deliver { member_id, msg };
        if let Ok(payload) = serde_json::to_string(&env) {
            // Send failure means the egress task is gone (process shutting
            // down) — nothing to do.
            let _ = self.egress.send(payload);
        }
    }
}

/// Spawn the per-replica ingress: subscribe to `bus` and deliver every
/// `BusMsg::Deliver` into `sinks` (the sockets connected to THIS replica).
pub fn spawn_ingress<B: SyncBus>(bus: &B, sinks: MemberSinks) {
    let mut rx = bus.subscribe();
    let local = LocalDelivery::new(sinks);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(payload) => match serde_json::from_str::<BusMsg>(&payload) {
                    Ok(BusMsg::Deliver { member_id, msg }) => local.deliver(member_id, msg),
                    // Command envelopes are handled by the command ingress
                    // (which owns the registry), not this delivery ingress.
                    Ok(BusMsg::Command { .. }) => {}
                    Err(_) => { /* not a bus envelope — ignore */ }
                },
                // A slow ingress that overran the broadcast buffer skips the
                // gap; state re-syncs via catch-up. Keep going.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::bus::LocalSyncBus;
    use std::time::Duration;
    use tokio::sync::mpsc as tmpsc;

    #[tokio::test]
    async fn deliver_crosses_replicas_to_the_members_sink() {
        // One bus models the shared NOTIFY channel. Replica A's actor delivers
        // through BusDelivery; replica B runs the ingress into its own sinks.
        // A member connected on B must receive the message.
        let bus = Arc::new(LocalSyncBus::new());

        // Replica B: sinks + ingress.
        let sinks_b = MemberSinks::new();
        spawn_ingress(bus.as_ref(), sinks_b.clone());
        let (tx, mut rx) = tmpsc::channel(8);
        let member = MemberId::new();
        sinks_b.insert(member, 1, tx);

        // Let B's ingress subscribe before A publishes.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Replica A: deliver to `member` (whose sink is on B).
        let delivery_a = BusDelivery::new(bus.clone(), MemberSinks::new());
        delivery_a.deliver(
            member,
            ServerMsg::Pause {
                at_server_ms: 42,
                position_ms: 0,
            },
        );

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("delivery must arrive")
            .expect("channel open");
        assert!(matches!(
            got,
            ServerMsg::Pause {
                at_server_ms: 42,
                position_ms: 0
            }
        ));
    }

    #[tokio::test]
    async fn local_member_is_delivered_directly_bypassing_the_bus() {
        // The bug: a same-replica member's message was round-tripped through the
        // bus (Postgres NOTIFY, 8000-byte cap), so a large PlayQueue was silently
        // dropped. Local-first delivery must reach a local member WITHOUT any bus
        // hop — proven here by running NO ingress at all and delivering a large
        // PlayQueue: it must still arrive.
        let bus = Arc::new(LocalSyncBus::new());
        let sinks = MemberSinks::new();
        let (tx, mut rx) = tmpsc::channel(8);
        let member = MemberId::new();
        sinks.insert(member, 1, tx);

        // BusDelivery over THIS replica's sinks — no spawn_ingress anywhere.
        let delivery = BusDelivery::new(bus.clone(), sinks.clone());
        let big = ServerMsg::PlayQueue {
            reason: "new_playlist".into(),
            items: (0..500)
                .map(|i| crate::messages::QueueItemInfo {
                    item_id: format!("item-{i}-0123456789abcdef"),
                    playlist_item_id: format!("pli-{i}-0123456789abcdef"),
                })
                .collect(),
            playing_index: 0,
            start_position_ms: 0,
            is_playing: true,
            repeat_mode: String::new(),
            shuffle_mode: String::new(),
            last_update_unix_ms: 1,
        };
        delivery.deliver(member, big);

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("local delivery must arrive without a bus/ingress")
            .expect("channel open");
        match got {
            ServerMsg::PlayQueue { items, .. } => assert_eq!(items.len(), 500),
            other => panic!("expected the large PlayQueue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ingress_ignores_a_member_not_local_to_this_replica() {
        // A delivery for a member whose sink lives on ANOTHER replica must be a
        // no-op here (no panic, nothing delivered to unrelated sinks).
        let bus = Arc::new(LocalSyncBus::new());
        let sinks = MemberSinks::new();
        spawn_ingress(bus.as_ref(), sinks.clone());
        let (tx, mut rx) = tmpsc::channel(8);
        let local_member = MemberId::new();
        sinks.insert(local_member, 1, tx);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let delivery = BusDelivery::new(bus.clone(), MemberSinks::new());
        // Deliver to some OTHER member id not registered here.
        delivery.deliver(
            MemberId::new(),
            ServerMsg::Pause {
                at_server_ms: 1,
                position_ms: 0,
            },
        );

        // The local member must NOT receive it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "unrelated member received a message"
        );
    }

    #[tokio::test]
    async fn delivery_order_is_preserved() {
        let bus = Arc::new(LocalSyncBus::new());
        let sinks = MemberSinks::new();
        spawn_ingress(bus.as_ref(), sinks.clone());
        let (tx, mut rx) = tmpsc::channel(16);
        let member = MemberId::new();
        sinks.insert(member, 1, tx);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let delivery = BusDelivery::new(bus.clone(), MemberSinks::new());
        for i in 0..5 {
            delivery.deliver(
                member,
                ServerMsg::Seek {
                    at_server_ms: i,
                    position_ms: i,
                },
            );
        }
        for i in 0..5 {
            let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            match got {
                ServerMsg::Seek { at_server_ms, .. } => assert_eq!(at_server_ms, i, "out of order"),
                other => panic!("expected Seek, got {other:?}"),
            }
        }
    }
}
