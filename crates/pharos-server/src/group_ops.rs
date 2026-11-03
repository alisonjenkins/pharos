//! Surface-neutral SyncPlay command routing: resolve a member key to its
//! group and send the actor a `GroupMsg`, whichever surface (Jellyfin HTTP or
//! the native Cap'n Proto `Group`) the command came in on. The HTTP layer maps
//! the returned [`CommandOutcome`] to a status; the native layer maps it to a
//! result.

use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::messages::MemberId;
use pharos_sync::{GroupRegistry, SessionHub};

/// The disposition of a dispatched SyncPlay command — the observability
/// contract for `pharos_syncplay_command_total`. Bounded, stable strings from
/// [`CommandOutcome::label`] (a renamed label breaks the dashboard silently);
/// asserted distinct in a test. Every path through [`dispatch`] records exactly
/// one of these, so a dropped command is never invisible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CommandOutcome {
    /// Applied via the caller's live `/socket` session — the normal path.
    Dispatched,
    /// No live socket (a fresh pod within the post-deploy reconnect gap), but
    /// the caller's persisted membership named a group — applied there. The
    /// caller's own view heals via catch-up when its `/socket` reconnects (B24).
    Recovered,
    /// A live socket, but the session is in no group AND no persisted group
    /// claims the member — the client is told `NotInGroup`.
    NotInGroup,
    /// No live socket AND no persisted membership — nothing to apply to.
    DroppedNoSocket,
    /// The request carried no `deviceId` at all.
    NoDevice,
    /// The session's group handle pointed at an actor that has terminated
    /// (the group emptied while this socket was between reconnects). The
    /// handle is detached and the client told `NotInGroup`.
    ActorDown,
    /// The group was found but the command's check refused it (a queue
    /// change a member may not see, V201); nothing was applied.
    Refused,
}

impl CommandOutcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Dispatched => "dispatched",
            Self::Recovered => "recovered",
            Self::NotInGroup => "not_in_group",
            Self::DroppedNoSocket => "dropped_no_socket",
            Self::NoDevice => "no_device",
            Self::ActorDown => "actor_down",
            Self::Refused => "refused",
        }
    }
}

/// Count one command dispatch. `command` is the fixed route label (pause, seek,
/// …), so the label set is bounded on both axes.
pub(crate) fn record_command(command: &str, outcome: CommandOutcome) {
    metrics::counter!(
        "pharos_syncplay_command_total",
        "command" => command.to_string(),
        "outcome" => outcome.label(),
    )
    .increment(1);
}

/// What [`recover_and_apply`] did with a command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Recovery {
    /// No persisted group claims the member, or the group could not be hydrated.
    NotFound,
    /// The group was found but `check` refused the command; nothing was sent.
    Refused,
    /// The command was sent to the group.
    Applied,
}

/// Apply a command via the caller's PERSISTED membership when there is no live
/// socket to resolve it through — the deploy-reconnect gap. The device's member
/// id is deterministic ([`member_id_for_device`]), so the persisted snapshots
/// still name its group; `get_or_create` re-owns/hydrates it (ownership-aware
/// under Postgres) and the command applies to group state. The caller receives
/// nothing now — it has no socket here — but its own player already performed
/// the action locally (jellyfin-web pauses/seeks first, then POSTs), and its
/// view reconciles via catch-up the instant its `/socket` reconnects (B24).
/// Says whether a group was found and, if so, whether the command was sent.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover_and_apply(
    hub: &SessionHub,
    registry: &GroupRegistry,
    stores: &crate::state::Stores,
    member_id: MemberId,
    device_id: &str,
    label: &str,
    check: impl AsyncFnOnce(&GroupHandle) -> bool,
    make: impl FnOnce(MemberId) -> GroupMsg,
) -> Recovery {
    let Some(gid) = crate::sync_recovery::find_persisted_group(stores, member_id).await else {
        return Recovery::NotFound;
    };
    let h = match registry.get_or_create(gid).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                command = label, device_id = %device_id, group = %gid, error = %e,
                "syncplay: recovery get_or_create failed — command dropped"
            );
            return Recovery::NotFound;
        }
    };
    tracing::info!(
        command = label, device_id = %device_id, %member_id, group = %gid,
        "syncplay: command applied from persisted membership (no live socket — deploy reconnect gap)"
    );
    // Re-attach the session too, when it has one: until the socket's own
    // B24 re-attach lands, every message this group delivers to the socket
    // would be translated with no group id and a zero epoch (When = 1970).
    hub.attach_group(device_id, h.clone());
    if !check(&h).await {
        record_command(label, CommandOutcome::Refused);
        return Recovery::Refused;
    }
    let _ = h.tx.send(make(member_id)).await;
    record_command(label, CommandOutcome::Recovered);
    Recovery::Applied
}

/// Resolve a device to its socket session, briefly RETRYING to close a race:
/// jellyfin-web opens `/socket` and then immediately POSTs `/SyncPlay/New` (or
/// Join), but the socket's hub registration runs in a spawned task that can lag
/// the HTTP handler by tens of ms. A single resolve then misses, the member is
/// never added, the fresh group dies member-less, the client never receives
/// GroupJoined, and its `enableSyncPlay → SetNewQueue → play` flow stalls with
/// nothing ever playing (B54, live-observed). Waits for the registration up to
/// [`SOCKET_REGISTRATION_WAIT`]; normally resolves on the first try.
pub(crate) async fn resolve_with_retry(
    hub: &SessionHub,
    device_id: &str,
) -> Option<pharos_sync::hub::ResolvedSession> {
    hub.resolve_wait(device_id, SOCKET_REGISTRATION_WAIT).await
}

/// How long an HTTP SyncPlay call waits for its device's `/socket` to register.
pub(crate) const SOCKET_REGISTRATION_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Resolve `key` (the member's SyncPlay session key) to its group and send it
/// a `GroupMsg` built from the member id, applying the command only when
/// `check` passes for the group it resolved to, on every path (live session
/// or persisted recovery). A command that finds no live socket is NOT
/// dropped: it is recovered against the caller's persisted membership (the
/// rolling-deploy wedge of 2026-07-26, where next/unpause/seek issued during
/// the socket-reconnect gap were silently lost). Every terminal path records
/// exactly one [`CommandOutcome`] and returns it.
pub(crate) async fn route_command(
    hub: &SessionHub,
    registry: &GroupRegistry,
    stores: &crate::state::Stores,
    key: Option<&str>,
    label: &str,
    check: impl AsyncFnOnce(&GroupHandle) -> bool,
    make: impl FnOnce(MemberId) -> GroupMsg,
) -> CommandOutcome {
    let Some(dev) = key else {
        tracing::warn!(
            command = label,
            "syncplay: command with no deviceId — dropped"
        );
        record_command(label, CommandOutcome::NoDevice);
        return CommandOutcome::NoDevice;
    };
    // B54 — resolve WITH RETRY: the group-forming commands (SetNewQueue
    // especially — it hands the group its media) can arrive in the same
    // open-socket-then-POST race as New/Join. A member with a live socket
    // resolves on the first try (no added latency); only the race window or a
    // genuine disconnect waits.
    match resolve_with_retry(hub, dev).await {
        Some(sess) => match sess.group {
            Some(h) => {
                if !check(&h).await {
                    record_command(label, CommandOutcome::Refused);
                    return CommandOutcome::Refused;
                }
                if h.tx.send(make(sess.member_id)).await.is_err() {
                    // The actor is gone (the group emptied while this socket
                    // was between reconnects) but the hub still pointed at
                    // it: every command was a 204 that applied nothing. Drop
                    // the dead handle and make the exit visible client-side.
                    tracing::warn!(
                        command = label, device_id = %dev, group = %h.group_id,
                        "syncplay: group actor is gone — detaching session and telling client NotInGroup"
                    );
                    hub.detach_group(dev);
                    let _ = sess
                        .sink
                        .send(pharos_sync::messages::ServerMsg::NotInGroup)
                        .await;
                    record_command(label, CommandOutcome::ActorDown);
                    return CommandOutcome::ActorDown;
                }
                tracing::info!(command = label, device_id = %dev, group = %h.group_id, "syncplay: command dispatched");
                record_command(label, CommandOutcome::Dispatched);
                CommandOutcome::Dispatched
            }
            // A live socket but no attached group here: its B24 re-attach may
            // not have run yet, so try persisted recovery before giving up.
            None => {
                match recover_and_apply(
                    hub,
                    registry,
                    stores,
                    sess.member_id,
                    dev,
                    label,
                    check,
                    make,
                )
                .await
                {
                    // `recover_and_apply` recorded Recovered or Refused itself.
                    Recovery::Applied => CommandOutcome::Recovered,
                    Recovery::Refused => CommandOutcome::Refused,
                    Recovery::NotFound => {
                        tracing::warn!(
                            command = label, device_id = %dev,
                            "syncplay: session is in no group — telling client NotInGroup"
                        );
                        // B24 — never drop silently: NotInGroup makes stock
                        // jellyfin-web disable SyncPlay visibly instead of
                        // desyncing one-sidedly.
                        let _ = sess
                            .sink
                            .send(pharos_sync::messages::ServerMsg::NotInGroup)
                            .await;
                        record_command(label, CommandOutcome::NotInGroup);
                        CommandOutcome::NotInGroup
                    }
                }
            }
        },
        // No live socket — the fresh-pod-after-deploy window. Recover from the
        // persisted membership instead of dropping (was: silent WARN + drop).
        None => {
            let member_id = pharos_sync::hub::member_id_for_device(dev);
            match recover_and_apply(hub, registry, stores, member_id, dev, label, check, make).await
            {
                Recovery::Applied => CommandOutcome::Recovered,
                Recovery::Refused => CommandOutcome::Refused,
                Recovery::NotFound => {
                    tracing::warn!(
                        command = label, device_id = %dev,
                        "syncplay: no /socket registered and no persisted group — command dropped"
                    );
                    record_command(label, CommandOutcome::DroppedNoSocket);
                    CommandOutcome::DroppedNoSocket
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::state::Stores;
    use pharos_core::{PersistedSyncGroup, SyncGroupStore};
    use pharos_sync::delivery::LocalDelivery;
    use pharos_sync::hub::member_id_for_device;
    use pharos_sync::messages::GroupId;
    use pharos_sync::persistence::GroupPersistence;
    use pharos_sync::MemberSinks;
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot};

    struct Capture(std::sync::Mutex<Option<String>>);

    impl GroupPersistence for Capture {
        fn persist(&self, _g: GroupId, _e: u64, state_json: String) {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state_json);
        }
        fn remove(&self, _g: GroupId) {}
    }

    /// A store holding a fresh persisted group whose roster is `device`'s
    /// member, and a registry that can hydrate it.
    async fn persisted_group_of(device: &str) -> (Stores, GroupRegistry) {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let capture = Arc::new(Capture(std::sync::Mutex::new(None)));
        let group_id = GroupId::new();
        let actor = GroupHandle::spawn_persistent(
            group_id,
            1_000,
            Arc::new(LocalDelivery::new(MemberSinks::new())),
            capture.clone(),
            None,
        );
        let (reply, joined) = oneshot::channel();
        actor
            .tx
            .send(GroupMsg::AddMember {
                user: None,
                member_id: member_id_for_device(device),
                name: "m".into(),
                reply,
            })
            .await
            .unwrap();
        joined.await.unwrap();
        let state_json = loop {
            let json = capture
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(json) = json {
                break json;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        let now = crate::sync_recovery::now_unix_secs();
        let row = PersistedSyncGroup {
            group_id: group_id.to_string(),
            epoch_unix_ms: 1_000,
            state_json,
            updated_at: now,
        };
        stores.upsert_sync_group(&row, now).await.unwrap();
        let registry = GroupRegistry::spawn(Arc::new(LocalDelivery::new(MemberSinks::new())));
        (stores, registry)
    }

    /// A persisted group that refuses the command is `Refused`, not
    /// `Applied`, on both the live-session and the no-socket route.
    /// Disarm check: with `recover_and_apply` answering `Applied` after a
    /// refusal, both assertions see `Recovered`.
    #[actix_web::test]
    async fn a_refused_recovered_command_is_refused() {
        let (stores, registry) = persisted_group_of("dev").await;
        let hub = SessionHub::new();

        let no_socket = route_command(
            &hub,
            &registry,
            &stores,
            Some("dev"),
            "pause",
            async |_| false,
            |sender| GroupMsg::PauseShared { sender },
        )
        .await;
        assert_eq!(no_socket, CommandOutcome::Refused);

        let (tx, _rx) = mpsc::channel(8);
        hub.register("dev".into(), "m".into(), tx);
        let live = route_command(
            &hub,
            &registry,
            &stores,
            Some("dev"),
            "pause",
            async |_| false,
            |sender| GroupMsg::PauseShared { sender },
        )
        .await;
        assert_eq!(live, CommandOutcome::Refused);
    }

    #[actix_web::test]
    async fn an_accepted_recovered_command_is_recovered() {
        let (stores, registry) = persisted_group_of("dev").await;
        let outcome = route_command(
            &SessionHub::new(),
            &registry,
            &stores,
            Some("dev"),
            "pause",
            async |_| true,
            |sender| GroupMsg::PauseShared { sender },
        )
        .await;
        assert_eq!(outcome, CommandOutcome::Recovered);
    }
}
