//! `/Search/Hints` + `/Search/Suggestions`. T32.
//!
//! Jellyfin's search controller is the single endpoint the global
//! search box drives. jellyfin-web (T29 crawl) loads the search page,
//! types a term, and POSTs `searchTerm` to `/Search/Hints` expecting a
//! `SearchHintsResult` shape — `SearchHints[]` + `TotalRecordCount`.
//!
//! Scope: substring match against `MediaItem.title`, case-insensitive,
//! against everything in the store. People + studios + genres come in
//! when T33's user-data and T34's metadata-by-name surfaces exist —
//! they share the same SearchHint shape, so the handler grows
//! additively.

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{MediaItem, MediaKind, MediaStore};
use serde::{Deserialize, Serialize};

pub fn register(cfg: &mut web::ServiceConfig) {
    for path in ["/Search/Hints", "/search/hints"] {
        cfg.route(path, web::get().to(search_hints));
    }
    for path in ["/Search/Suggestions", "/search/suggestions"] {
        cfg.route(path, web::get().to(search_suggestions));
    }
    for path in [
        "/Users/{user_id}/Suggestions",
        "/Users/{user_id}/suggestions",
    ] {
        cfg.route(path, web::get().to(user_suggestions));
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchQuery {
    #[serde(default)]
    search_term: Option<String>,
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    start_index: u32,
    /// Comma-separated `Movie,Episode,Audio,...`. Filters which kinds
    /// the hint scan considers.
    #[serde(default)]
    include_item_types: Option<String>,
}

fn default_limit() -> u32 {
    25
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SearchHint {
    item_id: String,
    id: String,
    name: String,
    /// Jellyfin's `Type` discriminator on the wire (Movie/Episode/Audio/...).
    #[serde(rename = "Type")]
    kind: &'static str,
    media_type: &'static str,
    run_time_ticks: u64,
    matched_term: String,
    #[serde(rename = "IsFolder")]
    is_folder: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SearchHintsResult {
    search_hints: Vec<SearchHint>,
    total_record_count: u32,
}

async fn search_hints(
    state: web::Data<AppState>,
    _user: AuthUser,
    q: web::Query<SearchQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let needle = q
        .search_term
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let kinds = parse_include_item_types(q.include_item_types.as_deref());

    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;

    let filtered: Vec<MediaItem> = all
        .into_iter()
        .filter(|i| kinds.as_ref().map_or(true, |k| k.contains(&i.kind)))
        .filter(|i| needle.is_empty() || i.title.to_ascii_lowercase().contains(&needle))
        .collect();

    let total = filtered.len() as u32;
    let start = q.start_index as usize;
    let end = (start + q.limit as usize).min(filtered.len());
    let page = if start >= filtered.len() {
        &[][..]
    } else {
        &filtered[start..end]
    };

    let hints: Vec<SearchHint> = page
        .iter()
        .map(|i| SearchHint {
            item_id: i.id.to_string(),
            id: i.id.to_string(),
            name: i.title.clone(),
            kind: jellyfin_type(i.kind),
            media_type: media_type(i.kind),
            run_time_ticks: 0,
            matched_term: q.search_term.clone().unwrap_or_default(),
            is_folder: false,
        })
        .collect();

    Ok(HttpResponse::Ok().json(SearchHintsResult {
        search_hints: hints,
        total_record_count: total,
    }))
}

async fn search_suggestions(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "Items": [],
        "TotalRecordCount": 0,
        "StartIndex": 0,
    }))
}

/// `/Users/{user_id}/Suggestions` — jellyfin-web fetches this on the
/// search page to show "What other people are watching" -style tiles.
/// Empty list keeps the page rendering without a Response throw.
async fn user_suggestions(
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let bearer = user.0.id.0.simple().to_string();
    if path.into_inner() != bearer {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": [],
        "TotalRecordCount": 0,
        "StartIndex": 0,
    })))
}

fn parse_include_item_types(s: Option<&str>) -> Option<Vec<MediaKind>> {
    let raw = s?;
    if raw.is_empty() {
        return None;
    }
    let kinds: Vec<MediaKind> = raw
        .split(',')
        .filter_map(|t| match t.trim() {
            "Movie" => Some(MediaKind::Movie),
            "Episode" => Some(MediaKind::Episode),
            "Audio" => Some(MediaKind::Audio),
            _ => None,
        })
        .collect();
    if kinds.is_empty() {
        None
    } else {
        Some(kinds)
    }
}

fn jellyfin_type(k: MediaKind) -> &'static str {
    match k {
        MediaKind::Movie => "Movie",
        MediaKind::Episode => "Episode",
        MediaKind::Audio => "Audio",
    }
}

fn media_type(k: MediaKind) -> &'static str {
    match k {
        MediaKind::Audio => "Audio",
        _ => "Video",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_include_item_types() {
        let kinds = parse_include_item_types(Some("Movie,Audio")).unwrap();
        assert!(kinds.contains(&MediaKind::Movie));
        assert!(kinds.contains(&MediaKind::Audio));
        assert!(!kinds.contains(&MediaKind::Episode));
    }

    #[test]
    fn empty_include_returns_none() {
        assert!(parse_include_item_types(None).is_none());
        assert!(parse_include_item_types(Some("")).is_none());
        assert!(parse_include_item_types(Some("Unknown")).is_none());
    }
}
