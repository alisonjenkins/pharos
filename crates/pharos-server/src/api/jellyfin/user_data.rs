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
use pharos_core::{MediaStore, UserDataStore, UserId, UserItemData};
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
    F: Fn(&mut UserItemData),
{
    let Some(item_id) = pharos_jellyfin_api::dto::parse_item_id(item_id_str) else {
        // Not a library item id — synthetic series/season ids land here.
        // jellyfin-web's series/season pages offer mark-played /
        // favourite buttons, so cascade over the child episodes instead
        // of 400ing (B36).
        return mutate_synth_folder(state, user_id, item_id_str, f).await;
    };
    // Confirm the item exists before writing a row that the cascade
    // would have to clean up later.
    let item = state.stores.get(item_id).await.map_err(|e| match e {
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
    let runtime = item.probe.run_time_ticks().unwrap_or(0);
    let dto = UserItemDataDto::from_domain_with_runtime(item_id, data, runtime);
    // T40 phase 2 / B36 — fan out the FULL DTO to every connected
    // /socket so jellyfin-web updates the watched indicator + favourite
    // star in place without a refresh.
    let entry =
        serde_json::to_value(&dto).map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    state.notify_user_data_changed(&user_id.0.simple().to_string(), vec![entry]);
    Ok(crate::api::jellyfin::wire::json(&dto))
}

/// B36 — mark-played / favourite on a synthetic series or season id:
/// apply the mutation to every child episode, broadcast one
/// `UserDataChanged` frame carrying all changed entries (plus a folder
/// entry so the series/season detail page's play-state matches by Key),
/// and return the folder-level DTO the way real Jellyfin does.
async fn mutate_synth_folder<F>(
    state: &AppState,
    user_id: UserId,
    id_str: &str,
    f: F,
) -> Result<HttpResponse, actix_web::Error>
where
    F: Fn(&mut UserItemData),
{
    use crate::api::jellyfin::dto::{season_id_for_key, series_id_for_key};
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let episodes: Vec<_> = all
        .iter()
        .filter(|item| {
            let Some(series) = item.series.as_ref() else {
                return false;
            };
            if series_id_for_key(series.series_folder.as_deref(), &series.series_name) == id_str {
                return true;
            }
            series.season_number.is_some_and(|n| {
                season_id_for_key(series.series_folder.as_deref(), &series.series_name, n) == id_str
            })
        })
        .collect();
    if episodes.is_empty() {
        return Err(error::ErrorNotFound("item not found"));
    }
    let mut entries = Vec::with_capacity(episodes.len() + 1);
    let mut folder_data = UserItemData::default();
    f(&mut folder_data);
    for item in episodes {
        let mut data = state
            .stores
            .get_user_data(user_id, item.id)
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        f(&mut data);
        state
            .stores
            .set_user_data(user_id, item.id, data)
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        let runtime = item.probe.run_time_ticks().unwrap_or(0);
        let dto = UserItemDataDto::from_domain_with_runtime(item.id, data, runtime);
        entries.push(
            serde_json::to_value(&dto)
                .map_err(|e| error::ErrorInternalServerError(e.to_string()))?,
        );
    }
    // Folder-level DTO: ItemId/Key are the synthetic id so the detail
    // page (which matches on Key) picks it up. B78/V38 — typed DTO, not a
    // json! literal, so the kotlin-required UserData field set stays complete.
    let folder_val = serde_json::to_value(UserItemDataDto::folder(
        id_str,
        folder_data.played,
        folder_data.play_count,
        folder_data.is_favorite,
    ))
    .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    entries.push(folder_val.clone());
    state.notify_user_data_changed(&user_id.0.simple().to_string(), entries);
    Ok(crate::api::jellyfin::wire::json(&folder_val))
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
