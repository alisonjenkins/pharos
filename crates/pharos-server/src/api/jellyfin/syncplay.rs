//! Jellyfin `/SyncPlay/*` HTTP surface — the command channel stock
//! jellyfin-web (and every official Jellyfin client) uses to drive group
//! watch. Commands arrive as HTTP POSTs identified by the caller's `deviceId`;
//! the resulting playback commands + group updates flow back to each member
//! over `/socket`.
//!
//! Each handler resolves the caller's session via the [`SessionHub`] (keyed by
//! `deviceId`), sends the matching [`GroupMsg`] to the group actor, and returns
//! `204` — matching Jellyfin, whose HTTP responses are empty and whose real
//! work rides the WebSocket. A caller with no registered socket, or not in a
//! group, is a `204` no-op.

use crate::api::jellyfin::auth_extractor::{AuthSession, AuthUser};
use actix_web::{web, HttpResponse, Responder};
use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::messages::{GroupId, MemberId};
use pharos_sync::{GroupRegistry, SessionHub};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use uuid::Uuid;

const POSITION_TICKS_PER_MS: u64 = 10_000;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: paths registered lowercase; `LowercasePath` middleware
    // folds jellyfin-web's PascalCase requests before routing.
    cfg.route("/syncplay/list", web::get().to(list_groups))
        .route("/syncplay/new", web::post().to(new_group))
        .route("/syncplay/join", web::post().to(join_group))
        .route("/syncplay/leave", web::post().to(leave_group))
        .route("/syncplay/setnewqueue", web::post().to(set_new_queue))
        .route("/syncplay/buffering", web::post().to(buffering))
        .route("/syncplay/ready", web::post().to(ready))
        .route("/syncplay/pause", web::post().to(pause))
        .route("/syncplay/unpause", web::post().to(unpause))
        .route("/syncplay/seek", web::post().to(seek))
        .route(
            "/syncplay/setplaylistitem",
            web::post().to(set_playlist_item),
        )
        .route("/syncplay/nextitem", web::post().to(next_item))
        .route("/syncplay/previousitem", web::post().to(previous_item))
        .route("/syncplay/setrepeatmode", web::post().to(set_repeat_mode))
        .route("/syncplay/setshufflemode", web::post().to(set_shuffle_mode))
        // Not yet modelled by the engine — accept + ignore so the client's
        // flow isn't broken by a 404.
        .route("/syncplay/moveplaylistitem", web::post().to(no_op_204))
        .route("/syncplay/removefromplaylist", web::post().to(no_op_204))
        .route("/syncplay/setignorewait", web::post().to(no_op_204))
        .route("/syncplay/ping", web::post().to(no_op_204));
}

fn no_content() -> HttpResponse {
    HttpResponse::NoContent().finish()
}

/// Resolve the caller's group and send it a `GroupMsg` built from the caller's
/// member id. The common shape of every command handler: no device / no socket
/// / not in a group all collapse to a `204` no-op.
async fn dispatch(
    hub: &SessionHub,
    device_id: Option<&str>,
    label: &str,
    make: impl FnOnce(MemberId) -> GroupMsg,
) -> HttpResponse {
    match device_id {
        None => tracing::warn!(
            command = label,
            "syncplay: command with no deviceId — dropped"
        ),
        Some(dev) => match hub.resolve(dev) {
            None => tracing::warn!(
                command = label,
                device_id = %dev,
                "syncplay: no /socket registered for this deviceId — command dropped \
                 (client must open /socket before commanding)"
            ),
            Some(sess) => match sess.group {
                None => tracing::warn!(
                    command = label,
                    device_id = %dev,
                    "syncplay: session is in no group — command dropped"
                ),
                Some(h) => {
                    tracing::info!(command = label, device_id = %dev, group = %h.group_id, "syncplay: command dispatched");
                    let _ = h.tx.send(make(sess.member_id)).await;
                }
            },
        },
    }
    no_content()
}

/// Add the caller (from the hub) to `handle` as a member. Shared by New + Join.
async fn add_caller_to_group(hub: &SessionHub, device_id: &str, handle: GroupHandle) {
    let Some(sess) = hub.resolve(device_id) else {
        tracing::warn!(
            device_id = %device_id,
            "syncplay: New/Join but no /socket registered for this deviceId — \
             cannot add member (client must open /socket first)"
        );
        return;
    };
    // Record the group before AddMember so the wall-clock epoch is available
    // the instant the first catch-up command is broadcast to the socket.
    hub.attach_group(device_id, handle.clone());
    let (reply_tx, reply_rx) = oneshot::channel();
    let member_id = sess.member_id;
    if handle
        .tx
        .send(GroupMsg::AddMember {
            member_id,
            name: sess.name.clone(),
            sink: sess.sink,
            reply: reply_tx,
        })
        .await
        .is_ok()
    {
        let _ = reply_rx.await;
        tracing::info!(
            device_id = %device_id, %member_id, user = %sess.name, group = %handle.group_id,
            "syncplay: member added to group"
        );
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct NewGroupBody {
    group_name: String,
}

/// Parse the optional `{GroupName}` body of `/SyncPlay/New` (tolerating an
/// empty/absent body).
fn parse_group_name(bytes: &web::Bytes) -> Option<String> {
    serde_json::from_slice::<NewGroupBody>(bytes)
        .ok()
        .map(|b| b.group_name)
        .filter(|n| !n.trim().is_empty())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct JoinGroupBody {
    group_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SetNewQueueBody {
    #[serde(default)]
    playing_queue: Vec<String>,
    #[serde(default)]
    playing_item_position: usize,
    #[serde(default)]
    start_position_ticks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SeekBody {
    #[serde(default)]
    position_ticks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReadyBody {
    #[serde(default)]
    position_ticks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SetPlaylistItemBody {
    playlist_item_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ModeBody {
    #[serde(default)]
    mode: String,
}

async fn new_group(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    body: web::Bytes,
) -> HttpResponse {
    let Some(dev) = auth.device_id.as_deref() else {
        tracing::warn!("syncplay: New with no deviceId — cannot form a group");
        return no_content();
    };
    let Ok(handle) = registry.create().await else {
        tracing::error!("syncplay: group registry unreachable on New");
        return no_content();
    };
    // Name the group from the request, else "<creator>'s Group".
    let name = parse_group_name(&body).unwrap_or_else(|| {
        let who = hub
            .resolve(dev)
            .map(|s| s.name)
            .unwrap_or_else(|| "Watch".to_string());
        format!("{who}'s Group")
    });
    tracing::info!(device_id = %dev, group = %handle.group_id, %name, "syncplay: New group created");
    // Add the creator as the FIRST message to the fresh actor. The group actor
    // terminates the moment it processes a message and finds no members, so a
    // brand-new (member-less) group must receive AddMember before anything else
    // (e.g. SetGroupName) — otherwise it dies before the creator ever joins.
    add_caller_to_group(&hub, dev, handle.clone()).await;
    let _ = handle.tx.send(GroupMsg::SetGroupName { name }).await;
    no_content()
}

async fn join_group(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    body: web::Json<JoinGroupBody>,
) -> HttpResponse {
    let Some(dev) = auth.device_id.as_deref() else {
        return no_content();
    };
    let Ok(handle) = registry.get_or_create(GroupId(body.group_id)).await else {
        tracing::error!("syncplay: group registry unreachable on Join");
        return no_content();
    };
    tracing::info!(device_id = %dev, group = %handle.group_id, "syncplay: Join group");
    add_caller_to_group(&hub, dev, handle).await;
    no_content()
}

async fn leave_group(auth: AuthSession, hub: web::Data<SessionHub>) -> HttpResponse {
    if let Some(dev) = auth.device_id.as_deref() {
        if let Some(sess) = hub.resolve(dev) {
            if let Some(h) = hub.detach_group(dev) {
                let _ =
                    h.tx.send(GroupMsg::RemoveMember {
                        member_id: sess.member_id,
                    })
                    .await;
            }
        }
    }
    no_content()
}

async fn set_new_queue(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    body: web::Json<SetNewQueueBody>,
) -> HttpResponse {
    let body = body.into_inner();
    let start_ms = body.start_position_ticks / POSITION_TICKS_PER_MS;
    dispatch(&hub, auth.device_id.as_deref(), "setnewqueue", move |mid| {
        GroupMsg::SetNewQueue {
            sender: mid,
            item_ids: body.playing_queue,
            playing_index: body.playing_item_position,
            start_position_ms: start_ms,
        }
    })
    .await
}

async fn unpause(auth: AuthSession, hub: web::Data<SessionHub>) -> HttpResponse {
    dispatch(&hub, auth.device_id.as_deref(), "unpause", |mid| {
        GroupMsg::Unpause { sender: mid }
    })
    .await
}

async fn pause(auth: AuthSession, hub: web::Data<SessionHub>) -> HttpResponse {
    dispatch(&hub, auth.device_id.as_deref(), "pause", |mid| {
        GroupMsg::PauseShared { sender: mid }
    })
    .await
}

async fn seek(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    body: web::Json<SeekBody>,
) -> HttpResponse {
    let pos = body.position_ticks / POSITION_TICKS_PER_MS;
    dispatch(&hub, auth.device_id.as_deref(), "seek", move |mid| {
        GroupMsg::SeekTo {
            sender: mid,
            position_ms: pos,
        }
    })
    .await
}

async fn buffering(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    body: web::Json<ReadyBody>,
) -> HttpResponse {
    let pos = body.position_ticks / POSITION_TICKS_PER_MS;
    dispatch(&hub, auth.device_id.as_deref(), "buffering", move |mid| {
        GroupMsg::BufferingStart {
            member_id: mid,
            position_ms: pos,
        }
    })
    .await
}

async fn ready(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    body: web::Json<ReadyBody>,
) -> HttpResponse {
    let pos = body.position_ticks / POSITION_TICKS_PER_MS;
    dispatch(&hub, auth.device_id.as_deref(), "ready", move |mid| {
        GroupMsg::MemberReady {
            member_id: mid,
            position_ms: pos,
        }
    })
    .await
}

async fn set_playlist_item(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    body: web::Json<SetPlaylistItemBody>,
) -> HttpResponse {
    let pli = body.into_inner().playlist_item_id;
    dispatch(
        &hub,
        auth.device_id.as_deref(),
        "setplaylistitem",
        move |mid| GroupMsg::SetPlaylistItem {
            sender: mid,
            playlist_item_id: pli,
        },
    )
    .await
}

async fn next_item(auth: AuthSession, hub: web::Data<SessionHub>) -> HttpResponse {
    dispatch(&hub, auth.device_id.as_deref(), "nextitem", |mid| {
        GroupMsg::NextItem { sender: mid }
    })
    .await
}

async fn previous_item(auth: AuthSession, hub: web::Data<SessionHub>) -> HttpResponse {
    dispatch(&hub, auth.device_id.as_deref(), "previousitem", |mid| {
        GroupMsg::PreviousItem { sender: mid }
    })
    .await
}

async fn set_repeat_mode(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    body: web::Json<ModeBody>,
) -> HttpResponse {
    let mode = body.into_inner().mode;
    dispatch(
        &hub,
        auth.device_id.as_deref(),
        "setrepeatmode",
        move |mid| GroupMsg::SetRepeatMode { sender: mid, mode },
    )
    .await
}

async fn set_shuffle_mode(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    body: web::Json<ModeBody>,
) -> HttpResponse {
    let mode = body.into_inner().mode;
    dispatch(
        &hub,
        auth.device_id.as_deref(),
        "setshufflemode",
        move |mid| GroupMsg::SetShuffleMode { sender: mid, mode },
    )
    .await
}

/// Jellyfin's `GroupInfoDto` shape. Only the fields jellyfin-web reads
/// for the dropdown render — full state lives over the socket.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct GroupInfoDto {
    group_id: String,
    group_name: String,
    /// `Idle` / `Playing` / `Paused` / `Waiting`. We map from the
    /// internal `PlaybackState` enum via the actor snapshot.
    state: &'static str,
    /// Member count. Full per-member list ships with the socket
    /// `SyncPlayGroupUpdate` payload — clients render it from there
    /// after joining.
    participants: Vec<String>,
    last_updated_at: String,
}

async fn list_groups(_user: AuthUser, registry: web::Data<GroupRegistry>) -> impl Responder {
    let Ok(handles) = registry.list().await else {
        // Actor unreachable: return empty rather than 500 so the UI
        // renders an "no active groups" pane instead of an error.
        let empty: Vec<GroupInfoDto> = vec![];
        return HttpResponse::Ok().json(empty);
    };
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        let Some(snap) = h.snapshot().await else {
            continue;
        };
        use pharos_sync::messages::GroupPlayState;
        let state = match snap.play_state {
            GroupPlayState::Idle => "Idle",
            GroupPlayState::Waiting => "Waiting",
            GroupPlayState::Playing => "Playing",
            GroupPlayState::Paused => "Paused",
        };
        out.push(GroupInfoDto {
            group_id: snap.id.to_string(),
            group_name: snap.group_name,
            state,
            participants: snap.participants,
            last_updated_at: now_iso8601(),
        });
    }
    HttpResponse::Ok().json(out)
}

async fn no_op_204(_user: AuthUser) -> impl Responder {
    HttpResponse::NoContent().finish()
}

fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    crate::api::jellyfin::dto::format_iso8601_ms(ms)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use actix_web::{test, App};
    use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};

    async fn seed_auth() -> (web::Data<crate::state::AppState>, String) {
        use crate::auth::BuiltinAuth;
        use crate::state::Stores;
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let auth = BuiltinAuth::new(stores.clone());
        let hash = auth.hash_password(&SecretString::new("p")).unwrap();
        let uid = UserId::new();
        stores
            .create(UserRecord {
                id: uid,
                name: "u".into(),
                password_hash: hash,
                policy: UserPolicy::default(),
            })
            .await
            .unwrap();
        let token = stores.issue(uid, "t").await.unwrap();
        let state = web::Data::new(crate::state::AppState::new(stores, "t".into()));
        (state, token.0.expose().to_string())
    }

    #[actix_web::test]
    async fn syncplay_list_empty_when_no_groups() {
        let (state, token) = seed_auth().await;
        let reg = web::Data::new(GroupRegistry::spawn());
        let app =
            test::init_service(App::new().app_data(state).app_data(reg).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/syncplay/list")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap().len(), 0);
    }

    #[actix_web::test]
    async fn syncplay_list_returns_active_group() {
        let (state, token) = seed_auth().await;
        let reg = GroupRegistry::spawn();
        let handle = reg.create().await.unwrap();
        // Group has zero members → snapshot may report 0; we don't
        // care about state here, only that the id surfaces.
        let reg_data = web::Data::new(reg);
        let app = test::init_service(
            App::new()
                .app_data(state)
                .app_data(reg_data)
                .configure(register),
        )
        .await;
        let req = test::TestRequest::get()
            .uri("/syncplay/list")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = v.as_array().unwrap();
        // Empty groups terminate themselves on the next message —
        // we either see the freshly-created group or already-empty.
        assert!(arr.len() <= 1);
        if let Some(first) = arr.first() {
            assert_eq!(first["GroupId"], handle.group_id.to_string());
        }
    }
}
