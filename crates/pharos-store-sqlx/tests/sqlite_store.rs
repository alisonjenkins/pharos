#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{DomainError, MediaItem, MediaKind, MediaStore};
use pharos_store_sqlx::sqlite::SqliteStore;
use std::sync::Arc;

async fn fresh() -> SqliteStore {
    SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite")
}

fn item(id: u64, path: &str, title: &str, kind: MediaKind) -> MediaItem {
    MediaItem {
        id,
        path: path.into(),
        title: title.into(),
        kind,
        ..Default::default()
    }
}

#[tokio::test]
async fn put_then_get_roundtrip() {
    let s = fresh().await;
    let it = item(1, "/m/a.mkv", "A", MediaKind::Movie);
    s.put(it.clone()).await.unwrap();
    let got = s.get(1).await.unwrap();
    // `created_at` is server-stamped on first insert — strip both
    // sides of that field before equality compare.
    let mut got_no_ts = got.clone();
    got_no_ts.created_at = None;
    let mut it_no_ts = it.clone();
    it_no_ts.created_at = None;
    assert_eq!(got_no_ts, it_no_ts);
    assert!(got.created_at.is_some(), "store should stamp created_at");
}

#[tokio::test]
async fn get_missing_is_not_found() {
    let s = fresh().await;
    match s.get(42).await {
        Err(DomainError::NotFound(42)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn list_returns_all_in_id_order() {
    let s = fresh().await;
    s.put(item(2, "/m/b.mkv", "B", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(3, "/m/c.flac", "C", MediaKind::Audio))
        .await
        .unwrap();
    let all = s.list().await.unwrap();
    let ids: Vec<u64> = all.iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[tokio::test]
async fn upsert_overwrites_existing_id() {
    let s = fresh().await;
    s.put(item(7, "/m/x.mkv", "old", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(7, "/m/x.mkv", "new", MediaKind::Movie))
        .await
        .unwrap();
    let got = s.get(7).await.unwrap();
    assert_eq!(got.title, "new");
}

#[tokio::test]
async fn concurrent_puts_do_not_lose_data() {
    // V10: store writes atomic per logical op. Spawn N puts in parallel;
    // every id observed afterwards.
    let s = Arc::new(fresh().await);
    let n: u64 = 32;
    let mut handles = Vec::with_capacity(n as usize);
    for i in 1..=n {
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            s.put(item(
                i,
                &format!("/m/{i}.mkv"),
                &format!("t{i}"),
                MediaKind::Movie,
            ))
            .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    let all = s.list().await.unwrap();
    assert_eq!(all.len(), n as usize);
}

#[tokio::test]
async fn scan_state_round_trips_through_mark_seen() {
    let s = fresh().await;
    s.put(item(1, "/a/x.mkv", "X", MediaKind::Movie))
        .await
        .unwrap();
    // No signature recorded yet (predates first scan).
    let st = s.scan_state(1).await.unwrap().expect("row present");
    assert_eq!(st.file_mtime, 0);
    assert_eq!(st.file_size, 0);
    assert_eq!(st.last_seen_scan_id, 0);

    let scan = s.begin_scan(std::path::Path::new("/a")).await.unwrap();
    s.mark_seen(1, scan, 1_700_000_000, 4242).await.unwrap();
    let st = s.scan_state(1).await.unwrap().expect("row present");
    assert_eq!(st.file_mtime, 1_700_000_000);
    assert_eq!(st.file_size, 4242);
    assert_eq!(st.last_seen_scan_id, scan);
    assert!(st.last_scanned > 0, "mark_seen stamps last_scanned");

    // Absent row -> None.
    assert!(s.scan_state(999).await.unwrap().is_none());
}

#[tokio::test]
async fn sweep_unseen_deletes_only_unseen_under_root() {
    let s = fresh().await;
    s.put(item(1, "/a/keep.mkv", "Keep", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "/a/gone.mkv", "Gone", MediaKind::Movie))
        .await
        .unwrap();

    let scan = s.begin_scan(std::path::Path::new("/a")).await.unwrap();
    // Only item 1 is seen this run; item 2 was deleted on disk.
    s.mark_seen(1, scan, 100, 10).await.unwrap();

    let swept = s.sweep_unseen(scan, "/a").await.unwrap();
    assert_eq!(swept, vec![2]);
    assert!(s.get(1).await.is_ok(), "seen item survives");
    match s.get(2).await {
        Err(DomainError::NotFound(2)) => {}
        other => panic!("unseen item should be gone, got {other:?}"),
    }
    s.finish_scan(scan, 1, swept.len() as i64).await.unwrap();
}

// B98 — the mark-and-sweep must NOT wipe the library when a scan observes far
// fewer files than exist (a mount that briefly under-reported the tree with no
// error). Seed a large root, mark NOTHING seen, and assert the sweep refuses.
#[tokio::test]
async fn sweep_guard_blocks_catastrophic_mass_delete() {
    let s = fresh().await;
    for i in 1..=120u64 {
        s.put(item(i, &format!("/big/f{i}.mkv"), "T", MediaKind::Movie))
            .await
            .unwrap();
    }
    // A scan that saw none of them (partial listing) — would delete all 120.
    let scan = s.begin_scan(std::path::Path::new("/big")).await.unwrap();
    let swept = s.sweep_unseen(scan, "/big").await.unwrap();
    assert!(
        swept.is_empty(),
        "sweep must abort when it would delete >25% of a large root, got {} deletions",
        swept.len()
    );
    // Every row must survive.
    for i in 1..=120u64 {
        assert!(
            s.get(i).await.is_ok(),
            "row {i} must survive the guarded sweep"
        );
    }
}

// The guard must NOT block a legitimate large-but-small-fraction delete (e.g. a
// removed series in a big library): 120 of 500 (24%, over the 100 floor) is a
// real deletion and must go through.
#[tokio::test]
async fn sweep_guard_allows_large_delete_under_the_fraction_cap() {
    let s = fresh().await;
    for i in 1..=500u64 {
        s.put(item(i, &format!("/lib/f{i}.mkv"), "T", MediaKind::Movie))
            .await
            .unwrap();
    }
    let scan = s.begin_scan(std::path::Path::new("/lib")).await.unwrap();
    // Mark 380 seen; 120 unseen = 24% (< 25% cap) → allowed.
    for i in 1..=380u64 {
        s.mark_seen(i, scan, 100, 10).await.unwrap();
    }
    let swept = s.sweep_unseen(scan, "/lib").await.unwrap();
    assert_eq!(
        swept.len(),
        120,
        "a 24% delete is under the cap and must apply"
    );
    assert!(s.get(1).await.is_ok(), "a seen row survives");
    match s.get(400).await {
        Err(DomainError::NotFound(400)) => {}
        other => panic!("an unseen row should be deleted, got {other:?}"),
    }
}

#[tokio::test]
async fn sweep_is_root_scoped_and_never_touches_sibling_root() {
    // V10 / brief: sweeping root A must not delete a root-B item even
    // though B's row was never marked by A's scan.
    let s = fresh().await;
    s.put(item(1, "/rootA/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "/rootB/b.mkv", "B", MediaKind::Movie))
        .await
        .unwrap();

    // Scan rootA, mark nothing under it (simulate everything deleted).
    let scan = s.begin_scan(std::path::Path::new("/rootA")).await.unwrap();
    let swept = s.sweep_unseen(scan, "/rootA").await.unwrap();
    assert_eq!(swept, vec![1], "only rootA item swept");
    assert!(
        s.get(2).await.is_ok(),
        "sibling root B item must be untouched"
    );
}

#[tokio::test]
async fn sweep_respects_path_boundary_not_string_prefix() {
    // Regression: sweeping /media/movies must NOT touch /media/movies-4k
    // (a sibling whose name merely shares a string prefix). Pre-fix, the
    // `path LIKE prefix || '%'` matched it.
    let s = fresh().await;
    s.put(item(1, "/media/movies/old.mkv", "Old", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(
        2,
        "/media/movies-4k/keep.mkv",
        "Keep",
        MediaKind::Movie,
    ))
    .await
    .unwrap();

    // Scan /media/movies, mark nothing (all gone on disk).
    let scan = s
        .begin_scan(std::path::Path::new("/media/movies"))
        .await
        .unwrap();
    let swept = s.sweep_unseen(scan, "/media/movies").await.unwrap();
    assert_eq!(swept, vec![1], "only the real /media/movies item swept");
    assert!(
        s.get(2).await.is_ok(),
        "/media/movies-4k must survive a /media/movies sweep"
    );
}

#[tokio::test]
async fn store_usable_via_generic_bound() {
    async fn drive<S: MediaStore>(s: &S, it: MediaItem) -> MediaItem {
        s.put(it.clone()).await.unwrap();
        s.get(it.id).await.unwrap()
    }
    let s = fresh().await;
    let got = drive(&s, item(1, "/m/a.mkv", "A", MediaKind::Movie)).await;
    assert_eq!(got.title, "A");
}

#[tokio::test]
async fn fingerprint_round_trips_through_set_and_find() {
    // LIB-A6: put -> set_fingerprint -> find_by_fp returns the same row.
    let s = fresh().await;
    s.put(item(1, "/a/movie.mkv", "Movie", MediaKind::Movie))
        .await
        .unwrap();

    // Absent fingerprint -> no match.
    let fp: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    assert!(
        s.find_by_fp(fp).await.unwrap().is_none(),
        "no row carries this fp yet"
    );

    s.set_fingerprint(1, fp).await.unwrap();
    let got = s.find_by_fp(fp).await.unwrap().expect("fp now present");
    assert_eq!(got.id, 1);
    assert_eq!(got.title, "Movie");

    // A different fp still misses.
    let other: [u8; 8] = [9, 9, 9, 9, 9, 9, 9, 9];
    assert!(s.find_by_fp(other).await.unwrap().is_none());
}

#[tokio::test]
async fn find_by_fp_returns_first_by_id() {
    // Two rows sharing a fingerprint (a true duplicate) -> the lowest id wins.
    let s = fresh().await;
    s.put(item(5, "/a/five.mkv", "Five", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "/a/two.mkv", "Two", MediaKind::Movie))
        .await
        .unwrap();
    let fp: [u8; 8] = [0xAB; 8];
    s.set_fingerprint(5, fp).await.unwrap();
    s.set_fingerprint(2, fp).await.unwrap();
    let got = s.find_by_fp(fp).await.unwrap().expect("match present");
    assert_eq!(got.id, 2, "first match is the lowest id");
}

#[tokio::test]
async fn set_fingerprint_on_absent_row_is_noop() {
    let s = fresh().await;
    // No row id 7 — UPDATE touches zero rows, no error.
    s.set_fingerprint(7, [1; 8]).await.unwrap();
    assert!(s.find_by_fp([1; 8]).await.unwrap().is_none());
}

#[tokio::test]
async fn rebind_path_keeps_id_and_repoints_path() {
    // LIB-A7: a moved file's row keeps its id (so user_data FK survives) and
    // just has its path column repointed in place.
    let s = fresh().await;
    s.put(item(3, "/old/movie.mkv", "Movie", MediaKind::Movie))
        .await
        .unwrap();

    s.rebind_path(3, std::path::Path::new("/new/film.mkv"))
        .await
        .unwrap();

    let got = s.get(3).await.expect("row still present under same id");
    assert_eq!(got.id, 3, "id preserved across rebind");
    assert_eq!(
        got.path,
        std::path::PathBuf::from("/new/film.mkv"),
        "path repointed to the new location"
    );
}

#[tokio::test]
async fn rebind_path_on_absent_row_is_noop() {
    let s = fresh().await;
    // No row id 42 — UPDATE touches zero rows, no error, no insert.
    s.rebind_path(42, std::path::Path::new("/x/y.mkv"))
        .await
        .unwrap();
    assert!(s.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn metadata_roundtrips_through_store() {
    // LIB-C7/C8/C9 — descriptive metadata must survive put → get.
    use pharos_core::{MediaMetadata, ProviderIds};
    let s = fresh().await;
    let mut it = item(7, "/m/matrix.mkv", "The Matrix", MediaKind::Movie);
    it.metadata = MediaMetadata {
        community_rating: Some(8.7),
        critic_rating: Some(83.0),
        official_rating: Some("R".into()),
        production_year: Some(1999),
        premiere_date: Some(922_060_800), // 1999-03-31
        overview: Some("A hacker learns the truth.".into()),
        tagline: Some("Free your mind.".into()),
        provider_ids: ProviderIds {
            tmdb: Some("603".into()),
            imdb: Some("tt0133093".into()),
            ..Default::default()
        },
        production_locations: vec!["USA".into(), "Australia".into()],
        trailers: vec!["https://youtu.be/m8e-FF8MsqU".into()],
    };
    s.put(it.clone()).await.unwrap();
    let got = s.get(7).await.unwrap();
    assert_eq!(got.metadata, it.metadata);
}

#[tokio::test]
async fn default_metadata_roundtrips_as_all_none() {
    // The un-enriched path: every metadata field is None / empty and
    // round-trips through NULL columns unchanged.
    let s = fresh().await;
    let it = item(8, "/m/plain.mkv", "Plain", MediaKind::Movie);
    s.put(it.clone()).await.unwrap();
    let got = s.get(8).await.unwrap();
    assert_eq!(got.metadata, pharos_core::MediaMetadata::default());
    assert!(got.metadata.provider_ids.is_empty());
}

#[tokio::test]
async fn series_folder_and_year_round_trip_through_store() {
    // LIB-C11 — the folder-keyed identity + parsed year must survive
    // put → get so the synth Series/Season wire ids stay stable across
    // restarts and same-name shows don't merge.
    use pharos_core::SeriesInfo;
    let s = fresh().await;
    let mut it = item(
        9,
        "/tv/Cosmos (1980)/Season 01/S01E01.mkv",
        "Cosmos E1",
        MediaKind::Episode,
    );
    it.series = Some(SeriesInfo {
        series_name: "Cosmos".into(),
        season_number: Some(1),
        episode_number: Some(1),
        series_folder: Some("/tv/Cosmos (1980)".into()),
        series_year: Some(1980),
    });
    s.put(it.clone()).await.unwrap();
    let got = s.get(9).await.unwrap();
    assert_eq!(got.series, it.series);
    let series = got.series.unwrap();
    assert_eq!(series.series_folder.as_deref(), Some("/tv/Cosmos (1980)"));
    assert_eq!(series.series_year, Some(1980));
    assert_eq!(series.series_key(), "/tv/Cosmos (1980)");
}

#[tokio::test]
async fn legacy_series_without_folder_round_trips_as_none() {
    // Rows scanned before C11 (no folder/year) decode with None and fall
    // back to the name-keyed identity.
    use pharos_core::SeriesInfo;
    let s = fresh().await;
    let mut it = item(
        10,
        "/tv/Firefly/S01E01.mkv",
        "Firefly E1",
        MediaKind::Episode,
    );
    it.series = Some(SeriesInfo {
        series_name: "Firefly".into(),
        season_number: Some(1),
        episode_number: Some(1),
        ..Default::default()
    });
    s.put(it.clone()).await.unwrap();
    let got = s.get(10).await.unwrap();
    let series = got.series.unwrap();
    assert_eq!(series.series_folder, None);
    assert_eq!(series.series_year, None);
    assert_eq!(series.series_key(), "Firefly");
}

// ---- LIB-C4: genres as entities -------------------------------------

fn item_with_genre(id: u64, path: &str, genre: &str) -> MediaItem {
    let mut it = item(id, path, "T", MediaKind::Movie);
    it.probe.genre = Some(genre.into());
    it
}

#[tokio::test]
async fn backfill_splits_genre_string_into_rows_and_links() {
    // LIB-C4 — a seeded item with "Action, Sci-Fi" yields two genre rows
    // and two item_genres links after backfill.
    use pharos_core::GenreStore;
    let s = fresh().await;
    s.put(item_with_genre(1, "/m/a.mkv", "Action, Sci-Fi"))
        .await
        .unwrap();
    let links = s.backfill_genres().await.unwrap();
    assert_eq!(links, 2, "two item_genres links");
    let rows = s.genres_with_counts().await.unwrap();
    let names: Vec<&str> = rows.iter().map(|g| g.genre.name.as_str()).collect();
    assert_eq!(names, vec!["Action", "Sci-Fi"], "ordered by name");
    assert!(rows.iter().all(|g| g.item_count == 1));
}

#[tokio::test]
async fn genre_wire_id_matches_dto_helper_and_resolves_items() {
    // LIB-C4 — /Items?ParentId=genre_id_for("Action") resolves to the
    // tagged item via the wire_id index (exact pivot).
    use pharos_core::{genre_wire_id, GenreStore};
    let s = fresh().await;
    s.put(item_with_genre(1, "/m/a.mkv", "Action, Sci-Fi"))
        .await
        .unwrap();
    s.backfill_genres().await.unwrap();
    let rows = s.genres_with_counts().await.unwrap();
    let action = rows.iter().find(|g| g.genre.name == "Action").unwrap();
    assert_eq!(action.genre.wire_id, genre_wire_id("Action"));
    let ids = s
        .item_ids_for_genre(&genre_wire_id("Action"))
        .await
        .unwrap();
    assert_eq!(ids, vec![1]);
    // An unknown wire id resolves to no items.
    assert!(s
        .item_ids_for_genre("ffffffffffffffffffffffffffffffff")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn shared_genre_increments_count_across_items() {
    // LIB-C4 — a second item sharing a genre bumps that genre's count and
    // both items resolve under the shared genre id.
    use pharos_core::{genre_wire_id, GenreStore};
    let s = fresh().await;
    s.put(item_with_genre(1, "/m/a.mkv", "Action, Sci-Fi"))
        .await
        .unwrap();
    s.put(item_with_genre(2, "/m/b.mkv", "Action"))
        .await
        .unwrap();
    s.backfill_genres().await.unwrap();
    let rows = s.genres_with_counts().await.unwrap();
    let action = rows.iter().find(|g| g.genre.name == "Action").unwrap();
    let scifi = rows.iter().find(|g| g.genre.name == "Sci-Fi").unwrap();
    assert_eq!(action.item_count, 2, "Action shared by both items");
    assert_eq!(scifi.item_count, 1, "Sci-Fi on one item");
    let mut ids = s
        .item_ids_for_genre(&genre_wire_id("Action"))
        .await
        .unwrap();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[tokio::test]
async fn backfill_is_idempotent() {
    // Running backfill twice does not duplicate rows or links.
    use pharos_core::GenreStore;
    let s = fresh().await;
    s.put(item_with_genre(1, "/m/a.mkv", "Action|Drama"))
        .await
        .unwrap();
    let first = s.backfill_genres().await.unwrap();
    let second = s.backfill_genres().await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first, 2);
    assert_eq!(s.genres_with_counts().await.unwrap().len(), 2);
}

#[tokio::test]
async fn link_item_genres_replaces_stale_links() {
    // A rescan that drops a genre clears the stale join row.
    use pharos_core::{genre_wire_id, GenreStore};
    let s = fresh().await;
    s.put(item_with_genre(1, "/m/a.mkv", "Action, Sci-Fi"))
        .await
        .unwrap();
    s.link_item_genres(1, &["Action".into(), "Sci-Fi".into()])
        .await
        .unwrap();
    // Re-link with only Drama: Action/Sci-Fi should no longer resolve item 1.
    s.link_item_genres(1, &["Drama".into()]).await.unwrap();
    assert!(s
        .item_ids_for_genre(&genre_wire_id("Action"))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        s.item_ids_for_genre(&genre_wire_id("Drama")).await.unwrap(),
        vec![1]
    );
}

// ---- LIB-C2: people as entities -------------------------------------

fn person(name: &str, kind: pharos_core::PersonKind) -> pharos_core::PersonRef {
    pharos_core::PersonRef {
        name: name.into(),
        kind,
        ..Default::default()
    }
}

#[tokio::test]
async fn link_item_people_persists_credits_with_wire_id_and_detail() {
    use pharos_core::{person_wire_id, PersonKind, PersonStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    let mut keanu = person("Keanu Reeves", PersonKind::Actor);
    keanu.character = Some("Neo".into());
    keanu.sort_order = Some(0);
    keanu.thumb = Some("http://x/keanu.jpg".into());
    let wachowski = person("Lana Wachowski", PersonKind::Director);
    s.link_item_people(1, &[keanu, wachowski]).await.unwrap();

    // /Persons list: name-ordered (Keanu < Lana), each carries the wire id.
    let rows = s.people_with_counts().await.unwrap();
    let names: Vec<&str> = rows.iter().map(|p| p.person.name.as_str()).collect();
    assert_eq!(names, vec!["Keanu Reeves", "Lana Wachowski"]);
    assert!(rows.iter().all(|p| p.item_count == 1));
    let keanu_row = rows
        .iter()
        .find(|p| p.person.name == "Keanu Reeves")
        .unwrap();
    assert_eq!(keanu_row.person.wire_id, person_wire_id("Keanu Reeves"));
    assert_eq!(
        keanu_row.person.thumb_url.as_deref(),
        Some("http://x/keanu.jpg")
    );

    // people_for_item: NFO order, structured kind + character round-trip.
    let credits = s.people_for_item(1).await.unwrap();
    assert_eq!(credits.len(), 2);
    assert_eq!(credits[0].name, "Keanu Reeves");
    assert_eq!(credits[0].kind, PersonKind::Actor);
    assert_eq!(credits[0].character.as_deref(), Some("Neo"));
    assert_eq!(credits[1].kind, PersonKind::Director);

    // ParentId pivot: person wire id → the crediting item.
    let ids = s
        .item_ids_for_person(&person_wire_id("Keanu Reeves"))
        .await
        .unwrap();
    assert_eq!(ids, vec![1]);
    // Unknown wire id → no items.
    assert!(s
        .item_ids_for_person("ffffffffffffffffffffffffffffffff")
        .await
        .unwrap()
        .is_empty());

    // person_by_wire_id resolves the single row.
    let p = s
        .person_by_wire_id(&person_wire_id("Lana Wachowski"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.name, "Lana Wachowski");
    assert!(s.person_by_wire_id("00").await.unwrap().is_none());
}

#[tokio::test]
async fn person_in_two_roles_on_one_item_keeps_both_credits() {
    // PK is (item_id, person_id, role) so one person can direct AND write.
    use pharos_core::{PersonKind, PersonStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    let mut dir = person("Jane Doe", PersonKind::Director);
    dir.role = Some("Director".into());
    let mut wri = person("Jane Doe", PersonKind::Writer);
    wri.role = Some("Writer".into());
    s.link_item_people(1, &[dir, wri]).await.unwrap();
    let credits = s.people_for_item(1).await.unwrap();
    assert_eq!(credits.len(), 2, "both distinct-role credits kept");
    let kinds: Vec<PersonKind> = credits.iter().map(|c| c.kind).collect();
    assert!(kinds.contains(&PersonKind::Director));
    assert!(kinds.contains(&PersonKind::Writer));
    // One person row, item count 1 (distinct item).
    let rows = s.people_with_counts().await.unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn link_item_people_replaces_stale_credits() {
    use pharos_core::{person_wire_id, PersonKind, PersonStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.link_item_people(1, &[person("Alice", PersonKind::Actor)])
        .await
        .unwrap();
    // Rescan crediting only Bob: Alice no longer resolves item 1.
    s.link_item_people(1, &[person("Bob", PersonKind::Actor)])
        .await
        .unwrap();
    assert!(s
        .item_ids_for_person(&person_wire_id("Alice"))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        s.item_ids_for_person(&person_wire_id("Bob")).await.unwrap(),
        vec![1]
    );
}

#[tokio::test]
async fn shared_person_increments_count_across_items() {
    use pharos_core::{person_wire_id, PersonKind, PersonStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "/m/b.mkv", "B", MediaKind::Movie))
        .await
        .unwrap();
    s.link_item_people(1, &[person("Tom Hanks", PersonKind::Actor)])
        .await
        .unwrap();
    s.link_item_people(2, &[person("Tom Hanks", PersonKind::Actor)])
        .await
        .unwrap();
    let rows = s.people_with_counts().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].item_count, 2, "Tom Hanks in both items");
    let mut ids = s
        .item_ids_for_person(&person_wire_id("Tom Hanks"))
        .await
        .unwrap();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[tokio::test]
async fn upsert_person_refreshes_thumb_without_clobbering() {
    // A later scan that learned the headshot fills thumb_url; a None on a
    // re-upsert does not wipe an existing value (COALESCE).
    use pharos_core::PersonStore;
    let s = fresh().await;
    s.upsert_person("Zoe", None, None, None).await.unwrap();
    s.upsert_person("Zoe", None, None, Some("http://x/zoe.jpg"))
        .await
        .unwrap();
    // Re-upsert with None thumb must not erase it.
    s.upsert_person("Zoe", None, None, None).await.unwrap();
    let rows = s.people_with_counts().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].person.thumb_url.as_deref(),
        Some("http://x/zoe.jpg")
    );
}

// ---- LIB-C1: typed libraries ----------------------------------------

#[tokio::test]
async fn libraries_upsert_list_and_backfill_by_path_prefix() {
    use pharos_core::{LibraryKind, LibraryStore};
    let s = fresh().await;
    // Two typed libraries + a path-boundary sibling that must NOT be
    // claimed by the /media/movies library.
    let movies_wire = "aaaa0000aaaa0000aaaa0000aaaa0000";
    let tv_wire = "bbbb1111bbbb1111bbbb1111bbbb1111";
    s.upsert_library("Movies", "/media/movies", LibraryKind::Movies, movies_wire)
        .await
        .unwrap();
    s.upsert_library("TV", "/media/tv", LibraryKind::TvShows, tv_wire)
        .await
        .unwrap();
    // item 1 under movies, item 2 under tv, item 3 under the sibling
    // /media/movies-4k (string-prefix of /media/movies but a different dir).
    s.put(item(1, "/media/movies/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(
        2,
        "/media/tv/Show/S01E01.mkv",
        "B",
        MediaKind::Episode,
    ))
    .await
    .unwrap();
    s.put(item(3, "/media/movies-4k/c.mkv", "C", MediaKind::Movie))
        .await
        .unwrap();

    let libs = s.libraries().await.unwrap();
    assert_eq!(libs.len(), 2);
    // Ordered by name: Movies, TV.
    assert_eq!(libs[0].name, "Movies");
    assert_eq!(libs[0].kind, LibraryKind::Movies);
    assert_eq!(libs[0].wire_id, movies_wire);
    assert_eq!(libs[1].kind, LibraryKind::TvShows);

    let assigned = s.backfill_library_ids().await.unwrap();
    // items 1 + 2 assigned; item 3 (movies-4k) untouched by the boundary.
    assert_eq!(assigned, 2);

    let movies_items = s.item_ids_for_library(movies_wire).await.unwrap();
    assert_eq!(movies_items, vec![1], "only the strictly-under item");
    let tv_items = s.item_ids_for_library(tv_wire).await.unwrap();
    assert_eq!(tv_items, vec![2]);
}

#[tokio::test]
async fn upsert_library_is_idempotent_and_updates_kind() {
    use pharos_core::{LibraryKind, LibraryStore};
    let s = fresh().await;
    let wire = "cccc2222cccc2222cccc2222cccc2222";
    let id1 = s
        .upsert_library("Lib", "/media/x", LibraryKind::Mixed, wire)
        .await
        .unwrap();
    // Re-upsert the same root with a new name + kind: same row, updated.
    let id2 = s
        .upsert_library("Movies", "/media/x", LibraryKind::Movies, wire)
        .await
        .unwrap();
    assert_eq!(id1, id2, "same root → same PK");
    let libs = s.libraries().await.unwrap();
    assert_eq!(libs.len(), 1);
    assert_eq!(libs[0].name, "Movies");
    assert_eq!(libs[0].kind, LibraryKind::Movies);
}

#[tokio::test]
async fn item_ids_for_unknown_library_wire_is_empty() {
    use pharos_core::LibraryStore;
    let s = fresh().await;
    assert!(s
        .item_ids_for_library("deadbeefdeadbeefdeadbeefdeadbeef")
        .await
        .unwrap()
        .is_empty());
}

// ---- LIB-C3: studios as entities ------------------------------------

#[tokio::test]
async fn studio_wire_id_matches_dto_helper_and_resolves_items() {
    // LIB-C3 — /Items?ParentId=studio_id_for("Warner Bros.") resolves to
    // the linked item via the wire_id index (exact pivot).
    use pharos_core::{studio_wire_id, StudioStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.link_item_studios(1, &["Warner Bros.".into(), "Village Roadshow".into()])
        .await
        .unwrap();
    let rows = s.studios_with_counts().await.unwrap();
    let names: Vec<&str> = rows.iter().map(|c| c.studio.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Village Roadshow", "Warner Bros."],
        "ordered by name"
    );
    let wb = rows
        .iter()
        .find(|c| c.studio.name == "Warner Bros.")
        .unwrap();
    assert_eq!(wb.studio.wire_id, studio_wire_id("Warner Bros."));
    assert_eq!(wb.item_count, 1);
    let ids = s
        .item_ids_for_studio(&studio_wire_id("Warner Bros."))
        .await
        .unwrap();
    assert_eq!(ids, vec![1]);
    // An unknown wire id resolves to no items.
    assert!(s
        .item_ids_for_studio("ffffffffffffffffffffffffffffffff")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn shared_studio_increments_count_across_items() {
    // LIB-C3 — a second item sharing a studio bumps that studio's count
    // and both items resolve under the shared studio id.
    use pharos_core::{studio_wire_id, StudioStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "/m/b.mkv", "B", MediaKind::Movie))
        .await
        .unwrap();
    s.link_item_studios(1, &["Warner Bros.".into(), "Village Roadshow".into()])
        .await
        .unwrap();
    s.link_item_studios(2, &["Warner Bros.".into()])
        .await
        .unwrap();
    let rows = s.studios_with_counts().await.unwrap();
    let wb = rows
        .iter()
        .find(|c| c.studio.name == "Warner Bros.")
        .unwrap();
    let vr = rows
        .iter()
        .find(|c| c.studio.name == "Village Roadshow")
        .unwrap();
    assert_eq!(wb.item_count, 2, "Warner Bros. shared by both items");
    assert_eq!(vr.item_count, 1, "Village Roadshow on one item");
    let mut ids = s
        .item_ids_for_studio(&studio_wire_id("Warner Bros."))
        .await
        .unwrap();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[tokio::test]
async fn link_item_studios_replaces_stale_links() {
    // A rescan that drops a studio clears the stale join row.
    use pharos_core::{studio_wire_id, StudioStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.link_item_studios(1, &["Warner Bros.".into(), "Village Roadshow".into()])
        .await
        .unwrap();
    // Re-link with only Netflix: the old studios no longer resolve item 1.
    s.link_item_studios(1, &["Netflix".into()]).await.unwrap();
    assert!(s
        .item_ids_for_studio(&studio_wire_id("Warner Bros."))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        s.item_ids_for_studio(&studio_wire_id("Netflix"))
            .await
            .unwrap(),
        vec![1]
    );
    // studios_for_item projects the surviving studio, name-ordered.
    let studios = s.studios_for_item(1).await.unwrap();
    let names: Vec<&str> = studios.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["Netflix"]);
    assert_eq!(studios[0].wire_id, studio_wire_id("Netflix"));
}

#[tokio::test]
async fn upsert_studio_is_idempotent() {
    // Re-upserting an existing studio name returns the same id.
    use pharos_core::StudioStore;
    let s = fresh().await;
    let id1 = s.upsert_studio("Warner Bros.").await.unwrap();
    let id2 = s.upsert_studio("Warner Bros.").await.unwrap();
    assert_eq!(id1, id2);
    assert_eq!(s.studios_with_counts().await.unwrap().len(), 1);
}

#[tokio::test]
async fn collection_wire_id_matches_dto_helper_and_resolves_members_in_order() {
    // LIB-C5 — /Items?ParentId=collection_wire_id("Trilogy") resolves to
    // the members via the wire_id index, in curated sort_order.
    use pharos_core::{collection_wire_id, CollectionStore};
    let s = fresh().await;
    for (id, p) in [(10u64, "/m/c.mkv"), (11, "/m/a.mkv"), (12, "/m/b.mkv")] {
        s.put(item(id, p, "x", MediaKind::Movie)).await.unwrap();
    }
    // Link in a non-id order so we prove sort_order (not item id) drives it.
    s.link_item_collections(12, &["Trilogy".into()])
        .await
        .unwrap();
    s.link_item_collections(10, &["Trilogy".into()])
        .await
        .unwrap();
    s.link_item_collections(11, &["Trilogy".into()])
        .await
        .unwrap();
    let rows = s.collections_with_counts().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].collection.name, "Trilogy");
    assert_eq!(rows[0].collection.wire_id, collection_wire_id("Trilogy"));
    assert_eq!(rows[0].collection.kind, "boxset");
    assert_eq!(rows[0].item_count, 3);
    let members = s
        .collection_items(&collection_wire_id("Trilogy"))
        .await
        .unwrap();
    assert_eq!(members, vec![12, 10, 11], "members in scan/sort order");
    // Unknown wire id resolves to no members.
    assert!(s
        .collection_items("ffffffffffffffffffffffffffffffff")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn link_item_collections_is_idempotent_and_appends() {
    // Re-linking an item already a member is a no-op (no duplicate, order
    // preserved); a new item appends after the current max.
    use pharos_core::{collection_wire_id, CollectionStore};
    let s = fresh().await;
    for id in [1u64, 2] {
        s.put(item(id, &format!("/m/{id}.mkv"), "x", MediaKind::Movie))
            .await
            .unwrap();
    }
    s.link_item_collections(1, &["Set".into()]).await.unwrap();
    s.link_item_collections(1, &["Set".into()]).await.unwrap(); // no-op
    s.link_item_collections(2, &["Set".into()]).await.unwrap();
    let members = s
        .collection_items(&collection_wire_id("Set"))
        .await
        .unwrap();
    assert_eq!(members, vec![1, 2]);
    let rows = s.collections_with_counts().await.unwrap();
    assert_eq!(rows[0].item_count, 2);
}

#[tokio::test]
async fn upsert_collection_refreshes_overview_without_clobbering() {
    // overview/kind COALESCE: a later None doesn't wipe a set value.
    use pharos_core::{collection_wire_id, CollectionStore};
    let s = fresh().await;
    let id1 = s
        .upsert_collection("Set", None, Some("synopsis"))
        .await
        .unwrap();
    let id2 = s.upsert_collection("Set", None, None).await.unwrap();
    assert_eq!(id1, id2, "idempotent on name");
    let c = s
        .collection_by_wire_id(&collection_wire_id("Set"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        c.overview.as_deref(),
        Some("synopsis"),
        "None didn't clobber"
    );
    // A new overview overwrites.
    s.upsert_collection("Set", None, Some("revised"))
        .await
        .unwrap();
    let c = s
        .collection_by_wire_id(&collection_wire_id("Set"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.overview.as_deref(), Some("revised"));
}

#[tokio::test]
async fn create_add_remove_collection_crud_roundtrip() {
    // LIB-C5 manual CRUD: create seeded, add more, remove, browse order.
    use pharos_core::{collection_wire_id, CollectionStore};
    let s = fresh().await;
    for id in [1u64, 2, 3, 4] {
        s.put(item(id, &format!("/m/{id}.mkv"), "x", MediaKind::Movie))
            .await
            .unwrap();
    }
    // Create seeded with [2, 1] — order preserved.
    let c = s.create_collection("MySet", &[2, 1]).await.unwrap();
    assert_eq!(c.wire_id, collection_wire_id("MySet"));
    assert_eq!(
        s.collection_items(&c.wire_id).await.unwrap(),
        vec![2, 1],
        "seed order"
    );
    // Add [3, 1, 4]: 1 already present (skipped), 3 and 4 appended.
    let added = s
        .add_collection_items(&c.wire_id, &[3, 1, 4])
        .await
        .unwrap();
    assert_eq!(added, Some(2), "only 3 and 4 newly added");
    assert_eq!(
        s.collection_items(&c.wire_id).await.unwrap(),
        vec![2, 1, 3, 4]
    );
    // Remove [1, 99]: 1 removed, 99 not a member.
    let removed = s
        .remove_collection_items(&c.wire_id, &[1, 99])
        .await
        .unwrap();
    assert_eq!(removed, Some(1));
    assert_eq!(s.collection_items(&c.wire_id).await.unwrap(), vec![2, 3, 4]);
    // CRUD against an unknown collection → None (handler maps to 404).
    let bogus = "ffffffffffffffffffffffffffffffff";
    assert_eq!(s.add_collection_items(bogus, &[1]).await.unwrap(), None);
    assert_eq!(s.remove_collection_items(bogus, &[1]).await.unwrap(), None);
}

// ---- LIB-C6: tags as entities --------------------------------------

#[tokio::test]
async fn tag_wire_id_matches_dto_helper_and_resolves_items() {
    // LIB-C6 — /Items?ParentId=tag_id_for("1080p") resolves to the linked
    // item via the wire_id index (exact pivot).
    use pharos_core::{tag_wire_id, TagStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.link_item_tags(1, &["1080p".into(), "cyberpunk".into()])
        .await
        .unwrap();
    let rows = s.tags_with_counts().await.unwrap();
    let names: Vec<&str> = rows.iter().map(|c| c.tag.name.as_str()).collect();
    assert_eq!(names, vec!["1080p", "cyberpunk"], "ordered by name");
    let cp = rows.iter().find(|c| c.tag.name == "cyberpunk").unwrap();
    assert_eq!(cp.tag.wire_id, tag_wire_id("cyberpunk"));
    assert_eq!(cp.item_count, 1);
    let ids = s.item_ids_for_tag(&tag_wire_id("1080p")).await.unwrap();
    assert_eq!(ids, vec![1]);
    // An unknown wire id resolves to no items.
    assert!(s
        .item_ids_for_tag("ffffffffffffffffffffffffffffffff")
        .await
        .unwrap()
        .is_empty());
    // tags_for_item projects name-ordered.
    let t = s.tags_for_item(1).await.unwrap();
    let tnames: Vec<&str> = t.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(tnames, vec!["1080p", "cyberpunk"]);
}

#[tokio::test]
async fn shared_tag_increments_count_across_items() {
    // LIB-C6 — a second item sharing a tag bumps that tag's count and both
    // items resolve under the shared tag id.
    use pharos_core::{tag_wire_id, TagStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "/m/b.mkv", "B", MediaKind::Movie))
        .await
        .unwrap();
    s.link_item_tags(1, &["1080p".into(), "cyberpunk".into()])
        .await
        .unwrap();
    s.link_item_tags(2, &["1080p".into()]).await.unwrap();
    let rows = s.tags_with_counts().await.unwrap();
    let hd = rows.iter().find(|c| c.tag.name == "1080p").unwrap();
    assert_eq!(hd.item_count, 2);
    let mut ids = s.item_ids_for_tag(&tag_wire_id("1080p")).await.unwrap();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[tokio::test]
async fn link_item_tags_replaces_stale_links() {
    // A rescan that drops a tag clears the stale join row (wholesale
    // replace), unlike the incremental add/remove path.
    use pharos_core::{tag_wire_id, TagStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.link_item_tags(1, &["1080p".into(), "cyberpunk".into()])
        .await
        .unwrap();
    s.link_item_tags(1, &["webrip".into()]).await.unwrap();
    assert!(s
        .item_ids_for_tag(&tag_wire_id("1080p"))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        s.item_ids_for_tag(&tag_wire_id("webrip")).await.unwrap(),
        vec![1]
    );
    let t = s.tags_for_item(1).await.unwrap();
    let names: Vec<&str> = t.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, vec!["webrip"]);
}

#[tokio::test]
async fn add_remove_item_tags_is_incremental() {
    // LIB-C6 — the manual mutation path adds/removes WITHOUT clobbering
    // the item's other tags (distinct from link's wholesale replace).
    use pharos_core::{tag_wire_id, TagStore};
    let s = fresh().await;
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.link_item_tags(1, &["1080p".into()]).await.unwrap();
    // Add two more (one duplicate) — only the genuinely-new one counts.
    let added = s
        .add_item_tags(1, &["cyberpunk".into(), "1080p".into()])
        .await
        .unwrap();
    assert_eq!(added, 1, "1080p already present, only cyberpunk is new");
    let t = s.tags_for_item(1).await.unwrap();
    let names: Vec<&str> = t.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, vec!["1080p", "cyberpunk"], "both survive");
    // Remove one — the other stays; removing an absent tag is a no-op.
    let removed = s
        .remove_item_tags(1, &["1080p".into(), "nope".into()])
        .await
        .unwrap();
    assert_eq!(removed, 1);
    let names: Vec<String> = s
        .tags_for_item(1)
        .await
        .unwrap()
        .into_iter()
        .map(|x| x.name)
        .collect();
    assert_eq!(names, vec!["cyberpunk".to_string()]);
    // The dropped tag's row survives (count drops to 0 but it still lists).
    let rows = s.tags_with_counts().await.unwrap();
    let hd = rows.iter().find(|c| c.tag.name == "1080p").unwrap();
    assert_eq!(hd.item_count, 0);
    assert_eq!(hd.tag.wire_id, tag_wire_id("1080p"));
}

#[tokio::test]
async fn upsert_tag_is_idempotent() {
    use pharos_core::TagStore;
    let s = fresh().await;
    let id1 = s.upsert_tag("cyberpunk").await.unwrap();
    let id2 = s.upsert_tag("cyberpunk").await.unwrap();
    assert_eq!(id1, id2);
    assert_eq!(s.tags_with_counts().await.unwrap().len(), 1);
}
