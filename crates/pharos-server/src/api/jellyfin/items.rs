//! /Items and /Library item-browsing routes.
//!
//! Phase-1 scope: list, get-by-id, per-user list, virtual-folders summary.
//! Phase-2 scope (this file): SearchTerm + IncludeItemTypes filters,
//! SortBy / SortOrder. Filtering is in-memory after `MediaStore::list()`
//! today — moves to SQL-side once library sizes warrant it.

use crate::api::jellyfin::ci_query::CiQuery;
use crate::{
    api::jellyfin::{
        auth_extractor::AuthUser,
        device_profile::{negotiate, Decision, DeviceProfile, SourceMedia},
        dto::{
            build_media_attachments, build_media_streams_with_subtitles, container_for,
            BaseItemDto, CollectionFolderDto, ItemsResultDto, MediaSourceInfoDto, NameGuidPairDto,
            PlaybackInfoResponseDto, SeriesFolderDto, SubtitleStreamCtx, SynthItemDto,
            UserItemDataDto,
        },
        subtitles::discover_sidecars,
    },
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{MediaItem, MediaKind, MediaStore, UserDataStore, UserId};
use pharos_store_sqlx::ServerConfigStore;
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
        // LIB-B5 — /Items/Filters (legacy) + /Items/Filters2 power the
        // filter drawer; registered BEFORE /items/{id} so `Filters` /
        // `Filters2` don't match as an item id.
        .route("/items/filters", web::get().to(items_filters_legacy))
        .route("/items/filters2", web::get().to(items_filters2))
        // B67 — path-less Latest alias (Android/Google-TV), BEFORE /items/{id}
        // so `latest` doesn't parse as an item id (→ 400 "invalid id").
        .route("/items/latest", web::get().to(items_latest))
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
        // B65 — the path-less alias the Android/Google-TV app uses.
        .route("/useritems/resume", web::get().to(user_items_resume))
        .route(
            "/users/{user_id}/items/{item_id}",
            web::get().to(get_user_item),
        )
        .route("/users/{user_id}/views", web::get().to(user_views))
        .route("/userviews", web::get().to(user_views_query))
        .route("/library/virtualfolders", web::get().to(virtual_folders))
        .route(
            "/library/virtualfolders",
            web::post().to(add_virtual_folder),
        )
        .route(
            "/library/virtualfolders",
            web::delete().to(remove_virtual_folder),
        )
        // T69 — library-settings sub-endpoints (order after the base routes;
        // more-specific paths so no collision).
        .route(
            "/library/virtualfolders/libraryoptions",
            web::post().to(update_virtual_folder_options),
        )
        .route(
            "/library/virtualfolders/name",
            web::post().to(rename_virtual_folder),
        )
        .route(
            "/library/virtualfolders/paths",
            web::post().to(add_media_path),
        )
        .route(
            "/library/virtualfolders/paths",
            web::delete().to(remove_media_path),
        )
        .route(
            "/libraries/availableoptions",
            web::get().to(libraries_available_options),
        )
        .route(
            "/environment/directorycontents",
            web::get().to(environment_directory_contents),
        )
        .route("/library/mediafolders", web::get().to(media_folders))
        .route("/items/{id}/refresh", web::post().to(refresh_item))
        .route("/items/{id}/playbackinfo", web::get().to(playback_info))
        .route("/items/{id}/playbackinfo", web::post().to(playback_info));

    // NB: thememedia/themesongs/themevideos are deliberately NOT here — the
    // `stubs` module registers them with the correct jellyfin shapes
    // (`thememedia` → an AllThemeMediaResult with ThemeVideosResult/
    // ThemeSongsResult/SoundtrackSongsResult, each carrying an OwnerId).
    // Registering them here to `empty_items_result` (a bare {Items,…} list)
    // shadowed the stubs handler (actix matches the first-registered route),
    // so jellyfin-web's ThemeMediaPlayer hit `ThemeSongsResult.OwnerId` on
    // undefined and threw — an unhandled TypeError on the detail page that
    // broke the play flow.
    for path in [
        "/items/{id}/specialfeatures",
        "/users/{user_id}/items/{item_id}/intros",
        "/shows/upcoming",
    ] {
        cfg.route(path, web::get().to(empty_items_result));
    }
    cfg.route("/items/{id}/similar", web::get().to(items_similar));
    // /Genres (LIB-C4) + /Studios (LIB-C3) are entity-backed: each lists
    // its entity rows with an item count via the indexed *_with_counts
    // store query (genres/item_genres, studios/item_studios).
    cfg.route("/genres", web::get().to(list_genres))
        .route("/studios", web::get().to(list_studios))
        // /Artists + /Albums power jellyfin-web's music navigation.
        .route("/artists", web::get().to(list_artists))
        .route("/artists/albumartists", web::get().to(list_artists))
        .route("/albums", web::get().to(list_albums))
        // LIB-C2 — /Persons is now entity-backed (people + item_people).
        // /persons/{id} BEFORE the list so a person wire id doesn't fall
        // through to the list handler.
        .route("/persons/{id}", web::get().to(get_person))
        .route("/persons", web::get().to(list_persons))
        // LIB-C6 — /Tags is entity-backed (tags + item_tags). Lists every
        // tag with its count, name-ordered; /Items?ParentId=<tag id> +
        // ?Tags=a,b resolve through the item_tags indexed join.
        .route("/tags", web::get().to(list_tags));
    // LIB-C6 — manual tag mutation on an item. POST adds the `Tags`
    // (incremental, leaving existing tags intact); DELETE removes them.
    // Registered before /items/{id} would shadow them — they share the
    // /items/{id}/tags path which doesn't collide with the bare /items/{id}
    // GET (different method + extra segment), so order is not load-bearing
    // here, but kept explicit alongside the read routes.
    cfg.route("/items/{id}/tags", web::post().to(item_tags_add))
        .route("/items/{id}/tags", web::delete().to(item_tags_remove));
    // LIB-C5 — collections / box sets. The /{id}/items add+remove routes
    // are registered BEFORE the bare create + list so a collection wire id
    // segment doesn't shadow them. POST /collections creates (optionally
    // seeded); GET /collections lists every box set; the per-collection
    // members add/remove drive the manual CRUD Jellyfin clients use.
    cfg.route(
        "/collections/{id}/items",
        web::post().to(collection_items_add),
    )
    .route(
        "/collections/{id}/items",
        web::delete().to(collection_items_remove),
    )
    .route("/collections", web::post().to(create_collection))
    .route("/collections", web::get().to(list_collections));
    // /Shows/NextUp has a real impl now that episode hierarchy
    // exists. Keep it after the empty-stub loop so the route is
    // registered with our handler.
    cfg.route("/shows/nextup", web::get().to(shows_next_up));
    // Series-page hierarchy: jellyfin-web fetches a show's seasons and a
    // season's / series' episodes from these, not /Items?ParentId=.
    cfg.route("/shows/{id}/seasons", web::get().to(shows_seasons));
    cfg.route("/shows/{id}/episodes", web::get().to(shows_episodes));
}

async fn empty_items_result(_user: AuthUser) -> impl Responder {
    crate::api::jellyfin::wire::json(&serde_json::json!({
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
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut movies = 0u32;
    let mut episodes = 0u32;
    let mut audio = 0u32;
    let mut series: HashSet<&str> = HashSet::new();
    let mut artists: HashSet<&str> = HashSet::new();
    let mut albums: HashSet<&str> = HashSet::new();
    let mut genres: HashSet<&str> = HashSet::new();
    for i in all.iter() {
        match i.kind {
            MediaKind::Movie => movies += 1,
            MediaKind::Episode => {
                episodes += 1;
                if let Some(s) = i.series.as_ref() {
                    // LIB-C11 — count distinct shows by folder identity so
                    // same-name shows aren't undercounted.
                    series.insert(s.series_key());
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
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
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

/// LIB-B5 — the `?ParentId=` / `?IncludeItemTypes=` / user-data scope a
/// filter-drawer request carries. A subset of `ListQuery`; reused to build
/// the base [`pharos_core::MediaQuery`] the facet counts aggregate over.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct FiltersQuery {
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    include_item_types: Option<String>,
    #[serde(default)]
    is_favorite: Option<bool>,
    #[serde(default)]
    is_played: Option<bool>,
    #[serde(default)]
    #[allow(dead_code)]
    user_id: Option<String>,
}

/// Build the base [`pharos_core::MediaQuery`] (parent + kind + user-data
/// scope, NO sort/paging) the facet counts aggregate over.
async fn build_facet_base(
    state: &AppState,
    user_id: UserId,
    q: &FiltersQuery,
) -> Result<Option<pharos_core::MediaQuery>, actix_web::Error> {
    use pharos_core::{MediaQuery, UserDataQuery};
    let parent = resolve_parent_filter(state, q.parent_id.as_deref()).await?;
    if matches!(parent, ParentResolution::Empty) {
        return Ok(None);
    }
    let mut mq = MediaQuery::default();
    match &parent {
        ParentResolution::Filter(pf) => mq.parent = Some(pf.clone()),
        ParentResolution::PathPrefix(p) => mq.filters.path_prefix = Some(p.clone()),
        ParentResolution::GenreProbe(t) => mq.filters.genre_probe_token = Some(t.clone()),
        ParentResolution::All | ParentResolution::Empty => {}
    }
    if let Some(types) = q.include_item_types.as_deref() {
        let wanted: Vec<pharos_core::MediaKind> =
            types.split(',').filter_map(MediaKind::from_wire).collect();
        if !wanted.is_empty() {
            mq.kinds = wanted;
        }
    }
    if q.is_favorite.is_some() || q.is_played.is_some() {
        mq.user_data = UserDataQuery {
            user: Some(user_id),
            is_favorite: q.is_favorite,
            is_played: q.is_played,
            is_resumable: false,
        };
    }
    Ok(Some(mq))
}

/// `GET /Items/Filters` (legacy `QueryFiltersLegacy` shape) — flat string /
/// int arrays of the distinct facet values in scope. No counts in the wire
/// shape (the legacy endpoint never carried them), but the values are
/// derived from the SAME `facets()` aggregate as `/Items/Filters2`.
async fn items_filters_legacy(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<FiltersQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::FacetRequest;
    let Some(base) = build_facet_base(&state, user.0.id, &q).await? else {
        return Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
            "Genres": [], "Tags": [], "OfficialRatings": [], "Years": [],
        })));
    };
    let req = FacetRequest::default();
    let facets = state
        .stores
        .facets(&base, &req)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let genres: Vec<&str> = facets.genres.iter().map(|f| f.value.as_str()).collect();
    let tags: Vec<&str> = facets.tags.iter().map(|f| f.value.as_str()).collect();
    let ratings: Vec<&str> = facets
        .official_ratings
        .iter()
        .map(|f| f.value.as_str())
        .collect();
    let years: Vec<i64> = facets
        .years
        .iter()
        .filter_map(|f| f.value.parse::<i64>().ok())
        .collect();
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Genres": genres,
        "Tags": tags,
        "OfficialRatings": ratings,
        "Years": years,
    })))
}

/// `GET /Items/Filters2` (`QueryFiltersDto` shape) — `Genres` + `Studios`
/// as `NameGuidPair[]` (Name + Id wire id), `Tags` as strings, plus the
/// LIB-B5 per-facet COUNTS (the data a filter UI needs to render
/// "Action (42)"). The counts ride in a `FacetCounts` extension object —
/// Jellyfin clients ignore unknown fields, so the response stays
/// Filters2-compatible while exposing the counts pharos computes.
async fn items_filters2(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<FiltersQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::{FacetRequest, FacetValue};
    let empty = serde_json::json!({
        "Genres": [], "Studios": [], "Tags": [], "OfficialRatings": [], "Years": [],
        "FacetCounts": { "Genres": [], "Studios": [], "Tags": [],
                         "Years": [], "OfficialRatings": [] },
    });
    let Some(base) = build_facet_base(&state, user.0.id, &q).await? else {
        return Ok(crate::api::jellyfin::wire::json(&empty));
    };
    let req = FacetRequest::default();
    let facets = state
        .stores
        .facets(&base, &req)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // NameGuidPair[] for the entity facets.
    let name_guid = |fs: &[FacetValue]| -> Vec<serde_json::Value> {
        fs.iter()
            .map(|f| serde_json::json!({ "Name": f.value, "Id": f.wire_id }))
            .collect()
    };
    // (value, count) buckets for the counts extension.
    let counted = |fs: &[FacetValue]| -> Vec<serde_json::Value> {
        fs.iter()
            .map(|f| serde_json::json!({ "Name": f.value, "Id": f.wire_id, "Count": f.count }))
            .collect()
    };
    let tags: Vec<&str> = facets.tags.iter().map(|f| f.value.as_str()).collect();
    let ratings: Vec<&str> = facets
        .official_ratings
        .iter()
        .map(|f| f.value.as_str())
        .collect();
    let years: Vec<i64> = facets
        .years
        .iter()
        .filter_map(|f| f.value.parse::<i64>().ok())
        .collect();
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Genres": name_guid(&facets.genres),
        "Studios": name_guid(&facets.studios),
        "Tags": tags,
        "OfficialRatings": ratings,
        "Years": years,
        "FacetCounts": {
            "Genres": counted(&facets.genres),
            "Studios": counted(&facets.studios),
            "Tags": counted(&facets.tags),
            "Years": counted(&facets.years),
            "OfficialRatings": counted(&facets.official_ratings),
        },
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct SimilarQuery {
    #[serde(default = "default_similar_limit")]
    limit: u32,
    #[serde(default)]
    #[allow(dead_code)]
    user_id: Option<String>,
    /// jellyfin-web's album detail passes the album artist here so "More
    /// Like This" surfaces OTHER artists' albums, not the same discography
    /// the "More From" rail already shows.
    #[serde(default)]
    exclude_artist_ids: Option<String>,
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
    q: CiQuery<SimilarQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let id_str = path.into_inner();
    let id: u64 = match pharos_jellyfin_api::dto::parse_item_id(&id_str).ok_or(()) {
        Ok(v) => v,
        // Synth ids: MusicAlbum / MusicArtist get real music-to-music
        // similarity (they're the ids jellyfin-web's album + artist detail
        // pages put in "More Like This"); other synth families (library /
        // series / season / genre) keep the empty result.
        Err(_) => {
            return music_similar(&state, &id_str, &q).await;
        }
    };
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let Some(target) = all.iter().find(|i| i.id == id) else {
        return Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
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
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud).with_trickplay(
                &item.probe,
                &state.generated_trickplay_widths(item.id),
                state.trickplay_interval_ms,
            )
        })
        .collect();
    fill_parent_ids(&state, &mut dtos, &picks);
    let total = dtos.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": dtos,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// "More Like This" for the synth MusicAlbum / MusicArtist detail pages.
/// Album target → other albums scored by shared album artist + genre-token
/// overlap (minus `ExcludeArtistIds` discographies — the "More From" rail
/// already shows those). Artist target → other artists by genre-token
/// overlap across their tracks. Unknown synth families → empty.
async fn music_similar(
    state: &AppState,
    id_str: &str,
    q: &SimilarQuery,
) -> Result<HttpResponse, actix_web::Error> {
    use crate::api::jellyfin::dto::{album_id_for, artist_id_for};
    let empty = || {
        crate::api::jellyfin::wire::json(&serde_json::json!({
            "Items": [], "TotalRecordCount": 0, "StartIndex": 0,
        }))
    };
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let wanted = id_str.to_ascii_lowercase();

    // Genre tokens of a track ("Rock, Nu Metal" → {rock, nu metal}).
    fn genre_tokens(i: &MediaItem, out: &mut std::collections::HashSet<String>) {
        if let Some(g) = i.probe.genre.as_deref() {
            for t in g.split([',', ';', '/', '|']) {
                let t = t.trim().to_lowercase();
                if !t.is_empty() {
                    out.insert(t);
                }
            }
        }
    }

    // Album target?
    let target_album = all.iter().find(|i| {
        i.kind == pharos_core::MediaKind::Audio
            && i.probe
                .album
                .as_deref()
                .is_some_and(|a| album_id_for(a).eq_ignore_ascii_case(&wanted))
    });
    if let Some(sample) = target_album {
        let album_name = sample.probe.album.clone().unwrap_or_default();
        let target_artist = sample
            .probe
            .album_artist
            .clone()
            .or_else(|| sample.probe.artist.clone());
        let mut target_genres = std::collections::HashSet::new();
        for t in all.iter().filter(|i| {
            i.probe
                .album
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(&album_name))
        }) {
            genre_tokens(t, &mut target_genres);
        }
        let excluded = id_set(q.exclude_artist_ids.as_deref());
        // Keep every track; the exclusions below are ALBUM-level.
        let mut aggs = aggregate_albums(&all, |_| true);
        aggs.retain(|a| !a.name.eq_ignore_ascii_case(&album_name));
        if !excluded.is_empty() {
            aggs.retain(|a| {
                !a.album_artist
                    .as_deref()
                    .is_some_and(|ar| excluded.contains(&artist_id_for(ar).to_ascii_lowercase()))
            });
        }
        // Score: same album artist strongest, then genre-token overlap.
        let mut scored: Vec<(u32, &AlbumAgg)> = aggs
            .iter()
            .map(|a| {
                let mut score = 0u32;
                if let (Some(t), Some(c)) = (target_artist.as_deref(), a.album_artist.as_deref()) {
                    if t.eq_ignore_ascii_case(c) {
                        score += 50;
                    }
                }
                let mut g = std::collections::HashSet::new();
                for t in all.iter().filter(|i| {
                    i.probe
                        .album
                        .as_deref()
                        .is_some_and(|al| al.eq_ignore_ascii_case(&a.name))
                }) {
                    genre_tokens(t, &mut g);
                }
                score += 20 * g.intersection(&target_genres).count() as u32;
                (score, a)
            })
            .filter(|(s, _)| *s > 0)
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
        });
        let items: Vec<serde_json::Value> = scored
            .iter()
            .take(q.limit as usize)
            .map(|(_, a)| synth_album_dto(state, a))
            .collect();
        let total = items.len() as u32;
        return Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
            "Items": items, "TotalRecordCount": total, "StartIndex": 0,
        })));
    }

    // Artist target?
    let mut artist_name: Option<String> = None;
    for i in all.iter() {
        for cand in [i.probe.album_artist.as_deref(), i.probe.artist.as_deref()] {
            if let Some(n) = cand.filter(|n| !n.is_empty()) {
                if artist_id_for(n).eq_ignore_ascii_case(&wanted) {
                    artist_name = Some(n.to_string());
                    break;
                }
            }
        }
        if artist_name.is_some() {
            break;
        }
    }
    let Some(artist) = artist_name else {
        return Ok(empty());
    };
    let of_artist = |i: &MediaItem, name: &str| {
        [i.probe.album_artist.as_deref(), i.probe.artist.as_deref()]
            .into_iter()
            .flatten()
            .any(|a| a.eq_ignore_ascii_case(name))
    };
    let mut target_genres = std::collections::HashSet::new();
    for t in all.iter().filter(|i| of_artist(i, &artist)) {
        genre_tokens(t, &mut target_genres);
    }
    // Candidate artists: every other distinct artist name.
    let mut seen = std::collections::HashSet::new();
    let mut scored: Vec<(u32, String)> = Vec::new();
    for i in all.iter() {
        for cand in [i.probe.album_artist.as_deref(), i.probe.artist.as_deref()] {
            let Some(n) = cand.filter(|n| !n.is_empty()) else {
                continue;
            };
            if n.eq_ignore_ascii_case(&artist) || !seen.insert(n.to_lowercase()) {
                continue;
            }
            let mut g = std::collections::HashSet::new();
            for t in all.iter().filter(|x| of_artist(x, n)) {
                genre_tokens(t, &mut g);
            }
            let score = 20 * g.intersection(&target_genres).count() as u32;
            if score > 0 {
                scored.push((score, n.to_string()));
            }
        }
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    let items: Vec<SynthItemDto> = scored
        .iter()
        .take(q.limit as usize)
        .map(|(_, n)| SynthItemDto {
            image_tags: Some(Default::default()),
            backdrop_image_tags: Some(Vec::new()),
            genres: Some(Vec::new()),
            tags: Some(Vec::new()),
            ..SynthItemDto::folder(
                artist_id_for(n),
                n.clone(),
                state.server_id.clone(),
                "MusicArtist",
            )
        })
        .collect();
    let total = items.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": items, "TotalRecordCount": total, "StartIndex": 0,
    })))
}

/// Score `candidate` for similarity to `target`. Higher = more
/// similar. Zero excludes.
fn similarity_score(target: &MediaItem, candidate: &MediaItem) -> u32 {
    // Hard media-class gate: music is only ever similar to music, video to
    // video. Without it a shared genre string ("Rock" on a concert film,
    // "Anime" on a soundtrack) surfaced TV shows/movies under an audio
    // track's "More Like This".
    let is_audio = |k: pharos_core::MediaKind| matches!(k, pharos_core::MediaKind::Audio);
    if is_audio(target.kind) != is_audio(candidate.kind) {
        return 0;
    }
    let mut s = 0u32;
    // Same Series (Episode) is the strongest signal. LIB-C11 — compare
    // on the folder-keyed identity so two same-name shows in distinct
    // folders aren't treated as one. Falls back to name for legacy rows.
    if let (Some(t), Some(c)) = (target.series.as_ref(), candidate.series.as_ref()) {
        if t.series_key().eq_ignore_ascii_case(c.series_key()) {
            s += 100;
        }
    }
    // Same album_artist (Audio) — same artist's catalogue.
    if let (Some(t), Some(c)) = (
        target.probe.album_artist.as_deref(),
        candidate.probe.album_artist.as_deref(),
    ) {
        if t.eq_ignore_ascii_case(c) {
            s += 50;
        }
    }
    // Same album (Audio) — same album's tracks.
    if let (Some(t), Some(c)) = (
        target.probe.album.as_deref(),
        candidate.probe.album.as_deref(),
    ) {
        if t.eq_ignore_ascii_case(c) {
            s += 40;
        }
    }
    // Same genre — broadly works for every kind.
    if let (Some(t), Some(c)) = (
        target.probe.genre.as_deref(),
        candidate.probe.genre.as_deref(),
    ) {
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
    use pharos_core::GenreStore;
    // LIB-C4 — genres are now entity rows. Lazily backfill the join from
    // the legacy probe.genre strings on first read after upgrade so rows
    // scanned before C4 still surface (idempotent; later scans keep the
    // join current). Then list the rows with their item counts; the `Id`
    // stays the 32-hex genre_id_for(name) wire id (= genres.wire_id) so
    // existing client URLs + the /Items?ParentId pivot keep resolving.
    state
        .stores
        .backfill_genres()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let rows = state
        .stores
        .genres_with_counts()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let items: Vec<SynthItemDto> = rows
        .iter()
        .map(|gc| SynthItemDto {
            child_count: Some(gc.item_count),
            ..SynthItemDto::folder(
                gc.genre.wire_id.clone(),
                gc.genre.name.clone(),
                state.server_id.clone(),
                "Genre",
            )
        })
        .collect();
    let total = items.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
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
    q: CiQuery<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use crate::api::jellyfin::dto::artist_id_for;
    use std::collections::HashSet;
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // A-Z letter picker filters by artist name.
    let name_prefix = q
        .name_starts_with
        .as_deref()
        .or(q.name_starts_with_or_greater.as_deref())
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty());
    let mut seen: HashSet<String> = HashSet::new();
    let mut names: Vec<String> = Vec::new();
    for i in all.iter() {
        for src in [i.probe.album_artist.as_deref(), i.probe.artist.as_deref()] {
            if let Some(n) = src.filter(|s| !s.is_empty()) {
                if name_prefix
                    .as_deref()
                    .map_or(true, |pre| name_matches_letter(n, pre))
                    && seen.insert(n.to_string())
                {
                    names.push(n.to_string());
                }
            }
        }
    }
    names.sort();
    let items: Vec<SynthItemDto> = names
        .iter()
        .map(|n| SynthItemDto {
            image_tags: Some(Default::default()),
            backdrop_image_tags: Some(Vec::new()),
            genres: Some(Vec::new()),
            tags: Some(Vec::new()),
            ..SynthItemDto::folder(
                artist_id_for(n),
                n.clone(),
                state.server_id.clone(),
                "MusicArtist",
            )
        })
        .collect();
    let total = items.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
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
    q: CiQuery<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use crate::api::jellyfin::dto::{album_id_for, artist_id_for};
    use std::collections::HashMap;
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // A-Z letter picker filters by album name.
    let name_prefix = q
        .name_starts_with
        .as_deref()
        .or(q.name_starts_with_or_greater.as_deref())
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty());
    // Map album_name → (album_artist, sample track id) so a click
    // into the album renders with the right artist on the tile.
    let mut albums: HashMap<String, Option<String>> = HashMap::new();
    for i in all.iter() {
        let Some(name) = i.probe.album.as_deref() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if name_prefix
            .as_deref()
            .is_some_and(|pre| !name_matches_letter(name, pre))
        {
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
    let items: Vec<SynthItemDto> = names
        .into_iter()
        .map(|n| {
            let artist = albums.get(n).and_then(|a| a.clone());
            SynthItemDto {
                image_tags: Some(Default::default()),
                backdrop_image_tags: Some(Vec::new()),
                genres: Some(Vec::new()),
                tags: Some(Vec::new()),
                album_artist: artist.clone(),
                album_artists: artist.as_ref().map(|a| {
                    vec![NameGuidPairDto {
                        name: a.clone(),
                        id: artist_id_for(a),
                    }]
                }),
                ..SynthItemDto::folder(
                    album_id_for(n),
                    n.clone(),
                    state.server_id.clone(),
                    "MusicAlbum",
                )
            }
        })
        .collect();
    let total = items.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// `GET /Studios` — LIB-C3: studios (production companies / TV networks)
/// as entity rows. Lists every studio with its item count, name-ordered,
/// `Id` = the 32-hex studio wire id (= `studios.wire_id` =
/// `studio_id_for(name)`), so a client click routes to
/// `/Items?ParentId=<studio id>` which `restrict_to_parent` resolves via
/// the `item_studios` indexed join. Replaces the old stub that aggregated
/// `album_artist` (a music-path stand-in that never reflected real
/// production studios). Unlike /Genres there is no backfill (no legacy
/// studio column on media_items); studios come purely from the scanner
/// wire-in of MetadataResult.studios.
async fn list_studios(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::StudioStore;
    let rows = state
        .stores
        .studios_with_counts()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let items: Vec<SynthItemDto> = rows
        .iter()
        .map(|sc| SynthItemDto {
            child_count: Some(sc.item_count),
            ..SynthItemDto::folder(
                sc.studio.wire_id.clone(),
                sc.studio.name.clone(),
                state.server_id.clone(),
                "Studio",
            )
        })
        .collect();
    let total = items.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// `GET /Tags` — LIB-C6: tags (free-form labels) as entity rows. Lists
/// every tag with its item count, name-ordered, `Id` = the 32-hex tag
/// wire id (= `tags.wire_id` = `tag_id_for(name)`), so a client click
/// routes to `/Items?ParentId=<tag id>` which `restrict_to_parent`
/// resolves via the `item_tags` indexed join. Unlike /Genres there is no
/// backfill (no legacy tag column on media_items); tags come from the
/// scanner wire-in of MetadataResult.tags + the manual add/remove
/// endpoints.
async fn list_tags(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::TagStore;
    let rows = state
        .stores
        .tags_with_counts()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let items: Vec<SynthItemDto> = rows
        .iter()
        // B69 — "Tag" is NOT a kotlin BaseItemKind (crashes the strict SDK's
        // enum decode). A tag surfaces as a browsable "Folder" of its tagged
        // items — a valid BaseItemKind.
        .map(|tc| SynthItemDto {
            child_count: Some(tc.item_count),
            ..SynthItemDto::folder(
                tc.tag.wire_id.clone(),
                tc.tag.name.clone(),
                state.server_id.clone(),
                "Folder",
            )
        })
        .collect();
    let total = items.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// `POST /Items/{id}/Tags` — LIB-C6 manual mutation: add the `Tags` to the
/// item's `item_tags` set WITHOUT clobbering tags it already carries
/// (incremental). The path id is a numeric media id (the item being
/// tagged); `Tags` is a comma/pipe-separated label list. 404 when no
/// media item carries that id. Returns 204 (Jellyfin's metadata-mutation
/// shape).
async fn item_tags_add(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<TagMutateQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::{MediaStore, TagStore};
    let id = parse_media_id(&path.into_inner())?;
    // V-spirit: reject a tag mutation against a non-existent item rather
    // than silently creating an orphan join row.
    state
        .stores
        .get(id)
        .await
        .map_err(|_| error::ErrorNotFound("item not found"))?;
    let names = split_tag_csv(q.tags.as_deref());
    state
        .stores
        .add_item_tags(id, &names)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

/// `DELETE /Items/{id}/Tags` — LIB-C6 manual mutation: remove the `Tags`
/// from the item's `item_tags` set, leaving the rest intact. The tag rows
/// stay (they may serve other items); only the join links drop. 404 when
/// no media item carries that id. Returns 204.
async fn item_tags_remove(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<TagMutateQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::{MediaStore, TagStore};
    let id = parse_media_id(&path.into_inner())?;
    state
        .stores
        .get(id)
        .await
        .map_err(|_| error::ErrorNotFound("item not found"))?;
    let names = split_tag_csv(q.tags.as_deref());
    state
        .stores
        .remove_item_tags(id, &names)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

/// `GET /Persons` — LIB-C2: people (cast & crew) as entity rows. Lists
/// every person with its item count, name-ordered, `Id` = the 32-hex
/// person wire id (= `people.wire_id` = `person_id_for(name)`), so a
/// client click routes to `/Items?ParentId=<person id>` which
/// `restrict_to_parent` resolves via the `item_people` indexed join.
/// Unlike /Genres there is no backfill (no legacy people column).
async fn list_persons(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::PersonStore;
    let rows = state
        .stores
        .people_with_counts()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|pc| person_dto(&state, pc.person.clone(), pc.item_count))
        .collect();
    let total = items.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// `GET /Persons/{id}` — LIB-C2: a single Person item resolved by its
/// wire id. 404 when no person row carries that wire id.
async fn get_person(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::PersonStore;
    let wire_id = path.into_inner();
    let person = state
        .stores
        .person_by_wire_id(&wire_id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?
        .ok_or_else(|| error::ErrorNotFound("not found"))?;
    // Count how many items credit this person so the detail tile renders
    // "appears in N items" consistently with the list.
    let count = state
        .stores
        .item_ids_for_person(&wire_id)
        .await
        .map(|ids| ids.len() as u32)
        .unwrap_or(0);
    Ok(crate::api::jellyfin::wire::json(&person_dto(
        &state, person, count,
    )))
}

/// Shared Person item JSON. The Jellyfin Person item is an `IsFolder`
/// view (clicking it lists the person's filmography via ParentId). When
/// a headshot URL is recorded, advertise a `Primary` image tag so the
/// client requests `/Items/{id}/Images/Primary`.
fn person_dto(state: &AppState, person: pharos_core::Person, item_count: u32) -> serde_json::Value {
    // A stable tag derived from the wire id; the image branch resolves the
    // recorded thumb_url for this person id. Empty map (no headshot) still emits
    // "ImageTags": {} to match the prior literal.
    let mut image_tags = std::collections::BTreeMap::new();
    if person.thumb_url.is_some() {
        image_tags.insert("Primary".to_string(), person.wire_id.clone());
    }
    // B78/V38 — typed SynthItemDto, serialized to keep the Value return.
    serde_json::to_value(SynthItemDto {
        child_count: Some(item_count),
        image_tags: Some(image_tags),
        ..SynthItemDto::folder(
            person.wire_id.clone(),
            person.name.clone(),
            state.server_id.clone(),
            "Person",
        )
    })
    .unwrap_or(serde_json::Value::Null)
}

/// `GET /Collections` — LIB-C5: collections / box sets as entity rows.
/// Lists every box set with its member count, name-ordered, `Id` = the
/// 32-hex collection wire id (= `collections.wire_id` =
/// `collection_id_for(name)`), so a client click routes to
/// `/Items?ParentId=<collection id>` which `restrict_to_parent` resolves
/// via the `collection_items` indexed join (members in `sort_order`).
async fn list_collections(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::CollectionStore;
    let rows = state
        .stores
        .collections_with_counts()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|cc| collection_dto(&state, &cc.collection, cc.item_count))
        .collect();
    let total = items.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

/// Manual-CRUD create/add/remove query: `Name` (create only) + `Ids`
/// (comma-separated numeric media ids). Matches Jellyfin's
/// `POST /Collections?Name=&Ids=` and `/Collections/{id}/Items?Ids=`.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct CollectionMutateQuery {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    ids: Option<String>,
}

/// Parse a comma-separated `Ids=` into numeric media ids, dropping the
/// 32-hex synth-id namespace (anything that doesn't parse as a u64 ≤ the
/// 20-digit bound) so a stray wire id never collides with a numeric id.
pub(crate) fn parse_id_csv(raw: Option<&str>) -> Vec<u64> {
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            // ≤36 admits the dashed-GUID form; the old ≤20 guard (decimal
            // u64) silently dropped the canonical 32-hex ids (B15).
            .filter(|t| !t.is_empty() && t.len() <= 36)
            .filter_map(pharos_jellyfin_api::dto::parse_item_id)
            .collect()
    })
    .unwrap_or_default()
}

/// LIB-C6 manual tag mutation query: `Tags` (comma/pipe-separated label
/// names). Matches Jellyfin's `POST /Items/{id}/Tags?Tags=a,b`.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct TagMutateQuery {
    #[serde(default)]
    tags: Option<String>,
}

/// Parse the numeric media id off an `/Items/{id}/...` path segment. A
/// 32-hex synth id (or any non-numeric / over-long token) is rejected as
/// 400 — tag mutation targets a real media row, never an aggregate id.
fn parse_media_id(raw: &str) -> Result<u64, actix_web::Error> {
    // parse_item_id accepts the canonical 32-hex GUID, dashed, and legacy
    // decimal forms — and still rejects 32-hex SYNTH ids (non-zero high
    // half → None), preserving this guard's aggregate-id rejection.
    pharos_jellyfin_api::dto::parse_item_id(raw)
        .ok_or_else(|| error::ErrorBadRequest("invalid item id"))
}

/// Split a `Tags=` value into individual tag names — accept both `|`
/// (Jellyfin's wire convention) and `,`, trimming and dropping blanks.
fn split_tag_csv(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split(['|', ','])
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// `POST /Collections` — LIB-C5 manual CRUD: create a box set named
/// `Name`, optionally seeded with the `Ids` members (in request order).
/// Returns the new BoxSet item (its `Id` is the collection wire id) so
/// the client routes straight into it — matching Jellyfin's create shape.
async fn create_collection(
    state: web::Data<AppState>,
    _user: AuthUser,
    q: CiQuery<CollectionMutateQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::CollectionStore;
    let name = q
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error::ErrorBadRequest("Name is required"))?;
    let ids = parse_id_csv(q.ids.as_deref());
    let collection = state
        .stores
        .create_collection(name, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let count = state
        .stores
        .collection_items(&collection.wire_id)
        .await
        .map(|m| m.len() as u32)
        .unwrap_or(ids.len() as u32);
    Ok(crate::api::jellyfin::wire::json(&collection_dto(
        &state,
        &collection,
        count,
    )))
}

/// `POST /Collections/{id}/Items` — LIB-C5 manual CRUD: add the `Ids`
/// members to the box set named by the wire id. 404 when the wire id
/// matches no collection. Returns 204 (Jellyfin's add-items shape).
async fn collection_items_add(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<CollectionMutateQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::CollectionStore;
    let wire_id = path.into_inner();
    let ids = parse_id_csv(q.ids.as_deref());
    let added = state
        .stores
        .add_collection_items(&wire_id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if added.is_none() {
        return Err(error::ErrorNotFound("collection not found"));
    }
    Ok(HttpResponse::NoContent().finish())
}

/// `DELETE /Collections/{id}/Items` — LIB-C5 manual CRUD: remove the
/// `Ids` members from the box set named by the wire id. 404 when the wire
/// id matches no collection. Returns 204 (Jellyfin's remove-items shape).
async fn collection_items_remove(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<CollectionMutateQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::CollectionStore;
    let wire_id = path.into_inner();
    let ids = parse_id_csv(q.ids.as_deref());
    let removed = state
        .stores
        .remove_collection_items(&wire_id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if removed.is_none() {
        return Err(error::ErrorNotFound("collection not found"));
    }
    Ok(HttpResponse::NoContent().finish())
}

/// Shared BoxSet item JSON. A Jellyfin collection is an `IsFolder`
/// BoxSet view — clicking it lists the members via
/// `/Items?ParentId=<wire id>`. The `Id` is the collection wire id so the
/// id round-trips byte-identically through the ParentId pivot and the
/// `/Items/{id}` BoxSet short-circuit.
fn collection_dto(
    state: &AppState,
    collection: &pharos_core::Collection,
    item_count: u32,
) -> serde_json::Value {
    // B78/V38 — typed SynthItemDto, serialized to keep the Value return.
    serde_json::to_value(SynthItemDto {
        child_count: Some(item_count),
        collection_type: Some("boxsets"),
        overview: collection
            .overview
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        image_tags: Some(Default::default()),
        backdrop_image_tags: Some(Vec::new()),
        genres: Some(Vec::new()),
        tags: Some(Vec::new()),
        ..SynthItemDto::folder(
            collection.wire_id.clone(),
            collection.name.clone(),
            state.server_id.clone(),
            "BoxSet",
        )
    })
    .unwrap_or(serde_json::Value::Null)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct NextUpQuery {
    #[serde(default)]
    user_id: Option<String>,
    /// When a Series detail page requests Next Up it passes `SeriesId`;
    /// scope the result to that one series. Absent on the home-screen
    /// "Next Up" row, which wants one pick across the whole library.
    #[serde(default)]
    series_id: Option<String>,
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
    q: CiQuery<NextUpQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let ids: Vec<u64> = all.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.0.id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // Group episodes by series; pick the lowest unwatched per series.
    use crate::api::jellyfin::dto::series_id_for_key;
    use std::collections::HashMap;
    let mut buckets: HashMap<String, Vec<(usize, &MediaItem)>> = HashMap::new();
    for (idx, item) in all.iter().enumerate() {
        if !matches!(item.kind, MediaKind::Episode) {
            continue;
        }
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        // A Series-page request scopes to that series (`SeriesId`); the
        // home-screen row leaves it unset and gets one pick per series.
        if let Some(want) = q.series_id.as_deref() {
            if series_id_for_key(series.series_folder.as_deref(), &series.series_name).as_str()
                != want
            {
                continue;
            }
        }
        // Skip already-played episodes.
        if user_data.get(idx).copied().unwrap_or_default().played {
            continue;
        }
        // LIB-C11 — bucket by the folder-keyed identity, not the bare
        // name, so two same-name shows in distinct folders stay separate
        // and don't interleave into one NextUp pick.
        buckets
            .entry(series.series_key().to_string())
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
                let n = e
                    .series
                    .as_ref()
                    .and_then(|s| s.episode_number)
                    .unwrap_or(0);
                (s, n)
            });
            eps.into_iter().next()
        })
        .collect();
    // Stable series-name sort across the result set.
    picks.sort_by(|a, b| {
        let an =
            a.1.series
                .as_ref()
                .map(|s| s.series_name.as_str())
                .unwrap_or("");
        let bn =
            b.1.series
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
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud).with_trickplay(
                &item.probe,
                &state.generated_trickplay_widths(item.id),
                state.trickplay_interval_ms,
            )
        })
        .collect();
    let total = dtos.len() as u32;
    let _ = q.user_id; // kept for future per-user scoping
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": dtos,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case", default)]
struct ShowsEpisodesQuery {
    /// Scope to a single season (jellyfin-web passes this when a season is
    /// selected). Absent → every episode of the series.
    season_id: Option<String>,
    user_id: Option<String>,
    limit: Option<u32>,
    /// Offset into the (season-, episode-)ordered list. jellyfin-web pages the
    /// episode list on scroll; ignoring this returned the SAME items from the
    /// top for every page, so the client appended duplicates (ep1…ep23, then
    /// ep1, ep23, … again).
    start_index: Option<u32>,
    /// Window the list to start AT this item id (jellyfin-web's "episodes from
    /// the next-up episode" on the series page). Applied after ordering, before
    /// `Limit`. Takes precedence over `StartIndex` when both are present.
    start_item_id: Option<String>,
}

/// Resolve the `[start, end)` slice of the ordered episode list for one page.
/// `StartItemId` (jellyfin-web's "episodes from the next-up item") wins over
/// `StartIndex`; an unknown id falls back to the top rather than an empty page.
/// `Limit` bounds the count. Pure so the paging logic is unit-tested without the
/// handler's `AppState`. The bug this fixes: ignoring `StartIndex` returned the
/// SAME items from the top for every page, so a scrolling client appended
/// duplicates (ep1…ep23, then ep1, ep23, … again).
fn resolve_episode_window(
    ids: &[String],
    start_index: Option<u32>,
    start_item_id: Option<&str>,
    limit: Option<u32>,
) -> std::ops::Range<usize> {
    let len = ids.len();
    let start = if let Some(want) = start_item_id {
        ids.iter().position(|id| id == want).unwrap_or(0)
    } else {
        (start_index.unwrap_or(0) as usize).min(len)
    };
    let take = limit.map(|l| l.max(1) as usize).unwrap_or(usize::MAX);
    let end = start.saturating_add(take).min(len);
    start..end
}

/// `GET /Shows/{id}/Episodes` — the episodes of a series (optionally one
/// season), ordered by (season, episode). jellyfin-web's series/season pages
/// and its "play next episode" resolution both drive off this; without it the
/// series page 404s and playback can't pick an episode.
async fn shows_episodes(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<ShowsEpisodesQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use crate::api::jellyfin::dto::{season_id_for_key, series_id_for_key};
    let series_id = path.into_inner();
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let ids: Vec<u64> = all.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.0.id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut eps: Vec<(usize, &MediaItem)> = all
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item.kind, MediaKind::Episode))
        .filter(|(_, item)| {
            item.series.as_ref().is_some_and(|s| {
                series_id_for_key(s.series_folder.as_deref(), &s.series_name) == series_id
            })
        })
        .filter(|(_, item)| {
            let Some(want) = q.season_id.as_deref() else {
                return true;
            };
            item.series.as_ref().is_some_and(|s| {
                s.season_number.is_some_and(|n| {
                    season_id_for_key(s.series_folder.as_deref(), &s.series_name, n) == want
                })
            })
        })
        .collect();
    eps.sort_by_key(|(_, e)| {
        let s = e.series.as_ref().and_then(|s| s.season_number).unwrap_or(0);
        let n = e
            .series
            .as_ref()
            .and_then(|s| s.episode_number)
            .unwrap_or(0);
        (s, n)
    });
    // TotalRecordCount is the full ordered set — computed BEFORE windowing so
    // the client can page correctly (reporting the truncated count made it
    // request past the real end and re-append the same items).
    let total = eps.len() as u32;
    // Canonical wire ids; window matching normalizes the client's
    // StartItemId to the same form below.
    let ids: Vec<String> = eps
        .iter()
        .map(|(_, e)| pharos_jellyfin_api::dto::wire_item_id(e.id))
        .collect();
    // Normalize the client's StartItemId to the canonical wire form so a
    // legacy-decimal id still matches the hex ids in `ids`.
    let start_item_canonical = q
        .start_item_id
        .as_deref()
        .and_then(pharos_jellyfin_api::dto::parse_item_id)
        .map(pharos_jellyfin_api::dto::wire_item_id);
    let window = resolve_episode_window(
        &ids,
        q.start_index,
        start_item_canonical.as_deref(),
        q.limit,
    );
    let start = window.start;
    let dtos: Vec<BaseItemDto> = eps
        .get(window)
        .unwrap_or(&[])
        .iter()
        .map(|(idx, item)| {
            let ud = user_data.get(*idx).copied().unwrap_or_default();
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud).with_trickplay(
                &item.probe,
                &state.generated_trickplay_widths(item.id),
                state.trickplay_interval_ms,
            )
        })
        .collect();
    let _ = q.user_id.as_deref();
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": dtos,
        "TotalRecordCount": total,
        "StartIndex": start as u32,
    })))
}

/// `GET /Shows/{id}/Seasons` — the distinct seasons of a series as synthetic
/// Season folder items, ordered by season number.
async fn shows_seasons(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    use crate::api::jellyfin::dto::{season_display_name, series_id_for_key};
    use std::collections::BTreeMap;
    let series_id = path.into_inner();
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // One representative SeriesInfo per season number (BTreeMap → ascending).
    let mut seasons: BTreeMap<u32, &pharos_core::SeriesInfo> = BTreeMap::new();
    for item in all.iter() {
        if !matches!(item.kind, MediaKind::Episode) {
            continue;
        }
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        if series_id_for_key(series.series_folder.as_deref(), &series.series_name) != series_id {
            continue;
        }
        let Some(n) = series.season_number else {
            continue;
        };
        seasons.entry(n).or_insert(series);
    }
    let items: Vec<serde_json::Value> = seasons
        .iter()
        .map(|(n, series)| season_dto(&state.server_id, series, *n, &season_display_name(*n)))
        .collect();
    let total = items.len() as u32;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct PlaybackInfoBody {
    device_profile: Option<DeviceProfile>,
    /// Audio/subtitle track picks jellyfin-web sends when the user switches
    /// tracks on a TRANSCODED stream: it re-POSTs PlaybackInfo with the new
    /// index in the `playbackInfoDto` BODY (not the query string) and reloads
    /// the returned TranscodingUrl. Threaded into the transcode URL below so
    /// the reloaded stream actually carries the chosen track. Subtitle uses
    /// `i64` because Jellyfin's "off" sentinel is `-1`.
    audio_stream_index: Option<u32>,
    subtitle_stream_index: Option<i64>,
}

async fn playback_info(
    state: web::Data<AppState>,
    user: AuthUser,
    req: actix_web::HttpRequest,
    path: web::Path<String>,
    body: Option<web::Json<PlaybackInfoBody>>,
) -> Result<impl Responder, actix_web::Error> {
    let id_str = path.into_inner();
    // The HLS master/variant playlists and every segment authenticate via an
    // `api_key` query param — hls.js fetches them with no auth header, so the
    // TranscodingUrl handed to the client MUST carry the caller's token or the
    // very first `master.m3u8` request 401s and playback dies with a fatal
    // `manifestLoadError`. Recover the bearer that authenticated THIS request.
    let api_key = crate::api::jellyfin::auth_extractor::extract_token(&req).unwrap_or_default();
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    // Canonical wire form regardless of which shape the caller sent —
    // TranscodingUrl / MediaSource.Id must round-trip consistently.
    let id_str = pharos_jellyfin_api::dto::wire_item_id(id);
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    // Bump this show to the front of the background trickplay pre-generator —
    // it (and the rest of its series) is about to be watched, so its scrub
    // previews should be built first. Best-effort; a full queue never blocks.
    if let Some(tx) = &state.trickplay_priority {
        let _ = tx.send(id);
    }
    // Warm THIS item's text subtitles NOW, ungated, so extraction starts the
    // moment playback is negotiated — seconds before the client toggles subs.
    // The gated library warm-all parks during active playback, so an
    // un-warmed title's first Stream.vtt/js/ass would otherwise cold-demux the
    // whole file (~seconds–30s) and the client gives up (the "swap away and
    // back to make a sub work" symptom — the retry hits the now-warm cache).
    // Idempotent: the persistent cache skips already-warm tracks instantly.
    crate::library_watch::spawn_warm_item_subtitles(state.clone(), id);
    // P4 — defensive resume offset. Clients that drive playback
    // purely from PlaybackInfo (Finamp, Jellyfin-Android-TV) never
    // see UserData.PlaybackPositionTicks via /Items/{id}; emit it
    // here too. `played=true` → no resume (Jellyfin convention:
    // already-watched items restart from 0).
    let resume_ticks = match state.stores.get_user_data(user.0.id, id).await {
        Ok(ud) if !ud.played => ud.last_played_position_ticks,
        _ => 0,
    };
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
        video_level: probe.video_level,
        video_profile: probe.video_profile.clone(),
        audio_channels: probe.audio_channels,
        width: probe.width,
        height: probe.height,
        video_bit_depth: pharos_jellyfin_api::device_profile::bit_depth_from_pix_fmt(
            probe.pixel_format.as_deref(),
        ),
    };

    // Consume the POST body once: the DeviceProfile drives negotiation, and
    // the track picks (present when jellyfin-web reloads for an audio/subtitle
    // switch) fall back into the transcode URL below.
    let (profile, body_audio_index, body_subtitle_index) = match body.map(web::Json::into_inner) {
        Some(b) => (
            b.device_profile.unwrap_or_default(),
            b.audio_stream_index,
            b.subtitle_stream_index,
        ),
        None => (DeviceProfile::default(), None, None),
    };
    let decision = negotiate(&profile, &source);

    // B75 — native direct-play now authenticates via a capability token (the
    // MediaSource ETag = PlaySessionId, which the SDK forwards as `?tag=`; the
    // stream route validates it against the registered session — see
    // `stream::authorize_media`). So we no longer force a transcode on native
    // clients: `negotiate()` already picked the correct per-DeviceProfile shape
    // (DirectPlay when the client can decode the source, transcode otherwise).
    // Forcing a transcode here (the old B73 workaround) was WRONG for every
    // device that can direct-play — it re-encoded needlessly and, on
    // memory-tight TVs, the extra transcode + HLS churn on seek got the app
    // OOM-killed. Trust the negotiator.
    //
    // `is_web_client` still distinguishes the browser (self-authenticating
    // `<video src>` with api_key + JellyfinAuth cookie) from a native app for
    // the resume-offset handling below (B74): only native players replay the
    // TranscodingUrl verbatim and need StartTimeTicks baked into it.
    let is_web_client = req
        .headers()
        .get(actix_web::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.contains("Mozilla"));

    // Firefox/Gecko browsers (including Zen) report `canPlayType("…avc1…")` =
    // "probably" — so jellyfin-web advertises H.264 and lists it first — yet
    // their Media Source Extensions frequently CANNOT decode H.264
    // (`isTypeSupported` → false). hls.js then aborts our H.264 HLS with a
    // fatal manifestParsingError ("no level with compatible codecs"). pharos
    // can't see that canPlayType/MSE mismatch from the profile, so use a
    // targeted client quirk: when a Firefox-family UA ALSO advertises a VP9
    // transcode target, serve the progressive VP9/WebM stream (which its MSE
    // decodes reliably) instead. Chromium/Safari keep the efficient H.264 HLS.
    let is_firefox = req
        .headers()
        .get(actix_web::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        // Match "Firefox" only — Chrome's UA contains "like Gecko" so a
        // bare "Gecko" check would wrongly capture Chromium. B43 — scope the
        // quirk to DESKTOP LINUX Firefox: only there can the distro build
        // lack the system H.264 decoder while canPlayType still says
        // "probably" (the lie this force exists for). Firefox on
        // macOS (VideoToolbox), Windows (Media Foundation) and Android
        // (MediaCodec) always decodes H.264 for real — forcing those onto
        // the VP9 encode path turned an h264 source's near-instant remux
        // (stream copy) into a ~2.5s-per-segment libvpx encode, the
        // dominant cost of every seek.
        .is_some_and(|ua| {
            ua.contains("Firefox") && ua.contains("Linux") && !ua.contains("Android")
        })
        // B60 — opt out of the VP9 force for Linux Firefox when configured:
        // modern Linux FF decodes H.264 in MSE, so serving h264 puts it on the
        // shared encode and off the fragile VP9 split-audio path (the Sabrina
        // hang). Reversible via `[server].linux_firefox_h264`.
        && !state.linux_firefox_h264;
    let client_offers_vp9 = profile.transcoding_profiles.iter().any(|t| {
        (t.kind.is_empty() || t.kind.eq_ignore_ascii_case("Video"))
            && t.video_codec.split(',').any(|c| {
                matches!(
                    c.trim().to_ascii_lowercase().as_str(),
                    "vp9" | "vp8" | "vp09" | "vp08"
                )
            })
    });
    // B60 — evidence for the codec-decision trace: does the client advertise
    // h264 as a transcode target at all? If a Linux Firefox that hangs on VP9
    // never even offers h264, the VP9 force is genuinely required for it.
    let client_offers_h264 = profile.transcoding_profiles.iter().any(|t| {
        (t.kind.is_empty() || t.kind.eq_ignore_ascii_case("Video"))
            && t.video_codec.split(',').any(|c| {
                matches!(
                    c.trim().to_ascii_lowercase().as_str(),
                    "h264" | "avc" | "avc1"
                )
            })
    });
    // The video codecs a modern Firefox reliably decodes in ANY context
    // (MSE or a plain <video>): the VP/AV1 family. THIS user's Firefox can't
    // do H.264 at all, so treat h264/hevc/mpeg4/… as un-decodable and force a
    // VP9 transcode. A genuine VP9/VP8/AV1 source is left to direct-play.
    let source_firefox_native = matches!(
        source
            .video_codec
            .as_deref()
            .map(|c| c.to_ascii_lowercase())
            .as_deref(),
        Some("vp9" | "vp09" | "vp8" | "vp08" | "av1" | "av01")
    );
    // Firefox client-quirk: jellyfin-web advertises H.264 (canPlayType says
    // "probably") and will DirectPlay / remux / HLS-transcode it, but the
    // browser's decoder rejects it. Force a progressive VP9/WebM transcode
    // UNLESS the negotiated outcome is already a direct-play of a codec Firefox
    // natively decodes (VP8/VP9/AV1). This also covers an AV1/VP9 source in a
    // container the client's profile doesn't direct-play (e.g. av1-in-mp4 when
    // the profile only lists av1-in-webm) — that would otherwise transcode to
    // H.264, which Firefox can't play.
    let force_webm = is_video
        && is_firefox
        && client_offers_vp9
        && !(decision.is_direct() && source_firefox_native);

    let direct_play = decision.is_direct() && !force_webm;
    // B77 — `SupportsDirectStream` promises the raw `/stream` file is playable
    // AS-IS: pharos's `/stream` serves the source verbatim (no container remux,
    // no audio transcode — the only massaging is `deliver_stream`'s webm
    // re-label). So it's true ONLY when the file genuinely plays for THIS
    // client: a real DirectPlay, OR a webm-codec Matroska (VP8/9/AV1 + Opus/
    // Vorbis) that the re-label serves as `video/webm` to a client that decodes
    // VP9. A VideoRemux/AudioRemux of anything else — the h264+eac3/ac3/dts
    // Matroska that dominates the library — is NOT playable raw (browser rejects
    // the container/audio), so it must transcode via the TranscodingUrl below.
    // Advertising DirectStream for it handed the client an unplayable stream
    // with (for AudioRemux) no URL to fall back to. Native players that CAN play
    // such files declare it in their profile → DirectPlay, unaffected.
    let webm_relabel_playable = {
        let lc = |c: &Option<String>| c.as_deref().map(|s| s.to_ascii_lowercase());
        let v = lc(&source.video_codec);
        let a = lc(&source.audio_codec);
        let cont = source.container.to_ascii_lowercase();
        let matroska = cont.contains("matroska") || cont.contains("webm") || cont.contains("mkv");
        // The client can actually consume the re-labelled `video/webm` when it
        // lists a webm DirectPlayProfile covering the source's video codec — the
        // precise "would this play if the container were webm" test, so a
        // vp9-in-matroska on a webm-capable client still direct-streams rather
        // than needlessly transcoding.
        let client_plays_webm_video = source.video_codec.as_deref().is_some_and(|sv| {
            profile.direct_play_profiles.iter().any(|p| {
                (p.kind.is_empty() || p.kind.eq_ignore_ascii_case("Video"))
                    && p.container
                        .split(',')
                        .any(|c| c.trim().eq_ignore_ascii_case("webm"))
                    && p.video_codec
                        .split(',')
                        .any(|c| c.trim().eq_ignore_ascii_case(sv))
            })
        });
        matroska
            && client_plays_webm_video
            && matches!(
                v.as_deref(),
                Some("vp9" | "vp09" | "vp8" | "vp08" | "av1" | "av01")
            )
            // Audio must be webm-legal too, else the re-labelled video/webm
            // plays video but the browser can't decode the audio track.
            && matches!(a.as_deref(), Some("opus" | "vorbis") | None)
    };
    let supports_direct_stream = !force_webm
        && (direct_play
            || (matches!(
                decision,
                Decision::VideoRemux { .. } | Decision::AudioRemux { .. }
            ) && webm_relabel_playable));
    // Forward the client's audio/subtitle track selection into the VP9 HLS
    // URL — jellyfin-web re-requests PlaybackInfo with these when the user
    // switches tracks. The VP9 segment handler maps them into ffmpeg (audio
    // select) + burns the chosen subtitle in.
    let stream_selection = {
        let q = req.query_string();
        // Query string wins (legacy/other clients + explicit overrides); the
        // POST body is the fallback, since jellyfin-web's transcode audio/
        // subtitle switch sends the index ONLY in the body.
        let from_query = |key: &str| -> Option<String> {
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        let mut s = String::new();
        let mut push = |key: &str, val: String| {
            s.push('&');
            s.push_str(key);
            s.push('=');
            s.push_str(&val);
        };
        if let Some(v) =
            from_query("AudioStreamIndex").or_else(|| body_audio_index.map(|n| n.to_string()))
        {
            push("AudioStreamIndex", v);
        }
        // Subtitle "off" (-1) is meaningful to the segment handler (it clears
        // any prior burn-in) — forward it, unlike an absent value.
        //
        // Only forward a POSITIVE index when the selected track is an IMAGE
        // subtitle (PGS/VOBSUB) — those MUST burn into the transcode. A TEXT /
        // ASS sub is delivered as a separate `External` rendition the client
        // renders (SubtitlesOctopus / cue JSON), so burning it is redundant AND
        // catastrophically slow: a burned-sub VP9 segment uses output seeking
        // (decode from 0), so a segment deep in the file takes tens of seconds
        // (Code Geass S01E01 seg ~90 measured at ~100 s for 6 s of content →
        // constant stutter). Never bake a text-sub index into the transcode URL.
        if let Some(v) =
            from_query("SubtitleStreamIndex").or_else(|| body_subtitle_index.map(|n| n.to_string()))
        {
            let forward = v.trim() == "-1"
                || v.trim()
                    .parse::<u32>()
                    .ok()
                    .and_then(|abs| {
                        probe
                            .subtitle_tracks
                            .iter()
                            .find(|t| t.stream_index == abs)
                            .map(|t| {
                                !crate::api::jellyfin::dto::is_text_subtitle_codec(
                                    t.codec.as_deref(),
                                )
                            })
                    })
                    // Unknown index → be safe and don't burn.
                    .unwrap_or(false);
            if forward {
                push("SubtitleStreamIndex", v);
            }
        } else if let Some(def) = probe.subtitle_tracks.iter().find(|t| {
            t.is_default && !crate::api::jellyfin::dto::is_text_subtitle_codec(t.codec.as_deref())
        }) {
            // B44 — no client pick at all: honour the container's DEFAULT
            // disposition when it names an IMAGE subtitle. Forced-Na'vi-style
            // tracks (Avatar: PGS "Forced Stylized (Na'vi)", default+forced)
            // exist precisely so untranslated-dialogue sections play
            // subtitled WITHOUT the user doing anything — real Jellyfin
            // burns them by default. We advertise the track via
            // DefaultSubtitleStreamIndex, so the client already believes
            // it's active; not baking it into the transcode URL silently
            // played those sections unsubtitled. (Text defaults still
            // deliver External — the client renders them itself.)
            push("SubtitleStreamIndex", def.stream_index.to_string());
        }
        s
    };
    // VP9-in-fMP4 HLS, NOT progressive WebM. A progressive `<video src>` VP9
    // stream plays in Firefox but cannot seek to an unbuffered position and
    // reports no reliable position for resume. Serving VP9 as fMP4 HLS (like
    // Jellyfin) gives hls.js a seekable VOD playlist — restoring seeking,
    // resume, and mid-playback track switching. SubProtocol resolves to "hls".
    let vp9_hls_url = format!(
        "/videos/{id_str}/vp9/master.m3u8?PlaySessionId={play_session_id}&api_key={api_key}{stream_selection}"
    );
    let transcoding_url = if force_webm {
        Some(vp9_hls_url.clone())
    } else {
        match &decision {
            // Any VIDEO transcode drives the HLS master playlist. pharos's HLS
            // pipeline always emits mpegts H.264/AAC segments regardless of the
            // container the client's profile nominally requested (hls.js demuxes
            // mpegts fine), so the URL must be emitted for every video transcode —
            // not only when the negotiated target_container happened to be "ts".
            // Gating on `== "ts"` left `SupportsTranscoding: true` with a null
            // TranscodingUrl whenever a client profile requested e.g. an mp4/hls
            // transcode, and jellyfin-web then failed with "error processing the
            // request" before ever fetching a segment.
            // PlaySessionId rides on the URL so the HLS handlers look up the cached
            // Decision instead of re-running the negotiator per segment.
            // A client whose transcoding profile asks for VP9/VP8 (a browser whose
            // MSE can't decode H.264 — e.g. some Firefox/Zen builds) gets a
            // VP9-in-fMP4 HLS stream instead of the H.264/mpegts HLS surface,
            // which it could not play. Everything else takes the H.264 HLS master.
            Decision::Transcode {
                target_video_codec, ..
            } if is_video
                && target_video_codec.as_deref().is_some_and(|c| {
                    matches!(
                        c.to_ascii_lowercase().as_str(),
                        "vp9" | "vp8" | "vp09" | "vp08"
                    )
                }) =>
            {
                Some(vp9_hls_url.clone())
            }
            // B41 — carry the caller's stream selection (audio track + image-
            // subtitle burn index) exactly like the VP9 URL: the h264 master
            // URL dropped it, so a selected PGS subtitle never reached the
            // segment handler (burn silently off) and an explicit audio track
            // fell back to the default.
            Decision::Transcode { .. } if is_video => Some(format!(
                "/videos/{id_str}/master.m3u8?PlaySessionId={play_session_id}&api_key={api_key}{stream_selection}"
            )),
            // Audio transcode → the universal audio endpoint.
            Decision::Transcode { .. } => Some(format!(
                "/audio/{id_str}/universal?PlaySessionId={play_session_id}&api_key={api_key}"
            )),
            // P9 — VideoRemux drives the same HLS surface as Transcode;
            // the segment handler reads `Decision::VideoRemux` from the
            // registered session and emits `-c:v copy`.
            Decision::VideoRemux { .. } => Some(format!(
                // Remux copies video (no burn possible) but the AUDIO pick
                // must still ride along (B41).
                "/videos/{id_str}/master.m3u8?PlaySessionId={play_session_id}&api_key={api_key}{stream_selection}"
            )),
            // B77 — a VIDEO AudioRemux (compatible video, undecodable audio)
            // drives the same HLS master as VideoRemux: the segment surface
            // re-encodes to h264 + aac (a `-c:v copy` segment stream is
            // structurally broken, see hls.rs/B45), giving the client audio it
            // can actually decode. Without this the client got DirectStream'd
            // raw eac3/ac3/dts and no URL to fall back to.
            Decision::AudioRemux { .. } if is_video => Some(format!(
                "/videos/{id_str}/master.m3u8?PlaySessionId={play_session_id}&api_key={api_key}{stream_selection}"
            )),
            // Audio-only source whose codec the client can't direct-play →
            // the universal audio endpoint remuxes to the negotiated target.
            Decision::AudioRemux { .. } => Some(format!(
                "/audio/{id_str}/universal?PlaySessionId={play_session_id}&api_key={api_key}"
            )),
            _ => None,
        }
    };
    // B74 — native players (Android TV / ExoPlayer) use the TranscodingUrl
    // verbatim and do NOT client-seek a transcode, so a resume must ride the
    // URL as `StartTimeTicks` (real Jellyfin does exactly this). Web self-seeks
    // in the full VOD playlist + carries its own resume via the top-level
    // StartPositionTicks, so its URL is left untouched. On the native side this
    // reaches `render_variant_playlist` (via playback_qs, B74) which emits
    // `EXT-X-START:TIME-OFFSET` → the player lands at the resume point instead
    // of 0:00 (deep segments transcode as fast as seg 0 — input seek).
    let transcoding_url = match transcoding_url {
        Some(u) if !is_web_client && resume_ticks > 0 && !u.contains("StartTimeTicks") => {
            Some(format!("{u}&StartTimeTicks={resume_ticks}"))
        }
        other => other,
    };
    // P9 — emitted container reflects the negotiated target when remuxing.
    let advertised_container = match &decision {
        Decision::VideoRemux {
            target_container, ..
        } => target_container.clone(),
        _ => container.clone(),
    };

    // B59 — codec-decision trace: exactly why a client got the codec it did.
    // The VP9-vs-h264 split is what breaks SyncPlay encode-sharing (a member
    // on VP9 can't share the group's warm h264 encode), so log every input to
    // the decision — the negotiated target codec, the Firefox force, whether
    // the client advertised VP9 — and the final route (vp9 fMP4 vs h264 HLS).
    let negotiated_video_codec = match &decision {
        Decision::Transcode {
            target_video_codec, ..
        } => target_video_codec.clone(),
        _ => None,
    };
    let route = match transcoding_url.as_deref() {
        Some(u) if u.contains("/vp9/") => "vp9-fmp4",
        Some(u) if u.contains("/master.m3u8") => "h264-hls",
        Some(_) => "other",
        None => "direct",
    };
    tracing::info!(
        media.id = id,
        user.name = %user.0.name,
        is_video,
        is_firefox,
        client_offers_vp9,
        client_offers_h264,
        force_webm,
        source_codec = source.video_codec.as_deref().unwrap_or("?"),
        negotiated_video_codec = negotiated_video_codec.as_deref().unwrap_or("-"),
        route,
        "playbackinfo: codec decision"
    );

    // Register the negotiated Decision under this PlaySessionId. For a
    // transcode the HLS segment generator reads it back to honour the target
    // codec / container / bitrate cap. For DIRECT PLAY we still register it
    // (B75): the session doubles as the stream capability token — the ETag we
    // hand back equals this PlaySessionId, and `stream::authorize_media`
    // authorizes the tokenless native `/stream?tag=…` request by resolving the
    // session and matching its media_id. The registry self-expires (5 min
    // in-memory / 6 h durable), so no-op direct-play entries don't accumulate.
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

    let sidecars = discover_sidecars(&item.path).await;
    let sidecar_langs: Vec<Option<String>> = sidecars
        .iter()
        .map(|(p, _)| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(crate::api::jellyfin::subtitles::sidecar_language_from_name)
        })
        .collect();
    let ctx = SubtitleStreamCtx {
        item_id: item.id,
        sidecar_count: sidecars.len() as u32,
        sidecar_langs,
    };
    let streams = build_media_streams_with_subtitles(probe, is_video, Some(&ctx));
    // Embedded fonts → MediaAttachments so jellyfin-web hands them to
    // SubtitlesOctopus and ASS/SSA subtitles render.
    let media_attachments = build_media_attachments(item.id, &probe.attachments);
    // Find the audio stream's actual index (or skip if there isn't one).
    // Hard-coding `1` for silent-video files made jellyfin-web's player
    // try to select a track that doesn't exist.
    let default_audio_stream_index: Option<u32> =
        streams.iter().find(|s| s.kind == "Audio").map(|s| s.index);

    // P12 — `DefaultSubtitleStreamIndex` resolution priority:
    //   1. Any subtitle track flagged `is_default` (probed from the
    //      container's disposition bits).
    //   2. The first English-language track.
    //   3. The first subtitle track of any kind.
    // None when no subtitle tracks exist — client renders no default.
    let default_subtitle_stream_index: Option<u32> = streams
        .iter()
        .find(|s| s.kind == "Subtitle" && s.is_default)
        .map(|s| s.index)
        .or_else(|| {
            streams
                .iter()
                .find(|s| {
                    s.kind == "Subtitle"
                        && s.language
                            .as_deref()
                            .map(|l| l.eq_ignore_ascii_case("eng") || l.eq_ignore_ascii_case("en"))
                            .unwrap_or(false)
                })
                .map(|s| s.index)
        })
        .or_else(|| {
            streams
                .iter()
                .find(|s| s.kind == "Subtitle")
                .map(|s| s.index)
        });

    // TranscodingSubProtocol only makes sense alongside a real TranscodingUrl.
    // Every video transcode pharos emits is now an HLS `.m3u8` surface (H.264
    // mpegts OR VP9 fMP4), driven by hls.js → SubProtocol "hls". The `.webm`
    // guard remains defensive: a progressive `<video src>` `.webm` transcode
    // must be "http" (feeding its bytes to hls.js yields manifestParsingError),
    // in case any path is re-pointed at the legacy progressive handler.
    // NEVER null: jellyfin-sdk-kotlin's MediaSourceInfo marks
    // TranscodingSubProtocol as a REQUIRED (non-nullable) MediaStreamProtocol
    // enum — a null (or absent) value fails the whole PlaybackInfo
    // deserialization in the native Android/TV apps. Real Jellyfin emits
    // "http" when not transcoding.
    let transcoding_sub_protocol = match transcoding_url.as_deref() {
        Some(u) if u.contains(".webm") => "http",
        Some(_) => "hls",
        None => "http",
    };

    // B78/V38 — typed MediaSourceInfoDto (not a json! literal): the constant /
    // required fields (V9 path-omission, the B13 non-null value set, the
    // never-null TranscodingSubProtocol enum, P17 tuning hints) live in
    // `MediaSourceInfoDto::default()`; only the item-specific fields are set
    // here. `ETag` (B75) doubles as the direct-play capability token equal to
    // PlaySessionId, disclosed only in this authenticated response.
    let primary_source = MediaSourceInfoDto {
        id: id_str.clone(),
        container: advertised_container.clone(),
        e_tag: play_session_id.clone(),
        run_time_ticks: probe.run_time_ticks(),
        size: probe.size_bytes,
        name: item.title.clone(),
        supports_direct_play: direct_play,
        supports_direct_stream,
        transcoding_url,
        transcoding_sub_protocol,
        media_streams: streams.clone(),
        media_attachments: media_attachments.clone(),
        bitrate: probe.bitrate_bps,
        default_audio_stream_index,
        default_subtitle_stream_index,
        start_position_ticks: resume_ticks,
        ..Default::default()
    };

    // P34 — additional editions probed for this item. We don't
    // re-run negotiation per alternate (a scanner-side enrichment
    // task should land that); each row inherits the primary's
    // Decision so jellyfin-web's edition picker hands the user a
    // playable URL. Direct-play falls back to the primary's
    // container negotiation result rather than asserting the
    // alternate matches.
    let mut media_sources = Vec::with_capacity(1 + probe.alternate_sources.len());
    media_sources.push(primary_source);
    for alt in &probe.alternate_sources {
        let alt_id = format!("{id_str}-{}", alt.id);
        let alt_container = alt
            .container
            .clone()
            .unwrap_or_else(|| advertised_container.clone());
        // Alternates default to transcode-only (SupportsDirect* false,
        // TranscodingUrl None, "http" sub-protocol, 0 resume) until
        // scanner-side negotiation lands — all from `Default`. `Name` carries
        // the edition label (P34); streams/attachments reuse the primary's.
        media_sources.push(MediaSourceInfoDto {
            id: alt_id,
            container: alt_container,
            run_time_ticks: alt.duration_ms.map(|ms| ms.saturating_mul(10_000)),
            size: alt.size_bytes.or(probe.size_bytes),
            name: alt.name.clone().unwrap_or_else(|| item.title.clone()),
            media_streams: streams.clone(),
            media_attachments: media_attachments.clone(),
            bitrate: alt.bitrate_bps.or(probe.bitrate_bps),
            default_audio_stream_index,
            default_subtitle_stream_index,
            ..Default::default()
        });
    }

    // P4 — top-level resume offset. jellyfin-web reads this when it didn't keep
    // a local copy of UserData.PlaybackPositionTicks.
    let response = PlaybackInfoResponseDto {
        media_sources,
        play_session_id: play_session_id.clone(),
        start_position_ticks: resume_ticks,
    };
    // B70 — the native TV crashes PARSING this 200; log the exact body (gated
    // by PHAROS_LOG_ALL_REQUESTS) so the offending field can be diffed vs the
    // kotlin model, since the client sends no usable crash report.
    if std::env::var("PHAROS_LOG_ALL_REQUESTS").as_deref() == Ok("1") {
        let body = serde_json::to_string(&response).unwrap_or_default();
        tracing::info!(media.id = id, %body, "playbackinfo response");
    }
    Ok(crate::api::jellyfin::wire::json(&response))
}

async fn list_user_items_latest(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let user_path = path.into_inner();
    let bearer_id = user.0.id.0.simple().to_string();
    if user_path != bearer_id {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    latest_items(&state, &user, &q.into_inner()).await
}

/// `GET /Items/Latest` (B67) — the path-less Latest alias the Android/Google-TV
/// app uses for its home "Latest" rows (jellyfin-web uses
/// `/Users/{id}/Items/Latest`). pharos only had the path form, so `/Items/Latest`
/// fell through to `/Items/{id}` with id="latest" → 400 "invalid id" → the TV
/// crashed building the home screen. Derives the user from the bearer.
async fn items_latest(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    latest_items(&state, &user, &q.into_inner()).await
}

/// Shared core: the "Latest" items list (ParentId + IncludeItemTypes filtered),
/// returned as a RAW `BaseItemDto` array (not the ItemsResult envelope).
async fn latest_items(
    state: &AppState,
    user: &AuthUser,
    q: &ListQuery,
) -> Result<HttpResponse, actix_web::Error> {
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // Honour ParentId so home-page "Latest" rows match the library
    // the user clicked into. Library / series / season ids all
    // resolve via the shared restrict_to_parent helper.
    let scoped = restrict_to_parent(state, &all, q.parent_id.as_deref()).await;
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
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud).with_trickplay(
                &item.probe,
                &state.generated_trickplay_widths(item.id),
                state.trickplay_interval_ms,
            )
        })
        .collect();
    // /Items/Latest returns a raw array, not the ItemsResult envelope.
    Ok(crate::api::jellyfin::wire::json(&dtos))
}

fn filter_by_kinds(items: Vec<MediaItem>, include: Option<&str>) -> Vec<MediaItem> {
    let Some(s) = include else { return items };
    let wanted: Vec<MediaKind> = s.split(',').filter_map(MediaKind::from_wire).collect();
    if wanted.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|i| wanted.contains(&i.kind))
        .collect()
}

async fn user_views(
    state: web::Data<AppState>,
    _user: AuthUser,
    _path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    Ok(crate::api::jellyfin::wire::json(&synth_views_body(&state)))
}

#[derive(serde::Deserialize)]
struct UserViewsQuery {
    #[serde(default)]
    #[allow(dead_code)]
    user_id: Option<String>,
}

async fn user_views_query(
    state: web::Data<AppState>,
    _user: AuthUser,
    _q: CiQuery<UserViewsQuery>,
) -> Result<impl Responder, actix_web::Error> {
    Ok(crate::api::jellyfin::wire::json(&synth_views_body(&state)))
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

/// A `CollectionType` the kotlin SDK's enum accepts, else `null`. Jellyfin's
/// enum has no "mixed" — a mixed-content library carries a NULL CollectionType,
/// and emitting the string "mixed" crashes the strict client's enum decode (B68).
fn wire_collection_type(ct: &str) -> serde_json::Value {
    match ct {
        "mixed" | "" => serde_json::Value::Null,
        other => serde_json::Value::String(other.to_string()),
    }
}

fn library_views(state: &AppState) -> Vec<serde_json::Value> {
    // LIB-C1 — prefer the typed libraries reconciled from config (each
    // carries its own CollectionType). The wire_id is the same
    // library_id_for_root hash a plain root would yield, so client URLs
    // are stable whether or not a library is typed.
    let libraries = state.libraries();
    if !libraries.is_empty() {
        return libraries
            .iter()
            .map(|lib| {
                // B78/V38 — typed CollectionFolderDto (embeds the B68 UserData).
                serde_json::to_value(CollectionFolderDto {
                    id: lib.wire_id.clone(),
                    name: lib.name.clone(),
                    server_id: state.server_id.clone(),
                    kind: "CollectionFolder",
                    collection_type: wire_collection_type(lib.kind.collection_type())
                        .as_str()
                        .map(str::to_string),
                    media_type: "Unknown",
                    is_folder: true,
                    user_data: UserItemDataDto::folder(&lib.wire_id, false, 0, false),
                })
                .unwrap_or(serde_json::Value::Null)
            })
            .collect();
    }
    drop(libraries);
    if state.media_roots.is_empty() {
        return vec![all_media_placeholder(&state.server_id)];
    }
    // Fallback: synthesise one `mixed` library per configured root (the
    // legacy shape — used by tests that only call `with_media_roots`).
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
            serde_json::to_value(CollectionFolderDto {
                id: id.clone(),
                name,
                server_id: state.server_id.clone(),
                kind: "CollectionFolder",
                collection_type: None,
                media_type: "Unknown",
                is_folder: true,
                user_data: UserItemDataDto::folder(&id, false, 0, false),
            })
            .unwrap_or(serde_json::Value::Null)
        })
        .collect()
}

fn all_media_placeholder(server_id: &str) -> serde_json::Value {
    const ALL_MEDIA_ID: &str = "00000000000000000000000000000000";
    serde_json::to_value(CollectionFolderDto {
        id: ALL_MEDIA_ID.to_string(),
        name: "All Media".to_string(),
        server_id: server_id.to_string(),
        kind: "CollectionFolder",
        collection_type: None,
        media_type: "Unknown",
        is_folder: true,
        user_data: UserItemDataDto::folder(ALL_MEDIA_ID, false, 0, false),
    })
    .unwrap_or(serde_json::Value::Null)
}

/// 32-char hex id derived from the canonical root path — same input →
/// same id across restarts. Two roots only collide if their xxh3 hashes
/// collide (cryptographically unlikely for any realistic library
/// count).
pub fn library_id_for_root(path: &std::path::Path) -> String {
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
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": views,
        "TotalRecordCount": count,
        "StartIndex": 0,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Comma-separated Jellyfin `Fields` the client wants hydrated on each
    /// list row (e.g. `People,Studios,Tags`). T67 — the list builder omits
    /// these join-backed arrays by default (one query per item), populating
    /// them only when explicitly requested here.
    #[serde(default)]
    fields: Option<String>,
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
    /// LIB-C6 — comma- or pipe-separated tag names. Jellyfin's `Tags`
    /// filter is AND across the listed names: an item passes only when it
    /// carries EVERY requested tag (resolved through the `item_tags`
    /// join, not a probe column). Items without all the tags are dropped.
    #[serde(default)]
    tags: Option<String>,
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
    /// Casing is handled by the `CiQuery` extractor (it snake-cases every
    /// incoming key, so `Is4K` / `is4K` both bind `is_4k`).
    #[serde(default)]
    is_4k: Option<bool>,
    #[serde(default)]
    is_hd: Option<bool>,
    #[serde(default)]
    is_3d: Option<bool>,
    /// Explicit min/max bounds on the source video width. Lets a
    /// power user pick a 1440p cutoff that the canned chips miss.
    #[serde(default)]
    min_width: Option<u32>,
    #[serde(default)]
    max_width: Option<u32>,
    /// Comma-separated synth MusicArtist ids — albums whose ALBUM ARTIST
    /// is one of these. jellyfin-web's album detail "More From {artist}"
    /// section sends this with `IncludeItemTypes=MusicAlbum`.
    #[serde(default)]
    album_artist_ids: Option<String>,
    /// Comma-separated synth MusicArtist ids — albums where the artist
    /// APPEARS (a track's `artist` matches) but is NOT the album artist.
    /// jellyfin-web's artist detail "Appears On" section.
    #[serde(default)]
    contributing_artist_ids: Option<String>,
    /// Comma-separated synth MusicArtist ids — either role matches.
    #[serde(default)]
    artist_ids: Option<String>,
    /// Comma-separated wire ids dropped from the result (jellyfin-web
    /// excludes the page's own item from its "More From" rail).
    #[serde(default)]
    exclude_item_ids: Option<String>,
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

/// LIB-C5 — a `/Items?IncludeItemTypes=BoxSet` request (with no media
/// kinds alongside) is asking for the box-set list, which lives in the
/// `collections` entity table, NOT the media_items `store.list()` set
/// that powers the rest of `/Items`. Detect that shape so the handler
/// can short-circuit to the box sets (the "Collections" library view
/// jellyfin-web renders). Returns `None` when BoxSet wasn't requested
/// (or was requested alongside real media kinds, which we don't mix).
fn boxset_only_request(q: &ListQuery) -> bool {
    let Some(types) = q.include_item_types.as_deref() else {
        return false;
    };
    let mut saw_boxset = false;
    for t in types.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if t.eq_ignore_ascii_case("BoxSet") {
            saw_boxset = true;
        } else {
            // Any non-BoxSet type → this isn't a pure box-set request.
            return false;
        }
    }
    saw_boxset
}

/// Build the box-set `/Items` page (entity-backed) for a BoxSet-only
/// request, honouring StartIndex / Limit. Shared by `/Items` and
/// `/Users/{u}/Items`.
async fn list_boxsets_page(
    state: &AppState,
    q: &ListQuery,
) -> Result<serde_json::Value, actix_web::Error> {
    use pharos_core::CollectionStore;
    let rows = state
        .stores
        .collections_with_counts()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let all: Vec<serde_json::Value> = rows
        .iter()
        .map(|cc| collection_dto(state, &cc.collection, cc.item_count))
        .collect();
    let total = all.len() as u32;
    let start = q.start_index as usize;
    let page: Vec<serde_json::Value> = all.into_iter().skip(start).take(q.limit as usize).collect();
    Ok(serde_json::json!({
        "Items": page,
        "TotalRecordCount": total,
        "StartIndex": q.start_index,
    }))
}

/// T70 — an `?IncludeItemTypes=Playlist` request (with no media kinds
/// alongside) asks for the playlist list, which lives in the `playlists`
/// entity table, NOT the media_items set. Same shape as
/// [`boxset_only_request`].
fn playlist_only_request(q: &ListQuery) -> bool {
    let Some(types) = q.include_item_types.as_deref() else {
        return false;
    };
    let mut saw = false;
    for t in types.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if t.eq_ignore_ascii_case("Playlist") {
            saw = true;
        } else {
            return false;
        }
    }
    saw
}

/// Build the playlist `/Items` page (entity-backed) for a Playlist-only
/// request, scoped to the bearer (server-owned playlists always included),
/// honouring StartIndex / Limit. Shared by `/Items` and `/Users/{u}/Items`.
async fn list_playlists_page(
    state: &AppState,
    user_id: UserId,
    q: &ListQuery,
) -> Result<serde_json::Value, actix_web::Error> {
    use pharos_core::PlaylistStore;
    let owner = user_id.0.simple().to_string();
    let rows = state
        .stores
        .playlists_for_owner(Some(&owner))
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut all: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    for pl in &rows {
        let count = state
            .stores
            .playlist_entries(&pl.wire_id)
            .await
            .map(|e| e.len() as u32)
            .unwrap_or(0);
        all.push(crate::api::jellyfin::playlists::playlist_dto(
            state, pl, count,
        ));
    }
    let total = all.len() as u32;
    let start = q.start_index as usize;
    let page: Vec<serde_json::Value> = all.into_iter().skip(start).take(q.limit as usize).collect();
    Ok(serde_json::json!({
        "Items": page,
        "TotalRecordCount": total,
        "StartIndex": q.start_index,
    }))
}

/// A `/Items` request that asks ONLY for MusicAlbum rows. pharos stores no
/// album entity — albums are synthesised by grouping audio rows — so this
/// short-circuits to [`list_music_albums_page`] the same way BoxSet /
/// Playlist requests do. (`MediaKind::from_wire` has no MusicAlbum mapping,
/// so without the short-circuit the type filter silently DROPPED nothing and
/// the request returned every kind — jellyfin-web's "More From {artist}"
/// rail rendered TV shows + movies under a music album.)
fn music_album_only_request(q: &ListQuery) -> bool {
    let Some(types) = q.include_item_types.as_deref() else {
        return false;
    };
    let mut saw = false;
    for t in types.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if t.eq_ignore_ascii_case("MusicAlbum") {
            saw = true;
        } else {
            return false;
        }
    }
    saw
}

/// Parse a comma-separated synth-id list into a set for hash matching.
fn id_set(raw: Option<&str>) -> std::collections::HashSet<String> {
    raw.map(|r| {
        r.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    })
    .unwrap_or_default()
}

/// One synthesised album: grouped audio rows sharing an `album` tag.
struct AlbumAgg {
    name: String,
    album_artist: Option<String>,
    /// Earliest release-year tag across the album's tracks.
    year: Option<u32>,
    child_count: u32,
}

/// Group audio rows into albums. `keep` decides which TRACKS participate
/// (artist-role filters); an album aggregates only its kept tracks.
fn aggregate_albums(items: &[MediaItem], keep: impl Fn(&MediaItem) -> bool) -> Vec<AlbumAgg> {
    use std::collections::HashMap;
    let mut map: HashMap<String, AlbumAgg> = HashMap::new();
    for i in items {
        if i.kind != pharos_core::MediaKind::Audio || !keep(i) {
            continue;
        }
        let Some(name) = i.probe.album.as_deref().filter(|a| !a.is_empty()) else {
            continue;
        };
        let e = map.entry(name.to_string()).or_insert_with(|| AlbumAgg {
            name: name.to_string(),
            album_artist: None,
            year: None,
            child_count: 0,
        });
        e.child_count += 1;
        if e.album_artist.is_none() {
            e.album_artist = i
                .probe
                .album_artist
                .clone()
                .or_else(|| i.probe.artist.clone());
        }
        e.year = match (e.year, i.probe.year) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    map.into_values().collect()
}

/// Wire DTO for one synthesised album (matches the `/Albums` shape, plus
/// the year + child count the detail rails render).
fn synth_album_dto(state: &AppState, a: &AlbumAgg) -> serde_json::Value {
    use crate::api::jellyfin::dto::{album_id_for, artist_id_for};
    let album_id = album_id_for(&a.name);
    // Advertise a Primary image tag so clients actually request the cover.
    // The album image endpoint resolves a synth album id to a child track's
    // artwork (embedded cover or sidecar folder.jpg); without a tag in
    // ImageTags jellyfin-web / the native apps never fetch it, so album
    // cards + the album header rendered blank. The tag value is only a
    // cache-buster — a stable hash of the album id keeps it deterministic.
    let primary_tag = {
        use xxhash_rust::xxh3::xxh3_64;
        let h = xxh3_64(album_id.as_bytes()) & 0x7FFF_FFFF_FFFF_FFFF;
        format!("{h:016x}")
    };
    let mut image_tags = std::collections::BTreeMap::new();
    image_tags.insert("Primary".to_string(), primary_tag);
    // B78/V38 — typed SynthItemDto, serialized to keep the Value return.
    serde_json::to_value(SynthItemDto {
        child_count: Some(a.child_count),
        production_year: a.year.map(|y| y as i32),
        album_artist: a.album_artist.clone(),
        album_artists: a.album_artist.as_deref().map(|artist| {
            vec![NameGuidPairDto {
                name: artist.to_string(),
                id: artist_id_for(artist),
            }]
        }),
        image_tags: Some(image_tags),
        backdrop_image_tags: Some(Vec::new()),
        genres: Some(Vec::new()),
        tags: Some(Vec::new()),
        ..SynthItemDto::folder(
            album_id,
            a.name.clone(),
            state.server_id.clone(),
            "MusicAlbum",
        )
    })
    .unwrap_or(serde_json::Value::Null)
}

/// Sort synthesised albums per the request: the music rails ask for
/// `PremiereDate,ProductionYear,SortName` (year first); anything else
/// falls back to name. Descending flips the whole chain.
fn sort_album_aggs(aggs: &mut [AlbumAgg], sort_by: Option<&str>, descending: bool) {
    let by_year = sort_by
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .next()
        .is_some_and(|t| matches!(t, "PremiereDate" | "ProductionYear"));
    if by_year {
        aggs.sort_by(|a, b| {
            a.year
                .cmp(&b.year)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    } else {
        aggs.sort_by_key(|a| a.name.to_lowercase());
    }
    if descending {
        aggs.reverse();
    }
}

/// Music short-circuits for `/Items` + `/Users/{u}/Items`. Returns `None`
/// when the request isn't a music-shaped one (the caller continues down
/// the SQL list path).
///
/// Handles:
/// - `IncludeItemTypes=MusicAlbum` (+ AlbumArtistIds / ContributingArtistIds /
///   ArtistIds / ExcludeItemIds) — the album grid, the "More From {artist}"
///   rail (album detail) and "Appears On" (artist detail).
/// - `ParentId=<artist synth id>` with no explicit type filter — the artist
///   detail's discography: the artist's ALBUMS (jellyfin-web renders this
///   section as album cards; raw tracks left it effectively empty), plus any
///   loose tracks that have no album tag.
async fn maybe_list_music(
    state: &AppState,
    user_id: UserId,
    q: &ListQuery,
) -> Result<Option<HttpResponse>, actix_web::Error> {
    use crate::api::jellyfin::dto::{album_id_for, artist_id_for};

    let album_only = music_album_only_request(q);
    // Artist-children shape: ParentId resolves to an artist, no type filter.
    let artist_parent: Option<String> = if q.include_item_types.is_none() {
        match resolve_parent_filter(state, q.parent_id.as_deref()).await? {
            ParentResolution::Filter(pharos_core::ParentFilter::Artist { name }) => Some(name),
            _ => None,
        }
    } else {
        None
    };
    if !album_only && artist_parent.is_none() {
        return Ok(None);
    }

    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let descending = matches!(q.sort_order.as_deref(), Some("Descending"));

    if let Some(artist) = artist_parent {
        // Discography: albums whose album artist is this artist, falling
        // back to the track `artist` for untagged album_artist rows.
        let mut aggs = aggregate_albums(&all, |i| {
            i.probe
                .album_artist
                .as_deref()
                .or(i.probe.artist.as_deref())
                .is_some_and(|a| a.eq_ignore_ascii_case(&artist))
        });
        sort_album_aggs(&mut aggs, q.sort_by.as_deref(), descending);
        let mut items: Vec<serde_json::Value> =
            aggs.iter().map(|a| synth_album_dto(state, a)).collect();
        // Loose tracks (no album tag) still belong on the artist page.
        let loose: Vec<&MediaItem> = all
            .iter()
            .filter(|i| {
                i.kind == pharos_core::MediaKind::Audio
                    && i.probe.album.as_deref().unwrap_or("").is_empty()
                    && [i.probe.artist.as_deref(), i.probe.album_artist.as_deref()]
                        .into_iter()
                        .flatten()
                        .any(|a| a.eq_ignore_ascii_case(&artist))
            })
            .collect();
        if !loose.is_empty() {
            let ids: Vec<u64> = loose.iter().map(|i| i.id).collect();
            let ud = state
                .stores
                .user_data_bulk(user_id, &ids)
                .await
                .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
            for (n, t) in loose.iter().enumerate() {
                let d = ud.get(n).copied().unwrap_or_default();
                items.push(
                    serde_json::to_value(BaseItemDto::from_domain_with_user_data(
                        t,
                        &state.server_id,
                        d,
                    ))
                    .unwrap_or_default(),
                );
            }
        }
        let total = items.len() as u32;
        let start = q.start_index as usize;
        let page: Vec<serde_json::Value> = items
            .into_iter()
            .skip(start)
            .take(q.limit as usize)
            .collect();
        return Ok(Some(crate::api::jellyfin::wire::json(&serde_json::json!({
            "Items": page,
            "TotalRecordCount": total,
            "StartIndex": q.start_index,
        }))));
    }

    // Album-only listing. Artist-role filters (synth-id sets — recovered by
    // hashing each track's artist names, same one-way scheme as ParentId).
    let album_artist_ids = id_set(q.album_artist_ids.as_deref());
    let contributing_ids = id_set(q.contributing_artist_ids.as_deref());
    let any_artist_ids = id_set(q.artist_ids.as_deref());
    let exclude_ids = id_set(q.exclude_item_ids.as_deref());

    let keep = |i: &MediaItem| -> bool {
        let aa_hash = i
            .probe
            .album_artist
            .as_deref()
            .map(|a| artist_id_for(a).to_ascii_lowercase());
        let ar_hash = i
            .probe
            .artist
            .as_deref()
            .map(|a| artist_id_for(a).to_ascii_lowercase());
        if !album_artist_ids.is_empty()
            && !aa_hash
                .as_deref()
                .is_some_and(|h| album_artist_ids.contains(h))
        {
            return false;
        }
        if !contributing_ids.is_empty() {
            // Appears On: performs on the track but is not the album artist.
            let contributes = ar_hash
                .as_deref()
                .is_some_and(|h| contributing_ids.contains(h));
            let owns_album = aa_hash
                .as_deref()
                .is_some_and(|h| contributing_ids.contains(h));
            if !contributes || owns_album {
                return false;
            }
        }
        if !any_artist_ids.is_empty()
            && !aa_hash
                .as_deref()
                .is_some_and(|h| any_artist_ids.contains(h))
            && !ar_hash
                .as_deref()
                .is_some_and(|h| any_artist_ids.contains(h))
        {
            return false;
        }
        true
    };
    let mut aggs = aggregate_albums(&all, keep);
    if !exclude_ids.is_empty() {
        aggs.retain(|a| !exclude_ids.contains(&album_id_for(&a.name).to_ascii_lowercase()));
    }
    sort_album_aggs(&mut aggs, q.sort_by.as_deref(), descending);
    let total = aggs.len() as u32;
    let start = q.start_index as usize;
    let page: Vec<serde_json::Value> = aggs
        .iter()
        .skip(start)
        .take(q.limit as usize)
        .map(|a| synth_album_dto(state, a))
        .collect();
    Ok(Some(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": page,
        "TotalRecordCount": total,
        "StartIndex": q.start_index,
    }))))
}

async fn list_items(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    // LIB-C5 — BoxSet-only listing comes from the collections entity table.
    if boxset_only_request(&q) {
        return Ok(crate::api::jellyfin::wire::json(
            &list_boxsets_page(&state, &q).await?,
        ));
    }
    // T70 — Playlist-only listing comes from the playlists entity table.
    if playlist_only_request(&q) {
        return Ok(crate::api::jellyfin::wire::json(
            &list_playlists_page(&state, user.0.id, &q).await?,
        ));
    }
    if let Some(resp) = maybe_list_music(&state, user.0.id, &q).await? {
        return Ok(resp);
    }
    if let Some(resp) = maybe_list_virtual_shows(&state, user.0.id, &user.0.policy, &q).await? {
        return Ok(resp);
    }
    let dto = run_items_list(&state, user.0.id, &user.0.policy, &q).await?;
    Ok(crate::api::jellyfin::wire::json(&dto))
}

async fn list_user_items(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: CiQuery<ListQuery>,
) -> Result<impl Responder, actix_web::Error> {
    // V9 spirit: the path user must match the bearer. Reject mismatched.
    let user_path = path.into_inner();
    let bearer_id = user.0.id.0.simple().to_string();
    if user_path != bearer_id {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    // LIB-C5 — BoxSet-only listing comes from the collections entity table.
    if boxset_only_request(&q) {
        return Ok(crate::api::jellyfin::wire::json(
            &list_boxsets_page(&state, &q).await?,
        ));
    }
    // T70 — Playlist-only listing comes from the playlists entity table.
    if playlist_only_request(&q) {
        return Ok(crate::api::jellyfin::wire::json(
            &list_playlists_page(&state, user.0.id, &q).await?,
        ));
    }
    if let Some(resp) = maybe_list_music(&state, user.0.id, &q).await? {
        return Ok(resp);
    }
    // Virtual Series/Season listing — pharos stores no Series/Season row, so a
    // `IncludeItemTypes=Series` (or `Season`) browse collapses the scoped
    // episodes into synthetic folder tiles. jellyfin-web's TV-library grid and
    // its "Shows" view both issue this; without the collapse they'd render one
    // tile per episode instead of one per show, so a series like Code Geass
    // never appears as a browsable entry.
    if let Some(resp) = maybe_list_virtual_shows(&state, user.0.id, &user.0.policy, &q).await? {
        return Ok(resp);
    }
    let dto = run_items_list(&state, user.0.id, &user.0.policy, &q).await?;
    Ok(crate::api::jellyfin::wire::json(&dto))
}

/// LIB-B2 — the single `/Items` + `/Users/{u}/Items` list path, routed
/// ENTIRELY through `MediaStore::query`. The legacy whole-library
/// `list()` + `restrict_to_parent` + `filter_and_sort` + in-memory
/// pagination is gone: the parent pivot, kind/search/entity/user-data
/// scope, the residual chip filters, the sort chain, and the page all
/// resolve in one parameterised SQL statement. `SortBy=Random` is the lone
/// exception — a seeded Fisher–Yates shuffle can't be a stable SQL order, so
/// for that case the filtered set is fetched unpaged via `query()` (no
/// whole-library load — the SQL filters still apply) then shuffled + paged in
/// memory with the same deterministic permutation the legacy path used.
async fn run_items_list(
    state: &AppState,
    user_id: UserId,
    policy: &pharos_core::UserPolicy,
    q: &ListQuery,
) -> Result<ItemsResultDto, actix_web::Error> {
    let parent = resolve_parent_filter(state, q.parent_id.as_deref()).await?;
    // An unknown ParentId resolves to nothing — render an empty library
    // without touching the store (mirrors the legacy `Vec::new()` tail).
    if matches!(parent, ParentResolution::Empty) {
        return Ok(ItemsResultDto {
            items: Vec::new(),
            total_record_count: 0,
            start_index: q.start_index,
        });
    }

    let is_random = sort_primary(q.sort_by.as_deref()) == SortPrimary::Random;
    let mut mq = build_media_query(user_id, q, &parent, is_random);
    // T68 — narrow the query to what the user's policy permits (library
    // allow-list + parental rating). Applied to the MediaQuery so page totals
    // and offsets reflect the restricted set.
    apply_policy_scope(&mut mq, policy, &state.parental_ratings);

    if is_random {
        // Byte-identical to the legacy in-memory Random path, where the
        // shuffle ran in `filter_and_sort` (over the kind/search/residual-
        // filtered set) BEFORE the tag + user-data filters and pagination.
        // So: fetch that set (NO tag/user-data clauses in SQL), shuffle, then
        // apply the tag + user-data filters over the shuffled order, then
        // page. The SQL still does all the heavy filtering — no whole-library
        // load.
        let (mut items, _total) = state
            .stores
            .query(&mq)
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        let seed = effective_sort_seed(q, user_id);
        shuffle_in_place(&mut items, seed);
        let tagged = apply_tags_filter(state, items, q).await?;
        let after_ud = apply_userdata_filter(state, user_id, tagged, q).await?;
        let total = after_ud.len() as u32;
        let start = q.start_index as usize;
        let end = (start + q.limit as usize).min(after_ud.len());
        let page: Vec<MediaItem> = if start >= after_ud.len() {
            Vec::new()
        } else {
            after_ud[start..end].to_vec()
        };
        return build_items_page_with_fields(
            state,
            user_id,
            &page,
            total,
            q.start_index,
            q.fields.as_deref(),
        )
        .await;
    }

    let (items, total) = state
        .stores
        .query(&mq)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let total = u32::try_from(total).unwrap_or(u32::MAX);
    build_items_page_with_fields(
        state,
        user_id,
        &items,
        total,
        q.start_index,
        q.fields.as_deref(),
    )
    .await
}

/// How a `?ParentId=` resolved against the store / config.
enum ParentResolution {
    /// No parent restriction (absent / zero-guid placeholder).
    All,
    /// A typed [`pharos_core::ParentFilter`] for the SQL query.
    Filter(pharos_core::ParentFilter),
    /// A configured media root WITHOUT a typed-library entity row — scope by
    /// component-boundary path prefix (test-only `with_media_roots` path).
    PathPrefix(String),
    /// LIB-C4 legacy fallback: the genre synth id matched no `item_genres`
    /// row (un-backfilled), but a `probe.genre` token hashes to it. Scope by
    /// the legacy probe-column genre name.
    GenreProbe(String),
    /// The id matched no known entity / synth key — render an empty library.
    Empty,
}

/// LIB-B2 — resolve a `?ParentId=` to a [`ParentResolution`] WITHOUT loading
/// the item set. Mirrors the branch order of the legacy `restrict_to_parent`:
/// library → series → season → artist → album → genre → person → studio →
/// collection → tag, falling to `Empty` for an unknown id. The synth
/// series/season/artist/album ids are one-way hashes, so the matching
/// folder/name is recovered by hashing the store's distinct candidate keys.
async fn resolve_parent_filter(
    state: &AppState,
    parent_id: Option<&str>,
) -> Result<ParentResolution, actix_web::Error> {
    use crate::api::jellyfin::dto::{
        album_id_for, artist_id_for, season_id_for_key, series_id_for_key,
    };
    use pharos_core::{
        CollectionStore, GenreStore, LibraryStore, ParentFilter, PersonStore, StudioStore, TagStore,
    };
    let Some(pid) = parent_id else {
        return Ok(ParentResolution::All);
    };
    if pid.is_empty() || pid == "00000000000000000000000000000000" {
        return Ok(ParentResolution::All);
    }
    let ise = |e: pharos_core::DomainError| error::ErrorInternalServerError(e.to_string());

    // 1) Typed library entity by wire_id. Clone the (wire_id, root) out of the
    // read guard first so it isn't held across the `await` below.
    let matched_lib = state
        .libraries()
        .iter()
        .find(|l| l.wire_id == pid)
        .map(|l| (l.wire_id.clone(), l.root_path.clone()));
    if let Some((wire_id, root_path)) = matched_lib {
        if let Ok(ids) = state.stores.item_ids_for_library(&wire_id).await {
            if !ids.is_empty() {
                return Ok(ParentResolution::Filter(ParentFilter::Library { wire_id }));
            }
        }
        // No backfilled rows yet → path-prefix fallback against the root.
        return Ok(ParentResolution::PathPrefix(root_path));
    }
    // Legacy media-root (no typed-library entity) → path-prefix scope.
    if let Some(root) = state
        .media_roots
        .iter()
        .find(|r| library_id_for_root(r) == pid)
    {
        return Ok(ParentResolution::PathPrefix(
            root.to_string_lossy().into_owned(),
        ));
    }

    // 2) Series synth id → recover (folder, name) by hashing distinct keys.
    let series_keys = state
        .stores
        .distinct_series_keys()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    for (folder, name) in &series_keys {
        if series_id_for_key(folder.as_deref(), name) == pid {
            return Ok(ParentResolution::Filter(ParentFilter::Series {
                folder: folder.clone(),
                name: name.clone(),
            }));
        }
    }
    // 3) Season synth id → recover (folder, name, season).
    let season_keys = state
        .stores
        .distinct_season_keys()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    for (folder, name, season) in &season_keys {
        if let Ok(season_u32) = u32::try_from(*season) {
            if season_id_for_key(folder.as_deref(), name, season_u32) == pid {
                return Ok(ParentResolution::Filter(ParentFilter::Season {
                    folder: folder.clone(),
                    name: name.clone(),
                    season: season_u32,
                }));
            }
        }
    }
    // 4) Artist synth id → recover the name.
    let artists = state
        .stores
        .distinct_artist_names()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    for name in &artists {
        if artist_id_for(name) == pid {
            return Ok(ParentResolution::Filter(ParentFilter::Artist {
                name: name.clone(),
            }));
        }
    }
    // 5) Album synth id → recover the name.
    let albums = state
        .stores
        .distinct_album_names()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    for name in &albums {
        if album_id_for(name) == pid {
            return Ok(ParentResolution::Filter(ParentFilter::Album {
                name: name.clone(),
            }));
        }
    }

    // 6-10) Entity wire-id pivots — the wire id IS the filter param, but we
    // probe the join first so an id matching no row falls through to Empty
    // (mirrors the legacy "non-empty ids" guard, except collections which
    // resolve even when empty so a fresh box set browses cleanly).
    if !state
        .stores
        .item_ids_for_genre(pid)
        .await
        .map_err(ise)?
        .is_empty()
    {
        return Ok(ParentResolution::Filter(ParentFilter::Genre {
            wire_id: pid.to_string(),
        }));
    }
    // LIB-C4 legacy fallback: the entity join is empty (rows scanned before
    // C4 + not backfilled), so resolve the genre NAME from the probe column
    // by hashing each distinct `probe.genre` token and scope by it.
    {
        let fields = state
            .stores
            .distinct_genre_fields()
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        for raw in &fields {
            for token in pharos_core::split_genre_field(raw) {
                if pharos_core::genre_wire_id(&token) == pid {
                    return Ok(ParentResolution::GenreProbe(token));
                }
            }
        }
    }
    if !state
        .stores
        .item_ids_for_person(pid)
        .await
        .map_err(ise)?
        .is_empty()
    {
        return Ok(ParentResolution::Filter(ParentFilter::Person {
            wire_id: pid.to_string(),
        }));
    }
    if !state
        .stores
        .item_ids_for_studio(pid)
        .await
        .map_err(ise)?
        .is_empty()
    {
        return Ok(ParentResolution::Filter(ParentFilter::Studio {
            wire_id: pid.to_string(),
        }));
    }
    // Collection resolves on existence (even when empty) so a freshly-created
    // empty box set browses to an empty list rather than falling to Empty.
    if state
        .stores
        .collection_by_wire_id(pid)
        .await
        .map_err(ise)?
        .is_some()
    {
        return Ok(ParentResolution::Filter(ParentFilter::Collection {
            wire_id: pid.to_string(),
        }));
    }
    if !state
        .stores
        .item_ids_for_tag(pid)
        .await
        .map_err(ise)?
        .is_empty()
    {
        return Ok(ParentResolution::Filter(ParentFilter::Tag {
            wire_id: pid.to_string(),
        }));
    }

    Ok(ParentResolution::Empty)
}

/// The sort key family the legacy `filter_and_sort` recognised. Anything not
/// listed here falls to `SortName` (parity with the in-memory `_` arm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortPrimary {
    None,
    Random,
    DateCreated,
    Runtime,
    AlbumArtist,
    Album,
    /// Album track order: disc then track then name. jellyfin-web's album
    /// detail sends `SortBy=ParentIndexNumber,IndexNumber,SortName`.
    TrackOrder,
    Name,
}

/// Map the raw `SortBy` to its [`SortPrimary`], replicating the legacy
/// "first non-empty comma token, else SortName" rule and the exact set of
/// tokens the in-memory `filter_and_sort` handled.
fn sort_primary(sort_by: Option<&str>) -> SortPrimary {
    let raw = sort_by.unwrap_or("SortName");
    let primary = raw
        .split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("SortName");
    match primary {
        "None" | "Default" => SortPrimary::None,
        "Random" => SortPrimary::Random,
        "DateCreated" | "DateAdded" => SortPrimary::DateCreated,
        "RuntimeTicks" | "Runtime" => SortPrimary::Runtime,
        "AlbumArtist" => SortPrimary::AlbumArtist,
        "Album" => SortPrimary::Album,
        "ParentIndexNumber" | "IndexNumber" => SortPrimary::TrackOrder,
        _ => SortPrimary::Name,
    }
}

/// T68 — narrow a built [`MediaQuery`] to what the requesting user's policy
/// permits: the library allow-list (`EnabledFolders`) and the parental-rating
/// ceiling (`MaxParentalRating`). Applied to the query (not a post-filter) so
/// `TotalRecordCount` and paging stay honest. A no-op for an unrestricted
/// (default) policy.
fn apply_policy_scope(
    mq: &mut pharos_core::MediaQuery,
    policy: &pharos_core::UserPolicy,
    ratings: &crate::parental::ParentalRatingMap,
) {
    if !policy.enable_all_folders {
        mq.allowed_library_wire_ids = if policy.enabled_folders.is_empty() {
            // Restricted to *no* library: a sentinel wire id that matches no
            // row (an empty allow-list means "unrestricted" to the store).
            vec!["\u{0}__none__".to_string()]
        } else {
            policy.enabled_folders.clone()
        };
    }
    if let Some(max) = policy.max_parental_rating {
        mq.parental = Some(pharos_core::ParentalScope {
            allowed_ratings_lc: ratings.allowed_ratings_lc(max),
            // Jellyfin's BlockUnratedItems is a per-type list; pharos applies it
            // coarsely — any entry blocks unrated items for this user.
            block_unrated: !policy.block_unrated_items.is_empty(),
        });
    }
}

/// Translate the (already parent-resolved) `ListQuery` into a [`MediaQuery`].
/// `force_unpaged` (the Random path) drops LIMIT/OFFSET so the caller can
/// shuffle + page in memory. The sort chain reproduces the legacy
/// stable-sort-then-conditional-reverse tiebreak semantics exactly (id is a
/// unique final tiebreak, so the builder's appended `id ASC` is a harmless
/// no-op after an explicit `Id DESC`).
fn build_media_query(
    user_id: UserId,
    q: &ListQuery,
    parent: &ParentResolution,
    force_unpaged: bool,
) -> pharos_core::MediaQuery {
    use pharos_core::{MediaFilters, MediaQuery, SortDir, SortKey};

    let mut mq = MediaQuery::default();

    // Parent pivot. PathPrefix folds into the residual `MediaFilters` built
    // below (see `path_prefix` there) — set the parent typed filter here.
    if let ParentResolution::Filter(pf) = parent {
        mq.parent = Some(pf.clone());
    }

    // IncludeItemTypes → kinds.
    if let Some(types) = q.include_item_types.as_deref() {
        let wanted: Vec<pharos_core::MediaKind> =
            types.split(',').filter_map(MediaKind::from_wire).collect();
        if !wanted.is_empty() {
            mq.kinds = wanted;
        }
    }

    // SearchTerm — case-insensitive substring on title.
    if let Some(term) = q.search_term.as_deref() {
        if !term.trim().is_empty() {
            mq.search_term = Some(term.to_string());
        }
    }

    // ?Tags= + user-data: applied in SQL on the normal (paged) path. On the
    // Random path (`force_unpaged`) they are LEFT OUT here and applied in
    // memory AFTER the shuffle, matching the legacy ordering (shuffle ran in
    // `filter_and_sort` ahead of the tag + user-data filters).
    if !force_unpaged {
        // ?Tags= — entity-join AND across tags.
        if let Some(raw) = q.tags.as_deref() {
            let ids: Vec<String> = raw
                .split(['|', ','])
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(pharos_core::tag_wire_id)
                .collect();
            mq.tag_wire_ids = ids;
        }

        // User-data predicates (Filters + IsFavorite/IsPlayed shortcuts).
        let mut udf = match q.filters.as_deref() {
            Some(raw) => UserDataFilter::parse(raw),
            None => UserDataFilter::default(),
        };
        if let Some(v) = q.is_favorite {
            udf.is_favorite = Some(v);
        }
        if let Some(v) = q.is_played {
            udf.is_played = Some(v);
        }
        if udf.is_active() {
            mq.user_data = pharos_core::UserDataQuery {
                user: Some(user_id),
                is_favorite: udf.is_favorite,
                is_played: udf.is_played,
                is_resumable: udf.is_resumable,
            };
        }
    }

    // Residual chip filters.
    let mut f = MediaFilters::default();
    // Media-root ParentId without a typed-library entity → path-prefix scope.
    if let ParentResolution::PathPrefix(p) = parent {
        f.path_prefix = Some(p.clone());
    }
    // LIB-C4 legacy ParentId=genre fallback → token-membership in the
    // probe.genre column (matches multi-genre strings like "Action, Sci-Fi").
    if let ParentResolution::GenreProbe(token) = parent {
        f.genre_probe_token = Some(token.clone());
    }
    if let Some(types) = q.exclude_item_types.as_deref() {
        f.exclude_kinds = types.split(',').filter_map(MediaKind::from_wire).collect();
    }
    if let Some(raw) = q.media_types.as_deref() {
        let want_audio = raw
            .split(',')
            .any(|s| s.trim().eq_ignore_ascii_case("Audio"));
        let want_video = raw
            .split(',')
            .any(|s| s.trim().eq_ignore_ascii_case("Video"));
        let mut kinds: Vec<pharos_core::MediaKind> = Vec::new();
        if want_audio {
            kinds.push(pharos_core::MediaKind::Audio);
        }
        if want_video {
            kinds.push(pharos_core::MediaKind::Movie);
            kinds.push(pharos_core::MediaKind::Episode);
        }
        f.media_type_kinds = kinds;
    }
    f.has_subtitles = q.has_subtitles;
    f.is_4k = q.is_4k;
    f.is_hd = q.is_hd;
    f.is_3d = q.is_3d;
    f.min_width = q.min_width;
    f.max_width = q.max_width;
    f.min_index_number = q.min_index_number;
    f.max_index_number = q.max_index_number;
    f.name_starts_with = q
        .name_starts_with
        .clone()
        .or_else(|| q.name_starts_with_or_greater.clone());
    f.name_less_than = q.name_less_than.clone();
    if let Some(raw) = q.ids.as_deref() {
        f.ids_present = true;
        f.ids = raw
            .split(',')
            .map(str::trim)
            // ≤36 admits the dashed-GUID form; the old ≤20 guard (decimal
            // u64) silently dropped EVERY canonical 32-hex id, so any
            // /Items?ids= refetch returned zero rows — which crashed
            // jellyfin-web's SyncPlay queue load ("Cannot read properties
            // of undefined (reading 'Type')") and killed group playback
            // (B22).
            .filter(|s| s.len() <= 36)
            .filter_map(pharos_jellyfin_api::dto::parse_item_id)
            .collect();
    }
    if let Some(raw) = q.genres.as_deref() {
        f.genre_probe_names = raw
            .split(['|', ','])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    mq.filters = f;

    // Sort chain — replicate the in-memory stable-sort + conditional reverse.
    let primary = sort_primary(q.sort_by.as_deref());
    let descending = matches!(q.sort_order.as_deref(), Some("Descending"));
    let collection_parent = matches!(
        parent,
        ParentResolution::Filter(pharos_core::ParentFilter::Collection { .. })
    );
    // A collection parent with no explicit SortBy keeps the curated order
    // (the legacy `preserve_collection_order` set SortBy=None). The query
    // builder already prepends the curated sort_order for a Collection pivot,
    // so we leave `sort` empty in that case.
    let effective_primary = if collection_parent && q.sort_by.is_none() {
        SortPrimary::None
    } else {
        primary
    };
    mq.sort = match effective_primary {
        SortPrimary::None | SortPrimary::Random => Vec::new(),
        SortPrimary::Name => {
            if descending {
                vec![(SortKey::Name, SortDir::Desc), (SortKey::Id, SortDir::Desc)]
            } else {
                vec![(SortKey::Name, SortDir::Asc)]
            }
        }
        SortPrimary::DateCreated => {
            // Legacy baseline (ascending param) = created_at DESC, id DESC
            // (newest-first); the Descending param reverses to ASC/ASC.
            if descending {
                vec![(SortKey::DateCreated, SortDir::Asc)]
            } else {
                vec![
                    (SortKey::DateCreated, SortDir::Desc),
                    (SortKey::Id, SortDir::Desc),
                ]
            }
        }
        SortPrimary::Runtime => {
            if descending {
                vec![
                    (SortKey::Runtime, SortDir::Desc),
                    (SortKey::Id, SortDir::Desc),
                ]
            } else {
                vec![(SortKey::Runtime, SortDir::Asc)]
            }
        }
        SortPrimary::AlbumArtist => {
            if descending {
                vec![
                    (SortKey::AlbumArtist, SortDir::Desc),
                    (SortKey::Name, SortDir::Desc),
                    (SortKey::Id, SortDir::Desc),
                ]
            } else {
                vec![
                    (SortKey::AlbumArtist, SortDir::Asc),
                    (SortKey::Name, SortDir::Asc),
                ]
            }
        }
        SortPrimary::Album => {
            if descending {
                vec![
                    (SortKey::Album, SortDir::Desc),
                    (SortKey::Name, SortDir::Desc),
                    (SortKey::Id, SortDir::Desc),
                ]
            } else {
                vec![
                    (SortKey::Album, SortDir::Asc),
                    (SortKey::Name, SortDir::Asc),
                ]
            }
        }
        SortPrimary::TrackOrder => {
            // Disc, then track, then name — the album track listing.
            // Untagged rows (NULL disc/track) sort AFTER tagged ones in
            // SQLite ASC? No — NULLs sort FIRST in SQLite ASC, LAST in
            // Postgres ASC by default. The builder's expression is used
            // verbatim, so accept the minor backend divergence for
            // untagged rows: tagged albums order identically on both.
            if descending {
                vec![
                    (SortKey::DiscNumber, SortDir::Desc),
                    (SortKey::TrackNumber, SortDir::Desc),
                    (SortKey::Name, SortDir::Desc),
                    (SortKey::Id, SortDir::Desc),
                ]
            } else {
                vec![
                    (SortKey::DiscNumber, SortDir::Asc),
                    (SortKey::TrackNumber, SortDir::Asc),
                    (SortKey::Name, SortDir::Asc),
                ]
            }
        }
    };

    // Pagination — SQL paginates unless the Random path needs the full set.
    if force_unpaged {
        mq.start_index = 0;
        mq.limit = None;
    } else {
        mq.start_index = u64::from(q.start_index);
        mq.limit = Some(q.limit);
    }
    mq
}

/// Build the `ItemsResultDto` for a resolved page of items: bulk user-data
/// lookup, per-item DTO with trickplay, then parent-id fill. Mirrors the
/// legacy `paginate` DTO assembly exactly (same field set, same order).
pub(crate) async fn build_items_page(
    state: &AppState,
    user_id: UserId,
    page: &[MediaItem],
    total: u32,
    start_index: u32,
) -> Result<ItemsResultDto, actix_web::Error> {
    build_items_page_with_fields(state, user_id, page, total, start_index, None).await
}

/// T67 — `build_items_page`, plus optional per-row hydration of the
/// join-backed `People` / `Studios` / `Tags` arrays when the client's
/// Jellyfin `Fields` requests them. Those are omitted by default (the
/// grid doesn't need cast/crew) because each is one indexed query per row;
/// a `Fields=People` request (the cast-on-cards view) opts into the cost.
pub(crate) async fn build_items_page_with_fields(
    state: &AppState,
    user_id: UserId,
    page: &[MediaItem],
    total: u32,
    start_index: u32,
    fields: Option<&str>,
) -> Result<ItemsResultDto, actix_web::Error> {
    use pharos_core::{PersonStore, StudioStore, TagStore};
    let want_people = fields_requests(fields, "People");
    let want_studios = fields_requests(fields, "Studios");
    let want_tags = fields_requests(fields, "Tags");
    let ids: Vec<u64> = page.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user_id, &ids)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut items: Vec<BaseItemDto> = Vec::with_capacity(page.len());
    for (i, item) in page.iter().enumerate() {
        let ud = user_data.get(i).copied().unwrap_or_default();
        let mut dto = BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud)
            .with_trickplay(
                &item.probe,
                &state.generated_trickplay_widths(item.id),
                state.trickplay_interval_ms,
            );
        // Best-effort per-row enrichment: a store error just leaves the
        // array empty, never a 500 (mirrors the detail path).
        if want_people {
            if let Ok(p) = state.stores.people_for_item(item.id).await {
                dto = dto.with_people(&p);
            }
        }
        if want_studios {
            if let Ok(s) = state.stores.studios_for_item(item.id).await {
                dto = dto.with_studios(&s);
            }
        }
        if want_tags {
            if let Ok(t) = state.stores.tags_for_item(item.id).await {
                dto = dto.with_tags(&t);
            }
        }
        items.push(dto);
    }
    let refs: Vec<&MediaItem> = page.iter().collect();
    fill_parent_ids(state, &mut items, &refs);
    Ok(ItemsResultDto {
        items,
        total_record_count: total,
        start_index,
    })
}

/// Whether the comma-separated Jellyfin `Fields` value requests `name`
/// (case-insensitive). `ItemCounts`, `PrimaryImageAspectRatio`, … are
/// ignored here — only the join-backed arrays this builder can hydrate.
fn fields_requests(fields: Option<&str>, name: &str) -> bool {
    fields.is_some_and(|f| {
        f.split(',')
            .map(str::trim)
            .any(|t| t.eq_ignore_ascii_case(name))
    })
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

/// LIB-C6 — apply the `?Tags=a,b` filter (AND across the listed tags): an
/// item passes only when it carries EVERY requested tag. Resolved through
/// the `item_tags` join (tags have no probe column), so the filter lives
/// here in the async path rather than in the sync `filter_and_sort`. A
/// no-op when `Tags` is absent/empty — no extra IO. A tag name that
/// matches no row contributes an empty id set, so the AND intersection
/// collapses to nothing (the correct "no item has this tag" result).
async fn apply_tags_filter(
    state: &AppState,
    items: Vec<MediaItem>,
    q: &ListQuery,
) -> Result<Vec<MediaItem>, actix_web::Error> {
    use pharos_core::TagStore;
    let Some(raw) = q.tags.as_deref() else {
        return Ok(items);
    };
    let wanted: Vec<String> = raw
        .split(['|', ','])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    if wanted.is_empty() {
        return Ok(items);
    }
    // Intersect the tagged-item sets across every requested tag (AND).
    let mut keep: Option<std::collections::HashSet<pharos_core::MediaId>> = None;
    for name in &wanted {
        let wire_id = pharos_core::tag_wire_id(name);
        let ids = state
            .stores
            .item_ids_for_tag(&wire_id)
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        let set: std::collections::HashSet<pharos_core::MediaId> = ids.into_iter().collect();
        keep = Some(match keep {
            None => set,
            Some(acc) => acc.intersection(&set).copied().collect(),
        });
    }
    let keep = keep.unwrap_or_default();
    Ok(items.into_iter().filter(|i| keep.contains(&i.id)).collect())
}

/// Drop items that don't live under the configured root / series /
/// season mapped to `parent_id`. Unknown `parent_id` → empty list.
/// The All-Media placeholder + `None` pass everything through.
async fn restrict_to_parent(
    state: &AppState,
    items: &[MediaItem],
    parent_id: Option<&str>,
) -> Vec<MediaItem> {
    use crate::api::jellyfin::dto::{
        album_id_for, artist_id_for, genre_id_for, season_id_for_key, series_id_for_key,
    };
    use pharos_core::{
        CollectionStore, GenreStore, LibraryStore, PersonStore, StudioStore, TagStore,
    };
    let Some(pid) = parent_id else {
        return items.to_vec();
    };
    if pid.is_empty() || pid == "00000000000000000000000000000000" {
        return items.to_vec();
    }
    // 1) Library / root match. LIB-C1 — prefer the typed `libraries` table:
    //    resolve the ParentId by wire_id to the set of item ids assigned
    //    that library (backfilled by path-prefix at boot, kept current by
    //    the scanner). Fall back to the legacy path-prefix scan over
    //    `media_roots` for libraries not yet backfilled (or test states
    //    wired only with `with_media_roots`).
    let matched_lib = state
        .libraries()
        .iter()
        .find(|l| l.wire_id == pid)
        .map(|l| (l.wire_id.clone(), l.root_path.clone()));
    if let Some((wire_id, root_path)) = matched_lib {
        if let Ok(ids) = state.stores.item_ids_for_library(&wire_id).await {
            if !ids.is_empty() {
                let want: std::collections::HashSet<u64> = ids.into_iter().collect();
                return items
                    .iter()
                    .filter(|i| want.contains(&i.id))
                    .cloned()
                    .collect();
            }
        }
        // No backfilled rows yet → fall back to the path-prefix scan
        // against this library's root so the view still resolves.
        let root = std::path::PathBuf::from(&root_path);
        return items
            .iter()
            .filter(|i| i.path.starts_with(&root))
            .cloned()
            .collect();
    }
    if let Some(root) = state
        .media_roots
        .iter()
        .find(|r| library_id_for_root(r) == pid)
    {
        return items
            .iter()
            .filter(|i| i.path.starts_with(root))
            .cloned()
            .collect();
    }
    // 2) Series id → every episode whose folder-keyed series id matches
    //    pid. LIB-C11: resolves via the show FOLDER (falling back to the
    //    bare name for legacy rows), so a same-name show in another folder
    //    isn't pulled in under the wrong series.
    if items.iter().any(|i| {
        i.series
            .as_ref()
            .is_some_and(|s| series_id_for_key(s.series_folder.as_deref(), &s.series_name) == pid)
    }) {
        return items
            .iter()
            .filter(|i| {
                i.series.as_ref().is_some_and(|s| {
                    series_id_for_key(s.series_folder.as_deref(), &s.series_name) == pid
                })
            })
            .cloned()
            .collect();
    }
    // 3) Season id → every episode in that (folder, season) pair.
    if items.iter().any(|i| {
        i.series.as_ref().is_some_and(|s| {
            s.season_number.is_some_and(|n| {
                season_id_for_key(s.series_folder.as_deref(), &s.series_name, n) == pid
            })
        })
    }) {
        return items
            .iter()
            .filter(|i| {
                i.series.as_ref().is_some_and(|s| {
                    s.season_number.is_some_and(|n| {
                        season_id_for_key(s.series_folder.as_deref(), &s.series_name, n) == pid
                    })
                })
            })
            .cloned()
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
            .iter()
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
            .cloned()
            .collect();
    }
    // 5) Album id → every track whose album hashes to pid.
    if items.iter().any(|i| {
        i.probe
            .album
            .as_deref()
            .is_some_and(|a| album_id_for(a) == pid)
    }) {
        return items
            .iter()
            .filter(|i| {
                i.probe
                    .album
                    .as_deref()
                    .is_some_and(|a| album_id_for(a) == pid)
            })
            .cloned()
            .collect();
    }
    // 6) Genre id → every item tagged with that genre. LIB-C4: resolve
    //    via the genres.wire_id → item_genres → items indexed join when
    //    the genre is a real row (the entity-backed exact pivot). If the
    //    wire id matches no genre row yet (rows scanned before C4 and not
    //    backfilled), fall back to the legacy in-memory probe.genre scan.
    if let Ok(ids) = state.stores.item_ids_for_genre(pid).await {
        if !ids.is_empty() {
            let want: std::collections::HashSet<pharos_core::MediaId> = ids.into_iter().collect();
            return items
                .iter()
                .filter(|i| want.contains(&i.id))
                .cloned()
                .collect();
        }
    }
    if items.iter().any(|i| {
        i.probe
            .genre
            .as_deref()
            .map(pharos_core::split_genre_field)
            .is_some_and(|gs| gs.iter().any(|g| genre_id_for(g) == pid))
    }) {
        return items
            .iter()
            .filter(|i| {
                i.probe
                    .genre
                    .as_deref()
                    .map(pharos_core::split_genre_field)
                    .is_some_and(|gs| gs.iter().any(|g| genre_id_for(g) == pid))
            })
            .cloned()
            .collect();
    }
    // 7) Person id → every item crediting that person. LIB-C2: resolve
    //    via people.wire_id → item_people → items indexed join. People
    //    have no legacy probe column, so there is no in-memory fallback
    //    (an unknown wire id simply yields no items → empty library).
    if let Ok(ids) = state.stores.item_ids_for_person(pid).await {
        if !ids.is_empty() {
            let want: std::collections::HashSet<pharos_core::MediaId> = ids.into_iter().collect();
            return items
                .iter()
                .filter(|i| want.contains(&i.id))
                .cloned()
                .collect();
        }
    }
    // 8) Studio id → every item tagged with that studio. LIB-C3: resolve
    //    via studios.wire_id → item_studios → items indexed join. Studios
    //    have no legacy probe column (the old /Studios stub borrowed
    //    album_artist but never linked it), so there is no in-memory
    //    fallback — an unknown wire id yields no items → empty library.
    if let Ok(ids) = state.stores.item_ids_for_studio(pid).await {
        if !ids.is_empty() {
            let want: std::collections::HashSet<pharos_core::MediaId> = ids.into_iter().collect();
            return items
                .iter()
                .filter(|i| want.contains(&i.id))
                .cloned()
                .collect();
        }
    }
    // 9) Collection / box set id → its members in curated sort_order.
    //    LIB-C5: resolve via collections.wire_id → collection_items →
    //    items, returning the members in the join's `sort_order` (NOT the
    //    id order of `items`) so a curated box set renders in order. The
    //    membership ids are the source of truth for both membership AND
    //    order; we index `items` by id and emit in member order. A wire id
    //    matching a *known but empty* collection still resolves (to an
    //    empty list) so a freshly-created empty box set browses cleanly,
    //    rather than falling through to the unknown-id empty Vec.
    if let Ok(collection) = state.stores.collection_by_wire_id(pid).await {
        if collection.is_some() {
            let member_ids = state.stores.collection_items(pid).await.unwrap_or_default();
            let mut by_id: std::collections::HashMap<pharos_core::MediaId, MediaItem> =
                items.iter().map(|i| (i.id, i.clone())).collect();
            return member_ids
                .iter()
                .filter_map(|id| by_id.remove(id))
                .collect();
        }
    }
    // 10) Tag id → every item carrying that tag. LIB-C6: resolve via
    //     tags.wire_id → item_tags → items indexed join. Tags have no
    //     legacy probe column, so there is no in-memory fallback — an
    //     unknown wire id yields no items → empty library.
    if let Ok(ids) = state.stores.item_ids_for_tag(pid).await {
        if !ids.is_empty() {
            let want: std::collections::HashSet<pharos_core::MediaId> = ids.into_iter().collect();
            return items
                .iter()
                .filter(|i| want.contains(&i.id))
                .cloned()
                .collect();
        }
    }
    // Unknown id — render an empty library.
    Vec::new()
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

/// B32 — resolve a synth MusicAlbum / MusicArtist wire id to the same DTO
/// shape the `/Albums` + `/Artists` list builders emit (plus `ChildCount` for
/// albums so the detail header shows a track count). `None` when the id names
/// neither. Names are recovered by hashing the store's distinct candidates —
/// the same pattern `resolve_parent_filter` uses, so any id that resolves as
/// a `ParentId` also resolves as a single item (and vice versa).
async fn synth_album_or_artist(
    state: &AppState,
    id_str: &str,
) -> Result<Option<serde_json::Value>, actix_web::Error> {
    use crate::api::jellyfin::dto::{album_id_for, artist_id_for};
    let albums = state
        .stores
        .distinct_album_names()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if let Some(name) = albums.iter().find(|n| album_id_for(n) == id_str) {
        let all = state
            .list_items_cached()
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        let tracks: Vec<_> = all
            .iter()
            .filter(|i| i.probe.album.as_deref() == Some(name.as_str()))
            .collect();
        let artist = tracks.iter().find_map(|i| {
            i.probe
                .album_artist
                .clone()
                .or_else(|| i.probe.artist.clone())
        });
        // B78/V38 — typed SynthItemDto.
        let mut it = SynthItemDto {
            child_count: Some(tracks.len() as u32),
            image_tags: Some(Default::default()),
            backdrop_image_tags: Some(Vec::new()),
            genres: Some(Vec::new()),
            tags: Some(Vec::new()),
            ..SynthItemDto::folder(
                id_str.to_string(),
                name.to_string(),
                state.server_id.clone(),
                "MusicAlbum",
            )
        };
        if let Some(a) = artist {
            it.album_artists = Some(vec![NameGuidPairDto {
                name: a.clone(),
                id: artist_id_for(&a),
            }]);
            it.album_artist = Some(a);
        }
        return Ok(Some(
            serde_json::to_value(it).unwrap_or(serde_json::Value::Null),
        ));
    }
    let artists = state
        .stores
        .distinct_artist_names()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if let Some(name) = artists.iter().find(|n| artist_id_for(n) == id_str) {
        // B78/V38 — typed SynthItemDto.
        return Ok(Some(
            serde_json::to_value(SynthItemDto {
                image_tags: Some(Default::default()),
                backdrop_image_tags: Some(Vec::new()),
                genres: Some(Vec::new()),
                tags: Some(Vec::new()),
                ..SynthItemDto::folder(
                    id_str.to_string(),
                    name.to_string(),
                    state.server_id.clone(),
                    "MusicArtist",
                )
            })
            .unwrap_or(serde_json::Value::Null),
        ));
    }
    Ok(None)
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
    // A real media item id is numeric; every synthesised / entity wire id
    // (library CollectionFolder, Series/Season, BoxSet collection) is 32-hex
    // and never parses as u64. Some of those resolutions scan the whole library
    // (`synth_series_or_season` → `store.list()`), so gate them behind a
    // non-numeric id — otherwise EVERY item-detail + poster fetch paid a full
    // ~13k-row scan before reaching the cheap `get(id)` below.
    let numeric_id: Option<u64> = pharos_jellyfin_api::dto::parse_item_id(id_str);
    if numeric_id.is_none() {
        // T-fix-7 follow-up: synthesised library CollectionFolder ids.
        if let Some(view) = library_view_for_id(state, id_str) {
            return Ok(crate::api::jellyfin::wire::json(&view));
        }
        if id_str == "00000000000000000000000000000000" {
            return Ok(crate::api::jellyfin::wire::json(&all_media_placeholder(
                &state.server_id,
            )));
        }
        // T-fix-18: synth Series + Season DTOs derived from any Episode item
        // whose series_id / season_id matches (one `store.list()`).
        if let Some(view) = synth_series_or_season(state, id_str).await? {
            return Ok(crate::api::jellyfin::wire::json(&view));
        }
        // LIB-C5 — a collection wire id resolves to its BoxSet BaseItemDto so
        // `/Items/{id}` on a box set returns the folder item.
        use pharos_core::CollectionStore;
        if let Some(collection) = state
            .stores
            .collection_by_wire_id(id_str)
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?
        {
            let count = state
                .stores
                .collection_items(id_str)
                .await
                .map(|m| m.len() as u32)
                .unwrap_or(0);
            return Ok(crate::api::jellyfin::wire::json(&collection_dto(
                state,
                &collection,
                count,
            )));
        }
        // T70 — a playlist wire id resolves to its Playlist BaseItemDto so
        // `/Items/{id}` on a playlist returns the folder item.
        use pharos_core::PlaylistStore;
        if let Some(playlist) = state
            .stores
            .playlist_by_wire_id(id_str)
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?
        {
            let count = state
                .stores
                .playlist_entries(id_str)
                .await
                .map(|e| e.len() as u32)
                .unwrap_or(0);
            return Ok(crate::api::jellyfin::wire::json(
                &crate::api::jellyfin::playlists::playlist_dto(state, &playlist, count),
            ));
        }
        // B32 — synth MusicAlbum / MusicArtist ids. Every list surface
        // (grids, a track's AlbumId/ArtistItems) emits these, and
        // jellyfin-web's audio detail page + playbackManager fetch them as
        // single items — a 400 here broke the whole music play flow (the
        // detail view aborted before the play button was wired). Same
        // one-way-hash recovery as `resolve_parent_filter`.
        if let Some(view) = synth_album_or_artist(state, id_str).await? {
            return Ok(crate::api::jellyfin::wire::json(&view));
        }
    }
    let id: u64 = numeric_id.ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let user_data = state
        .stores
        .get_user_data(user_id, id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // LIB-D5 — advertise ImageTags for any role with a recorded local
    // sidecar (D4) so clients request roles that are otherwise
    // upload-only (Logo/Banner/Art/Disc). Best-effort: a store error
    // just means no extra tags, never a 500.
    let local_art_roles: Vec<String> = match state.stores.artwork_for(id).await {
        Ok(rows) => rows
            .into_iter()
            .filter(|(_, source, _)| source == "local")
            .map(|(role, _, _)| role)
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, media.id = id, "artwork lookup for image tags failed");
            Vec::new()
        }
    };
    // LIB-C2 — project the item's cast/crew (item_people join, NFO order)
    // onto BaseItemDto.People. Best-effort: a store error just means an
    // empty cast list, never a 500.
    let people: Vec<pharos_core::ItemPerson> = {
        use pharos_core::PersonStore;
        match state.stores.people_for_item(id).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, media.id = id, "people lookup for item failed");
                Vec::new()
            }
        }
    };
    // LIB-C3 — project the item's studios (item_studios join, name-ordered)
    // onto BaseItemDto.Studios. Best-effort: a store error just means an
    // empty studio list, never a 500.
    let studios: Vec<pharos_core::Studio> = {
        use pharos_core::StudioStore;
        match state.stores.studios_for_item(id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, media.id = id, "studios lookup for item failed");
                Vec::new()
            }
        }
    };
    // LIB-C6 — project the item's tags (item_tags join, name-ordered) onto
    // BaseItemDto.Tags. Best-effort: a store error just means an empty tag
    // list, never a 500.
    let tags: Vec<pharos_core::Tag> = {
        use pharos_core::TagStore;
        match state.stores.tags_for_item(id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, media.id = id, "tags lookup for item failed");
                Vec::new()
            }
        }
    };
    Ok(crate::api::jellyfin::wire::json(
        &BaseItemDto::from_domain_with_user_data(&item, &state.server_id, user_data)
            .with_trickplay(
                &item.probe,
                &state.generated_trickplay_widths(item.id),
                state.trickplay_interval_ms,
            )
            .with_local_artwork_tags(id, &local_art_roles)
            .with_people(&people)
            .with_studios(&studios)
            .with_tags(&tags),
    ))
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
/// Which synthetic show-folder kind a `IncludeItemTypes=` browse asked for.
/// `None` unless the requested types are drawn EXCLUSIVELY from the virtual
/// show folders — a mixed request (e.g. `Series,Movie`) falls through to the
/// normal media query so real kinds still page from SQL.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShowFolderKind {
    Series,
    Season,
}

fn virtual_show_kind(include_item_types: Option<&str>) -> Option<ShowFolderKind> {
    let raw = include_item_types?;
    let types: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if types.is_empty() {
        return None;
    }
    // Every requested type must be a virtual show folder for us to intercept.
    if !types.iter().all(|t| t == "series" || t == "season") {
        return None;
    }
    // Season wins when present (a Season browse is always scoped to a series).
    if types.iter().any(|t| t == "season") {
        Some(ShowFolderKind::Season)
    } else {
        Some(ShowFolderKind::Series)
    }
}

/// Collapse the scoped episodes into synthetic Series (or Season) folder tiles.
/// Returns `None` when the request is not an exclusively-virtual show browse,
/// so the caller falls through to the normal media query.
/// Does `name` fall in the letter-picker bucket `prefix_lower` (already
/// lower-cased)? Letters match a case-insensitive prefix; the "#" bucket
/// (jellyfin's non-alphabetic chip) matches names whose first character isn't
/// an ASCII letter (digits, symbols, CJK, …).
fn name_matches_letter(name: &str, prefix_lower: &str) -> bool {
    let name_lower = name.trim_start().to_lowercase();
    if prefix_lower == "#" {
        !name_lower
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
    } else {
        name_lower.starts_with(prefix_lower)
    }
}

async fn maybe_list_virtual_shows(
    state: &AppState,
    user_id: UserId,
    policy: &pharos_core::UserPolicy,
    q: &ListQuery,
) -> Result<Option<HttpResponse>, actix_web::Error> {
    use crate::api::jellyfin::dto::{season_display_name, series_id_for_key};
    use std::collections::BTreeMap;

    let Some(mode) = virtual_show_kind(q.include_item_types.as_deref()) else {
        return Ok(None);
    };

    // Reuse the full parent/search scoping the media query already implements:
    // fetch the matching EPISODES unpaged, then collapse them in memory. The
    // SearchTerm→series_name match means a `SearchTerm=Code Geass` browse
    // narrows to that show before collapsing.
    let parent = resolve_parent_filter(state, q.parent_id.as_deref()).await?;
    if matches!(parent, ParentResolution::Empty) {
        return Ok(Some(crate::api::jellyfin::wire::json(&serde_json::json!({
            "Items": [],
            "TotalRecordCount": 0,
            "StartIndex": q.start_index,
        }))));
    }
    let mut mq = build_media_query(user_id, q, &parent, true);
    apply_policy_scope(&mut mq, policy, &state.parental_ratings);
    mq.kinds = vec![MediaKind::Episode];
    mq.limit = None;
    mq.start_index = 0;
    mq.sort = Vec::new();
    // The A-Z letter picker filters by NAME. These synthetic tiles are Series
    // (or Seasons), so the name is the *series name* — but the media query
    // filters `media_items.title` (the episode title). Applied there it would
    // pick series that merely *have* an episode titled "A…", not series *named*
    // "A…". Strip it from the episode query and re-apply it to the collapsed
    // series names below.
    let name_prefix = q
        .name_starts_with
        .as_deref()
        .or(q.name_starts_with_or_greater.as_deref())
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty());
    mq.filters.name_starts_with = None;
    let (episodes, _total) = state
        .stores
        .query(&mq)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;

    let descending = q
        .sort_order
        .as_deref()
        .is_some_and(|o| o.eq_ignore_ascii_case("Descending"));

    // Build one representative DTO per distinct key, name-sorted.
    let mut reps: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    match mode {
        ShowFolderKind::Series => {
            for ep in &episodes {
                let Some(series) = ep.series.as_ref() else {
                    continue;
                };
                // A-Z letter picker: keep only series whose NAME matches the
                // clicked letter (the whole point of the jump nav).
                if let Some(pre) = name_prefix.as_deref() {
                    if !name_matches_letter(&series.series_name, pre) {
                        continue;
                    }
                }
                // Sort key: lowercase name for a stable case-insensitive order,
                // with the folder-keyed id appended so same-name shows in
                // distinct folders stay separate tiles.
                let sort_key = format!(
                    "{}\u{0}{}",
                    series.series_name.to_ascii_lowercase(),
                    series_id_for_key(series.series_folder.as_deref(), &series.series_name),
                );
                reps.entry(sort_key)
                    .or_insert_with(|| series_dto(&state.server_id, series));
            }
        }
        ShowFolderKind::Season => {
            for ep in &episodes {
                let Some(series) = ep.series.as_ref() else {
                    continue;
                };
                let Some(season_n) = series.season_number else {
                    continue;
                };
                // Zero-padded season number → ascending numeric order.
                let sort_key = format!("{season_n:06}");
                reps.entry(sort_key).or_insert_with(|| {
                    season_dto(
                        &state.server_id,
                        series,
                        season_n,
                        &season_display_name(season_n),
                    )
                });
            }
        }
    }

    let mut items: Vec<serde_json::Value> = reps.into_values().collect();
    if descending {
        items.reverse();
    }
    let total = items.len() as u32;
    let start = q.start_index as usize;
    let end = start.saturating_add(q.limit as usize).min(items.len());
    let page: Vec<serde_json::Value> = if start >= items.len() {
        Vec::new()
    } else {
        items.drain(start..end).collect()
    };
    Ok(Some(crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": page,
        "TotalRecordCount": total,
        "StartIndex": q.start_index,
    }))))
}

async fn synth_series_or_season(
    state: &AppState,
    id_str: &str,
) -> Result<Option<serde_json::Value>, actix_web::Error> {
    use crate::api::jellyfin::dto::{season_display_name, season_id_for_key, series_id_for_key};
    let all = state
        .list_items_cached()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // First: series match. LIB-C11 — resolve via the folder-keyed id so
    // the synth DTO matches the ids minted on episodes, and same-name
    // shows in distinct folders surface as distinct Series.
    for item in all.iter() {
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        if series_id_for_key(series.series_folder.as_deref(), &series.series_name) == id_str {
            return Ok(Some(series_dto(&state.server_id, series)));
        }
    }
    // Then: season match. We need (folder/series, season_number) so
    // walk every Episode again.
    for item in all.iter() {
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        let Some(season_n) = series.season_number else {
            continue;
        };
        if season_id_for_key(
            series.series_folder.as_deref(),
            &series.series_name,
            season_n,
        ) == id_str
        {
            return Ok(Some(season_dto(
                &state.server_id,
                series,
                season_n,
                &season_display_name(season_n),
            )));
        }
    }
    Ok(None)
}

fn series_dto(server_id: &str, series: &pharos_core::SeriesInfo) -> serde_json::Value {
    use crate::api::jellyfin::dto::series_id_for_key;
    let id = series_id_for_key(series.series_folder.as_deref(), &series.series_name);
    // Advertise a Primary + Thumb tag so jellyfin-web requests the poster.
    // pharos has no stored Series row, so `/Items/{id}/Images/Primary` resolves
    // the synth id to a representative episode's frame (see images.rs). The tag
    // value is only a cache-buster; the stable id keeps client URLs constant.
    // B78/V38 — typed SeriesFolderDto. The Primary/Thumb/Backdrop tags are the
    // stable id (a cache-buster; images.rs resolves the synth id to a
    // representative episode frame). LIB-C11 — surface the folder-parsed year so
    // jellyfin-web tells same-name shows apart.
    let mut image_tags = std::collections::BTreeMap::new();
    image_tags.insert("Primary".to_string(), id.clone());
    image_tags.insert("Thumb".to_string(), id.clone());
    serde_json::to_value(SeriesFolderDto {
        name: series.series_name.clone(),
        server_id: server_id.to_string(),
        kind: "Series",
        media_type: "Unknown",
        is_folder: true,
        can_play: false,
        series_name: None,
        series_id: None,
        index_number: None,
        user_data: UserItemDataDto::folder(&id, false, 0, false),
        genres: Vec::new(),
        genre_items: Vec::new(),
        tags: Vec::new(),
        studios: Vec::new(),
        production_locations: Some(Vec::new()),
        remote_trailers: Some(Vec::new()),
        chapters: Some(Vec::new()),
        image_tags,
        backdrop_image_tags: vec![id.clone()],
        provider_ids: std::collections::BTreeMap::new(),
        production_year: series.series_year.map(|y| y as i32),
        id,
    })
    .unwrap_or(serde_json::Value::Null)
}

fn season_dto(
    server_id: &str,
    series: &pharos_core::SeriesInfo,
    season_number: u32,
    season_name: &str,
) -> serde_json::Value {
    use crate::api::jellyfin::dto::{season_id_for_key, series_id_for_key};
    let id = season_id_for_key(
        series.series_folder.as_deref(),
        &series.series_name,
        season_number,
    );
    // B78/V38 — typed SeriesFolderDto (Season kind).
    let mut image_tags = std::collections::BTreeMap::new();
    image_tags.insert("Primary".to_string(), id.clone());
    serde_json::to_value(SeriesFolderDto {
        name: season_name.to_string(),
        server_id: server_id.to_string(),
        kind: "Season",
        media_type: "Unknown",
        is_folder: true,
        can_play: false,
        series_name: Some(series.series_name.clone()),
        series_id: Some(series_id_for_key(
            series.series_folder.as_deref(),
            &series.series_name,
        )),
        index_number: Some(season_number),
        user_data: UserItemDataDto::folder(&id, false, 0, false),
        genres: Vec::new(),
        genre_items: Vec::new(),
        tags: Vec::new(),
        studios: Vec::new(),
        production_locations: None,
        remote_trailers: None,
        chapters: None,
        image_tags,
        backdrop_image_tags: Vec::new(),
        provider_ids: std::collections::BTreeMap::new(),
        production_year: None,
        id,
    })
    .unwrap_or(serde_json::Value::Null)
}

async fn list_user_items_resume(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    req: actix_web::HttpRequest,
) -> Result<impl Responder, actix_web::Error> {
    let bearer_id = user.0.id.0.simple().to_string();
    if path.into_inner() != bearer_id {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    resume_items(&state, &user, &req).await
}

/// `GET /UserItems/Resume` (B65) — the newer, path-less Resume alias the
/// Android/Google-TV app uses (jellyfin-web still uses
/// `/Users/{id}/Items/Resume`). Derives the user from the bearer; pharos only
/// registered the path form, so the TV's home-screen "Continue Watching" 404'd.
async fn user_items_resume(
    state: web::Data<AppState>,
    user: AuthUser,
    req: actix_web::HttpRequest,
) -> Result<impl Responder, actix_web::Error> {
    resume_items(&state, &user, &req).await
}

/// Shared core: the resumable-items list, filtered by the `MediaTypes` query
/// (Continue Watching/Listening/Reading rows), as a `BaseItemDto` result.
async fn resume_items(
    state: &AppState,
    user: &AuthUser,
    req: &actix_web::HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    // jellyfin-web renders three home rows — Continue Watching / Listening /
    // Reading — from the SAME endpoint, distinguished only by a `MediaTypes`
    // filter (`Video` / `Audio` / `Book`). Honour it, else every row shows the
    // same items (e.g. movies under "Continue Reading"). Absent = no filter.
    let media_types: Vec<String> = req
        .query_string()
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case("MediaTypes"))
        .map(|(_, v)| {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let media_type_of = |kind: pharos_core::MediaKind| match kind {
        pharos_core::MediaKind::Audio => "audio",
        _ => "video",
    };
    let ids = state
        .stores
        .resumable_items(user.0.id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut items: Vec<MediaItem> = Vec::with_capacity(ids.len());
    let mut ids_kept: Vec<pharos_core::MediaId> = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Ok(item) = state.stores.get(*id).await {
            // pharos has no Book media, so a `MediaTypes=Book` row is correctly
            // empty (hidden by the client).
            if media_types.is_empty() || media_types.iter().any(|t| t == media_type_of(item.kind)) {
                items.push(item);
                ids_kept.push(*id);
            }
        }
    }
    let ids = ids_kept;
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
            BaseItemDto::from_domain_with_user_data(item, &state.server_id, ud).with_trickplay(
                &item.probe,
                &state.generated_trickplay_widths(item.id),
                state.trickplay_interval_ms,
            )
        })
        .collect();
    Ok(crate::api::jellyfin::wire::json(&ItemsResultDto {
        items: dtos,
        total_record_count: total,
        start_index: 0,
    }))
}

async fn virtual_folders(
    state: web::Data<AppState>,
    _user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    // LIB-C1 — one VirtualFolderInfoDto per real typed library, with its
    // root path as the single `Locations` entry and the per-kind
    // CollectionType. Item id = the stable library wire id so client URLs
    // survive. Falls back to synthesising one `mixed` folder per
    // configured `media_roots` entry, then to the legacy "All Media" stub
    // when neither is configured (keeps the wire shape jellyfin-web
    // accepts).
    // Snapshot the library rows into owned tuples so the AppState mutex guard
    // isn't held across the per-folder `.await`s below.
    let libraries: Vec<(String, String, &'static str, String)> = state
        .libraries()
        .iter()
        .map(|l| {
            (
                l.name.clone(),
                l.root_path.clone(),
                l.kind.collection_type(),
                l.wire_id.clone(),
            )
        })
        .collect();
    let mut folders: Vec<serde_json::Value> = Vec::new();
    if !libraries.is_empty() {
        for (name, root, ctype, wire) in libraries.iter() {
            folders.push(virtual_folder_json(&state, name, root, ctype, wire).await);
        }
    } else if !state.media_roots.is_empty() {
        for root in state.media_roots.iter() {
            let name = root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("Media")
                .to_string();
            let wire = library_id_for_root(root);
            folders.push(
                virtual_folder_json(&state, &name, &root.to_string_lossy(), "mixed", &wire).await,
            );
        }
    } else {
        folders.push(
            virtual_folder_json(
                &state,
                "All Media",
                "",
                "mixed",
                "00000000000000000000000000000000",
            )
            .await,
        );
    }
    Ok(crate::api::jellyfin::wire::json(&folders))
}

/// T69 — build one `VirtualFolderInfo` as JSON, overlaying the library's
/// persisted `LibraryOptions` (named_config key `libopts:{wire}`) on the
/// shaped defaults and appending any operator-added extra `Locations`
/// (`libpaths:{wire}`) to the root path.
async fn virtual_folder_json(
    state: &AppState,
    name: &str,
    root_path: &str,
    collection_type: &'static str,
    wire_id: &str,
) -> serde_json::Value {
    let mut library_options = default_library_options();
    if let Ok(Some(raw)) = state
        .stores
        .load_named_config(&format!("libopts:{wire_id}"))
        .await
    {
        if let (Some(base), Ok(serde_json::Value::Object(stored))) = (
            library_options.as_object_mut(),
            serde_json::from_str::<serde_json::Value>(&raw),
        ) {
            for (k, v) in stored {
                base.insert(k, v);
            }
        }
    }
    let mut locations: Vec<String> = Vec::new();
    if !root_path.is_empty() {
        locations.push(root_path.to_string());
    }
    locations.extend(library_extra_paths(state, wire_id).await);
    serde_json::json!({
        "Name": name,
        "Locations": locations,
        "CollectionType": collection_type,
        "ItemId": wire_id,
        "LibraryOptions": library_options,
    })
}

/// The extra media paths an operator added to a library beyond its root
/// (`POST /Library/VirtualFolders/Paths`), stored as a JSON array under
/// `libpaths:{wire}`. Empty when none / on any parse error.
async fn library_extra_paths(state: &AppState, wire_id: &str) -> Vec<String> {
    match state
        .stores
        .load_named_config(&format!("libpaths:{wire_id}"))
        .await
    {
        Ok(Some(raw)) => serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A shaped default `LibraryOptions` object — the fields jellyfin-web's
/// library-settings form reads. Persisted overrides overlay this.
fn default_library_options() -> serde_json::Value {
    serde_json::json!({
        "Enabled": true,
        "EnablePhotos": true,
        "EnableRealtimeMonitor": false,
        "EnableChapterImageExtraction": false,
        "ExtractChapterImagesDuringLibraryScan": false,
        "EnableInternetProviders": false,
        "SaveLocalMetadata": false,
        "PreferredMetadataLanguage": "en",
        "MetadataCountryCode": "US",
        "SeasonZeroDisplayName": "Specials",
        "AutomaticallyAddToCollection": false,
        "MetadataSavers": [],
        "DisabledLocalMetadataReaders": [],
        "LocalMetadataReaderOrder": ["Nfo"],
        "TypeOptions": [],
        "PathInfos": [],
    })
}

/// `POST /Library/VirtualFolders` — the dashboard "Add Media Library" wizard.
/// jellyfin-web sends `?name=&collectionType=&refreshLibrary=` with the path in
/// the body (`LibraryOptions.PathInfos[].Path`); some flows also pass `?paths=`.
/// We create one typed library per path (the store keys a library by its root
/// path), reload the runtime set, backfill `library_id` on the already-scanned
/// items under that path, and — only when the path is not already under a
/// configured media root — kick a background scan to import fresh content.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case", default)]
struct AddVirtualFolderQuery {
    name: Option<String>,
    collection_type: Option<String>,
    /// `?paths=` (repeatable in Jellyfin; actix hands us the last, so the body
    /// is the primary source). A single `?path=` is also accepted.
    path: Option<String>,
    #[allow(dead_code)]
    refresh_library: Option<bool>,
}

async fn add_virtual_folder(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<AddVirtualFolderQuery>,
    body: Option<web::Json<serde_json::Value>>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    use pharos_core::LibraryStore;

    // The raw LibraryOptions object from the body — persisted per library
    // (T69) so EnablePhotos / PreferredMetadataLanguage / fetcher order etc.
    // round-trip, instead of being dropped down to just the path.
    let body_val = body.map(|b| b.into_inner());
    let library_options_blob = body_val
        .as_ref()
        .and_then(|v| v.get("LibraryOptions"))
        .cloned();
    // Gather the paths: body PathInfos first, then a `?path=` fallback.
    let mut paths: Vec<String> = library_options_blob
        .as_ref()
        .and_then(|lo| lo.get("PathInfos"))
        .and_then(|pi| pi.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("Path").and_then(|v| v.as_str()))
                .map(str::to_string)
                .filter(|p| !p.trim().is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        if let Some(p) = q.path.as_deref() {
            if !p.trim().is_empty() {
                paths.push(p.to_string());
            }
        }
    }
    if paths.is_empty() {
        return Err(error::ErrorBadRequest(
            "no library path supplied (LibraryOptions.PathInfos[].Path)",
        ));
    }

    let kind = pharos_core::LibraryKind::parse(q.collection_type.as_deref().unwrap_or("mixed"));

    for (i, path) in paths.iter().enumerate() {
        let root = std::path::PathBuf::from(path);
        // One library per path — name the extras after the folder so they
        // stay distinct rows (the store keys on root_path anyway).
        let name = match (&q.name, i) {
            (Some(n), 0) => n.clone(),
            (Some(n), _) => format!(
                "{n} ({})",
                root.file_name().and_then(|s| s.to_str()).unwrap_or("extra")
            ),
            (None, _) => root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("Media")
                .to_string(),
        };
        let wire_id = library_id_for_root(&root);
        state
            .stores
            .upsert_library(&name, path, kind, &wire_id)
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        // T69 — persist the LibraryOptions blob (minus PathInfos, which are the
        // Locations, tracked separately) so it round-trips on the GET.
        if let Some(mut lo) = library_options_blob.clone() {
            if let Some(obj) = lo.as_object_mut() {
                obj.remove("PathInfos");
            }
            let _ = state
                .stores
                .set_named_config(&format!("libopts:{wire_id}"), &lo.to_string())
                .await;
        }
    }

    // Stamp library_id on items already indexed under the new path(s), then
    // publish the reloaded set so the views/virtualfolders reflect it now.
    reload_libraries_and_backfill(&state).await?;

    // Kick a background scan for any path that isn't already under a scanned
    // media root (paths under an existing root are already indexed → backfill
    // above is enough; avoid a redundant full walk).
    let new_roots: Vec<std::path::PathBuf> = paths
        .iter()
        .map(std::path::PathBuf::from)
        .filter(|p| !state.media_roots.iter().any(|r| p.starts_with(r)))
        .collect();
    if !new_roots.is_empty() {
        spawn_scan(state.clone().into_inner(), new_roots, false);
    }

    // Echo the created folder set back (jellyfin-web ignores the body but
    // expects 2xx). 204 keeps it simple.
    Ok(HttpResponse::NoContent().finish())
}

/// `DELETE /Library/VirtualFolders?name=<display name>` — drop a library by
/// its display name. The items stay indexed (they still live under a media
/// root); only the typed grouping is removed.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case", default)]
struct RemoveVirtualFolderQuery {
    name: Option<String>,
}

async fn remove_virtual_folder(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<RemoveVirtualFolderQuery>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    use pharos_core::LibraryStore;
    let Some(name) = q.name.as_deref().filter(|n| !n.trim().is_empty()) else {
        return Err(error::ErrorBadRequest("missing ?name="));
    };
    // Resolve the root_path from the current set, then delete by root.
    let root = state
        .libraries()
        .iter()
        .find(|l| l.name == name)
        .map(|l| l.root_path.clone());
    let Some(root) = root else {
        return Err(error::ErrorNotFound("no such library"));
    };
    state
        .stores
        .delete_library(&root)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    reload_libraries_and_backfill(&state).await?;
    Ok(HttpResponse::NoContent().finish())
}

/// `POST /Items/{id}/Refresh` — the metadata-editor "Refresh" button + the
/// context-menu "Refresh metadata". jellyfin-web re-fetches from providers;
/// pharos has no online providers, so a refresh = re-PROBE the item's file
/// (force-bypassing the incremental `(mtime,size)` skip) so a changed file or
/// a probe-schema bump is picked up, plus a re-read of its NFO/sidecars. Kicks
/// a background force-scan of the item's parent directory and returns 204.
async fn refresh_item(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    use pharos_core::MediaStore;
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    // Scan the item's parent directory (force) so its file is re-probed. The
    // walk skips unchanged siblings quickly; only the target (and any genuinely
    // changed sibling) is re-read.
    let Some(dir) = item.path.parent().map(std::path::Path::to_path_buf) else {
        return Err(error::ErrorInternalServerError("item has no parent dir"));
    };
    tracing::info!(media.id = id, dir = %dir.display(), "item refresh requested");
    spawn_scan(state.into_inner(), vec![dir], true);
    Ok(HttpResponse::NoContent().finish())
}

/// Resolve a library's `wire_id` from its display name against the current set.
fn wire_id_for_library_name(state: &AppState, name: &str) -> Option<String> {
    state
        .libraries()
        .iter()
        .find(|l| l.name == name)
        .map(|l| l.wire_id.clone())
}

/// `POST /Library/VirtualFolders/LibraryOptions` (T69) — persist a library's
/// full `LibraryOptions` blob (keyed by its `Id`/wire id) so the settings
/// round-trip on the next GET. Body: `{ Id, LibraryOptions }`.
async fn update_virtual_folder_options(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<serde_json::Value>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let v = body.into_inner();
    let id = v
        .get("Id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| error::ErrorBadRequest("missing Id"))?;
    let mut opts = v
        .get("LibraryOptions")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    if let Some(obj) = opts.as_object_mut() {
        obj.remove("PathInfos"); // Locations are tracked separately.
    }
    state
        .stores
        .set_named_config(&format!("libopts:{id}"), &opts.to_string())
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case", default)]
struct RenameVirtualFolderQuery {
    id: Option<String>,
    new_name: Option<String>,
}

/// `POST /Library/VirtualFolders/Name?id=&newName=` (T69) — rename a library
/// in place (wire id + path unchanged, so item URLs survive).
async fn rename_virtual_folder(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<RenameVirtualFolderQuery>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let id =
        q.id.as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| error::ErrorBadRequest("missing id"))?;
    let new_name = q
        .new_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error::ErrorBadRequest("missing newName"))?;
    let updated = state
        .stores
        .rename_library(id, new_name)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if updated == 0 {
        return Err(error::ErrorNotFound("no such library"));
    }
    reload_libraries_and_backfill(&state).await?;
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct MediaPathBody {
    name: String,
    path_info: PathInfoValue,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct PathInfoValue {
    path: String,
}

/// `POST /Library/VirtualFolders/Paths` (T69) — add an extra media path to a
/// library (appears in `Locations`). Stored under `libpaths:{wire}`.
///
/// NOTE: pharos models a library by a single root path in the store, so the
/// extra path is recorded + surfaced for the UI but is NOT independently
/// scanned as its own root yet (multi-root libraries are a separate change);
/// files placed under it are picked up if it's within an existing media root.
async fn add_media_path(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<MediaPathBody>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let body = body.into_inner();
    let path = body.path_info.path.trim().to_string();
    if body.name.trim().is_empty() || path.is_empty() {
        return Err(error::ErrorBadRequest("Name and PathInfo.Path required"));
    }
    let Some(wire_id) = wire_id_for_library_name(&state, body.name.trim()) else {
        return Err(error::ErrorNotFound("no such library"));
    };
    let mut paths = library_extra_paths(&state, &wire_id).await;
    if !paths.contains(&path) {
        paths.push(path);
    }
    let json = serde_json::to_string(&paths).unwrap_or_else(|_| "[]".to_string());
    state
        .stores
        .set_named_config(&format!("libpaths:{wire_id}"), &json)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case", default)]
struct RemoveMediaPathQuery {
    name: Option<String>,
    path: Option<String>,
}

/// `DELETE /Library/VirtualFolders/Paths?name=&path=` (T69) — remove an
/// operator-added extra media path from a library.
async fn remove_media_path(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<RemoveMediaPathQuery>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let name = q
        .name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| error::ErrorBadRequest("missing name"))?;
    let path = q
        .path
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| error::ErrorBadRequest("missing path"))?;
    let Some(wire_id) = wire_id_for_library_name(&state, name) else {
        return Err(error::ErrorNotFound("no such library"));
    };
    let mut paths = library_extra_paths(&state, &wire_id).await;
    paths.retain(|p| p != path);
    let json = serde_json::to_string(&paths).unwrap_or_else(|_| "[]".to_string());
    state
        .stores
        .set_named_config(&format!("libpaths:{wire_id}"), &json)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

/// `GET /Libraries/AvailableOptions` (T69) — the fetcher / saver / per-type
/// catalogue jellyfin-web's library-settings form populates its dropdowns
/// from. pharos ships no ONLINE metadata/image providers (it reads local NFO +
/// sidecar art), so the fetcher lists are empty; the shape is complete so the
/// form renders. `Nfo` is advertised as the local metadata reader/saver.
async fn libraries_available_options(_user: AuthUser) -> impl Responder {
    let type_option = |t: &str| {
        serde_json::json!({
            "Type": t,
            "MetadataFetchers": [],
            "MetadataFetcherOrder": [],
            "ImageFetchers": [],
            "ImageFetcherOrder": [],
            "SupportedImageTypes": ["Primary", "Backdrop", "Logo", "Thumb", "Banner"],
            "DefaultImageOptions": [],
        })
    };
    crate::api::jellyfin::wire::json(&serde_json::json!({
        "MetadataSavers": [
            { "Name": "Nfo", "DefaultEnabled": false }
        ],
        "MetadataReaders": [
            { "Name": "Nfo", "DefaultEnabled": true }
        ],
        "SubtitleFetchers": [],
        "TypeOptions": [
            type_option("Movie"),
            type_option("Series"),
            type_option("Season"),
            type_option("Episode"),
            type_option("MusicAlbum"),
            type_option("MusicArtist"),
        ],
    }))
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case", default)]
struct DirectoryContentsQuery {
    path: Option<String>,
    #[serde(default)]
    include_directories: bool,
    #[serde(default)]
    include_files: bool,
}

/// `GET /Environment/DirectoryContents?path=&includeDirectories=` (T69) — the
/// server-side folder picker the add-library wizard uses. Lists the entries of
/// `path` (defaulting to `/`), filtered to directories and/or files. Admin
/// only (it browses the server filesystem, which is the point).
async fn environment_directory_contents(
    user: AuthUser,
    q: CiQuery<DirectoryContentsQuery>,
) -> Result<impl Responder, actix_web::Error> {
    crate::api::jellyfin::admin::require_admin(&user)?;
    let path = q.path.as_deref().filter(|p| !p.is_empty()).unwrap_or("/");
    // Default to directories when neither flag is set (the picker's usual mode).
    let want_dirs = q.include_directories || !q.include_files;
    let want_files = q.include_files;
    let mut out: Vec<serde_json::Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let is_dir = ft.is_dir();
            if (is_dir && !want_dirs) || (!is_dir && !want_files) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue; // hide dotfiles from the picker
            }
            out.push(serde_json::json!({
                "Name": name,
                "Path": entry.path().to_string_lossy(),
                "Type": if is_dir { "Directory" } else { "File" },
            }));
        }
    }
    out.sort_by(|a, b| {
        a["Name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["Name"].as_str().unwrap_or(""))
    });
    Ok(crate::api::jellyfin::wire::json(&out))
}

/// Reload the typed-library set from the store into `AppState`, re-stamping
/// `media_items.library_id` by path-prefix, and broadcast a `LibraryChanged`
/// so connected clients refresh their view list.
async fn reload_libraries_and_backfill(state: &AppState) -> Result<(), actix_web::Error> {
    use pharos_core::LibraryStore;
    state
        .stores
        .backfill_library_ids()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let libraries = state
        .stores
        .libraries()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    state.set_libraries(libraries);
    state.notify_library_changed();
    Ok(())
}

/// Spawn a background incremental scan of `roots` on the actix runtime, mirroring
/// `/Library/Refresh`. Returns immediately; the `LibraryChanged` broadcast on
/// completion lets connected clients invalidate their caches.
pub(crate) fn spawn_scan(
    state: std::sync::Arc<AppState>,
    roots: Vec<std::path::PathBuf>,
    force: bool,
) {
    actix_web::rt::spawn(async move {
        // Mirror `main::scan`'s prober selection: the `ffmpeg-lib` build probes
        // in-process via the resident libav worker (the distroless OCI image
        // ships no `ffprobe` binary, so `FfmpegProber` would fail every probe);
        // the spawn build keeps `FfmpegProber`. `force` bypasses the
        // incremental `(mtime,size)` skip to re-probe every file.
        // Adaptive backpressure — draw every probe read through the shared I/O
        // gate the server shrinks during live playback, so this background
        // re-scan paces itself down to a trickle while streaming instead of
        // saturating shared storage (the failure mode that stalled playback),
        // yet never fully pauses.
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        let scanner =
            pharos_scanner::FsScanner::new(pharos_scanner::LibavProber::with_discovered_bin())
                .with_rate_limit_ms(state.scan_rate_limit_ms)
                .with_probe_concurrency_opt(state.scan_probe_concurrency)
                .with_io_gate(state.bg_io.clone())
                .with_force(force);
        #[cfg(not(all(unix, feature = "ffmpeg-lib")))]
        let scanner = pharos_scanner::FsScanner::new(pharos_scanner::FfmpegProber::new())
            .with_rate_limit_ms(state.scan_rate_limit_ms)
            .with_probe_concurrency_opt(state.scan_probe_concurrency)
            .with_io_gate(state.bg_io.clone())
            .with_force(force);
        let mut added: Vec<pharos_core::MediaId> = Vec::new();
        let mut removed: Vec<pharos_core::MediaId> = Vec::new();
        for root in &roots {
            match scanner.scan_into(root, &state.stores).await {
                Ok(outcome) => {
                    tracing::info!(
                        root = %root.display(),
                        added = outcome.added.len(),
                        updated = outcome.updated.len(),
                        removed = outcome.removed.len(),
                        skipped = outcome.skipped,
                        "add-library: root scanned"
                    );
                    added.extend(outcome.added.iter().copied());
                    added.extend(outcome.updated.iter().copied());
                    removed.extend(outcome.removed.iter().copied());
                }
                Err(e) => {
                    tracing::warn!(root = %root.display(), error = %e, "add-library: scan failed")
                }
            }
        }
        use pharos_core::LibraryStore;
        if let Err(e) = state.stores.backfill_library_ids().await {
            tracing::warn!(error = %e, "add-library: post-scan backfill failed");
        }
        // Reload the runtime set so newly-scanned items resolve under the lib.
        if let Ok(libs) = state.stores.libraries().await {
            state.set_libraries(libs);
        }
        state.notify_library_delta(&added, &removed);
    });
}

#[cfg(test)]
mod alpha_picker_tests {
    use super::name_matches_letter;
    use super::resolve_episode_window;

    #[test]
    fn episode_window_paginates_by_start_index() {
        let ids: Vec<String> = (1..=23).map(|n| n.to_string()).collect();
        // Page 3 of a 23-item season: StartIndex=20, Limit=10 → eps 21,22,23.
        let w = resolve_episode_window(&ids, Some(20), None, Some(10));
        assert_eq!(w, 20..23, "StartIndex must skip, not return from the top");
        // First page.
        assert_eq!(resolve_episode_window(&ids, Some(0), None, Some(10)), 0..10);
        // No params → the whole list.
        assert_eq!(resolve_episode_window(&ids, None, None, None), 0..23);
        // StartIndex past the end → empty page, not a wrap.
        assert_eq!(
            resolve_episode_window(&ids, Some(100), None, Some(10)),
            23..23
        );
    }

    #[test]
    fn episode_window_start_item_id_wins_and_falls_back() {
        let ids: Vec<String> = (1..=23).map(|n| n.to_string()).collect();
        // StartItemId takes precedence over StartIndex and windows from that item.
        let w = resolve_episode_window(&ids, Some(0), Some("21"), Some(5));
        assert_eq!(w, 20..23);
        // Unknown id → fall back to the top (not an empty page).
        assert_eq!(
            resolve_episode_window(&ids, None, Some("nope"), Some(3)),
            0..3
        );
    }

    #[test]
    fn letter_prefix_is_case_insensitive_and_trims() {
        assert!(name_matches_letter("Angel", "a"));
        assert!(name_matches_letter("angel", "a"));
        assert!(name_matches_letter("  Angel", "a")); // leading space trimmed
        assert!(!name_matches_letter("Breaking Bad", "a"));
        assert!(name_matches_letter("An Idiot Abroad", "an"));
    }

    #[test]
    fn hash_bucket_matches_non_letters() {
        assert!(name_matches_letter("24", "#"));
        assert!(name_matches_letter("3%", "#"));
        assert!(name_matches_letter("[REC]", "#"));
        assert!(!name_matches_letter("Angel", "#"));
    }
}
