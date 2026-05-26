//! Jellyfin-compat HTTP surface. Each sub-module owns one Jellyfin
//! controller's worth of routes (see `docs/jellyfin-mapping.md` §3).

pub mod admin;
pub mod auth_extractor;
pub mod device_profile;
pub mod dto;
pub mod hls;
pub mod images;
pub mod items;
pub mod live_tv;
pub mod search;
pub mod sessions;
pub mod socket;
pub mod socket_messages;
pub mod stream;
pub mod system;
pub mod user_data;
pub mod users;

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
    sessions::register(cfg);
    socket::register(cfg);
}
