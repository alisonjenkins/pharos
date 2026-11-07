//! Jellyfin's "Search for subtitles" dialog, backed by OpenSubtitles (spec 030,
//! ADR-0069).
//!
//! - `GET  /Items/{id}/RemoteSearch/Subtitles/{lang}` lists candidates as
//!   `RemoteSubtitleInfo[]`, hash matches first.
//! - `POST /Items/{id}/RemoteSearch/Subtitles/{subtitleId}` downloads one onto
//!   the cache volume, where it joins the item's external subtitle streams.
//!
//! With no API key (or no cache directory) the feature is off: search answers
//! an empty list, download answers 400, and the provider is never called
//! (V563).

use crate::api::jellyfin::auth_extractor::AuthUser;
use crate::opensubtitles::telemetry::{provider_language, record_download, record_search};
use crate::opensubtitles::{hash, store, OpenSubtitles, ProviderError, SearchQuery, SubtitleHit};
use crate::state::AppState;
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{MediaItem, MediaKind, MediaStore, SeriesMetadataStore};
use pharos_jellyfin_api::dto::RemoteSubtitleInfoDto;

const PROVIDER_NAME: &str = "OpenSubtitles";

/// `Id` handed to the client: the OpenSubtitles file id and the language the
/// search asked for (the POST carries nothing else), as `<file id>-<language>`.
fn subtitle_id(hit: &SubtitleHit, language: &str) -> String {
    format!("{}-{}", hit.file_id, language)
}

fn parse_subtitle_id(raw: &str) -> Option<(u64, String)> {
    let (file_id, language) = raw.split_once('-')?;
    let language = provider_language(language)?;
    Some((file_id.parse().ok()?, language))
}

/// The numeric part of an IMDb id (`tt0111161` is `111161`).
fn imdb_number(id: &str) -> Option<u64> {
    id.trim().trim_start_matches("tt").parse().ok()
}

async fn build_query(state: &AppState, item: &MediaItem, language: String) -> SearchQuery {
    let ids = &item.metadata.provider_ids;
    let mut q = SearchQuery {
        imdb_id: ids.imdb.as_deref().and_then(imdb_number),
        tmdb_id: ids.tmdb.as_deref().and_then(|s| s.trim().parse().ok()),
        language,
        ..Default::default()
    };
    if matches!(item.kind, MediaKind::Movie | MediaKind::Episode) {
        match hash::moviehash(&item.path).await {
            Ok(h) => q.moviehash = h,
            Err(e) => {
                tracing::warn!(item = item.id, error = %e, "opensubtitles: could not hash the media file");
            }
        }
    }
    if let (MediaKind::Episode, Some(series)) = (item.kind, item.series.as_ref()) {
        // A show-level TMDB id is the parent id the API wants. Without one the
        // season/episode numbers mean nothing, so they are left out.
        let key = series.series_key().to_string();
        if let Ok(map) = state
            .stores
            .series_metadata_by_keys(std::slice::from_ref(&key))
            .await
        {
            let parent_tmdb = map.get(&key).and_then(|m| {
                if m.match_provider.as_deref() == Some("tmdb") {
                    m.match_external_id
                        .as_deref()
                        .and_then(|s| s.trim().parse().ok())
                } else {
                    None
                }
            });
            if parent_tmdb.is_some() {
                q.parent_tmdb_id = parent_tmdb;
                q.season = series.season_number;
                q.episode = series.episode_number;
            }
        }
    }
    q
}

/// Hash matches first, then trusted uploaders, then popularity.
fn rank(hits: &mut [SubtitleHit]) {
    hits.sort_by(|a, b| {
        b.hash_match
            .cmp(&a.hash_match)
            .then(b.from_trusted.cmp(&a.from_trusted))
            .then(b.download_count.cmp(&a.download_count))
            .then(a.file_id.cmp(&b.file_id))
    });
}

fn to_dto(hit: SubtitleHit, requested: &str) -> RemoteSubtitleInfoDto {
    RemoteSubtitleInfoDto {
        three_letter_iso_language_name: three_letter(&hit.language),
        id: subtitle_id(&hit, requested),
        provider_name: PROVIDER_NAME.to_string(),
        name: hit.release.clone(),
        format: "srt".to_string(),
        author: hit.uploader,
        comment: None,
        date_created: hit.upload_date,
        community_rating: Some(hit.ratings),
        frame_rate: hit.fps,
        download_count: hit.download_count,
        is_hash_match: hit.hash_match,
        ai_translated: hit.ai_translated,
        machine_translated: hit.machine_translated,
        forced: hit.foreign_parts_only,
        hearing_impaired: hit.hearing_impaired,
    }
}

fn three_letter(language: &str) -> Option<String> {
    let two = language.split('-').next()?.to_ascii_lowercase();
    let code = match two.as_str() {
        "en" => "eng",
        "ja" => "jpn",
        "fr" => "fra",
        "de" => "deu",
        "es" => "spa",
        "it" => "ita",
        "pt" => "por",
        "ko" => "kor",
        "zh" => "zho",
        "ru" => "rus",
        "nl" => "nld",
        "pl" => "pol",
        "sv" => "swe",
        "no" => "nor",
        "da" => "dan",
        "fi" => "fin",
        "tr" => "tur",
        "ar" => "ara",
        "he" => "heb",
        "hi" => "hin",
        "cs" => "ces",
        "hu" => "hun",
        "el" => "ell",
        "ro" => "ron",
        "th" => "tha",
        "vi" => "vie",
        "uk" => "ukr",
        "id" => "ind",
        _ => return None,
    };
    Some(code.to_string())
}

async fn item_by_id(state: &AppState, raw: &str) -> Result<MediaItem, actix_web::Error> {
    let id = pharos_jellyfin_api::dto::parse_item_id(raw)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })
}

fn provider_error(e: &ProviderError) -> actix_web::Error {
    match e {
        ProviderError::QuotaExceeded { reset_unix } => error::ErrorTooManyRequests(format!(
            "OpenSubtitles download quota used up until unix time {reset_unix}"
        )),
        ProviderError::RateLimited { retry_after_secs } => error::ErrorTooManyRequests(format!(
            "OpenSubtitles rate limit; retry in {retry_after_secs}s"
        )),
        other => error::ErrorBadGateway(other.to_string()),
    }
}

/// Core of the search, callable from tests without `web::Path` extraction.
pub async fn search_inner(
    state: &AppState,
    raw_id: &str,
    raw_language: &str,
) -> Result<Vec<RemoteSubtitleInfoDto>, actix_web::Error> {
    let Some(os) = state.opensubtitles.as_ref() else {
        return Ok(Vec::new());
    };
    let item = item_by_id(state, raw_id).await?;
    let Some(language) = provider_language(raw_language) else {
        tracing::info!(
            item = item.id,
            language = raw_language,
            "opensubtitles: unknown language, no search"
        );
        return Ok(Vec::new());
    };
    let query = build_query(state, &item, language.clone()).await;
    let result = os.provider.search(&query).await;
    record_search(&result);
    match result {
        Ok(mut hits) => {
            tracing::info!(
                item = item.id,
                language = %language,
                hashed = query.moviehash.is_some(),
                imdb = query.imdb_id,
                tmdb = query.tmdb_id,
                results = hits.len(),
                "opensubtitles: search"
            );
            rank(&mut hits);
            Ok(hits.into_iter().map(|h| to_dto(h, &language)).collect())
        }
        Err(e) => {
            tracing::warn!(item = item.id, language = %language, error = %e, "opensubtitles: search failed");
            Err(provider_error(&e))
        }
    }
}

/// Core of the download.
pub async fn download_inner(
    state: &AppState,
    raw_id: &str,
    raw_subtitle_id: &str,
) -> Result<(), actix_web::Error> {
    let Some(os) = state.opensubtitles.as_ref() else {
        return Err(error::ErrorBadRequest("OpenSubtitles is not configured"));
    };
    let item = item_by_id(state, raw_id).await?;
    let (file_id, language) = parse_subtitle_id(raw_subtitle_id)
        .ok_or_else(|| error::ErrorBadRequest("invalid subtitle id"))?;
    save_download(os, &item, file_id, &language).await
}

async fn save_download(
    os: &OpenSubtitles,
    item: &MediaItem,
    file_id: u64,
    language: &str,
) -> Result<(), actix_web::Error> {
    let result = os.provider.download(file_id).await;
    record_download(&result);
    let downloaded = result.map_err(|e| {
        tracing::warn!(item = item.id, file_id, language, error = %e, "opensubtitles: download failed");
        provider_error(&e)
    })?;
    if downloaded.bytes.is_empty() {
        return Err(error::ErrorBadGateway(
            "OpenSubtitles returned an empty file",
        ));
    }
    let path = store::save(&os.root, &item.path, language, file_id, &downloaded.bytes)
        .await
        .map_err(|e| {
            tracing::error!(item = item.id, file_id, error = %e, "opensubtitles: could not store the download");
            error::ErrorInternalServerError("could not store the subtitle")
        })?;
    tracing::info!(
        item = item.id,
        file_id,
        language,
        bytes = downloaded.bytes.len(),
        remaining = downloaded.remaining,
        path = %path.display(),
        "opensubtitles: downloaded"
    );
    Ok(())
}

/// `GET /Items/{id}/RemoteSearch/Subtitles/{lang}`.
pub async fn remote_subtitle_search(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let (id, language) = path.into_inner();
    let found = search_inner(&state, &id, &language).await?;
    Ok(crate::api::jellyfin::wire::json(&found))
}

/// `POST /Items/{id}/RemoteSearch/Subtitles/{subtitleId}`.
pub async fn remote_subtitle_download(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let (id, subtitle_id) = path.into_inner();
    download_inner(&state, &id, &subtitle_id).await?;
    Ok(HttpResponse::NoContent().finish())
}
