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
        // /items/counts BEFORE /items/{id} so `Counts` doesn't match
        // as an item id.
        .route("/items/counts", web::get().to(items_counts))
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
        "/items/{id}/thememedia",
        "/items/{id}/themesongs",
        "/items/{id}/themevideos",
        "/items/{id}/specialfeatures",
        "/users/{user_id}/items/{item_id}/intros",
        "/shows/upcoming",
        "/persons", // No people metadata yet — phase 3 of T34.
    ] {
        cfg.route(path, web::get().to(empty_items_result));
    }
    cfg.route("/items/{id}/similar", web::get().to(items_similar));
    // /Genres + /Studios aggregate over MediaItem.{genre, album_artist}
    // tags. Replace stub when those columns ship — T-fix-31.
    cfg.route("/genres", web::get().to(list_genres))
        .route("/studios", web::get().to(list_studios))
        // /Artists + /Albums power jellyfin-web's music navigation.
        .route("/artists", web::get().to(list_artists))
        .route("/artists/albumartists", web::get().to(list_artists))
        .route("/albums", web::get().to(list_albums));
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

/// `GET /Items/Counts` — Jellyfin clients render a "library stats"
/// strip on the home page from this. Returns counts by kind +
/// aggregate name counts (Artist/Album/Genre) for the music view.
async fn items_counts(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use std::collections::HashSet;
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut movies = 0u32;
    let mut episodes = 0u32;
    let mut audio = 0u32;
    let mut series: HashSet<&str> = HashSet::new();
    let mut artists: HashSet<&str> = HashSet::new();
    let mut albums: HashSet<&str> = HashSet::new();
    let mut genres: HashSet<&str> = HashSet::new();
    for i in &all {
        match i.kind {
            MediaKind::Movie => movies += 1,
            MediaKind::Episode => {
                episodes += 1;
                if let Some(s) = i.series.as_ref() {
                    series.insert(s.series_name.as_str());
                }
            }
            MediaKind::Audio => audio += 1,
        }
        if let Some(n) = i.probe.artist.as_deref() {
            artists.insert(n);
        }
        if let Some(n) = i.probe.album_artist.as_deref() {
            artists.insert(n);
        }
        if let Some(n) = i.probe.album.as_deref() {
            albums.insert(n);
        }
        if let Some(n) = i.probe.genre.as_deref() {
            genres.insert(n);
        }
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "MovieCount": movies,
        "SeriesCount": series.len() as u32,
        "EpisodeCount": episodes,
        "ArtistCount": artists.len() as u32,
        "ProgramCount": 0,
        "TrailerCount": 0,
        "SongCount": audio,
        "AlbumCount": albums.len() as u32,
        "MusicVideoCount": 0,
        "BoxSetCount": 0,
        "BookCount": 0,
        "ItemCount": all.len() as u32,
        // GenreCount isn't part of jellyfin-web's stats row but
        // clients sometimes read it; cheap to include.
        "GenreCount": genres.len() as u32,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SimilarQuery {
    #[serde(default = "default_similar_limit")]
    limit: u32,
    #[serde(default)]
    #[allow(dead_code)]
    user_id: Option<String>,
}
fn default_similar_limit() -> u32 {
    12
}

/// `GET /Items/{id}/Similar` — "more like this" for the item-detail
/// view. Heuristic, ordered by overlap score:
///
/// - Episode → other episodes in the same Series (excluding self),
///   then other episodes period.
/// - Audio → tracks sharing album → album_artist → genre.
/// - Movie → other Movies tagged with the same genre, falling
///   through to other Movies sorted by title.
async fn items_similar(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: web::Query<SimilarQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let id_str = path.into_inner();
    let id: u64 = match id_str.parse() {
        Ok(v) => v,
        // Synth ids (library / series / season / artist / album / genre)
        // — no "similar" semantics, return empty rather than 4xx.
        Err(_) => {
            return Ok(HttpResponse::Ok().json(serde_json::json!({
                "Items": [], "TotalRecordCount": 0, "StartIndex": 0,
            })));
        }
    };
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let Some(target) = all.iter().find(|i| i.id == id) else {
        return Ok(HttpResponse::Ok().json(serde_json::json!({
            "Items": [], "TotalRecordCount": 0, "StartIndex": 0,
        })));
    };
    // Score every other item by overlap with the target.
    let mut scored: Vec<(u32, &MediaItem)> = all
        .iter()
        .filter(|i| i.id != target.id)
        .map(|i| (similarity_score(target, i), i))
        .filter(|(s, _)| *s > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.cmp(&b.1.title)));
    let picks: Vec<&MediaItem> = scored
        .iter()
        .map(|(_, i)| *i)
        .take(q.limit as usize)
        .collect();
    let ids: Vec<u64> = picks.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.0.id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut dtos: Vec<BaseItemDto> = picks
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let ud = user_data.get(i).copied().unwrap_or_default();
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud)
        })
        .collect();
    fill_parent_ids(&state, &mut dtos, &picks);
    let total = dtos.len() as u32;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": dtos,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// Score `candidate` for similarity to `target`. Higher = more
/// similar. Zero excludes.
fn similarity_score(target: &MediaItem, candidate: &MediaItem) -> u32 {
    let mut s = 0u32;
    // Same Series (Episode) is the strongest signal.
    if let (Some(t), Some(c)) = (target.series.as_ref(), candidate.series.as_ref()) {
        if t.series_name.eq_ignore_ascii_case(&c.series_name) {
            s += 100;
        }
    }
    // Same album_artist (Audio) — same artist's catalogue.
    if let (Some(t), Some(c)) =
        (target.probe.album_artist.as_deref(), candidate.probe.album_artist.as_deref())
    {
        if t.eq_ignore_ascii_case(c) {
            s += 50;
        }
    }
    // Same album (Audio) — same album's tracks.
    if let (Some(t), Some(c)) = (target.probe.album.as_deref(), candidate.probe.album.as_deref()) {
        if t.eq_ignore_ascii_case(c) {
            s += 40;
        }
    }
    // Same genre — broadly works for every kind.
    if let (Some(t), Some(c)) = (target.probe.genre.as_deref(), candidate.probe.genre.as_deref()) {
        if t.eq_ignore_ascii_case(c) {
            s += 20;
        }
    }
    // Same kind — weak signal but stops Movies surfacing as similar
    // to Audio tracks when no other field overlaps.
    if target.kind == candidate.kind {
        s = s.saturating_add(5);
    }
    s
}

/// `GET /Genres` — aggregate every distinct `genre` tag across all
/// items into a flat list. Each entry carries a stable id derived
/// via dto::genre_id_for, so /Items?ParentId={id} pivots cleanly
/// once that branch is wired (currently library/series/season only).
async fn list_genres(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use crate::api::jellyfin::dto::genre_id_for;
    use std::collections::HashSet;
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut genres: Vec<&str> = all
        .iter()
        .filter_map(|i| i.probe.genre.as_deref())
        .filter(|g| !g.is_empty() && seen.insert(g.to_string()))
        .collect();
    genres.sort_unstable();
    let items: Vec<serde_json::Value> = genres
        .iter()
        .map(|g| {
            serde_json::json!({
                "Id": genre_id_for(g),
                "Name": g,
                "ServerId": state.server_id,
                "Type": "Genre",
                "MediaType": "Unknown",
                "IsFolder": true,
            })
        })
        .collect();
    let total = items.len() as u32;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// `GET /Artists` — aggregate artist + album_artist tags. Each
/// entry's `Id` is stable per name; clicking through goes to
/// /Items?ParentId={id} which restrict_to_parent resolves.
async fn list_artists(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use crate::api::jellyfin::dto::artist_id_for;
    use std::collections::HashSet;
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut names: Vec<String> = Vec::new();
    for i in &all {
        for src in [i.probe.album_artist.as_deref(), i.probe.artist.as_deref()] {
            if let Some(n) = src.filter(|s| !s.is_empty()) {
                if seen.insert(n.to_string()) {
                    names.push(n.to_string());
                }
            }
        }
    }
    names.sort();
    let items: Vec<serde_json::Value> = names
        .iter()
        .map(|n| {
            serde_json::json!({
                "Id": artist_id_for(n),
                "Name": n,
                "ServerId": state.server_id,
                "Type": "MusicArtist",
                "MediaType": "Unknown",
                "IsFolder": true,
                "ImageTags": {},
                "BackdropImageTags": [],
                "Genres": [], "Tags": [],
            })
        })
        .collect();
    let total = items.len() as u32;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// `GET /Albums` — aggregate distinct album names. Each entry
/// carries the album_artist so jellyfin-web's track tile renders the
/// "Album • Artist" subtitle without a follow-up fetch.
async fn list_albums(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use crate::api::jellyfin::dto::{album_id_for, artist_id_for};
    use std::collections::HashMap;
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // Map album_name → (album_artist, sample track id) so a click
    // into the album renders with the right artist on the tile.
    let mut albums: HashMap<String, Option<String>> = HashMap::new();
    for i in &all {
        let Some(name) = i.probe.album.as_deref() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let entry = albums.entry(name.to_string()).or_insert(None);
        if entry.is_none() {
            *entry = i
                .probe
                .album_artist
                .clone()
                .or_else(|| i.probe.artist.clone());
        }
    }
    let mut names: Vec<&String> = albums.keys().collect();
    names.sort();
    let items: Vec<serde_json::Value> = names
        .into_iter()
        .map(|n| {
            let artist = albums.get(n).and_then(|a| a.clone());
            let mut v = serde_json::json!({
                "Id": album_id_for(n),
                "Name": n,
                "ServerId": state.server_id,
                "Type": "MusicAlbum",
                "MediaType": "Unknown",
                "IsFolder": true,
                "ImageTags": {},
                "BackdropImageTags": [],
                "Genres": [], "Tags": [],
            });
            if let Some(a) = artist {
                v["AlbumArtist"] = serde_json::Value::String(a.clone());
                v["AlbumArtists"] = serde_json::json!([{
                    "Name": a,
                    "Id": artist_id_for(&a),
                }]);
            }
            v
        })
        .collect();
    let total = items.len() as u32;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// `GET /Studios` — same shape as /Genres but pivoting on
/// album_artist (closest field we persist to a "studio" — Jellyfin's
/// schema overloads the term across music + film). Real studio
/// metadata waits on a metadata-provider layer.
async fn list_studios(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use std::collections::HashSet;
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut studios: Vec<&str> = all
        .iter()
        .filter_map(|i| i.probe.album_artist.as_deref())
        .filter(|s| !s.is_empty() && seen.insert(s.to_string()))
        .collect();
    studios.sort_unstable();
    let items: Vec<serde_json::Value> = studios
        .iter()
        .map(|s| {
            serde_json::json!({
                "Id": crate::api::jellyfin::dto::artist_id_for(s),
                "Name": s,
                "ServerId": state.server_id,
                "Type": "Studio",
                "MediaType": "Unknown",
                "IsFolder": true,
            })
        })
        .collect();
    let total = items.len() as u32;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
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
        .into_values()
        .filter_map(|mut eps| {
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
            // V9: media file paths never leak to clients. Jellyfin's
            // own server omits this for non-admins; pharos omits it
            // wholesale — playback uses the StreamUrl / DirectStreamUrl
            // the client already has, not the on-disk path.
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
    /// Comma-separated UserData filters. Recognised:
    /// `IsFavorite`, `IsNotFavorite`, `IsPlayed`, `IsUnplayed`,
    /// `IsResumable`. Multiple filters AND together. Unknown tokens
    /// are ignored (Jellyfin parity).
    #[serde(default)]
    filters: Option<String>,
    /// Comma-separated list of item ids. When present, restricts the
    /// result set to those items only — clients use this to refresh
    /// a batch of cached items by id. Synth ids (library / series /
    /// season / artist / album / genre) are silently dropped; only
    /// numeric ids in the store match.
    #[serde(default)]
    ids: Option<String>,
    /// Stable seed for `SortBy=Random`. Jellyfin clients pass this so
    /// pagination across a randomised list returns a consistent order
    /// (no duplicates, no holes). Absent → server derives a seed from
    /// the bearer's user id so each user's "Random" view stays stable
    /// within a browse session but differs across users.
    #[serde(default)]
    sort_seed: Option<u64>,
    /// Comma- or pipe-separated genre names. Jellyfin's wire convention
    /// uses `|` between genre names. Items whose `probe.genre` matches
    /// any of the listed names (case-insensitive) pass. Items without
    /// a genre tag are dropped when this filter is active.
    #[serde(default)]
    genres: Option<String>,
    /// Letter-jump nav: jellyfin-web's A-Z chip strip sends
    /// `NameStartsWith=A`. Items whose `title` starts with the given
    /// prefix (case-insensitive) pass.
    #[serde(default)]
    name_starts_with: Option<String>,
    /// Same as `NameStartsWith` semantically, but Jellyfin's
    /// "starts-with-or-greater" letter nav uses this for everything
    /// past Z (numbers, symbols). Same handler — clients use
    /// whichever name they remember.
    #[serde(default)]
    name_starts_with_or_greater: Option<String>,
    /// Strict alphabetic upper bound — `NameLessThan=B` drops items
    /// at or after "B". Used by jellyfin-web's "0-9" letter chip
    /// when paired with NameStartsWith="0". Optional; rarely used.
    #[serde(default)]
    name_less_than: Option<String>,
    /// Inverse of `IncludeItemTypes` — drop items of any listed kind.
    /// Useful when jellyfin-web wants "everything but Episode" in a
    /// movies-only view that happens to share a parent.
    #[serde(default)]
    exclude_item_types: Option<String>,
    /// Comma-separated `Audio` / `Video`. Equivalent to filtering by
    /// MediaKind's media_type — `Video` matches Movie + Episode,
    /// `Audio` matches Audio. Real Jellyfin uses this for the
    /// audio-only / video-only library splits.
    #[serde(default)]
    media_types: Option<String>,
    /// Direct boolean shortcut. Jellyfin's "Favorites" sidebar item
    /// sometimes sends `IsFavorite=true` instead of
    /// `Filters=IsFavorite`. Behaviourally identical — folded into
    /// the same UserData lookup.
    #[serde(default)]
    is_favorite: Option<bool>,
    /// Direct boolean shortcut, sibling of `IsFavorite`. `IsPlayed=true`
    /// returns only items the user has finished; `false` returns
    /// unplayed.
    #[serde(default)]
    is_played: Option<bool>,
    /// Episode-list filters jellyfin-web uses for the season detail
    /// view. `MinIndexNumber=3` drops episodes 1-2; `MaxIndexNumber=5`
    /// drops 6+.
    #[serde(default)]
    min_index_number: Option<u32>,
    #[serde(default)]
    max_index_number: Option<u32>,
    /// `HasSubtitles=true` returns only items with at least one
    /// subtitle track (embedded or sidecar — pharos reads embedded
    /// from `probe.subtitle_tracks`). `false` returns only items
    /// without. Real Jellyfin honours both directions.
    #[serde(default)]
    has_subtitles: Option<bool>,
    /// Resolution-category booleans jellyfin-web's "Quality" filter
    /// chips send. We compare against `probe.width` since aspect
    /// ratios vary and height alone undercounts widescreen content.
    /// Items without width data drop when any of these are active.
    /// The explicit `rename`s honour Jellyfin's case (Is4K with an
    /// uppercase K, Is3D with an uppercase D) — serde's PascalCase
    /// renamer alone produces `Is4k` / `Is3d` which the client never
    /// sends.
    #[serde(default, rename = "Is4K")]
    is_4k: Option<bool>,
    #[serde(default)]
    is_hd: Option<bool>,
    #[serde(default, rename = "Is3D")]
    is_3d: Option<bool>,
    /// Explicit min/max bounds on the source video width. Lets a
    /// power user pick a 1440p cutoff that the canned chips miss.
    #[serde(default)]
    min_width: Option<u32>,
    #[serde(default)]
    max_width: Option<u32>,
}

#[derive(Debug, Default, Clone, Copy)]
struct UserDataFilter {
    is_favorite: Option<bool>,
    is_played: Option<bool>,
    is_resumable: bool,
}

impl UserDataFilter {
    fn parse(raw: &str) -> Self {
        let mut f = Self::default();
        for tok in raw.split(',').map(str::trim) {
            match tok {
                "IsFavorite" => f.is_favorite = Some(true),
                "IsNotFavorite" => f.is_favorite = Some(false),
                "IsPlayed" => f.is_played = Some(true),
                "IsUnplayed" => f.is_played = Some(false),
                "IsResumable" => f.is_resumable = true,
                _ => {}
            }
        }
        f
    }

    fn is_active(&self) -> bool {
        self.is_favorite.is_some() || self.is_played.is_some() || self.is_resumable
    }

    fn matches(&self, ud: pharos_core::UserItemData) -> bool {
        if let Some(want) = self.is_favorite {
            if ud.is_favorite != want {
                return false;
            }
        }
        if let Some(want) = self.is_played {
            if ud.played != want {
                return false;
            }
        }
        if self.is_resumable && (ud.played || ud.last_played_position_ticks == 0) {
            return false;
        }
        true
    }
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
    let seed = effective_sort_seed(&q, user.0.id);
    let filtered = filter_and_sort(
        restrict_to_parent(&state, all, q.parent_id.as_deref()),
        &q,
        seed,
    );
    let after_ud = apply_userdata_filter(&state, user.0.id, filtered, &q).await?;
    let dto = paginate(&state, user.0.id, after_ud, q.start_index, q.limit).await?;
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
    let seed = effective_sort_seed(&q, user.0.id);
    let filtered = filter_and_sort(
        restrict_to_parent(&state, all, q.parent_id.as_deref()),
        &q,
        seed,
    );
    let after_ud = apply_userdata_filter(&state, user.0.id, filtered, &q).await?;
    let dto = paginate(&state, user.0.id, after_ud, q.start_index, q.limit).await?;
    Ok(HttpResponse::Ok().json(dto))
}

/// Returns the seed used for `SortBy=Random`. When the client passes
/// `SortSeed`, honour it (clients use this to keep pagination stable
/// across requests). Otherwise derive from the bearer's user id so
/// the order is stable per-user within a session.
fn effective_sort_seed(q: &ListQuery, user_id: UserId) -> u64 {
    if let Some(s) = q.sort_seed {
        return s | 1;
    }
    // Fold UUID bytes into a u64 — `as u64` slice is enough for a
    // shuffle seed; cryptographic strength is not required.
    let bytes = user_id.0.as_bytes();
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf) | 1
}

/// Filter the list by per-user UserData state (favorite / played /
/// resumable). One bulk UserData lookup for the full set when active;
/// no extra IO when the Filters parameter is empty.
async fn apply_userdata_filter(
    state: &AppState,
    user_id: UserId,
    items: Vec<MediaItem>,
    q: &ListQuery,
) -> Result<Vec<MediaItem>, actix_web::Error> {
    let mut f = match q.filters.as_deref() {
        Some(raw) => UserDataFilter::parse(raw),
        None => UserDataFilter::default(),
    };
    // Direct boolean shortcuts fold into the same filter so a single
    // bulk-userdata lookup serves both wire conventions.
    if let Some(v) = q.is_favorite {
        f.is_favorite = Some(v);
    }
    if let Some(v) = q.is_played {
        f.is_played = Some(v);
    }
    if !f.is_active() {
        return Ok(items);
    }
    let ids: Vec<u64> = items.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user_id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let kept: Vec<MediaItem> = items
        .into_iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let ud = user_data.get(i).copied().unwrap_or_default();
            if f.matches(ud) {
                Some(item)
            } else {
                None
            }
        })
        .collect();
    Ok(kept)
}

/// Drop items that don't live under the configured root / series /
/// season mapped to `parent_id`. Unknown `parent_id` → empty list.
/// The All-Media placeholder + `None` pass everything through.
fn restrict_to_parent(
    state: &AppState,
    items: Vec<MediaItem>,
    parent_id: Option<&str>,
) -> Vec<MediaItem> {
    use crate::api::jellyfin::dto::{
        album_id_for, artist_id_for, genre_id_for, season_id_for, series_id_for,
    };
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
    // 4) Artist id → every track whose artist or album_artist matches.
    if items.iter().any(|i| {
        i.probe
            .artist
            .as_deref()
            .is_some_and(|a| artist_id_for(a) == pid)
            || i.probe
                .album_artist
                .as_deref()
                .is_some_and(|a| artist_id_for(a) == pid)
    }) {
        return items
            .into_iter()
            .filter(|i| {
                i.probe
                    .artist
                    .as_deref()
                    .is_some_and(|a| artist_id_for(a) == pid)
                    || i.probe
                        .album_artist
                        .as_deref()
                        .is_some_and(|a| artist_id_for(a) == pid)
            })
            .collect();
    }
    // 5) Album id → every track whose album hashes to pid.
    if items
        .iter()
        .any(|i| i.probe.album.as_deref().is_some_and(|a| album_id_for(a) == pid))
    {
        return items
            .into_iter()
            .filter(|i| {
                i.probe.album.as_deref().is_some_and(|a| album_id_for(a) == pid)
            })
            .collect();
    }
    // 6) Genre id → every item tagged with that genre.
    if items
        .iter()
        .any(|i| i.probe.genre.as_deref().is_some_and(|g| genre_id_for(g) == pid))
    {
        return items
            .into_iter()
            .filter(|i| {
                i.probe.genre.as_deref().is_some_and(|g| genre_id_for(g) == pid)
            })
            .collect();
    }
    // Unknown id — render an empty library.
    Vec::new()
}

fn filter_and_sort(mut items: Vec<MediaItem>, q: &ListQuery, sort_seed: u64) -> Vec<MediaItem> {
    if let Some(raw) = q.ids.as_ref() {
        // 32-char synth ids (library / series / season / artist /
        // album / genre) live in a different namespace from numeric
        // store ids. Treat anything longer than u64::MAX's 20-digit
        // bound as a non-match — `"00000…001".parse()` otherwise
        // collides with id 1.
        let wanted: std::collections::HashSet<u64> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| s.len() <= 20)
            .filter_map(|s| s.parse::<u64>().ok())
            .collect();
        // Empty `Ids=` (or all-non-numeric, eg only synth ids) → no
        // matches. Matches Jellyfin's "you asked for nothing, you
        // get nothing" semantics.
        items.retain(|i| wanted.contains(&i.id));
    }
    if let Some(term) = q.search_term.as_ref() {
        // Unicode-aware lowercase so titles with accents ("Pokémon",
        // "Café") match queries typed in different case. ASCII-only
        // `to_ascii_lowercase` left the É / é alone and silently
        // dropped the match.
        let needle = term.to_lowercase();
        if !needle.is_empty() {
            items.retain(|i| i.title.to_lowercase().contains(&needle));
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
    if let Some(types) = q.exclude_item_types.as_ref() {
        let blocked: Vec<MediaKind> = types
            .split(',')
            .filter_map(|s| jellyfin_type_to_kind(s.trim()))
            .collect();
        if !blocked.is_empty() {
            items.retain(|i| !blocked.contains(&i.kind));
        }
    }
    if let Some(raw) = q.media_types.as_ref() {
        let want_audio = raw.split(',').any(|s| s.trim().eq_ignore_ascii_case("Audio"));
        let want_video = raw.split(',').any(|s| s.trim().eq_ignore_ascii_case("Video"));
        if want_audio || want_video {
            items.retain(|i| match i.kind {
                MediaKind::Audio => want_audio,
                MediaKind::Movie | MediaKind::Episode => want_video,
            });
        }
    }
    if let Some(want) = q.has_subtitles {
        items.retain(|i| i.probe.subtitle_tracks.is_empty() != want);
    }
    // Resolution filters: "4K" ≥ 3840, "HD" 1280..3840, "SD" < 1280.
    // Width-based — height alone undercounts widescreen content.
    if let Some(want) = q.is_4k {
        items.retain(|i| i.probe.width.map(|w| w >= 3840) == Some(want));
    }
    if let Some(want) = q.is_hd {
        items.retain(|i| {
            i.probe
                .width
                .map(|w| (1280..3840).contains(&w))
                == Some(want)
        });
    }
    if let Some(want) = q.is_3d {
        // No 3D detection in the prober yet; report false for every
        // item so `Is3D=false` returns everything and `Is3D=true`
        // returns nothing. Lets clients tick the chip without 500'ing.
        items.retain(|_| !want);
    }
    if let Some(min) = q.min_width {
        items.retain(|i| i.probe.width.is_some_and(|w| w >= min));
    }
    if let Some(max) = q.max_width {
        items.retain(|i| i.probe.width.is_some_and(|w| w <= max));
    }
    if let Some(min) = q.min_index_number {
        items.retain(|i| {
            i.series
                .as_ref()
                .and_then(|s| s.episode_number)
                .map(|n| n >= min)
                .unwrap_or(false)
        });
    }
    if let Some(max) = q.max_index_number {
        items.retain(|i| {
            i.series
                .as_ref()
                .and_then(|s| s.episode_number)
                .map(|n| n <= max)
                .unwrap_or(false)
        });
    }
    if let Some(prefix) = q
        .name_starts_with
        .as_deref()
        .or(q.name_starts_with_or_greater.as_deref())
    {
        let lower = prefix.to_lowercase();
        if !lower.is_empty() {
            items.retain(|i| i.title.to_lowercase().starts_with(&lower));
        }
    }
    if let Some(bound) = q.name_less_than.as_deref() {
        let lower = bound.to_lowercase();
        if !lower.is_empty() {
            items.retain(|i| i.title.to_lowercase().as_str() < lower.as_str());
        }
    }
    if let Some(raw) = q.genres.as_ref() {
        // Jellyfin's wire convention splits genres on `|` but some
        // clients use `,`. Accept both — empty tokens (trailing
        // separator) are ignored.
        let wanted: std::collections::HashSet<String> = raw
            .split(['|', ','])
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if !wanted.is_empty() {
            items.retain(|i| {
                i.probe
                    .genre
                    .as_deref()
                    .map(|g| wanted.contains(&g.to_lowercase()))
                    .unwrap_or(false)
            });
        }
    }
    let sort_by = q.sort_by.as_deref().unwrap_or("SortName");
    let descending = matches!(q.sort_order.as_deref(), Some("Descending"));
    // Jellyfin's SortBy accepts a comma-separated chain — clients use
    // it for stable secondary keys (e.g. "AlbumArtist,Album,SortName").
    // We honour the first token that maps to a known key, then fall
    // through to SortName so identical-by-primary items stay stable.
    let primary = sort_by
        .split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("SortName");
    match primary {
        "Random" => shuffle_in_place(&mut items, sort_seed),
        "DateCreated" | "DateAdded" => {
            // Newest-first by default. Items without a created_at
            // (pre-migration-0010 rows) sort to the end of the
            // descending order via `unwrap_or(0)`.
            items.sort_by(|a, b| {
                b.created_at
                    .unwrap_or(0)
                    .cmp(&a.created_at.unwrap_or(0))
                    .then(b.id.cmp(&a.id))
            });
            if descending {
                items.reverse();
            }
        }
        "RuntimeTicks" | "Runtime" => {
            items.sort_by(|a, b| {
                a.probe
                    .duration_ms
                    .unwrap_or(0)
                    .cmp(&b.probe.duration_ms.unwrap_or(0))
            });
            if descending {
                items.reverse();
            }
        }
        "AlbumArtist" => {
            // Unicode-aware case-fold so accented artist names sort
            // consistently against ASCII ones (Ärzte vs Adele).
            items.sort_by(|a, b| {
                let an = a.probe.album_artist.as_deref().unwrap_or("");
                let bn = b.probe.album_artist.as_deref().unwrap_or("");
                an.to_lowercase()
                    .cmp(&bn.to_lowercase())
                    // Tiebreak by title for stable per-artist track order.
                    .then(a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            });
            if descending {
                items.reverse();
            }
        }
        "Album" => {
            items.sort_by(|a, b| {
                let an = a.probe.album.as_deref().unwrap_or("");
                let bn = b.probe.album.as_deref().unwrap_or("");
                an.to_lowercase()
                    .cmp(&bn.to_lowercase())
                    .then(a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            });
            if descending {
                items.reverse();
            }
        }
        // SortName (default) and anything unrecognised.
        _ => {
            items.sort_by(|a, b| {
                a.title
                    .to_lowercase()
                    .cmp(&b.title.to_lowercase())
            });
            if descending {
                items.reverse();
            }
        }
    }
    items
}

fn jellyfin_type_to_kind(s: &str) -> Option<MediaKind> {
    // Case-insensitive — some clients send lowercase (Finamp's
    // `audio`), real Jellyfin accepts both. Server-side ascii-fold
    // is fine: these are all ASCII identifiers.
    match s.to_ascii_lowercase().as_str() {
        "movie" => Some(MediaKind::Movie),
        "episode" => Some(MediaKind::Episode),
        "audio" => Some(MediaKind::Audio),
        _ => None,
    }
}

/// xorshift64 Fisher–Yates. Deterministic for a given seed — same
/// `(items, seed)` yields the same permutation. Caller threads a
/// seed (from `?SortSeed=` or the bearer's user id) so /Items?
/// SortBy=Random pagination doesn't reshuffle between page requests.
fn shuffle_in_place(items: &mut [MediaItem], seed: u64) {
    // xorshift64 reqs non-zero state; caller forces low bit.
    let mut state = seed | 1;
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

/// Library-root id whose path prefixes this item, if any. Used to
/// post-fill `BaseItemDto.parent_id` for top-level items.
fn library_parent_id(state: &AppState, item: &MediaItem) -> Option<String> {
    state
        .media_roots
        .iter()
        .find(|r| item.path.starts_with(r))
        .map(|r| library_id_for_root(r))
}

/// Walk a Vec<BaseItemDto> + matching items, post-filling ParentId
/// with the library root id when the DTO didn't already set
/// SeasonId / AlbumId.
fn fill_parent_ids(state: &AppState, dtos: &mut [BaseItemDto], items: &[&MediaItem]) {
    for (dto, item) in dtos.iter_mut().zip(items.iter()) {
        if dto.parent_id.is_none() {
            dto.parent_id = library_parent_id(state, item);
        }
    }
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
    let mut items: Vec<BaseItemDto> = slice
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let ud = user_data.get(i).copied().unwrap_or_default();
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud)
        })
        .collect();
    let refs: Vec<&pharos_core::MediaItem> = slice.iter().collect();
    fill_parent_ids(state, &mut items, &refs);
    Ok(ItemsResultDto {
        items,
        total_record_count: total,
        start_index,
    })
}
