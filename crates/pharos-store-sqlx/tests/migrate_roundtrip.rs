//! TDD round-trip test for `pharos_store_sqlx::migrate::migrate_sqlite_to_postgres`
//! (the `pharos admin db-migrate` engine). Seeds a SqliteStore with at least
//! one row in every table on `migrate::COPIED_TABLES`, through the store trait
//! methods where they reach and raw SQL (`seed_raw`) where they don't, migrates
//! into a fresh empty Postgres, then checks the report, the row counts on both
//! sides, every column of every row (`assert_same_rows`, B1071), and a few
//! read-backs through the Postgres store trait methods.

#![cfg(all(feature = "sqlite", feature = "postgres"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{
    genre_wire_id, CollectionStore, GenreStore, LibraryKind, LibraryStore, MediaId, MediaItem,
    MediaKind, MediaMetadata, MediaProbe, PersonKind, PersonRef, PersonStore, PlaylistStore,
    PreferenceStore, SecretString, StudioStore, TagStore, TokenStore, UserDataStore, UserId,
    UserItemData, UserPolicy, UserPrefStore, UserRecord, UserStore,
};
use pharos_store_sqlx::postgres::PostgresStore;
use pharos_store_sqlx::sqlite::SqliteStore;
use pharos_store_sqlx::{RuntimeConfig, ServerConfigStore};
use std::collections::HashMap;

fn media_item(id: MediaId, title: &str, kind: MediaKind) -> MediaItem {
    MediaItem {
        locked: Default::default(),
        id,
        path: format!("/media/migrate/{id}.mkv").into(),
        title: title.into(),
        kind,
        book: None,
        probe: MediaProbe::default(),
        series: None,
        created_at: Some(1_700_000_000 + id as i64),
        metadata: MediaMetadata::default(),
        has_primary_art: false,
        art_version: 0,
        match_provider: None,
        match_external_id: None,
        match_source: None,
        match_confidence: None,
        metadata_refreshed_at: None,
        edition: None,
    }
}

fn user_record(name: &str, admin: bool) -> UserRecord {
    UserRecord {
        id: UserId::new(),
        name: name.into(),
        password_hash: SecretString::new("$argon2id$fake"),
        policy: UserPolicy {
            admin,
            ..Default::default()
        },
    }
}

/// Seed a fresh SqliteStore with representative data touching every table
/// `migrate_sqlite_to_postgres` copies. Returns the store plus a couple of
/// identifiers the test needs afterwards for spot-checks.
async fn seed(store: &SqliteStore) -> (UserId, UserId, MediaId) {
    // ServerConfigStore: system_identity, server_secrets, runtime_config, named_config.
    store.load_or_create_server_id().await.unwrap();
    store
        .load_or_create_secret("image_tag_key", &[0x42; 32])
        .await
        .unwrap();
    store
        .set_runtime_config(&RuntimeConfig {
            server_name: Some("Migrate Test Server".into()),
            login_disclaimer: Some("disclaimer".into()),
            custom_css: Some("body{}".into()),
        })
        .await
        .unwrap();
    store
        .set_named_config("section-a", r#"{"k":"v"}"#)
        .await
        .unwrap();

    // Users (>= 2).
    let user1 = user_record("migrate-user-1", true);
    let user2 = user_record("migrate-user-2", false);
    let uid1 = user1.id;
    let uid2 = user2.id;
    UserStore::create(store, user1).await.unwrap();
    UserStore::create(store, user2).await.unwrap();

    // Tokens (auth_tokens).
    TokenStore::issue(store, uid1, "device-1").await.unwrap();
    TokenStore::issue(store, uid2, "device-2").await.unwrap();

    // Library.
    let lib_wire_id = "deadbeefdeadbeefdeadbeefdeadbeef";
    LibraryStore::upsert_library(
        store,
        "Migrate Library",
        "/media/migrate",
        LibraryKind::Movies,
        lib_wire_id,
    )
    .await
    .unwrap();

    // Media items (several).
    let item1: MediaId = 1;
    let item2: MediaId = 2;
    let item3: MediaId = 3;
    let mut m1 = media_item(item1, "Migrate Movie", MediaKind::Movie);
    m1.metadata.overview = Some("A movie about migrating.".into());
    m1.metadata.community_rating = Some(8.5);
    MediaStoreExt::put(store, m1).await;
    MediaStoreExt::put(
        store,
        media_item(item2, "Migrate Episode", MediaKind::Episode),
    )
    .await;
    MediaStoreExt::put(store, media_item(item3, "Migrate Audio", MediaKind::Audio)).await;
    LibraryStore::backfill_library_ids(store).await.unwrap();

    // Scan run (scan_runs + scan-state columns on media_items).
    let root = std::path::Path::new("/media/migrate");
    let scan_id = pharos_core::MediaStore::begin_scan(store, root)
        .await
        .unwrap();
    pharos_core::MediaStore::mark_seen(store, item1, scan_id, 1_700_000_000, 1024)
        .await
        .unwrap();
    pharos_core::MediaStore::finish_scan(store, scan_id, 1, 0)
        .await
        .unwrap();

    // Genres / people / studios / tags + item_* associations.
    GenreStore::link_item_genres(store, item1, &["Sci-Fi".to_string()])
        .await
        .unwrap();
    TagStore::link_item_tags(store, item1, &["migrate-tag".to_string()])
        .await
        .unwrap();
    let person = PersonRef {
        name: "Migrate Actor".into(),
        kind: PersonKind::Actor,
        ..Default::default()
    };
    PersonStore::link_item_people(store, item1, std::slice::from_ref(&person))
        .await
        .unwrap();
    StudioStore::link_item_studios(store, item1, &["Migrate Studio".to_string()])
        .await
        .unwrap();

    // Collection + collection_items.
    CollectionStore::create_collection(store, "Migrate Box", &[item1, item2])
        .await
        .unwrap();

    // Playlist + playlist_items.
    let owner_id = uid1.0.simple().to_string();
    PlaylistStore::create_playlist(
        store,
        "Migrate Playlist",
        Some(owner_id.as_str()),
        "Video",
        &[item1, item3],
    )
    .await
    .unwrap();

    // user_data.
    let data = UserItemData {
        played: true,
        play_count: 2,
        last_played_position_ticks: 12_345,
        is_favorite: true,
        last_played_at: 1_700_000_500,
        ..Default::default()
    };
    UserDataStore::set_user_data(store, uid1, item1, data)
        .await
        .unwrap();
    UserDataStore::set_user_data(store, uid2, item2, data)
        .await
        .unwrap();

    // user_configuration + display_preferences.
    PreferenceStore::set_user_configuration(store, uid1, r#"{"audio":"en"}"#)
        .await
        .unwrap();
    PreferenceStore::set_display_preferences(store, uid1, "home", "migrate-client", r#"{"x":1}"#)
        .await
        .unwrap();

    // artwork.
    pharos_core::MediaStore::set_artwork(
        store,
        item1,
        "Primary",
        "local",
        "/media/migrate/poster.jpg",
    )
    .await
    .unwrap();

    (uid1, uid2, item1)
}

/// Trivial helper so `MediaStore::put` reads naturally at call sites above
/// without importing the trait name into scope twice.
struct MediaStoreExt;
impl MediaStoreExt {
    async fn put(store: &SqliteStore, item: MediaItem) {
        pharos_core::MediaStore::put(store, item).await.unwrap();
    }
}

/// Create a dedicated, guaranteed-empty target database for the migrate test,
/// isolated from the shared conformance database. Connects to the `postgres`
/// maintenance DB on the same server, drops+recreates `pharos_migrate_roundtrip`,
/// and returns its URL.
async fn fresh_target_db(pg_url: &str) -> String {
    use sqlx::Connection;
    let (base, _shared_db) = pg_url.rsplit_once('/').expect("postgres url has a db path");
    let db = "pharos_migrate_roundtrip";
    let mut admin = sqlx::postgres::PgConnection::connect(&format!("{base}/postgres"))
        .await
        .expect("connect postgres maintenance db");
    // DROP/CREATE DATABASE cannot run inside a transaction — execute directly.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {db}")))
        .execute(&mut admin)
        .await
        .expect("drop stale target db");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&mut admin)
        .await
        .expect("create target db");
    format!("{base}/{db}")
}

#[tokio::test]
async fn migrate_sqlite_to_postgres_round_trip() {
    let Ok(pg_url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!("SKIP migrate_sqlite_to_postgres_round_trip: PHAROS_TEST_POSTGRES_URL unset");
        return;
    };

    let sqlite = SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let (uid1, uid2, item1) = seed(&sqlite).await;
    seed_raw(&sqlite).await;
    UserPrefStore::set_user_pref(&sqlite, uid1, "diagnostics.opt_in", b"true")
        .await
        .unwrap();
    // Tagged tracks, to check the target's language index afterwards.
    let mut tagged = media_item(9_001, "Tagged Movie", MediaKind::Movie);
    tagged.probe.audio_tracks = vec![pharos_core::AudioTrack {
        language: Some("fra".into()),
        ..Default::default()
    }];
    tagged.probe.subtitle_tracks = vec![pharos_core::SubtitleTrack {
        language: Some("eng".into()),
        ..Default::default()
    }];
    pharos_core::MediaStore::put(&sqlite, tagged).await.unwrap();
    // A 2D title with a grouped 3D edition: the edition list and the
    // derived has_3d flag must survive the copy, or the version picker and
    // the Is3D filter lose the 3D rip after a backend move.
    let mut with_3d = media_item(9_002, "Edition Movie", MediaKind::Movie);
    with_3d.probe.alternate_sources = vec![pharos_core::AlternateMediaSource {
        id: "3d-hsbs".into(),
        path: "/media/migrate/9002 - 3D.HSBS.mkv".into(),
        name: Some("3D HSBS".into()),
        video_3d_format: Some(pharos_core::Video3DFormat::HalfSideBySide),
        ..Default::default()
    }];
    pharos_core::MediaStore::put(&sqlite, with_3d)
        .await
        .unwrap();

    // A stored sort name that differs from sort_name(title) must be carried
    // verbatim; a NULL one (not yet backfilled) is derived from the title.
    sqlx::query("UPDATE media_items SET sort_name = 'custom-sort-key' WHERE id = 1")
        .execute(sqlite.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE media_items SET sort_name = NULL WHERE id = 2")
        .execute(sqlite.pool())
        .await
        .unwrap();

    // B1070: a show's metadata row, with a manual match, ratings and an
    // admin edit (overview locked), must move with everything else.
    let show_key = "/media/shows/Migrate Show".to_string();
    pharos_core::SeriesMetadataStore::upsert_series_metadata(
        &sqlite,
        pharos_core::SeriesMetadata {
            series_key: show_key.clone(),
            series_name: "Migrate Show".into(),
            match_provider: Some("tvdb".into()),
            match_external_id: Some("424242".into()),
            match_source: Some("manual".into()),
            match_confidence: Some(0.5),
            metadata_refreshed_at: Some(1_700_000_000),
            overview: Some("From the provider.".into()),
            community_rating: Some(7.5),
            premiere_date: Some(946_684_800),
            official_rating: Some("TV-14".into()),
            original_language: Some("en".into()),
            genres: vec!["Drama".into()],
            studios: vec!["Migrate Studio".into()],
            poster_locator: Some("/cache/show-poster.jpg".into()),
            backdrop_locator: Some("/cache/show-backdrop.jpg".into()),
            match_flag: Some("ambiguous".into()),
            match_runner_up_id: Some("777".into()),
            match_runner_up_score: Some(0.25),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    pharos_core::RatingsStore::set_ratings(
        &sqlite,
        &pharos_core::RatingsTarget::Series(show_key.clone()),
        pharos_core::ExternalRatings {
            imdb: Some(8.5),
            tomatometer: Some(93),
        },
        1_700_000_123,
    )
    .await
    .unwrap();
    pharos_core::MetadataEditStore::edit_metadata(
        &sqlite,
        &pharos_core::EditTarget::Series {
            key: show_key.clone(),
            name: "Migrate Show".into(),
        },
        &pharos_core::MetadataEdit {
            overview: Some(Some("Written by an admin.".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let sqlite_show = pharos_core::SeriesMetadataStore::series_metadata_by_keys(
        &sqlite,
        std::slice::from_ref(&show_key),
    )
    .await
    .unwrap()
    .remove(&show_key)
    .expect("the seeded show has a row");
    assert!(
        sqlite_show
            .locked
            .contains(pharos_core::EditableField::Overview),
        "the seed must lock the edited overview: {sqlite_show:?}"
    );

    // The migrate engine requires an EMPTY target. The shared
    // PHAROS_TEST_POSTGRES_URL database is also written by `backend_conformance`
    // (which runs concurrently under nextest), so migrating into it collides on
    // unique keys (genres, system_identity). Create a dedicated, freshly-dropped
    // database for this test instead.
    let target_url = fresh_target_db(&pg_url).await;
    let postgres = PostgresStore::connect(&target_url)
        .await
        .expect("connect fresh postgres (must be an empty database)");

    let report = pharos_store_sqlx::migrate::migrate_sqlite_to_postgres(&sqlite, &postgres)
        .await
        .expect("migration must succeed");

    let counts: HashMap<String, u64> = report.tables.into_iter().collect();
    // Every copied table is seeded, so a column the copy mishandles shows
    // up in the row comparison below: seed a new table in `seed_raw`.
    let empty: Vec<&str> = pharos_store_sqlx::migrate::COPIED_TABLES
        .iter()
        .copied()
        .filter(|table| {
            *counts
                .get(*table)
                .unwrap_or_else(|| panic!("migration report missing table {table}"))
                == 0
        })
        .collect();
    assert!(empty.is_empty(), "seed these tables: {empty:?}");

    // Independent count verification on both sides.
    for table in counts.keys() {
        let (sqlite_count,): (i64,) =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(sqlite.pool())
                .await
                .unwrap();
        let (pg_count,): (i64,) =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(postgres.pool())
                .await
                .unwrap();
        assert_eq!(
            sqlite_count, pg_count,
            "row count mismatch on {table}: sqlite={sqlite_count} postgres={pg_count}"
        );
    }

    // B1071: every copied table arrives row for row and column for column.
    for table in pharos_store_sqlx::migrate::COPIED_TABLES {
        assert_same_rows(&sqlite, &postgres, table).await;
    }

    // Integrity spot-checks via the real Postgres store trait methods.
    let pg_secret = postgres
        .load_or_create_secret("image_tag_key", &[0x99; 32])
        .await
        .unwrap();
    assert_eq!(
        pg_secret,
        vec![0x42; 32],
        "migrated server secret must survive sqlite -> postgres copy"
    );

    let pg_item = pharos_core::MediaStore::get(&postgres, item1)
        .await
        .unwrap();
    assert_eq!(pg_item.title, "Migrate Movie");

    let pg_editions = pharos_core::MediaStore::get(&postgres, 9_002)
        .await
        .unwrap();
    assert_eq!(
        pg_editions.probe.alternate_sources.len(),
        1,
        "grouped editions must be copied"
    );
    assert_eq!(
        pg_editions.probe.alternate_sources[0].video_3d_format,
        Some(pharos_core::Video3DFormat::HalfSideBySide)
    );
    let (has_3d,): (i64,) = sqlx::query_as("SELECT has_3d FROM media_items WHERE id = 9002")
        .fetch_one(postgres.pool())
        .await
        .unwrap();
    assert_eq!(has_3d, 1, "has_3d must be copied, not reset to its default");
    assert_eq!(
        pg_item.metadata.overview.as_deref(),
        Some("A movie about migrating.")
    );

    // B1070: the show's row arrives whole, edit and lock included.
    let pg_show = pharos_core::SeriesMetadataStore::series_metadata_by_keys(
        &postgres,
        std::slice::from_ref(&show_key),
    )
    .await
    .unwrap()
    .remove(&show_key)
    .expect("series_metadata must be copied");
    assert_eq!(
        pg_show, sqlite_show,
        "every show field must survive the copy"
    );
    let (fetched_at,): (Option<i64>,) =
        sqlx::query_as("SELECT ratings_fetched_at FROM series_metadata WHERE series_key = $1")
            .bind(&show_key)
            .fetch_one(postgres.pool())
            .await
            .unwrap();
    assert_eq!(
        fetched_at,
        Some(1_700_000_123),
        "the ratings lookup time must move, or every show is looked up again"
    );

    // media_item_languages is derived, not copied: the insert trigger indexes
    // the copied rows, and the language filter finds them.
    let (rows,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM media_item_languages WHERE item_id = 9001")
            .fetch_one(postgres.pool())
            .await
            .unwrap();
    assert_eq!(rows, 2, "the migrated item must be language-indexed");
    let by_language = pharos_core::MediaQuery {
        filters: pharos_core::MediaFilters {
            audio_languages: vec!["fre".into()],
            subtitle_languages: vec!["eng".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let (found, _) = pharos_core::MediaStore::query(&postgres, &by_language)
        .await
        .unwrap();
    assert_eq!(found.iter().map(|i| i.id).collect::<Vec<_>>(), [9_001]);

    let pg_sort_names: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT id, sort_name FROM media_items WHERE id IN (1, 2) ORDER BY id")
            .fetch_all(postgres.pool())
            .await
            .unwrap();
    assert_eq!(
        pg_sort_names,
        vec![
            (1, Some("custom-sort-key".to_string())),
            (2, Some(pharos_core::sort_name("Migrate Episode"))),
        ]
    );

    let genre_item_ids = GenreStore::item_ids_for_genre(&postgres, &genre_wire_id("Sci-Fi"))
        .await
        .unwrap();
    assert!(
        genre_item_ids.contains(&item1),
        "migrated genre link must resolve on postgres"
    );

    let got_data1 = UserDataStore::get_user_data(&postgres, uid1, item1)
        .await
        .unwrap();
    assert!(got_data1.played, "migrated user_data must round-trip");
    assert_eq!(got_data1.play_count, 2);

    let pg_user2 = UserStore::get(&postgres, uid2).await.unwrap();
    assert_eq!(pg_user2.name, "migrate-user-2");

    let pref = UserPrefStore::get_user_pref(&postgres, uid1, "diagnostics.opt_in")
        .await
        .unwrap();
    assert_eq!(
        pref.as_deref(),
        Some(b"true".as_slice()),
        "migrated user pref must read back through the postgres UserPrefStore"
    );
}

/// One value as text, read by its postgres column's type on both sides, so
/// the two backends' rows compare directly. Floats compare at postgres
/// `real` precision.
fn render_sqlite(row: &sqlx::sqlite::SqliteRow, i: usize, pg_type: &str) -> String {
    use sqlx::Row;
    match pg_type {
        "bigint" | "integer" | "smallint" => {
            format!("{:?}", row.try_get_unchecked::<Option<i64>, _>(i).unwrap())
        }
        "boolean" => format!(
            "{:?}",
            row.try_get_unchecked::<Option<i64>, _>(i)
                .unwrap()
                .map(|v| v != 0)
        ),
        #[allow(clippy::cast_possible_truncation)]
        "real" | "double precision" => format!(
            "{:?}",
            row.try_get_unchecked::<Option<f64>, _>(i)
                .unwrap()
                .map(|v| v as f32)
        ),
        "bytea" => format!(
            "{:?}",
            row.try_get_unchecked::<Option<Vec<u8>>, _>(i).unwrap()
        ),
        _ => format!(
            "{:?}",
            row.try_get_unchecked::<Option<String>, _>(i).unwrap()
        ),
    }
}

fn render_pg(row: &sqlx::postgres::PgRow, i: usize, pg_type: &str) -> String {
    use sqlx::Row;
    match pg_type {
        "bigint" | "integer" | "smallint" => {
            format!("{:?}", row.try_get::<Option<i64>, _>(i).unwrap())
        }
        "boolean" => format!("{:?}", row.try_get::<Option<bool>, _>(i).unwrap()),
        #[allow(clippy::cast_possible_truncation)]
        "real" | "double precision" => format!(
            "{:?}",
            row.try_get::<Option<f64>, _>(i).unwrap().map(|v| v as f32)
        ),
        "bytea" => format!("{:?}", row.try_get::<Option<Vec<u8>>, _>(i).unwrap()),
        _ => format!("{:?}", row.try_get::<Option<String>, _>(i).unwrap()),
    }
}

/// Every row of `table` on both sides, compared over every SQLite column.
/// The one value the copy changes on purpose, a missing media sort name
/// (derived from the title), is left out and checked on its own.
async fn assert_same_rows(sqlite: &SqliteStore, postgres: &PostgresStore, table: &str) {
    let cols: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .bind(table)
            .fetch_all(sqlite.pool())
            .await
            .unwrap();
    let pg_types: HashMap<String, String> = sqlx::query_as::<_, (String, String)>(
        "SELECT column_name::text, data_type::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1",
    )
    .bind(table)
    .fetch_all(postgres.pool())
    .await
    .unwrap()
    .into_iter()
    .collect();
    let cols: Vec<(String, String)> = cols
        .into_iter()
        .map(|(c,)| c)
        .filter(|c| !(table == "media_items" && c == "sort_name"))
        .map(|c| {
            let t = pg_types
                .get(&c)
                .unwrap_or_else(|| panic!("postgres has no column {table}.{c}"))
                .clone();
            (c, t)
        })
        .collect();
    let sqlite_list = cols
        .iter()
        .map(|(c, _)| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    // Postgres integers of every width decode as i64 and floats as f64.
    let pg_list = cols
        .iter()
        .map(|(c, t)| match t.as_str() {
            "integer" | "smallint" => format!("\"{c}\"::bigint"),
            "real" => format!("\"{c}\"::float8"),
            _ => format!("\"{c}\""),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut left: Vec<Vec<String>> = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {sqlite_list} FROM \"{table}\""
    )))
    .fetch_all(sqlite.pool())
    .await
    .unwrap()
    .iter()
    .map(|r| {
        cols.iter()
            .enumerate()
            .map(|(i, (_, t))| render_sqlite(r, i, t))
            .collect()
    })
    .collect();
    let mut right: Vec<Vec<String>> = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {pg_list} FROM \"{table}\""
    )))
    .fetch_all(postgres.pool())
    .await
    .unwrap()
    .iter()
    .map(|r| {
        cols.iter()
            .enumerate()
            .map(|(i, (_, t))| render_pg(r, i, t))
            .collect()
    })
    .collect();
    left.sort();
    right.sort();
    for (l, r) in left.iter().zip(&right) {
        for (i, (c, _)) in cols.iter().enumerate() {
            assert_eq!(l[i], r[i], "{table}.{c} differs after the copy");
        }
    }
    assert_eq!(left.len(), right.len(), "{table} row count differs");
}

/// B1071: rows for the tables the store traits make awkward to reach from
/// here, and values for columns the earlier hand-kept copy lists dropped,
/// so the row comparison sees each one carried.
async fn seed_raw(store: &SqliteStore) {
    for sql in [
        "INSERT INTO series_overrides (series_folder, path, target_key, target_name, \
         target_year, source, created_by, created_at) VALUES \
         ('/media/shows/Cosmos', NULL, 'tvdb:1', 'Cosmos', 1980, 'admin', 'ali', 1700000000)",
        "INSERT INTO person_details (wire_id, biography, birth_date, death_date, birth_place, \
         imdb_id, wikidata_id, fetched_at) VALUES \
         ('p1', 'Bio.', '1950-01-02', NULL, 'Cardiff', 'nm1', 'Q1', 1700000001)",
        "INSERT INTO metadata_translations (owner_kind, owner_key, language, title, overview, \
         is_original, fetched_at) VALUES ('item', '1', 'fr', 'Le Film', 'Un film.', 1, 1700000002)",
        "INSERT INTO sync_groups (group_id, epoch_unix_ms, state_json, updated_at) VALUES \
         ('g1', 1700000003000, '{\"state\":\"paused\"}', 1700000003)",
        "INSERT INTO read_positions (user_id, item_id, locator, fraction, updated_at) \
         SELECT id, 1, 'epubcfi(/6/4)', 0.25, 1700000004 FROM users LIMIT 1",
        "INSERT INTO episode_fingerprints (item_id, kind, points, schema_version) VALUES \
         (2, 'intro', X'01020304', 3)",
        "INSERT INTO media_segments (item_id, kind, start_ms, end_ms, detector, confidence, \
         schema_version) VALUES (2, 'Intro', 1000, 61000, 'chromaprint', 0.875, 4)",
        "INSERT INTO media_segment_scans (item_id, schema_version) VALUES (2, 4)",
        "INSERT INTO media_segments_snapshot (label, item_id, kind, start_ms, end_ms, detector, \
         confidence, schema_version) VALUES ('before', 2, 'Outro', 5, 9, 'blackframe', 0.5, 2)",
        "INSERT INTO subtitle_styling (item_id, source_index, source_mtime, source_size, \
         sidecar_mtime, sidecar_size, director_version, outcome, plan_json, styled_at) VALUES \
         (1, 2, 1700000005, 4096, 1700000006, 512, 7, 'styled', '{}', 1700000007)",
        "INSERT INTO transcode_sessions (play_session_id, media_id, decision_json, \
         source_probe_json, updated_at, burn_subtitle_indices_json, holder_user_id, \
         holder_token_id) VALUES ('ps1', 1, '{}', '{}', 1700000008, '[3]', 'u1', 't1')",
        "UPDATE media_items SET track_number = 3, disc_number = 1, release_year = 1979, \
         production_locations_json = '[\"UK\"]', trailers_json = '[]', synopsis = 'Long.', \
         content_rating = 'R', network = 'BBC', release_date = 300000000, has_primary_art = 1, \
         match_provider = 'tmdb', match_external_id = '348', match_source = 'search', \
         match_confidence = 0.75, metadata_refreshed_at = 1700000009, original_language = 'en', \
         art_version = 2, book_format = 'epub', book_page_count = 320, book_author = 'A', \
         book_publisher = 'P', book_series = 'S', book_series_index = 2, book_isbn = '978', \
         imdb_rating = 8.5, rt_tomatometer = 93, ratings_fetched_at = 1700000010, \
         series_key = 'show-key', series_reconcile_pending = 1700000011 WHERE id = 1",
        "UPDATE users SET policy_json = '{\"IsAdministrator\":true}', \
         failed_login_attempts = 2",
    ] {
        sqlx::query(sql)
            .execute(store.pool())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
