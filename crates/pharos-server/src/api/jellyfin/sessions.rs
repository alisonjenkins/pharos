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
use serde::Deserialize;
use uuid::Uuid;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/Sessions", web::get().to(list_sessions))
        .route("/Sessions/Playing", web::post().to(playing_started))
        .route(
            "/Sessions/Playing/Progress",
            web::post().to(playing_progress),
        )
        .route(
            "/Sessions/Playing/Stopped",
            web::post().to(playing_stopped),
        )
        .route("/Sessions/Capabilities", web::post().to(capabilities))
        .route(
            "/Sessions/Capabilities/Full",
            web::post().to(capabilities),
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
    _user: AuthUser,
    body: web::Json<ProgressBody>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    let Some(session_id) = body.play_session_id else {
        return Ok(HttpResponse::NoContent().finish());
    };
    state
        .sessions
        .apply(SessionEvent::Progress {
            session_id,
            item_id: body.item_id,
            position_ticks: body.position_ticks.unwrap_or(0),
            is_paused: body.is_paused,
        })
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
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
