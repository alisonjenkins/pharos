//! Shared "backend conformance" suite: one sequential walk over EVERY
//! store trait's contract, run against both backends (SQLite always,
//! Postgres when `PHAROS_TEST_POSTGRES_URL` is set + the `postgres`
//! feature is enabled). Proves parity — the same assertions, genuinely
//! exercised against real queries on both engines, not just "it compiles".
//!
//! Kept ONE sequential test function (no parallel DB access within a run)
//! so failures are easy to localize to the exact operation that diverged.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{
    collection_wire_id, genre_wire_id, person_wire_id, studio_wire_id, tag_wire_id,
    CollectionStore, GenreStore, LibraryKind, LibraryStore, MediaId, MediaItem, MediaKind,
    MediaMetadata, MediaProbe, MediaQuery, MediaStore, PersistedSyncGroup,
    PersistedTranscodeSession, PersonKind, PersonRef, PersonStore, PlaylistStore, PreferenceStore,
    SecretString, StudioStore, SyncGroupStore, TagStore, TokenStore, TranscodeSessionStore,
    UserDataStore, UserId, UserItemData, UserPolicy, UserRecord, UserStore,
};
use pharos_store_sqlx::RuntimeConfig;

/// A minimal-but-valid MediaItem satisfying every NOT NULL column, mirroring
/// the helper shape used by `tests/media_query.rs`.
fn media_item(id: MediaId, title: &str) -> MediaItem {
    MediaItem {
        id,
        path: format!("/media/conformance/{id}.mkv").into(),
        title: title.into(),
        kind: MediaKind::Movie,
        probe: MediaProbe::default(),
        series: None,
        created_at: Some(1_700_000_000 + id as i64),
        metadata: MediaMetadata::default(),
        has_primary_art: false,
        match_provider: None,
        match_external_id: None,
        match_source: None,
        match_confidence: None,
        metadata_refreshed_at: None,
    }
}

fn user_record(name: &str) -> UserRecord {
    UserRecord {
        id: UserId::new(),
        name: name.into(),
        password_hash: SecretString::new("$argon2id$fake"),
        policy: UserPolicy::default(),
    }
}

async fn run_conformance<S>(store: S)
where
    S: pharos_core::MediaStore
        + pharos_core::UserStore
        + pharos_core::TokenStore
        + pharos_core::UserDataStore
        + pharos_core::PreferenceStore
        + pharos_core::GenreStore
        + pharos_core::TagStore
        + pharos_core::PersonStore
        + pharos_core::StudioStore
        + pharos_core::CollectionStore
        + pharos_core::PlaylistStore
        + pharos_core::LibraryStore
        + pharos_core::TranscodeSessionStore
        + pharos_core::SyncGroupStore
        + pharos_store_sqlx::ServerConfigStore
        + Clone
        + Send
        + Sync,
{
    // -----------------------------------------------------------------
    // 1. ServerConfigStore
    // -----------------------------------------------------------------
    let server_id_1 = store.load_or_create_server_id().await.unwrap();
    assert!(!server_id_1.is_empty());
    let server_id_2 = store.load_or_create_server_id().await.unwrap();
    assert_eq!(
        server_id_1, server_id_2,
        "server id must be stable across calls"
    );

    let default_runtime = store.load_runtime_config().await.unwrap();
    assert_eq!(default_runtime, RuntimeConfig::default());
    let rc = RuntimeConfig {
        server_name: Some("Conformance Server".into()),
        login_disclaimer: Some("disclaimer text".into()),
        custom_css: Some("body{}".into()),
    };
    store.set_runtime_config(&rc).await.unwrap();
    let loaded_rc = store.load_runtime_config().await.unwrap();
    assert_eq!(loaded_rc, rc);

    assert!(store
        .load_named_config("nonexistent-key")
        .await
        .unwrap()
        .is_none());
    store
        .set_named_config("section-a", r#"{"k":"v"}"#)
        .await
        .unwrap();
    assert_eq!(
        store
            .load_named_config("section-a")
            .await
            .unwrap()
            .as_deref(),
        Some(r#"{"k":"v"}"#)
    );

    // -----------------------------------------------------------------
    // 2. UserStore
    // -----------------------------------------------------------------
    let user = user_record("conformance-user");
    let uid = user.id;
    UserStore::create(&store, user.clone()).await.unwrap();
    let got = UserStore::get(&store, uid).await.unwrap();
    assert_eq!(got.id, uid);
    assert_eq!(got.name, "conformance-user");
    assert!(!got.policy.admin);

    let listed = UserStore::list(&store).await.unwrap();
    assert!(listed.iter().any(|u| u.id == uid));

    UserStore::set_policy(
        &store,
        uid,
        UserPolicy {
            admin: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let got_admin = UserStore::get(&store, uid).await.unwrap();
    assert!(got_admin.policy.admin, "set_policy must flip admin flag");

    // -----------------------------------------------------------------
    // 3. TokenStore
    // -----------------------------------------------------------------
    let token = TokenStore::issue(&store, uid, "conformance-device")
        .await
        .unwrap();
    let resolved = TokenStore::resolve(&store, token.0.expose()).await.unwrap();
    assert_eq!(resolved, uid);

    let tokens = TokenStore::tokens_for(&store, uid).await.unwrap();
    assert!(tokens.iter().any(|t| t.device_id == "conformance-device"));

    let revoked = TokenStore::revoke_tokens_by_device(&store, uid, "conformance-device")
        .await
        .unwrap();
    assert_eq!(
        revoked, 1,
        "revoke_tokens_by_device must report 1 row dropped"
    );
    assert!(
        TokenStore::resolve(&store, token.0.expose()).await.is_err(),
        "resolving a device-revoked token must fail"
    );

    // Issue + revoke-by-value a fresh token.
    let token2 = TokenStore::issue(&store, uid, "device-2").await.unwrap();
    TokenStore::resolve(&store, token2.0.expose())
        .await
        .unwrap();
    TokenStore::revoke(&store, token2.0.expose()).await.unwrap();
    assert!(
        TokenStore::resolve(&store, token2.0.expose())
            .await
            .is_err(),
        "resolving a value-revoked token must fail"
    );

    // -----------------------------------------------------------------
    // 4. MediaStore
    // -----------------------------------------------------------------
    let item_id: MediaId = 1;
    let mut item = media_item(item_id, "The Conformance Movie");
    item.metadata.overview = Some("A movie about proving parity.".into());
    MediaStore::put(&store, item.clone()).await.unwrap();

    let got_item = MediaStore::get(&store, item_id).await.unwrap();
    // created_at is server-stamped on first insert but we supplied it
    // explicitly, so the round-trip must be exact.
    assert_eq!(got_item, item);

    let listed_items = MediaStore::list(&store).await.unwrap();
    assert!(listed_items.iter().any(|i| i.id == item_id));

    let q = MediaQuery::default();
    let (page, total) = MediaStore::query(&store, &q).await.unwrap();
    assert!(total >= 1);
    assert!(page.iter().any(|i| i.id == item_id));

    let search_q = pharos_core::SearchQuery {
        term: "Conformance".into(),
        kinds: Vec::new(),
        limit: 10,
        offset: 0,
    };
    let (hits, hit_total) = MediaStore::search(&store, &search_q).await.unwrap();
    assert!(hit_total >= 1);
    assert!(hits.iter().any(|i| i.id == item_id));

    // begin_scan -> mark_seen -> sweep_unseen basic cycle.
    let root = std::path::Path::new("/media/conformance");
    let scan_id = MediaStore::begin_scan(&store, root).await.unwrap();
    MediaStore::mark_seen(&store, item_id, scan_id, 1_700_000_000, 1024)
        .await
        .unwrap();
    let state = MediaStore::scan_state(&store, item_id).await.unwrap();
    assert!(state.is_some(), "mark_seen must persist a scan_state row");

    // Sweep this scan run under the root: item_id was marked seen by
    // scan_id, so it must survive; a second item that is never marked
    // must be swept.
    let stray_id: MediaId = 2;
    MediaStore::put(&store, media_item(stray_id, "Stray Unseen Item"))
        .await
        .unwrap();
    let swept = MediaStore::sweep_unseen(&store, scan_id, "/media/conformance")
        .await
        .unwrap();
    assert!(
        swept.contains(&stray_id),
        "sweep_unseen must remove the unmarked item"
    );
    assert!(
        !swept.contains(&item_id),
        "sweep_unseen must not remove the marked item"
    );
    MediaStore::finish_scan(&store, scan_id, 1, 1)
        .await
        .unwrap();

    // -----------------------------------------------------------------
    // 5. UserDataStore
    // -----------------------------------------------------------------
    let data = UserItemData {
        played: true,
        play_count: 3,
        last_played_position_ticks: 12_345,
        is_favorite: true,
        last_played_at: 1_700_000_500,
    };
    UserDataStore::set_user_data(&store, uid, item_id, data)
        .await
        .unwrap();
    let got_data = UserDataStore::get_user_data(&store, uid, item_id)
        .await
        .unwrap();
    assert_eq!(got_data, data);

    // -----------------------------------------------------------------
    // 6. PreferenceStore
    // -----------------------------------------------------------------
    assert!(PreferenceStore::get_user_configuration(&store, uid)
        .await
        .unwrap()
        .is_none());
    PreferenceStore::set_user_configuration(&store, uid, r#"{"audio":"en"}"#)
        .await
        .unwrap();
    assert_eq!(
        PreferenceStore::get_user_configuration(&store, uid)
            .await
            .unwrap()
            .as_deref(),
        Some(r#"{"audio":"en"}"#)
    );

    PreferenceStore::set_display_preferences(
        &store,
        uid,
        "home",
        "conformance-client",
        r#"{"x":1}"#,
    )
    .await
    .unwrap();
    assert_eq!(
        PreferenceStore::get_display_preferences(&store, uid, "home", "conformance-client")
            .await
            .unwrap()
            .as_deref(),
        Some(r#"{"x":1}"#)
    );

    // -----------------------------------------------------------------
    // 7. GenreStore / TagStore / PersonStore / StudioStore
    // -----------------------------------------------------------------
    GenreStore::link_item_genres(&store, item_id, &["Sci-Fi".to_string()])
        .await
        .unwrap();
    let genre_counts = GenreStore::genres_with_counts(&store).await.unwrap();
    assert!(genre_counts
        .iter()
        .any(|g| g.genre.name == "Sci-Fi" && g.item_count >= 1));
    let genre_item_ids = GenreStore::item_ids_for_genre(&store, &genre_wire_id("Sci-Fi"))
        .await
        .unwrap();
    assert!(genre_item_ids.contains(&item_id));

    TagStore::link_item_tags(&store, item_id, &["conformance-tag".to_string()])
        .await
        .unwrap();
    let tag_counts = TagStore::tags_with_counts(&store).await.unwrap();
    assert!(tag_counts
        .iter()
        .any(|t| t.tag.name == "conformance-tag" && t.item_count >= 1));
    let tag_item_ids = TagStore::item_ids_for_tag(&store, &tag_wire_id("conformance-tag"))
        .await
        .unwrap();
    assert!(tag_item_ids.contains(&item_id));
    let tags_for_item = TagStore::tags_for_item(&store, item_id).await.unwrap();
    assert!(tags_for_item.iter().any(|t| t.name == "conformance-tag"));

    let person = PersonRef {
        name: "Conformance Actor".into(),
        kind: PersonKind::Actor,
        ..Default::default()
    };
    PersonStore::link_item_people(&store, item_id, std::slice::from_ref(&person))
        .await
        .unwrap();
    let person_counts = PersonStore::people_with_counts(&store).await.unwrap();
    assert!(person_counts
        .iter()
        .any(|p| p.person.name == "Conformance Actor" && p.item_count >= 1));
    let person_item_ids =
        PersonStore::item_ids_for_person(&store, &person_wire_id("Conformance Actor"))
            .await
            .unwrap();
    assert!(person_item_ids.contains(&item_id));
    let people_for_item = PersonStore::people_for_item(&store, item_id).await.unwrap();
    assert!(people_for_item
        .iter()
        .any(|p| p.name == "Conformance Actor"));

    StudioStore::link_item_studios(&store, item_id, &["Conformance Studio".to_string()])
        .await
        .unwrap();
    let studio_counts = StudioStore::studios_with_counts(&store).await.unwrap();
    assert!(studio_counts
        .iter()
        .any(|s| s.studio.name == "Conformance Studio" && s.item_count >= 1));
    let studio_item_ids =
        StudioStore::item_ids_for_studio(&store, &studio_wire_id("Conformance Studio"))
            .await
            .unwrap();
    assert!(studio_item_ids.contains(&item_id));
    let studios_for_item = StudioStore::studios_for_item(&store, item_id)
        .await
        .unwrap();
    assert!(studios_for_item
        .iter()
        .any(|s| s.name == "Conformance Studio"));

    // -----------------------------------------------------------------
    // 8. LibraryStore
    // -----------------------------------------------------------------
    let lib_wire_id = "deadbeefdeadbeefdeadbeefdeadbeef";
    LibraryStore::upsert_library(
        &store,
        "Conformance Library",
        "/media/conformance",
        LibraryKind::Movies,
        lib_wire_id,
    )
    .await
    .unwrap();
    let libraries = LibraryStore::libraries(&store).await.unwrap();
    assert!(libraries
        .iter()
        .any(|l| l.wire_id == lib_wire_id && l.name == "Conformance Library"));
    let assigned = LibraryStore::backfill_library_ids(&store).await.unwrap();
    assert!(
        assigned >= 1,
        "backfill must assign at least the conformance item"
    );
    let lib_item_ids = LibraryStore::item_ids_for_library(&store, lib_wire_id)
        .await
        .unwrap();
    assert!(lib_item_ids.contains(&item_id));

    // -----------------------------------------------------------------
    // 9. CollectionStore / PlaylistStore
    // -----------------------------------------------------------------
    let collection = CollectionStore::create_collection(&store, "Conformance Box", &[item_id])
        .await
        .unwrap();
    assert_eq!(collection.wire_id, collection_wire_id("Conformance Box"));
    let coll_counts = CollectionStore::collections_with_counts(&store)
        .await
        .unwrap();
    assert!(coll_counts
        .iter()
        .any(|c| c.collection.name == "Conformance Box" && c.item_count == 1));
    let coll_by_wire = CollectionStore::collection_by_wire_id(&store, &collection.wire_id)
        .await
        .unwrap();
    assert!(coll_by_wire.is_some());
    let coll_items = CollectionStore::collection_items(&store, &collection.wire_id)
        .await
        .unwrap();
    assert_eq!(coll_items, vec![item_id]);

    let owner_id = uid.0.simple().to_string();
    let playlist = PlaylistStore::create_playlist(
        &store,
        "Conformance Playlist",
        Some(owner_id.as_str()),
        "Video",
        &[item_id],
    )
    .await
    .unwrap();
    assert_eq!(playlist.name, "Conformance Playlist");
    let playlist_by_wire = PlaylistStore::playlist_by_wire_id(&store, &playlist.wire_id)
        .await
        .unwrap();
    assert!(playlist_by_wire.is_some());
    let entries = PlaylistStore::playlist_entries(&store, &playlist.wire_id)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].item_id, item_id);
    let owned = PlaylistStore::playlists_for_owner(&store, Some(owner_id.as_str()))
        .await
        .unwrap();
    assert!(owned.iter().any(|p| p.wire_id == playlist.wire_id));

    // -----------------------------------------------------------------
    // 10. TranscodeSessionStore (Phase B1 failover breadcrumb)
    // -----------------------------------------------------------------
    let psid = "conformance-play-session";
    assert!(
        TranscodeSessionStore::get_transcode_session(&store, psid)
            .await
            .unwrap()
            .is_none(),
        "unknown play session must be None"
    );
    let sess = PersistedTranscodeSession {
        media_id: item_id,
        decision_json: r#"{"Transcode":{"target_container":"mp4"}}"#.into(),
        source_probe_json: r#"{"container":"mkv"}"#.into(),
    };
    TranscodeSessionStore::upsert_transcode_session(&store, psid, &sess, 100)
        .await
        .unwrap();
    let got = TranscodeSessionStore::get_transcode_session(&store, psid)
        .await
        .unwrap()
        .expect("session must round-trip");
    assert_eq!(got, sess);

    // Upsert overwrites payload + bumps updated_at.
    let sess2 = PersistedTranscodeSession {
        media_id: item_id,
        decision_json: r#"{"DirectPlay":null}"#.into(),
        source_probe_json: r#"{"container":"mp4"}"#.into(),
    };
    TranscodeSessionStore::upsert_transcode_session(&store, psid, &sess2, 200)
        .await
        .unwrap();
    assert_eq!(
        TranscodeSessionStore::get_transcode_session(&store, psid)
            .await
            .unwrap()
            .unwrap(),
        sess2,
        "upsert must overwrite an existing play session"
    );

    // Prune below the row's updated_at (200) is a no-op; above it removes.
    let pruned_none = TranscodeSessionStore::prune_transcode_sessions(&store, 150)
        .await
        .unwrap();
    assert_eq!(
        pruned_none, 0,
        "prune cutoff below updated_at removes nothing"
    );
    assert!(TranscodeSessionStore::get_transcode_session(&store, psid)
        .await
        .unwrap()
        .is_some());
    let pruned = TranscodeSessionStore::prune_transcode_sessions(&store, 300)
        .await
        .unwrap();
    assert_eq!(pruned, 1, "prune cutoff above updated_at removes the row");
    assert!(TranscodeSessionStore::get_transcode_session(&store, psid)
        .await
        .unwrap()
        .is_none());

    // Explicit remove path (re-insert, then delete).
    TranscodeSessionStore::upsert_transcode_session(&store, psid, &sess, 400)
        .await
        .unwrap();
    TranscodeSessionStore::remove_transcode_session(&store, psid)
        .await
        .unwrap();
    assert!(TranscodeSessionStore::get_transcode_session(&store, psid)
        .await
        .unwrap()
        .is_none());

    // -----------------------------------------------------------------
    // 11. SyncGroupStore (Phase B4 group-survives-deploy snapshot)
    // -----------------------------------------------------------------
    let gid = "conformance-sync-group";
    assert!(
        SyncGroupStore::get_sync_group(&store, gid)
            .await
            .unwrap()
            .is_none(),
        "unknown sync group must be None"
    );
    assert!(
        SyncGroupStore::list_sync_groups(&store)
            .await
            .unwrap()
            .iter()
            .all(|g| g.group_id != gid),
        "unknown group must not appear in the list"
    );
    let group = PersistedSyncGroup {
        group_id: gid.to_string(),
        epoch_unix_ms: 1_700_000_000_000,
        state_json: r#"{"leader":"m1","playback":"idle"}"#.into(),
        updated_at: 100,
    };
    SyncGroupStore::upsert_sync_group(&store, &group, 100)
        .await
        .unwrap();
    let got = SyncGroupStore::get_sync_group(&store, gid)
        .await
        .unwrap()
        .expect("group must round-trip");
    assert_eq!(got, group);
    assert!(
        SyncGroupStore::list_sync_groups(&store)
            .await
            .unwrap()
            .iter()
            .any(|g| g.group_id == gid && g.epoch_unix_ms == 1_700_000_000_000),
        "persisted group must appear in the list with its epoch"
    );

    // Upsert overwrites the blob + epoch + bumps updated_at.
    let group2 = PersistedSyncGroup {
        group_id: gid.to_string(),
        epoch_unix_ms: 1_700_000_500_000,
        state_json: r#"{"leader":"m2","playback":{"playing":{"position_ms":42}}}"#.into(),
        updated_at: 200,
    };
    SyncGroupStore::upsert_sync_group(&store, &group2, 200)
        .await
        .unwrap();
    assert_eq!(
        SyncGroupStore::get_sync_group(&store, gid)
            .await
            .unwrap()
            .unwrap(),
        group2,
        "upsert must overwrite an existing group snapshot"
    );

    // Prune below the row's updated_at (200) is a no-op; above it removes.
    let pruned_none = SyncGroupStore::prune_sync_groups(&store, 150)
        .await
        .unwrap();
    assert_eq!(
        pruned_none, 0,
        "prune cutoff below updated_at removes nothing"
    );
    assert!(SyncGroupStore::get_sync_group(&store, gid)
        .await
        .unwrap()
        .is_some());
    let pruned = SyncGroupStore::prune_sync_groups(&store, 300)
        .await
        .unwrap();
    assert_eq!(pruned, 1, "prune cutoff above updated_at removes the row");
    assert!(SyncGroupStore::get_sync_group(&store, gid)
        .await
        .unwrap()
        .is_none());

    // Explicit remove path (re-insert, then delete).
    SyncGroupStore::upsert_sync_group(&store, &group, 400)
        .await
        .unwrap();
    SyncGroupStore::remove_sync_group(&store, gid)
        .await
        .unwrap();
    assert!(SyncGroupStore::get_sync_group(&store, gid)
        .await
        .unwrap()
        .is_none());
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_conformance() {
    let s = pharos_store_sqlx::sqlite::SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    run_conformance(s).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_conformance() {
    let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!("SKIP postgres_conformance: PHAROS_TEST_POSTGRES_URL unset");
        return;
    };
    let p = pharos_store_sqlx::postgres::PostgresStore::connect(&url)
        .await
        .expect("connect postgres");
    run_conformance(p).await;
}
