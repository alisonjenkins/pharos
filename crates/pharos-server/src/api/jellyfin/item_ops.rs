//! T76 — assorted item operations jellyfin-web exposes on the item detail
//! page and context menus: merge alternate versions, override content type,
//! remote metadata search (images / subtitles), lyrics, and instant mix.
//!
//! pharos has no external metadata-provider subsystem, so the remote-search
//! endpoints return honest empty results (a stock Jellyfin with no providers
//! configured does the same) rather than 404-ing the client's fetch. Lyrics
//! are served from an `.lrc` sidecar next to the audio file when present.
//! InstantMix is a real same-kind mix drawn from the library.

use crate::{
    api::jellyfin::{auth_extractor::AuthUser, items},
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::MediaStore;
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
            "/items/{id}/remotesearch/subtitles/{lang}",
            web::get().to(remote_subtitle_search),
        )
        .route("/audio/{id}/lyrics", web::get().to(get_lyrics))
        .route("/items/{id}/instantmix", web::get().to(instant_mix))
        .route("/items/{id}/metadataeditor", web::get().to(metadata_editor));
}

/// `GET /Items/{id}/MetadataEditor` (T67) — the bundle jellyfin-web's
/// metadata editor loads to build its form: the culture picker, the
/// external-id fields (Imdb/Tmdb/Tvdb), the parental-rating + country
/// options, and the item's current content type. pharos serves the same
/// static option catalogue everywhere; the per-item bits (ContentType) are
/// derived from the item's kind.
async fn metadata_editor(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id: u64 = path
        .into_inner()
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let content_type = match item.kind {
        pharos_core::MediaKind::Movie => "Movies",
        pharos_core::MediaKind::Episode => "tvshows",
        pharos_core::MediaKind::Audio => "music",
    };
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "ContentType": content_type,
        "ContentTypeOptions": [
            { "Name": "Movies", "Value": "movies" },
            { "Name": "Shows",  "Value": "tvshows" },
            { "Name": "Music",  "Value": "music" },
        ],
        "Cultures": crate::api::jellyfin::system::LOCALIZATION_CULTURES,
        "Countries": [
            { "Name": "US", "DisplayName": "United States",
              "TwoLetterISORegionName": "US", "ThreeLetterISORegionName": "USA" }
        ],
        "ParentalRatingOptions": [],
        "ExternalIdInfos": EXTERNAL_ID_INFOS,
    })))
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
#[serde(rename_all = "PascalCase", default)]
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
    q: web::Query<MergeVersionsQuery>,
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
#[serde(rename_all = "camelCase", default)]
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
    q: web::Query<ContentTypeQuery>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let id: u64 = path
        .into_inner()
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
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

/// `GET /Items/{id}/RemoteImages` — remote artwork search. pharos has no
/// image providers, so the result is an empty, well-shaped
/// `RemoteImageResult` (200, not 404) — the same as stock Jellyfin with no
/// providers.
async fn remote_images(_user: AuthUser, _path: web::Path<String>) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "Images": [],
        "TotalRecordCount": 0,
        "Providers": [],
    }))
}

/// `GET /Items/{id}/RemoteImages/Providers` — the configured image
/// providers (none).
async fn remote_image_providers(_user: AuthUser, _path: web::Path<String>) -> impl Responder {
    HttpResponse::Ok().json(serde_json::Value::Array(vec![]))
}

/// `GET /Items/{id}/RemoteSearch/Subtitles/{lang}` — remote subtitle
/// search. No subtitle providers configured → an empty
/// `RemoteSubtitleInfo[]` (200).
async fn remote_subtitle_search(
    _user: AuthUser,
    _path: web::Path<(String, String)>,
) -> impl Responder {
    HttpResponse::Ok().json(serde_json::Value::Array(vec![]))
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
    let id: u64 = path
        .into_inner()
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let lines = read_lrc_sidecar(&item.path).unwrap_or_default();
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Metadata": {},
        "Lyrics": lines,
    })))
}

/// Read + parse an `.lrc` sidecar (same stem as the media file). Returns the
/// synced lines as Jellyfin `LyricLine` objects, or `None` when no sidecar
/// exists / it can't be read. Malformed lines are skipped.
fn read_lrc_sidecar(media_path: &std::path::Path) -> Option<Vec<serde_json::Value>> {
    let lrc = media_path.with_extension("lrc");
    let text = std::fs::read_to_string(&lrc).ok()?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for line in text.lines() {
        if let Some((ticks, content)) = parse_lrc_line(line) {
            out.push(serde_json::json!({ "Text": content, "Start": ticks }));
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
#[serde(rename_all = "PascalCase", default)]
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
    q: web::Query<InstantMixQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::MediaQuery;
    let id: u64 = path
        .into_inner()
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
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
    Ok(HttpResponse::Ok().json(page))
}
