//! Jellyfin-compat HTTP surface. Each sub-module owns one Jellyfin
//! controller's worth of routes (see `docs/jellyfin-mapping.md` §3).

pub mod admin;
pub mod auth_extractor;
pub mod channels;
pub mod ci_query;
mod download;
mod duplicates;
pub use pharos_cache::fmp4;
pub mod hls;
pub mod images;
pub mod item_ops;
pub mod items;
pub mod live_tv;
pub(crate) mod media_source;
mod metadata_edit;
pub(crate) mod output_size;
pub mod play_capability;
pub mod playlists;
pub mod remote_subtitles;
pub mod search;
pub mod seek;
mod series_identity;
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
pub mod user_images;
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
    series_identity::register(cfg);
    items::register(cfg);
    item_ops::register(cfg);
    metadata_edit::register(cfg);
    download::register(cfg);
    playlists::register(cfg);
    live_tv::register(cfg);
    channels::register(cfg);
    search::register(cfg);
    user_data::register(cfg);
    images::register(cfg);
    user_images::register(cfg);
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
        ItemHidden::LibraryNotAllowed
        | ItemHidden::NoLibrary
        | ItemHidden::ParentalRating
        | ItemHidden::Tag
        | ItemHidden::NotFound => actix_web::error::ErrorNotFound("not found"),
        ItemHidden::Unauthenticated => actix_web::error::ErrorUnauthorized("missing token"),
        ItemHidden::OutOfScope => {
            actix_web::error::ErrorForbidden("token scope does not permit this route")
        }
    }
}

/// The bearer of a request for an item's files (`id` as the path spelled
/// it), from a header, `api_key` or, on a GET or HEAD, the `JellyfinAuth`
/// cookie; 401 without one (V189, V191).
pub(crate) async fn require_bearer(
    state: &crate::state::AppState,
    req: &actix_web::HttpRequest,
    id: &str,
    route: &'static str,
) -> Result<pharos_core::User, actix_web::Error> {
    let token = auth_extractor::media_token(Some(state), req).await;
    // A playback's subtitle and font URLs carry its PlaySessionId, not a
    // token (V1050).
    if token.is_none() {
        if let Some(media_id) = pharos_jellyfin_api::dto::parse_item_id(id) {
            if let Some(user) = play_capability::capability_user(state, req, media_id).await? {
                return Ok(user);
            }
        }
    }
    let pattern = req.match_pattern();
    match crate::services::access::authenticate(
        state,
        token.as_deref(),
        id,
        route,
        req.method().as_str(),
        pattern.as_deref(),
    )
    .await
    {
        Ok(user) => {
            auth_extractor::note_header_credential(req);
            Ok(user)
        }
        // A stale cookie must not shadow a live capability.
        Err(crate::services::access::ItemHidden::Unauthenticated) if token.is_some() => {
            let capability = match pharos_jellyfin_api::dto::parse_item_id(id) {
                Some(media_id) => play_capability::capability_user(state, req, media_id).await?,
                None => None,
            };
            capability
                .ok_or_else(|| access_error(crate::services::access::ItemHidden::Unauthenticated))
        }
        Err(hidden) => Err(access_error(hidden)),
    }
}

/// The bearer of an image request, or `None` for an anonymous one. Image
/// GETs stay anonymous like Jellyfin's: jellyfin-web's `<img>` tags and the
/// native apps' image loaders send no credential (B286, V225). A token that
/// does not resolve counts as anonymous too, so a stale cookie or revoked
/// token cannot cost a client its artwork, and neither counts as a refusal;
/// 503 only when the store could not say.
pub(crate) async fn optional_bearer(
    state: &crate::state::AppState,
    req: &actix_web::HttpRequest,
    id: &str,
    route: &'static str,
) -> Result<Option<pharos_core::User>, actix_web::Error> {
    use crate::services::{access::ItemHidden, auth::TokenRefusal};
    let Some(token) = auth_extractor::media_token(Some(state), req).await else {
        tracing::debug!(item_id = id, route, "anonymous image request");
        return Ok(None);
    };
    match crate::services::auth::scoped_user_for_token(&state.stores, &token).await {
        // A restricted token (a cast TV's) speaks for its user only on the
        // routes its scope allows (V229); elsewhere the request is served
        // like an anonymous one, which an image route permits anyway.
        Ok((user, scope)) => {
            let pattern = req.match_pattern();
            if !auth_extractor::scope_permits(scope, req.method().as_str(), pattern.as_deref()) {
                tracing::debug!(
                    item_id = id,
                    route,
                    scope = scope.label(),
                    "image request served anonymously: token scope does not cover the route"
                );
                return Ok(None);
            }
            auth_extractor::note_header_credential(req);
            Ok(Some(user))
        }
        Err(TokenRefusal::Unavailable(detail)) => {
            let hidden = ItemHidden::Unavailable(detail);
            metrics::counter!("pharos_item_access_denied_total", "reason" => hidden.label())
                .increment(1);
            tracing::warn!(item_id = id, route, reason = hidden.label(), detail = %hidden, "item withheld: credential not checked");
            Err(access_error(hidden))
        }
        Err(refusal) => {
            tracing::debug!(
                item_id = id,
                route,
                credential = refusal.label(),
                "image request served anonymously: credential did not resolve"
            );
            Ok(None)
        }
    }
}

/// [`require_item_access`] for a signed-in requester; an anonymous one is
/// served like on Jellyfin (see [`optional_bearer`]).
pub(crate) async fn require_item_access_if_signed_in(
    state: &crate::state::AppState,
    user: Option<&pharos_core::User>,
    id: pharos_core::MediaId,
    route: &'static str,
) -> Result<(), actix_web::Error> {
    match user {
        Some(user) => require_item_access(state, user, id, route).await,
        None => Ok(()),
    }
}
