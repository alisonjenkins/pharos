//! Jellyfin-compat HTTP surface. Each sub-module owns one Jellyfin
//! controller's worth of routes (see `docs/jellyfin-mapping.md` §3).

pub mod admin;
pub mod auth_extractor;
pub mod fmp4;
pub mod hls;
pub mod images;
pub mod items;
pub mod live_tv;
pub mod search;
pub mod sessions;
pub mod socket;
pub mod stream;
pub mod stubs;
pub mod subtitles;
pub mod syncplay;
pub mod system;
pub mod trickplay;
pub mod user_data;
pub mod users;
pub mod waveform;

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
    live_tv::register(cfg);
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
