//! /Items and /Library item-browsing routes.
//!
//! Phase-1 scope: list, get-by-id, per-user list, virtual-folders summary.
//! Phase-2 scope (this file): SearchTerm + IncludeItemTypes filters,
//! SortBy / SortOrder. Filtering is in-memory after `MediaStore::list()`
//! today — moves to SQL-side once library sizes warrant it.

use crate::{
    api::jellyfin::{
        auth_extractor::AuthUser,
        dto::{BaseItemDto, ItemsResultDto, VirtualFolderInfoDto, VirtualFolderOptionsDto},
    },
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{MediaItem, MediaKind, MediaStore};
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    for path in ["/Items", "/items"] {
        cfg.route(path, web::get().to(list_items));
    }
    for path in ["/Items/{id}", "/items/{id}"] {
        cfg.route(path, web::get().to(get_item));
    }
    for path in ["/Users/{user_id}/Items", "/Users/{user_id}/items"] {
        cfg.route(path, web::get().to(list_user_items));
    }
    for path in [
        "/Users/{user_id}/Items/Latest",
        "/Users/{user_id}/items/latest",
    ] {
        cfg.route(path, web::get().to(list_user_items_latest));
    }
    for path in [
        "/Users/{user_id}/Items/{item_id}",
        "/Users/{user_id}/items/{item_id}",
    ] {
        cfg.route(path, web::get().to(get_user_item));
    }
    for path in ["/Users/{user_id}/Views", "/Users/{user_id}/views"] {
        cfg.route(path, web::get().to(user_views));
    }
    // Legacy alias jellyfin-web hits: GET /UserViews?userId=...
    for path in ["/UserViews", "/userviews"] {
        cfg.route(path, web::get().to(user_views_query));
    }
    for path in ["/Library/VirtualFolders", "/library/virtualfolders"] {
        cfg.route(path, web::get().to(virtual_folders));
    }
    for path in ["/Library/MediaFolders", "/library/mediafolders"] {
        cfg.route(path, web::get().to(media_folders));
    }
}

async fn list_user_items_latest(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: web::Query<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let user_path = path.into_inner();
    let bearer_id = user.0.id.0.simple().to_string();
    if user_path != bearer_id {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let limit = q.limit.min(100) as usize;
    let dtos: Vec<BaseItemDto> = all
        .into_iter()
        .take(limit)
        .map(|i| BaseItemDto::from_domain(&i, &state.server_id))
        .collect();
    // /Items/Latest returns a raw array, not the ItemsResult envelope.
    Ok(HttpResponse::Ok().json(dtos))
}

async fn user_views(
    state: web::Data<AppState>,
    _user: AuthUser,
    _path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    Ok(HttpResponse::Ok().json(synth_views_body(&state.server_id)))
}

#[derive(serde::Deserialize)]
struct UserViewsQuery {
    #[serde(default, rename = "userId")]
    #[allow(dead_code)]
    user_id: Option<String>,
}

async fn user_views_query(
    state: web::Data<AppState>,
    _user: AuthUser,
    _q: web::Query<UserViewsQuery>,
) -> Result<impl Responder, actix_web::Error> {
    Ok(HttpResponse::Ok().json(synth_views_body(&state.server_id)))
}

fn synth_views_body(server_id: &str) -> serde_json::Value {
    // Phase 1: synthesize one "All Media" collection so the home page
    // renders something. Real per-root libraries lands once
    // [media].roots are wired into the scanner + store.
    let view = serde_json::json!({
        "Id": "00000000000000000000000000000000",
        "Name": "All Media",
        "ServerId": server_id,
        "Type": "CollectionFolder",
        "CollectionType": "mixed",
        "MediaType": "Unknown",
        "IsFolder": true,
        "UserData": { "Played": false, "PlayCount": 0 },
    });
    serde_json::json!({
        "Items": [view],
        "TotalRecordCount": 1,
        "StartIndex": 0,
    })
}

async fn media_folders(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    let view = serde_json::json!({
        "Id": "00000000000000000000000000000000",
        "Name": "All Media",
        "ServerId": state.server_id,
        "Type": "CollectionFolder",
        "CollectionType": "mixed",
        "MediaType": "Unknown",
        "IsFolder": true,
        "UserData": { "Played": false, "PlayCount": 0 },
    });
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": [view],
        "TotalRecordCount": 1,
        "StartIndex": 0,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListQuery {
    #[serde(default)]
    start_index: u32,
    #[serde(default = "default_limit")]
    limit: u32,
    /// Substring of the item title; case-insensitive.
    #[serde(default)]
    search_term: Option<String>,
    /// Comma-separated Jellyfin Type names: e.g. "Movie,Episode".
    #[serde(default)]
    include_item_types: Option<String>,
    /// `SortName` (default), `Random`, `DateCreated` (currently same as SortName — no created-at column yet).
    #[serde(default)]
    sort_by: Option<String>,
    /// `Ascending` (default) | `Descending`.
    #[serde(default)]
    sort_order: Option<String>,
}

fn default_limit() -> u32 {
    100
}

async fn list_items(
    state: web::Data<AppState>,
    _user: AuthUser,
    q: web::Query<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let filtered = filter_and_sort(all, &q);
    Ok(HttpResponse::Ok().json(paginate(filtered, &state.server_id, q.start_index, q.limit)))
}

async fn list_user_items(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: web::Query<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    // V9 spirit: the path user must match the bearer. Reject mismatched.
    let user_path = path.into_inner();
    let bearer_id = user.0.id.0.simple().to_string();
    if user_path != bearer_id {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let filtered = filter_and_sort(all, &q);
    Ok(HttpResponse::Ok().json(paginate(filtered, &state.server_id, q.start_index, q.limit)))
}

fn filter_and_sort(mut items: Vec<MediaItem>, q: &ListQuery) -> Vec<MediaItem> {
    if let Some(term) = q.search_term.as_ref() {
        let needle = term.to_ascii_lowercase();
        if !needle.is_empty() {
            items.retain(|i| i.title.to_ascii_lowercase().contains(&needle));
        }
    }
    if let Some(types) = q.include_item_types.as_ref() {
        let wanted: Vec<MediaKind> = types
            .split(',')
            .filter_map(|s| jellyfin_type_to_kind(s.trim()))
            .collect();
        if !wanted.is_empty() {
            items.retain(|i| wanted.contains(&i.kind));
        }
    }
    let sort_by = q.sort_by.as_deref().unwrap_or("SortName");
    let descending = matches!(q.sort_order.as_deref(), Some("Descending"));
    match sort_by {
        "Random" => shuffle_in_place(&mut items),
        _ => {
            items.sort_by(|a, b| {
                a.title
                    .to_ascii_lowercase()
                    .cmp(&b.title.to_ascii_lowercase())
            });
            if descending {
                items.reverse();
            }
        }
    }
    items
}

fn jellyfin_type_to_kind(s: &str) -> Option<MediaKind> {
    match s {
        "Movie" => Some(MediaKind::Movie),
        "Episode" => Some(MediaKind::Episode),
        "Audio" => Some(MediaKind::Audio),
        _ => None,
    }
}

/// Deterministic-when-tested shuffle. Uses `getrandom` to seed a small
/// xorshift so the random-sort doesn't pull in the rand crate.
fn shuffle_in_place(items: &mut [MediaItem]) {
    let mut seed = [0u8; 8];
    if getrandom::getrandom(&mut seed).is_err() {
        // Fall back to a fixed seed — caller already accepts non-determinism;
        // a fixed seed is no worse than panicking under unprivileged sandbox.
        seed = [1, 2, 3, 4, 5, 6, 7, 8];
    }
    let mut state = u64::from_le_bytes(seed) | 1;
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state as usize) % (i + 1);
        items.swap(i, j);
    }
}

async fn get_item(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id_str = path.into_inner();
    fetch_item_dto(&state, &id_str).await
}

async fn get_user_item(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (user_path, item_id) = path.into_inner();
    let bearer_id = user.0.id.0.simple().to_string();
    if user_path != bearer_id {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    fetch_item_dto(&state, &item_id).await
}

async fn fetch_item_dto(
    state: &AppState,
    id_str: &str,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    Ok(HttpResponse::Ok().json(BaseItemDto::from_domain(&item, &state.server_id)))
}

async fn virtual_folders(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    // Phase 1: report a single synthesized "All Media" library covering the
    // entire store. Real per-root libraries land with media-roots wiring.
    let folder = VirtualFolderInfoDto {
        name: "All Media".into(),
        locations: vec![],
        collection_type: "mixed",
        item_id: "00000000000000000000000000000000".into(),
        library_options: VirtualFolderOptionsDto::default(),
    };
    let _ = &state.stores;
    Ok(HttpResponse::Ok().json(vec![folder]))
}

fn paginate(
    all: Vec<pharos_core::MediaItem>,
    server_id: &str,
    start_index: u32,
    limit: u32,
) -> ItemsResultDto {
    let total = all.len() as u32;
    let start = start_index as usize;
    let end = (start + limit as usize).min(all.len());
    let slice = if start >= all.len() {
        &[][..]
    } else {
        &all[start..end]
    };
    let items: Vec<BaseItemDto> = slice
        .iter()
        .map(|i| BaseItemDto::from_domain(i, server_id))
        .collect();
    ItemsResultDto {
        items,
        total_record_count: total,
        start_index,
    }
}
