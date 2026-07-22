//! LIB-B4 + LIB-B5 — `MediaStore::search` (fts5) and `MediaStore::facets`
//! against `SqliteStore`.
//!
//! Exercises: FTS finds prefix + mid-word tokens; the FTS result is a
//! SUPERSET of the legacy substring scan; the external-content triggers
//! keep media_fts in sync on insert / update / delete; and facet counts
//! are correct for a seeded library across genres / studios / tags / years
//! / official-ratings, honouring the base-query scope.

#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{genre_wire_id, studio_wire_id, tag_wire_id};
use pharos_core::{
    FacetRequest, GenreStore, MediaItem, MediaKind, MediaMetadata, MediaProbe, MediaQuery,
    MediaStore, SearchQuery, StudioStore, TagStore,
};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn fresh() -> SqliteStore {
    SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite")
}

fn item(id: u64, title: &str, overview: Option<&str>, kind: MediaKind) -> MediaItem {
    MediaItem {
        id,
        path: format!("/m/{id}.mkv").into(),
        title: title.to_string(),
        kind,
        probe: MediaProbe::default(),
        series: None,
        created_at: Some(1_700_000_000 + id as i64),
        metadata: MediaMetadata {
            overview: overview.map(str::to_string),
            ..Default::default()
        },
        has_primary_art: false,
        match_provider: None,
        match_external_id: None,
        match_source: None,
        match_confidence: None,
        metadata_refreshed_at: None,
    }
}

fn sq(term: &str) -> SearchQuery {
    SearchQuery {
        term: term.to_string(),
        kinds: Vec::new(),
        limit: 100,
        offset: 0,
    }
}

/// Legacy substring semantics the FTS result must be a superset of:
/// case-insensitive substring on title (the pre-LIB-B4 /Search/Hints scan).
fn legacy_substring_ids(items: &[MediaItem], term: &str) -> Vec<u64> {
    let needle = term.trim().to_lowercase();
    let mut ids: Vec<u64> = items
        .iter()
        .filter(|i| i.title.to_lowercase().contains(&needle))
        .map(|i| i.id)
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn fts_finds_prefix_tokens() {
    let s = fresh().await;
    s.put(item(1, "Pokemon Detective", None, MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "The Matrix", None, MediaKind::Movie))
        .await
        .unwrap();

    // Prefix: "pok" matches the "Pokemon" token.
    let (hits, total) = s.search(&sq("pok")).await.unwrap();
    assert_eq!(total, 1);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, 1);

    // Prefix on the second token.
    let (hits2, _) = s.search(&sq("detect")).await.unwrap();
    assert_eq!(hits2.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1]);

    // Multi-token AND: both prefixes must match.
    let (hits3, _) = s.search(&sq("pok det")).await.unwrap();
    assert_eq!(hits3.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1]);
    let (hits4, _) = s.search(&sq("pok matrix")).await.unwrap();
    assert!(hits4.is_empty(), "AND of disjoint tokens matches nothing");
}

#[tokio::test]
async fn fts_finds_mid_word_substring() {
    let s = fresh().await;
    s.put(item(1, "Pokemon", None, MediaKind::Movie))
        .await
        .unwrap();
    // "kemon" is mid-word — fts5 prefix can't reach it, but the substring
    // arm of search() must (the SUPERSET guarantee).
    let (hits, total) = s.search(&sq("kemon")).await.unwrap();
    assert_eq!(total, 1);
    assert_eq!(hits[0].id, 1);
}

#[tokio::test]
async fn fts_searches_overview_not_only_title() {
    let s = fresh().await;
    s.put(item(
        1,
        "Untitled Film",
        Some("A documentary about volcanoes and lava."),
        MediaKind::Movie,
    ))
    .await
    .unwrap();
    let (hits, _) = s.search(&sq("volcano")).await.unwrap();
    assert_eq!(hits.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1]);
}

#[tokio::test]
async fn fts_is_superset_of_legacy_substring() {
    let s = fresh().await;
    let items = vec![
        item(1, "Pokemon", None, MediaKind::Movie),
        item(2, "Mon Oncle", None, MediaKind::Movie),
        item(3, "Common Ground", None, MediaKind::Episode),
        item(4, "Unrelated", None, MediaKind::Audio),
    ];
    for i in &items {
        s.put(i.clone()).await.unwrap();
    }
    // "mon" is a substring of Pokemon / Mon Oncle / Common Ground.
    let legacy = legacy_substring_ids(&items, "mon");
    let (hits, _) = s.search(&sq("mon")).await.unwrap();
    let mut got: Vec<u64> = hits.iter().map(|i| i.id).collect();
    got.sort_unstable();
    for id in &legacy {
        assert!(
            got.contains(id),
            "FTS result {got:?} must be a superset of legacy {legacy:?} (missing {id})"
        );
    }
}

#[tokio::test]
async fn fts_kind_filter_and_paging() {
    let s = fresh().await;
    for id in 1..=10u64 {
        let kind = if id % 2 == 0 {
            MediaKind::Movie
        } else {
            MediaKind::Episode
        };
        s.put(item(id, &format!("Story {id}"), None, kind))
            .await
            .unwrap();
    }
    // "story" prefix hits all 10; restrict to Movies → 5.
    let q = SearchQuery {
        term: "story".into(),
        kinds: vec![MediaKind::Movie],
        limit: 100,
        offset: 0,
    };
    let (hits, total) = s.search(&q).await.unwrap();
    assert_eq!(total, 5);
    assert!(hits.iter().all(|i| i.kind == MediaKind::Movie));

    // Paging: limit 2, offset 2 over the 5 movies, total unchanged.
    let q2 = SearchQuery {
        term: "story".into(),
        kinds: vec![MediaKind::Movie],
        limit: 2,
        offset: 2,
    };
    let (page, total2) = s.search(&q2).await.unwrap();
    assert_eq!(total2, 5);
    assert_eq!(page.len(), 2);
}

#[tokio::test]
async fn empty_term_matches_nothing() {
    let s = fresh().await;
    s.put(item(1, "Anything", None, MediaKind::Movie))
        .await
        .unwrap();
    let (hits, total) = s.search(&sq("   ")).await.unwrap();
    assert!(hits.is_empty());
    assert_eq!(total, 0);
}

#[tokio::test]
async fn trigger_keeps_fts_synced_on_update() {
    let s = fresh().await;
    s.put(item(1, "Original Title", None, MediaKind::Movie))
        .await
        .unwrap();
    assert_eq!(s.search(&sq("original")).await.unwrap().1, 1);

    // Re-put with a new title (the put's ON CONFLICT fires the UPDATE
    // trigger). Old token must drop; new token must appear.
    s.put(item(1, "Replaced Heading", None, MediaKind::Movie))
        .await
        .unwrap();
    assert_eq!(
        s.search(&sq("original")).await.unwrap().1,
        0,
        "stale token must be gone after update"
    );
    assert_eq!(
        s.search(&sq("replaced")).await.unwrap().1,
        1,
        "new token must be indexed after update"
    );
}

#[tokio::test]
async fn trigger_keeps_fts_synced_on_delete() {
    let s = fresh().await;
    s.put(item(1, "Deletable Movie", None, MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "Deletable Episode", None, MediaKind::Episode))
        .await
        .unwrap();
    assert_eq!(s.search(&sq("deletable")).await.unwrap().1, 2);

    // Sweep deletes row 1 (mark-and-sweep DELETE → fires the AD trigger).
    let scan = s.begin_scan(std::path::Path::new("/m")).await.unwrap();
    s.mark_seen(2, scan, 1, 1).await.unwrap();
    let swept = s.sweep_unseen(scan, "/m").await.unwrap();
    assert!(swept.contains(&1));
    let (hits, total) = s.search(&sq("deletable")).await.unwrap();
    assert_eq!(total, 1, "deleted row must drop out of the fts index");
    assert_eq!(hits[0].id, 2);
}

// ---------------------------------------------------------------------------
// LIB-B5 facets
// ---------------------------------------------------------------------------

async fn seed_faceted(s: &SqliteStore) {
    // 1: Movie, Action+Sci-Fi, Studio A, tag hd, 2019, PG-13
    // 2: Movie, Action,        Studio A, tag hd, 2019, PG-13
    // 3: Movie, Drama,         Studio B, tag 4k, 2020, R
    // 4: Episode, Comedy,      Studio B,         2020, TV-14
    let mut m1 = item(1, "One", None, MediaKind::Movie);
    m1.metadata.production_year = Some(2019);
    m1.metadata.official_rating = Some("PG-13".into());
    let mut m2 = item(2, "Two", None, MediaKind::Movie);
    m2.metadata.production_year = Some(2019);
    m2.metadata.official_rating = Some("PG-13".into());
    let mut m3 = item(3, "Three", None, MediaKind::Movie);
    m3.metadata.production_year = Some(2020);
    m3.metadata.official_rating = Some("R".into());
    let mut m4 = item(4, "Four", None, MediaKind::Episode);
    m4.metadata.production_year = Some(2020);
    m4.metadata.official_rating = Some("TV-14".into());
    for m in [&m1, &m2, &m3, &m4] {
        s.put(m.clone()).await.unwrap();
    }
    s.link_item_genres(1, &["Action".into(), "Sci-Fi".into()])
        .await
        .unwrap();
    s.link_item_genres(2, &["Action".into()]).await.unwrap();
    s.link_item_genres(3, &["Drama".into()]).await.unwrap();
    s.link_item_genres(4, &["Comedy".into()]).await.unwrap();
    s.link_item_studios(1, &["Studio A".into()]).await.unwrap();
    s.link_item_studios(2, &["Studio A".into()]).await.unwrap();
    s.link_item_studios(3, &["Studio B".into()]).await.unwrap();
    s.link_item_studios(4, &["Studio B".into()]).await.unwrap();
    s.link_item_tags(1, &["hd".into()]).await.unwrap();
    s.link_item_tags(2, &["hd".into()]).await.unwrap();
    s.link_item_tags(3, &["4k".into()]).await.unwrap();
}

#[tokio::test]
async fn facets_counts_over_whole_library() {
    let s = fresh().await;
    seed_faceted(&s).await;
    let f = s
        .facets(&MediaQuery::default(), &FacetRequest::default())
        .await
        .unwrap();

    // Genres: Action(2), Sci-Fi(1), Drama(1), Comedy(1). Action first
    // (count DESC, then name ASC).
    let action = f.genres.iter().find(|g| g.value == "Action").unwrap();
    assert_eq!(action.count, 2);
    assert_eq!(action.wire_id, genre_wire_id("Action"));
    assert_eq!(f.genres.first().unwrap().value, "Action");
    assert_eq!(
        f.genres.iter().find(|g| g.value == "Sci-Fi").unwrap().count,
        1
    );

    // Studios: A(2), B(2).
    let studio_a = f.studios.iter().find(|x| x.value == "Studio A").unwrap();
    assert_eq!(studio_a.count, 2);
    assert_eq!(studio_a.wire_id, studio_wire_id("Studio A"));
    assert_eq!(
        f.studios
            .iter()
            .find(|x| x.value == "Studio B")
            .unwrap()
            .count,
        2
    );

    // Tags: hd(2), 4k(1).
    let hd = f.tags.iter().find(|t| t.value == "hd").unwrap();
    assert_eq!(hd.count, 2);
    assert_eq!(hd.wire_id, tag_wire_id("hd"));

    // Years: 2019(2), 2020(2), newest first.
    assert_eq!(f.years.first().unwrap().value, "2020");
    assert_eq!(f.years.iter().find(|y| y.value == "2019").unwrap().count, 2);

    // Official ratings: PG-13(2), R(1), TV-14(1).
    assert_eq!(
        f.official_ratings
            .iter()
            .find(|r| r.value == "PG-13")
            .unwrap()
            .count,
        2
    );
}

#[tokio::test]
async fn facets_respect_base_kind_scope() {
    let s = fresh().await;
    seed_faceted(&s).await;
    // Restrict the base query to Movies → the Episode (id 4, Comedy,
    // Studio B, 2020, TV-14) drops out of every facet.
    let base = MediaQuery {
        kinds: vec![MediaKind::Movie],
        ..Default::default()
    };
    let f = s.facets(&base, &FacetRequest::default()).await.unwrap();
    assert!(
        f.genres.iter().all(|g| g.value != "Comedy"),
        "Comedy is episode-only and must not appear in a Movie-scoped facet"
    );
    // Studio B now only has the Movie id 3 → count 1 (not 2).
    assert_eq!(
        f.studios
            .iter()
            .find(|x| x.value == "Studio B")
            .unwrap()
            .count,
        1
    );
    // TV-14 was episode-only.
    assert!(f.official_ratings.iter().all(|r| r.value != "TV-14"));
}

#[tokio::test]
async fn facets_respect_genre_parent_scope() {
    let s = fresh().await;
    seed_faceted(&s).await;
    // Base scoped to genre Action (items 1 + 2). Studio facet → A(2) only.
    let base = MediaQuery {
        parent: Some(pharos_core::ParentFilter::Genre {
            wire_id: genre_wire_id("Action"),
        }),
        ..Default::default()
    };
    let f = s.facets(&base, &FacetRequest::default()).await.unwrap();
    assert_eq!(f.studios.len(), 1);
    assert_eq!(f.studios[0].value, "Studio A");
    assert_eq!(f.studios[0].count, 2);
    // Tags within Action scope: hd(2) only (4k is on the Drama item 3).
    assert_eq!(f.tags.len(), 1);
    assert_eq!(f.tags[0].value, "hd");
    assert_eq!(f.tags[0].count, 2);
}

#[tokio::test]
async fn facets_request_gating() {
    let s = fresh().await;
    seed_faceted(&s).await;
    // Only ask for genres → other dimensions stay empty.
    let req = FacetRequest {
        genres: true,
        studios: false,
        tags: false,
        years: false,
        official_ratings: false,
        people: false,
    };
    let f = s.facets(&MediaQuery::default(), &req).await.unwrap();
    assert!(!f.genres.is_empty());
    assert!(f.studios.is_empty());
    assert!(f.tags.is_empty());
    assert!(f.years.is_empty());
    assert!(f.official_ratings.is_empty());
}
