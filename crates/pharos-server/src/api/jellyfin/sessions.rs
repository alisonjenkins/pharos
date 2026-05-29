//! Jellyfin /Sessions + /PlayState reporting.
//!
//! Routes accept the standard Jellyfin POST bodies, forward to the
//! `SessionRegistry` actor (V18) for in-memory tracking, and return
//! 204 No Content. GET /Sessions returns the actor's snapshot.

use crate::{
    api::jellyfin::auth_extractor::{auth_header_from_request, AuthUser},
    sessions::SessionEvent,
    state::AppState,
};
use actix_web::{error, web, HttpRequest, HttpResponse, Responder};
use pharos_core::{MediaStore, UserDataStore};
use serde::Deserialize;
use uuid::Uuid;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths; `LowercasePath` middleware
    // rewrites jellyfin-web's PascalCase before the router matches.
    cfg.route("/sessions", web::get().to(list_sessions))
        .route("/sessions/playing", web::post().to(playing_started))
        .route(
            "/sessions/playing/progress",
            web::post().to(playing_progress),
        )
        .route("/sessions/playing/stopped", web::post().to(playing_stopped))
        .route("/sessions/capabilities", web::post().to(capabilities))
        .route("/sessions/capabilities/full", web::post().to(capabilities))
        // Remote-control commands targeted at a specific session
        // (T40 phase 2 / T-fix-17). Jellyfin's casting / "play here"
        // UI POSTs to these paths after picking a session.
        .route(
            "/sessions/{id}/playing/{command}",
            web::post().to(session_playstate_command),
        )
        .route(
            "/sessions/{id}/command/{command}",
            web::post().to(session_general_command),
        )
        .route(
            "/sessions/{id}/playing",
            web::post().to(session_play_request),
        );
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlayingBody {
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    play_session_id: Option<String>,
    #[serde(default)]
    position_ticks: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProgressBody {
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    play_session_id: Option<String>,
    #[serde(default)]
    position_ticks: Option<u64>,
    #[serde(default)]
    is_paused: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StoppedBody {
    #[serde(default)]
    play_session_id: Option<String>,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    position_ticks: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CapabilitiesBody {
    #[serde(default)]
    #[allow(dead_code)]
    playable_media_types: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    supported_commands: Vec<String>,
}

async fn list_sessions(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    let snap = state
        .sessions
        .snapshot()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // Real Jellyfin: admins see every active session, non-admins only
    // their own. Bare `_user: AuthUser` (no filter) was a V9 leak —
    // user A could read user B's now-playing item via /Sessions.
    let filtered: Vec<_> = if user.0.policy.admin {
        snap
    } else {
        let bearer = user.0.id.0.simple().to_string();
        snap.into_iter().filter(|s| s.user_id == bearer).collect()
    };
    Ok(HttpResponse::Ok().json(filtered))
}

async fn playing_started(
    state: web::Data<AppState>,
    user: AuthUser,
    req: HttpRequest,
    body: web::Json<PlayingBody>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    let session_id = body
        .play_session_id
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let auth = auth_header_from_request(&req);
    state
        .sessions
        .apply(SessionEvent::Started {
            session_id,
            user_id: user.0.id,
            user_name: user.0.name.clone(),
            device_id: auth
                .device_id
                .clone()
                .or_else(|| auth.device.clone())
                .unwrap_or_else(|| "unknown".into()),
            device_name: auth.device_label(),
            client: auth.client_label(),
            version: auth.version_label(),
            item_id: body.item_id,
            position_ticks: body.position_ticks.unwrap_or(0),
        })
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

async fn playing_progress(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<ProgressBody>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    let position_ticks = body.position_ticks.unwrap_or(0);
    let item_id_str = body.item_id.clone();

    if let Some(session_id) = body.play_session_id.clone() {
        state
            .sessions
            .apply(SessionEvent::Progress {
                session_id: session_id.clone(),
                item_id: body.item_id,
                position_ticks,
                is_paused: body.is_paused,
            })
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        // P10 — fan out to /socket subscribers so the Currently
        // Watching sidebar updates live without polling.
        state.notify_playback_progress(
            &session_id,
            &user.0.id.0.simple().to_string(),
            &item_id_str,
            position_ticks,
            body.is_paused,
        );
    }

    // T33: persist the resume position so the Resume row picks the
    // item up across sessions. The session-snapshot path above lives
    // and dies with the in-process actor (V18) and won't survive
    // restarts.
    if let Ok(item_id) = item_id_str.parse::<pharos_core::MediaId>() {
        if let Ok(mut data) = state.stores.get_user_data(user.0.id, item_id).await {
            data.last_played_position_ticks = position_ticks;
            data.last_played_at = now_unix();
            if state
                .stores
                .set_user_data(user.0.id, item_id, data)
                .await
                .is_ok()
            {
                state.notify_user_data_changed(
                    &user.0.id.0.simple().to_string(),
                    &item_id.to_string(),
                );
            }
        }
    }
    Ok(HttpResponse::NoContent().finish())
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn playing_stopped(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<StoppedBody>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    if let Some(session_id) = body.play_session_id {
        state
            .sessions
            .apply(SessionEvent::Stopped { session_id })
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    }

    // Persist final UserData. If the client stopped within the last
    // ~10% of the runtime, treat as "finished" — mark played, reset
    // resume position. Otherwise save the position so the Resume row
    // picks the item up later. Without this the Resume row holds
    // every finished item forever (jellyfin-web only sends an
    // explicit /PlayedItems POST on manual mark-played).
    if let Some(item_id_str) = body.item_id {
        if let Ok(item_id) = item_id_str.parse::<pharos_core::MediaId>() {
            let position = body.position_ticks.unwrap_or(0);
            let runtime = state
                .stores
                .get(item_id)
                .await
                .ok()
                .and_then(|it| it.probe.run_time_ticks())
                .unwrap_or(0);
            let finished = runtime > 0 && position >= runtime.saturating_sub(runtime / 10);
            if let Ok(mut data) = state.stores.get_user_data(user.0.id, item_id).await {
                if finished {
                    data.played = true;
                    data.play_count = data.play_count.saturating_add(1);
                    data.last_played_position_ticks = 0;
                } else {
                    data.last_played_position_ticks = position;
                }
                data.last_played_at = now_unix();
                if state
                    .stores
                    .set_user_data(user.0.id, item_id, data)
                    .await
                    .is_ok()
                {
                    state.notify_user_data_changed(
                        &user.0.id.0.simple().to_string(),
                        &item_id.to_string(),
                    );
                }
            }
        }
    }
    Ok(HttpResponse::NoContent().finish())
}

async fn capabilities(
    _state: web::Data<AppState>,
    _user: AuthUser,
    _body: web::Json<CapabilitiesBody>,
) -> impl Responder {
    // Phase 1: accept + discard. Real client-capability tracking lands when
    // session-targeted commands (remote control) are needed.
    HttpResponse::NoContent().finish()
}

/// `POST /Sessions/{id}/Playing/{command}` — PlayState transport
/// commands (Pause / Play / Stop / Seek / NextTrack / PreviousTrack
/// / Rewind / FastForward / SetVolume / etc).
///
/// Body may carry a `SeekPositionTicks` (Seek) or `Volume` (SetVolume)
/// payload; we forward everything via the SessionCommand broadcast so
/// the receiving WS handler decides how to interpret it.
async fn session_playstate_command(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<(String, String)>,
    body: Option<web::Json<serde_json::Value>>,
) -> impl Responder {
    let (session_id, command) = path.into_inner();
    let arg = body
        .map(|b| b.into_inner())
        .unwrap_or(serde_json::json!({}));
    state.notify_session_command(&session_id, &canonical_command(&command), arg);
    HttpResponse::NoContent().finish()
}

/// `POST /Sessions/{id}/Command/{command}` — general commands
/// (DisplayContent, ToggleMute, ToggleFullscreen, ...). Same wire
/// shape as PlayState; the target client filters on the command name.
async fn session_general_command(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<(String, String)>,
    body: Option<web::Json<serde_json::Value>>,
) -> impl Responder {
    let (session_id, command) = path.into_inner();
    let arg = body
        .map(|b| b.into_inner())
        .unwrap_or(serde_json::json!({}));
    state.notify_session_command(&session_id, &canonical_command(&command), arg);
    HttpResponse::NoContent().finish()
}

/// Map the (already-lowercased by `LowercasePath` middleware) command
/// path component back to Jellyfin's canonical PascalCase form so the
/// broadcast `Command` field reads as the receiving client expects.
fn canonical_command(s: &str) -> String {
    match s.to_ascii_lowercase().as_str() {
        "play" => "Play",
        "pause" => "Pause",
        "unpause" => "Unpause",
        "stop" => "Stop",
        "seek" => "Seek",
        "nexttrack" => "NextTrack",
        "previoustrack" => "PreviousTrack",
        "rewind" => "Rewind",
        "fastforward" => "FastForward",
        "playpause" => "PlayPause",
        "setvolume" => "SetVolume",
        "setrepeatmode" => "SetRepeatMode",
        "setshufflequeue" => "SetShuffleQueue",
        "setaudiostreamindex" => "SetAudioStreamIndex",
        "setsubtitlestreamindex" => "SetSubtitleStreamIndex",
        "togglemute" => "ToggleMute",
        "togglefullscreen" => "ToggleFullscreen",
        "displaycontent" => "DisplayContent",
        "displaymessage" => "DisplayMessage",
        "channelup" => "ChannelUp",
        "channeldown" => "ChannelDown",
        // Fallback: title-case the first letter so unknown commands
        // still resemble Jellyfin's shape without losing the rest.
        other => return title_case(other),
    }
    .to_string()
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// `POST /Sessions/{id}/Playing` — "play these items here". Body
/// carries `ItemIds`, `StartPositionTicks`, `PlayCommand` (PlayNow /
/// PlayNext / PlayLast). Forwards verbatim as a `Play` command so
/// the receiving client kicks off playback.
async fn session_play_request(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    body: Option<web::Json<serde_json::Value>>,
) -> impl Responder {
    let session_id = path.into_inner();
    let arg = body
        .map(|b| b.into_inner())
        .unwrap_or(serde_json::json!({}));
    state.notify_session_command(&session_id, "Play", arg);
    HttpResponse::NoContent().finish()
}
