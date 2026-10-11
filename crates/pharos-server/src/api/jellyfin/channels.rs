//! Jellyfin `/Channels` controller (the SDK `ChannelsApi` jellyfin-web calls).
//!
//! Channels are a plugin concept; pharos has none, so every list is an empty
//! page and a per-channel feature lookup is a 404, matching Jellyfin's
//! `ResourceNotFoundException` for an unknown channel id. (Live TV lives
//! under `/LiveTv`, not here.)

use crate::api::jellyfin::{auth_extractor::AuthUser, wire};
use actix_web::{error, web, HttpResponse, Responder};

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/channels", web::get().to(empty_page))
        .route("/channels/features", web::get().to(all_features))
        .route("/channels/items/latest", web::get().to(empty_page))
        .route("/channels/{channel_id}/features", web::get().to(features))
        .route("/channels/{channel_id}/items", web::get().to(empty_page));
}

async fn empty_page(_user: AuthUser) -> impl Responder {
    wire::query_result::<serde_json::Value>(Vec::new(), 0, 0)
}

async fn all_features(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(Vec::<serde_json::Value>::new())
}

async fn features(_user: AuthUser, path: web::Path<String>) -> Result<HttpResponse, error::Error> {
    Err(error::ErrorNotFound(format!(
        "unknown channel {}",
        path.into_inner()
    )))
}
