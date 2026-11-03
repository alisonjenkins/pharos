//! `/Users/{userId}/PlayedItems` + `/Users/{userId}/FavoriteItems`. T33.
//!
//! Wire shape: Jellyfin returns the updated `UserItemDataDto` on
//! success. jellyfin-web reads the response and updates its store, so
//! returning anything else (e.g. 204) blanks the watched indicator
//! until a refresh.

use crate::{
    api::jellyfin::{auth_extractor::AuthUser, dto::UserItemDataDto},
    services::{library::DetailError, user_data},
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::UserItemData;
use uuid::Uuid;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical routes; LowercasePath middleware folds
    // jellyfin-web's PascalCase requests onto them.
    cfg.route(
        "/users/{user_id}/playeditems/{item_id}",
        web::post().to(mark_played),
    )
    .route(
        "/users/{user_id}/playeditems/{item_id}",
        web::delete().to(unmark_played),
    )
    .route(
        "/users/{user_id}/favoriteitems/{item_id}",
        web::post().to(mark_favorite),
    )
    .route(
        "/users/{user_id}/favoriteitems/{item_id}",
        web::delete().to(unmark_favorite),
    )
    // B84 — the userId-less variants the modern jellyfin-sdk-kotlin (Android
    // TV / Google TV) calls: the user is the bearer, no `{userId}` segment.
    // Without these a "mark watched" POST hit no route, and the SDK CRASHED
    // the app trying to deserialize the error response as a UserItemDataDto.
    .route("/userplayeditems/{item_id}", web::post().to(mark_played_me))
    .route(
        "/userplayeditems/{item_id}",
        web::delete().to(unmark_played_me),
    )
    .route(
        "/userfavoriteitems/{item_id}",
        web::post().to(mark_favorite_me),
    )
    .route(
        "/userfavoriteitems/{item_id}",
        web::delete().to(unmark_favorite_me),
    );
}

/// The four UserData mutations, shared by the legacy `/Users/{userId}/…`
/// routes and the modern userId-less `/UserPlayedItems|UserFavoriteItems/…`
/// routes so the two families can never drift.
fn op_mark_played(d: &mut UserItemData) {
    user_data::apply_played(d, true);
}
fn op_unmark_played(d: &mut UserItemData) {
    user_data::apply_played(d, false);
}
fn op_mark_favorite(d: &mut UserItemData) {
    user_data::apply_favorite(d, true);
}
fn op_unmark_favorite(d: &mut UserItemData) {
    user_data::apply_favorite(d, false);
}

/// `POST /UserPlayedItems/{itemId}` — mark played for the bearer (no path
/// userId). Same body shape (UserItemDataDto) as the legacy route.
async fn mark_played_me(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    mutate(&state, &user.0, &path.into_inner(), op_mark_played).await
}

async fn unmark_played_me(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    mutate(&state, &user.0, &path.into_inner(), op_unmark_played).await
}

async fn mark_favorite_me(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    mutate(&state, &user.0, &path.into_inner(), op_mark_favorite).await
}

async fn unmark_favorite_me(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    mutate(&state, &user.0, &path.into_inner(), op_unmark_favorite).await
}

async fn mark_played(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (uid_path, item_id) = path.into_inner();
    require_bearer_match(&user, &uid_path)?;
    mutate(&state, &user.0, &item_id, op_mark_played).await
}

async fn unmark_played(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (uid_path, item_id) = path.into_inner();
    require_bearer_match(&user, &uid_path)?;
    mutate(&state, &user.0, &item_id, op_unmark_played).await
}

async fn mark_favorite(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (uid_path, item_id) = path.into_inner();
    require_bearer_match(&user, &uid_path)?;
    mutate(&state, &user.0, &item_id, op_mark_favorite).await
}

async fn unmark_favorite(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (uid_path, item_id) = path.into_inner();
    require_bearer_match(&user, &uid_path)?;
    mutate(&state, &user.0, &item_id, op_unmark_favorite).await
}

fn require_bearer_match(user: &AuthUser, uid_path: &str) -> Result<(), actix_web::Error> {
    let bearer = user.0.id.0.simple().to_string();
    if uid_path != bearer {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    // Defence-in-depth — also reject when the path id can't even be
    // parsed as a UUID. AuthUser already verified the bearer maps to a
    // real user, but we don't want to silently accept garbage paths.
    if Uuid::parse_str(uid_path).is_err() {
        return Err(error::ErrorBadRequest("invalid user id"));
    }
    Ok(())
}

async fn mutate<F>(
    state: &AppState,
    user: &pharos_core::User,
    item_id_str: &str,
    f: F,
) -> Result<HttpResponse, actix_web::Error>
where
    F: Fn(&mut UserItemData),
{
    let Some(item_id) = pharos_jellyfin_api::dto::parse_item_id(item_id_str) else {
        // Not a library item id — synthetic series/season ids land here.
        // jellyfin-web's series/season pages offer mark-played /
        // favourite buttons, so cascade over the child episodes instead
        // of 400ing (B36).
        return mutate_synth_folder(state, user, item_id_str, f).await;
    };
    let (item, data) = user_data::update(state, user, item_id, f)
        .await
        .map_err(detail_error)?;
    let runtime = item.probe.run_time_ticks().unwrap_or(0);
    let dto = UserItemDataDto::from_domain_with_runtime(item_id, data, runtime);
    Ok(crate::api::jellyfin::wire::json(&dto))
}

fn detail_error(e: DetailError) -> actix_web::Error {
    match e {
        DetailError::Hidden(hidden) => crate::api::jellyfin::access_error(hidden),
        DetailError::NotFound => error::ErrorNotFound("item not found"),
        DetailError::Store(e) => error::ErrorInternalServerError(e.to_string()),
    }
}

/// B36 — mark-played / favourite on a synthetic series or season id:
/// apply the mutation to every child episode and return the folder-level
/// DTO the way real Jellyfin does.
async fn mutate_synth_folder<F>(
    state: &AppState,
    user: &pharos_core::User,
    id_str: &str,
    f: F,
) -> Result<HttpResponse, actix_web::Error>
where
    F: Fn(&mut UserItemData),
{
    // B91 — the Android TV kotlin SDK marks a whole show / season watched via
    // `POST /UserPlayedItems/{id}` with the synth id re-serialised DASHED
    // (`11a177ee-3a0e-0b8f-11a1-77ee3a0e0b8f`). Every comparison is against
    // the dashless `series_id_for_key` / `season_id_for_key` hash, so without
    // canonicalising the dashed id never matches → the cascade found no episodes
    // → 404, which the SDK can't deserialise → the app CRASHES (the B84 class).
    // The same dashed-id sweep already fixed nav (B85) + parent lists (B82).
    let canonical = crate::api::jellyfin::items::canonical_wire_id(id_str);
    let id_str: &str = canonical.as_ref();
    let folder = user_data::update_folder(state, user, id_str, f)
        .await
        .map_err(detail_error)?;
    Ok(crate::api::jellyfin::wire::json(&UserItemDataDto::folder(
        id_str,
        folder.played,
        folder.play_count,
        folder.is_favorite,
    )))
}
