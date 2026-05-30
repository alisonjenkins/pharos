//! Group actor — one tokio task per active group. Owns membership, leader,
//! per-member clock offsets, and broadcast policy. mpsc inbox; member sinks
//! are owned, not shared (V18).
//!
//! V3: scheduling lead time is `MIN_LEAD_MS + max(member.median_rtt)/2` so
//! a slow member never gets a Play scheduled in its past.
//! V19: per-member buffering — one member's BufferingStart pauses the group
//! once; a second BufferingStart while paused is a no-op (no buffer storm).
//! V20: actor never sees Jellyfin shapes; only `ServerMsg` flows out.

use super::clock::ClockOffset;
use super::messages::{ErrorCode, GroupId, MemberId, MemberSummary, ServerMsg};
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

/// Minimum delay between "server decides to play" and `at_server_ms`.
/// Each member subtracts its own offset to schedule locally — see
/// `docs/group-sync-protocol.md` §4.
pub const MIN_LEAD_MS: u64 = 200;

#[derive(Debug)]
pub enum GroupMsg {
    AddMember {
        member_id: MemberId,
        name: String,
        sink: mpsc::Sender<ServerMsg>,
        reply: oneshot::Sender<Joined>,
    },
    RemoveMember {
        member_id: MemberId,
    },
    LeaderPlay {
        sender: MemberId,
        position_ms: u64,
    },
    LeaderPause {
        sender: MemberId,
    },
    LeaderSeek {
        sender: MemberId,
        position_ms: u64,
    },
    ObserveClock {
        member_id: MemberId,
        t1: u64,
        t2: u64,
        t3: u64,
        t4: u64,
    },
    BufferingStart {
        member_id: MemberId,
        position_ms: u64,
    },
    BufferingEnd {
        member_id: MemberId,
    },
    Snapshot {
        reply: oneshot::Sender<GroupSnapshot>,
    },
}

#[derive(Debug)]
pub struct Joined {
    pub group_id: GroupId,
    pub leader: MemberId,
    pub members: Vec<MemberSummary>,
}

#[derive(Debug, Clone)]
pub struct GroupSnapshot {
    pub id: GroupId,
    pub leader: Option<MemberId>,
    pub member_count: usize,
    pub buffering_member_count: usize,
}

struct MemberRec {
    name: String,
    sink: mpsc::Sender<ServerMsg>,
    offset: ClockOffset,
    buffering: bool,
}

/// Last broadcast playback state. V19 — kept on the actor so a late
/// joiner (or a network-blip reconnect) immediately syncs to the
/// group's current position instead of staying frozen at its own
/// startup time.
#[derive(Debug, Clone, Copy)]
enum PlaybackState {
    Idle,
    Playing {
        position_ms: u64,
        /// `server_ms_now()` at the moment of the last `Play`.
        anchor_server_ms: u64,
    },
    Paused {
        position_ms: u64,
    },
}

struct GroupState {
    id: GroupId,
    started_at: Instant,
    members: HashMap<MemberId, MemberRec>,
    leader: Option<MemberId>,
    group_paused_due_to_buffering: bool,
    playback: PlaybackState,
}

impl GroupState {
    fn new(id: GroupId) -> Self {
        Self {
            id,
            started_at: Instant::now(),
            members: HashMap::new(),
            leader: None,
            group_paused_due_to_buffering: false,
            playback: PlaybackState::Idle,
        }
    }

    fn server_ms_now(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Lowest-MemberId-wins election. Deterministic, no voting needed.
    fn elect_leader(&mut self) {
        self.leader = self.members.keys().min().copied();
    }

    fn lead_time_ms(&self) -> u64 {
        let half_max_rtt = self
            .members
            .values()
            .map(|m| m.offset.max_rtt_ms() / 2)
            .max()
            .unwrap_or(0);
        MIN_LEAD_MS + half_max_rtt
    }

    /// V19: one slow / wedged member must not block the actor or
    /// delay broadcasts to everyone else. `try_send` returns
    /// immediately; on a full sink the message is dropped (the
    /// member will reconcile via the next state catch-up).
    fn broadcast(&self, msg: ServerMsg) {
        for m in self.members.values() {
            let _ = m.sink.try_send(msg.clone());
        }
    }

    fn send_one(&self, to: MemberId, msg: ServerMsg) {
        if let Some(m) = self.members.get(&to) {
            let _ = m.sink.try_send(msg);
        }
    }

    fn member_summaries(&self) -> Vec<MemberSummary> {
        let mut out: Vec<_> = self
            .members
            .iter()
            .map(|(id, m)| MemberSummary {
                member_id: *id,
                name: m.name.clone(),
                is_leader: Some(*id) == self.leader,
            })
            .collect();
        out.sort_by_key(|s| s.member_id);
        out
    }
}

#[derive(Clone)]
pub struct GroupHandle {
    pub group_id: GroupId,
    pub tx: mpsc::Sender<GroupMsg>,
}

impl GroupHandle {
    /// Spawn a fresh group actor.
    pub fn spawn(group_id: GroupId) -> Self {
        let (tx, mut rx) = mpsc::channel::<GroupMsg>(256);
        let mut state = GroupState::new(group_id);
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                handle(&mut state, msg).await;
                if state.members.is_empty() {
                    // No members → terminate. Registry will spawn a new one on
                    // next Join.
                    break;
                }
            }
        });
        Self { tx, group_id }
    }

    /// Request a `GroupSnapshot` from the actor. Returns `None` when
    /// the actor has terminated (every member left). Used by the HTTP
    /// `/SyncPlay/List` surface.
    pub async fn snapshot(&self) -> Option<GroupSnapshot> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(GroupMsg::Snapshot { reply: tx }).await.ok()?;
        rx.await.ok()
    }
}

async fn handle(state: &mut GroupState, msg: GroupMsg) {
    match msg {
        GroupMsg::AddMember {
            member_id,
            name,
            sink,
            reply,
        } => {
            let was_empty = state.members.is_empty();
            state.members.insert(
                member_id,
                MemberRec {
                    name: name.clone(),
                    sink,
                    offset: ClockOffset::default(),
                    buffering: false,
                },
            );
            if was_empty {
                state.elect_leader();
            }
            let summaries = state.member_summaries();
            let leader = state.leader.unwrap_or(member_id); // Always Some after election above.
            let _ = reply.send(Joined {
                group_id: state.id,
                leader,
                members: summaries.clone(),
            });
            // Tell existing members someone joined.
            let me = MemberSummary {
                member_id,
                name,
                is_leader: Some(member_id) == state.leader,
            };
            for (other_id, rec) in &state.members {
                if *other_id != member_id {
                    let _ = rec
                        .sink
                        .try_send(ServerMsg::MemberJoined { member: me.clone() });
                }
            }
            // V19: late joiner catch-up. If the group is in flight,
            // send the most recent Play/Pause/Seek to just this member
            // so its UI doesn't sit at position 0 until the next
            // leader command.
            let server_ms = state.server_ms_now();
            match state.playback {
                PlaybackState::Idle => {}
                PlaybackState::Playing {
                    position_ms,
                    anchor_server_ms,
                } => {
                    let elapsed = server_ms.saturating_sub(anchor_server_ms);
                    state.send_one(
                        member_id,
                        ServerMsg::Play {
                            at_server_ms: server_ms + MIN_LEAD_MS,
                            position_ms: position_ms + elapsed,
                        },
                    );
                }
                PlaybackState::Paused { position_ms } => {
                    state.send_one(
                        member_id,
                        ServerMsg::Seek {
                            at_server_ms: server_ms + MIN_LEAD_MS,
                            position_ms,
                        },
                    );
                    state.send_one(
                        member_id,
                        ServerMsg::Pause {
                            at_server_ms: server_ms + MIN_LEAD_MS,
                        },
                    );
                }
            }
        }
        GroupMsg::RemoveMember { member_id } => {
            let was_leader = state.leader == Some(member_id);
            state.members.remove(&member_id);
            if was_leader {
                state.elect_leader();
                if let Some(new_leader) = state.leader {
                    state.broadcast(ServerMsg::LeaderChange { leader: new_leader });
                }
            }
            state.broadcast(ServerMsg::MemberLeft { member_id });
        }
        GroupMsg::LeaderPlay {
            sender,
            position_ms,
        } => {
            if state.leader != Some(sender) {
                state.send_one(
                    sender,
                    ServerMsg::Error {
                        code: ErrorCode::NotLeader,
                        detail: "only leader may issue Play".into(),
                    },
                );
                return;
            }
            let server_ms = state.server_ms_now();
            let at_server_ms = server_ms + state.lead_time_ms();
            state.playback = PlaybackState::Playing {
                position_ms,
                anchor_server_ms: server_ms,
            };
            state.broadcast(ServerMsg::Play {
                at_server_ms,
                position_ms,
            });
        }
        GroupMsg::LeaderPause { sender } => {
            if state.leader != Some(sender) {
                state.send_one(
                    sender,
                    ServerMsg::Error {
                        code: ErrorCode::NotLeader,
                        detail: "only leader may issue Pause".into(),
                    },
                );
                return;
            }
            let server_ms = state.server_ms_now();
            let at_server_ms = server_ms + state.lead_time_ms();
            // Freeze position at the moment we paused so late joiners
            // get the correct still-frame.
            if let PlaybackState::Playing {
                position_ms,
                anchor_server_ms,
            } = state.playback
            {
                let elapsed = server_ms.saturating_sub(anchor_server_ms);
                state.playback = PlaybackState::Paused {
                    position_ms: position_ms + elapsed,
                };
            }
            state.broadcast(ServerMsg::Pause { at_server_ms });
        }
        GroupMsg::LeaderSeek {
            sender,
            position_ms,
        } => {
            if state.leader != Some(sender) {
                state.send_one(
                    sender,
                    ServerMsg::Error {
                        code: ErrorCode::NotLeader,
                        detail: "only leader may issue Seek".into(),
                    },
                );
                return;
            }
            let server_ms = state.server_ms_now();
            let at_server_ms = server_ms + state.lead_time_ms();
            // Seek preserves play/pause; only mutates the position
            // anchor. Idle treats Seek as "load this position paused".
            state.playback = match state.playback {
                PlaybackState::Playing { .. } => PlaybackState::Playing {
                    position_ms,
                    anchor_server_ms: server_ms,
                },
                _ => PlaybackState::Paused { position_ms },
            };
            state.broadcast(ServerMsg::Seek {
                at_server_ms,
                position_ms,
            });
        }
        GroupMsg::ObserveClock {
            member_id,
            t1,
            t2,
            t3,
            t4,
        } => {
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.offset.observe(t1, t2, t3, t4);
            }
        }
        GroupMsg::BufferingStart {
            member_id,
            position_ms: _,
        } => {
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.buffering = true;
            }
            // V19: one corrective Pause, not a storm. If already paused due
            // to another member's buffering, do nothing.
            if !state.group_paused_due_to_buffering && state.members.values().any(|m| m.buffering) {
                state.group_paused_due_to_buffering = true;
                let server_ms = state.server_ms_now();
                // Freeze playback state too, the same way LeaderPause does.
                // Without this, `playback` stays `Playing` for the whole
                // buffering window, so a member joining during the window
                // hits the late-joiner catch-up and is told to *Play* —
                // desynced from everyone else who is paused. (V19 buffer
                // isolation.)
                if let PlaybackState::Playing {
                    position_ms,
                    anchor_server_ms,
                } = state.playback
                {
                    let elapsed = server_ms.saturating_sub(anchor_server_ms);
                    state.playback = PlaybackState::Paused {
                        position_ms: position_ms + elapsed,
                    };
                }
                let at_server_ms = server_ms + MIN_LEAD_MS;
                state.broadcast(ServerMsg::Pause { at_server_ms });
            }
        }
        GroupMsg::BufferingEnd { member_id } => {
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.buffering = false;
            }
            if state.group_paused_due_to_buffering && !state.members.values().any(|m| m.buffering) {
                state.group_paused_due_to_buffering = false;
                // No automatic resume — leader decides when to continue.
            }
        }
        GroupMsg::Snapshot { reply } => {
            let snap = GroupSnapshot {
                id: state.id,
                leader: state.leader,
                member_count: state.members.len(),
                buffering_member_count: state.members.values().filter(|m| m.buffering).count(),
            };
            let _ = reply.send(snap);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::time::Duration;

    async fn fresh() -> (GroupHandle, mpsc::Receiver<ServerMsg>, MemberId) {
        let h = GroupHandle::spawn(GroupId::new());
        let (tx, rx) = mpsc::channel(64);
        let mid = MemberId::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(GroupMsg::AddMember {
            member_id: mid,
            name: "first".into(),
            sink: tx,
            reply: reply_tx,
        })
        .await
        .unwrap();
        let joined = reply_rx.await.unwrap();
        assert_eq!(joined.leader, mid);
        (h, rx, mid)
    }

    async fn add_member(h: &GroupHandle, name: &str) -> (MemberId, mpsc::Receiver<ServerMsg>) {
        let (tx, rx) = mpsc::channel(64);
        let mid = MemberId::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(GroupMsg::AddMember {
            member_id: mid,
            name: name.into(),
            sink: tx,
            reply: reply_tx,
        })
        .await
        .unwrap();
        let _ = reply_rx.await.unwrap();
        (mid, rx)
    }

    #[tokio::test]
    async fn first_member_becomes_leader() {
        let (h, _rx, mid) = fresh().await;
        let (tx, rx) = oneshot::channel();
        h.tx.send(GroupMsg::Snapshot { reply: tx }).await.unwrap();
        let snap = rx.await.unwrap();
        assert_eq!(snap.leader, Some(mid));
        assert_eq!(snap.member_count, 1);
    }

    #[tokio::test]
    async fn non_leader_play_returns_not_leader_error() {
        let (h, _rx_leader, _leader) = fresh().await;
        let (other_mid, mut other_rx) = add_member(&h, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: other_mid,
            position_ms: 0,
        })
        .await
        .unwrap();
        let msg = other_rx.recv().await.unwrap();
        match msg {
            ServerMsg::Error { code, .. } => assert_eq!(code, ErrorCode::NotLeader),
            other => panic!("expected Error/NotLeader, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn leader_play_broadcasts_to_all_members() {
        let (h, mut leader_rx, leader) = fresh().await;
        let (_other, mut other_rx) = add_member(&h, "second").await;
        // Drain MemberJoined sent to leader.
        let _ = leader_rx.recv().await.unwrap();

        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 5000,
        })
        .await
        .unwrap();

        let m1 = leader_rx.recv().await.unwrap();
        let m2 = other_rx.recv().await.unwrap();
        let check = |m: ServerMsg| match m {
            ServerMsg::Play {
                at_server_ms,
                position_ms,
            } => {
                assert_eq!(position_ms, 5000);
                assert!(at_server_ms >= MIN_LEAD_MS);
            }
            other => panic!("expected Play, got {other:?}"),
        };
        check(m1);
        check(m2);
    }

    #[tokio::test]
    async fn leader_handoff_on_leader_remove() {
        let (h, _leader_rx, leader) = fresh().await;
        let (m2_id, mut m2_rx) = add_member(&h, "b").await;
        let (m3_id, mut m3_rx) = add_member(&h, "c").await;
        h.tx.send(GroupMsg::RemoveMember { member_id: leader })
            .await
            .unwrap();
        // m2 and m3 should both see exactly one LeaderChange + one MemberLeft
        // before any other messages. Order may interleave with their earlier
        // MemberJoined notifications; drain until we find LeaderChange.
        let new_leader = std::cmp::min(m2_id, m3_id);
        let mut seen_leader_change = false;
        for _ in 0..10 {
            tokio::select! {
                Some(m) = m2_rx.recv() => {
                    if let ServerMsg::LeaderChange { leader: l } = m {
                        assert_eq!(l, new_leader);
                        seen_leader_change = true;
                        break;
                    }
                }
                Some(m) = m3_rx.recv() => {
                    if let ServerMsg::LeaderChange { leader: l } = m {
                        assert_eq!(l, new_leader);
                        seen_leader_change = true;
                        break;
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => break,
            }
        }
        assert!(seen_leader_change, "no LeaderChange observed");
    }

    #[tokio::test]
    async fn buffering_pauses_group_only_once() {
        // V19: a single broadcast of Pause across the group; subsequent
        // BufferingStart from another member does NOT trigger a second
        // broadcast. Each broadcast = one Pause per member sink.
        let (h, mut leader_rx, _leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, "b").await;
        let (m3, mut m3_rx) = add_member(&h, "c").await;

        // Drain MemberJoined notifications.
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}
        while m3_rx.try_recv().is_ok() {}

        // First buffering report → exactly one Pause per member (3 total).
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 0,
        })
        .await
        .unwrap();
        assert!(matches!(
            leader_rx.recv().await.unwrap(),
            ServerMsg::Pause { .. }
        ));
        assert!(matches!(
            m2_rx.recv().await.unwrap(),
            ServerMsg::Pause { .. }
        ));
        assert!(matches!(
            m3_rx.recv().await.unwrap(),
            ServerMsg::Pause { .. }
        ));

        // Second buffering report from a different member while group is
        // already paused → no broadcast, no Pause on any sink.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m3,
            position_ms: 0,
        })
        .await
        .unwrap();
        let mut extra_pauses = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                Some(m) = leader_rx.recv() => if matches!(m, ServerMsg::Pause { .. }) { extra_pauses += 1 },
                Some(m) = m2_rx.recv() => if matches!(m, ServerMsg::Pause { .. }) { extra_pauses += 1 },
                Some(m) = m3_rx.recv() => if matches!(m, ServerMsg::Pause { .. }) { extra_pauses += 1 },
            }
        }
        assert_eq!(extra_pauses, 0, "buffer storm: extra Pauses observed");
    }

    #[tokio::test]
    async fn late_joiner_during_buffer_pause_gets_pause_not_play() {
        // V19: while the group is paused waiting on a buffering member, a
        // freshly-joined member must NOT be told to Play (which would
        // desync it from the paused cohort). The buffering pause freezes
        // playback state, so the late joiner's catch-up yields Seek+Pause.
        let (h, mut leader_rx, leader) = fresh().await;
        let (m2, mut _m2_rx) = add_member(&h, "b").await;
        while leader_rx.try_recv().is_ok() {}

        // Leader is playing.
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 10_000,
        })
        .await
        .unwrap();
        // m2 buffers → group pauses + freezes.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 0,
        })
        .await
        .unwrap();

        // Late joiner during the buffer-pause window.
        let (_late, mut late_rx) = add_member(&h, "late").await;
        // Collect a few messages; must include Pause and must NOT include Play.
        let mut saw_pause = false;
        let mut saw_play = false;
        for _ in 0..6 {
            match tokio::time::timeout(Duration::from_millis(100), late_rx.recv()).await {
                Ok(Some(ServerMsg::Pause { .. })) => saw_pause = true,
                Ok(Some(ServerMsg::Play { .. })) => saw_play = true,
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        assert!(saw_pause, "late joiner should receive Pause");
        assert!(
            !saw_play,
            "late joiner must NOT receive Play during buffer pause"
        );
    }

    #[tokio::test]
    async fn observe_clock_extends_lead_time_for_high_rtt() {
        let (h, mut leader_rx, leader) = fresh().await;
        let (_m2, _m2_rx) = add_member(&h, "b").await;
        // Drain.
        while leader_rx.try_recv().is_ok() {}

        // Inject a sample with RTT = 1000ms.
        h.tx.send(GroupMsg::ObserveClock {
            member_id: leader,
            t1: 0,
            t2: 50,
            t3: 60,
            t4: 1010, // T4 - T1 = 1010 ; T3 - T2 = 10 ; rtt = 1000
        })
        .await
        .unwrap();

        // Snapshot to ensure the observe was processed.
        let (tx, rx) = oneshot::channel();
        h.tx.send(GroupMsg::Snapshot { reply: tx }).await.unwrap();
        let _ = rx.await.unwrap();

        // Capture server_ms at time of issue.
        let before = std::time::Instant::now();
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 0,
        })
        .await
        .unwrap();
        let msg = leader_rx.recv().await.unwrap();
        let elapsed_since_send = before.elapsed().as_millis() as u64;

        match msg {
            ServerMsg::Play { at_server_ms, .. } => {
                // Lead time = MIN_LEAD_MS + max_rtt/2 = 200 + 500 = 700.
                // at_server_ms is measured from group start; we can't
                // pin it exactly but it must be at least 700 ms ahead
                // of "now-on-server", which translates to: at_server_ms
                // should exceed elapsed_since_send + 700 - small slack.
                assert!(
                    at_server_ms >= 700 - 50,
                    "at_server_ms={at_server_ms} elapsed_since_send={elapsed_since_send}; expected >= 650"
                );
            }
            other => panic!("expected Play, got {other:?}"),
        }
    }
}
