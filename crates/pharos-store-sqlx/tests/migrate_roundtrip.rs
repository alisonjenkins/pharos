//! TDD round-trip test for `pharos_store_sqlx::migrate::migrate_sqlite_to_postgres`
//! (the `pharos admin db-migrate` engine). Seeds a SqliteStore touching every
//! copied table via the real store trait methods (same pattern as
//! `backend_conformance.rs`), migrates into a fresh empty Postgres, then
//! verifies both the returned report and independent row counts on both
//! sides, plus a few read-back integrity spot-checks through the Postgres
//! store trait methods.

#![cfg(all(feature = "sqlite", feature = "postgres"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{
    genre_wire_id, CollectionStore, GenreStore, LibraryKind, LibraryStore, MediaId, MediaItem,
    MediaKind, MediaMetadata, MediaProbe, PersonKind, PersonRef, PersonStore, PlaylistStore,
    PreferenceStore, SecretString, StudioStore, TagStore, TokenStore, UserDataStore, UserId,
    UserItemData, UserPolicy, UserRecord, UserStore,
};
use pharos_store_sqlx::postgres::PostgresStore;
use pharos_store_sqlx::sqlite::SqliteStore;
use pharos_store_sqlx::{RuntimeConfig, ServerConfigStore};
use std::collections::HashMap;

fn media_item(id: MediaId, title: &str, kind: MediaKind) -> MediaItem {
    MediaItem {
        id,
        path: format!("/media/migrate/{id}.mkv").into(),
        title: title.into(),
        kind,
        probe: MediaProbe::default(),
        series: None,
        created_at: Some(1_700_000_000 + id as i64),
        metadata: MediaMetadata::default(),
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
    // ServerConfigStore: system_identity, runtime_config, named_config.
    store.load_or_create_server_id().await.unwrap();
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
    sqlx::query(&format!("DROP DATABASE IF EXISTS {db}"))
        .execute(&mut admin)
        .await
        .expect("drop stale target db");
    sqlx::query(&format!("CREATE DATABASE {db}"))
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
    for table in [
        "users",
        "libraries",
        "genres",
        "people",
        "studios",
        "tags",
        "collections",
        "playlists",
        "media_items",
        "system_identity",
        "runtime_config",
        "named_config",
        "scan_runs",
        "auth_tokens",
        "user_data",
        "user_configuration",
        "display_preferences",
        "artwork",
        "item_genres",
        "item_people",
        "item_studios",
        "item_tags",
        "collection_items",
        "playlist_items",
    ] {
        let n = *counts
            .get(table)
            .unwrap_or_else(|| panic!("migration report missing table {table}"));
        assert!(
            n > 0,
            "table {table} must have copied a non-zero row count, got {n}"
        );
    }

    // Independent count verification on both sides.
    for table in counts.keys() {
        let (sqlite_count,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(sqlite.pool())
            .await
            .unwrap();
        let (pg_count,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(postgres.pool())
            .await
            .unwrap();
        assert_eq!(
            sqlite_count, pg_count,
            "row count mismatch on {table}: sqlite={sqlite_count} postgres={pg_count}"
        );
    }

    // Integrity spot-checks via the real Postgres store trait methods.
    let pg_item = pharos_core::MediaStore::get(&postgres, item1)
        .await
        .unwrap();
    assert_eq!(pg_item.title, "Migrate Movie");
    assert_eq!(
        pg_item.metadata.overview.as_deref(),
        Some("A movie about migrating.")
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
}
