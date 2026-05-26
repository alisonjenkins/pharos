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
use pharos_core::{MediaItem, MediaKind, MediaStore, UserDataStore, UserId};
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: paths registered in lowercase only — the `LowercasePath`
    // middleware rewrites PascalCase requests before routing. Empty-
    // list stubs cover the long-tail endpoints jellyfin-web fetches
    // on the home + details pages so the client renders the empty
    // state instead of throwing a Response exception.
    cfg.route("/items", web::get().to(list_items))
        .route("/items/{id}", web::get().to(get_item))
        .route("/users/{user_id}/items", web::get().to(list_user_items))
        .route(
            "/users/{user_id}/items/latest",
            web::get().to(list_user_items_latest),
        )
        .route(
            "/users/{user_id}/items/resume",
            web::get().to(list_user_items_resume),
        )
        .route(
            "/users/{user_id}/items/{item_id}",
            web::get().to(get_user_item),
        )
        .route("/users/{user_id}/views", web::get().to(user_views))
        .route("/userviews", web::get().to(user_views_query))
        .route("/library/virtualfolders", web::get().to(virtual_folders))
        .route("/library/mediafolders", web::get().to(media_folders))
        .route("/items/{id}/playbackinfo", web::get().to(playback_info))
        .route("/items/{id}/playbackinfo", web::post().to(playback_info));

    for path in [
        "/items/{id}/similar",
        "/items/{id}/thememedia",
        "/items/{id}/themesongs",
        "/items/{id}/themevideos",
        "/items/{id}/specialfeatures",
        "/users/{user_id}/items/{item_id}/intros",
        "/shows/nextup",
        "/shows/upcoming",
        "/genres",
        "/studios",
        "/persons",
    ] {
        cfg.route(path, web::get().to(empty_items_result));
    }
}

async fn empty_items_result(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "Items": [],
        "TotalRecordCount": 0,
        "StartIndex": 0,
    }))
}

async fn playback_info(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id_str = path.into_inner();
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let play_session_id = uuid::Uuid::new_v4().simple().to_string();
    let video_streams = matches!(item.kind, MediaKind::Movie | MediaKind::Episode);
    let mut streams = Vec::new();
    if video_streams {
        streams.push(serde_json::json!({
            "Type": "Video",
            "Index": 0,
            "Codec": "h264",
            "Width": 1920,
            "Height": 1080,
            "AspectRatio": "16:9",
            "IsDefault": true,
            "BitDepth": 8,
            "FrameRate": 30.0,
        }));
    }
    streams.push(serde_json::json!({
        "Type": "Audio",
        "Index": if video_streams { 1 } else { 0 },
        "Codec": "aac",
        "Channels": 2,
        "SampleRate": 48000,
        "IsDefault": true,
    }));
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "MediaSources": [{
            "Id": id_str,
            "Path": item.path.to_string_lossy(),
            "Type": "Default",
            "Container": if video_streams { "mp4" } else { "mp3" },
            "IsRemote": false,
            "ETag": "",
            "RunTimeTicks": 50_000_000_u64,
            "Name": item.title,
            "Protocol": "File",
            "SupportsDirectPlay": true,
            "SupportsDirectStream": true,
            "SupportsTranscoding": true,
            "RequiresOpening": false,
            "RequiresClosing": false,
            "RequiresLooping": false,
            "SupportsProbing": true,
            "MediaStreams": streams,
            "Bitrate": 2_500_000,
            "VideoType": "VideoFile",
            "DefaultAudioStreamIndex": if video_streams { 1 } else { 0 },
            "DefaultSubtitleStreamIndex": null,
        }],
        "PlaySessionId": play_session_id,
    })))
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
    let page: Vec<MediaItem> = all.into_iter().take(limit).collect();
    let ids: Vec<u64> = page.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.0.id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let dtos: Vec<BaseItemDto> = page
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let ud = user_data.get(i).copied().unwrap_or_default();
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud)
        })
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
    user: AuthUser,
    q: web::Query<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let filtered = filter_and_sort(all, &q);
    let dto = paginate(&state, user.0.id, filtered, q.start_index, q.limit).await?;
    Ok(HttpResponse::Ok().json(dto))
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
    let dto = paginate(&state, user.0.id, filtered, q.start_index, q.limit).await?;
    Ok(HttpResponse::Ok().json(dto))
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
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id_str = path.into_inner();
    fetch_item_dto(&state, &id_str, user.0.id).await
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
    fetch_item_dto(&state, &item_id, user.0.id).await
}

async fn fetch_item_dto(
    state: &AppState,
    id_str: &str,
    user_id: UserId,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let user_data = state
        .stores
        .get_user_data(user_id, id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::Ok().json(BaseItemDto::from_domain_with_user_data(
        &item,
        &state.server_id,
        user_data,
    )))
}

async fn list_user_items_resume(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let bearer_id = user.0.id.0.simple().to_string();
    if path.into_inner() != bearer_id {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    let ids = state
        .stores
        .resumable_items(user.0.id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut items: Vec<MediaItem> = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Ok(item) = state.stores.get(*id).await {
            items.push(item);
        }
    }
    let user_data = state
        .stores
        .user_data_bulk(user.0.id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let total = items.len() as u32;
    let dtos: Vec<BaseItemDto> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let ud = user_data.get(i).copied().unwrap_or_default();
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud)
        })
        .collect();
    Ok(HttpResponse::Ok().json(ItemsResultDto {
        items: dtos,
        total_record_count: total,
        start_index: 0,
    }))
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

async fn paginate(
    state: &AppState,
    user_id: UserId,
    all: Vec<pharos_core::MediaItem>,
    start_index: u32,
    limit: u32,
) -> Result<ItemsResultDto, actix_web::Error> {
    let total = all.len() as u32;
    let start = start_index as usize;
    let end = (start + limit as usize).min(all.len());
    let slice = if start >= all.len() {
        &[][..]
    } else {
        &all[start..end]
    };
    let ids: Vec<u64> = slice.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user_id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let items: Vec<BaseItemDto> = slice
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let ud = user_data.get(i).copied().unwrap_or_default();
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud)
        })
        .collect();
    Ok(ItemsResultDto {
        items,
        total_record_count: total,
        start_index,
    })
}
