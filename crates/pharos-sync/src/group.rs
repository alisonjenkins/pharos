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
use super::messages::{
    ErrorCode, GroupId, GroupPlayState, MemberId, MemberSummary, QueueItemInfo, ServerMsg,
};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// Minimum delay between "server decides to play" and `at_server_ms`.
/// Each member subtracts its own offset to schedule locally — see
/// `docs/group-sync-protocol.md` §4.
pub const MIN_LEAD_MS: u64 = 200;

/// How long the readiness gate waits for every member to report `Ready`
/// before starting anyway. Bounds the WAITING state so a silent or wedged
/// client can never block the whole group forever (the failure mode behind
/// jellyfin#8140 / #5619). A client that is genuinely still buffering when
/// this fires will re-sync via its own drift correction.
pub const READY_TIMEOUT_MS: u64 = 5_000;

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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
    /// Jellyfin HTTP `/SyncPlay/Unpause`. Unlike the native [`LeaderPlay`]
    /// (which broadcasts immediately), this enters the readiness gate: the
    /// group starts only once every member has reported `Ready` (or the
    /// timeout fires), so a slow/transcoding client doesn't start late.
    ///
    /// [`LeaderPlay`]: GroupMsg::LeaderPlay
    Unpause {
        sender: MemberId,
    },
    /// Jellyfin HTTP `/SyncPlay/Pause` — SHARED control: any member may pause
    /// (Jellyfin's default group mode), unlike the leader-gated native
    /// [`LeaderPause`](GroupMsg::LeaderPause).
    PauseShared {
        sender: MemberId,
    },
    /// Jellyfin HTTP `/SyncPlay/Seek` — gated seek (re-buffer then resume).
    SeekTo {
        sender: MemberId,
        position_ms: u64,
    },
    /// Jellyfin HTTP `/SyncPlay/Ready` — this member has buffered the current
    /// item and is ready to start. Clears the member from the readiness gate.
    MemberReady {
        member_id: MemberId,
        position_ms: u64,
    },
    /// Jellyfin HTTP `/SyncPlay/SetNewQueue` — replace the playlist and start
    /// (leader only). `item_ids` are library item ids; the server assigns a
    /// `playlist_item_id` per entry.
    SetNewQueue {
        sender: MemberId,
        item_ids: Vec<String>,
        playing_index: usize,
        start_position_ms: u64,
    },
    /// Jump to a specific queue entry by its `playlist_item_id` (leader only).
    SetPlaylistItem {
        sender: MemberId,
        playlist_item_id: String,
    },
    /// Advance to the next / previous queue entry (leader only).
    NextItem {
        sender: MemberId,
    },
    PreviousItem {
        sender: MemberId,
    },
    /// Set repeat / shuffle mode (leader only). `mode` is the Jellyfin string
    /// (`RepeatNone|RepeatOne|RepeatAll`, `Sorted|Shuffle`).
    SetRepeatMode {
        sender: MemberId,
        mode: String,
    },
    SetShuffleMode {
        sender: MemberId,
        mode: String,
    },
    /// Set the group's display name (from the `/SyncPlay/New` request). Any
    /// member may set it — jellyfin-web only sends it at creation.
    SetGroupName {
        name: String,
    },
    /// Refresh a member's sink after its `/socket` reconnected (the member
    /// itself persists across socket churn). Re-sends the catch-up so the
    /// reconnected client immediately re-syncs to the group's current state.
    UpdateSink {
        member_id: MemberId,
        sink: mpsc::Sender<ServerMsg>,
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
    /// Coarse playback state for the `/SyncPlay/List` `GroupInfoDto`.
    pub play_state: GroupPlayState,
    /// Human-readable group name (what the creator's client sent on `New`).
    pub group_name: String,
    /// Member display names — the `Participants` the join dialog renders.
    pub participants: Vec<String>,
}

struct MemberRec {
    name: String,
    sink: mpsc::Sender<ServerMsg>,
    offset: ClockOffset,
    buffering: bool,
}

/// One entry in the group's play queue.
struct QueueEntry {
    item_id: String,
    playlist_item_id: String,
}

/// The group's play queue (playlist + cursor + modes).
#[derive(Default)]
struct PlayQueue {
    items: Vec<QueueEntry>,
    playing_index: usize,
    repeat_mode: String,
    shuffle_mode: String,
}

impl PlayQueue {
    fn item_infos(&self) -> Vec<QueueItemInfo> {
        self.items
            .iter()
            .map(|e| QueueItemInfo {
                item_id: e.item_id.clone(),
                playlist_item_id: e.playlist_item_id.clone(),
            })
            .collect()
    }
}

/// The readiness gate: while `Some`, the group is in `Waiting` and will not
/// broadcast the pending `Play`/`Pause` until every member in `pending` has
/// reported `Ready` (or `deadline` fires — the anti-wedge timeout).
struct WaitingGate {
    pending: HashSet<MemberId>,
    /// Whether the group should be `Playing` (true) or `Paused` (false) once
    /// the gate resolves — e.g. a seek while paused resolves to paused.
    resume_playing: bool,
    position_ms: u64,
    deadline: tokio::time::Instant,
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
    /// Wall-clock (unix ms) time base. `server_ms` is measured as
    /// `unix_now_ms() - epoch_unix_ms`, so ANY replica derives the same
    /// monotonic-looking clock from the same epoch — the property that lets a
    /// group's actor migrate to another replica after a deploy without shifting
    /// already-scheduled `at_server_ms` instants. (Replica wall clocks are
    /// NTP-synced; on the single-node cluster they are the same clock.)
    epoch_unix_ms: u64,
    members: HashMap<MemberId, MemberRec>,
    leader: Option<MemberId>,
    group_paused_due_to_buffering: bool,
    playback: PlaybackState,
    queue: PlayQueue,
    /// `Some` while the readiness gate is open (group is `Waiting`).
    waiting: Option<WaitingGate>,
    /// Display name shown in the join dialog (set from `/SyncPlay/New`).
    group_name: String,
}

impl GroupState {
    fn new(id: GroupId, epoch_unix_ms: u64) -> Self {
        Self {
            id,
            epoch_unix_ms,
            members: HashMap::new(),
            leader: None,
            group_paused_due_to_buffering: false,
            playback: PlaybackState::Idle,
            queue: PlayQueue::default(),
            waiting: None,
            group_name: "Watch Party".to_string(),
        }
    }

    fn server_ms_now(&self) -> u64 {
        unix_now_ms().saturating_sub(self.epoch_unix_ms)
    }

    /// Coarse playback state for snapshots / `StateUpdate`.
    fn play_state(&self) -> GroupPlayState {
        if self.waiting.is_some() {
            return GroupPlayState::Waiting;
        }
        match self.playback {
            PlaybackState::Idle => GroupPlayState::Idle,
            PlaybackState::Playing { .. } => GroupPlayState::Playing,
            PlaybackState::Paused { .. } => GroupPlayState::Paused,
        }
    }

    fn current_position_ms(&self) -> u64 {
        match self.playback {
            PlaybackState::Idle => 0,
            PlaybackState::Paused { position_ms } => position_ms,
            PlaybackState::Playing {
                position_ms,
                anchor_server_ms,
            } => position_ms + self.server_ms_now().saturating_sub(anchor_server_ms),
        }
    }

    /// Send one member the current queue + playback state so it (re)syncs to the
    /// group — used both for a fresh join and for a socket reconnect. Never
    /// mutates group state (esp. never advances `playing_index`).
    fn send_catch_up(&self, member_id: MemberId) {
        if !self.queue.items.is_empty() {
            self.send_one(
                member_id,
                ServerMsg::PlayQueue {
                    reason: "user_joined".into(),
                    items: self.queue.item_infos(),
                    playing_index: self.queue.playing_index,
                    start_position_ms: self.current_position_ms(),
                    is_playing: matches!(self.playback, PlaybackState::Playing { .. }),
                    repeat_mode: self.queue.repeat_mode.clone(),
                    shuffle_mode: self.queue.shuffle_mode.clone(),
                },
            );
        }
        self.send_playback_state(member_id);
    }

    /// Send just the live playback command (Play, or Seek+Pause) to a single
    /// member — the current position/state WITHOUT the `PlayQueue`. Used to
    /// resume a member that already loaded the current item but missed the
    /// group's live Play/Pause (see the late-`Ready` path in `MemberReady`).
    /// Re-sending the `PlayQueue` here would make jellyfin-web reload the
    /// player from scratch, so this deliberately omits it.
    fn send_playback_state(&self, member_id: MemberId) {
        let server_ms = self.server_ms_now();
        match self.playback {
            PlaybackState::Idle => {}
            PlaybackState::Playing {
                position_ms,
                anchor_server_ms,
            } => {
                let elapsed = server_ms.saturating_sub(anchor_server_ms);
                self.send_one(
                    member_id,
                    ServerMsg::Play {
                        at_server_ms: server_ms + MIN_LEAD_MS,
                        position_ms: position_ms + elapsed,
                    },
                );
            }
            PlaybackState::Paused { position_ms } => {
                self.send_one(
                    member_id,
                    ServerMsg::Seek {
                        at_server_ms: server_ms + MIN_LEAD_MS,
                        position_ms,
                    },
                );
                self.send_one(
                    member_id,
                    ServerMsg::Pause {
                        at_server_ms: server_ms + MIN_LEAD_MS,
                    },
                );
            }
        }
    }

    /// Broadcast the current queue to every member (Jellyfin `PlayQueue`).
    fn broadcast_play_queue(&self, reason: &str, is_playing: bool, start_position_ms: u64) {
        self.broadcast(ServerMsg::PlayQueue {
            reason: reason.to_string(),
            items: self.queue.item_infos(),
            playing_index: self.queue.playing_index,
            start_position_ms,
            is_playing,
            repeat_mode: self.queue.repeat_mode.clone(),
            shuffle_mode: self.queue.shuffle_mode.clone(),
        });
    }

    /// Open the readiness gate: enter `Waiting`, await every member's `Ready`.
    /// The group starts (or re-pauses) only once the gate resolves.
    fn enter_waiting(&mut self, resume_playing: bool, position_ms: u64, reason: &str) {
        let pending: HashSet<MemberId> = self.members.keys().copied().collect();
        // An empty group can't resolve a gate; nothing to wait on.
        if pending.is_empty() {
            return;
        }
        self.waiting = Some(WaitingGate {
            pending,
            resume_playing,
            position_ms,
            deadline: tokio::time::Instant::now()
                + std::time::Duration::from_millis(READY_TIMEOUT_MS),
        });
        self.broadcast(ServerMsg::StateUpdate {
            state: GroupPlayState::Waiting,
            reason: reason.to_string(),
        });
    }

    /// Resolve the readiness gate: schedule the pending `Play`/`Pause` for a
    /// common future instant and broadcast it. Called when the last member
    /// reports `Ready`, or when the anti-wedge timeout fires.
    fn resolve_waiting(&mut self) {
        let Some(w) = self.waiting.take() else {
            return;
        };
        let server_ms = self.server_ms_now();
        let at_server_ms = server_ms + self.lead_time_ms();
        if w.resume_playing {
            self.playback = PlaybackState::Playing {
                position_ms: w.position_ms,
                anchor_server_ms: server_ms,
            };
            self.broadcast(ServerMsg::Play {
                at_server_ms,
                position_ms: w.position_ms,
            });
            self.broadcast(ServerMsg::StateUpdate {
                state: GroupPlayState::Playing,
                reason: "ready".into(),
            });
        } else {
            self.playback = PlaybackState::Paused {
                position_ms: w.position_ms,
            };
            self.broadcast(ServerMsg::Seek {
                at_server_ms,
                position_ms: w.position_ms,
            });
            self.broadcast(ServerMsg::StateUpdate {
                state: GroupPlayState::Paused,
                reason: "ready".into(),
            });
        }
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
    /// Wall-clock (unix ms) time base captured at spawn. It IS the actor's
    /// `server_ms` origin (`server_ms = unix_now_ms() - epoch_unix_ms`), so a
    /// member's socket adds a `ServerMsg`'s `at_server_ms` to this to produce
    /// the absolute UTC `When` the Jellyfin client schedules against. Exposed on
    /// the handle so the HTTP layer can stash it in the session hub *before*
    /// `AddMember` triggers any late-joiner catch-up broadcast (else the first
    /// command carries `When = 0`). Persisted with the group snapshot so a new
    /// owner after a deploy reuses the same origin.
    pub epoch_unix_ms: u64,
}

impl GroupHandle {
    /// Spawn a fresh group actor.
    pub fn spawn(group_id: GroupId) -> Self {
        let (tx, mut rx) = mpsc::channel::<GroupMsg>(256);
        let epoch_unix_ms = unix_now_ms();
        let mut state = GroupState::new(group_id, epoch_unix_ms);
        tokio::spawn(async move {
            // A brand-new group has no members yet — it must NOT terminate on
            // the empty check before its creator's AddMember lands (else a New
            // that sends anything first, e.g. SetGroupName, kills the group
            // before anyone joins). Only terminate once it has HAD a member and
            // then lost the last one.
            let mut ever_joined = false;
            loop {
                // Arm the readiness-gate timeout only while waiting, so a
                // silent/wedged member can never block the group forever.
                let deadline = state.waiting.as_ref().map(|w| w.deadline);
                tokio::select! {
                    maybe = rx.recv() => {
                        let Some(msg) = maybe else { break };
                        handle(&mut state, msg).await;
                    }
                    _ = async {
                        // `deadline` is Some in this arm (guarded below).
                        tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)).await
                    }, if deadline.is_some() => {
                        // Timeout: drop still-pending members from the gate and
                        // start anyway (anti-wedge). They re-sync via their own
                        // drift correction.
                        state.resolve_waiting();
                    }
                }
                ever_joined |= !state.members.is_empty();
                if ever_joined && state.members.is_empty() {
                    // Had members, now empty → terminate. Registry respawns on
                    // the next Join.
                    break;
                }
            }
        });
        Self {
            tx,
            group_id,
            epoch_unix_ms,
        }
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
            // Notify the joiner over its own sink too (not just the oneshot
            // reply), so the HTTP-driven join path — where the reply goes to the
            // HTTP handler, not the socket — still delivers `GroupJoined` to the
            // client. The WS path's socket receives the same message.
            state.send_one(
                member_id,
                ServerMsg::Joined {
                    group_id: state.id,
                    leader,
                    members: summaries.clone(),
                },
            );
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
            // Queue + playback catch-up so the new member loads the SAME item at
            // the group's current position. Adding a member NEVER mutates
            // `playing_index` (A6: a join must not advance the group).
            state.send_catch_up(member_id);
        }
        GroupMsg::UpdateSink { member_id, sink } => {
            // A reconnected socket for an existing member: swap in the fresh
            // sink and re-send the catch-up so it immediately re-syncs. The
            // member (and its place in any readiness gate) is untouched.
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.sink = sink;
                state.send_catch_up(member_id);
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
            // A departing member must not wedge the readiness gate: drop it
            // from the pending set and resolve if it was the last holdout (and
            // members remain — an empty group terminates the actor anyway).
            if let Some(w) = state.waiting.as_mut() {
                w.pending.remove(&member_id);
                if w.pending.is_empty() && !state.members.is_empty() {
                    state.resolve_waiting();
                }
            }
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
        GroupMsg::Unpause { sender: _ } => {
            let position_ms = state.current_position_ms();
            state.enter_waiting(true, position_ms, "unpause");
        }
        GroupMsg::PauseShared { sender: _ } => {
            // Immediate group pause (no readiness gate). Freeze the position so
            // a late joiner gets the correct still-frame, then broadcast.
            let server_ms = state.server_ms_now();
            let at_server_ms = server_ms + state.lead_time_ms();
            // Cancel any pending readiness gate — we're pausing, not starting.
            state.waiting = None;
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
            state.broadcast(ServerMsg::StateUpdate {
                state: GroupPlayState::Paused,
                reason: "pause".into(),
            });
        }
        GroupMsg::SeekTo {
            sender: _,
            position_ms,
        } => {
            // Preserve play/pause across a seek: a seek while playing resumes
            // playing (after re-buffer); while paused/idle it stays paused.
            let resume = matches!(state.playback, PlaybackState::Playing { .. });
            state.enter_waiting(resume, position_ms, "seek");
        }
        GroupMsg::MemberReady {
            member_id,
            position_ms: _,
        } => {
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.buffering = false;
            }
            let resolved = if let Some(w) = state.waiting.as_mut() {
                w.pending.remove(&member_id);
                w.pending.is_empty()
            } else {
                // No waiting gate: the group already resolved (often because
                // the ready-timeout fired before THIS member's player finished
                // loading, so it dropped the broadcast Unpause — "no active
                // player"). Heal it: replay the live playback state to just
                // this member so a slow-to-start client still catches up
                // instead of being stranded paused while everyone else plays.
                state.send_playback_state(member_id);
                false
            };
            if resolved {
                state.resolve_waiting();
            }
        }
        GroupMsg::SetNewQueue {
            sender: _,
            item_ids,
            playing_index,
            start_position_ms,
        } => {
            state.queue.items = item_ids
                .into_iter()
                .map(|item_id| QueueEntry {
                    item_id,
                    playlist_item_id: Uuid::new_v4().simple().to_string(),
                })
                .collect();
            state.queue.playing_index =
                playing_index.min(state.queue.items.len().saturating_sub(1));
            state.broadcast_play_queue("new_playlist", true, start_position_ms);
            state.enter_waiting(true, start_position_ms, "new_playlist");
        }
        GroupMsg::SetPlaylistItem {
            sender: _,
            playlist_item_id,
        } => {
            if let Some(idx) = state
                .queue
                .items
                .iter()
                .position(|e| e.playlist_item_id == playlist_item_id)
            {
                state.queue.playing_index = idx;
                state.broadcast_play_queue("set_current_item", true, 0);
                state.enter_waiting(true, 0, "set_current_item");
            }
        }
        GroupMsg::NextItem { sender: _ } => {
            if state.queue.playing_index + 1 < state.queue.items.len() {
                state.queue.playing_index += 1;
                state.broadcast_play_queue("next_item", true, 0);
                state.enter_waiting(true, 0, "next_item");
            }
        }
        GroupMsg::PreviousItem { sender: _ } => {
            if state.queue.playing_index > 0 {
                state.queue.playing_index -= 1;
                state.broadcast_play_queue("previous_item", true, 0);
                state.enter_waiting(true, 0, "previous_item");
            }
        }
        GroupMsg::SetRepeatMode { sender: _, mode } => {
            state.queue.repeat_mode = mode;
            state.broadcast_play_queue("repeat_mode", false, state.current_position_ms());
        }
        GroupMsg::SetShuffleMode { sender: _, mode } => {
            state.queue.shuffle_mode = mode;
            state.broadcast_play_queue("shuffle_mode", false, state.current_position_ms());
        }
        GroupMsg::SetGroupName { name } => {
            if !name.trim().is_empty() {
                state.group_name = name;
            }
        }
        GroupMsg::Snapshot { reply } => {
            let snap = GroupSnapshot {
                id: state.id,
                leader: state.leader,
                member_count: state.members.len(),
                buffering_member_count: state.members.values().filter(|m| m.buffering).count(),
                play_state: state.play_state(),
                group_name: state.group_name.clone(),
                participants: state
                    .member_summaries()
                    .into_iter()
                    .map(|s| s.name)
                    .collect(),
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
        // The engine also sends `Joined` to the member's own sink (so the
        // HTTP-driven join delivers GroupJoined) — drain it so tests see the
        // same post-join stream they did before that was added.
        let mut rx = rx;
        assert!(matches!(rx.recv().await, Some(ServerMsg::Joined { .. })));
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
        // Drain the self-`Joined` (see `fresh`).
        let mut rx = rx;
        assert!(matches!(rx.recv().await, Some(ServerMsg::Joined { .. })));
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
    async fn fresh_group_survives_a_message_before_its_first_member() {
        // Regression: a New that sends SetGroupName before AddMember must not
        // kill the member-less group (the actor's empty-check used to terminate
        // it before the creator ever joined → "can't create a group").
        let h = GroupHandle::spawn(GroupId::new());
        h.tx.send(GroupMsg::SetGroupName {
            name: "Movie Night".into(),
        })
        .await
        .unwrap();
        // The actor must still be alive to accept the creator.
        let (tx, rx) = mpsc::channel(8);
        let mid = MemberId::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(GroupMsg::AddMember {
            member_id: mid,
            name: "ali".into(),
            sink: tx,
            reply: reply_tx,
        })
        .await
        .expect("actor must still be alive after a pre-member message");
        let joined = reply_rx.await.expect("AddMember must complete");
        assert_eq!(joined.leader, mid);
        drop(rx);
    }

    #[tokio::test]
    async fn snapshot_reports_group_name_and_member_names() {
        // The join dialog renders GroupName + Participants from the snapshot —
        // these must be the real name + usernames, not the group id / member-N.
        let (h, _rx, _mid) = fresh().await;
        let _ = add_member(&h, "gf").await;
        h.tx.send(GroupMsg::SetGroupName {
            name: "Movie Night".into(),
        })
        .await
        .unwrap();
        let (tx, rx) = oneshot::channel();
        h.tx.send(GroupMsg::Snapshot { reply: tx }).await.unwrap();
        let snap = rx.await.unwrap();
        assert_eq!(snap.group_name, "Movie Night");
        let mut names = snap.participants.clone();
        names.sort();
        assert_eq!(names, vec!["first".to_string(), "gf".to_string()]);
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
    async fn late_ready_after_group_playing_heals_the_member() {
        // Regression: a member whose player finishes loading AFTER the group
        // already started playing (e.g. its transcode start outran the
        // ready-timeout, so it dropped the broadcast Unpause with "no active
        // player") must still be told to play when it finally reports Ready —
        // not left stranded paused while everyone else watches.
        let (h, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, "slow").await;
        // Drain the MemberJoined the leader receives for m2.
        let _ = leader_rx.recv().await.unwrap();

        // Group starts playing (LeaderPlay broadcasts directly — no gate left).
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 5000,
        })
        .await
        .unwrap();
        // Drain the broadcast Play both members receive.
        let _ = leader_rx.recv().await.unwrap();
        let _ = m2_rx.recv().await.unwrap();

        // m2 reports Ready only now — after the group is already Playing and
        // there is no waiting gate.
        h.tx.send(GroupMsg::MemberReady {
            member_id: m2,
            position_ms: 0,
        })
        .await
        .unwrap();

        // It must be healed with a fresh Play at (at least) the live position.
        match m2_rx.recv().await.unwrap() {
            ServerMsg::Play { position_ms, .. } => assert!(
                position_ms >= 5000,
                "heal Play should resume at the live position, got {position_ms}"
            ),
            other => panic!("expected a heal Play for the late-ready member, got {other:?}"),
        }
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
