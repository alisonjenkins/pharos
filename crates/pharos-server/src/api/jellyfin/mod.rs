//! Jellyfin-compat HTTP surface. Each sub-module owns one Jellyfin
//! controller's worth of routes (see `docs/jellyfin-mapping.md` §3).

pub mod admin;
pub mod auth_extractor;
pub mod channels;
pub mod ci_query;
mod download;
pub mod fmp4;
pub mod hls;
pub mod images;
pub mod item_ops;
pub mod items;
pub mod live_tv;
pub(crate) mod media_source;
pub mod playlists;
pub mod search;
pub mod seek;
pub mod sessions;
pub mod socket;
pub mod stream;
pub mod stream_select;
pub mod stubs;
pub mod subtitle_burn;
pub mod subtitle_delivery;
pub mod subtitles;
pub mod syncplay;
pub mod system;
pub mod trickplay;
pub mod user_data;
pub mod user_prefs;
pub mod users;
pub mod waveform;
pub mod wire;

// Phase A — DTO + serde leaf-crate extraction. Modules now live in
// `pharos-jellyfin-api`; re-exported here so existing call sites
// (`crate::api::jellyfin::dto::*`,
// `pharos_server::api::jellyfin::socket_messages::*`) keep compiling
// without edit churn.
pub use pharos_jellyfin_api::{device_profile, dto, socket_messages};

use actix_web::web;

/// Mount Jellyfin routes onto a `web::ServiceConfig`. Caller decides scope.
pub fn configure(cfg: &mut web::ServiceConfig) {
    system::register(cfg);
    users::register(cfg);
    // Admin routes registered AFTER `users` so admin /users (list) does
    // not collide with a more-specific bearer-only `/users/{id}` (the
    // actix router matches by registration order on duplicate specificity).
    admin::register(cfg);
    items::register(cfg);
    item_ops::register(cfg);
    download::register(cfg);
    playlists::register(cfg);
    live_tv::register(cfg);
    channels::register(cfg);
    search::register(cfg);
    user_data::register(cfg);
    images::register(cfg);
    stream::register(cfg);
    hls::register(cfg);
    trickplay::register(cfg);
    sessions::register(cfg);
    socket::register(cfg);
    subtitles::register(cfg);
    syncplay::register(cfg);
    waveform::register(cfg);
    stubs::register(cfg);
}

/// Answers an item `user` may not reach (V187) as 404, like a missing one,
/// so a hidden item's existence does not leak; 503 when the store could not
/// say.
pub(crate) async fn require_item_access(
    state: &crate::state::AppState,
    user: &pharos_core::User,
    id: pharos_core::MediaId,
    route: &'static str,
) -> Result<(), actix_web::Error> {
    crate::services::access::check_item(state, user, id, route)
        .await
        .map_err(access_error)
}

/// The response for a withheld item: 404 when hidden, 503 when the store
/// could not say.
pub(crate) fn access_error(hidden: crate::services::access::ItemHidden) -> actix_web::Error {
    use crate::services::access::ItemHidden;
    match hidden {
        ItemHidden::Unavailable(_) => {
            actix_web::error::ErrorServiceUnavailable("store unavailable")
        }
        ItemHidden::LibraryNotAllowed | ItemHidden::NoLibrary => {
            actix_web::error::ErrorNotFound("not found")
        }
        ItemHidden::Unauthenticated => actix_web::error::ErrorUnauthorized("missing token"),
    }
}

/// The bearer of a request for an item's files (`id` as the path spelled
/// it), from a header, `api_key` or the `JellyfinAuth` cookie; 401 without
/// one (V189).
pub(crate) async fn require_bearer(
    state: &crate::state::AppState,
    req: &actix_web::HttpRequest,
    id: &str,
    route: &'static str,
) -> Result<pharos_core::User, actix_web::Error> {
    let token = auth_extractor::extract_token(req);
    crate::services::access::authenticate(state, token.as_deref(), id, route)
        .await
        .map_err(access_error)
}
