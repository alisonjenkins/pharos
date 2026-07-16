//! `/Search/Hints` + `/Search/Suggestions`. T32.
//!
//! Jellyfin's search controller is the single endpoint the global
//! search box drives. jellyfin-web (T29 crawl) loads the search page,
//! types a term, and POSTs `searchTerm` to `/Search/Hints` expecting a
//! `SearchHintsResult` shape — `SearchHints[]` + `TotalRecordCount`.
//!
//! Scope: substring match against `MediaItem.title`, case-insensitive,
//! against everything in the store, plus aggregate name hints for
//! artists / albums / genres (LIB-C4) / people (LIB-C2) / studios
//! (LIB-C3) — each a synthetic IsFolder hint sharing the SearchHint
//! shape so the handler grows additively.

use crate::api::jellyfin::ci_query::CiQuery;
use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, web, Responder};
use pharos_core::{MediaItem, MediaKind, MediaStore};
use serde::{Deserialize, Serialize};

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase-only — `LowercasePath` middleware handles PascalCase.
    cfg.route("/search/hints", web::get().to(search_hints))
        .route("/search/suggestions", web::get().to(search_suggestions))
        .route(
            "/users/{user_id}/suggestions",
            web::get().to(user_suggestions),
        );
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SearchHint {
    item_id: String,
    id: String,
    name: String,
    /// Jellyfin's `Type` discriminator on the wire (Movie/Episode/Audio/...).
    #[serde(rename = "Type")]
    kind: &'static str,
    media_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_time_ticks: Option<u64>,
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
    q: CiQuery<SearchQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use crate::api::jellyfin::dto::{album_id_for, artist_id_for, genre_id_for};
    use std::collections::HashSet;

    // Unicode-aware lowercase so accented titles match queries typed
    // in different case (Pokémon / POKÉMON). ASCII-only
    // to_ascii_lowercase silently dropped these matches.
    let needle = q.search_term.as_deref().unwrap_or("").trim().to_lowercase();
    let kinds = parse_include_item_types(q.include_item_types.as_deref());
    let include_aggregates = q
        .include_item_types
        .as_deref()
        .map(|s| {
            s.split(',').any(|t| {
                let t = t.trim();
                t.eq_ignore_ascii_case("MusicArtist")
                    || t.eq_ignore_ascii_case("MusicAlbum")
                    || t.eq_ignore_ascii_case("Genre")
                    || t.eq_ignore_ascii_case("Person")
                    || t.eq_ignore_ascii_case("Studio")
                    || t.eq_ignore_ascii_case("Tag")
            })
        })
        .unwrap_or(true);

    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;

    let mut hints: Vec<SearchHint> = Vec::new();
    // 1. Title (+ overview) matches on real items.
    //
    // With a term: LIB-B4 full-text search — the backend's FTS (sqlite
    // fts5 / postgres tsvector) returns ranked, prefix-friendly hits over
    // title + overview that are a strict SUPERSET of the legacy substring
    // scan (mid-word substrings included), so results never shrink versus
    // the old in-memory path.
    //
    // Without a term (the "browse everything" case jellyfin-web uses to
    // pre-populate the search page): FTS matches nothing by contract, so
    // fall back to listing every item, kind-filtered — preserving the
    // legacy empty-term-returns-the-corpus behaviour.
    let term_items: Vec<MediaItem> = if needle.is_empty() {
        all.iter()
            .filter(|i| kinds.as_ref().map_or(true, |k| k.contains(&i.kind)))
            .cloned()
            .collect()
    } else {
        use pharos_core::SearchQuery;
        let item_kinds = kinds.clone().unwrap_or_default();
        let term = q.search_term.clone().unwrap_or_default();
        // Pull a generous window so aggregate hints + the result page can
        // both draw from it; bounded so a one-letter query can't load the
        // whole library. The handler re-paginates the combined hint list.
        let fetch = q
            .start_index
            .saturating_add(q.limit)
            .saturating_add(200)
            .max(200);
        let (items, _total) = state
            .stores
            .search(&SearchQuery {
                term: term.trim().to_string(),
                kinds: item_kinds,
                limit: fetch,
                offset: 0,
            })
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        items
    };
    for i in &term_items {
        hints.push(SearchHint {
            item_id: pharos_jellyfin_api::dto::wire_item_id(i.id),
            id: pharos_jellyfin_api::dto::wire_item_id(i.id),
            name: i.title.clone(),
            kind: jellyfin_type(i.kind),
            media_type: media_type(i.kind),
            run_time_ticks: i.probe.run_time_ticks(),
            matched_term: q.search_term.clone().unwrap_or_default(),
            is_folder: false,
        });
    }

    // 2. Aggregate name matches (Artist / Album / Genre) — emitted
    // as synthetic IsFolder hints jellyfin-web routes to /Items?
    // ParentId={id}. Skipped when IncludeItemTypes explicitly omits
    // all three of MusicArtist / MusicAlbum / Genre.
    if include_aggregates && !needle.is_empty() {
        let mut seen_artist: HashSet<String> = HashSet::new();
        let mut seen_album: HashSet<String> = HashSet::new();
        let mut seen_genre: HashSet<String> = HashSet::new();
        for i in &all {
            for n in [i.probe.artist.as_deref(), i.probe.album_artist.as_deref()]
                .into_iter()
                .flatten()
            {
                {
                    if n.to_lowercase().contains(&needle) && seen_artist.insert(n.into()) {
                        hints.push(SearchHint {
                            item_id: artist_id_for(n),
                            id: artist_id_for(n),
                            name: n.to_string(),
                            kind: "MusicArtist",
                            media_type: "Unknown",
                            run_time_ticks: None,
                            matched_term: q.search_term.clone().unwrap_or_default(),
                            is_folder: true,
                        });
                    }
                }
            }
            if let Some(n) = i.probe.album.as_deref() {
                if n.to_lowercase().contains(&needle) && seen_album.insert(n.into()) {
                    hints.push(SearchHint {
                        item_id: album_id_for(n),
                        id: album_id_for(n),
                        name: n.to_string(),
                        kind: "MusicAlbum",
                        media_type: "Unknown",
                        run_time_ticks: None,
                        matched_term: q.search_term.clone().unwrap_or_default(),
                        is_folder: true,
                    });
                }
            }
            if let Some(n) = i.probe.genre.as_deref() {
                if n.to_lowercase().contains(&needle) && seen_genre.insert(n.into()) {
                    hints.push(SearchHint {
                        item_id: genre_id_for(n),
                        id: genre_id_for(n),
                        name: n.to_string(),
                        kind: "Genre",
                        media_type: "Unknown",
                        run_time_ticks: None,
                        matched_term: q.search_term.clone().unwrap_or_default(),
                        is_folder: true,
                    });
                }
            }
        }
        // LIB-C2 — Person aggregate hints, entity-backed (people +
        // item_people) rather than probe-derived. Each match is a
        // synthetic IsFolder hint jellyfin-web routes to
        // /Items?ParentId=<person wire id>.
        {
            use pharos_core::PersonStore;
            if let Ok(people) = state.stores.people_with_counts().await {
                for pc in &people {
                    if pc.person.name.to_lowercase().contains(&needle) {
                        hints.push(SearchHint {
                            item_id: pc.person.wire_id.clone(),
                            id: pc.person.wire_id.clone(),
                            name: pc.person.name.clone(),
                            kind: "Person",
                            media_type: "Unknown",
                            run_time_ticks: None,
                            matched_term: q.search_term.clone().unwrap_or_default(),
                            is_folder: true,
                        });
                    }
                }
            }
        }
        // LIB-C3 — Studio aggregate hints, entity-backed (studios +
        // item_studios) rather than the old album_artist stand-in. Each
        // match is a synthetic IsFolder hint jellyfin-web routes to
        // /Items?ParentId=<studio wire id>.
        {
            use pharos_core::StudioStore;
            if let Ok(studios) = state.stores.studios_with_counts().await {
                for sc in &studios {
                    if sc.studio.name.to_lowercase().contains(&needle) {
                        hints.push(SearchHint {
                            item_id: sc.studio.wire_id.clone(),
                            id: sc.studio.wire_id.clone(),
                            name: sc.studio.name.clone(),
                            kind: "Studio",
                            media_type: "Unknown",
                            run_time_ticks: None,
                            matched_term: q.search_term.clone().unwrap_or_default(),
                            is_folder: true,
                        });
                    }
                }
            }
        }
        // LIB-C5 — Collection / BoxSet aggregate hints, entity-backed
        // (collections + collection_items). Each match is a synthetic
        // IsFolder hint jellyfin-web routes to
        // /Items?ParentId=<collection wire id> (the box set's members).
        {
            use pharos_core::CollectionStore;
            if let Ok(collections) = state.stores.collections_with_counts().await {
                for cc in &collections {
                    if cc.collection.name.to_lowercase().contains(&needle) {
                        hints.push(SearchHint {
                            item_id: cc.collection.wire_id.clone(),
                            id: cc.collection.wire_id.clone(),
                            name: cc.collection.name.clone(),
                            kind: "BoxSet",
                            media_type: "Unknown",
                            run_time_ticks: None,
                            matched_term: q.search_term.clone().unwrap_or_default(),
                            is_folder: true,
                        });
                    }
                }
            }
        }
        // LIB-C6 — Tag aggregate hints, entity-backed (tags + item_tags).
        // Each match is a synthetic IsFolder hint jellyfin-web routes to
        // /Items?ParentId=<tag wire id> (the tag's tagged items).
        {
            use pharos_core::TagStore;
            if let Ok(tags) = state.stores.tags_with_counts().await {
                for tc in &tags {
                    if tc.tag.name.to_lowercase().contains(&needle) {
                        hints.push(SearchHint {
                            item_id: tc.tag.wire_id.clone(),
                            id: tc.tag.wire_id.clone(),
                            name: tc.tag.name.clone(),
                            kind: "Tag",
                            media_type: "Unknown",
                            run_time_ticks: None,
                            matched_term: q.search_term.clone().unwrap_or_default(),
                            is_folder: true,
                        });
                    }
                }
            }
        }
    }

    let total = hints.len() as u32;
    let start = q.start_index as usize;
    let end = (start + q.limit as usize).min(hints.len());
    let page: Vec<SearchHint> = if start >= hints.len() {
        vec![]
    } else {
        hints[start..end].to_vec()
    };

    Ok(crate::api::jellyfin::wire::json(&SearchHintsResult {
        search_hints: page,
        total_record_count: total,
    }))
}

async fn search_suggestions(
    state: web::Data<crate::state::AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    Ok(crate::api::jellyfin::wire::json(
        &build_suggestions(&state, user.0.id, 12).await?,
    ))
}

/// `/Users/{user_id}/Suggestions` — jellyfin-web fetches this on the
/// search page to show "What other people are watching" -style tiles.
/// Bearer-matches-path check applies (V9).
async fn user_suggestions(
    state: web::Data<crate::state::AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let bearer = user.0.id.0.simple().to_string();
    if path.into_inner() != bearer {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    Ok(crate::api::jellyfin::wire::json(
        &build_suggestions(&state, user.0.id, 12).await?,
    ))
}

/// Build a random-sample suggestion result balanced across kinds.
/// Picks up to `limit/kinds` items per kind, shuffles, returns the
/// flattened envelope jellyfin-web expects.
async fn build_suggestions(
    state: &crate::state::AppState,
    user_id: pharos_core::UserId,
    limit: usize,
) -> Result<serde_json::Value, actix_web::Error> {
    use crate::api::jellyfin::dto::BaseItemDto;
    use pharos_core::{MediaStore, UserDataStore};
    let all = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let ids: Vec<u64> = all.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user_id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // Bucket by kind. Drop already-played items so the suggestions
    // surface things the user hasn't watched yet.
    let mut by_kind: std::collections::HashMap<MediaKind, Vec<(usize, &pharos_core::MediaItem)>> =
        std::collections::HashMap::new();
    for (idx, item) in all.iter().enumerate() {
        let ud = user_data.get(idx).copied().unwrap_or_default();
        if ud.played {
            continue;
        }
        by_kind.entry(item.kind).or_default().push((idx, item));
    }
    let per_kind = (limit / 3).max(1); // 3 kinds: Movie / Episode / Audio
    let mut picks: Vec<(usize, &pharos_core::MediaItem)> = Vec::new();
    let mut seed = pseudo_seed();
    for items in by_kind.values_mut() {
        // Cheap xorshift shuffle in place — deterministic enough for
        // suggestions, no rand dep needed.
        xorshift_shuffle(items, &mut seed);
        for entry in items.iter().take(per_kind) {
            picks.push(*entry);
        }
    }
    // One more pass to mix kinds in the final list.
    xorshift_shuffle(&mut picks, &mut seed);
    picks.truncate(limit);
    let dtos: Vec<BaseItemDto> = picks
        .iter()
        .map(|(idx, item)| {
            let ud = user_data.get(*idx).copied().unwrap_or_default();
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud).with_trickplay(
                &item.probe,
                &state.trickplay_widths,
                state.trickplay_interval_ms,
            )
        })
        .collect();
    let total = dtos.len() as u32;
    Ok(serde_json::json!({
        "Items": dtos,
        "TotalRecordCount": total,
        "StartIndex": 0,
    }))
}

fn pseudo_seed() -> u64 {
    let mut buf = [0u8; 8];
    if getrandom::getrandom(&mut buf).is_err() {
        buf = [1, 2, 3, 4, 5, 6, 7, 8];
    }
    u64::from_le_bytes(buf) | 1
}

fn xorshift_shuffle<T>(items: &mut [T], state: &mut u64) {
    for i in (1..items.len()).rev() {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        let j = (*state as usize) % (i + 1);
        items.swap(i, j);
    }
}

fn parse_include_item_types(s: Option<&str>) -> Option<Vec<MediaKind>> {
    let raw = s?;
    if raw.is_empty() {
        return None;
    }
    let kinds: Vec<MediaKind> = raw.split(',').filter_map(MediaKind::from_wire).collect();
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
