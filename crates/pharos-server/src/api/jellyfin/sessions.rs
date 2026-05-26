//! Jellyfin /Sessions + /PlayState reporting.
//!
//! Routes accept the standard Jellyfin POST bodies, forward to the
//! `SessionRegistry` actor (V18) for in-memory tracking, and return
//! 204 No Content. GET /Sessions returns the actor's snapshot.

use crate::{
    api::jellyfin::auth_extractor::AuthUser,
    sessions::SessionEvent,
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::UserDataStore;
use serde::Deserialize;
use uuid::Uuid;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths; `LowercasePath` middleware
    // rewrites jellyfin-web's PascalCase before the router matches.
    cfg.route("/sessions", web::get().to(list_sessions))
        .route("/sessions/playing", web::post().to(playing_started))
        .route("/sessions/playing/progress", web::post().to(playing_progress))
        .route("/sessions/playing/stopped", web::post().to(playing_stopped))
        .route("/sessions/capabilities", web::post().to(capabilities))
        .route("/sessions/capabilities/full", web::post().to(capabilities));
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
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    let snap = state
        .sessions
        .snapshot()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::Ok().json(snap))
}

async fn playing_started(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<PlayingBody>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    let session_id = body
        .play_session_id
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    state
        .sessions
        .apply(SessionEvent::Started {
            session_id,
            user_id: user.0.id,
            user_name: user.0.name.clone(),
            device_id: "unknown".into(),
            device_name: "unknown".into(),
            client: "unknown".into(),
            version: "0".into(),
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
                session_id,
                item_id: body.item_id,
                position_ticks,
                is_paused: body.is_paused,
            })
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    }

    // T33: persist the resume position so the Resume row picks the
    // item up across sessions. The session-snapshot path above lives
    // and dies with the in-process actor (V18) and won't survive
    // restarts.
    if let Ok(item_id) = item_id_str.parse::<pharos_core::MediaId>() {
        if let Ok(mut data) = state.stores.get_user_data(user.0.id, item_id).await {
            data.last_played_position_ticks = position_ticks;
            data.last_played_at = now_unix();
            if state.stores.set_user_data(user.0.id, item_id, data).await.is_ok() {
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
    _user: AuthUser,
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
