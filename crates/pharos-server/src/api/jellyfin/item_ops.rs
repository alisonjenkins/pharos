//! T76 — assorted item operations jellyfin-web exposes on the item detail
//! page and context menus: merge alternate versions, override content type,
//! remote metadata search (images / subtitles), lyrics, and instant mix.
//!
//! pharos has no external metadata-provider subsystem, so the remote-search
//! endpoints return honest empty results (a stock Jellyfin with no providers
//! configured does the same) rather than 404-ing the client's fetch. Lyrics
//! are served from an `.lrc` sidecar next to the audio file when present.
//! InstantMix is a real same-kind mix drawn from the library.

use crate::api::jellyfin::ci_query::CiQuery;
use crate::online_enrich::OnlineEnricher;
use crate::tmdb::{TmdbClient, TmdbEnricher};
use crate::tvdb::{ReqwestTransport, TvdbClient, TvdbEnricher};
use crate::{
    api::jellyfin::{auth_extractor::AuthUser, items},
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_scanner::FilenameProvider;
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/videos/mergeversions", web::post().to(merge_versions))
        .route("/items/{id}/contenttype", web::post().to(set_content_type))
        .route("/items/{id}/remoteimages", web::get().to(remote_images))
        .route(
            "/items/{id}/remoteimages/providers",
            web::get().to(remote_image_providers),
        )
        .route(
            "/items/{id}/remoteimages/download",
            web::post().to(remote_images_download),
        )
        .route(
            "/items/{id}/remotesearch/subtitles/{lang}",
            web::get().to(remote_subtitle_search),
        )
        .route("/audio/{id}/lyrics", web::get().to(get_lyrics))
        .route("/items/{id}/instantmix", web::get().to(instant_mix))
        .route("/items/{id}/metadataeditor", web::get().to(metadata_editor))
        .route(
            "/items/{id}/remotesearch/movie",
            web::get().to(remote_search_movie),
        )
        .route(
            "/items/{id}/remotesearch/series",
            web::get().to(remote_search_series),
        )
        .route(
            "/items/{id}/remotesearch/apply",
            web::post().to(remote_search_apply),
        );
}

/// `GET /Items/{id}/MetadataEditor` (T67) — the bundle jellyfin-web's
/// metadata editor loads to build its form: the culture picker, the
/// external-id fields (Imdb/Tmdb/Tvdb), the parental-rating + country
/// options, and the item's current content type. pharos serves the same
/// static option catalogue everywhere; the per-item bits (ContentType) are
/// derived from the item's kind.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct ContentTypeOptionDto {
    name: &'static str,
    value: &'static str,
}

const CONTENT_TYPE_OPTIONS: &[ContentTypeOptionDto] = &[
    ContentTypeOptionDto {
        name: "Movies",
        value: "movies",
    },
    ContentTypeOptionDto {
        name: "Shows",
        value: "tvshows",
    },
    ContentTypeOptionDto {
        name: "Music",
        value: "music",
    },
];

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct CountryInfoDto {
    name: &'static str,
    display_name: &'static str,
    #[serde(rename = "TwoLetterISORegionName")]
    two_letter_iso_region_name: &'static str,
    #[serde(rename = "ThreeLetterISORegionName")]
    three_letter_iso_region_name: &'static str,
}

const METADATA_EDITOR_COUNTRIES: &[CountryInfoDto] = &[CountryInfoDto {
    name: "US",
    display_name: "United States",
    two_letter_iso_region_name: "US",
    three_letter_iso_region_name: "USA",
}];

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct MetadataEditorInfoDto {
    content_type: &'static str,
    content_type_options: &'static [ContentTypeOptionDto],
    cultures: &'static [crate::api::jellyfin::system::Culture],
    countries: &'static [CountryInfoDto],
    parental_rating_options: &'static [()],
    external_id_infos: &'static [ExternalIdInfo],
}

async fn metadata_editor(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let content_type = match item.kind {
        pharos_core::MediaKind::Movie => "Movies",
        pharos_core::MediaKind::Episode => "tvshows",
        pharos_core::MediaKind::Audio => "music",
    };
    Ok(crate::api::jellyfin::wire::json(&MetadataEditorInfoDto {
        content_type,
        content_type_options: CONTENT_TYPE_OPTIONS,
        cultures: crate::api::jellyfin::system::LOCALIZATION_CULTURES,
        countries: METADATA_EDITOR_COUNTRIES,
        parental_rating_options: &[],
        external_id_infos: EXTERNAL_ID_INFOS,
    }))
}

/// The external metadata id fields the editor exposes. `UrlFormatString`'s
/// `{0}` is where jellyfin-web substitutes the id to build the outbound
/// link — the same set pharos already emits as `ExternalUrls`/`ProviderIds`.
const EXTERNAL_ID_INFOS: &[ExternalIdInfo] = &[
    ExternalIdInfo::new("IMDb", "Imdb", "https://www.imdb.com/title/{0}"),
    ExternalIdInfo::new("TheMovieDb", "Tmdb", "https://www.themoviedb.org/movie/{0}"),
    ExternalIdInfo::new("TheTVDB", "Tvdb", "https://thetvdb.com/?tab=series&id={0}"),
];

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct ExternalIdInfo {
    name: &'static str,
    key: &'static str,
    #[serde(rename = "Type")]
    id_type: Option<&'static str>,
    url_format_string: &'static str,
}

impl ExternalIdInfo {
    const fn new(name: &'static str, key: &'static str, url: &'static str) -> Self {
        Self {
            name,
            key,
            id_type: None,
            url_format_string: url,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case", default)]
struct MergeVersionsQuery {
    ids: Option<String>,
}

/// `POST /Videos/MergeVersions?Ids=` — Jellyfin groups several items as
/// alternate versions of one title. pharos treats every file as its own
/// item and has no alternate-version grouping model, so this validates the
/// referenced ids exist and accepts the request (204) without collapsing
/// them — the honest maximum until a versions model lands. Rejects a merge
/// naming fewer than two real items so a malformed call is a clear 400.
async fn merge_versions(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<MergeVersionsQuery>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let ids = items::parse_id_csv(q.ids.as_deref());
    if ids.len() < 2 {
        return Err(error::ErrorBadRequest(
            "MergeVersions needs at least two Ids",
        ));
    }
    let mut found = 0usize;
    for id in &ids {
        if state.stores.get(*id).await.is_ok() {
            found += 1;
        }
    }
    if found < 2 {
        return Err(error::ErrorNotFound(
            "fewer than two of the given Ids resolve to items",
        ));
    }
    tracing::info!(
        ids = ?ids,
        "MergeVersions accepted (pharos has no alternate-version grouping; no-op)"
    );
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case", default)]
struct ContentTypeQuery {
    content_type: Option<String>,
}

/// `POST /Items/{id}/ContentType?contentType=` — Jellyfin lets an admin
/// override a file's inferred type. pharos derives an item's kind from its
/// probe (not an override column), so this validates the target exists and
/// accepts the request (204); the kind stays probe-derived.
async fn set_content_type(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<ContentTypeQuery>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    tracing::info!(
        media.id = id,
        content_type = q.content_type.as_deref().unwrap_or(""),
        "ContentType override accepted (pharos derives kind from probe; no-op)"
    );
    Ok(HttpResponse::NoContent().finish())
}

/// One image in a `GET /Items/{id}/RemoteImages` result — Jellyfin's
/// `RemoteImageInfo`. `RatingType` is always `"Score"` (both providers give a
/// numeric score, not a like/dislike).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteImageInfoDto {
    provider_name: String,
    url: String,
    #[serde(rename = "Type")]
    image_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    community_rating: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vote_count: Option<u32>,
    rating_type: &'static str,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteImagesResultDto {
    images: Vec<RemoteImageInfoDto>,
    total_record_count: u32,
    providers: Vec<String>,
}

/// Query for `GET /Items/{id}/RemoteImages` (bound case-insensitively via
/// [`CiQuery`], so `Type`/`type` and `ProviderName`/`providerName` all work).
#[derive(Debug, Default, Deserialize)]
struct RemoteImagesQuery {
    #[serde(rename = "type", default)]
    image_type: Option<String>,
    #[serde(default)]
    provider_name: Option<String>,
}

/// Query for `POST /Items/{id}/RemoteImages/Download`.
#[derive(Debug, Deserialize)]
struct RemoteImageDownloadQuery {
    #[serde(rename = "type")]
    image_type: String,
    image_url: String,
    #[serde(default)]
    provider_name: Option<String>,
}

/// Jellyfin display name for an internal provider token.
fn provider_display(token: &str) -> &'static str {
    match token {
        "tmdb" => "TheMovieDb",
        _ => "TheTVDB",
    }
}

/// Jellyfin display name (or internal token) → internal provider token.
fn provider_token(display: &str) -> Option<&'static str> {
    match display.to_ascii_lowercase().as_str() {
        "themoviedb" | "tmdb" => Some("tmdb"),
        "thetvdb" | "tvdb" => Some("tvdb"),
        _ => None,
    }
}

/// A requested Jellyfin `Type` token → [`pharos_core::ArtworkRole`], restricted
/// to the three roles the picker supports. `None` for any other type (→ 400 on
/// download). `ArtworkRole` has no `from_str_ci`; `ImageRole` does, so parse via
/// it.
fn artwork_role_from_type(t: &str) -> Option<pharos_core::ArtworkRole> {
    use pharos_cache::image_cache::ImageRole;
    match ImageRole::from_str_ci(t)? {
        ImageRole::Primary => Some(pharos_core::ArtworkRole::Primary),
        ImageRole::Backdrop => Some(pharos_core::ArtworkRole::Backdrop),
        ImageRole::Logo => Some(pharos_core::ArtworkRole::Logo),
        _ => None,
    }
}

/// The provider + id to enumerate images from, for a real item or a synth
/// Series container. `None` when nothing is matched (→ empty list).
struct ImageMatch {
    provider: &'static str, // "tmdb" | "tvdb"
    external_id: String,
    kind: MediaKind,
}

/// Resolve the (provider, id, kind) to list images for.
/// - Real item: whichever `metadata.provider_ids` is set (tmdb preferred).
/// - Synth Series: the `series_metadata` row via `series_key`.
async fn resolve_image_match(state: &AppState, id_str: &str) -> Option<ImageMatch> {
    use pharos_core::SeriesMetadataStore;
    // Real numeric id?
    if let Some(id) = pharos_jellyfin_api::dto::parse_item_id(id_str) {
        if let Ok(item) = state.stores.get(id).await {
            let ids = &item.metadata.provider_ids;
            if let Some(tmdb) = ids.tmdb.as_deref().filter(|s| !s.is_empty()) {
                return Some(ImageMatch {
                    provider: "tmdb",
                    external_id: tmdb.to_string(),
                    kind: item.kind,
                });
            }
            if let Some(tvdb) = ids.tvdb.as_deref().filter(|s| !s.is_empty()) {
                return Some(ImageMatch {
                    provider: "tvdb",
                    external_id: tvdb.to_string(),
                    kind: item.kind,
                });
            }
            return None;
        }
    }
    // Synth Series/Season → representative episode → series_key → metadata row.
    let item = crate::api::jellyfin::images::resolve_synth_image_item(state, id_str).await?;
    let key = item.series.as_ref()?.series_key().to_string();
    let map = state
        .stores
        .series_metadata_by_keys(std::slice::from_ref(&key))
        .await
        .ok()?;
    let meta = map.get(&key)?;
    let provider = match meta.match_provider.as_deref()? {
        "tmdb" => "tmdb",
        "tvdb" => "tvdb",
        _ => return None,
    };
    Some(ImageMatch {
        provider,
        external_id: meta.match_external_id.clone()?,
        // Series-level, exactly as the enrichment path resolves a show.
        kind: MediaKind::Episode,
    })
}

/// Core of `GET /Items/{id}/RemoteImages`. Empty (200) on no key / no match /
/// provider blip. Filters to the requested `Type` when given.
async fn remote_images_inner(
    state: &AppState,
    id_str: &str,
    q: &RemoteImagesQuery,
) -> RemoteImagesResultDto {
    let Some(m) = resolve_image_match(state, id_str).await else {
        return RemoteImagesResultDto {
            images: vec![],
            total_record_count: 0,
            providers: vec![],
        };
    };
    // Honour an explicit provider filter: if the client asked for a provider we
    // didn't match this item to, there's nothing to offer.
    if let Some(req) = q.provider_name.as_deref().and_then(provider_token) {
        if req != m.provider {
            return RemoteImagesResultDto {
                images: vec![],
                total_record_count: 0,
                providers: vec![],
            };
        }
    }
    // Build the matched provider's enricher iff its key is configured.
    let images: Vec<crate::online_enrich::RemoteImage> = match m.provider {
        "tmdb" => match non_empty(state.tmdb_api_key.as_deref()) {
            Some(key) => {
                TmdbEnricher(TmdbClient::new(key.to_string()))
                    .list_images(m.kind, &m.external_id)
                    .await
            }
            None => vec![],
        },
        _ => match non_empty(state.tvdb_api_key.as_deref()) {
            Some(key) => {
                TvdbEnricher(TvdbClient::new(key.to_string()))
                    .list_images(m.kind, &m.external_id)
                    .await
            }
            None => vec![],
        },
    };
    let want = q.image_type.as_deref();
    let dtos: Vec<RemoteImageInfoDto> = images
        .into_iter()
        .filter(|img| match want {
            Some(t) => img.role.as_str().eq_ignore_ascii_case(t),
            None => true,
        })
        .map(|img| RemoteImageInfoDto {
            provider_name: provider_display(m.provider).to_string(),
            url: img.url,
            image_type: img.role.as_str().to_string(),
            height: img.height,
            width: img.width,
            language: img.language,
            community_rating: img.community_rating,
            vote_count: img.vote_count,
            rating_type: "Score",
        })
        .collect();
    let providers = if dtos.is_empty() {
        vec![]
    } else {
        vec![provider_display(m.provider).to_string()]
    };
    RemoteImagesResultDto {
        total_record_count: dtos.len() as u32,
        images: dtos,
        providers,
    }
}

/// `GET /Items/{id}/RemoteImages` — the Edit-Images dialog's candidate list.
async fn remote_images(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<RemoteImagesQuery>,
) -> impl Responder {
    let result = remote_images_inner(&state, &path.into_inner(), &q).await;
    crate::api::jellyfin::wire::json(&result)
}

/// `GET /Items/{id}/RemoteImages/Providers` — the matched provider name(s).
async fn remote_image_providers(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> impl Responder {
    let providers = match resolve_image_match(&state, &path.into_inner()).await {
        Some(m) => vec![provider_display(m.provider).to_string()],
        None => Vec::<String>::new(),
    };
    crate::api::jellyfin::wire::json(&providers)
}

/// Plain public-CDN GET for the chosen image bytes (both TMDB and TVDB serve
/// art from public CDNs — no auth needed for the download itself). `Err` with
/// the reason on any transport/HTTP error.
async fn fetch_url_bytes(url: &str) -> Result<Vec<u8>, String> {
    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("image host returned {}", resp.status()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

/// Core of `POST /Items/{id}/RemoteImages/Download`. Fetches the chosen URL,
/// caches it as the item's art, and freezes the row to `manual` so the
/// background enrichment pass never clobbers the curated pick.
async fn remote_images_download_inner(
    state: &AppState,
    id_str: &str,
    q: RemoteImageDownloadQuery,
) -> Result<(), actix_web::Error> {
    let role = artwork_role_from_type(&q.image_type)
        .ok_or_else(|| error::ErrorBadRequest("unsupported image Type"))?;
    let bytes = fetch_url_bytes(&q.image_url)
        .await
        .map_err(|e| error::ErrorBadRequest(format!("could not fetch image: {e}")))?;
    let now = crate::metadata_backfill::now_secs();
    let provider = q
        .provider_name
        .as_deref()
        .and_then(provider_token)
        .unwrap_or("tmdb");

    // Real numeric id → item art row + freeze identity.
    if let Some(id) = pharos_jellyfin_api::dto::parse_item_id(id_str) {
        if let Ok(item) = state.stores.get(id).await {
            let cache = state
                .images
                .as_ref()
                .ok_or_else(|| error::ErrorInternalServerError("no image cache configured"))?;
            let art = crate::online_enrich::RemoteArt {
                role,
                url: q.image_url.clone(),
            };
            crate::online_enrich::download_and_cache_art(
                cache,
                &state.stores,
                &item,
                provider,
                &art,
                bytes,
            )
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
            // Freeze: keep the existing provider id if any, else the download's.
            let ext = item
                .metadata
                .provider_ids
                .tmdb
                .clone()
                .or_else(|| item.metadata.provider_ids.tvdb.clone())
                .unwrap_or_default();
            state
                .stores
                .set_item_match(id, provider, &ext, "manual", None, now)
                .await
                .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
            return Ok(());
        }
    }

    // Synth Series → series_metadata locator + freeze.
    let item = crate::api::jellyfin::images::resolve_synth_image_item(state, id_str)
        .await
        .ok_or_else(|| error::ErrorNotFound("not found"))?;
    let key = item
        .series
        .as_ref()
        .ok_or_else(|| error::ErrorNotFound("not found"))?
        .series_key()
        .to_string();
    let cache = state
        .images
        .as_ref()
        .ok_or_else(|| error::ErrorInternalServerError("no image cache configured"))?;
    let image_role = pharos_cache::image_cache::ImageRole::from_str_ci(&q.image_type)
        .ok_or_else(|| error::ErrorBadRequest("unsupported image Type"))?;
    let path = cache
        .upload_series_art(&key, image_role, &bytes)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("series art cache: {e}")))?;

    use pharos_core::SeriesMetadataStore;
    let mut meta = state
        .stores
        .series_metadata_by_keys(std::slice::from_ref(&key))
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?
        .remove(&key)
        .unwrap_or_else(|| pharos_core::SeriesMetadata {
            series_key: key.clone(),
            series_name: key.clone(),
            ..Default::default()
        });
    match role {
        pharos_core::ArtworkRole::Primary => {
            meta.poster_locator = Some(path.to_string_lossy().into_owned())
        }
        pharos_core::ArtworkRole::Backdrop => {
            meta.backdrop_locator = Some(path.to_string_lossy().into_owned())
        }
        // Logo lives at the deterministic `series_image_path`; no locator column.
        _ => {}
    }
    meta.match_source = Some("manual".to_string());
    meta.metadata_refreshed_at = Some(now);
    state
        .stores
        .upsert_series_metadata(meta)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(())
}

/// `POST /Items/{id}/RemoteImages/Download` — download the user's chosen image.
async fn remote_images_download(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<RemoteImageDownloadQuery>,
) -> Result<impl Responder, actix_web::Error> {
    remote_images_download_inner(&state, &path.into_inner(), q.into_inner()).await?;
    Ok(HttpResponse::NoContent().finish())
}

/// `GET /Items/{id}/RemoteSearch/Subtitles/{lang}` — remote subtitle
/// search. No subtitle providers configured → an empty
/// `RemoteSubtitleInfo[]` (200).
async fn remote_subtitle_search(
    _user: AuthUser,
    _path: web::Path<(String, String)>,
) -> impl Responder {
    crate::api::jellyfin::wire::json(&serde_json::Value::Array(vec![]))
}

/// `GET /Audio/{id}/Lyrics` — serve time-synced lyrics from an `.lrc`
/// sidecar beside the audio file when present, else an empty lyric doc
/// (200). Shape is Jellyfin's `LyricDto` (`{ Metadata, Lyrics: [{Text,
/// Start}] }`, `Start` in ticks).
async fn get_lyrics(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let lines = read_lrc_sidecar(&item.path).unwrap_or_default();
    // B78/V38 — typed LyricDto, not a json! literal.
    Ok(crate::api::jellyfin::wire::json(&LyricDto {
        metadata: LyricMetadataDto {},
        lyrics: lines,
    }))
}

/// Jellyfin `LyricDto` (`GET /Audio/{id}/Lyrics`). `Metadata` is an all-nullable
/// `LyricMetadata` — an empty `{}` deserializes cleanly on strict clients.
#[derive(serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct LyricDto {
    metadata: LyricMetadataDto,
    lyrics: Vec<LyricLineDto>,
}

/// Empty `LyricMetadata` — serializes to `{}`.
#[derive(serde::Serialize)]
struct LyricMetadataDto {}

/// One `LyricLine`: `Start` in Jellyfin ticks (100 ns).
#[derive(serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct LyricLineDto {
    text: String,
    start: u64,
}

/// Read + parse an `.lrc` sidecar (same stem as the media file). Returns the
/// synced lines as Jellyfin `LyricLine` objects, or `None` when no sidecar
/// exists / it can't be read. Malformed lines are skipped.
fn read_lrc_sidecar(media_path: &std::path::Path) -> Option<Vec<LyricLineDto>> {
    let lrc = media_path.with_extension("lrc");
    let text = std::fs::read_to_string(&lrc).ok()?;
    let mut out: Vec<LyricLineDto> = Vec::new();
    for line in text.lines() {
        if let Some((ticks, content)) = parse_lrc_line(line) {
            out.push(LyricLineDto {
                text: content,
                start: ticks,
            });
        }
    }
    Some(out)
}

/// Parse one `[mm:ss.xx]text` LRC line into `(start_ticks, text)`. Returns
/// `None` for metadata / blank / malformed lines. Ticks are 100 ns units
/// (Jellyfin's `Start`).
fn parse_lrc_line(line: &str) -> Option<(u64, String)> {
    let line = line.trim();
    let close = line.find(']')?;
    if !line.starts_with('[') {
        return None;
    }
    let stamp = &line[1..close];
    let text = line[close + 1..].trim().to_string();
    let (min_s, rest) = stamp.split_once(':')?;
    let minutes: u64 = min_s.parse().ok()?;
    // Seconds may carry a fractional part (.xx or .xxx).
    let seconds: f64 = rest.parse().ok()?;
    let total_ms = (minutes as f64) * 60_000.0 + seconds * 1000.0;
    let ticks = (total_ms * 10_000.0) as u64;
    Some((ticks, text))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case", default)]
struct InstantMixQuery {
    limit: Option<u32>,
}

/// `GET /Items/{id}/InstantMix` — build a mix seeded from an item: the seed
/// first, then other library items of the same kind. A genuine (if simple)
/// mix — pharos has no acoustic-similarity model, so kind is the similarity
/// axis. Returns an `ItemsResult` page.
async fn instant_mix(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<InstantMixQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::MediaQuery;
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let seed = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    let mq = MediaQuery {
        kinds: vec![seed.kind],
        limit: Some(limit + 1), // +1 so dropping the seed still fills the page
        ..Default::default()
    };
    let (rows, _total) = state
        .stores
        .query(&mq)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // Seed first, then the rest of the same-kind set (seed de-duplicated).
    let mut mix: Vec<pharos_core::MediaItem> = vec![seed.clone()];
    for it in rows.into_iter().filter(|i| i.id != seed.id) {
        if mix.len() as u32 >= limit {
            break;
        }
        mix.push(it);
    }
    let total = mix.len() as u32;
    let page = items::build_items_page(&state, user.0.id, &mix, total, 0).await?;
    Ok(crate::api::jellyfin::wire::json(&page))
}

// ---------------------------------------------------------------------
// T11 — manual Identify: search a provider for candidates, apply a chosen
// one as an override. Unlike `remote_images`/`remote_subtitle_search`
// above (pharos has NO providers for those), pharos DOES have TMDB/TVDB
// wired for the T9 background enrichment sweep — these endpoints expose
// that same capability to jellyfin-web's Identify dialog for a manual
// override, on demand (a fresh enricher per call; see `AppState::tmdb_api_key`
// / `tvdb_api_key`).
// ---------------------------------------------------------------------

/// One candidate in a `RemoteSearch` result — trimmed to the fields
/// jellyfin-web's Identify dialog renders + needs to round-trip into
/// `POST .../RemoteSearch/Apply`.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteSearchResultDto {
    name: String,
    production_year: Option<u32>,
    provider_ids: RemoteSearchProviderIdsDto,
    /// Always absent today — [`pharos_core::SearchCandidate`] carries no
    /// thumbnail; omitted (not `null`) so a strict client's optional-field
    /// handling sees "not offered" rather than a broken image request.
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<String>,
}

#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteSearchProviderIdsDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    tmdb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tvdb: Option<String>,
}

/// `POST /Items/{id}/RemoteSearch/Apply` body — Jellyfin's Identify dialog
/// posts the chosen candidate's provider + id.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteSearchApplyDto {
    provider: String,
    id: String,
}

/// `key.filter(not empty)` — a blank string counts as unset, mirroring
/// `Config::apply_env`'s secret-injection rule.
fn non_empty(key: Option<&str>) -> Option<&str> {
    key.filter(|k| !k.is_empty())
}

/// Derive `(title, year)` to search a provider with, from an item's stem
/// (movie) or series metadata (series/episode) — mirrors
/// [`crate::metadata_backfill::enrich_one`]'s search-key derivation so a
/// manual search surfaces the same candidates the background pass would.
fn search_key(item: &MediaItem, movie: bool) -> (String, Option<u32>) {
    let stem = item
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(item.title.as_str());
    if movie {
        let parsed = FilenameProvider::parse_stem(stem, true);
        (
            parsed.title.unwrap_or_else(|| item.title.clone()),
            parsed.year,
        )
    } else {
        let series = item.series.as_ref();
        let title = series
            .map(|s| s.series_name.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                FilenameProvider::parse_stem(stem, false)
                    .title
                    .unwrap_or_else(|| item.title.clone())
            });
        (title, series.and_then(|s| s.series_year))
    }
}

/// [`pharos_core::SearchCandidate`]s → wire DTOs, tagging the id under the
/// right `ProviderIds` key.
fn candidates_to_dto(
    candidates: Vec<pharos_core::SearchCandidate>,
    provider: &str,
) -> Vec<RemoteSearchResultDto> {
    candidates
        .into_iter()
        .map(|c| {
            let mut provider_ids = RemoteSearchProviderIdsDto::default();
            match provider {
                "tmdb" => provider_ids.tmdb = Some(c.id),
                _ => provider_ids.tvdb = Some(c.id),
            }
            RemoteSearchResultDto {
                name: c.title,
                production_year: c.year,
                provider_ids,
                image_url: None,
            }
        })
        .collect()
}

/// Core of `GET /Items/{id}/RemoteSearch/Movie` — search TMDB for candidate
/// matches. No `[tmdb].api_key` configured → an honest empty array, the
/// same treatment `remote_images`/`remote_subtitle_search` above give their
/// (also absent) providers, rather than erroring the client's fetch.
/// Split from the actix handler below so it's directly callable from tests
/// without going through `web::Path` extraction.
async fn remote_search_movie_inner(
    state: &AppState,
    id: u64,
) -> Result<Vec<RemoteSearchResultDto>, actix_web::Error> {
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let Some(key) = non_empty(state.tmdb_api_key.as_deref()) else {
        return Ok(Vec::new());
    };
    let enricher = TmdbEnricher(TmdbClient::new(key.to_string()));
    let (title, year) = search_key(&item, true);
    let candidates = enricher.search(MediaKind::Movie, &title, year).await;
    Ok(candidates_to_dto(candidates, "tmdb"))
}

/// `GET /Items/{id}/RemoteSearch/Movie` — jellyfin-web's Identify dialog
/// lists these candidates.
async fn remote_search_movie(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let results = remote_search_movie_inner(&state, id).await?;
    Ok(crate::api::jellyfin::wire::json(&results))
}

/// Core of `GET /Items/{id}/RemoteSearch/Series` — search TVDB (falling
/// back to TMDB when no `[tvdb].api_key` is configured but
/// `[tmdb].api_key` is), mirroring
/// [`crate::metadata_backfill::enrich_one`]'s episode provider preference.
/// Neither key configured → an empty array.
async fn remote_search_series_inner(
    state: &AppState,
    id: u64,
) -> Result<Vec<RemoteSearchResultDto>, actix_web::Error> {
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let (title, year) = search_key(&item, false);
    if let Some(key) = non_empty(state.tvdb_api_key.as_deref()) {
        let enricher = TvdbEnricher(TvdbClient::new(key.to_string()));
        let candidates = enricher.search(MediaKind::Episode, &title, year).await;
        return Ok(candidates_to_dto(candidates, "tvdb"));
    }
    if let Some(key) = non_empty(state.tmdb_api_key.as_deref()) {
        let enricher = TmdbEnricher(TmdbClient::new(key.to_string()));
        let candidates = enricher.search(MediaKind::Episode, &title, year).await;
        return Ok(candidates_to_dto(candidates, "tmdb"));
    }
    Ok(Vec::new())
}

/// `GET /Items/{id}/RemoteSearch/Series` — jellyfin-web's Identify dialog
/// lists these candidates.
async fn remote_search_series(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let results = remote_search_series_inner(&state, id).await?;
    Ok(crate::api::jellyfin::wire::json(&results))
}

/// Core of `POST /Items/{id}/RemoteSearch/Apply` — sets `match_source =
/// "manual"` (a user override the T9 background sweep NEVER reprocesses —
/// see [`pharos_core::MediaStore::items_needing_match`]), then attempts an
/// immediate re-enrich of just this item so its metadata/art reflect the
/// new match right away instead of waiting for the next scheduled pass.
/// The override is persisted even when no provider key is configured (a
/// user's stated identity is honoured regardless of whether pharos can
/// currently fetch it) — only the immediate re-enrich step is then skipped.
/// Split from the actix handler below so it's directly callable from tests
/// without going through `web::Path`/`web::Json` extraction.
async fn remote_search_apply_inner(
    state: &AppState,
    id: u64,
    body: RemoteSearchApplyDto,
) -> Result<(), actix_web::Error> {
    // Confirm the item exists first — `set_item_match` is a silent no-op on
    // an unknown id, which would otherwise mask a bad id as a false 204.
    state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let provider = body.provider.to_ascii_lowercase();
    if provider != "tmdb" && provider != "tvdb" {
        return Err(error::ErrorBadRequest(
            "Provider must be \"tmdb\" or \"tvdb\"",
        ));
    }
    let now = crate::metadata_backfill::now_secs();
    let tmdb = non_empty(state.tmdb_api_key.as_deref())
        .map(|key| TmdbEnricher(TmdbClient::new(key.to_string())));
    let tvdb: Option<TvdbEnricher<ReqwestTransport>> = non_empty(state.tvdb_api_key.as_deref())
        .map(|key| TvdbEnricher(TvdbClient::new(key.to_string())));
    crate::metadata_backfill::apply_manual_match(
        &state.stores,
        &state.bg_io,
        state.images.as_ref(),
        tmdb.as_ref(),
        tvdb.as_ref(),
        &state.metadata_config,
        id,
        &provider,
        &body.id,
        now,
    )
    .await
    .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(())
}

/// `POST /Items/{id}/RemoteSearch/Apply` — jellyfin-web's Identify dialog
/// posts the user's chosen candidate here.
async fn remote_search_apply(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    body: web::Json<RemoteSearchApplyDto>,
) -> Result<impl Responder, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    remote_search_apply_inner(&state, id, body.into_inner()).await?;
    Ok(HttpResponse::NoContent().finish())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::state::Stores;

    async fn seed_state() -> AppState {
        let stores = Stores::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        AppState::new(stores, "t".into())
    }

    async fn put_movie(state: &AppState, id: u64, title: &str) {
        let item = pharos_core::MediaItem {
            id,
            path: format!("/movies/{title}.mkv").into(),
            title: title.to_string(),
            kind: MediaKind::Movie,
            ..pharos_core::MediaItem::default()
        };
        state.stores.put(item).await.expect("seed item");
    }

    /// Handler-core test: drives `remote_search_apply_inner` directly — the
    /// thin actix wrapper above only parses `web::Path`/`web::Json` and
    /// delegates here, so this exercises the real handler logic (provider
    /// validation, `apply_manual_match` wiring) against a real in-memory
    /// `SqliteStore`, without needing a live login session to satisfy the
    /// `AuthUser` extractor.
    #[actix_web::test]
    async fn apply_flips_match_source_to_manual_and_persists() {
        let state = seed_state().await;
        put_movie(&state, 42, "Dune (2021)").await;

        remote_search_apply_inner(
            &state,
            42,
            RemoteSearchApplyDto {
                provider: "tmdb".to_string(),
                id: "438631".to_string(),
            },
        )
        .await
        .expect("apply handler");

        let got = state.stores.get(42).await.expect("item still present");
        assert_eq!(got.match_source.as_deref(), Some("manual"));
        assert_eq!(got.match_provider.as_deref(), Some("tmdb"));
        assert_eq!(got.match_external_id.as_deref(), Some("438631"));
    }

    async fn seed_state_with_cache(root: &std::path::Path) -> AppState {
        let stores = Stores::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        AppState::new(stores, "t".into()).with_image_cache(pharos_cache::ImageCache::new(root))
    }

    // ---- RemoteImages picker (T-remote-images) ----

    #[actix_web::test]
    async fn remote_images_empty_without_key_or_match() {
        let state = seed_state().await;
        put_movie(&state, 42, "Dune (2021)").await; // no provider id, no key
        let r = remote_images_inner(&state, "42", &RemoteImagesQuery::default()).await;
        assert_eq!(r.total_record_count, 0);
        assert!(r.images.is_empty());
        assert!(r.providers.is_empty());
    }

    #[actix_web::test]
    async fn resolve_match_reads_item_provider_id() {
        let state = seed_state().await;
        let item = pharos_core::MediaItem {
            id: 50,
            path: "/movies/x.mkv".into(),
            title: "X".into(),
            kind: MediaKind::Movie,
            metadata: pharos_core::MediaMetadata {
                provider_ids: pharos_core::ProviderIds {
                    tmdb: Some("603".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        state.stores.put(item).await.unwrap();
        let m = resolve_image_match(&state, "50").await.unwrap();
        assert_eq!(m.provider, "tmdb");
        assert_eq!(m.external_id, "603");
    }

    #[actix_web::test]
    async fn download_bad_type_is_400() {
        let state = seed_state().await;
        put_movie(&state, 44, "X").await;
        let err = remote_images_download_inner(
            &state,
            "44",
            RemoteImageDownloadQuery {
                image_type: "Nonsense".into(),
                image_url: "https://example/x.jpg".into(),
                provider_name: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.as_response_error().status_code(),
            actix_web::http::StatusCode::BAD_REQUEST
        );
    }

    #[actix_web::test]
    async fn download_unreachable_url_is_400() {
        let state = seed_state().await;
        put_movie(&state, 45, "X").await;
        let err = remote_images_download_inner(
            &state,
            "45",
            RemoteImageDownloadQuery {
                image_type: "Primary".into(),
                image_url: "http://127.0.0.1:1/nope.jpg".into(),
                provider_name: Some("TheMovieDb".into()),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.as_response_error().status_code(),
            actix_web::http::StatusCode::BAD_REQUEST
        );
    }

    #[actix_web::test]
    async fn download_writes_item_art_and_freezes_to_manual() {
        use actix_web::App;
        // A tiny local server standing in for the provider image CDN.
        let img_srv = actix_test::start(|| {
            App::new().route(
                "/poster.jpg",
                web::get().to(|| async { HttpResponse::Ok().body(&b"POSTERBYTES"[..]) }),
            )
        });
        let url = img_srv.url("/poster.jpg");

        let cache_dir = tempfile::tempdir().unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        put_movie(&state, 60, "X").await;

        remote_images_download_inner(
            &state,
            "60",
            RemoteImageDownloadQuery {
                image_type: "Primary".into(),
                image_url: url,
                provider_name: Some("TheMovieDb".into()),
            },
        )
        .await
        .expect("download should succeed");

        // Freeze: identity pinned to manual so the background pass skips it.
        let got = state.stores.get(60).await.unwrap();
        assert_eq!(got.match_source.as_deref(), Some("manual"));
        // A Primary artwork row now exists, sourced from tmdb, pointing at a
        // real cached file holding the fetched bytes.
        let rows = state.stores.artwork_for(60).await.unwrap();
        let (_, source, locator) = rows
            .iter()
            .find(|(role, _, _)| role.eq_ignore_ascii_case("Primary"))
            .expect("a Primary artwork row");
        assert_eq!(source, "tmdb");
        assert_eq!(std::fs::read(locator).unwrap(), b"POSTERBYTES");
    }

    /// No `[tmdb].api_key` configured (the default in `seed_state`) → the
    /// search route returns an honest empty array, never an error, matching
    /// the file's existing no-providers stubs (`remote_images` etc).
    #[actix_web::test]
    async fn search_movie_returns_empty_without_a_key() {
        let state = seed_state().await;
        put_movie(&state, 43, "Dune (2021)").await;

        let results = remote_search_movie_inner(&state, 43)
            .await
            .expect("search handler");
        assert!(results.is_empty());
    }

    /// Unknown item id → 404, not a silent 204 — `set_item_match` is a
    /// no-op on an unknown id, which would otherwise mask a bad id as
    /// success.
    #[actix_web::test]
    async fn apply_unknown_item_returns_404() {
        let state = seed_state().await;
        let err = remote_search_apply_inner(
            &state,
            999,
            RemoteSearchApplyDto {
                provider: "tmdb".to_string(),
                id: "1".to_string(),
            },
        )
        .await
        .expect_err("unknown id must 404");
        assert_eq!(
            err.error_response().status(),
            actix_web::http::StatusCode::NOT_FOUND
        );
    }

    /// An unrecognised `Provider` value is rejected with 400, not silently
    /// persisted as a bogus match.
    #[actix_web::test]
    async fn apply_rejects_unknown_provider() {
        let state = seed_state().await;
        put_movie(&state, 44, "Whatever").await;
        let err = remote_search_apply_inner(
            &state,
            44,
            RemoteSearchApplyDto {
                provider: "letterboxd".to_string(),
                id: "1".to_string(),
            },
        )
        .await
        .expect_err("unknown provider must 400");
        assert_eq!(
            err.error_response().status(),
            actix_web::http::StatusCode::BAD_REQUEST
        );
    }
}
