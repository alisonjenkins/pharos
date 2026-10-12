//! Pure mapping between the group's messages and the cast driver's vocabulary.
//!
//! Nothing here performs IO or reads a clock: callers pass the server clock and
//! `Instant::now()` as a pair so every function is deterministic under test.

use std::time::Duration;

use pharos_cast::driver::{DriverEvent, GroupIntent};
use pharos_sync::messages::ServerMsg;
use tokio::time::Instant;

/// Item type sent to the receiver when the queue does not carry one: a group
/// queue holds only item ids, and the receiver treats anything playable as video.
pub const DEFAULT_ITEM_TYPE: &str = "Video";

/// A point on the group's `server_ms` clock paired with the local instant it
/// was read at, so a future `at_server_ms` becomes a local `Instant`.
#[derive(Debug, Clone, Copy)]
pub struct ClockPair {
    pub now_server_ms: u64,
    pub now: Instant,
}

impl ClockPair {
    /// The local instant of `at_server_ms`; a time already past is "now".
    pub fn instant_of(&self, at_server_ms: u64) -> Instant {
        let ahead = at_server_ms.saturating_sub(self.now_server_ms);
        self.now
            .checked_add(Duration::from_millis(ahead))
            .unwrap_or(self.now)
    }
}

fn ms(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// The intent a group message gives its TV, or `None` when the TV has nothing
/// to do for it.
///
/// - `Play` / `Pause` map one to one, at their scheduled instant.
/// - `Seek` maps to a forced seek that leaves the TV paused (`playing: false`):
///   the group follows every seek with a `Play` once its readiness gate
///   resolves, and a seek made while paused must stay paused.
/// - `PlayQueue` loads the current queue entry at the queue's start position.
///   An empty queue or an out-of-range `playing_index` yields `None`: there is
///   nothing to load, and guessing an item would cast the wrong thing.
/// - Everything else (`Welcome`, `Joined`, `Pong`, `LeaderChange`, roster
///   changes, `StateUpdate`, `Error`, `NotInGroup`, `GroupLeft`) is group
///   bookkeeping the TV cannot act on. `StateUpdate` in particular is derived
///   from the same transitions that also emit `Play` / `Pause`, so mapping it
///   too would apply every transition twice.
pub fn intent_for(msg: &ServerMsg, clock: ClockPair) -> Option<GroupIntent> {
    match msg {
        ServerMsg::Play {
            at_server_ms,
            position_ms,
        } => Some(GroupIntent::Play {
            position_ms: ms(*position_ms),
            apply_at: clock.instant_of(*at_server_ms),
        }),
        ServerMsg::Pause {
            at_server_ms,
            position_ms,
        } => Some(GroupIntent::Pause {
            position_ms: ms(*position_ms),
            apply_at: clock.instant_of(*at_server_ms),
        }),
        ServerMsg::Seek {
            at_server_ms,
            position_ms,
        } => Some(GroupIntent::Seek {
            position_ms: ms(*position_ms),
            playing: false,
            apply_at: clock.instant_of(*at_server_ms),
        }),
        ServerMsg::PlayQueue {
            items,
            playing_index,
            start_position_ms,
            is_playing,
            ..
        } => {
            let item = items.get(*playing_index)?;
            Some(GroupIntent::Load {
                item_id: item.item_id.clone(),
                item_type: DEFAULT_ITEM_TYPE.to_owned(),
                position_ms: ms(*start_position_ms),
                playing: *is_playing,
            })
        }
        ServerMsg::Welcome { .. }
        | ServerMsg::Joined { .. }
        | ServerMsg::Pong { .. }
        | ServerMsg::LeaderChange { .. }
        | ServerMsg::MemberJoined { .. }
        | ServerMsg::MemberLeft { .. }
        | ServerMsg::StateUpdate { .. }
        | ServerMsg::Error { .. }
        | ServerMsg::NotInGroup
        | ServerMsg::GroupLeft => None,
    }
}

/// The queue entry a `PlayQueue` plays, if any. A group re-sends the same queue
/// for catch-ups and repeat/shuffle changes; a caller that skips a `Load` whose
/// entry is unchanged avoids reloading the item the TV is already playing.
pub fn playing_entry(msg: &ServerMsg) -> Option<&str> {
    match msg {
        ServerMsg::PlayQueue {
            items,
            playing_index,
            ..
        } => items
            .get(*playing_index)
            .map(|i| i.playlist_item_id.as_str()),
        _ => None,
    }
}

/// A command the TV's owner issues to the group because someone used the TV's
/// own remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupCommandRequest {
    Pause,
    Unpause,
    Seek {
        position_ms: u64,
    },
    /// Not a group command: the owner turned the TV off, which ends the cast.
    StopCast,
}

impl GroupCommandRequest {
    /// Stable route-style label, the `command` label of `pharos_syncplay_command_total`.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Unpause => "unpause",
            Self::Seek { .. } => "seek",
            Self::StopCast => "stop_cast",
        }
    }
}

/// The owner command a driver event asks for. Connection-state and end events
/// are the service's own business and ask for nothing. A negative reported
/// position (a receiver before its first frame) seeks to the start.
pub fn command_for(event: &DriverEvent) -> Option<GroupCommandRequest> {
    match event {
        DriverEvent::UserPaused { .. } => Some(GroupCommandRequest::Pause),
        DriverEvent::UserResumed { .. } => Some(GroupCommandRequest::Unpause),
        DriverEvent::UserSeeked { position_ms } => Some(GroupCommandRequest::Seek {
            position_ms: u64::try_from(*position_ms).unwrap_or(0),
        }),
        DriverEvent::UserStopped => Some(GroupCommandRequest::StopCast),
        DriverEvent::State(_) | DriverEvent::Ended { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_cast::events::CastReason;
    use pharos_cast::obs::ConnState;
    use pharos_sync::messages::{
        ErrorCode, GroupId, GroupPlayState, MemberId, MemberSummary, QueueItemInfo,
    };

    fn clock() -> ClockPair {
        ClockPair {
            now_server_ms: 10_000,
            now: Instant::now(),
        }
    }

    fn queue(index: usize, items: &[(&str, &str)], pos: u64, playing: bool) -> ServerMsg {
        ServerMsg::PlayQueue {
            reason: "user_joined".into(),
            items: items
                .iter()
                .map(|(i, p)| QueueItemInfo {
                    item_id: (*i).into(),
                    playlist_item_id: (*p).into(),
                })
                .collect(),
            playing_index: index,
            start_position_ms: pos,
            is_playing: playing,
            repeat_mode: "RepeatNone".into(),
            shuffle_mode: "Sorted".into(),
            last_update_unix_ms: 1,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn play_pause_seek_are_scheduled_at_their_server_time() {
        let c = clock();
        let play = ServerMsg::Play {
            at_server_ms: 10_750,
            position_ms: 42_000,
        };
        assert_eq!(
            intent_for(&play, c),
            Some(GroupIntent::Play {
                position_ms: 42_000,
                apply_at: c.now + Duration::from_millis(750),
            })
        );
        let pause = ServerMsg::Pause {
            at_server_ms: 10_100,
            position_ms: 9,
        };
        assert_eq!(
            intent_for(&pause, c),
            Some(GroupIntent::Pause {
                position_ms: 9,
                apply_at: c.now + Duration::from_millis(100),
            })
        );
        let seek = ServerMsg::Seek {
            at_server_ms: 11_000,
            position_ms: 5_000,
        };
        assert_eq!(
            intent_for(&seek, c),
            Some(GroupIntent::Seek {
                position_ms: 5_000,
                playing: false,
                apply_at: c.now + Duration::from_millis(1_000),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_time_already_past_applies_now() {
        let c = clock();
        let late = ServerMsg::Play {
            at_server_ms: 9_000,
            position_ms: 1,
        };
        assert_eq!(
            intent_for(&late, c),
            Some(GroupIntent::Play {
                position_ms: 1,
                apply_at: c.now,
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn play_queue_loads_the_current_entry() {
        let c = clock();
        let q = queue(1, &[("a", "pa"), ("b", "pb")], 61_000, true);
        assert_eq!(
            intent_for(&q, c),
            Some(GroupIntent::Load {
                item_id: "b".into(),
                item_type: "Video".into(),
                position_ms: 61_000,
                playing: true,
            })
        );
        assert_eq!(playing_entry(&q), Some("pb"));
        let paused = queue(0, &[("a", "pa")], 0, false);
        assert!(matches!(
            intent_for(&paused, c),
            Some(GroupIntent::Load { playing: false, .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn play_queue_with_nothing_to_load_maps_to_nothing() {
        let c = clock();
        assert_eq!(intent_for(&queue(0, &[], 0, true), c), None);
        assert_eq!(playing_entry(&queue(0, &[], 0, true)), None);
        let out_of_range = queue(3, &[("a", "pa")], 0, true);
        assert_eq!(intent_for(&out_of_range, c), None);
        assert_eq!(playing_entry(&out_of_range), None);
    }

    #[tokio::test(start_paused = true)]
    async fn bookkeeping_messages_map_to_nothing() {
        let c = clock();
        let m = MemberId::new();
        let others = [
            ServerMsg::Welcome {
                member_id: m,
                server_ms: 1,
            },
            ServerMsg::Joined {
                group_id: GroupId::new(),
                leader: m,
                members: vec![],
            },
            ServerMsg::Pong {
                client_ms_echo: 1,
                server_ms: 2,
            },
            ServerMsg::LeaderChange { leader: m },
            ServerMsg::MemberJoined {
                member: MemberSummary {
                    member_id: m,
                    name: "x".into(),
                    is_leader: false,
                    device: None,
                },
            },
            ServerMsg::MemberLeft {
                member_id: m,
                name: "x".into(),
            },
            ServerMsg::StateUpdate {
                state: GroupPlayState::Playing,
                reason: "unpause".into(),
            },
            ServerMsg::Error {
                code: ErrorCode::NotLeader,
                detail: "d".into(),
            },
            ServerMsg::NotInGroup,
            ServerMsg::GroupLeft,
        ];
        for msg in &others {
            assert_eq!(intent_for(msg, c), None, "{}", msg.kind());
            assert_eq!(playing_entry(msg), None, "{}", msg.kind());
        }
    }

    #[test]
    fn user_events_become_owner_commands() {
        assert_eq!(
            command_for(&DriverEvent::UserPaused { position_ms: 5 }),
            Some(GroupCommandRequest::Pause)
        );
        assert_eq!(
            command_for(&DriverEvent::UserResumed { position_ms: 5 }),
            Some(GroupCommandRequest::Unpause)
        );
        assert_eq!(
            command_for(&DriverEvent::UserSeeked {
                position_ms: 90_000
            }),
            Some(GroupCommandRequest::Seek {
                position_ms: 90_000
            })
        );
        assert_eq!(
            command_for(&DriverEvent::UserSeeked { position_ms: -40 }),
            Some(GroupCommandRequest::Seek { position_ms: 0 })
        );
        assert_eq!(
            command_for(&DriverEvent::UserStopped),
            Some(GroupCommandRequest::StopCast)
        );
    }

    #[test]
    fn state_and_end_events_ask_for_nothing() {
        for s in ConnState::ALL {
            assert_eq!(command_for(&DriverEvent::State(s)), None);
        }
        for r in CastReason::ALL {
            assert_eq!(command_for(&DriverEvent::Ended { reason: r }), None);
        }
    }

    #[test]
    fn command_labels_are_distinct() {
        let all = [
            GroupCommandRequest::Pause,
            GroupCommandRequest::Unpause,
            GroupCommandRequest::Seek { position_ms: 0 },
            GroupCommandRequest::StopCast,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|c| c.label()).collect();
        assert_eq!(labels.len(), all.len());
    }
}
