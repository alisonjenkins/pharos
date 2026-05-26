//! `/Users/{userId}/PlayedItems` + `/Users/{userId}/FavoriteItems`. T33.
//!
//! Wire shape: Jellyfin returns the updated `UserItemDataDto` on
//! success. jellyfin-web reads the response and updates its store, so
//! returning anything else (e.g. 204) blanks the watched indicator
//! until a refresh.

use crate::{
    api::jellyfin::{auth_extractor::AuthUser, dto::UserItemDataDto},
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{MediaId, MediaStore, UserDataStore, UserId, UserItemData};
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
    );
}

async fn mark_played(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (uid_path, item_id) = path.into_inner();
    require_bearer_match(&user, &uid_path)?;
    mutate(&state, user.0.id, &item_id, |d| {
        d.played = true;
        d.play_count = d.play_count.saturating_add(1);
        d.last_played_position_ticks = 0;
        d.last_played_at = now_unix();
    })
    .await
}

async fn unmark_played(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (uid_path, item_id) = path.into_inner();
    require_bearer_match(&user, &uid_path)?;
    mutate(&state, user.0.id, &item_id, |d| {
        d.played = false;
    })
    .await
}

async fn mark_favorite(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (uid_path, item_id) = path.into_inner();
    require_bearer_match(&user, &uid_path)?;
    mutate(&state, user.0.id, &item_id, |d| {
        d.is_favorite = true;
    })
    .await
}

async fn unmark_favorite(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (uid_path, item_id) = path.into_inner();
    require_bearer_match(&user, &uid_path)?;
    mutate(&state, user.0.id, &item_id, |d| {
        d.is_favorite = false;
    })
    .await
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
    user_id: UserId,
    item_id_str: &str,
    f: F,
) -> Result<HttpResponse, actix_web::Error>
where
    F: FnOnce(&mut UserItemData),
{
    let item_id: MediaId = item_id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid item id"))?;
    // Confirm the item exists before writing a row that the cascade
    // would have to clean up later.
    state.stores.get(item_id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("item not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let mut data = state
        .stores
        .get_user_data(user_id, item_id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    f(&mut data);
    state
        .stores
        .set_user_data(user_id, item_id, data)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // T40 phase 2 — fan out to every connected /socket so jellyfin-web
    // updates the watched indicator + favourite star without a refresh.
    state.notify_user_data_changed(&user_id.0.simple().to_string(), &item_id.to_string());
    Ok(HttpResponse::Ok().json(UserItemDataDto::from_domain(item_id, data)))
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
