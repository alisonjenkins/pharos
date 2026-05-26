//! Jellyfin-compat HTTP surface. Each sub-module owns one Jellyfin
//! controller's worth of routes (see `docs/jellyfin-mapping.md` §3).

pub mod auth_extractor;
pub mod dto;
pub mod images;
pub mod items;
pub mod sessions;
pub mod socket;
pub mod socket_messages;
pub mod stream;
pub mod system;
pub mod users;

use actix_web::web;

/// Mount Jellyfin routes onto a `web::ServiceConfig`. Caller decides scope.
pub fn configure(cfg: &mut web::ServiceConfig) {
    system::register(cfg);
    users::register(cfg);
    items::register(cfg);
    images::register(cfg);
    stream::register(cfg);
    sessions::register(cfg);
    socket::register(cfg);
}
