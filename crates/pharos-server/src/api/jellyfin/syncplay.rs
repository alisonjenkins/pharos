//! Jellyfin `/SyncPlay/*` HTTP surface.
//!
//! Group-watch UI in jellyfin-web fetches `GET /SyncPlay/List` the
//! moment the user clicks the group icon. Without a route there, the
//! 404 surfaces as "Error" in the UI and the panel never renders.
//!
//! Phase 1 returns an empty list + accepts the create / join / leave
//! / playback-control POSTs as 204 no-ops. The real SyncPlay protocol
//! flows over the `/socket` WebSocket (T16 / T17), so these HTTP
//! endpoints exist mostly to keep jellyfin-web's REST polling happy
//! while the WS path drives state changes. Phase 2 mirrors the full
//! Jellyfin GroupInfoDto and threads state from `GroupRegistry`.

use crate::api::jellyfin::auth_extractor::AuthUser;
use actix_web::{web, HttpResponse, Responder};

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: paths registered lowercase; `LowercasePath` middleware
    // folds jellyfin-web's PascalCase requests before routing.
    cfg.route("/syncplay/list", web::get().to(list_groups))
        .route("/syncplay/new", web::post().to(no_op_204))
        .route("/syncplay/join", web::post().to(no_op_204))
        .route("/syncplay/leave", web::post().to(no_op_204))
        .route("/syncplay/setnewqueue", web::post().to(no_op_204))
        .route("/syncplay/buffering", web::post().to(no_op_204))
        .route("/syncplay/ready", web::post().to(no_op_204))
        .route("/syncplay/pause", web::post().to(no_op_204))
        .route("/syncplay/unpause", web::post().to(no_op_204))
        .route("/syncplay/seek", web::post().to(no_op_204))
        .route("/syncplay/moveplaylistitem", web::post().to(no_op_204))
        .route("/syncplay/setignorewait", web::post().to(no_op_204))
        .route("/syncplay/nextitem", web::post().to(no_op_204))
        .route("/syncplay/previousitem", web::post().to(no_op_204))
        .route("/syncplay/setplaylistitem", web::post().to(no_op_204))
        .route("/syncplay/removefromplaylist", web::post().to(no_op_204))
        .route("/syncplay/setrepeatmode", web::post().to(no_op_204))
        .route("/syncplay/setshufflemode", web::post().to(no_op_204))
        .route("/syncplay/ping", web::post().to(no_op_204));
}

/// `GET /SyncPlay/List`. jellyfin-web expects an array of GroupInfoDto
/// shapes — empty array means "no active groups, but the feature
/// exists". Returning 404 here surfaces as a UI error and hides the
/// whole group panel.
async fn list_groups(_user: AuthUser) -> impl Responder {
    let empty: Vec<serde_json::Value> = Vec::new();
    HttpResponse::Ok().json(empty)
}

async fn no_op_204(_user: AuthUser) -> impl Responder {
    HttpResponse::NoContent().finish()
}
