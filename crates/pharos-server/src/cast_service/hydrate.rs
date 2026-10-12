//! Resuming persisted cast seats (spec 009, T042).
//!
//! A group snapshot keeps its `CastSeat` members and their Controllers, but not
//! the TV connection or the TV token. When this replica hydrates a group, the
//! service finds each seat in it and rebuilds the cast: look the TV up in the
//! config, revoke whatever token the dead process minted, connect with the
//! same grace as a lost connection, and hand the seat the group's state. A seat
//! that cannot come back is removed and its owner restored, so the roster never
//! keeps a seat nothing drives.

use pharos_core::TokenStore;
use pharos_sync::group::{CastSeatInfo, GroupHandle, GroupMsg, MemberKind};
use pharos_sync::GroupRegistry;
use tokio::sync::broadcast::error::RecvError;

use super::*;
use crate::cast::tv_device_id;

/// First pause between connection attempts to a TV that is not answering yet.
const RESUME_RETRY_START: Duration = Duration::from_millis(250);
/// Longest pause between those attempts.
const RESUME_RETRY_CAP: Duration = Duration::from_secs(2);

/// A seat being resumed while its TV connects.
pub(super) struct ResumeParams {
    group: GroupHandle,
    seat: CastSeatInfo,
    tv: ConfiguredTv,
    owner: UserId,
    owner_member: MemberId,
}

/// A resume's connect task reporting back to the actor.
pub(super) struct Resumed<T> {
    tv: String,
    outcome: OpenOutcome<T>,
}

/// Tell the actor about every cast seat in each group this replica hydrates,
/// and in the groups it already runs. Holds only a weak handle, so it never
/// keeps the service alive.
pub(super) fn spawn_listener(registry: GroupRegistry, to_actor: mpsc::WeakSender<Command>) {
    let mut hydrated = registry.subscribe_hydrated();
    tokio::spawn(async move {
        let running = registry.list().await.unwrap_or_default();
        drop(registry);
        for group in running {
            if !offer(&to_actor, group).await {
                return;
            }
        }
        loop {
            match hydrated.recv().await {
                Ok(group) => {
                    if !offer(&to_actor, group).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "cast: hydrated groups were missed; their seats resume when the group is next hydrated");
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
}

/// Hand each cast seat of `group` to the actor. `false` once the service is gone.
async fn offer(to_actor: &mpsc::WeakSender<Command>, group: GroupHandle) -> bool {
    // A remote handle does not answer: only the replica running the actor resumes.
    let Some(snapshot) = group.snapshot().await else {
        return true;
    };
    for seat in snapshot.cast_seats {
        let Some(tx) = to_actor.upgrade() else {
            return false;
        };
        let group = group.clone();
        if tx.send(Command::Resume { group, seat }).await.is_err() {
            return false;
        }
    }
    true
}

impl<F: TransportFactory> Actor<F> {
    pub(super) async fn begin_resume(&mut self, group: GroupHandle, seat: CastSeatInfo) {
        let group_label = group.group_id.to_string();
        let tv_name = seat.cast.tv.clone();
        // The same seat can be offered twice (found while running and again on
        // hydration); only act while the roster still holds it.
        let still_seated = group
            .snapshot()
            .await
            .is_some_and(|g| g.cast_seats.iter().any(|s| s.seat == seat.seat));
        if !still_seated {
            return;
        }
        let Ok(owner) = uuid::Uuid::parse_str(&seat.cast.owner).map(UserId) else {
            let detail = format!(
                "seat {} in group {group_label} names owner {:?}, which is not a user id",
                seat.seat, seat.cast.owner
            );
            return self
                .drop_seat(&group, &seat, None, CastReason::GroupGone, &detail)
                .await;
        };
        let Some(owner_member) = seat.controller else {
            let detail = format!(
                "seat {} for {tv_name:?} in group {group_label} has no Controller to restore",
                seat.seat
            );
            return self
                .drop_seat(&group, &seat, Some(owner), CastReason::GroupGone, &detail)
                .await;
        };
        if self
            .casts
            .get(&tv_name)
            .is_some_and(|c| c.seat_member == seat.seat)
            || self
                .resuming
                .get(&tv_name)
                .is_some_and(|p| p.seat.seat == seat.seat)
        {
            tracing::debug!(tv = %tv_name, group = %group_label, "cast: seat already resumed");
            return;
        }
        let Some(tv) = self.cfg.tv(&tv_name).cloned() else {
            let detail = format!(
                "{tv_name:?} is no longer in the cast config; ending the seat in group {group_label}"
            );
            return self
                .drop_seat(&group, &seat, Some(owner), CastReason::TvRemoved, &detail)
                .await;
        };
        let held_by = self
            .casts
            .get(&tv_name)
            .map(|c| (c.group.group_id, c.owner_name.clone()))
            .or_else(|| {
                self.resuming
                    .get(&tv_name)
                    .map(|p| (p.group.group_id, p.seat.name.clone()))
            });
        if let Some((other_group, other_owner)) = held_by {
            let detail = format!(
                "{tv_name:?} already carries {other_owner:?}'s seat in group {other_group}; \
                 ending the seat in group {group_label}"
            );
            return self
                .drop_seat(&group, &seat, Some(owner), CastReason::TvBusy, &detail)
                .await;
        }
        if let Some(existing) = self.by_user.get(&owner) {
            let detail = format!(
                "user {:?} already has a seat on {existing:?}; ending the seat on {tv_name:?} \
                 in group {group_label}",
                seat.name
            );
            return self
                .drop_seat(&group, &seat, Some(owner), CastReason::UserHasSeat, &detail)
                .await;
        }

        let params = ResumeParams {
            group,
            seat,
            tv: tv.clone(),
            owner,
            owner_member,
        };
        self.resuming.insert(tv_name.clone(), params);
        let factory = Arc::clone(&self.factory);
        let resumed_tx = self.resumed_tx.clone();
        let grace = Duration::from_secs(self.cfg.reconnect_grace_secs);
        tokio::spawn(async move {
            let deadline = Instant::now() + grace;
            let mut pause = RESUME_RETRY_START;
            let outcome = loop {
                let outcome = tokio::time::timeout(OPEN_TIMEOUT, factory.open(&tv)).await;
                if matches!(outcome, Ok(Ok(_))) || Instant::now() + pause > deadline {
                    break outcome;
                }
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(RESUME_RETRY_CAP);
            };
            let _ = resumed_tx
                .send(Resumed {
                    tv: tv.name,
                    outcome,
                })
                .await;
        });
    }

    pub(super) async fn on_resumed(&mut self, done: Resumed<F::Transport>) {
        let Some(params) = self.resuming.remove(&done.tv) else {
            return;
        };
        let ResumeParams {
            group,
            seat,
            tv,
            owner,
            owner_member,
        } = params;
        let group_label = group.group_id.to_string();
        let (transport, occupancy) = match done.outcome {
            Ok(Ok(opened)) => opened,
            Ok(Err(e)) => {
                let detail = format!("cannot reach {:?} at {}: {e}", tv.name, tv.address);
                return self
                    .drop_seat(
                        &group,
                        &seat,
                        Some(owner),
                        CastReason::TvUnreachable,
                        &detail,
                    )
                    .await;
            }
            Err(_) => {
                let detail = format!(
                    "{:?} at {} did not answer within {OPEN_TIMEOUT:?}",
                    tv.name, tv.address
                );
                return self
                    .drop_seat(
                        &group,
                        &seat,
                        Some(owner),
                        CastReason::TvUnreachable,
                        &detail,
                    )
                    .await;
            }
        };
        if let Occupancy::ForeignApp {
            app_id,
            display_name,
        } = occupancy
        {
            let detail = format!(
                "{:?} is running another app: {app_id} ({})",
                tv.name,
                display_name.as_deref().unwrap_or("unnamed")
            );
            return self
                .drop_seat(&group, &seat, Some(owner), CastReason::TvBusy, &detail)
                .await;
        }

        self.revoke_stale_tokens(owner, &tv.name).await;
        let token = match mint_tv_token(
            &self.deps.stores,
            owner,
            &tv.name,
            DEFAULT_TV_TOKEN_LIFETIME,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                let detail = format!("cannot mint a TV token: {e}");
                return self
                    .drop_seat(&group, &seat, Some(owner), CastReason::TokenFailed, &detail)
                    .await;
            }
        };

        let cast = self.assemble(
            transport,
            &token,
            Assembly {
                tv: &tv,
                owner,
                owner_name: &seat.name,
                owner_key: None,
                sender_device: String::new(),
                group: &group,
                owner_member,
            },
        );
        if cast.seat_member != seat.seat {
            tracing::error!(
                derived = %cast.seat_member, persisted = %seat.seat, tv = %tv.name,
                "cast: the persisted seat id is not the one derived for this owner and TV"
            );
        }
        // The roster already holds the seat; asking for the catch-up now gives
        // the driver the group's item and position to load.
        let _ = group
            .tx
            .send(GroupMsg::ResyncMember {
                member_id: cast.seat_member,
            })
            .await;
        obs::record_cast_event(
            CastEvent::Resumed,
            CastReason::UserStop,
            &seat.name,
            "",
            &tv.name,
            &group_label,
            &format!("seat {} on {}", cast.seat_member, tv.address),
        );
        self.by_user.insert(owner, tv.name.clone());
        self.casts.insert(tv.name.clone(), cast);
    }

    /// Revoke every TV token `owner` holds for `tv`: the process that minted
    /// them is gone, so none of them belongs to a live cast.
    async fn revoke_stale_tokens(&self, owner: UserId, tv: &str) {
        match self
            .deps
            .stores
            .revoke_tokens_by_device(owner, &tv_device_id(tv))
            .await
        {
            Ok(0) => {}
            Ok(revoked) => {
                tracing::info!(tv, owner = %owner.0.simple(), revoked, "cast: stale TV tokens revoked")
            }
            Err(e) => tracing::error!(
                tv, owner = %owner.0.simple(), error = %e,
                "cast: stale TV tokens could not be revoked; they expire on their own"
            ),
        }
    }

    /// End a persisted seat that cannot come back: remove it, restore its
    /// owner's player, revoke what the dead process minted, and record why.
    async fn drop_seat(
        &self,
        group: &GroupHandle,
        seat: &CastSeatInfo,
        owner: Option<UserId>,
        reason: CastReason,
        detail: &str,
    ) {
        let _ = group
            .tx
            .send(GroupMsg::RemoveMember {
                member_id: seat.seat,
            })
            .await;
        if let Some(controller) = seat.controller {
            let _ = group
                .tx
                .send(GroupMsg::SetMemberKind {
                    member: controller,
                    kind: MemberKind::Player,
                    cast: None,
                })
                .await;
        }
        if let Some(owner) = owner {
            self.revoke_stale_tokens(owner, &seat.cast.tv).await;
        }
        obs::record_cast_event(
            CastEvent::Ended,
            reason,
            &seat.name,
            "",
            &seat.cast.tv,
            &group.group_id.to_string(),
            detail,
        );
    }
}
