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
        GroupMsg::ResyncMember { member_id, user } => RemoteCommand::Resync { member_id, user },
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
            playlist_item_id,
        } => RemoteCommand::BufferingStart {
            member_id,
            position_ms,
            playlist_item_id,
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
            playlist_item_id,
            is_playing,
        } => RemoteCommand::MemberReady {
            member_id,
            position_ms,
            playlist_item_id,
            is_playing,
        },
        GroupMsg::MemberPing { member_id } => RemoteCommand::MemberPing { member_id },
        GroupMsg::SetNewQueue {
            sender,
            item_ids,
            item_runtimes_ms,
            playing_index,
            start_position_ms,
        } => RemoteCommand::SetNewQueue {
            sender,
            item_ids,
            item_runtimes_ms,
            playing_index,
            start_position_ms,
        },
        GroupMsg::Queue {
            sender,
            item_ids,
            item_runtimes_ms,
            mode,
        } => RemoteCommand::Queue {
            sender,
            item_ids,
            item_runtimes_ms,
            mode,
        },
        GroupMsg::RemoveFromPlaylist {
            sender,
            playlist_item_ids,
            clear_playlist,
            clear_playing_item,
        } => RemoteCommand::RemoveFromPlaylist {
            sender,
            playlist_item_ids,
            clear_playlist,
            clear_playing_item,
        },
        GroupMsg::MovePlaylistItem {
            sender,
            playlist_item_id,
            new_index,
        } => RemoteCommand::MovePlaylistItem {
            sender,
            playlist_item_id,
            new_index,
        },
        GroupMsg::SetPlaylistItem {
            sender,
            playlist_item_id,
        } => RemoteCommand::SetPlaylistItem {
            sender,
            playlist_item_id,
        },
        GroupMsg::NextItem {
            sender,
            playlist_item_id,
        } => RemoteCommand::NextItem {
            sender,
            playlist_item_id,
        },
        GroupMsg::PreviousItem {
            sender,
            playlist_item_id,
        } => RemoteCommand::PreviousItem {
            sender,
            playlist_item_id,
        },
        GroupMsg::SetRepeatMode { sender, mode } => RemoteCommand::SetRepeatMode { sender, mode },
        GroupMsg::SetShuffleMode { sender, mode } => RemoteCommand::SetShuffleMode { sender, mode },
        GroupMsg::SetGroupName { name } => RemoteCommand::SetGroupName { name },
        GroupMsg::SetTracks {
            sender,
            audio,
            subtitle,
            playlist_item_id,
        } => RemoteCommand::SetTracks {
            sender,
            audio,
            subtitle,
            playlist_item_id,
        },
        GroupMsg::FollowTracks { member_id, follow } => {
            RemoteCommand::FollowTracks { member_id, follow }
        }
        GroupMsg::SetIgnoreWait { member_id, ignore } => {
            RemoteCommand::SetIgnoreWait { member_id, ignore }
        }
        GroupMsg::SetMemberKind { member, kind, cast } => {
            RemoteCommand::SetMemberKind { member, kind, cast }
        }
        // Answered locally on the remote replica, never forwarded. `SeedQueue`
        // is issued only from `/SyncPlay/New`, whose caller's replica created
        // and therefore OWNS the group — so it is always applied locally and
        // never needs to cross the bus. `MemberSocketLost` is per-replica socket
        // liveness (a member's socket lives on exactly one replica), so it is
        // meaningful only where the socket dropped and is never forwarded.
        GroupMsg::AddMember { .. }
        | GroupMsg::AddCastSeat { .. }
        | GroupMsg::Snapshot { .. }
        | GroupMsg::SeedQueue { .. }
        | GroupMsg::MemberSocketLost { .. } => return None,
    })
}

/// What a replica that does not own a group knows of it from the owner's last
/// persisted snapshot, enough to fill a synthetic `Joined` truthfully.
#[derive(Debug, Clone)]
pub(crate) struct RemoteHint {
    pub(crate) group_name: String,
    pub(crate) state: crate::messages::GroupPlayState,
}

impl RemoteHint {
    /// `None` when the snapshot does not parse.
    pub(crate) fn from_snapshot(state_json: &str) -> Option<Self> {
        let summary = crate::group::snapshot_summary(state_json)?;
        Some(Self {
            group_name: summary.group_name,
            state: summary.play_state,
        })
    }
}

/// Spawn a *remote* group handle: a `GroupHandle` whose `tx` feeds a task that
/// forwards each command to the owner via `commands`. The owner's broadcasts
/// reach this replica's members over the delivery bus, so this handle only ever
/// sends upstream. `AddMember` is answered with a synthetic `Joined` (the real
/// one arrives via delivery) carrying `hint`'s name and play state; with no
/// `hint` the reply is dropped rather than filled with placeholders, so the
/// delivered `Joined` is the first the client sees. `Snapshot` (rare — the
/// list surface uses the store) gets an empty reply.
pub(crate) fn spawn_remote_handle(
    group_id: GroupId,
    epoch_unix_ms: u64,
    commands: Arc<dyn CommandSink>,
    hint: Option<RemoteHint>,
) -> GroupHandle {
    let (tx, mut rx) = mpsc::channel::<GroupMsg>(256);
    let synthetic = move |member_id: crate::messages::MemberId, name: String| {
        hint.as_ref().map(|hint| crate::group::Joined {
            group_id,
            leader: member_id,
            members: vec![crate::messages::MemberSummary {
                member_id,
                name,
                is_leader: true,
                device: None,
            }],
            group_name: hint.group_name.clone(),
            state: hint.state,
        })
    };
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            // Answer reply-carrying variants locally before/instead of
            // forwarding, so callers awaiting the reply don't hang.
            match &msg {
                GroupMsg::AddMember {
                    member_id,
                    name,
                    user,
                    ..
                } => {
                    // Forward the join, then synthesize a reply. The authoritative
                    // roster + GroupJoined arrive over delivery.
                    let (member_id, name, user) = (*member_id, name.clone(), *user);
                    if let GroupMsg::AddMember { reply, .. } = msg {
                        if let Some(joined) = synthetic(member_id, name.clone()) {
                            let _ = reply.send(joined);
                        }
                    }
                    commands.submit(
                        group_id,
                        RemoteCommand::AddMember {
                            member_id,
                            name,
                            user,
                        },
                    );
                }
                GroupMsg::AddCastSeat { .. } => {
                    if let GroupMsg::AddCastSeat {
                        member_id,
                        name,
                        cast,
                        reply,
                    } = msg
                    {
                        if let Some(joined) = synthetic(member_id, name.clone()) {
                            let _ = reply.send(joined);
                        }
                        commands.submit(
                            group_id,
                            RemoteCommand::AddCastSeat {
                                member_id,
                                name,
                                cast,
                            },
                        );
                    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::group::QueueMode;
    use crate::messages::MemberId;

    /// A command crosses the bus as JSON; it must come back as the same
    /// `GroupMsg` the owner's actor would have received locally.
    fn survives_the_bus(make: impl Fn() -> GroupMsg) {
        let cmd = to_remote_command(make()).expect("forwardable");
        let wire = serde_json::to_string(&cmd).unwrap();
        let back: RemoteCommand = serde_json::from_str(&wire).unwrap();
        assert_eq!(
            format!("{:?}", back.into_group_msg()),
            format!("{:?}", make())
        );
    }

    #[test]
    fn queue_commands_round_trip_over_the_bus() {
        let sender = MemberId::new();
        for mode in [QueueMode::Queue, QueueMode::QueueNext] {
            survives_the_bus(|| GroupMsg::Queue {
                sender,
                item_ids: vec!["10".into(), "11".into()],
                item_runtimes_ms: vec![Some(1_000), None],
                mode,
            });
        }
        survives_the_bus(|| GroupMsg::RemoveFromPlaylist {
            sender,
            playlist_item_ids: vec!["a".into(), "b".into()],
            clear_playlist: true,
            clear_playing_item: false,
        });
        survives_the_bus(|| GroupMsg::MovePlaylistItem {
            sender,
            playlist_item_id: "a".into(),
            new_index: -3,
        });
    }

    #[test]
    fn tracks_commands_round_trip_over_the_bus() {
        use crate::messages::TrackChoice;
        let sender = MemberId::new();
        survives_the_bus(|| GroupMsg::SetTracks {
            sender,
            audio: TrackChoice::Index(2),
            subtitle: TrackChoice::Off,
            playlist_item_id: Some("pli-1".into()),
        });
        survives_the_bus(|| GroupMsg::FollowTracks {
            member_id: sender,
            follow: false,
        });
    }

    /// A replica from before the item id was added forwards `SetTracks`
    /// without it; the owner takes that as "no stale check".
    #[test]
    fn a_tracks_command_without_an_item_id_still_decodes() {
        let wire = format!(
            r#"{{"SetTracks":{{"sender":"{}","audio":{{"index":2}},"subtitle":"unset"}}}}"#,
            MemberId::new()
        );
        let cmd: RemoteCommand = serde_json::from_str(&wire).unwrap();
        assert!(matches!(
            cmd.into_group_msg(),
            GroupMsg::SetTracks {
                playlist_item_id: None,
                ..
            }
        ));
    }

    struct NoCommands;
    impl CommandSink for NoCommands {
        fn submit(&self, _group_id: GroupId, _cmd: RemoteCommand) {}
    }

    async fn join_remote(
        hint: Option<RemoteHint>,
        cast: bool,
    ) -> Result<crate::group::Joined, tokio::sync::oneshot::error::RecvError> {
        let group_id = GroupId::new();
        let h = spawn_remote_handle(group_id, 0, Arc::new(NoCommands), hint);
        let (reply, rx) = tokio::sync::oneshot::channel();
        let member_id = MemberId::new();
        let msg = if cast {
            GroupMsg::AddCastSeat {
                member_id,
                name: "tv".into(),
                cast: crate::group::CastBinding {
                    tv: "tv".into(),
                    owner: "o".into(),
                },
                reply,
            }
        } else {
            GroupMsg::AddMember {
                user: None,
                member_id,
                name: "late".into(),
                reply,
            }
        };
        h.tx.send(msg).await.unwrap();
        rx.await
    }

    /// V581: a synthetic `Joined` from a remote handle carries the owner's
    /// persisted name and play state, never `""` / `Idle` placeholders.
    #[tokio::test]
    async fn a_remote_join_reply_carries_the_snapshot_name_and_state() {
        for cast in [false, true] {
            let hint = RemoteHint {
                group_name: "Movie Night".into(),
                state: crate::messages::GroupPlayState::Paused,
            };
            let joined = join_remote(Some(hint), cast).await.unwrap();
            assert_eq!(joined.group_name, "Movie Night");
            assert_eq!(joined.state, crate::messages::GroupPlayState::Paused);
        }
    }

    /// With nothing known of the group the reply is dropped, so no
    /// placeholder `Joined` is forwarded ahead of the owner's.
    #[tokio::test]
    async fn a_remote_join_reply_without_a_snapshot_is_dropped() {
        for cast in [false, true] {
            assert!(join_remote(None, cast).await.is_err());
        }
    }

    #[test]
    fn a_hint_needs_a_parseable_snapshot() {
        assert!(RemoteHint::from_snapshot("not json").is_none());
    }
}
