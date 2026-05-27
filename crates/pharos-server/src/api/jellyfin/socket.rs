//! Jellyfin `/socket` WebSocket — multipurpose multiplex of `MessageType`
//! payloads. Phase 1 covers the SyncPlay subset so existing Jellyfin phone
//! and TV clients participate in pharos's improved group sync (V20).
//!
//! Non-SyncPlay messages (KeepAlive, Sessions, etc.) are accepted and
//! ignored — phase 2 will fan them out to the relevant subsystems.

use super::auth_extractor::AuthUser;
use super::socket_messages::{
    CommandData, GroupUpdateData, Inbound, Outbound, SyncPlayJoinData, SyncPlayPlayData,
    SyncPlaySeekData,
};
use crate::state::{AppState, SocketBroadcast};
use crate::sync::{
    group::{GroupHandle, GroupMsg, Joined},
    messages::{GroupId, MemberId, ServerMsg},
    registry::GroupRegistry,
};
use actix_web::{web, HttpRequest, HttpResponse};
use actix_ws::{AggregatedMessage, Session};
use futures_util::StreamExt;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc, oneshot};

const POSITION_TICKS_PER_MS: u64 = 10_000;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/socket", web::get().to(ws_entry));
}

async fn ws_entry(
    req: HttpRequest,
    body: web::Payload,
    state: web::Data<AppState>,
    registry: web::Data<GroupRegistry>,
    user: AuthUser,
) -> Result<HttpResponse, actix_web::Error> {
    let (response, session, stream) = actix_ws::handle(&req, body)?;
    let stream = stream
        .aggregate_continuations()
        .max_continuation_size(64 * 1024);
    let bus_rx = state.bus.subscribe();
    let user_id_str = user.0.id.0.simple().to_string();
    actix_web::rt::spawn(handle_connection(
        session,
        stream,
        registry.get_ref().clone(),
        bus_rx,
        user.0.name,
        user_id_str,
    ));
    Ok(response)
}

async fn handle_connection<S>(
    mut session: Session,
    mut stream: S,
    registry: GroupRegistry,
    mut bus_rx: broadcast::Receiver<SocketBroadcast>,
    member_name: String,
    bound_user_id: String,
) where
    S: futures_util::Stream<Item = Result<AggregatedMessage, actix_ws::ProtocolError>> + Unpin,
{
    let started = Instant::now();
    let member_id = MemberId::new();
    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(64);
    let mut current_group: Option<GroupHandle> = None;

    'pump: loop {
        tokio::select! {
            biased;
            Some(server_msg) = out_rx.recv() => {
                if let Some(out) = translate_outbound(server_msg, current_group.as_ref().map(|h| h.group_id)) {
                    if send_outbound(&mut session, &out).await.is_err() {
                        break 'pump;
                    }
                }
            }
            broadcast_msg = bus_rx.recv() => {
                // Lagged means the broadcast buffer overran this
                // subscriber. Stay connected; the next library refresh
                // will sync the client anyway.
                let Ok(b) = broadcast_msg else { continue };
                // V9: UserDataChanged is scoped to one user. Drop the
                // broadcast on this socket unless the bound bearer
                // matches — otherwise user A learns user B watched
                // item 42 (info leak across tenants).
                if let SocketBroadcast::UserDataChanged { user_id, .. } = &b {
                    if user_id != &bound_user_id {
                        continue;
                    }
                }
                if let Some(out) = translate_broadcast(b) {
                    if send_outbound(&mut session, &out).await.is_err() {
                        break 'pump;
                    }
                }
            }
            frame = stream.next() => {
                let Some(frame) = frame else { break 'pump };
                match frame {
                    Ok(AggregatedMessage::Text(txt)) => {
                        let inbound: Inbound = match serde_json::from_str(&txt) {
                            Ok(v) => v,
                            Err(_) => continue 'pump,
                        };
                        // KeepAlive: reply in-line. The Jellyfin clients
                        // close the socket after ~10 s of silence; this
                        // pong keeps it open without involving the group
                        // actor.
                        if inbound.message_type == "KeepAlive" {
                            let out = Outbound::new(
                                "KeepAlive",
                                serde_json::Value::Null,
                            );
                            if send_outbound(&mut session, &out).await.is_err() {
                                break 'pump;
                            }
                            continue 'pump;
                        }
                        handle_inbound(
                            inbound,
                            &mut current_group,
                            member_id,
                            &member_name,
                            &out_tx,
                            &registry,
                            started,
                        )
                        .await;
                    }
                    Ok(AggregatedMessage::Ping(p)) => {
                        let _ = session.pong(&p).await;
                    }
                    Ok(AggregatedMessage::Close(_)) | Err(_) => break 'pump,
                    Ok(_) => {}
                }
            }
        }
    }

    if let Some(h) = current_group.take() {
        let _ = h.tx.send(GroupMsg::RemoveMember { member_id }).await;
    }
    let _ = session.clone().close(None).await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_inbound(
    inbound: Inbound,
    current_group: &mut Option<GroupHandle>,
    member_id: MemberId,
    member_name: &str,
    out_tx: &mpsc::Sender<ServerMsg>,
    registry: &GroupRegistry,
    started: Instant,
) {
    let _ = started;
    match inbound.message_type.as_str() {
        "SyncPlayCreateGroup" => {
            if current_group.is_some() {
                return;
            }
            let handle = match registry.create().await {
                Ok(h) => h,
                Err(_) => return,
            };
            join_via_handle(handle, current_group, member_id, member_name, out_tx).await;
        }
        "SyncPlayJoinGroup" => {
            let Ok(data) = serde_json::from_value::<SyncPlayJoinData>(inbound.data) else {
                return;
            };
            let group_id = GroupId(data.group_id);
            let handle = match registry.get_or_create(group_id).await {
                Ok(h) => h,
                Err(_) => return,
            };
            join_via_handle(handle, current_group, member_id, member_name, out_tx).await;
        }
        "SyncPlayLeaveGroup" => {
            if let Some(h) = current_group.take() {
                let _ = h.tx.send(GroupMsg::RemoveMember { member_id }).await;
            }
        }
        "SyncPlayPlay" => {
            let Some(h) = current_group else { return };
            let data: SyncPlayPlayData = serde_json::from_value(inbound.data).unwrap_or(
                SyncPlayPlayData { playback_position_ticks: 0 },
            );
            let _ = h
                .tx
                .send(GroupMsg::LeaderPlay {
                    sender: member_id,
                    position_ms: data.playback_position_ticks / POSITION_TICKS_PER_MS,
                })
                .await;
        }
        "SyncPlayPause" | "SyncPlayUnpause" => {
            let Some(h) = current_group else { return };
            let _ = h
                .tx
                .send(GroupMsg::LeaderPause { sender: member_id })
                .await;
        }
        "SyncPlaySeek" => {
            let Some(h) = current_group else { return };
            let Ok(data) = serde_json::from_value::<SyncPlaySeekData>(inbound.data) else {
                return;
            };
            let _ = h
                .tx
                .send(GroupMsg::LeaderSeek {
                    sender: member_id,
                    position_ms: data.position_ticks / POSITION_TICKS_PER_MS,
                })
                .await;
        }
        "SyncPlayBuffering" => {
            let Some(h) = current_group else { return };
            let _ = h
                .tx
                .send(GroupMsg::BufferingStart {
                    member_id,
                    position_ms: 0,
                })
                .await;
        }
        "SyncPlayReady" => {
            let Some(h) = current_group else { return };
            let _ = h
                .tx
                .send(GroupMsg::BufferingEnd { member_id })
                .await;
        }
        // KeepAlive, Sessions, etc. — phase 2.
        _ => {}
    }
}

async fn join_via_handle(
    handle: GroupHandle,
    current_group: &mut Option<GroupHandle>,
    member_id: MemberId,
    member_name: &str,
    out_tx: &mpsc::Sender<ServerMsg>,
) {
    let (reply_tx, reply_rx) = oneshot::channel();
    if handle
        .tx
        .send(GroupMsg::AddMember {
            member_id,
            name: member_name.to_string(),
            sink: out_tx.clone(),
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return;
    }
    let Ok(Joined { group_id, .. }) = reply_rx.await else {
        return;
    };
    *current_group = Some(handle);
    // Emit a Jellyfin-shaped GroupJoined notification to ourself.
    let _ = out_tx
        .send(ServerMsg::Joined {
            group_id,
            leader: member_id, // local, will be corrected by actor broadcasts
            members: vec![],
        })
        .await;
}

fn translate_outbound(msg: ServerMsg, group_id: Option<GroupId>) -> Option<Outbound> {
    match msg {
        ServerMsg::Play { position_ms, .. } => Some(Outbound::new(
            "SyncPlayCommand",
            serde_json::to_value(CommandData {
                command: "Unpause",
                position_ticks: Some(position_ms * POSITION_TICKS_PER_MS),
                when: None,
            })
            .ok()?,
        )),
        ServerMsg::Pause { .. } => Some(Outbound::new(
            "SyncPlayCommand",
            serde_json::to_value(CommandData {
                command: "Pause",
                position_ticks: None,
                when: None,
            })
            .ok()?,
        )),
        ServerMsg::Seek { position_ms, .. } => Some(Outbound::new(
            "SyncPlayCommand",
            serde_json::to_value(CommandData {
                command: "Seek",
                position_ticks: Some(position_ms * POSITION_TICKS_PER_MS),
                when: None,
            })
            .ok()?,
        )),
        ServerMsg::Joined { group_id: gid, .. } => Some(Outbound::new(
            "SyncPlayGroupUpdate",
            serde_json::to_value(GroupUpdateData {
                kind: "GroupJoined",
                group_id: gid.to_string(),
            })
            .ok()?,
        )),
        ServerMsg::MemberJoined { .. } => Some(Outbound::new(
            "SyncPlayGroupUpdate",
            serde_json::to_value(GroupUpdateData {
                kind: "UserJoined",
                group_id: group_id.map(|g| g.to_string()).unwrap_or_default(),
            })
            .ok()?,
        )),
        ServerMsg::MemberLeft { .. } => Some(Outbound::new(
            "SyncPlayGroupUpdate",
            serde_json::to_value(GroupUpdateData {
                kind: "UserLeft",
                group_id: group_id.map(|g| g.to_string()).unwrap_or_default(),
            })
            .ok()?,
        )),
        ServerMsg::LeaderChange { leader } => Some(Outbound::new(
            "SyncPlayGroupUpdate",
            // Jellyfin's PlaybackAccessControl payload is `{ Type:
            // "LeaderChanged", Data: { LeaderId } }`. We keep the same
            // top-level kind ("LeaderChanged") jellyfin-web's
            // playbackManager listens for. group_id flows through as
            // GroupId so jellyfin-web's group store updates the right
            // session card; the actual `Leader` rides in `LeaderId`
            // — wire-level a string for the member uuid.
            serde_json::json!({
                "Type": "LeaderChanged",
                "GroupId": group_id.map(|g| g.to_string()).unwrap_or_default(),
                "LeaderId": leader.to_string(),
            }),
        )),
        ServerMsg::Welcome { .. } | ServerMsg::Pong { .. } | ServerMsg::Error { .. } => None,
    }
}

async fn send_outbound(session: &mut Session, msg: &Outbound) -> Result<(), actix_ws::Closed> {
    let s = serde_json::to_string(msg).map_err(|_| actix_ws::Closed)?;
    session.text(s).await
}

/// Translate a server-side `SocketBroadcast` into a Jellyfin-shaped
/// `Outbound`. T40 phase 2 — keeps the wire format identical to what
/// jellyfin-web expects when it subscribes via Sessions/LibraryChanged.
pub(crate) fn translate_broadcast(b: SocketBroadcast) -> Option<Outbound> {
    match b {
        SocketBroadcast::LibraryChanged => Some(Outbound::new(
            "LibraryChanged",
            serde_json::json!({
                "FoldersAddedTo": [],
                "FoldersRemovedFrom": [],
                "ItemsAdded": [],
                "ItemsRemoved": [],
                "ItemsUpdated": [],
                "CollectionFolders": [],
                "IsEmpty": false,
            }),
        )),
        SocketBroadcast::UserDataChanged { user_id, item_id } => Some(Outbound::new(
            "UserDataChanged",
            serde_json::json!({
                "UserId": user_id,
                "UserDataList": [{ "ItemId": item_id }],
            }),
        )),
        SocketBroadcast::SessionCommand { session_id, command, arg } => Some(Outbound::new(
            // Jellyfin uses different MessageTypes per command family.
            // The `PlayState` family covers playback transport (Play
            // /Pause/Stop/Seek/Volume) — that's what jellyfin-web's
            // session card surfaces. General commands (DisplayContent,
            // ToggleMute, ...) use `GeneralCommand`. For phase 2 we
            // route every command via PlayState since the playback
            // family is what unblocks the casting UI.
            "PlayState",
            serde_json::json!({
                "ControllingUserId": "",
                "SessionId": session_id,
                "Command": command,
                "SeekPositionTicks": arg.get("SeekPositionTicks").cloned(),
                // The remote side filters on `SessionId == own`.
            }),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn translate_library_changed_emits_libchanged_outbound() {
        let out = translate_broadcast(SocketBroadcast::LibraryChanged).unwrap();
        assert_eq!(out.message_type, "LibraryChanged");
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["MessageType"], "LibraryChanged");
        // Jellyfin's LibraryChanged payload exposes these arrays even when empty.
        assert!(v["Data"]["ItemsUpdated"].is_array());
        assert!(v["Data"]["ItemsAdded"].is_array());
    }

    #[test]
    fn translate_userdata_changed_carries_user_and_item_ids() {
        let out = translate_broadcast(SocketBroadcast::UserDataChanged {
            user_id: "u-1".into(),
            item_id: "42".into(),
        })
        .unwrap();
        assert_eq!(out.message_type, "UserDataChanged");
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["Data"]["UserId"], "u-1");
        assert_eq!(v["Data"]["UserDataList"][0]["ItemId"], "42");
    }
}
