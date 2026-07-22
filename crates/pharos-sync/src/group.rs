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
use super::delivery::Delivery;
use super::messages::{
    ErrorCode, GroupId, GroupPlayState, MemberId, MemberSummary, QueueItemInfo, ServerMsg,
};
use super::persistence::GroupPersistence;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// Minimum delay between "server decides to play" and `at_server_ms`.
/// Each member subtracts its own offset to schedule locally — see
/// `docs/group-sync-protocol.md` §4.
pub const MIN_LEAD_MS: u64 = 200;

/// B38 — upper bound on the RTT-derived part of the scheduling lead.
/// `lead_time_ms` = MIN_LEAD_MS + min(max_rtt/2, this). Belt-and-braces with
/// the clock-sample discard (`clock::MAX_SAMPLE_RTT_MS`): even if bad samples
/// slip in, no command is ever scheduled more than ~2.2s out.
pub const MAX_HALF_RTT_LEAD_MS: u64 = 2_000;

/// How long the readiness gate waits for every member to report `Ready`
/// before starting anyway. Bounds the WAITING state so a silent or wedged
/// client can never block the whole group forever (the failure mode behind
/// jellyfin#8140 / #5619). A client that is genuinely still buffering when
/// this fires will re-sync via its own drift correction.
///
/// 30s, not 5s: a member joining mid-playback must hydrate the whole play
/// queue (jellyfin-web fetches every item's metadata — a full season is
/// hundreds of requests) AND buffer the first segment before its player fires
/// `playbackstart` and posts `Ready`. At 5s the anti-wedge fired first, the
/// group's `Unpause` reached the slow joiner before its player was active (so
/// it was dropped — "no active player"), and the joiner was stranded on a
/// spinner. A late `Ready` still heals via [`GroupState::send_playback_state`],
/// but the wider window lets the common case resolve cleanly with everyone in.
pub const READY_TIMEOUT_MS: u64 = 30_000;

/// B55 — anti-wedge bound on the V19 buffering freeze. One member's
/// `BufferingStart` pauses the whole group; if that member then buffers
/// forever, or vanishes mid-buffer without a matching `BufferingEnd` (socket
/// drop, tab close, crash), the group would stay frozen indefinitely. When the
/// freeze has stood this long the actor force-clears every buffering flag and
/// resumes; a member genuinely still buffering re-syncs via its own drift
/// correction. Matches `READY_TIMEOUT_MS` — the buffering client's slow path
/// is the same order of magnitude as a slow joiner's.
pub const BUFFERING_MAX_MS: u64 = 30_000;

/// T83 — how long a member may stay SILENT (no socket KeepAlive, no
/// `/SyncPlay/Ping`, no command) before the group prunes it as a ghost.
/// jellyfin-web KeepAlives every ~30s, so a live client refreshes several
/// times per TTL; a hydrated post-restart roster entry whose device never
/// reconnects exceeds it and is removed instead of wedging every readiness
/// gate to the anti-wedge timeout forever.
pub const MEMBER_TTL_MS: u64 = 150_000;

/// How often the actor sweeps for TTL-expired members.
const MEMBER_PRUNE_TICK_MS: u64 = 30_000;

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum GroupMsg {
    /// Register a member in the group's roster. The member's socket sink is
    /// held by the per-replica `MemberSinks` (registered by the caller before
    /// this message), not the actor — so no sink travels here, and the same
    /// message re-adds a member that reconnected onto a different replica.
    AddMember {
        member_id: MemberId,
        name: String,
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
        /// The queue entry the client is buffering (jellyfin-web sends it on
        /// every Buffering/Ready POST). `None` = legacy/native path.
        playlist_item_id: Option<String>,
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
        /// The queue entry this Ready is FOR. A Ready for a stale entry (the
        /// old episode's teardown transition racing a queue change) must not
        /// satisfy the new item's readiness gate (B37) — real Jellyfin
        /// validates this id against the current item.
        playlist_item_id: Option<String>,
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
    /// `playlist_item_id` is the entry the CLIENT believes is playing —
    /// real Jellyfin no-ops the request when it doesn't match the current
    /// entry, which dedupes double-presses and racing Next from two members.
    NextItem {
        sender: MemberId,
        playlist_item_id: Option<String>,
    },
    PreviousItem {
        sender: MemberId,
        playlist_item_id: Option<String>,
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
    /// Jellyfin HTTP `/SyncPlay/SetIgnoreWait` — the client asks to be
    /// excluded from (or re-included in) group waits. jellyfin-web posts
    /// `true` when it halts its own playback (player never started within its
    /// 30s budget) and `false` when it re-follows group playback.
    SetIgnoreWait {
        member_id: MemberId,
        ignore: bool,
    },
    /// A member's `/socket` reconnected (its fresh sink is already swapped into
    /// the per-replica `MemberSinks`; the member persists across socket churn).
    /// Re-sends the catch-up so the reconnected client immediately re-syncs to
    /// the group's current state. Also the re-hydration path: a member landing
    /// on a new replica after a deploy resyncs through here.
    ResyncMember {
        member_id: MemberId,
    },
    /// Liveness beacon (T83): the member's `/socket` KeepAlive (every ~30s in
    /// jellyfin-web) and its periodic `/SyncPlay/Ping` both refresh the
    /// member's `last_seen`. A member silent past [`MEMBER_TTL_MS`] is a GHOST
    /// (typically a post-restart hydrated roster entry whose device never
    /// reconnected) and gets pruned — otherwise every readiness gate waits the
    /// full anti-wedge timeout on it, forever.
    MemberPing {
        member_id: MemberId,
    },
    Snapshot {
        reply: oneshot::Sender<GroupSnapshot>,
    },
}

/// The serializable subset of [`GroupMsg`] a non-owner replica forwards to the
/// owner over the bus (Phase B4.3d). It omits the reply-carrying variants
/// (`Snapshot`; `AddMember`'s reply is synthesized locally and the real
/// `Joined` flows back via delivery) — those never cross replicas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteCommand {
    AddMember {
        member_id: MemberId,
        name: String,
    },
    RemoveMember {
        member_id: MemberId,
    },
    Resync {
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
        #[serde(default)]
        playlist_item_id: Option<String>,
    },
    BufferingEnd {
        member_id: MemberId,
    },
    Unpause {
        sender: MemberId,
    },
    PauseShared {
        sender: MemberId,
    },
    SeekTo {
        sender: MemberId,
        position_ms: u64,
    },
    MemberReady {
        member_id: MemberId,
        position_ms: u64,
        #[serde(default)]
        playlist_item_id: Option<String>,
    },
    MemberPing {
        member_id: MemberId,
    },
    SetNewQueue {
        sender: MemberId,
        item_ids: Vec<String>,
        playing_index: usize,
        start_position_ms: u64,
    },
    SetPlaylistItem {
        sender: MemberId,
        playlist_item_id: String,
    },
    NextItem {
        sender: MemberId,
        #[serde(default)]
        playlist_item_id: Option<String>,
    },
    PreviousItem {
        sender: MemberId,
        #[serde(default)]
        playlist_item_id: Option<String>,
    },
    SetRepeatMode {
        sender: MemberId,
        mode: String,
    },
    SetShuffleMode {
        sender: MemberId,
        mode: String,
    },
    SetGroupName {
        name: String,
    },
    SetIgnoreWait {
        member_id: MemberId,
        ignore: bool,
    },
}

impl RemoteCommand {
    /// The `GroupMsg` an owner's actor applies for this forwarded command. For
    /// `AddMember` a throwaway reply channel is used — the caller on the remote
    /// replica already answered its own handler, and the real `Joined` reaches
    /// the member via delivery.
    pub fn into_group_msg(self) -> GroupMsg {
        match self {
            RemoteCommand::AddMember { member_id, name } => {
                let (reply, _rx) = oneshot::channel();
                GroupMsg::AddMember {
                    member_id,
                    name,
                    reply,
                }
            }
            RemoteCommand::RemoveMember { member_id } => GroupMsg::RemoveMember { member_id },
            RemoteCommand::Resync { member_id } => GroupMsg::ResyncMember { member_id },
            RemoteCommand::LeaderPlay {
                sender,
                position_ms,
            } => GroupMsg::LeaderPlay {
                sender,
                position_ms,
            },
            RemoteCommand::LeaderPause { sender } => GroupMsg::LeaderPause { sender },
            RemoteCommand::LeaderSeek {
                sender,
                position_ms,
            } => GroupMsg::LeaderSeek {
                sender,
                position_ms,
            },
            RemoteCommand::ObserveClock {
                member_id,
                t1,
                t2,
                t3,
                t4,
            } => GroupMsg::ObserveClock {
                member_id,
                t1,
                t2,
                t3,
                t4,
            },
            RemoteCommand::BufferingStart {
                member_id,
                position_ms,
                playlist_item_id,
            } => GroupMsg::BufferingStart {
                member_id,
                position_ms,
                playlist_item_id,
            },
            RemoteCommand::BufferingEnd { member_id } => GroupMsg::BufferingEnd { member_id },
            RemoteCommand::Unpause { sender } => GroupMsg::Unpause { sender },
            RemoteCommand::PauseShared { sender } => GroupMsg::PauseShared { sender },
            RemoteCommand::SeekTo {
                sender,
                position_ms,
            } => GroupMsg::SeekTo {
                sender,
                position_ms,
            },
            RemoteCommand::MemberReady {
                member_id,
                position_ms,
                playlist_item_id,
            } => GroupMsg::MemberReady {
                member_id,
                position_ms,
                playlist_item_id,
            },
            RemoteCommand::MemberPing { member_id } => GroupMsg::MemberPing { member_id },
            RemoteCommand::SetNewQueue {
                sender,
                item_ids,
                playing_index,
                start_position_ms,
            } => GroupMsg::SetNewQueue {
                sender,
                item_ids,
                playing_index,
                start_position_ms,
            },
            RemoteCommand::SetPlaylistItem {
                sender,
                playlist_item_id,
            } => GroupMsg::SetPlaylistItem {
                sender,
                playlist_item_id,
            },
            RemoteCommand::NextItem {
                sender,
                playlist_item_id,
            } => GroupMsg::NextItem {
                sender,
                playlist_item_id,
            },
            RemoteCommand::PreviousItem {
                sender,
                playlist_item_id,
            } => GroupMsg::PreviousItem {
                sender,
                playlist_item_id,
            },
            RemoteCommand::SetRepeatMode { sender, mode } => {
                GroupMsg::SetRepeatMode { sender, mode }
            }
            RemoteCommand::SetShuffleMode { sender, mode } => {
                GroupMsg::SetShuffleMode { sender, mode }
            }
            RemoteCommand::SetGroupName { name } => GroupMsg::SetGroupName { name },
            RemoteCommand::SetIgnoreWait { member_id, ignore } => {
                GroupMsg::SetIgnoreWait { member_id, ignore }
            }
        }
    }
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
    /// Wire item id of the queue entry currently playing (T87 — lets the
    /// HTTP seek handler prewarm that item's segments). None on an empty
    /// queue.
    pub current_item_id: Option<String>,
}

struct MemberRec {
    name: String,
    offset: ClockOffset,
    buffering: bool,
    /// Jellyfin `/SyncPlay/SetIgnoreWait` — the client halted its own playback
    /// (e.g. its player never started) and asked to be left out of group
    /// waits. Excluded from every readiness-gate pending set until it posts
    /// `IgnoreWait: false` (or a `Ready`, which implies it re-followed).
    ignore_wait: bool,
    /// The member's last sign of life (any attributed message — KeepAlive-
    /// driven `MemberPing`, clock reports, Ready, commands). `tokio::time::
    /// Instant` (monotonic, test-clock-aware), NOT wall clock. NOT persisted:
    /// a hydrating replica stamps "now", giving every roster entry a full
    /// [`MEMBER_TTL_MS`] to reconnect after a deploy.
    last_seen: tokio::time::Instant,
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
    /// Wall-clock (unix ms) of the last real change to this queue, monotonic.
    /// Stamped on every outbound `PlayQueue` as `last_update_unix_ms` so a
    /// catch-up re-send carries the SAME value the client already applied and
    /// its `LastUpdate <=` staleness guard drops the duplicate.
    updated_unix_ms: u64,
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
    /// B38 — server-clock instant (the broadcast command's `at_server_ms`)
    /// before which NO legitimate `Ready` can exist: clients execute the
    /// command AT that time, so an earlier Ready is a spurious player
    /// transition (e.g. the pause wiggle right after a Seek broadcast) and
    /// must not count toward the gate. 0 = no scheduled command (queue-change
    /// gates), accept immediately.
    not_before_server_ms: u64,
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
    /// When the V19 buffering freeze started (monotonic; NOT persisted). Arms
    /// the buffering anti-wedge deadline in the actor loop so a member that
    /// buffers forever — or vanishes mid-buffer without a `BufferingEnd` —
    /// can never freeze the group past `BUFFERING_MAX_MS`. `None` whenever the
    /// group is not frozen for buffering. (B55.)
    buffering_since: Option<tokio::time::Instant>,
    /// Intent captured when the V19 buffering freeze engages: was the group
    /// PLAYING at that moment (→ resume Playing when it lifts) or not. Without
    /// it, every freeze-recovery path force-resumed Playing — so a user pause
    /// during/around a member's buffer, or a track-change reload while already
    /// paused, played the group out from under the pause. Persisted so a
    /// mid-freeze replica takeover resumes to the right state.
    buffering_resume_playing: bool,
    playback: PlaybackState,
    queue: PlayQueue,
    /// `Some` while the readiness gate is open (group is `Waiting`).
    waiting: Option<WaitingGate>,
    /// Display name shown in the join dialog (set from `/SyncPlay/New`).
    group_name: String,
    /// How outbound `ServerMsg`s reach members. `LocalDelivery` on the
    /// single-replica path (straight to local sinks); `BusDelivery` under
    /// Postgres (publish to every replica). The actor never touches sinks.
    delivery: Arc<dyn Delivery>,
    /// Where the group snapshot is persisted after each mutation (Phase B4.3c).
    /// `None` on the single-replica / SQLite path (groups never leave the
    /// process); `Some` under Postgres so another replica can take over.
    persistence: Option<Arc<dyn GroupPersistence>>,
}

impl GroupState {
    fn new(
        id: GroupId,
        epoch_unix_ms: u64,
        delivery: Arc<dyn Delivery>,
        persistence: Option<Arc<dyn GroupPersistence>>,
    ) -> Self {
        Self {
            id,
            epoch_unix_ms,
            members: HashMap::new(),
            leader: None,
            group_paused_due_to_buffering: false,
            buffering_since: None,
            buffering_resume_playing: false,
            playback: PlaybackState::Idle,
            queue: PlayQueue::default(),
            waiting: None,
            group_name: "Watch Party".to_string(),
            delivery,
            persistence,
        }
    }

    /// Write the current snapshot to the persistence sink (fire-and-forget).
    /// No-op when persistence isn't wired (single-replica path). A serialize
    /// failure (not reachable for this plain data) simply skips the write; the
    /// next mutation re-attempts.
    fn persist(&self) {
        if let Some(p) = &self.persistence {
            if let Ok(json) = serde_json::to_string(&self.to_persist()) {
                p.persist(self.id, self.epoch_unix_ms, json);
            }
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
                    // Reuse the queue's current change-timestamp (do NOT bump) —
                    // this is the same queue the client may already have, so it
                    // must look NO newer or the client re-processes it.
                    last_update_unix_ms: self.queue.updated_unix_ms,
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
                // A single Pause suffices: jellyfin-web's schedulePause seeks
                // to the command's PositionTicks after pausing. (This also
                // survives the client's pre-time-sync queue, which keeps only
                // the LAST queued command.)
                self.send_one(
                    member_id,
                    ServerMsg::Pause {
                        at_server_ms: server_ms + MIN_LEAD_MS,
                        position_ms,
                    },
                );
            }
        }
    }

    /// Broadcast the current queue to every member (Jellyfin `PlayQueue`).
    /// This is the single funnel for a *real* queue change, so it bumps the
    /// queue's change-timestamp — kept strictly monotonic so two changes in the
    /// same wall-clock millisecond still each look newer to the client's
    /// `LastUpdate <=` staleness guard.
    fn broadcast_play_queue(&mut self, reason: &str, is_playing: bool, start_position_ms: u64) {
        self.queue.updated_unix_ms = unix_now_ms().max(self.queue.updated_unix_ms + 1);
        self.broadcast(ServerMsg::PlayQueue {
            reason: reason.to_string(),
            items: self.queue.item_infos(),
            playing_index: self.queue.playing_index,
            start_position_ms,
            is_playing,
            repeat_mode: self.queue.repeat_mode.clone(),
            shuffle_mode: self.queue.shuffle_mode.clone(),
            last_update_unix_ms: self.queue.updated_unix_ms,
        });
    }

    /// The `playlist_item_id` of the queue entry the group currently plays.
    fn current_playlist_item_id(&self) -> Option<&str> {
        self.queue
            .items
            .get(self.queue.playing_index)
            .map(|e| e.playlist_item_id.as_str())
    }

    /// B37 — a client-reported `PlaylistItemId` that names anything OTHER
    /// than the current queue entry is STALE: the client hasn't applied the
    /// latest queue change yet (or its old player raced a teardown
    /// transition). `None` (legacy / ws-native callers) is never stale.
    fn pli_is_stale(&self, pli: &Option<String>) -> bool {
        match (pli.as_deref(), self.current_playlist_item_id()) {
            (Some(sent), Some(current)) => sent != current,
            _ => false,
        }
    }

    /// Re-send the CURRENT play queue to one member (catch-up). Deliberately
    /// keeps `updated_unix_ms` unchanged: jellyfin-web applies a PlayQueue
    /// only when its LastUpdate is NEWER than what it has, so an up-to-date
    /// client drops this as a duplicate while a behind client (the one whose
    /// stale Ready triggered it) applies it and loads the right item.
    fn send_play_queue_to(&self, member_id: MemberId) {
        self.send_one(
            member_id,
            ServerMsg::PlayQueue {
                reason: "set_current_item".to_string(),
                items: self.queue.item_infos(),
                playing_index: self.queue.playing_index,
                start_position_ms: 0,
                is_playing: matches!(self.playback, PlaybackState::Playing { .. }),
                repeat_mode: self.queue.repeat_mode.clone(),
                shuffle_mode: self.queue.shuffle_mode.clone(),
                last_update_unix_ms: self.queue.updated_unix_ms,
            },
        );
    }

    /// The members a readiness gate may wait on: everyone not opted out via
    /// `SetIgnoreWait` (a halted client never posts `Ready`, so waiting on it
    /// only ever runs the gate into the anti-wedge timeout).
    fn follower_ids(&self) -> HashSet<MemberId> {
        self.members
            .iter()
            .filter(|(_, m)| !m.ignore_wait)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Drop every V19 buffering-freeze bookkeeping field. A deliberate pause, a
    /// seek, or a resume all supersede an in-flight freeze; this is the ONE
    /// place those flags are cleared so no path forgets one (a stray
    /// `buffering_since` re-arms the anti-wedge and phantom-resumes the group
    /// ~30s later). Does not touch `playback`.
    fn clear_buffering_freeze(&mut self) {
        self.group_paused_due_to_buffering = false;
        self.buffering_since = None;
        self.buffering_resume_playing = false;
    }

    /// Lift a V19 buffering freeze, resuming per the intent captured when it
    /// engaged: `Playing` if the group was playing (the normal mid-play stall),
    /// else settle `Paused` (a freeze that engaged around a paused group / a
    /// user paused during the freeze). Clears the freeze bookkeeping either way.
    /// Caller must ensure the freeze should lift (no buffering members, no open
    /// readiness gate).
    fn resume_after_buffering(&mut self) {
        let position_ms = self.current_position_ms();
        if self.buffering_resume_playing {
            self.start_playing(position_ms, "Ready");
        } else {
            self.playback = PlaybackState::Paused { position_ms };
            self.clear_buffering_freeze();
            self.broadcast(ServerMsg::StateUpdate {
                state: GroupPlayState::Paused,
                reason: "Ready".into(),
            });
        }
    }

    /// Freeze playback into `Paused` at the current live position and return
    /// it (Idle stays Idle at 0). The value every outbound `Pause` must carry —
    /// jellyfin-web's `schedulePause` seeks to the command's PositionTicks, so
    /// a missing position seeks the client to 0:00.
    fn freeze_paused_position(&mut self) -> u64 {
        let position_ms = self.current_position_ms();
        if !matches!(self.playback, PlaybackState::Idle) {
            self.playback = PlaybackState::Paused { position_ms };
        }
        position_ms
    }

    /// Start group playback NOW: anchor `Playing` and broadcast the scheduled
    /// `Play` + `StateUpdate`. `reason` must be one of jellyfin-web's OSD
    /// strings ('Unpause' / 'Ready' — matched case-sensitively).
    fn start_playing(&mut self, position_ms: u64, reason: &str) {
        let server_ms = self.server_ms_now();
        self.playback = PlaybackState::Playing {
            position_ms,
            anchor_server_ms: server_ms,
        };
        self.clear_buffering_freeze();
        self.broadcast(ServerMsg::Play {
            at_server_ms: server_ms + self.lead_time_ms(),
            position_ms,
        });
        self.broadcast(ServerMsg::StateUpdate {
            state: GroupPlayState::Playing,
            reason: reason.to_string(),
        });
    }

    /// Open the readiness gate: enter `Waiting` until every member in
    /// `pending` reports `Ready` (or the anti-wedge deadline fires), then
    /// start (or re-pause) the group. `pending` must only contain members
    /// that WILL post a `Ready` — i.e. players about to load/buffer something
    /// (a queue change or a just-broadcast Seek). jellyfin-web only posts
    /// `Ready` on a player transition, so gating on an idle paused player
    /// deadlocks until the timeout.
    fn enter_waiting(
        &mut self,
        pending: HashSet<MemberId>,
        resume_playing: bool,
        position_ms: u64,
        reason: &str,
        not_before_server_ms: u64,
    ) {
        // An empty group can't resolve a gate; nothing to wait on.
        if self.members.is_empty() {
            return;
        }
        tracing::info!(
            group = %self.id,
            pending = pending.len(),
            resume_playing,
            position_ms,
            reason,
            not_before_server_ms,
            lead_ms = self.lead_time_ms(),
            "syncplay: readiness gate opened"
        );
        self.waiting = Some(WaitingGate {
            pending,
            resume_playing,
            position_ms,
            not_before_server_ms,
            deadline: tokio::time::Instant::now()
                + std::time::Duration::from_millis(READY_TIMEOUT_MS),
        });
        // Nobody to wait on (e.g. every member opted out of waits): resolve
        // straight away — no Waiting broadcast, no timeout detour.
        if self.waiting.as_ref().is_some_and(|w| w.pending.is_empty()) {
            self.resolve_waiting();
            return;
        }
        self.broadcast(ServerMsg::StateUpdate {
            state: GroupPlayState::Waiting,
            reason: reason.to_string(),
        });
    }

    /// Resolve the readiness gate: schedule the pending `Play` (or settle
    /// `Paused`) and broadcast it. Called when the last member reports
    /// `Ready`, or when the anti-wedge timeout fires.
    fn resolve_waiting(&mut self) {
        let Some(w) = self.waiting.take() else {
            return;
        };
        tracing::info!(
            group = %self.id,
            resume_playing = w.resume_playing,
            position_ms = w.position_ms,
            "syncplay: readiness gate resolved"
        );
        if w.resume_playing {
            self.start_playing(w.position_ms, "Ready");
        } else {
            // The members already applied the Seek broadcast at gate entry and
            // sit paused at the position — re-sending it would re-trigger their
            // seek→Ready cycle. Just settle the group state.
            self.playback = PlaybackState::Paused {
                position_ms: w.position_ms,
            };
            self.broadcast(ServerMsg::StateUpdate {
                state: GroupPlayState::Paused,
                reason: "Ready".into(),
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
        // B38 — clamp: an unclamped lead let one member's pathological RTT
        // schedule every command tens of seconds into the future for the
        // WHOLE group (observed live: ~40s frozen seek). A member whose real
        // half-RTT exceeds the cap gets its command slightly in the past and
        // heals via drift correction — vastly better than freezing everyone.
        MIN_LEAD_MS + half_max_rtt.min(MAX_HALF_RTT_LEAD_MS)
    }

    /// V19: one slow / wedged member must not block the actor or delay
    /// broadcasts to everyone else. Delivery is fire-and-forget (a full sink
    /// drops; the member reconciles via the next state catch-up).
    ///
    /// Broadcasts are expanded over the actor's OWN roster (not the replica's
    /// sink table) so a socket that registered its sink before its `AddMember`
    /// was processed receives nothing until the actor admits it — see the
    /// delivery module docs.
    fn broadcast(&self, msg: ServerMsg) {
        for mid in self.members.keys() {
            self.delivery.deliver(*mid, msg.clone());
        }
    }

    /// Broadcast to everyone except one member (the "someone joined"
    /// notification the joiner itself must not receive).
    fn broadcast_except(&self, except: MemberId, msg: ServerMsg) {
        for mid in self.members.keys() {
            if *mid != except {
                self.delivery.deliver(*mid, msg.clone());
            }
        }
    }

    fn send_one(&self, to: MemberId, msg: ServerMsg) {
        self.delivery.deliver(to, msg);
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

    /// Serialize the whole coordination state to the opaque blob the store
    /// holds (Phase B4.3c). Per-connection data that a re-hydrating replica
    /// re-derives is deliberately dropped: each member's clock offset (re-
    /// estimated from fresh NTP pings on reconnect) and buffering flag (reset —
    /// the client re-reports). The readiness-gate deadline (a `tokio::Instant`,
    /// neither serializable nor portable) is reconstructed on hydrate.
    fn to_persist(&self) -> PersistState {
        PersistState {
            members: self
                .members
                .iter()
                .map(|(id, m)| PersistMember {
                    id: *id,
                    name: m.name.clone(),
                    ignore_wait: m.ignore_wait,
                })
                .collect(),
            leader: self.leader,
            group_paused_due_to_buffering: self.group_paused_due_to_buffering,
            buffering_resume_playing: self.buffering_resume_playing,
            playback: match self.playback {
                PlaybackState::Idle => PersistPlayback::Idle,
                PlaybackState::Playing {
                    position_ms,
                    anchor_server_ms,
                } => PersistPlayback::Playing {
                    position_ms,
                    anchor_server_ms,
                },
                PlaybackState::Paused { position_ms } => PersistPlayback::Paused { position_ms },
            },
            queue_items: self
                .queue
                .items
                .iter()
                .map(|e| PersistQueueItem {
                    item_id: e.item_id.clone(),
                    playlist_item_id: e.playlist_item_id.clone(),
                })
                .collect(),
            playing_index: self.queue.playing_index,
            repeat_mode: self.queue.repeat_mode.clone(),
            shuffle_mode: self.queue.shuffle_mode.clone(),
            queue_updated_unix_ms: self.queue.updated_unix_ms,
            waiting: self.waiting.as_ref().map(|w| PersistWaiting {
                pending: w.pending.iter().copied().collect(),
                resume_playing: w.resume_playing,
                position_ms: w.position_ms,
            }),
            group_name: self.group_name.clone(),
        }
    }

    /// Rebuild a group's state from a persisted snapshot (the takeover path).
    /// Members are restored with fresh clock offsets + cleared buffering flags;
    /// a still-open readiness gate gets a fresh deadline so the anti-wedge
    /// timeout still fires on the new owner.
    fn apply_persist(&mut self, ps: PersistState) {
        // Stamp every restored member "seen now": each gets a full
        // MEMBER_TTL_MS to reconnect after the deploy before ghost-pruning.
        let now = tokio::time::Instant::now();
        self.members = ps
            .members
            .into_iter()
            .map(|m| {
                (
                    m.id,
                    MemberRec {
                        name: m.name,
                        offset: ClockOffset::default(),
                        buffering: false,
                        ignore_wait: m.ignore_wait,
                        last_seen: now,
                    },
                )
            })
            .collect();
        self.leader = ps.leader;
        self.group_paused_due_to_buffering = ps.group_paused_due_to_buffering;
        // Back-fill a frozen-but-fieldless (pre-field) snapshot as "was
        // playing" — a freeze only ever engages from a Playing group.
        self.buffering_resume_playing =
            ps.buffering_resume_playing || ps.group_paused_due_to_buffering;
        // B55 — a group hydrated mid-freeze restores members with cleared
        // buffering flags (they re-report on reconnect), so the BufferingEnd
        // auto-resume can never fire on its own. Re-arm the anti-wedge deadline
        // (fresh, like the readiness gate below) so the new owner resumes
        // instead of squatting frozen forever.
        self.buffering_since = self
            .group_paused_due_to_buffering
            .then(tokio::time::Instant::now);
        self.playback = match ps.playback {
            PersistPlayback::Idle => PlaybackState::Idle,
            PersistPlayback::Playing {
                position_ms,
                anchor_server_ms,
            } => PlaybackState::Playing {
                position_ms,
                anchor_server_ms,
            },
            PersistPlayback::Paused { position_ms } => PlaybackState::Paused { position_ms },
        };
        self.queue.items = ps
            .queue_items
            .into_iter()
            .map(|e| QueueEntry {
                item_id: e.item_id,
                playlist_item_id: e.playlist_item_id,
            })
            .collect();
        self.queue.playing_index = ps.playing_index;
        self.queue.repeat_mode = ps.repeat_mode;
        self.queue.shuffle_mode = ps.shuffle_mode;
        self.queue.updated_unix_ms = ps.queue_updated_unix_ms;
        self.waiting = ps.waiting.map(|w| WaitingGate {
            pending: w.pending.into_iter().collect(),
            resume_playing: w.resume_playing,
            position_ms: w.position_ms,
            // Not persisted: the spurious-Ready window is ~lead-sized (≤2.2s)
            // and a takeover mid-gate re-arms the deadline anyway.
            not_before_server_ms: 0,
            deadline: tokio::time::Instant::now()
                + std::time::Duration::from_millis(READY_TIMEOUT_MS),
        });
        self.group_name = ps.group_name;
    }
}

// ---------------------------------------------------------------------------
// Persisted snapshot (Phase B4.3c). Mirrors `GroupState` minus per-connection
// data (sinks, clock offsets, buffering flags) and the non-portable readiness
// deadline — everything a re-hydrating replica re-derives. Serialized to the
// opaque `state_json` the store holds.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct PersistMember {
    id: MemberId,
    name: String,
    /// `default` keeps older snapshots deserializable.
    #[serde(default)]
    ignore_wait: bool,
}

#[derive(Serialize, Deserialize)]
enum PersistPlayback {
    Idle,
    Playing {
        position_ms: u64,
        anchor_server_ms: u64,
    },
    Paused {
        position_ms: u64,
    },
}

#[derive(Serialize, Deserialize)]
struct PersistQueueItem {
    item_id: String,
    playlist_item_id: String,
}

#[derive(Serialize, Deserialize)]
struct PersistWaiting {
    pending: Vec<MemberId>,
    resume_playing: bool,
    position_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct PersistState {
    members: Vec<PersistMember>,
    leader: Option<MemberId>,
    group_paused_due_to_buffering: bool,
    /// Resume intent captured when the freeze engaged. `default` keeps older
    /// snapshots (written before this field) deserializable; `apply_persist`
    /// back-fills a frozen-but-fieldless snapshot as "was playing" (the only
    /// state a freeze engages from).
    #[serde(default)]
    buffering_resume_playing: bool,
    playback: PersistPlayback,
    queue_items: Vec<PersistQueueItem>,
    playing_index: usize,
    repeat_mode: String,
    shuffle_mode: String,
    /// Preserve the queue's change-timestamp across a deploy/takeover so a
    /// hydrated replica re-sends the SAME `LastUpdate` and clients don't
    /// re-process the queue. `default` keeps older snapshots deserializable.
    #[serde(default)]
    queue_updated_unix_ms: u64,
    waiting: Option<PersistWaiting>,
    group_name: String,
}

/// Whether a persisted group snapshot's roster contains `member_id` (B24 —
/// membership recovery after a deploy). Member ids are DETERMINISTIC per
/// device (`hub::member_id_for_device`), so a reconnecting device derives the
/// same id the snapshot recorded and the server can re-attach it to its group
/// without the client re-joining. Malformed / older JSON is simply `false`.
pub fn snapshot_contains_member(state_json: &str, member_id: MemberId) -> bool {
    serde_json::from_str::<PersistState>(state_json)
        .map(|ps| ps.members.iter().any(|m| m.id == member_id))
        .unwrap_or(false)
}

/// The display fields of a persisted snapshot, for the `/SyncPlay/List`
/// surface (B28): after a restart — or on a replica that doesn't own the
/// group — the in-memory registry knows nothing, but the party still exists
/// in the store and MUST stay joinable from the client's group picker.
pub struct SnapshotSummary {
    pub group_name: String,
    pub participants: Vec<String>,
    pub play_state: GroupPlayState,
}

/// Parse a persisted snapshot's display summary. `None` for malformed JSON.
pub fn snapshot_summary(state_json: &str) -> Option<SnapshotSummary> {
    let ps: PersistState = serde_json::from_str(state_json).ok()?;
    let play_state = if ps.waiting.is_some() {
        GroupPlayState::Waiting
    } else {
        match ps.playback {
            PersistPlayback::Idle => GroupPlayState::Idle,
            PersistPlayback::Playing { .. } => GroupPlayState::Playing,
            PersistPlayback::Paused { .. } => GroupPlayState::Paused,
        }
    };
    Some(SnapshotSummary {
        group_name: ps.group_name,
        participants: ps.members.into_iter().map(|m| m.name).collect(),
        play_state,
    })
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
    /// Spawn a fresh group actor delivering through `delivery` (local sinks on
    /// a single replica, the cross-replica bus under Postgres). No persistence.
    pub fn spawn(group_id: GroupId, delivery: Arc<dyn Delivery>) -> Self {
        Self::spawn_inner(group_id, unix_now_ms(), delivery, None, None)
    }

    /// Spawn a group actor that persists its snapshot after every mutation, and
    /// optionally hydrates its initial state from a prior snapshot (the takeover
    /// path: another replica owned this group before a deploy). `epoch_unix_ms`
    /// must be the group's persisted origin so scheduling stays absolute across
    /// the handoff; for a brand-new group pass `unix_now_ms()`.
    pub fn spawn_persistent(
        group_id: GroupId,
        epoch_unix_ms: u64,
        delivery: Arc<dyn Delivery>,
        persistence: Arc<dyn GroupPersistence>,
        hydrate_from: Option<&str>,
    ) -> Self {
        let hydrated =
            hydrate_from.and_then(|json| serde_json::from_str::<PersistState>(json).ok());
        Self::spawn_inner(
            group_id,
            epoch_unix_ms,
            delivery,
            Some(persistence),
            hydrated,
        )
    }

    fn spawn_inner(
        group_id: GroupId,
        epoch_unix_ms: u64,
        delivery: Arc<dyn Delivery>,
        persistence: Option<Arc<dyn GroupPersistence>>,
        hydrate: Option<PersistState>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<GroupMsg>(256);
        let mut state = GroupState::new(group_id, epoch_unix_ms, delivery, persistence);
        // A hydrated takeover starts already populated, so it must not treat
        // itself as brand-new (which would terminate on the first empty check).
        // ANY hydration counts as "has had members" (a group only persists
        // after its creator joined) — so a crash-orphaned EMPTY snapshot
        // hydrates into an actor that terminates on its first message and
        // deletes the orphan row, instead of idling forever (B29).
        let mut ever_joined = false;
        if let Some(ps) = hydrate {
            state.apply_persist(ps);
            ever_joined = true;
        }
        tokio::spawn(async move {
            // A brand-new group has no members yet — it must NOT terminate on
            // the empty check before its creator's AddMember lands (else a New
            // that sends anything first, e.g. SetGroupName, kills the group
            // before anyone joins). Only terminate once it has HAD a member and
            // then lost the last one — OR when nobody ever joins within the
            // join deadline (B30: a /SyncPlay/New whose caller's AddMember
            // failed — e.g. no socket registered yet — left an IMMORTAL empty
            // group squatting in the picker forever).
            let spawned_at = tokio::time::Instant::now();
            const NEVER_JOINED_DEADLINE_MS: u64 = 120_000;
            let mut prune_tick =
                tokio::time::interval(std::time::Duration::from_millis(MEMBER_PRUNE_TICK_MS));
            prune_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                // Arm the readiness-gate timeout only while waiting, so a
                // silent/wedged member can never block the group forever.
                let deadline = state.waiting.as_ref().map(|w| w.deadline);
                // B55 — arm the buffering anti-wedge deadline only while the
                // group is frozen for buffering.
                let buffering_deadline = state
                    .buffering_since
                    .map(|t| t + std::time::Duration::from_millis(BUFFERING_MAX_MS));
                tokio::select! {
                    maybe = rx.recv() => {
                        let Some(msg) = maybe else { break };
                        // Snapshot/ObserveClock/MemberPing don't change persisted
                        // state; skip a write for them (ObserveClock and the
                        // KeepAlive-driven MemberPing are high-frequency).
                        let mutates = !matches!(msg,
                            GroupMsg::Snapshot { .. }
                            | GroupMsg::ObserveClock { .. }
                            | GroupMsg::MemberPing { .. });
                        handle(&mut state, msg).await;
                        if mutates {
                            state.persist();
                        }
                    }
                    _ = async {
                        // `deadline` is Some in this arm (guarded below).
                        tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)).await
                    }, if deadline.is_some() => {
                        // Timeout: drop still-pending members from the gate and
                        // start anyway (anti-wedge). They re-sync via their own
                        // drift correction.
                        if let Some(w) = state.waiting.as_ref() {
                            tracing::warn!(
                                group = %state.id,
                                pending = ?w.pending.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
                                reason = "anti-wedge timeout",
                                "syncplay: readiness gate timed out; starting without stragglers"
                            );
                        }
                        state.resolve_waiting();
                        state.persist();
                    }
                    _ = async {
                        // `buffering_deadline` is Some in this arm (guarded below).
                        tokio::time::sleep_until(buffering_deadline.unwrap_or_else(tokio::time::Instant::now)).await
                    }, if buffering_deadline.is_some() => {
                        // B55 — the V19 buffering freeze has stood past
                        // BUFFERING_MAX_MS: the buffering member is stuck or gone
                        // without a BufferingEnd. Force-clear every buffering flag
                        // and resume; a member still genuinely buffering re-syncs
                        // via its own drift correction (same contract as the
                        // readiness-gate anti-wedge above). Skip if a readiness
                        // gate is mid-flight — that path owns the resume.
                        tracing::warn!(
                            group = %state.id,
                            reason = "buffering anti-wedge timeout",
                            "syncplay: buffering freeze timed out; resuming group"
                        );
                        for m in state.members.values_mut() {
                            m.buffering = false;
                        }
                        if state.waiting.is_none() {
                            // Resume per the captured intent (Playing normally;
                            // Paused if the freeze engaged around a user pause).
                            state.resume_after_buffering();
                        } else {
                            // A gate owns the resume; just disarm the freeze so
                            // this arm doesn't spin.
                            state.clear_buffering_freeze();
                        }
                        state.persist();
                    }
                    _ = prune_tick.tick() => {
                        // T83 — ghost prune: members silent past MEMBER_TTL_MS
                        // (no KeepAlive-driven ping, no clock report, no command)
                        // are gone-for-good roster entries — typically hydrated
                        // after a deploy from a device that never reconnected.
                        // Removing them keeps readiness gates from waiting the
                        // full anti-wedge timeout on every play/seek forever and
                        // keeps the participants list honest.
                        let now = tokio::time::Instant::now();
                        let ghosts: Vec<MemberId> = state
                            .members
                            .iter()
                            .filter(|(_, m)| now.saturating_duration_since(m.last_seen).as_millis() as u64 > MEMBER_TTL_MS)
                            .map(|(id, _)| *id)
                            .collect();
                        if !ghosts.is_empty() {
                            for id in ghosts {
                                tracing::info!(group = %state.id, member = %id, "syncplay: pruning unresponsive member (ghost)");
                                remove_member(&mut state, id);
                            }
                            state.persist();
                        }
                    }
                }
                ever_joined |= !state.members.is_empty();
                if ever_joined && state.members.is_empty() {
                    // Had members, now empty → terminate. Registry respawns on
                    // the next Join. Drop the persisted snapshot so a stale group
                    // can't be re-hydrated after everyone has left.
                    if let Some(p) = &state.persistence {
                        p.remove(state.id);
                    }
                    break;
                }
                // B30 — nobody EVER joined within the deadline: the creator's
                // AddMember never arrived (e.g. New without a registered
                // socket). Dissolve instead of squatting in the picker forever.
                if !ever_joined
                    && spawned_at.elapsed().as_millis() as u64 > NEVER_JOINED_DEADLINE_MS
                {
                    tracing::info!(group = %state.id, "syncplay: dissolving never-joined group (join deadline)");
                    if let Some(p) = &state.persistence {
                        p.remove(state.id);
                    }
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

/// Remove a member from the roster: re-elect leadership if needed, notify the
/// remaining members, and unblock any readiness gate it was pending in.
/// Shared by the explicit `RemoveMember` (socket teardown / Leave) and the
/// T83 ghost prune.
fn remove_member(state: &mut GroupState, member_id: MemberId) {
    let was_leader = state.leader == Some(member_id);
    // Capture the display name BEFORE the roster drop — the wire `UserLeft`
    // toast renders this string verbatim (B37: a uuid here reached users).
    let name = state
        .members
        .get(&member_id)
        .map(|m| m.name.clone())
        .unwrap_or_default();
    tracing::info!(group = %state.id, member = %member_id, user = %name, "syncplay: member removed from group");
    state.members.remove(&member_id);
    if was_leader {
        state.elect_leader();
        if let Some(new_leader) = state.leader {
            state.broadcast(ServerMsg::LeaderChange { leader: new_leader });
        }
    }
    state.broadcast(ServerMsg::MemberLeft { member_id, name });
    // A departing member must not wedge the readiness gate: drop it
    // from the pending set and resolve if it was the last holdout (and
    // members remain — an empty group terminates the actor anyway).
    if let Some(w) = state.waiting.as_mut() {
        w.pending.remove(&member_id);
        if w.pending.is_empty() && !state.members.is_empty() {
            state.resolve_waiting();
        }
    }
    // B55 — a departing member must not wedge the V19 buffering freeze either.
    // If the group is frozen for buffering and the member that just left was
    // the last one still buffering, lift the freeze now (same auto-resume as
    // BufferingEnd) instead of waiting out the anti-wedge deadline. Skipped
    // when a readiness gate is open — that path owns the resume.
    if state.group_paused_due_to_buffering
        && !state.members.is_empty()
        && state.waiting.is_none()
        && !state.members.values().any(|m| m.buffering)
    {
        tracing::info!(group = %state.id, "syncplay: buffering member left — lifting freeze");
        state.resume_after_buffering();
    }
}

/// The member a message is ATTRIBUTED to, for liveness tracking (T83): any
/// message a client causes counts as a sign of life. `AddMember` stamps its
/// own fresh record; `RemoveMember`/`Snapshot`/`SetGroupName` carry no
/// attributable member.
fn msg_member(msg: &GroupMsg) -> Option<MemberId> {
    match msg {
        GroupMsg::MemberPing { member_id }
        | GroupMsg::MemberReady { member_id, .. }
        | GroupMsg::BufferingStart { member_id, .. }
        | GroupMsg::BufferingEnd { member_id }
        | GroupMsg::ObserveClock { member_id, .. }
        | GroupMsg::SetIgnoreWait { member_id, .. }
        | GroupMsg::ResyncMember { member_id } => Some(*member_id),
        GroupMsg::LeaderPlay { sender, .. }
        | GroupMsg::LeaderPause { sender }
        | GroupMsg::LeaderSeek { sender, .. }
        | GroupMsg::Unpause { sender }
        | GroupMsg::PauseShared { sender }
        | GroupMsg::SeekTo { sender, .. }
        | GroupMsg::SetNewQueue { sender, .. }
        | GroupMsg::SetPlaylistItem { sender, .. }
        | GroupMsg::NextItem { sender, .. }
        | GroupMsg::PreviousItem { sender, .. }
        | GroupMsg::SetRepeatMode { sender, .. }
        | GroupMsg::SetShuffleMode { sender, .. } => Some(*sender),
        GroupMsg::AddMember { .. } | GroupMsg::RemoveMember { .. } | GroupMsg::Snapshot { .. } => {
            None
        }
        GroupMsg::SetGroupName { .. } => None,
    }
}

async fn handle(state: &mut GroupState, msg: GroupMsg) {
    // T83 — any attributed message is a sign of life.
    if let Some(id) = msg_member(&msg) {
        if let Some(m) = state.members.get_mut(&id) {
            m.last_seen = tokio::time::Instant::now();
        }
    }
    match msg {
        GroupMsg::AddMember {
            member_id,
            name,
            reply,
        } => {
            // B57 — idempotent re-add: a duplicate New/Join for a member already
            // in the roster (client retry, or the New/Join churn a socket race
            // can trigger) must NOT reset the member's buffering/ignore_wait
            // flags or fire a spurious "user joined" toast to everyone. Treat it
            // as a reconnect: refresh name + liveness, re-send this member's own
            // Joined + catch-up (so its fresh client re-syncs), and reply — but
            // skip the roster-wide MemberJoined broadcast.
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.name = name.clone();
                rec.last_seen = tokio::time::Instant::now();
                let summaries = state.member_summaries();
                let leader = state.leader.unwrap_or(member_id);
                let _ = reply.send(Joined {
                    group_id: state.id,
                    leader,
                    members: summaries.clone(),
                });
                state.send_one(
                    member_id,
                    ServerMsg::Joined {
                        group_id: state.id,
                        leader,
                        members: summaries,
                    },
                );
                state.send_catch_up(member_id);
                return;
            }
            let was_empty = state.members.is_empty();
            state.members.insert(
                member_id,
                MemberRec {
                    name: name.clone(),
                    offset: ClockOffset::default(),
                    buffering: false,
                    ignore_wait: false,
                    last_seen: tokio::time::Instant::now(),
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
            // Tell existing members someone joined (not the joiner itself).
            let me = MemberSummary {
                member_id,
                name,
                is_leader: Some(member_id) == state.leader,
            };
            state.broadcast_except(member_id, ServerMsg::MemberJoined { member: me });
            // Queue + playback catch-up so the new member loads the SAME item at
            // the group's current position. Adding a member NEVER mutates
            // `playing_index` (A6: a join must not advance the group).
            state.send_catch_up(member_id);
        }
        GroupMsg::ResyncMember { member_id } => {
            // A reconnected socket for an existing member (its fresh sink is
            // already in the replica's MemberSinks): re-send the catch-up so it
            // immediately re-syncs. The member (and its place in any readiness
            // gate) is untouched. Ignored if the member isn't in the roster.
            if state.members.contains_key(&member_id) {
                // A reconnect (page reload / deploy rollover) hands a FRESH
                // jellyfin-web Manager whose `groupInfo` is null and whose
                // SyncPlay is not enabled. Lead the catch-up with `Joined`
                // (→ GroupJoined) so it re-establishes `groupInfo` + re-enables
                // SyncPlay BEFORE the queue/playback commands — otherwise the
                // client ignores the resumed Unpause ("SyncPlay not enabled")
                // and crashes reading `groupInfo.Participants` on the PlayQueue,
                // poisoning SyncPlay for the whole session. `enableSyncPlay` is
                // idempotent client-side, so a live socket-blip re-join is a
                // no-op there.
                state.send_one(
                    member_id,
                    ServerMsg::Joined {
                        group_id: state.id,
                        leader: state.leader.unwrap_or(member_id),
                        members: state.member_summaries(),
                    },
                );
                state.send_catch_up(member_id);
            }
        }
        GroupMsg::RemoveMember { member_id } => {
            remove_member(state, member_id);
        }
        GroupMsg::MemberPing { member_id } => {
            // Liveness only — the shared touch at the top of `handle` already
            // refreshed `last_seen_server_ms`.
            let _ = member_id;
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
            let at_server_ms = state.server_ms_now() + state.lead_time_ms();
            // A deliberate pause supersedes an in-flight V19 buffering freeze.
            state.clear_buffering_freeze();
            // Freeze position at the moment we paused so late joiners
            // get the correct still-frame.
            let position_ms = state.freeze_paused_position();
            state.broadcast(ServerMsg::Pause {
                at_server_ms,
                position_ms,
            });
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
            playlist_item_id,
        } => {
            // B37 — a Buffering report for a STALE queue entry is the old
            // player stalling mid-teardown while the group already moved on
            // (next episode). Freezing the whole group for it wedges the new
            // item's start; instead pull the behind client forward.
            if state.pli_is_stale(&playlist_item_id) {
                tracing::info!(
                    group = %state.id, member = %member_id,
                    sent = playlist_item_id.as_deref().unwrap_or(""),
                    current = state.current_playlist_item_id().unwrap_or(""),
                    "syncplay: stale-item Buffering ignored; re-sending queue"
                );
                state.send_play_queue_to(member_id);
                return;
            }
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.buffering = true;
            }
            // V19: one corrective Pause, not a storm. If already paused due
            // to another member's buffering, do nothing. Only a PLAYING group
            // freezes: buffer isolation exists to pause a party that is playing
            // when one member stalls — a group that is already Paused (a user
            // pause, or a track-change reload while paused) has nothing to
            // isolate, and freezing it would resume Playing on the reload's
            // Ready, playing out from under the pause.
            if !state.group_paused_due_to_buffering
                && matches!(state.playback, PlaybackState::Playing { .. })
                && state.members.values().any(|m| m.buffering)
            {
                tracing::info!(group = %state.id, member = %member_id, "syncplay: member buffering — freezing group (V19)");
                state.group_paused_due_to_buffering = true;
                // Capture resume intent BEFORE freeze_paused_position() below
                // overwrites Playing → Paused. Guarded on Playing above, so this
                // is always true here; kept explicit so the recovery paths read
                // one field rather than re-deriving intent from clobbered state.
                state.buffering_resume_playing = true;
                // B55 — arm the anti-wedge deadline the moment the freeze
                // engages so a member buffering forever (or vanishing mid-buffer
                // without a BufferingEnd) can't hold the group past
                // BUFFERING_MAX_MS.
                state.buffering_since = Some(tokio::time::Instant::now());
                let at_server_ms = state.server_ms_now() + MIN_LEAD_MS;
                // Freeze playback state too, the same way LeaderPause does.
                // Without this, `playback` stays `Playing` for the whole
                // buffering window, so a member joining during the window
                // hits the late-joiner catch-up and is told to *Play* —
                // desynced from everyone else who is paused. (V19 buffer
                // isolation.)
                let position_ms = state.freeze_paused_position();
                state.broadcast(ServerMsg::Pause {
                    at_server_ms,
                    position_ms,
                });
            }
        }
        GroupMsg::BufferingEnd { member_id } => {
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.buffering = false;
            }
            // B57 — a member the group is WAITING on (an Unpause opened a gate
            // on the members that were buffering) signals its recovery with
            // BufferingEnd, not Ready (ws-native / native clients). If the gate
            // never counted BufferingEnd it could only clear via the 30s
            // anti-wedge — a needless hang on every mid-buffer Unpause. Treat it
            // like MemberReady: drop the member from the gate and resolve when
            // it was the last holdout.
            if let Some(w) = state.waiting.as_mut() {
                if w.pending.remove(&member_id) && w.pending.is_empty() {
                    state.resolve_waiting();
                }
            }
            // B27 — same auto-resume as the HTTP path's MemberReady: the
            // buffering-caused freeze lifts the moment the last buffering
            // member recovers (Jellyfin parity; ws-native clients report
            // BufferingEnd instead of Ready).
            if state.group_paused_due_to_buffering
                && !state.members.values().any(|m| m.buffering)
                && state.waiting.is_none()
            {
                state.resume_after_buffering();
            }
        }
        GroupMsg::Unpause { sender: _ } => {
            // A gate is already open (queue load / seek in flight): it
            // resolves on its members' Readys — do NOT replace it. (Replacing
            // reset the anti-wedge deadline, so a user spamming Unpause used
            // to extend the group's hang indefinitely.)
            if state.waiting.is_some() {
                return;
            }
            let position_ms = state.current_position_ms();
            // jellyfin-web only posts `Ready` on a player transition, so an
            // already-buffered paused player never ACKs a withheld Unpause —
            // gating here deadlocked until the anti-wedge fired (a guaranteed
            // 30s hang on every resume, and the eventual play() landed
            // outside the user's activation window → autoplay-blocked).
            // Start immediately; gate only on members actually buffering.
            let buffering: HashSet<MemberId> = state
                .members
                .iter()
                .filter(|(_, m)| m.buffering && !m.ignore_wait)
                .map(|(id, _)| *id)
                .collect();
            if buffering.is_empty() {
                state.start_playing(position_ms, "Unpause");
            } else {
                state.enter_waiting(buffering, true, position_ms, "Unpause", 0);
            }
        }
        GroupMsg::PauseShared { sender: _ } => {
            // Immediate group pause (no readiness gate). Freeze the position so
            // a late joiner gets the correct still-frame, then broadcast.
            let at_server_ms = state.server_ms_now() + state.lead_time_ms();
            // Cancel any pending readiness gate — we're pausing, not starting.
            state.waiting = None;
            // A deliberate pause supersedes an in-flight V19 buffering freeze:
            // clear its bookkeeping so a member's later Ready / the anti-wedge
            // timer can't force-resume the group out from under this pause.
            state.clear_buffering_freeze();
            let position_ms = state.freeze_paused_position();
            state.broadcast(ServerMsg::Pause {
                at_server_ms,
                position_ms,
            });
            state.broadcast(ServerMsg::StateUpdate {
                state: GroupPlayState::Paused,
                reason: "Pause".into(),
            });
        }
        GroupMsg::SeekTo {
            sender: _,
            position_ms,
        } => {
            // Preserve play/pause across a seek: a seek while playing resumes
            // playing (after re-buffer); while paused/idle it stays paused.
            //
            // B58 — but a Seek always sets `playback = Paused` below, so a
            // SECOND seek arriving before the first's gate resolves (scrubbing
            // the timeline while playing sends a burst) would recompute `resume`
            // from that Paused state = false, and the group would settle Paused
            // after the seek even though the user was playing. When a Seek gate
            // is already open it already holds the true intent — carry it over.
            let resume = match state.waiting.as_ref() {
                Some(w) => w.resume_playing,
                None => matches!(state.playback, PlaybackState::Playing { .. }),
            };
            // Deliver the Seek NOW: each client applies it (pause + seek +
            // re-buffer) and ACKs with `Ready` (scheduleSeek's 'ready'
            // handler). Withholding it until the gate resolved deadlocked —
            // nobody can ACK a command they never received.
            let at_server_ms = state.server_ms_now() + state.lead_time_ms();
            state.playback = PlaybackState::Paused { position_ms };
            state.broadcast(ServerMsg::Seek {
                at_server_ms,
                position_ms,
            });
            let pending = state.follower_ids();
            state.enter_waiting(pending, resume, position_ms, "Seek", at_server_ms);
        }
        GroupMsg::MemberReady {
            member_id,
            position_ms: _,
            playlist_item_id,
        } => {
            // B37 — the poisoned-gate bug: jellyfin-web posts Ready on EVERY
            // player transition, including the OLD episode's teardown right
            // after a NextItem queue change. Counting that stale Ready toward
            // the new item's readiness gate released Play before anyone had
            // the next episode loaded. Real Jellyfin validates the Ready's
            // PlaylistItemId against the current entry — do the same, and
            // re-send the queue so a genuinely behind client catches up and
            // posts a fresh (valid) Ready.
            if state.pli_is_stale(&playlist_item_id) {
                tracing::info!(
                    group = %state.id, member = %member_id,
                    sent = playlist_item_id.as_deref().unwrap_or(""),
                    current = state.current_playlist_item_id().unwrap_or(""),
                    "syncplay: stale-item Ready ignored; re-sending queue"
                );
                state.send_play_queue_to(member_id);
                return;
            }
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.buffering = false;
                // jellyfin-web re-follows group playback before posting Ready,
                // so a Ready implies the member no longer wants ignoring.
                rec.ignore_wait = false;
            }
            if state.waiting.is_some() {
                // B38 — a Ready arriving BEFORE the gated command's scheduled
                // at_server_ms cannot be an ACK of it (clients execute the
                // command AT that instant): it's a spurious player transition,
                // e.g. the pause wiggle right after a Seek broadcast. Counting
                // those resolved the gate ~1s after a Seek — long before any
                // client ran its scheduled seek — so the server's Play then
                // CANCELLED the clients' pending seek callbacks and everyone
                // resumed from the pre-seek position.
                let now = state.server_ms_now();
                let premature = state
                    .waiting
                    .as_ref()
                    .is_some_and(|w| now < w.not_before_server_ms);
                if premature {
                    tracing::debug!(
                        group = %state.id, member = %member_id,
                        "syncplay: Ready before the gated command's schedule time — ignored"
                    );
                    return;
                }
                let resolved = state.waiting.as_mut().is_some_and(|w| {
                    w.pending.remove(&member_id);
                    w.pending.is_empty()
                });
                if resolved {
                    state.resolve_waiting();
                }
            } else if state.group_paused_due_to_buffering
                && !state.members.values().any(|m| m.buffering)
            {
                // B27 — the freeze was INVOLUNTARY (a member's mid-play
                // buffering paused the group, V19 buffer isolation); the last
                // recovering member's Ready must resume the party
                // automatically. Real Jellyfin does exactly this
                // (WaitingGroupState issues an internal Unpause once
                // IsBuffering() clears); without it every network hiccup
                // paused the group until a human pressed play. Resume per the
                // captured intent — a freeze that engaged around a user pause
                // settles Paused rather than force-playing.
                state.resume_after_buffering();
            } else if matches!(state.playback, PlaybackState::Playing { .. }) {
                // No waiting gate: the group already resolved (often because
                // the ready-timeout fired before THIS member's player finished
                // loading, so it dropped the broadcast Unpause — "no active
                // player"). Heal it: replay the live playback state to just
                // this member so a slow-to-start client still catches up
                // instead of being stranded paused while everyone else plays.
                // Only while Playing — a paused member is already settled, and
                // healing it with another Pause would re-trigger its Ready
                // (command loop).
                state.send_playback_state(member_id);
            }
        }
        GroupMsg::SetIgnoreWait { member_id, ignore } => {
            if let Some(rec) = state.members.get_mut(&member_id) {
                rec.ignore_wait = ignore;
            }
            // A member opting out has HALTED its own playback: it will never
            // post the Ready/BufferingEnd the group is waiting on. Release it
            // from BOTH gates it could be holding.
            if ignore {
                // 1) The readiness gate (Seek / Unpause in flight).
                if let Some(w) = state.waiting.as_mut() {
                    w.pending.remove(&member_id);
                    if w.pending.is_empty() {
                        state.resolve_waiting();
                    }
                }
                // 2) The V19 buffering freeze. A buffering freeze opens NO
                // readiness gate (BufferingStart broadcasts a bare Pause), so
                // the branch above is a no-op for it and `rec.buffering` would
                // stay set forever — the resume checks in BufferingEnd /
                // MemberReady only fire on a member EVENT, and no further event
                // arrives for a halted member. So the group stayed frozen until
                // the 30s anti-wedge. This is the audio/subtitle track-change
                // wedge: the reloading member's cold transcode overran its start
                // budget, it posted SetIgnoreWait, and nothing cleared its
                // buffering flag. Clear it here and actively re-run the resume
                // check (mirrors BufferingEnd's B27 auto-resume).
                if let Some(rec) = state.members.get_mut(&member_id) {
                    rec.buffering = false;
                }
                if state.group_paused_due_to_buffering
                    && !state.members.values().any(|m| m.buffering)
                    && state.waiting.is_none()
                {
                    state.resume_after_buffering();
                }
            }
        }
        GroupMsg::SetNewQueue {
            sender: _,
            item_ids,
            playing_index,
            start_position_ms,
        } => {
            // B57 — an empty queue has nothing to play: setting it and opening a
            // readiness gate would leave the group reporting Playing with no
            // current item (a wedge that only a later non-empty queue clears).
            // jellyfin-web never sends this, but a malformed/racing client could.
            if item_ids.is_empty() {
                tracing::warn!(group = %state.id, "syncplay: SetNewQueue with empty queue ignored");
                return;
            }
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
            let pending = state.follower_ids();
            state.enter_waiting(pending, true, start_position_ms, "Unpause", 0);
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
                let pending = state.follower_ids();
                state.enter_waiting(pending, true, 0, "Unpause", 0);
            }
        }
        GroupMsg::NextItem {
            sender,
            playlist_item_id,
        } => {
            // B37 — real-Jellyfin dedupe: the request names the entry the
            // client believes is playing. A mismatch means the client is
            // behind (or two members raced Next) — advancing again would skip
            // an episode. No-op, like NextItemGroupRequest does.
            if state.pli_is_stale(&playlist_item_id) {
                tracing::info!(
                    group = %state.id, member = %sender,
                    sent = playlist_item_id.as_deref().unwrap_or(""),
                    current = state.current_playlist_item_id().unwrap_or(""),
                    "syncplay: NextItem for stale entry ignored (double-press/race)"
                );
                return;
            }
            if state.queue.playing_index + 1 < state.queue.items.len() {
                state.queue.playing_index += 1;
                tracing::info!(
                    group = %state.id, index = state.queue.playing_index,
                    "syncplay: queue advanced to next item"
                );
                state.broadcast_play_queue("next_item", true, 0);
                let pending = state.follower_ids();
                state.enter_waiting(pending, true, 0, "Unpause", 0);
            }
        }
        GroupMsg::PreviousItem {
            sender,
            playlist_item_id,
        } => {
            if state.pli_is_stale(&playlist_item_id) {
                tracing::info!(
                    group = %state.id, member = %sender,
                    "syncplay: PreviousItem for stale entry ignored"
                );
                return;
            }
            if state.queue.playing_index > 0 {
                state.queue.playing_index -= 1;
                tracing::info!(
                    group = %state.id, index = state.queue.playing_index,
                    "syncplay: queue moved to previous item"
                );
                state.broadcast_play_queue("previous_item", true, 0);
                let pending = state.follower_ids();
                state.enter_waiting(pending, true, 0, "Unpause", 0);
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
                current_item_id: state
                    .queue
                    .items
                    .get(state.queue.playing_index)
                    .map(|e| e.item_id.clone()),
            };
            let _ = reply.send(snap);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::delivery::{LocalDelivery, MemberSinks};
    use std::sync::Mutex;
    use std::time::Duration;

    /// B24 — `snapshot_contains_member` recognises a member id in a REAL
    /// persisted snapshot (produced by the actor's own persistence hook, not a
    /// hand-written JSON that could drift from `PersistState`).
    #[tokio::test]
    async fn snapshot_contains_member_matches_real_persisted_state() {
        struct Capture(Mutex<Option<String>>);
        impl crate::persistence::GroupPersistence for Capture {
            fn persist(&self, _g: GroupId, _e: u64, state_json: String) {
                *self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state_json);
            }
            fn remove(&self, _g: GroupId) {}
        }
        let capture = Arc::new(Capture(Mutex::new(None)));
        let sinks = MemberSinks::new();
        let delivery = Arc::new(LocalDelivery::new(sinks.clone()));
        let h =
            GroupHandle::spawn_persistent(GroupId::new(), 1_000, delivery, capture.clone(), None);
        let member = MemberId::new();
        let (sink_tx, _sink_rx) = mpsc::channel(8);
        sinks.insert(member, 1, sink_tx);
        let (rtx, rrx) = oneshot::channel();
        h.tx.send(GroupMsg::AddMember {
            member_id: member,
            name: "alison".into(),
            reply: rtx,
        })
        .await
        .unwrap();
        let _ = rrx.await.unwrap();
        // The persistence hook fires on mutation; poll briefly for the write.
        let mut json = None;
        for _ in 0..100 {
            json = capture
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if json.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let json = json.expect("snapshot persisted after AddMember");
        assert!(
            snapshot_contains_member(&json, member),
            "roster member found"
        );
        assert!(
            !snapshot_contains_member(&json, MemberId::new()),
            "foreign member not found"
        );
        assert!(!snapshot_contains_member("not json", member));
    }

    /// Spawn a group whose delivery goes to an in-process `MemberSinks`, the
    /// same wiring the single-replica server uses. Return the sinks so tests can
    /// register each member's socket before `AddMember`.
    fn spawn_group() -> (GroupHandle, MemberSinks) {
        let sinks = MemberSinks::new();
        let delivery = Arc::new(LocalDelivery::new(sinks.clone()));
        let h = GroupHandle::spawn(GroupId::new(), delivery);
        (h, sinks)
    }

    /// Test persistence sink: captures the latest snapshot the actor wrote, so a
    /// second (hydrated) actor can be built from it — modelling takeover.
    #[derive(Default)]
    struct CapturePersistence {
        latest: Mutex<Option<(GroupId, u64, String)>>,
    }
    impl GroupPersistence for CapturePersistence {
        fn persist(&self, group_id: GroupId, epoch_unix_ms: u64, state_json: String) {
            *self.latest.lock().unwrap() = Some((group_id, epoch_unix_ms, state_json));
        }
        fn remove(&self, _group_id: GroupId) {
            *self.latest.lock().unwrap() = None;
        }
    }

    async fn snapshot_of(h: &GroupHandle) -> GroupSnapshot {
        let (tx, rx) = oneshot::channel();
        h.tx.send(GroupMsg::Snapshot { reply: tx }).await.unwrap();
        rx.await.unwrap()
    }

    /// Register `mid`'s sink in `sinks` (as the socket layer does) then send
    /// `AddMember`. Drains the self-`Joined` so tests see the post-join stream.
    async fn join(
        h: &GroupHandle,
        sinks: &MemberSinks,
        mid: MemberId,
        name: &str,
    ) -> mpsc::Receiver<ServerMsg> {
        let (tx, mut rx) = mpsc::channel(64);
        sinks.insert(mid, 1, tx);
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(GroupMsg::AddMember {
            member_id: mid,
            name: name.into(),
            reply: reply_tx,
        })
        .await
        .unwrap();
        let joined = reply_rx.await.unwrap();
        let first = rx.recv().await;
        assert!(
            matches!(first, Some(ServerMsg::Joined { .. })),
            "first message to a joiner must be Joined, got {first:?}"
        );
        let _ = joined;
        rx
    }

    /// T83 — a member that stops signalling (no KeepAlive-driven ping, no
    /// commands) past MEMBER_TTL_MS is pruned as a ghost; a member that keeps
    /// pinging survives indefinitely. Paused-clock test: `advance` drives both
    /// the prune interval and the TTL arithmetic deterministically.
    #[tokio::test(start_paused = true)]
    async fn ghost_member_is_pruned_while_pinging_member_survives() {
        let (h, sinks) = spawn_group();
        let alive = MemberId::new();
        let ghost = MemberId::new();
        let _rx_alive = join(&h, &sinks, alive, "alive").await;
        let _rx_ghost = join(&h, &sinks, ghost, "ghost").await;
        assert_eq!(snapshot_of(&h).await.participants.len(), 2);

        // 7 × 30s = 210s > MEMBER_TTL_MS (150s). "alive" pings every 30s;
        // "ghost" stays silent. Snapshot after each step is a processing
        // barrier (the actor is a single loop).
        for _ in 0..7 {
            tokio::time::advance(std::time::Duration::from_millis(MEMBER_PRUNE_TICK_MS)).await;
            h.tx.send(GroupMsg::MemberPing { member_id: alive })
                .await
                .unwrap();
            let _ = snapshot_of(&h).await;
        }
        let snap = snapshot_of(&h).await;
        assert_eq!(
            snap.participants,
            vec!["alive".to_string()],
            "ghost pruned, pinger survives"
        );
    }

    /// B30 — a group NOBODY ever joined (New whose caller's AddMember failed)
    /// dissolves at the join deadline instead of squatting forever.
    #[tokio::test(start_paused = true)]
    async fn never_joined_group_dissolves_at_join_deadline() {
        let (h, _sinks) = spawn_group();
        // Name it (the New handler does) — still no members.
        h.tx.send(GroupMsg::SetGroupName {
            name: "ghost town".into(),
        })
        .await
        .unwrap();
        // Advance past the 120s join deadline (prune tick wakes the loop).
        for _ in 0..6 {
            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;
        }
        for _ in 0..200 {
            if h.tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(h.tx.is_closed(), "never-joined group must dissolve");
    }

    /// B30 guard-rail: the join deadline must NOT kill a group whose creator
    /// joined — a live (pinging) group survives well past it. (A joined-but-
    /// silent group is the T83 ghost path, covered separately.)
    #[tokio::test(start_paused = true)]
    async fn joined_group_survives_past_join_deadline() {
        let (h, sinks) = spawn_group();
        let m = MemberId::new();
        let _rx = join(&h, &sinks, m, "pinger").await;
        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(30)).await;
            h.tx.send(GroupMsg::MemberPing { member_id: m })
                .await
                .unwrap();
            let _ = snapshot_of(&h).await;
        }
        assert!(
            !h.tx.is_closed(),
            "joined + pinging group must survive well past the join deadline"
        );
    }

    /// REAL-TIME probe (not paused clock): hydrated roster with no reconnects
    /// must ghost-prune + dissolve within ~TTL+tick. Ignored by default (slow);
    /// run explicitly when chasing live prune failures.
    #[tokio::test]
    #[ignore]
    async fn realtime_hydrated_ghosts_prune() {
        let capture1 = Arc::new(CapturePersistence::default());
        let sinks = MemberSinks::new();
        let delivery = Arc::new(LocalDelivery::new(sinks.clone()));
        let gid = GroupId::new();
        let h1 =
            GroupHandle::spawn_persistent(gid, 1_000, delivery.clone(), capture1.clone(), None);
        let _r1 = join(&h1, &sinks, MemberId::new(), "a").await;
        let _r2 = join(&h1, &sinks, MemberId::new(), "b").await;
        let json = capture1.latest.lock().unwrap().clone().unwrap().2;
        drop(h1);
        let capture2 = Arc::new(CapturePersistence::default());
        capture2.persist(gid, 1_000, json.clone());
        let h2 = GroupHandle::spawn_persistent(
            gid,
            1_000,
            Arc::new(LocalDelivery::new(MemberSinks::new())),
            capture2.clone(),
            Some(&json),
        );
        // TTL 150s + tick 30s + slack.
        tokio::time::sleep(Duration::from_secs(200)).await;
        assert!(h2.tx.is_closed(), "actor must dissolve in real time");
        assert!(capture2.latest.lock().unwrap().is_none(), "row removed");
    }

    /// B29 probe — a hydrated EMPTY snapshot (crash orphan) must dissolve on
    /// its first tick and REMOVE its row.
    #[tokio::test(start_paused = true)]
    async fn hydrated_empty_snapshot_dissolves_immediately() {
        let capture = Arc::new(CapturePersistence::default());
        let gid = GroupId::new();
        // Empty-roster snapshot json, produced by serializing a fresh state.
        let empty_state = GroupState::new(
            gid,
            1_000,
            Arc::new(LocalDelivery::new(MemberSinks::new())),
            None,
        );
        let json = serde_json::to_string(&empty_state.to_persist()).unwrap();
        capture.persist(gid, 1_000, json.clone());
        let h = GroupHandle::spawn_persistent(
            gid,
            1_000,
            Arc::new(LocalDelivery::new(MemberSinks::new())),
            capture.clone(),
            Some(&json),
        );
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(31)).await;
            tokio::task::yield_now().await;
        }
        for _ in 0..200 {
            if h.tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(h.tx.is_closed(), "hydrated-empty actor must terminate");
        assert!(
            capture.latest.lock().unwrap().is_none(),
            "hydrated-empty group's row must be removed"
        );
    }

    /// B29 — a hydrated group whose members NEVER reconnect must dissolve by
    /// itself: hydration stamps the roster "seen now", the ghost prune reaps
    /// everyone after MEMBER_TTL_MS, the emptied actor terminates AND deletes
    /// its persisted snapshot. This is what keeps orphaned snapshots (pod
    /// restarted, everyone gone) from haunting the join picker for 48h.
    #[tokio::test(start_paused = true)]
    async fn hydrated_group_with_no_show_members_dissolves_and_removes_snapshot() {
        // Produce a REAL snapshot: an actor with one member persists its state.
        let capture1 = Arc::new(CapturePersistence::default());
        let sinks = MemberSinks::new();
        let delivery = Arc::new(LocalDelivery::new(sinks.clone()));
        let gid = GroupId::new();
        let h1 =
            GroupHandle::spawn_persistent(gid, 1_000, delivery.clone(), capture1.clone(), None);
        let _rx = join(&h1, &sinks, MemberId::new(), "ghost").await;
        let json = capture1
            .latest
            .lock()
            .unwrap()
            .clone()
            .expect("snapshot persisted")
            .2;
        drop(h1);

        // "Restart": hydrate the snapshot on a fresh actor; nobody reconnects.
        let capture2 = Arc::new(CapturePersistence::default());
        // Seed the capture as non-empty so we can observe the REMOVE.
        capture2.persist(gid, 1_000, json.clone());
        let h2 = GroupHandle::spawn_persistent(
            gid,
            1_000,
            Arc::new(LocalDelivery::new(MemberSinks::new())),
            capture2.clone(),
            Some(&json),
        );

        // TTL (150s) + prune tick (30s) → the no-show roster is reaped and the
        // actor dissolves. Advance well past it.
        for _ in 0..8 {
            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;
        }
        for _ in 0..200 {
            if h2.tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(h2.tx.is_closed(), "orphan group actor must terminate");
        assert!(
            capture2.latest.lock().unwrap().is_none(),
            "orphan group's snapshot must be REMOVED on dissolution"
        );
    }

    /// A fresh group with one member ("first", the leader). Returns the shared
    /// `MemberSinks` so the test can add more members via `add_member`.
    async fn fresh() -> (
        GroupHandle,
        MemberSinks,
        mpsc::Receiver<ServerMsg>,
        MemberId,
    ) {
        let (h, sinks) = spawn_group();
        let mid = MemberId::new();
        let rx = join(&h, &sinks, mid, "first").await;
        (h, sinks, rx, mid)
    }

    /// Add another member to an existing group + its sinks.
    async fn add_member(
        h: &GroupHandle,
        sinks: &MemberSinks,
        name: &str,
    ) -> (MemberId, mpsc::Receiver<ServerMsg>) {
        let mid = MemberId::new();
        let rx = join(h, sinks, mid, name).await;
        (mid, rx)
    }

    #[tokio::test]
    async fn first_member_becomes_leader() {
        let (h, _sinks, _rx, mid) = fresh().await;
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
        let (h, sinks) = spawn_group();
        h.tx.send(GroupMsg::SetGroupName {
            name: "Movie Night".into(),
        })
        .await
        .unwrap();
        // The actor must still be alive to accept the creator.
        let (tx, rx) = mpsc::channel(8);
        let mid = MemberId::new();
        sinks.insert(mid, 1, tx);
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(GroupMsg::AddMember {
            member_id: mid,
            name: "ali".into(),
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
        let (h, sinks, _rx, _mid) = fresh().await;
        let _ = add_member(&h, &sinks, "gf").await;
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
        let (h, sinks, _rx_leader, _leader) = fresh().await;
        let (other_mid, mut other_rx) = add_member(&h, &sinks, "second").await;
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
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (_other, mut other_rx) = add_member(&h, &sinks, "second").await;
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
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, &sinks, "slow").await;
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
            playlist_item_id: None,
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
        let (h, sinks, _leader_rx, leader) = fresh().await;
        let (m2_id, mut m2_rx) = add_member(&h, &sinks, "b").await;
        let (m3_id, mut m3_rx) = add_member(&h, &sinks, "c").await;
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
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, &sinks, "b").await;
        let (m3, mut m3_rx) = add_member(&h, &sinks, "c").await;

        // The group must be PLAYING for a member's buffer to freeze it (V19
        // buffer isolation only pauses a playing party).
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 0,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;

        // Drain MemberJoined + the Play notifications.
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}
        while m3_rx.try_recv().is_ok() {}

        // First buffering report → exactly one Pause per member (3 total).
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 0,
            playlist_item_id: None,
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
            playlist_item_id: None,
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
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut _m2_rx) = add_member(&h, &sinks, "b").await;
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
            playlist_item_id: None,
        })
        .await
        .unwrap();

        // Late joiner during the buffer-pause window.
        let (_late, mut late_rx) = add_member(&h, &sinks, "late").await;
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
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (_m2, _m2_rx) = add_member(&h, &sinks, "b").await;
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

    #[test]
    fn remote_command_maps_to_group_msg() {
        // A forwarded command must reconstruct the matching GroupMsg on the
        // owner. Spot-check a few variants incl. AddMember (synthetic reply).
        let m = MemberId::new();
        assert!(matches!(
            RemoteCommand::AddMember {
                member_id: m,
                name: "x".into()
            }
            .into_group_msg(),
            GroupMsg::AddMember { member_id, .. } if member_id == m
        ));
        assert!(matches!(
            RemoteCommand::Resync { member_id: m }.into_group_msg(),
            GroupMsg::ResyncMember { member_id } if member_id == m
        ));
        assert!(matches!(
            RemoteCommand::PauseShared { sender: m }.into_group_msg(),
            GroupMsg::PauseShared { sender } if sender == m
        ));
        assert!(matches!(
            RemoteCommand::SetNewQueue {
                sender: m,
                item_ids: vec!["a".into()],
                playing_index: 0,
                start_position_ms: 9
            }
            .into_group_msg(),
            GroupMsg::SetNewQueue {
                start_position_ms: 9,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn snapshot_persists_and_hydrates_onto_a_new_replica() {
        // Phase B4.3c: drive a group through real mutations on "replica A",
        // capturing each persisted snapshot; then spawn a fresh actor on
        // "replica B" hydrated from that snapshot (the deploy takeover) and
        // assert the coordination state — leader, roster, queue, playback,
        // group name — carried across.
        let cap = Arc::new(CapturePersistence::default());
        let sinks_a = MemberSinks::new();
        let gid = GroupId::new();
        let epoch = unix_now_ms();
        let a = GroupHandle::spawn_persistent(
            gid,
            epoch,
            Arc::new(LocalDelivery::new(sinks_a.clone())),
            cap.clone(),
            None,
        );

        // Two members; leader sets a queue and starts playing.
        let leader = MemberId::new();
        let _lr = join(&a, &sinks_a, leader, "leader").await;
        let m2 = MemberId::new();
        let _r2 = join(&a, &sinks_a, m2, "gf").await;
        a.tx.send(GroupMsg::SetGroupName {
            name: "Movie Night".into(),
        })
        .await
        .unwrap();
        a.tx.send(GroupMsg::SetNewQueue {
            sender: leader,
            item_ids: vec!["ep1".into(), "ep2".into()],
            playing_index: 1,
            start_position_ms: 4200,
        })
        .await
        .unwrap();
        // Drain the actor by round-tripping a Snapshot so all writes landed.
        let snap_a = snapshot_of(&a).await;

        // Grab the last persisted blob (the takeover source).
        let (cap_gid, cap_epoch, json) = cap.latest.lock().unwrap().clone().expect("persisted");
        assert_eq!(cap_gid, gid);
        assert_eq!(cap_epoch, epoch, "epoch persisted for a stable time base");

        // Replica B hydrates a fresh actor from the snapshot. Its members'
        // sinks re-register on THIS replica (reconnect), but for the state
        // assertion we only need the roster/queue/leader to have carried over.
        let sinks_b = MemberSinks::new();
        let b = GroupHandle::spawn_persistent(
            gid,
            cap_epoch,
            Arc::new(LocalDelivery::new(sinks_b)),
            cap.clone(),
            Some(&json),
        );
        let snap_b = snapshot_of(&b).await;

        assert_eq!(snap_b.leader, Some(leader), "leader survived takeover");
        assert_eq!(snap_b.member_count, 2, "roster survived takeover");
        assert_eq!(snap_b.group_name, "Movie Night");
        assert_eq!(snap_b.play_state, snap_a.play_state);
        let mut names = snap_b.participants.clone();
        names.sort();
        assert_eq!(names, vec!["gf".to_string(), "leader".to_string()]);
    }

    #[tokio::test]
    async fn reconnect_resync_resends_group_joined_first() {
        // A reconnected socket (page reload / deploy rollover) is a FRESH
        // jellyfin-web Manager whose `this.groupInfo` is null and whose SyncPlay
        // is not enabled. The catch-up MUST lead with `Joined` (→ GroupJoined),
        // or the client (a) logs "SyncPlay not enabled, ignoring command" for the
        // Unpause and (b) crashes reading `this.groupInfo.Participants` on the
        // PlayQueue — poisoning SyncPlay for the whole session.
        let (h, sinks, _leader_rx, leader) = fresh().await;
        h.tx.send(GroupMsg::SetNewQueue {
            sender: leader,
            item_ids: vec!["ep1".into(), "ep2".into()],
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 0,
        })
        .await
        .unwrap();

        // Second member joins, then drain everything its own join produced.
        let (gf, mut gf_rx) = add_member(&h, &sinks, "gf").await;
        let _ = snapshot_of(&h).await; // flush the actor
        while gf_rx.try_recv().is_ok() {}

        // Now simulate the reconnect: its fresh sink is already swapped in, the
        // actor re-sends the catch-up.
        h.tx.send(GroupMsg::ResyncMember { member_id: gf })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await; // flush the actor

        let first = gf_rx.try_recv().expect("catch-up must send something");
        assert!(
            matches!(first, ServerMsg::Joined { .. }),
            "reconnect catch-up must lead with Joined (GroupJoined), got {first:?}"
        );
        // The queue + playback state still follow.
        let mut saw_queue = false;
        while let Ok(msg) = gf_rx.try_recv() {
            if matches!(msg, ServerMsg::PlayQueue { .. }) {
                saw_queue = true;
            }
        }
        assert!(
            saw_queue,
            "reconnect catch-up must still resend the PlayQueue"
        );
    }

    /// B27 — a mid-play buffering freeze is INVOLUNTARY: when the last
    /// buffering member reports Ready, the group must resume by itself
    /// (Jellyfin parity — WaitingGroupState issues an internal Unpause).
    /// Previously it stayed paused until a human pressed play.
    #[tokio::test]
    async fn buffering_freeze_auto_resumes_on_last_ready() {
        let (h, sinks, mut rx1, m1) = fresh().await;
        let (m2, mut rx2) = add_member(&h, &sinks, "second").await;
        // Leader = the FIRST member (election runs only on the empty-join;
        // a later, lower id does NOT usurp).
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}
        while rx2.try_recv().is_ok() {}

        // m2 stalls: group freezes with ONE corrective Pause (V19).
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 1_500,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        let mut saw_pause = false;
        while let Ok(msg) = rx1.try_recv() {
            if matches!(msg, ServerMsg::Pause { .. }) {
                saw_pause = true;
            }
        }
        assert!(saw_pause, "buffering must freeze the group with a Pause");

        // m2 recovers → Ready (the HTTP wire pairing) → the group must
        // RESUME on its own: both members receive a scheduled Play.
        h.tx.send(GroupMsg::MemberReady {
            member_id: m2,
            position_ms: 1_500,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        let mut m1_play = false;
        while let Ok(msg) = rx1.try_recv() {
            if matches!(msg, ServerMsg::Play { .. }) {
                m1_play = true;
            }
        }
        let mut m2_play = false;
        while let Ok(msg) = rx2.try_recv() {
            if matches!(msg, ServerMsg::Play { .. }) {
                m2_play = true;
            }
        }
        assert!(
            m1_play && m2_play,
            "last buffering member's Ready must auto-resume the whole group"
        );
        let snap = snapshot_of(&h).await;
        assert_eq!(snap.play_state, GroupPlayState::Playing);
    }

    /// Track-change wedge (Root Cause C): a member whose cold transcode reload
    /// overran its start budget posts `SetIgnoreWait(true)` to opt out of the
    /// wait. A V19 buffering freeze opens no readiness gate, so opting out must
    /// ALSO clear the member's buffering flag and actively re-resume — otherwise
    /// the group stays frozen until the 30s anti-wedge (the reported "stuck
    /// syncing, had to seek back to recover" symptom).
    #[tokio::test]
    async fn set_ignore_wait_clears_buffering_freeze_and_resumes() {
        let (h, sinks, mut rx1, m1) = fresh().await;
        let (m2, mut rx2) = add_member(&h, &sinks, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}
        while rx2.try_recv().is_ok() {}

        // m2 reloads for a track change → BufferingStart freezes the group.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 1_500,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}
        while rx2.try_recv().is_ok() {}
        assert_eq!(
            snapshot_of(&h).await.play_state,
            GroupPlayState::Paused,
            "buffering must freeze the group"
        );

        // m2's cold reload overruns the budget → it opts out. This must lift the
        // freeze immediately, NOT wait out BUFFERING_MAX_MS.
        h.tx.send(GroupMsg::SetIgnoreWait {
            member_id: m2,
            ignore: true,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        let mut m1_play = false;
        while let Ok(msg) = rx1.try_recv() {
            if matches!(msg, ServerMsg::Play { .. }) {
                m1_play = true;
            }
        }
        assert!(
            m1_play,
            "opting a buffering member out must resume the group without the 30s anti-wedge"
        );
        assert_eq!(
            snapshot_of(&h).await.play_state,
            GroupPlayState::Playing,
            "group must be Playing again after the opt-out"
        );
    }

    /// Root Cause A: a deliberate pause DURING a member's buffer freeze must
    /// win — the later Ready must NOT force-resume the group out from under the
    /// user's pause. Before the intent capture, every freeze recovery path
    /// called start_playing unconditionally.
    #[tokio::test]
    async fn user_pause_during_buffer_freeze_is_not_overridden_by_ready() {
        let (h, sinks, mut rx1, m1) = fresh().await;
        let (m2, mut rx2) = add_member(&h, &sinks, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}
        while rx2.try_recv().is_ok() {}

        // m2 buffers → group freezes (was playing → resume intent = play).
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 1_500,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        // The user deliberately pauses while m2 is still buffering.
        h.tx.send(GroupMsg::PauseShared { sender: m1 })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}
        while rx2.try_recv().is_ok() {}

        // m2 recovers → Ready. The group must STAY paused, not resume.
        h.tx.send(GroupMsg::MemberReady {
            member_id: m2,
            position_ms: 1_500,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        let mut saw_play = false;
        while let Ok(msg) = rx1.try_recv() {
            if matches!(msg, ServerMsg::Play { .. }) {
                saw_play = true;
            }
        }
        assert!(
            !saw_play,
            "a Ready after a deliberate pause must not force-resume the group"
        );
        assert_eq!(
            snapshot_of(&h).await.play_state,
            GroupPlayState::Paused,
            "group must remain paused after the user pause"
        );
    }

    /// Root Cause A (already-paused variant): a track-change reload while the
    /// group is PAUSED must not engage the freeze, so the reload's Ready leaves
    /// the group paused rather than resuming Playing.
    #[tokio::test]
    async fn buffer_while_paused_does_not_resume_the_group() {
        let (h, sinks, mut rx1, m1) = fresh().await;
        let (m2, mut rx2) = add_member(&h, &sinks, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        h.tx.send(GroupMsg::PauseShared { sender: m1 })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}
        while rx2.try_recv().is_ok() {}

        // m2 reloads for a track change while the group is paused.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 1_000,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        h.tx.send(GroupMsg::BufferingEnd { member_id: m2 })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await;
        let mut saw_play = false;
        while let Ok(msg) = rx1.try_recv() {
            if matches!(msg, ServerMsg::Play { .. }) {
                saw_play = true;
            }
        }
        assert!(
            !saw_play,
            "a track-change reload while paused must not resume the group"
        );
        assert_eq!(
            snapshot_of(&h).await.play_state,
            GroupPlayState::Paused,
            "group must stay paused"
        );
    }

    /// B55 — the SOLE buffering member disconnecting (socket drop / Leave) must
    /// lift the V19 freeze immediately, not strand the rest of the group frozen
    /// forever waiting on a BufferingEnd that will never come.
    #[tokio::test]
    async fn buffering_freeze_lifts_when_sole_buffering_member_leaves() {
        let (h, sinks, mut rx1, m1) = fresh().await;
        let (m2, mut rx2) = add_member(&h, &sinks, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}
        while rx2.try_recv().is_ok() {}

        // m2 stalls → group freezes.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 1_500,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let snap = snapshot_of(&h).await;
        assert_eq!(snap.play_state, GroupPlayState::Paused, "freeze engaged");

        // m2 (the only buffering member) leaves WITHOUT a BufferingEnd. The
        // remaining member must be resumed, not left frozen.
        h.tx.send(GroupMsg::RemoveMember { member_id: m2 })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await;
        let mut m1_play = false;
        while let Ok(msg) = rx1.try_recv() {
            if matches!(msg, ServerMsg::Play { .. }) {
                m1_play = true;
            }
        }
        assert!(
            m1_play,
            "sole buffering member leaving must resume the group"
        );
        assert_eq!(snapshot_of(&h).await.play_state, GroupPlayState::Playing);
    }

    /// B55 — a member that buffers FOREVER (never sends BufferingEnd, never
    /// disconnects) must not hold the group frozen past BUFFERING_MAX_MS: the
    /// anti-wedge deadline force-clears the freeze and resumes.
    #[tokio::test(start_paused = true)]
    async fn buffering_freeze_times_out_via_anti_wedge() {
        let (h, sinks, mut rx1, m1) = fresh().await;
        let (m2, mut _rx2) = add_member(&h, &sinks, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}

        // m2 stalls and never recovers.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 1_500,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        assert_eq!(
            snapshot_of(&h).await.play_state,
            GroupPlayState::Paused,
            "freeze engaged"
        );

        // Advance past the anti-wedge deadline; the loop wakes and resumes.
        tokio::time::advance(Duration::from_millis(BUFFERING_MAX_MS + 1_000)).await;
        tokio::task::yield_now().await;
        let _ = snapshot_of(&h).await;
        let mut m1_play = false;
        while let Ok(msg) = rx1.try_recv() {
            if matches!(msg, ServerMsg::Play { .. }) {
                m1_play = true;
            }
        }
        assert!(
            m1_play,
            "buffering freeze must time out and resume the group"
        );
        assert_eq!(snapshot_of(&h).await.play_state, GroupPlayState::Playing);
    }

    /// B57 — a duplicate AddMember for a member already in the roster (New/Join
    /// retry or churn) must be idempotent: preserve the member's buffering flag
    /// and fire NO second "user joined" toast to the others.
    #[tokio::test]
    async fn duplicate_add_member_is_idempotent() {
        let (h, sinks, mut rx1, m1) = fresh().await;
        let (m2, mut _rx2) = add_member(&h, &sinks, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        // m2 is buffering → group frozen, buffering_member_count == 1.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 0,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}

        // Re-add m2 (a duplicate join).
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(GroupMsg::AddMember {
            member_id: m2,
            name: "second".into(),
            reply: reply_tx,
        })
        .await
        .unwrap();
        let _ = reply_rx.await;
        let snap = snapshot_of(&h).await;
        assert_eq!(snap.member_count, 2, "no phantom third member");
        assert_eq!(
            snap.buffering_member_count, 1,
            "re-add must NOT reset the buffering flag"
        );
        let mut dup_join = false;
        while let Ok(msg) = rx1.try_recv() {
            if matches!(msg, ServerMsg::MemberJoined { .. }) {
                dup_join = true;
            }
        }
        assert!(!dup_join, "duplicate re-add must not toast a second join");
    }

    /// B57 — SetNewQueue with an empty queue is ignored (never leaves the group
    /// reporting Playing with no current item).
    #[tokio::test]
    async fn empty_set_new_queue_is_ignored() {
        let (h, _sinks, _rx, _m1) = fresh().await;
        h.tx.send(GroupMsg::SetNewQueue {
            sender: _m1,
            item_ids: vec![],
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
        let snap = snapshot_of(&h).await;
        assert_eq!(
            snap.play_state,
            GroupPlayState::Idle,
            "empty queue must not start playback"
        );
    }

    /// B57 — a member the group is WAITING on (Unpause opened a gate on the
    /// buffering member) that recovers via BufferingEnd (not Ready) must resolve
    /// the gate promptly, not hang until the 30s anti-wedge.
    #[tokio::test]
    async fn buffering_end_resolves_an_open_unpause_gate() {
        let (h, sinks, mut rx1, m1) = fresh().await;
        let (m2, mut _rx2) = add_member(&h, &sinks, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        // m2 buffers → freeze.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 0,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        // Leader presses play while m2 still buffers → gate opens on m2.
        h.tx.send(GroupMsg::Unpause { sender: m1 }).await.unwrap();
        assert_eq!(
            snapshot_of(&h).await.play_state,
            GroupPlayState::Waiting,
            "Unpause on a buffering member opens a gate"
        );
        while rx1.try_recv().is_ok() {}

        // m2 recovers via BufferingEnd (ws-native signal) → gate must resolve.
        h.tx.send(GroupMsg::BufferingEnd { member_id: m2 })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await;
        let mut m1_play = false;
        while let Ok(msg) = rx1.try_recv() {
            if matches!(msg, ServerMsg::Play { .. }) {
                m1_play = true;
            }
        }
        assert!(m1_play, "BufferingEnd must resolve the gate and resume");
        assert_eq!(snapshot_of(&h).await.play_state, GroupPlayState::Playing);
    }

    /// B58 — scrubbing the timeline while playing sends a burst of Seeks. Each
    /// Seek sets playback=Paused, so a second Seek arriving before the first's
    /// gate resolves must NOT recompute resume=false and leave the group stuck
    /// Paused after the seek. The group was playing; it must resume playing.
    #[tokio::test(start_paused = true)]
    async fn seek_burst_while_playing_stays_playing() {
        let (h, sinks, mut _rx1, m1) = fresh().await;
        let (_m2, mut _rx2) = add_member(&h, &sinks, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;

        // First seek opens a gate (resume intent = playing).
        h.tx.send(GroupMsg::SeekTo {
            sender: m1,
            position_ms: 5_000,
        })
        .await
        .unwrap();
        // Second seek arrives before the gate resolves — the burst.
        h.tx.send(GroupMsg::SeekTo {
            sender: m1,
            position_ms: 8_000,
        })
        .await
        .unwrap();
        assert_eq!(
            snapshot_of(&h).await.play_state,
            GroupPlayState::Waiting,
            "still gated on the follower"
        );

        // Gate resolves via the anti-wedge timeout; the group must land Playing.
        tokio::time::advance(Duration::from_millis(READY_TIMEOUT_MS + 1_000)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            snapshot_of(&h).await.play_state,
            GroupPlayState::Playing,
            "a seek burst while playing must resume playing, not stick paused"
        );
    }

    /// B27 guard-rail: a VOLUNTARY pause (someone pressed pause) must NOT be
    /// overridden by a stray Ready — auto-resume applies only to the
    /// buffering-caused freeze.
    #[tokio::test]
    async fn voluntary_pause_is_not_auto_resumed_by_ready() {
        let (h, sinks, mut rx1, m1) = fresh().await;
        let (m2, mut rx2) = add_member(&h, &sinks, "second").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: m1,
            position_ms: 1_000,
        })
        .await
        .unwrap();
        // Someone pauses on purpose.
        h.tx.send(GroupMsg::PauseShared { sender: m1 })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await;
        while rx1.try_recv().is_ok() {}
        while rx2.try_recv().is_ok() {}

        // A late Ready trickles in (e.g. a slow player finishing its load).
        h.tx.send(GroupMsg::MemberReady {
            member_id: m2,
            position_ms: 1_000,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while let Ok(msg) = rx1.try_recv() {
            assert!(
                !matches!(msg, ServerMsg::Play { .. }),
                "a voluntary pause must stay paused"
            );
        }
        let snap = snapshot_of(&h).await;
        assert_eq!(snap.play_state, GroupPlayState::Paused);
    }

    /// Receive until `f` matches or `budget` elapses. Returns the match.
    async fn recv_matching(
        rx: &mut mpsc::Receiver<ServerMsg>,
        budget: Duration,
        f: impl Fn(&ServerMsg) -> bool,
    ) -> Option<ServerMsg> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(m)) if f(&m) => return Some(m),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    }

    #[tokio::test]
    async fn unpause_with_nobody_buffering_starts_immediately() {
        // jellyfin-web only posts /SyncPlay/Ready on a player transition, so a
        // paused idle player never ACKs a withheld Unpause. Gating here
        // deadlocked until the 30s anti-wedge — a guaranteed hang on every
        // resume. Unpause must broadcast Play immediately.
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (_m2, mut m2_rx) = add_member(&h, &sinks, "gf").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 10_000,
        })
        .await
        .unwrap();
        h.tx.send(GroupMsg::PauseShared { sender: leader })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await;
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}

        h.tx.send(GroupMsg::Unpause { sender: leader })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await;

        // Both members get the Play right away — and NO Waiting state first.
        for rx in [&mut leader_rx, &mut m2_rx] {
            let mut saw_play = false;
            while let Ok(m) = rx.try_recv() {
                match m {
                    ServerMsg::Play { position_ms, .. } => {
                        assert!(position_ms >= 10_000);
                        saw_play = true;
                    }
                    ServerMsg::StateUpdate { state, .. } => assert_ne!(
                        state,
                        GroupPlayState::Waiting,
                        "unpause of a non-buffering group must not enter Waiting"
                    ),
                    _ => {}
                }
            }
            assert!(saw_play, "unpause must broadcast Play immediately");
        }
    }

    #[tokio::test]
    async fn unpause_gates_only_on_buffering_members() {
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, &sinks, "slow").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 5_000,
        })
        .await
        .unwrap();
        // m2 buffers → group pauses.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 0,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}

        // Unpause while m2 is still buffering → gate on m2 ONLY.
        h.tx.send(GroupMsg::Unpause { sender: leader })
            .await
            .unwrap();
        let waiting = recv_matching(&mut leader_rx, Duration::from_millis(500), |m| {
            matches!(
                m,
                ServerMsg::StateUpdate {
                    state: GroupPlayState::Waiting,
                    ..
                }
            )
        })
        .await;
        assert!(waiting.is_some(), "unpause with a buffering member gates");

        // The leader never posts Ready (its player is idle-paused, no
        // transition) — only m2's Ready must resolve the gate.
        h.tx.send(GroupMsg::MemberReady {
            member_id: m2,
            position_ms: 0,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let play = recv_matching(&mut leader_rx, Duration::from_millis(500), |m| {
            matches!(m, ServerMsg::Play { .. })
        })
        .await;
        assert!(
            play.is_some(),
            "the buffering member's Ready alone must resolve the gate"
        );
    }

    #[tokio::test]
    async fn seek_broadcasts_seek_at_gate_entry_then_play_on_ready() {
        // The Seek command must be DELIVERED when the gate opens — clients
        // ACK it with Ready after re-buffering (scheduleSeek's 'ready'
        // handler). Withholding it deadlocked the gate.
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, &sinks, "gf").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 5_000,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}

        h.tx.send(GroupMsg::SeekTo {
            sender: leader,
            position_ms: 60_000,
        })
        .await
        .unwrap();
        // Both members receive the Seek immediately (gate still open).
        for rx in [&mut leader_rx, &mut m2_rx] {
            let seek = recv_matching(rx, Duration::from_millis(500), |m| {
                matches!(
                    m,
                    ServerMsg::Seek {
                        position_ms: 60_000,
                        ..
                    }
                )
            })
            .await;
            assert!(seek.is_some(), "Seek must be broadcast at gate entry");
        }
        // B38 — Readys before the Seek's scheduled at_server_ms are treated
        // as spurious transitions; wait past MIN_LEAD like a real client.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Both ACK → the gate resolves to Play (seek-while-playing resumes).
        for mid in [leader, m2] {
            h.tx.send(GroupMsg::MemberReady {
                member_id: mid,
                position_ms: 60_000,
                playlist_item_id: None,
            })
            .await
            .unwrap();
        }
        let play = recv_matching(&mut m2_rx, Duration::from_millis(500), |m| {
            matches!(
                m,
                ServerMsg::Play {
                    position_ms: 60_000,
                    ..
                }
            )
        })
        .await;
        assert!(play.is_some(), "all-Ready must resume playback at the seek");
    }

    #[tokio::test]
    async fn ignore_wait_member_is_excluded_and_releases_open_gates() {
        // A member that halted its playback (SetIgnoreWait true) must not
        // hold any gate: excluded from new pending sets, and removed from an
        // open gate (resolving it if it was the last holdout).
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, &sinks, "halted").await;
        h.tx.send(GroupMsg::SetNewQueue {
            sender: leader,
            item_ids: vec!["ep1".into()],
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}

        // Leader ACKs; the gate still waits on m2.
        h.tx.send(GroupMsg::MemberReady {
            member_id: leader,
            position_ms: 0,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        // m2 halts → its pending slot is released → gate resolves now.
        h.tx.send(GroupMsg::SetIgnoreWait {
            member_id: m2,
            ignore: true,
        })
        .await
        .unwrap();
        let play = recv_matching(&mut leader_rx, Duration::from_millis(500), |m| {
            matches!(m, ServerMsg::Play { .. })
        })
        .await;
        assert!(
            play.is_some(),
            "a halting member must release the gate it held"
        );

        // And the NEXT gate must not wait on the ignoring member at all.
        h.tx.send(GroupMsg::SeekTo {
            sender: leader,
            position_ms: 30_000,
        })
        .await
        .unwrap();
        // B38 — ACK after the Seek's scheduled time, like a real client.
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.tx.send(GroupMsg::MemberReady {
            member_id: leader,
            position_ms: 30_000,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let play = recv_matching(&mut leader_rx, Duration::from_millis(500), |m| {
            matches!(m, ServerMsg::Play { .. })
        })
        .await;
        assert!(
            play.is_some(),
            "gates must resolve without the ignore-wait member's Ready"
        );
    }

    #[tokio::test]
    async fn pause_broadcast_carries_the_frozen_position() {
        // jellyfin-web's schedulePause seeks to the command's PositionTicks —
        // a Pause without the frozen position seeks the client to 0:00 and
        // permanently desyncs it (drift correction is off by default).
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (_m2, _m2_rx) = add_member(&h, &sinks, "gf").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 10_000,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        h.tx.send(GroupMsg::PauseShared { sender: leader })
            .await
            .unwrap();
        let pause = recv_matching(&mut leader_rx, Duration::from_millis(500), |m| {
            matches!(m, ServerMsg::Pause { .. })
        })
        .await;
        match pause {
            Some(ServerMsg::Pause { position_ms, .. }) => assert!(
                (10_000..10_500).contains(&position_ms),
                "Pause must carry the freeze position, got {position_ms}"
            ),
            other => panic!("expected Pause, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ready_while_group_paused_sends_nothing() {
        // The late-Ready heal must only fire while the group is Playing. A
        // paused member is already settled; healing it with another Pause
        // would re-trigger its Ready → an endless command loop.
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, &sinks, "gf").await;
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 5_000,
        })
        .await
        .unwrap();
        h.tx.send(GroupMsg::PauseShared { sender: leader })
            .await
            .unwrap();
        let _ = snapshot_of(&h).await;
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}

        h.tx.send(GroupMsg::MemberReady {
            member_id: m2,
            position_ms: 5_000,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        assert!(
            m2_rx.try_recv().is_err(),
            "no heal commands while the group is paused"
        );
    }

    /// Pull `last_update_unix_ms` from the next `PlayQueue` on `rx`.
    async fn next_queue_ts(rx: &mut mpsc::Receiver<ServerMsg>) -> u64 {
        loop {
            match rx.recv().await.expect("expected a PlayQueue") {
                ServerMsg::PlayQueue {
                    last_update_unix_ms,
                    ..
                } => return last_update_unix_ms,
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn catch_up_reuses_stable_queue_last_update() {
        // jellyfin-web drops a PlayQueue whose LastUpdate is `<=` the one it
        // already applied. So a catch-up re-send of the SAME queue must carry
        // the SAME timestamp — otherwise the client re-processes it and restarts
        // playback (the "no active player" loop). A REAL queue change must bump.
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        h.tx.send(GroupMsg::SetNewQueue {
            sender: leader,
            item_ids: vec!["ep1".into(), "ep2".into()],
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
        let t_queue = next_queue_ts(&mut leader_rx).await;
        assert!(
            t_queue > 0,
            "a real queue change stamps a nonzero timestamp"
        );

        // A reconnecting member's catch-up must reuse that exact timestamp.
        let (gf, mut gf_rx) = add_member(&h, &sinks, "gf").await;
        let _ = snapshot_of(&h).await;
        while gf_rx.try_recv().is_ok() {}
        h.tx.send(GroupMsg::ResyncMember { member_id: gf })
            .await
            .unwrap();
        let t_catch_up = next_queue_ts(&mut gf_rx).await;
        assert_eq!(
            t_catch_up, t_queue,
            "catch-up must reuse the queue's timestamp, not stamp a fresh one"
        );

        // A genuine change (advance the cursor) must produce a STRICTLY newer
        // timestamp so the client does apply it.
        h.tx.send(GroupMsg::NextItem {
            sender: leader,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        let t_next = next_queue_ts(&mut leader_rx).await;
        assert!(
            t_next > t_queue,
            "a real queue change must bump the timestamp ({t_next} > {t_queue})"
        );
    }

    /// B37 — helper: set a 3-entry queue and return each entry's
    /// playlist_item_id (captured from the PlayQueue broadcast).
    async fn seed_queue(
        h: &GroupHandle,
        leader: MemberId,
        rx: &mut mpsc::Receiver<ServerMsg>,
    ) -> Vec<String> {
        h.tx.send(GroupMsg::SetNewQueue {
            sender: leader,
            item_ids: vec!["10".into(), "11".into(), "12".into()],
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
        let Some(ServerMsg::PlayQueue { items, .. }) =
            recv_matching(rx, Duration::from_secs(2), |m| {
                matches!(m, ServerMsg::PlayQueue { .. })
            })
            .await
        else {
            panic!("no PlayQueue broadcast after SetNewQueue");
        };
        items.into_iter().map(|i| i.playlist_item_id).collect()
    }

    /// B37 — the poisoned-gate bug behind the Code Geass ep20→21 incident:
    /// jellyfin-web posts Ready on EVERY player transition, including the OLD
    /// episode's teardown right after NextItem. A Ready naming a stale queue
    /// entry must NOT satisfy the new item's readiness gate — the sender gets
    /// a PlayQueue catch-up instead, and Play fires only once every member is
    /// ready FOR THE NEW ENTRY.
    #[tokio::test]
    async fn stale_ready_does_not_resolve_queue_change_gate() {
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, &sinks, "gf").await;
        let plis = seed_queue(&h, leader, &mut leader_rx).await;
        // Settle the initial load gate (both ready for entry 0).
        for mid in [leader, m2] {
            h.tx.send(GroupMsg::MemberReady {
                member_id: mid,
                position_ms: 0,
                playlist_item_id: Some(plis[0].clone()),
            })
            .await
            .unwrap();
        }
        let _ = snapshot_of(&h).await;
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}

        // Advance to entry 1 — a fresh gate opens on both members.
        h.tx.send(GroupMsg::NextItem {
            sender: leader,
            playlist_item_id: Some(plis[0].clone()),
        })
        .await
        .unwrap();
        // Leader's old player emits a teardown transition → STALE Ready.
        h.tx.send(GroupMsg::MemberReady {
            member_id: leader,
            position_ms: 0,
            playlist_item_id: Some(plis[0].clone()),
        })
        .await
        .unwrap();
        // m2 is genuinely ready for the NEW entry.
        h.tx.send(GroupMsg::MemberReady {
            member_id: m2,
            position_ms: 0,
            playlist_item_id: Some(plis[1].clone()),
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;

        // Leader got the queue change + a catch-up re-send, but NO Play yet —
        // its stale Ready must still be pending in the gate.
        let mut saw_catch_up = false;
        while let Ok(m) = leader_rx.try_recv() {
            match m {
                ServerMsg::Play { .. } => {
                    panic!("stale Ready must not resolve the gate (B37)")
                }
                ServerMsg::PlayQueue { reason, .. } if reason == "set_current_item" => {
                    saw_catch_up = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_catch_up,
            "a stale Ready must trigger a PlayQueue catch-up to its sender"
        );

        // Leader posts the CORRECT Ready → gate resolves → Play to everyone.
        h.tx.send(GroupMsg::MemberReady {
            member_id: leader,
            position_ms: 0,
            playlist_item_id: Some(plis[1].clone()),
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        for rx in [&mut leader_rx, &mut m2_rx] {
            let mut saw_play = false;
            while let Ok(m) = rx.try_recv() {
                if matches!(m, ServerMsg::Play { .. }) {
                    saw_play = true;
                }
            }
            assert!(
                saw_play,
                "gate must resolve once every member is ready for the new entry"
            );
        }
    }

    /// B37 — the catch-up re-send must reuse the queue's LastUpdate, so an
    /// up-to-date client's `LastUpdate <=` guard drops it as a duplicate
    /// while a behind client applies it.
    #[tokio::test]
    async fn stale_ready_catch_up_reuses_queue_timestamp() {
        let (h, _sinks, mut leader_rx, leader) = fresh().await;
        let plis = seed_queue(&h, leader, &mut leader_rx).await;
        h.tx.send(GroupMsg::NextItem {
            sender: leader,
            playlist_item_id: Some(plis[0].clone()),
        })
        .await
        .unwrap();
        let Some(ServerMsg::PlayQueue {
            last_update_unix_ms: t_change,
            ..
        }) = recv_matching(&mut leader_rx, Duration::from_secs(2), |m| {
            matches!(m, ServerMsg::PlayQueue { .. })
        })
        .await
        else {
            panic!("no queue-change broadcast");
        };
        h.tx.send(GroupMsg::MemberReady {
            member_id: leader,
            position_ms: 0,
            playlist_item_id: Some(plis[0].clone()),
        })
        .await
        .unwrap();
        let Some(ServerMsg::PlayQueue {
            last_update_unix_ms: t_catch_up,
            ..
        }) = recv_matching(&mut leader_rx, Duration::from_secs(2), |m| {
            matches!(m, ServerMsg::PlayQueue { .. })
        })
        .await
        else {
            panic!("no catch-up after stale Ready");
        };
        assert_eq!(
            t_catch_up, t_change,
            "catch-up must not mint a fresh LastUpdate (it would replay on every client)"
        );
    }

    /// B37 — a Buffering report naming a stale entry is the OLD player
    /// stalling mid-teardown; it must not freeze the whole group.
    #[tokio::test]
    async fn stale_buffering_does_not_freeze_group() {
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, &sinks, "gf").await;
        let plis = seed_queue(&h, leader, &mut leader_rx).await;
        for mid in [leader, m2] {
            h.tx.send(GroupMsg::MemberReady {
                member_id: mid,
                position_ms: 0,
                playlist_item_id: Some(plis[0].clone()),
            })
            .await
            .unwrap();
        }
        h.tx.send(GroupMsg::NextItem {
            sender: leader,
            playlist_item_id: Some(plis[0].clone()),
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}

        // m2's OLD player reports buffering for the stale entry.
        h.tx.send(GroupMsg::BufferingStart {
            member_id: m2,
            position_ms: 0,
            playlist_item_id: Some(plis[0].clone()),
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while let Ok(m) = leader_rx.try_recv() {
            assert!(
                !matches!(m, ServerMsg::Pause { .. }),
                "stale-entry buffering must not pause the group (B37)"
            );
        }
        // The stale reporter gets pulled forward instead.
        let mut saw_catch_up = false;
        while let Ok(m) = m2_rx.try_recv() {
            if matches!(&m, ServerMsg::PlayQueue { reason, .. } if reason == "set_current_item") {
                saw_catch_up = true;
            }
        }
        assert!(
            saw_catch_up,
            "stale buffering must trigger a queue catch-up"
        );
    }

    /// B37 — real-Jellyfin NextItem dedupe: the request names the entry the
    /// client believes is playing; a mismatch (double-press, or two members
    /// racing Next) must NOT advance again and skip an episode.
    #[tokio::test]
    async fn next_item_for_stale_entry_is_ignored() {
        let (h, _sinks, mut leader_rx, leader) = fresh().await;
        let plis = seed_queue(&h, leader, &mut leader_rx).await;
        h.tx.send(GroupMsg::NextItem {
            sender: leader,
            playlist_item_id: Some(plis[0].clone()),
        })
        .await
        .unwrap();
        let Some(ServerMsg::PlayQueue { playing_index, .. }) =
            recv_matching(&mut leader_rx, Duration::from_secs(2), |m| {
                matches!(m, ServerMsg::PlayQueue { .. })
            })
            .await
        else {
            panic!("first NextItem must broadcast the queue change");
        };
        assert_eq!(playing_index, 1);
        // Double-press: still names entry 0 → ignored.
        h.tx.send(GroupMsg::NextItem {
            sender: leader,
            playlist_item_id: Some(plis[0].clone()),
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while let Ok(m) = leader_rx.try_recv() {
            assert!(
                !matches!(m, ServerMsg::PlayQueue { .. }),
                "stale NextItem must not advance the queue again (B37)"
            );
        }
        // A VALID follow-up (naming the now-current entry) advances normally.
        h.tx.send(GroupMsg::NextItem {
            sender: leader,
            playlist_item_id: Some(plis[1].clone()),
        })
        .await
        .unwrap();
        let Some(ServerMsg::PlayQueue { playing_index, .. }) =
            recv_matching(&mut leader_rx, Duration::from_secs(2), |m| {
                matches!(m, ServerMsg::PlayQueue { .. })
            })
            .await
        else {
            panic!("valid NextItem must broadcast");
        };
        assert_eq!(playing_index, 2);
    }

    /// B37 — the `MemberLeft` broadcast must carry the member's display name:
    /// the wire `UserLeft` toast renders it verbatim (a uuid reached users).
    #[tokio::test]
    async fn member_left_carries_display_name() {
        let (h, sinks, mut leader_rx, _leader) = fresh().await;
        let (m2, _m2_rx) = add_member(&h, &sinks, "jana").await;
        h.tx.send(GroupMsg::RemoveMember { member_id: m2 })
            .await
            .unwrap();
        let Some(ServerMsg::MemberLeft { name, .. }) =
            recv_matching(&mut leader_rx, Duration::from_secs(2), |m| {
                matches!(m, ServerMsg::MemberLeft { .. })
            })
            .await
        else {
            panic!("no MemberLeft broadcast");
        };
        assert_eq!(name, "jana");
    }

    /// B38 — one member's pathological RTT must not schedule commands tens of
    /// seconds into the future for the whole group (observed live: a ~40s
    /// frozen seek). The RTT-derived lead is clamped.
    #[tokio::test]
    async fn seek_lead_is_clamped_against_pathological_rtt() {
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, _m2_rx) = add_member(&h, &sinks, "laggy").await;
        // rtt = (t4-t1) - (t3-t2) = 9_998ms — below the sample-discard bound,
        // far above the lead clamp. Unclamped lead would be ~5.2s.
        h.tx.send(GroupMsg::ObserveClock {
            member_id: m2,
            t1: 0,
            t2: 4_999,
            t3: 4_999,
            t4: 9_998,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        h.tx.send(GroupMsg::SeekTo {
            sender: leader,
            position_ms: 90_000,
        })
        .await
        .unwrap();
        let Some(ServerMsg::Seek { at_server_ms, .. }) =
            recv_matching(&mut leader_rx, Duration::from_secs(2), |m| {
                matches!(m, ServerMsg::Seek { .. })
            })
            .await
        else {
            panic!("no Seek broadcast");
        };
        // server_ms is measured from group creation — the whole test runs in
        // well under a second, so an unclamped lead (>5s) is unambiguous.
        assert!(
            at_server_ms < 3_500,
            "lead must be clamped (MIN_LEAD + 2s cap), got at_server_ms={at_server_ms}"
        );
    }

    /// B38 — the poisoned seek gate: jellyfin-web emits spurious player
    /// transitions (→ Ready posts) immediately after a Seek broadcast, long
    /// before any client runs its SCHEDULED seek at `When`. Those must not
    /// resolve the gate — the resulting early Play cancels the clients'
    /// pending seek callbacks and the whole group resumes from the pre-seek
    /// position.
    #[tokio::test]
    async fn ready_before_seek_schedule_time_does_not_resolve_gate() {
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let (m2, mut m2_rx) = add_member(&h, &sinks, "gf").await;
        // Give the group a real lead (~2.2s: rtt 4s → half 2s = cap).
        h.tx.send(GroupMsg::ObserveClock {
            member_id: m2,
            t1: 0,
            t2: 2_000,
            t3: 2_000,
            t4: 4_000,
        })
        .await
        .unwrap();
        // The group must be PLAYING for the seek gate to resume with Play.
        h.tx.send(GroupMsg::LeaderPlay {
            sender: leader,
            position_ms: 0,
        })
        .await
        .unwrap();
        let _ = snapshot_of(&h).await;
        while leader_rx.try_recv().is_ok() {}
        while m2_rx.try_recv().is_ok() {}
        h.tx.send(GroupMsg::SeekTo {
            sender: leader,
            position_ms: 90_000,
        })
        .await
        .unwrap();
        // Spurious instant Readys from BOTH members (well before When).
        for mid in [leader, m2] {
            h.tx.send(GroupMsg::MemberReady {
                member_id: mid,
                position_ms: 0,
                playlist_item_id: None,
            })
            .await
            .unwrap();
        }
        let _ = snapshot_of(&h).await;
        while let Ok(m) = leader_rx.try_recv() {
            assert!(
                !matches!(m, ServerMsg::Play { .. }),
                "premature Readys must not resolve a seek gate (B38)"
            );
        }
        while m2_rx.try_recv().is_ok() {}
        // Past When (lead ≈2.2s), the same Readys are legitimate ACKs.
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        for mid in [leader, m2] {
            h.tx.send(GroupMsg::MemberReady {
                member_id: mid,
                position_ms: 90_000,
                playlist_item_id: None,
            })
            .await
            .unwrap();
        }
        let got = recv_matching(&mut leader_rx, Duration::from_secs(2), |m| {
            matches!(m, ServerMsg::Play { .. })
        })
        .await;
        assert!(
            got.is_some(),
            "post-When Readys must resolve the gate and broadcast Play"
        );
    }

    /// T87 — the snapshot names the currently-playing queue entry so the
    /// HTTP seek handler can prewarm its segments.
    #[tokio::test]
    async fn snapshot_carries_current_item_id() {
        let (h, sinks, mut leader_rx, leader) = fresh().await;
        let _ = &sinks;
        assert_eq!(snapshot_of(&h).await.current_item_id, None);
        let plis = seed_queue(&h, leader, &mut leader_rx).await;
        let _ = plis;
        assert_eq!(snapshot_of(&h).await.current_item_id.as_deref(), Some("10"));
        h.tx.send(GroupMsg::NextItem {
            sender: leader,
            playlist_item_id: None,
        })
        .await
        .unwrap();
        assert_eq!(snapshot_of(&h).await.current_item_id.as_deref(), Some("11"));
    }
}
