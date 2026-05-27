//! /Items and /Library item-browsing routes.
//!
//! Phase-1 scope: list, get-by-id, per-user list, virtual-folders summary.
//! Phase-2 scope (this file): SearchTerm + IncludeItemTypes filters,
//! SortBy / SortOrder. Filtering is in-memory after `MediaStore::list()`
//! today — moves to SQL-side once library sizes warrant it.

use crate::{
    api::jellyfin::{
        auth_extractor::AuthUser,
        device_profile::{negotiate, Decision, DeviceProfile, SourceMedia},
        dto::{
            build_media_streams_with_subtitles, container_for, BaseItemDto, ItemsResultDto,
            SubtitleStreamCtx, VirtualFolderInfoDto, VirtualFolderOptionsDto,
        },
        subtitles::discover_sidecars,
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
        "/shows/upcoming",
        "/genres",
        "/studios",
        "/persons",
    ] {
        cfg.route(path, web::get().to(empty_items_result));
    }
    // /Shows/NextUp has a real impl now that episode hierarchy
    // exists. Keep it after the empty-stub loop so the route is
    // registered with our handler.
    cfg.route("/shows/nextup", web::get().to(shows_next_up));
}

async fn empty_items_result(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "Items": [],
        "TotalRecordCount": 0,
        "StartIndex": 0,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NextUpQuery {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default = "default_limit")]
    limit: u32,
}

/// `GET /Shows/NextUp` — per Series, return the lowest-numbered
/// Episode the user hasn't played yet. Sorted by series name; capped
/// by the client's `Limit`. Driven entirely by the persisted
/// SeriesInfo + UserItemData — no extra columns needed.
async fn shows_next_up(
    state: web::Data<AppState>,
    user: AuthUser,
    q: web::Query<NextUpQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let ids: Vec<u64> = all.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.0.id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // Group episodes by series; pick the lowest unwatched per series.
    use std::collections::HashMap;
    let mut buckets: HashMap<String, Vec<(usize, &MediaItem)>> = HashMap::new();
    for (idx, item) in all.iter().enumerate() {
        if !matches!(item.kind, MediaKind::Episode) {
            continue;
        }
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        // Skip already-played episodes.
        if user_data.get(idx).copied().unwrap_or_default().played {
            continue;
        }
        buckets
            .entry(series.series_name.clone())
            .or_default()
            .push((idx, item));
    }
    // Sort each bucket by (season_number, episode_number) ascending,
    // pick the head.
    let mut picks: Vec<(usize, &MediaItem)> = buckets
        .into_iter()
        .filter_map(|(_name, mut eps)| {
            eps.sort_by_key(|(_, e)| {
                let s = e.series.as_ref().and_then(|s| s.season_number).unwrap_or(0);
                let n = e.series.as_ref().and_then(|s| s.episode_number).unwrap_or(0);
                (s, n)
            });
            eps.into_iter().next()
        })
        .collect();
    // Stable series-name sort across the result set.
    picks.sort_by(|a, b| {
        let an = a
            .1
            .series
            .as_ref()
            .map(|s| s.series_name.as_str())
            .unwrap_or("");
        let bn = b
            .1
            .series
            .as_ref()
            .map(|s| s.series_name.as_str())
            .unwrap_or("");
        an.cmp(bn)
    });
    picks.truncate(q.limit.max(1) as usize);
    let dtos: Vec<BaseItemDto> = picks
        .iter()
        .map(|(idx, item)| {
            let ud = user_data.get(*idx).copied().unwrap_or_default();
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud)
        })
        .collect();
    let total = dtos.len() as u32;
    let _ = q.user_id; // kept for future per-user scoping
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": dtos,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct PlaybackInfoBody {
    device_profile: Option<DeviceProfile>,
}

async fn playback_info(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    body: Option<web::Json<PlaybackInfoBody>>,
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
    let is_video = matches!(item.kind, MediaKind::Movie | MediaKind::Episode);
    let probe = &item.probe;

    // Source-media shape pulled from the probe persisted by the scanner
    // (T29 follow-up). Container falls back to a kind-derived default
    // only when ffprobe never ran (mirrors `container_for` in dto.rs so
    // negotiator + DTO see the same value).
    let container = container_for(probe, is_video);
    let source = SourceMedia {
        container: container.clone(),
        video_codec: probe.video_codec.clone(),
        audio_codec: probe.audio_codec.clone(),
        bitrate_bps: probe.bitrate_bps,
        is_video,
    };

    let profile = body
        .and_then(|b| b.into_inner().device_profile)
        .unwrap_or_default();
    let decision = negotiate(&profile, &source);

    let direct_play = decision.is_direct();
    let supports_direct_stream = direct_play
        || matches!(decision, Decision::AudioRemux { .. });
    let transcoding_url = match &decision {
        Decision::Transcode { target_container, .. } if target_container == "ts" => {
            // PlaySessionId rides on the URL so the HLS handlers can
            // look up the cached Decision (T-fix-2 part 2) instead of
            // re-running the negotiator per segment.
            Some(format!(
                "/videos/{id_str}/master.m3u8?PlaySessionId={play_session_id}"
            ))
        }
        _ => None,
    };

    // Register the negotiated Decision so HLS segment generation
    // honours the target codec / container / bitrate cap. Only
    // matters when we actually emitted a TranscodingUrl; direct play
    // skips this so the cache doesn't bloat with no-op entries.
    if transcoding_url.is_some() {
        let _ = state
            .transcode_sessions
            .insert(
                play_session_id.clone(),
                crate::transcode_sessions::TranscodeSession {
                    media_id: id,
                    decision: decision.clone(),
                    source_probe: probe.clone(),
                },
            )
            .await;
    }

    let sidecars = discover_sidecars(&item.path).await;
    let ctx = SubtitleStreamCtx {
        item_id: item.id,
        sidecar_count: sidecars.len() as u32,
    };
    let streams = build_media_streams_with_subtitles(probe, is_video, Some(&ctx));
    // Find the audio stream's actual index (or skip if there isn't one).
    // Hard-coding `1` for silent-video files made jellyfin-web's player
    // try to select a track that doesn't exist.
    let default_audio_stream_index: Option<u32> = streams
        .iter()
        .find(|s| s.kind == "Audio")
        .map(|s| s.index);

    // TranscodingSubProtocol only makes sense alongside a real
    // TranscodingUrl. Emitting `"hls"` unconditionally made
    // jellyfin-web's htmlVideoPlayer route the direct-play webm URL
    // through hls.js — which then errored with manifestParsingError
    // when it tried to parse the webm bytes as an HLS manifest.
    let transcoding_sub_protocol = if transcoding_url.is_some() {
        Some("hls")
    } else {
        None
    };

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "MediaSources": [{
            "Id": id_str,
            "Path": item.path.to_string_lossy(),
            "Type": "Default",
            "Container": container,
            "IsRemote": false,
            "ETag": "",
            "RunTimeTicks": probe.run_time_ticks(),
            "Size": probe.size_bytes,
            "Name": item.title,
            "Protocol": "File",
            "SupportsDirectPlay": direct_play,
            "SupportsDirectStream": supports_direct_stream,
            "SupportsTranscoding": true,
            "TranscodingUrl": transcoding_url,
            "TranscodingSubProtocol": transcoding_sub_protocol,
            "RequiresOpening": false,
            "RequiresClosing": false,
            "RequiresLooping": false,
            "SupportsProbing": true,
            "MediaStreams": streams,
            "Bitrate": probe.bitrate_bps,
            "VideoType": "VideoFile",
            "DefaultAudioStreamIndex": default_audio_stream_index,
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
    // Honour ParentId so home-page "Latest" rows match the library
    // the user clicked into. Library / series / season ids all
    // resolve via the shared restrict_to_parent helper.
    let scoped = restrict_to_parent(&state, all, q.parent_id.as_deref());
    // Also honour IncludeItemTypes — jellyfin-web's "Latest Movies"
    // row filters to Type=Movie.
    let typed = filter_by_kinds(scoped, q.include_item_types.as_deref());
    let limit = q.limit.min(100) as usize;
    let page: Vec<MediaItem> = typed.into_iter().take(limit).collect();
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

fn filter_by_kinds(items: Vec<MediaItem>, include: Option<&str>) -> Vec<MediaItem> {
    let Some(s) = include else { return items };
    let wanted: Vec<MediaKind> = s
        .split(',')
        .filter_map(|t| jellyfin_type_to_kind(t.trim()))
        .collect();
    if wanted.is_empty() {
        return items;
    }
    items.into_iter().filter(|i| wanted.contains(&i.kind)).collect()
}

async fn user_views(
    state: web::Data<AppState>,
    _user: AuthUser,
    _path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    Ok(HttpResponse::Ok().json(synth_views_body(&state)))
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
    Ok(HttpResponse::Ok().json(synth_views_body(&state)))
}

/// Synthesise a `Folder`/`CollectionFolder` view per configured
/// `[media].roots` entry. The library `Id` is the stable_id of the
/// canonical root path so the same id survives restarts; jellyfin-web
/// stores it in client-side state.
///
/// Zero roots → single "All Media" placeholder so the sidebar still
/// renders (used in tests that hit `AppState::new` without roots).
fn synth_views_body(state: &AppState) -> serde_json::Value {
    let views = library_views(state);
    let count = views.len() as u32;
    serde_json::json!({
        "Items": views,
        "TotalRecordCount": count,
        "StartIndex": 0,
    })
}

fn library_views(state: &AppState) -> Vec<serde_json::Value> {
    if state.media_roots.is_empty() {
        return vec![all_media_placeholder(&state.server_id)];
    }
    state
        .media_roots
        .iter()
        .map(|root| {
            let id = library_id_for_root(root);
            let name = root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("Media")
                .to_string();
            serde_json::json!({
                "Id": id,
                "Name": name,
                "ServerId": state.server_id,
                "Type": "CollectionFolder",
                "CollectionType": "mixed",
                "MediaType": "Unknown",
                "IsFolder": true,
                "UserData": { "Played": false, "PlayCount": 0 },
            })
        })
        .collect()
}

fn all_media_placeholder(server_id: &str) -> serde_json::Value {
    serde_json::json!({
        "Id": "00000000000000000000000000000000",
        "Name": "All Media",
        "ServerId": server_id,
        "Type": "CollectionFolder",
        "CollectionType": "mixed",
        "MediaType": "Unknown",
        "IsFolder": true,
        "UserData": { "Played": false, "PlayCount": 0 },
    })
}

/// 32-char hex id derived from the canonical root path — same input →
/// same id across restarts. Two roots only collide if their xxh3 hashes
/// collide (cryptographically unlikely for any realistic library
/// count).
pub(crate) fn library_id_for_root(path: &std::path::Path) -> String {
    let h = pharos_scanner::stable_id(path);
    // Pad to 32 hex chars so jellyfin-web's uuid-shaped id regex
    // accepts it (some downstream code assumes 32-hex shapes).
    format!("{h:016x}{h:016x}")
}

async fn media_folders(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    let views = library_views(&state);
    let count = views.len() as u32;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": views,
        "TotalRecordCount": count,
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
    /// Library / collection id (one per `[media].roots` entry). When
    /// present, restricts the result set to items whose stored path
    /// lives under the matching root. `00000000…0000` (the
    /// All-Media placeholder) means "no parent filter".
    #[serde(default)]
    parent_id: Option<String>,
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
    let filtered = filter_and_sort(restrict_to_parent(&state, all, q.parent_id.as_deref()), &q);
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
    let filtered = filter_and_sort(restrict_to_parent(&state, all, q.parent_id.as_deref()), &q);
    let dto = paginate(&state, user.0.id, filtered, q.start_index, q.limit).await?;
    Ok(HttpResponse::Ok().json(dto))
}

/// Drop items that don't live under the configured root / series /
/// season mapped to `parent_id`. Unknown `parent_id` → empty list.
/// The All-Media placeholder + `None` pass everything through.
fn restrict_to_parent(
    state: &AppState,
    items: Vec<MediaItem>,
    parent_id: Option<&str>,
) -> Vec<MediaItem> {
    use crate::api::jellyfin::dto::{season_id_for, series_id_for};
    let Some(pid) = parent_id else {
        return items;
    };
    if pid.is_empty() || pid == "00000000000000000000000000000000" {
        return items;
    }
    // 1) Library / root match (per-root collections).
    if let Some(root) = state
        .media_roots
        .iter()
        .find(|r| library_id_for_root(r) == pid)
    {
        return items
            .into_iter()
            .filter(|i| i.path.starts_with(root))
            .collect();
    }
    // 2) Series id → every episode whose series_name hashes to pid.
    if items
        .iter()
        .any(|i| i.series.as_ref().is_some_and(|s| series_id_for(&s.series_name) == pid))
    {
        return items
            .into_iter()
            .filter(|i| {
                i.series
                    .as_ref()
                    .is_some_and(|s| series_id_for(&s.series_name) == pid)
            })
            .collect();
    }
    // 3) Season id → every episode in that (series, season) pair.
    if items.iter().any(|i| {
        i.series.as_ref().is_some_and(|s| {
            s.season_number
                .is_some_and(|n| season_id_for(&s.series_name, n) == pid)
        })
    }) {
        return items
            .into_iter()
            .filter(|i| {
                i.series.as_ref().is_some_and(|s| {
                    s.season_number
                        .is_some_and(|n| season_id_for(&s.series_name, n) == pid)
                })
            })
            .collect();
    }
    // Unknown id — render an empty library.
    Vec::new()
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
    // T-fix-7 follow-up: when the id is one of the synthesised
    // library CollectionFolder ids (32-hex, derived from
    // [media].roots), short-circuit with a CollectionFolder DTO.
    if let Some(view) = library_view_for_id(state, id_str) {
        return Ok(HttpResponse::Ok().json(view));
    }
    if id_str == "00000000000000000000000000000000" {
        return Ok(HttpResponse::Ok().json(all_media_placeholder(&state.server_id)));
    }
    // T-fix-18: synth Series + Season DTOs derived from any Episode
    // item whose series_id / season_id matches. Each requires one
    // store.list() — fine at phase-1 scale; once libraries grow
    // a series_index lands.
    if let Some(view) = synth_series_or_season(state, id_str).await? {
        return Ok(HttpResponse::Ok().json(view));
    }
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

/// If `id_str` matches the library id of any configured root, return
/// the same CollectionFolder JSON that `/Users/{u}/Views` emits.
fn library_view_for_id(state: &AppState, id_str: &str) -> Option<serde_json::Value> {
    library_views(state)
        .into_iter()
        .find(|v| v.get("Id").and_then(|i| i.as_str()) == Some(id_str))
}

/// Look up `id_str` against the synthesised Series + Season ids
/// derived from every Episode in the store. Returns a Jellyfin-shaped
/// Series / Season BaseItem DTO when matched. `None` otherwise.
async fn synth_series_or_season(
    state: &AppState,
    id_str: &str,
) -> Result<Option<serde_json::Value>, actix_web::Error> {
    use crate::api::jellyfin::dto::{season_display_name, season_id_for, series_id_for};
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // First: series match.
    for item in all.iter() {
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        if series_id_for(&series.series_name) == id_str {
            return Ok(Some(series_dto(&state.server_id, &series.series_name)));
        }
    }
    // Then: season match. We need (series_name, season_number) so
    // walk every Episode again.
    for item in all.iter() {
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        let Some(season_n) = series.season_number else {
            continue;
        };
        if season_id_for(&series.series_name, season_n) == id_str {
            return Ok(Some(season_dto(
                &state.server_id,
                &series.series_name,
                season_n,
                &season_display_name(season_n),
            )));
        }
    }
    Ok(None)
}

fn series_dto(server_id: &str, series_name: &str) -> serde_json::Value {
    use crate::api::jellyfin::dto::series_id_for;
    serde_json::json!({
        "Id": series_id_for(series_name),
        "Name": series_name,
        "ServerId": server_id,
        "Type": "Series",
        "MediaType": "Unknown",
        "IsFolder": true,
        "CanPlay": false,
        "UserData": { "Played": false, "PlayCount": 0 },
        // Empty array fields jellyfin-web spreads over.
        "Genres": [], "GenreItems": [], "Tags": [], "Studios": [],
        "ProductionLocations": [], "RemoteTrailers": [], "Chapters": [],
        "ImageTags": {}, "BackdropImageTags": [], "ProviderIds": {},
    })
}

fn season_dto(
    server_id: &str,
    series_name: &str,
    season_number: u32,
    season_name: &str,
) -> serde_json::Value {
    use crate::api::jellyfin::dto::{season_id_for, series_id_for};
    serde_json::json!({
        "Id": season_id_for(series_name, season_number),
        "Name": season_name,
        "ServerId": server_id,
        "Type": "Season",
        "MediaType": "Unknown",
        "IsFolder": true,
        "CanPlay": false,
        "SeriesName": series_name,
        "SeriesId": series_id_for(series_name),
        "IndexNumber": season_number,
        "UserData": { "Played": false, "PlayCount": 0 },
        "Genres": [], "GenreItems": [], "Tags": [], "Studios": [],
        "ImageTags": {}, "BackdropImageTags": [], "ProviderIds": {},
    })
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
