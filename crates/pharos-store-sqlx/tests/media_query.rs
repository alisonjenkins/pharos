//! LIB-B1 — `MediaStore::query()` against `SqliteStore`.
//!
//! Seeds a synthetic library (default 5k rows, scalable to 20k) and
//! exercises: pagination + TotalRecordCount, the kind filter, EVERY
//! `ParentFilter` variant resolved by wire_id, every `SortKey`, parity
//! between `query()` and the legacy `list()` + in-memory filter/sort for
//! representative queries, and the user-data filters.

#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{collection_wire_id, genre_wire_id, person_wire_id, studio_wire_id, tag_wire_id};
use pharos_core::{
    CollectionStore, GenreStore, LibraryKind, LibraryStore, MediaFilters, MediaId, MediaItem,
    MediaKind, MediaMetadata, MediaProbe, MediaQuery, MediaStore, ParentFilter, PersonKind,
    PersonRef, PersonStore, SeriesInfo, SortDir, SortKey, StudioStore, SubtitleTrack, TagStore,
    UserDataQuery, UserDataStore, UserId, UserItemData, UserRecord, UserStore,
};
use pharos_store_sqlx::sqlite::SqliteStore;
use pharos_store_sqlx::ServerConfigStore;

async fn fresh() -> SqliteStore {
    SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite")
}

fn kind_for(i: u64) -> MediaKind {
    match i % 3 {
        0 => MediaKind::Movie,
        1 => MediaKind::Episode,
        _ => MediaKind::Audio,
    }
}

/// A synthetic item. `id`, deterministic title/created_at/duration so the
/// sort assertions are exact. Episodes carry a series folder + season.
fn synth_item(id: u64) -> MediaItem {
    let kind = kind_for(id);
    let series = if kind == MediaKind::Episode {
        Some(SeriesInfo {
            series_name: format!("Show {}", id % 10),
            season_number: Some((id % 4) as u32 + 1),
            episode_number: Some((id % 20) as u32 + 1),
            series_folder: Some(format!("/tv/show-{}", id % 10)),
            series_year: Some(2000 + (id % 25) as u32),
        })
    } else {
        None
    };
    MediaItem {
        id,
        path: format!("/media/root-{}/file-{id:06}.mkv", id % 3).into(),
        // Title intentionally NOT id-sorted: zero-padded reversed-ish so
        // SortName order differs from id order.
        title: format!("Title {:05}", 99_999 - id),
        kind,
        probe: MediaProbe {
            duration_ms: Some((id % 7_200) * 1_000 + 1_000),
            artist: (kind == MediaKind::Audio).then(|| format!("Artist {}", id % 5)),
            album: (kind == MediaKind::Audio).then(|| format!("Album {}", id % 8)),
            album_artist: (kind == MediaKind::Audio).then(|| format!("AArtist {}", id % 5)),
            ..Default::default()
        },
        series,
        created_at: Some(1_700_000_000 + id as i64),
        metadata: MediaMetadata {
            production_year: Some(1980 + (id % 40) as u32),
            premiere_date: Some(1_500_000_000 + id as i64 * 100),
            community_rating: Some((id % 100) as f32 / 10.0),
            ..Default::default()
        },
    }
}

async fn seed(store: &SqliteStore, n: u64) {
    for id in 1..=n {
        store.put(synth_item(id)).await.unwrap();
    }
}

#[tokio::test]
async fn pagination_and_total_record_count() {
    let s = fresh().await;
    seed(&s, 5_000).await;

    // Page 0: 100 items, total = 5000.
    let q = MediaQuery {
        sort: vec![(SortKey::Id, SortDir::Asc)],
        start_index: 0,
        limit: Some(100),
        ..Default::default()
    };
    let (page, total) = s.query(&q).await.unwrap();
    assert_eq!(total, 5_000);
    assert_eq!(page.len(), 100);
    assert_eq!(page.first().unwrap().id, 1);
    assert_eq!(page.last().unwrap().id, 100);

    // Page 10 (offset 1000).
    let q2 = MediaQuery {
        start_index: 1_000,
        limit: Some(100),
        sort: vec![(SortKey::Id, SortDir::Asc)],
        ..Default::default()
    };
    let (page2, total2) = s.query(&q2).await.unwrap();
    assert_eq!(total2, 5_000);
    assert_eq!(page2.first().unwrap().id, 1_001);
    assert_eq!(page2.len(), 100);

    // Over-offset page: empty page, but total must still be 5000 (the
    // window-count fallback).
    let q3 = MediaQuery {
        start_index: 10_000,
        limit: Some(100),
        ..Default::default()
    };
    let (page3, total3) = s.query(&q3).await.unwrap();
    assert!(page3.is_empty());
    assert_eq!(total3, 5_000, "over-offset page must still report total");

    // No-limit, non-zero offset: every row past the offset.
    let q4 = MediaQuery {
        start_index: 4_990,
        limit: None,
        sort: vec![(SortKey::Id, SortDir::Asc)],
        ..Default::default()
    };
    let (page4, total4) = s.query(&q4).await.unwrap();
    assert_eq!(total4, 5_000);
    assert_eq!(page4.len(), 10);
    assert_eq!(page4.first().unwrap().id, 4_991);
}

#[tokio::test]
async fn no_pages_no_offset_returns_all() {
    let s = fresh().await;
    seed(&s, 500).await;
    let (all, total) = s.query(&MediaQuery::default()).await.unwrap();
    assert_eq!(total, 500);
    assert_eq!(all.len(), 500);
    // Default sort is the id tiebreak.
    assert_eq!(all.first().unwrap().id, 1);
    assert_eq!(all.last().unwrap().id, 500);
}

#[tokio::test]
async fn kind_filter() {
    let s = fresh().await;
    seed(&s, 3_000).await;
    // ids 1..=3000: kind = id%3 → Movie(0), Episode(1), Audio(2).
    // Movies = ids {3,6,...,3000} = 1000.
    let q = MediaQuery {
        kinds: vec![MediaKind::Movie],
        ..Default::default()
    };
    let (movies, total) = s.query(&q).await.unwrap();
    assert_eq!(total, 1_000);
    assert!(movies.iter().all(|i| i.kind == MediaKind::Movie));

    // Two kinds.
    let q2 = MediaQuery {
        kinds: vec![MediaKind::Movie, MediaKind::Audio],
        ..Default::default()
    };
    let (mixed, total2) = s.query(&q2).await.unwrap();
    assert_eq!(total2, 2_000);
    assert!(mixed
        .iter()
        .all(|i| matches!(i.kind, MediaKind::Movie | MediaKind::Audio)));
}

#[tokio::test]
async fn search_term_substring_case_insensitive() {
    let s = fresh().await;
    s.put(synth_item(1)).await.unwrap(); // "Title 99998"
    s.put(synth_item(2)).await.unwrap(); // "Title 99997"
    let mut weird = synth_item(3);
    weird.title = "The Matrix Reloaded".into();
    s.put(weird).await.unwrap();

    let q = MediaQuery {
        search_term: Some("matrix".into()),
        ..Default::default()
    };
    let (hits, total) = s.query(&q).await.unwrap();
    assert_eq!(total, 1);
    assert_eq!(hits[0].id, 3);

    // Substring with a LIKE wildcard in the term is treated literally.
    let mut pct = synth_item(4);
    pct.title = "50% off".into();
    s.put(pct).await.unwrap();
    let q2 = MediaQuery {
        search_term: Some("50%".into()),
        ..Default::default()
    };
    let (hits2, _) = s.query(&q2).await.unwrap();
    assert_eq!(hits2.len(), 1);
    assert_eq!(hits2[0].id, 4);
}

// ---------------------------------------------------------------------
// Every ParentFilter via wire_id.
// ---------------------------------------------------------------------

#[tokio::test]
async fn parent_genre_studio_person_tag_collection() {
    let s = fresh().await;
    seed(&s, 50).await;

    // Link items 1,2,3 to genre "Sci-Fi"; 4,5 to "Drama".
    s.link_item_genres(1, &["Sci-Fi".into()]).await.unwrap();
    s.link_item_genres(2, &["Sci-Fi".into()]).await.unwrap();
    s.link_item_genres(3, &["Sci-Fi".into()]).await.unwrap();
    s.link_item_genres(4, &["Drama".into()]).await.unwrap();
    s.link_item_genres(5, &["Drama".into()]).await.unwrap();

    let q = MediaQuery {
        parent: Some(ParentFilter::Genre {
            wire_id: genre_wire_id("Sci-Fi"),
        }),
        ..Default::default()
    };
    let (items, total) = s.query(&q).await.unwrap();
    assert_eq!(total, 3);
    let ids: Vec<MediaId> = items.iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![1, 2, 3]);

    // Studios.
    s.link_item_studios(10, &["A24".into()]).await.unwrap();
    s.link_item_studios(11, &["A24".into()]).await.unwrap();
    let qs = MediaQuery {
        parent: Some(ParentFilter::Studio {
            wire_id: studio_wire_id("A24"),
        }),
        ..Default::default()
    };
    let (st, total_s) = s.query(&qs).await.unwrap();
    assert_eq!(total_s, 2);
    assert_eq!(st.iter().map(|i| i.id).collect::<Vec<_>>(), vec![10, 11]);

    // People (link table carries role/kind; pivot is by person wire_id).
    let person = PersonRef {
        name: "Keanu Reeves".into(),
        kind: PersonKind::Actor,
        ..Default::default()
    };
    s.link_item_people(20, std::slice::from_ref(&person))
        .await
        .unwrap();
    s.link_item_people(21, std::slice::from_ref(&person))
        .await
        .unwrap();
    let qp = MediaQuery {
        parent: Some(ParentFilter::Person {
            wire_id: person_wire_id("Keanu Reeves"),
        }),
        ..Default::default()
    };
    let (pe, total_p) = s.query(&qp).await.unwrap();
    assert_eq!(total_p, 2);
    assert_eq!(pe.iter().map(|i| i.id).collect::<Vec<_>>(), vec![20, 21]);

    // Tags.
    s.link_item_tags(30, &["cyberpunk".into()]).await.unwrap();
    s.link_item_tags(31, &["cyberpunk".into()]).await.unwrap();
    s.link_item_tags(32, &["cyberpunk".into()]).await.unwrap();
    let qt = MediaQuery {
        parent: Some(ParentFilter::Tag {
            wire_id: tag_wire_id("cyberpunk"),
        }),
        ..Default::default()
    };
    let (tg, total_t) = s.query(&qt).await.unwrap();
    assert_eq!(total_t, 3);
    assert_eq!(
        tg.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![30, 31, 32]
    );

    // Collection: created with a curated member order (40, 35, 38). The
    // pivot must return members in that sort_order, NOT id order.
    let coll = s.create_collection("Trilogy", &[40, 35, 38]).await.unwrap();
    let qc = MediaQuery {
        parent: Some(ParentFilter::Collection {
            wire_id: coll.wire_id.clone(),
        }),
        ..Default::default()
    };
    let (cm, total_c) = s.query(&qc).await.unwrap();
    assert_eq!(total_c, 3);
    assert_eq!(
        cm.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![40, 35, 38],
        "collection members render in curated sort_order"
    );
}

#[tokio::test]
async fn parent_collection_matches_wire_id() {
    // The collection wire_id == collection_wire_id(name).
    let s = fresh().await;
    seed(&s, 10).await;
    let coll = s.create_collection("Box", &[1, 2]).await.unwrap();
    assert_eq!(coll.wire_id, collection_wire_id("Box"));
}

#[tokio::test]
async fn parent_library() {
    let s = fresh().await;
    seed(&s, 30).await;
    // ids → path /media/root-{id%3}/... . Upsert a library at /media/root-0
    // and backfill so library_id is stamped.
    let wire = "deadbeefdeadbeefdeadbeefdeadbeef";
    s.upsert_library("Movies", "/media/root-0", LibraryKind::Movies, wire)
        .await
        .unwrap();
    let assigned = s.backfill_library_ids().await.unwrap();
    assert!(assigned > 0);

    let q = MediaQuery {
        parent: Some(ParentFilter::Library {
            wire_id: wire.into(),
        }),
        sort: vec![(SortKey::Id, SortDir::Asc)],
        ..Default::default()
    };
    let (items, total) = s.query(&q).await.unwrap();
    // ids with id%3==0 under root-0: 3,6,...,30 → 10 items.
    assert_eq!(total, 10);
    assert!(items.iter().all(|i| i.id % 3 == 0));
}

#[tokio::test]
async fn parent_series_and_season() {
    let s = fresh().await;
    seed(&s, 60).await;
    // Episodes are kind=id%3==1, folder=/tv/show-{id%10}, season=id%4+1.
    // Series folder /tv/show-1 → episode ids where id%3==1 AND id%10==1:
    // ids ≡ 1 mod 3 and ≡ 1 mod 10 → id ≡ 31 mod 30 within 1..=60 → {1,31}.
    let q = MediaQuery {
        parent: Some(ParentFilter::Series {
            folder: Some("/tv/show-1".into()),
            name: "Show 1".into(),
        }),
        sort: vec![(SortKey::Id, SortDir::Asc)],
        ..Default::default()
    };
    let (eps, total) = s.query(&q).await.unwrap();
    assert!(total > 0);
    assert!(eps
        .iter()
        .all(|e| e.series.as_ref().unwrap().series_folder.as_deref() == Some("/tv/show-1")));

    // Season: restrict to one season number within that folder.
    let season = eps[0].series.as_ref().unwrap().season_number.unwrap();
    let qs = MediaQuery {
        parent: Some(ParentFilter::Season {
            folder: Some("/tv/show-1".into()),
            name: "Show 1".into(),
            season,
        }),
        ..Default::default()
    };
    let (s_eps, s_total) = s.query(&qs).await.unwrap();
    assert!(s_total > 0);
    assert!(s_eps.iter().all(|e| {
        let si = e.series.as_ref().unwrap();
        si.series_folder.as_deref() == Some("/tv/show-1") && si.season_number == Some(season)
    }));
}

#[tokio::test]
async fn parent_artist_and_album() {
    let s = fresh().await;
    seed(&s, 30).await;
    // Audio items (id%3==2) carry artist "Artist {id%5}" / album "Album {id%8}".
    let q = MediaQuery {
        parent: Some(ParentFilter::Artist {
            name: "Artist 0".into(),
        }),
        ..Default::default()
    };
    let (tracks, total) = s.query(&q).await.unwrap();
    assert!(total > 0);
    assert!(tracks.iter().all(|t| {
        t.probe.artist.as_deref() == Some("Artist 0")
            || t.probe.album_artist.as_deref() == Some("Artist 0")
    }));

    let qa = MediaQuery {
        parent: Some(ParentFilter::Album {
            name: "Album 1".into(),
        }),
        ..Default::default()
    };
    let (album_tracks, album_total) = s.query(&qa).await.unwrap();
    assert!(album_total > 0);
    assert!(album_tracks
        .iter()
        .all(|t| t.probe.album.as_deref() == Some("Album 1")));
}

#[tokio::test]
async fn unknown_parent_wire_id_is_empty() {
    let s = fresh().await;
    seed(&s, 20).await;
    let q = MediaQuery {
        parent: Some(ParentFilter::Genre {
            wire_id: "00000000000000000000000000000000".into(),
        }),
        ..Default::default()
    };
    let (items, total) = s.query(&q).await.unwrap();
    assert!(items.is_empty());
    assert_eq!(total, 0);
}

// ---------------------------------------------------------------------
// Every SortKey.
// ---------------------------------------------------------------------

#[tokio::test]
async fn all_sort_keys() {
    let s = fresh().await;
    seed(&s, 200).await;

    async fn sorted_ids(s: &SqliteStore, key: SortKey, dir: SortDir) -> Vec<MediaId> {
        let q = MediaQuery {
            sort: vec![(key, dir)],
            ..Default::default()
        };
        s.query(&q)
            .await
            .unwrap()
            .0
            .into_iter()
            .map(|i| i.id)
            .collect()
    }

    // Name asc: title = "Title {99999-id}", so ascending title == descending id.
    let by_name = sorted_ids(&s, SortKey::Name, SortDir::Asc).await;
    assert_eq!(by_name.first(), Some(&200));
    assert_eq!(by_name.last(), Some(&1));

    // Name desc.
    let by_name_desc = sorted_ids(&s, SortKey::Name, SortDir::Desc).await;
    assert_eq!(by_name_desc.first(), Some(&1));

    // DateCreated asc == id asc (created_at = base + id).
    let by_date = sorted_ids(&s, SortKey::DateCreated, SortDir::Asc).await;
    assert_eq!(by_date.first(), Some(&1));
    assert_eq!(by_date.last(), Some(&200));

    // Runtime: duration_ms = (id%7200)*1000+1000, monotonic in id here.
    let by_rt = sorted_ids(&s, SortKey::Runtime, SortDir::Asc).await;
    assert_eq!(by_rt.first(), Some(&1));

    // PremiereDate asc == id asc.
    let by_pd = sorted_ids(&s, SortKey::PremiereDate, SortDir::Asc).await;
    assert_eq!(by_pd.first(), Some(&1));

    // ProductionYear: 1980 + id%40 — ties broken by id. id=1 → 1981.
    let by_py = sorted_ids(&s, SortKey::ProductionYear, SortDir::Asc).await;
    // The smallest year is 1980 (id%40==0 → ids 40,80,...). First of those by
    // id tiebreak is 40.
    assert_eq!(by_py.first(), Some(&40));

    // CommunityRating: id%100/10 — id=100 and 200 → 0.0, first by id == 100.
    let by_cr = sorted_ids(&s, SortKey::CommunityRating, SortDir::Asc).await;
    assert_eq!(by_cr.first(), Some(&100));

    // Album / AlbumArtist: only audio rows carry them; NULLs sort first in
    // sqlite ASC, so the head is a non-audio row. Just assert it runs and is
    // a full page with the id tiebreak stable.
    let by_album = sorted_ids(&s, SortKey::Album, SortDir::Asc).await;
    assert_eq!(by_album.len(), 200);
    let by_aa = sorted_ids(&s, SortKey::AlbumArtist, SortDir::Asc).await;
    assert_eq!(by_aa.len(), 200);

    // IndexNumber (episode_number): NULL for non-episodes (sort first ASC).
    let by_idx = sorted_ids(&s, SortKey::IndexNumber, SortDir::Asc).await;
    assert_eq!(by_idx.len(), 200);

    // Id desc.
    let by_id_desc = sorted_ids(&s, SortKey::Id, SortDir::Desc).await;
    assert_eq!(by_id_desc.first(), Some(&200));
    assert_eq!(by_id_desc.last(), Some(&1));
}

#[tokio::test]
async fn multi_key_sort_with_tiebreak() {
    let s = fresh().await;
    // Two items, same production_year, distinct ids — id tiebreak resolves.
    let mut a = synth_item(1);
    a.metadata.production_year = Some(2020);
    let mut b = synth_item(2);
    b.metadata.production_year = Some(2020);
    s.put(a).await.unwrap();
    s.put(b).await.unwrap();
    let q = MediaQuery {
        sort: vec![(SortKey::ProductionYear, SortDir::Asc)],
        ..Default::default()
    };
    let (items, _) = s.query(&q).await.unwrap();
    assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1, 2]);
}

// ---------------------------------------------------------------------
// query() vs list() + manual filter/sort parity.
// ---------------------------------------------------------------------

#[tokio::test]
async fn parity_with_list_for_representative_queries() {
    let s = fresh().await;
    seed(&s, 1_000).await;

    // Representative query: movies, sorted by name desc, paged.
    let q = MediaQuery {
        kinds: vec![MediaKind::Movie],
        sort: vec![(SortKey::Name, SortDir::Desc)],
        start_index: 50,
        limit: Some(25),
        ..Default::default()
    };
    let (page, total) = s.query(&q).await.unwrap();

    // Reference: list() + the same filter/sort done by hand.
    let mut all = MediaStore::list(&s).await.unwrap();
    all.retain(|i| i.kind == MediaKind::Movie);
    let ref_total = all.len() as u64;
    all.sort_by(|a, b| {
        b.title
            .to_lowercase()
            .cmp(&a.title.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    let ref_page: Vec<MediaId> = all.iter().skip(50).take(25).map(|i| i.id).collect();

    assert_eq!(total, ref_total);
    assert_eq!(page.iter().map(|i| i.id).collect::<Vec<_>>(), ref_page);

    // Second representative query: genre pivot + DateCreated asc.
    for id in [3u64, 6, 9, 12, 15] {
        s.link_item_genres(id, &["Action".into()]).await.unwrap();
    }
    let q2 = MediaQuery {
        parent: Some(ParentFilter::Genre {
            wire_id: genre_wire_id("Action"),
        }),
        sort: vec![(SortKey::DateCreated, SortDir::Asc)],
        ..Default::default()
    };
    let (g_page, g_total) = s.query(&q2).await.unwrap();
    assert_eq!(g_total, 5);
    assert_eq!(
        g_page.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![3, 6, 9, 12, 15]
    );

    // The page items round-trip identically to a direct get().
    for it in &page {
        let got = MediaStore::get(&s, it.id).await.unwrap();
        assert_eq!(&got, it, "query() row must equal get() row");
    }
}

// ---------------------------------------------------------------------
// User-data filters.
// ---------------------------------------------------------------------

async fn make_user(s: &SqliteStore, name: &str) -> UserId {
    let uid = UserId::new();
    s.create(UserRecord {
        id: uid,
        name: name.into(),
        password_hash: pharos_core::SecretString::from("x".to_string()),
        policy: Default::default(),
    })
    .await
    .unwrap();
    uid
}

#[tokio::test]
async fn user_data_filters() {
    let s = fresh().await;
    seed(&s, 100).await;
    let uid = make_user(&s, "alice").await;

    // Favourite ids 1,2,3; played 4,5; resumable 6 (pos>0, unplayed).
    for id in [1u64, 2, 3] {
        s.set_user_data(
            uid,
            id,
            UserItemData {
                is_favorite: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    for id in [4u64, 5] {
        s.set_user_data(
            uid,
            id,
            UserItemData {
                played: true,
                play_count: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    s.set_user_data(
        uid,
        6,
        UserItemData {
            last_played_position_ticks: 123_456,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // is_favorite=true → {1,2,3}.
    let qf = MediaQuery {
        user_data: UserDataQuery {
            user: Some(uid),
            is_favorite: Some(true),
            ..Default::default()
        },
        sort: vec![(SortKey::Id, SortDir::Asc)],
        ..Default::default()
    };
    let (fav, fav_total) = s.query(&qf).await.unwrap();
    assert_eq!(fav_total, 3);
    assert_eq!(fav.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1, 2, 3]);

    // is_played=true → {4,5}.
    let qp = MediaQuery {
        user_data: UserDataQuery {
            user: Some(uid),
            is_played: Some(true),
            ..Default::default()
        },
        sort: vec![(SortKey::Id, SortDir::Asc)],
        ..Default::default()
    };
    let (played, played_total) = s.query(&qp).await.unwrap();
    assert_eq!(played_total, 2);
    assert_eq!(played.iter().map(|i| i.id).collect::<Vec<_>>(), vec![4, 5]);

    // is_played=false → 100 - 2 = 98 (a missing row is unplayed).
    let qu = MediaQuery {
        user_data: UserDataQuery {
            user: Some(uid),
            is_played: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    let (_unplayed, unplayed_total) = s.query(&qu).await.unwrap();
    assert_eq!(unplayed_total, 98);

    // is_resumable → {6} only (4,5 are played, the rest pos==0).
    let qr = MediaQuery {
        user_data: UserDataQuery {
            user: Some(uid),
            is_resumable: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let (resume, resume_total) = s.query(&qr).await.unwrap();
    assert_eq!(resume_total, 1);
    assert_eq!(resume[0].id, 6);

    // Combined: favourite AND a kind filter.
    let qc = MediaQuery {
        kinds: vec![kind_for(1), kind_for(2), kind_for(3)],
        user_data: UserDataQuery {
            user: Some(uid),
            is_favorite: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };
    let (_c, c_total) = s.query(&qc).await.unwrap();
    assert_eq!(c_total, 3);

    // A different user sees no favourites.
    let bob = make_user(&s, "bob").await;
    let qb = MediaQuery {
        user_data: UserDataQuery {
            user: Some(bob),
            is_favorite: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };
    let (_b, b_total) = s.query(&qb).await.unwrap();
    assert_eq!(b_total, 0);
}

#[tokio::test]
async fn stackable_entity_filters_and_tag_intersection() {
    let s = fresh().await;
    seed(&s, 50).await;
    // Item 1 carries tags A+B; item 2 carries only A; item 3 carries B.
    s.link_item_tags(1, &["A".into(), "B".into()])
        .await
        .unwrap();
    s.link_item_tags(2, &["A".into()]).await.unwrap();
    s.link_item_tags(3, &["B".into()]).await.unwrap();

    // tag_wire_ids = [A, B] → AND → only item 1.
    let q = MediaQuery {
        tag_wire_ids: vec![tag_wire_id("A"), tag_wire_id("B")],
        ..Default::default()
    };
    let (items, total) = s.query(&q).await.unwrap();
    assert_eq!(total, 1);
    assert_eq!(items[0].id, 1);

    // Stack a genre filter on top of a kind filter.
    s.link_item_genres(6, &["Horror".into()]).await.unwrap();
    s.link_item_genres(9, &["Horror".into()]).await.unwrap();
    let q2 = MediaQuery {
        kinds: vec![MediaKind::Movie],
        genre_wire_id: Some(genre_wire_id("Horror")),
        sort: vec![(SortKey::Id, SortDir::Asc)],
        ..Default::default()
    };
    let (g, g_total) = s.query(&q2).await.unwrap();
    // ids 6,9 are both Movies (id%3==0).
    assert_eq!(g_total, 2);
    assert_eq!(g.iter().map(|i| i.id).collect::<Vec<_>>(), vec![6, 9]);
}

#[tokio::test]
async fn large_library_20k_pagination_total() {
    let s = fresh().await;
    seed(&s, 20_000).await;
    let q = MediaQuery {
        kinds: vec![MediaKind::Audio],
        sort: vec![(SortKey::Name, SortDir::Asc)],
        start_index: 0,
        limit: Some(50),
        ..Default::default()
    };
    let (page, total) = s.query(&q).await.unwrap();
    // Audio = id%3==2 over 1..=20000 → ids 2,5,…,20000 = 6667.
    assert_eq!(total, 6_667);
    assert_eq!(page.len(), 50);
    assert!(page.iter().all(|i| i.kind == MediaKind::Audio));
}

// ---------------------------------------------------------------------------
// LIB-B2 — the residual `MediaFilters` chip filters + the synth-id distinct
// resolvers. These back the `/Items` API path that no longer loads the whole
// library; the predicates must match the legacy in-memory `filter_and_sort`.
// ---------------------------------------------------------------------------

fn item_with(id: u64, title: &str, kind: MediaKind) -> MediaItem {
    MediaItem {
        id,
        path: format!("/m/{id}.mkv").into(),
        title: title.into(),
        kind,
        ..Default::default()
    }
}

/// Seed a small explicit corpus for the residual-filter assertions.
async fn seed_residual(s: &SqliteStore) {
    // Movies with varied widths, subtitles, genres.
    let mut a = item_with(1, "Alpha", MediaKind::Movie);
    a.probe.width = Some(3840);
    a.probe.subtitle_tracks = vec![SubtitleTrack::default()];
    a.probe.genre = Some("Action, Sci-Fi".into());
    s.put(a).await.unwrap();

    let mut b = item_with(2, "Bravo", MediaKind::Movie);
    b.probe.width = Some(1920);
    b.probe.genre = Some("Drama".into());
    s.put(b).await.unwrap();

    let mut c = item_with(3, "Charlie", MediaKind::Movie);
    c.probe.width = Some(1280);
    c.probe.subtitle_tracks = vec![SubtitleTrack::default(), SubtitleTrack::default()];
    c.probe.genre = Some("Action".into());
    s.put(c).await.unwrap();

    // Episodes with index numbers.
    for (id, ep) in [(10u64, 1u32), (11, 2), (12, 3)] {
        let mut e = item_with(id, &format!("Ep {ep}"), MediaKind::Episode);
        e.series = Some(SeriesInfo {
            series_name: "Show".into(),
            season_number: Some(1),
            episode_number: Some(ep),
            series_folder: Some("/tv/Show".into()),
            series_year: Some(2020),
        });
        e.path = format!("/tv/Show/Season 1/e{ep}.mkv").into();
        s.put(e).await.unwrap();
    }
    // An audio track for artist/album resolver coverage.
    let mut au = item_with(20, "Song", MediaKind::Audio);
    au.path = "/music/song.mp3".into();
    au.probe.artist = Some("The Artist".into());
    au.probe.album = Some("The Album".into());
    au.probe.album_artist = Some("The Artist".into());
    s.put(au).await.unwrap();
}

async fn ids_for(s: &SqliteStore, f: MediaFilters) -> Vec<MediaId> {
    let q = MediaQuery {
        filters: f,
        sort: vec![(SortKey::Id, SortDir::Asc)],
        ..Default::default()
    };
    let (rows, _total) = s.query(&q).await.unwrap();
    rows.into_iter().map(|i| i.id).collect()
}

#[tokio::test]
async fn residual_exclude_and_media_type_kinds() {
    let s = fresh().await;
    seed_residual(&s).await;
    // Exclude Episode → movies + audio (1,2,3,20).
    let got = ids_for(
        &s,
        MediaFilters {
            exclude_kinds: vec![MediaKind::Episode],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(got, vec![1, 2, 3, 20]);
    // MediaTypes=Video → Movie + Episode (1,2,3,10,11,12).
    let got = ids_for(
        &s,
        MediaFilters {
            media_type_kinds: vec![MediaKind::Movie, MediaKind::Episode],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(got, vec![1, 2, 3, 10, 11, 12]);
}

#[tokio::test]
async fn residual_has_subtitles_and_resolution() {
    let s = fresh().await;
    seed_residual(&s).await;
    // Has subtitles → 1 (1 track) + 3 (2 tracks).
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                has_subtitles: Some(true),
                ..Default::default()
            }
        )
        .await,
        vec![1, 3]
    );
    // No subtitles → everything else.
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                has_subtitles: Some(false),
                ..Default::default()
            }
        )
        .await,
        vec![2, 10, 11, 12, 20]
    );
    // 4K → width >= 3840 → id 1.
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                is_4k: Some(true),
                ..Default::default()
            }
        )
        .await,
        vec![1]
    );
    // HD → 1280..3840 → ids 2, 3.
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                is_hd: Some(true),
                ..Default::default()
            }
        )
        .await,
        vec![2, 3]
    );
    // 3D true → nothing (no detection).
    assert!(ids_for(
        &s,
        MediaFilters {
            is_3d: Some(true),
            ..Default::default()
        }
    )
    .await
    .is_empty());
    // min/max width.
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                min_width: Some(1920),
                ..Default::default()
            }
        )
        .await,
        vec![1, 2]
    );
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                max_width: Some(1920),
                ..Default::default()
            }
        )
        .await,
        vec![2, 3]
    );
}

#[tokio::test]
async fn residual_index_name_and_ids() {
    let s = fresh().await;
    seed_residual(&s).await;
    // MinIndexNumber=2 → episodes 11,12.
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                min_index_number: Some(2),
                ..Default::default()
            }
        )
        .await,
        vec![11, 12]
    );
    // MaxIndexNumber=1 → episode 10.
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                max_index_number: Some(1),
                ..Default::default()
            }
        )
        .await,
        vec![10]
    );
    // NameStartsWith=B → Bravo (id 2).
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                name_starts_with: Some("B".into()),
                ..Default::default()
            }
        )
        .await,
        vec![2]
    );
    // NameLessThan=C → Alpha, Bravo (case-folded title < "c").
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                name_less_than: Some("C".into()),
                ..Default::default()
            }
        )
        .await,
        vec![1, 2]
    );
    // Ids present, explicit set.
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                ids: vec![3, 11, 9999],
                ids_present: true,
                ..Default::default()
            }
        )
        .await,
        vec![3, 11]
    );
    // Ids present but empty → nothing.
    assert!(ids_for(
        &s,
        MediaFilters {
            ids_present: true,
            ..Default::default()
        }
    )
    .await
    .is_empty());
}

#[tokio::test]
async fn residual_genre_probe_whole_and_token_and_path_prefix() {
    let s = fresh().await;
    seed_residual(&s).await;
    // genre_probe_names (whole-string, ?Genres= semantics): "Action" matches
    // only id 3 (whose genre is exactly "Action"), NOT id 1 ("Action, Sci-Fi").
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                genre_probe_names: vec!["Action".into()],
                ..Default::default()
            }
        )
        .await,
        vec![3]
    );
    // genre_probe_token (token-membership, ParentId=genre fallback): "Action"
    // matches BOTH id 1 ("Action, Sci-Fi") and id 3 ("Action").
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                genre_probe_token: Some("Action".into()),
                ..Default::default()
            }
        )
        .await,
        vec![1, 3]
    );
    // "Sci-Fi" token → only id 1.
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                genre_probe_token: Some("Sci-Fi".into()),
                ..Default::default()
            }
        )
        .await,
        vec![1]
    );
    // path_prefix component-boundary scope: /tv/Show → episodes 10,11,12.
    assert_eq!(
        ids_for(
            &s,
            MediaFilters {
                path_prefix: Some("/tv/Show".into()),
                ..Default::default()
            }
        )
        .await,
        vec![10, 11, 12]
    );
    // A sibling prefix must NOT be claimed (boundary safety).
    assert!(ids_for(
        &s,
        MediaFilters {
            path_prefix: Some("/tv/Sho".into()),
            ..Default::default()
        }
    )
    .await
    .is_empty());
}

#[tokio::test]
async fn distinct_resolvers_recover_synth_id_components() {
    let s = fresh().await;
    seed_residual(&s).await;
    // Series / season keys.
    let series = s.distinct_series_keys().await.unwrap();
    assert!(series
        .iter()
        .any(|(folder, name)| folder.as_deref() == Some("/tv/Show") && name == "Show"));
    let seasons = s.distinct_season_keys().await.unwrap();
    assert!(seasons.iter().any(
        |(folder, name, season)| folder.as_deref() == Some("/tv/Show")
            && name == "Show"
            && *season == 1
    ));
    // Artist / album names.
    let artists = s.distinct_artist_names().await.unwrap();
    assert!(artists.iter().any(|a| a == "The Artist"));
    let albums = s.distinct_album_names().await.unwrap();
    assert!(albums.iter().any(|a| a == "The Album"));
    // Genre fields (raw, including the multi-genre string).
    let genres = s.distinct_genre_fields().await.unwrap();
    assert!(genres.iter().any(|g| g == "Action, Sci-Fi"));
    assert!(genres.iter().any(|g| g == "Drama"));
}
