//! Distributed-coordination hooks for the [`GroupRegistry`] (Phase B4.3d-3).
//!
//! Under Postgres a group has exactly ONE owner replica (elected by a per-group
//! advisory lock); it runs the actor and persists. Non-owner replicas hold a
//! *remote handle* whose commands are forwarded to the owner over the bus, and
//! deliver the owner's broadcasts to their own local sinks. These object-safe
//! traits inject the store/bus dependencies the registry needs without
//! pharos-sync taking a storage dependency (mirroring `Delivery` /
//! `GroupPersistence`). The server implements them over `SyncGroupStore` + the
//! bus; single-replica / SQLite deployments pass none of this and keep the plain
//! always-owned registry.

use crate::group::{GroupHandle, GroupMsg, RemoteCommand};
use crate::messages::GroupId;
use crate::persistence::GroupPersistence;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Boxed future returned by [`OwnershipSource::try_own`].
pub type OwnFuture<'a> = Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
/// Boxed future returned by [`HydrationSource::load`]: `(epoch_unix_ms, json)`.
pub type LoadFuture<'a> = Pin<Box<dyn Future<Output = Option<(u64, String)>> + Send + 'a>>;

/// Per-group ownership election. `try_own` acquires (and internally retains) the
/// group's advisory lock, returning whether this replica now owns it; `release`
/// drops the lease so another replica can take over.
pub trait OwnershipSource: Send + Sync + 'static {
    fn try_own(&self, group_id: GroupId) -> OwnFuture<'_>;
    fn release(&self, group_id: GroupId);
}

/// Loads a group's persisted snapshot for takeover, or `None` if there is
/// nothing to hydrate (a brand-new group).
pub trait HydrationSource: Send + Sync + 'static {
    fn load(&self, group_id: GroupId) -> LoadFuture<'_>;
}

/// Forwards a command from a non-owner replica to the owner over the bus
/// (fire-and-forget; serializes a `BusMsg::Command`).
pub trait CommandSink: Send + Sync + 'static {
    fn submit(&self, group_id: GroupId, cmd: RemoteCommand);
}

/// The dependency bundle a [`GroupRegistry`] needs to run in distributed mode.
#[derive(Clone)]
pub struct Distributed {
    pub ownership: Arc<dyn OwnershipSource>,
    pub hydration: Arc<dyn HydrationSource>,
    pub commands: Arc<dyn CommandSink>,
    /// Where an owned group's snapshot is persisted after each mutation.
    pub persistence: Arc<dyn GroupPersistence>,
}

/// Translate a `GroupMsg` sent to a *remote* group handle into the serializable
/// [`RemoteCommand`] forwarded to the owner. Returns `None` for the reply-
/// carrying variants a remote handle answers locally instead of forwarding
/// (`AddMember`'s synthetic reply; `Snapshot`, which the list surface reads from
/// the store).
pub(crate) fn to_remote_command(msg: GroupMsg) -> Option<RemoteCommand> {
    Some(match msg {
        GroupMsg::RemoveMember { member_id } => RemoteCommand::RemoveMember { member_id },
        GroupMsg::ResyncMember { member_id } => RemoteCommand::Resync { member_id },
        GroupMsg::LeaderPlay {
            sender,
            position_ms,
        } => RemoteCommand::LeaderPlay {
            sender,
            position_ms,
        },
        GroupMsg::LeaderPause { sender } => RemoteCommand::LeaderPause { sender },
        GroupMsg::LeaderSeek {
            sender,
            position_ms,
        } => RemoteCommand::LeaderSeek {
            sender,
            position_ms,
        },
        GroupMsg::ObserveClock {
            member_id,
            t1,
            t2,
            t3,
            t4,
        } => RemoteCommand::ObserveClock {
            member_id,
            t1,
            t2,
            t3,
            t4,
        },
        GroupMsg::BufferingStart {
            member_id,
            position_ms,
        } => RemoteCommand::BufferingStart {
            member_id,
            position_ms,
        },
        GroupMsg::BufferingEnd { member_id } => RemoteCommand::BufferingEnd { member_id },
        GroupMsg::Unpause { sender } => RemoteCommand::Unpause { sender },
        GroupMsg::PauseShared { sender } => RemoteCommand::PauseShared { sender },
        GroupMsg::SeekTo {
            sender,
            position_ms,
        } => RemoteCommand::SeekTo {
            sender,
            position_ms,
        },
        GroupMsg::MemberReady {
            member_id,
            position_ms,
        } => RemoteCommand::MemberReady {
            member_id,
            position_ms,
        },
        GroupMsg::SetNewQueue {
            sender,
            item_ids,
            playing_index,
            start_position_ms,
        } => RemoteCommand::SetNewQueue {
            sender,
            item_ids,
            playing_index,
            start_position_ms,
        },
        GroupMsg::SetPlaylistItem {
            sender,
            playlist_item_id,
        } => RemoteCommand::SetPlaylistItem {
            sender,
            playlist_item_id,
        },
        GroupMsg::NextItem { sender } => RemoteCommand::NextItem { sender },
        GroupMsg::PreviousItem { sender } => RemoteCommand::PreviousItem { sender },
        GroupMsg::SetRepeatMode { sender, mode } => RemoteCommand::SetRepeatMode { sender, mode },
        GroupMsg::SetShuffleMode { sender, mode } => RemoteCommand::SetShuffleMode { sender, mode },
        GroupMsg::SetGroupName { name } => RemoteCommand::SetGroupName { name },
        // Answered locally on the remote replica, never forwarded.
        GroupMsg::AddMember { .. } | GroupMsg::Snapshot { .. } => return None,
    })
}

/// Spawn a *remote* group handle: a `GroupHandle` whose `tx` feeds a task that
/// forwards each command to the owner via `commands`. The owner's broadcasts
/// reach this replica's members over the delivery bus, so this handle only ever
/// sends upstream. `AddMember` is answered with a synthetic `Joined` (the real
/// one arrives via delivery); `Snapshot` (rare — the list surface uses the
/// store) gets an empty reply.
pub(crate) fn spawn_remote_handle(
    group_id: GroupId,
    epoch_unix_ms: u64,
    commands: Arc<dyn CommandSink>,
) -> GroupHandle {
    let (tx, mut rx) = mpsc::channel::<GroupMsg>(256);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            // Answer reply-carrying variants locally before/instead of
            // forwarding, so callers awaiting the reply don't hang.
            match &msg {
                GroupMsg::AddMember {
                    member_id, name, ..
                } => {
                    // Forward the join, then synthesize a reply. The authoritative
                    // roster + GroupJoined arrive over delivery.
                    let (member_id, name) = (*member_id, name.clone());
                    if let GroupMsg::AddMember { reply, .. } = msg {
                        let _ = reply.send(crate::group::Joined {
                            group_id,
                            leader: member_id,
                            members: vec![crate::messages::MemberSummary {
                                member_id,
                                name: name.clone(),
                                is_leader: true,
                            }],
                        });
                    }
                    commands.submit(group_id, RemoteCommand::AddMember { member_id, name });
                }
                GroupMsg::Snapshot { .. } => {
                    // A remote handle can't answer from the actor; the list
                    // surface reads the store instead. Drop the reply (the
                    // oneshot sender is consumed as the message is dropped).
                }
                _ => {
                    if let Some(cmd) = to_remote_command(msg) {
                        commands.submit(group_id, cmd);
                    }
                }
            }
        }
    });
    GroupHandle {
        tx,
        group_id,
        epoch_unix_ms,
    }
}
