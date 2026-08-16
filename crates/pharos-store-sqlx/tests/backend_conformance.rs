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
    CollectionStore, DetectedSegment, GenreStore, LibraryKind, LibraryStore, MediaId, MediaItem,
    MediaKind, MediaMetadata, MediaProbe, MediaQuery, MediaSegmentKind, MediaSegmentStore,
    MediaStore, PersistedSyncGroup, PersistedTranscodeSession, PersonKind, PersonRef, PersonStore,
    PlaylistStore, PreferenceStore, SecretString, SeriesInfo, SeriesMetadata, SeriesMetadataStore,
    StudioStore, SyncGroupStore, TagStore, TokenStore, TranscodeSessionStore, UserDataStore,
    UserId, UserItemData, UserPolicy, UserRecord, UserStore, SEGMENT_SCHEMA_VERSION,
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
        + pharos_core::SeriesMetadataStore
        + pharos_core::MediaSegmentStore
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

    // 004-books (T070) — reading order, asserted on BOTH backends because that
    // is the whole reason the column expression is what it is. SQLite sorts
    // NULLs FIRST in ASC and Postgres sorts them LAST, so a bare
    // `book_series_index` would put the unnumbered volume ahead of book one on
    // one engine and behind it on the other: the same query, two orders,
    // neither of them wrong-looking. The store coalesces NULL to a sentinel so
    // "unknown is not first" holds everywhere (migration 0053).
    for (id, title, index) in [
        (9001 as MediaId, "Second", Some(2u32)),
        (9002, "Unnumbered", None),
        (9003, "First", Some(1)),
    ] {
        let mut book = media_item(id, title);
        book.kind = MediaKind::Book;
        book.path = format!("/media/conformance/books/{title}.epub").into();
        book.book = Some(pharos_core::BookMeta {
            format: pharos_core::BookFormat::Epub,
            series_name: Some("Conformance Chronicles".into()),
            series_index: index,
            ..Default::default()
        });
        MediaStore::put(&store, book).await.unwrap();
    }
    let reading_order = MediaQuery {
        kinds: vec![MediaKind::Book],
        sort: vec![
            (pharos_core::SortKey::BookSeries, pharos_core::SortDir::Asc),
            (
                pharos_core::SortKey::BookSeriesIndex,
                pharos_core::SortDir::Asc,
            ),
            (pharos_core::SortKey::Name, pharos_core::SortDir::Asc),
        ],
        ..Default::default()
    };
    let (books, _) = MediaStore::query(&store, &reading_order).await.unwrap();
    // Filtered to THIS series: the postgres backend runs against a shared
    // database that other tests have already written books into, so asserting
    // the whole list would fail on leftovers rather than on ordering.
    let titles: Vec<&str> = books
        .iter()
        .filter(|i| {
            i.book.as_ref().and_then(|b| b.series_name.as_deref()) == Some("Conformance Chronicles")
        })
        .map(|i| i.title.as_str())
        .collect();
    assert_eq!(
        titles,
        ["First", "Second", "Unnumbered"],
        "an unnumbered volume sorts LAST within its series on every backend"
    );

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

    // T117 — the BATCHED mark, which is the path a real scan actually takes.
    //
    // `mark_seen_batch` had a sqlite override and no postgres one, so on the
    // deployment every scan fell back to the trait default: one autocommit
    // UPDATE per file, 13k transactions and 13k WAL flushes for a 13k-file
    // library, on the same device the segment cache reads from. Nothing in the
    // suite called it, so a broken batch query would have compiled, passed
    // `just test`, and only failed in production.
    let batch_root = std::path::Path::new("/media/conformance-batch");
    let batch_scan = MediaStore::begin_scan(&store, batch_root).await.unwrap();
    let a: MediaId = 900;
    let b: MediaId = 901;
    let never_marked: MediaId = 902;
    for (id, title) in [
        (a, "Batched A"),
        (b, "Batched B"),
        (never_marked, "Batched C"),
    ] {
        let mut it = media_item(id, title);
        it.path = format!("/media/conformance-batch/{id}.mkv").into();
        MediaStore::put(&store, it).await.unwrap();
    }
    MediaStore::mark_seen_batch(
        &store,
        &[(a, 1_700_000_100, 4096), (b, 1_700_000_200, 8192)],
        batch_scan,
    )
    .await
    .unwrap();

    // Every row in the batch must carry ITS OWN mtime/size, not the last one's
    // — a positional UNNEST that mismatched its arrays would still "work" here
    // if the values were not checked per row.
    let sa = MediaStore::scan_state(&store, a).await.unwrap().unwrap();
    let sb = MediaStore::scan_state(&store, b).await.unwrap().unwrap();
    assert_eq!(sa.file_mtime, 1_700_000_100, "row A keeps its own mtime");
    assert_eq!(sa.file_size, 4096, "row A keeps its own size");
    assert_eq!(sb.file_mtime, 1_700_000_200, "row B keeps its own mtime");
    assert_eq!(sb.file_size, 8192, "row B keeps its own size");

    // …and the batch must satisfy the sweep exactly as per-row marks do (V10):
    // a batched mark that did not commit reads as UNSEEN and the row is deleted.
    let swept = MediaStore::sweep_unseen(&store, batch_scan, "/media/conformance-batch")
        .await
        .unwrap();
    assert!(
        swept.contains(&never_marked),
        "an item absent from the batch must still be swept"
    );
    assert!(
        !swept.contains(&a) && !swept.contains(&b),
        "a batched mark must protect its rows from the sweep: swept {swept:?}"
    );
    MediaStore::finish_scan(&store, batch_scan, 2, 1)
        .await
        .unwrap();

    // An empty batch is a no-op, not an error: a scan whose final flush has
    // nothing left must not fail.
    MediaStore::mark_seen_batch(&store, &[], batch_scan)
        .await
        .expect("an empty batch is a no-op");

    // audio_items_needing_art — the album-art pass's eligibility query. Exercised
    // here rather than only in a unit test because sqlx checks neither
    // placeholder arity nor column names at compile time, so a `?` vs `$1` slip
    // or a boolean-literal difference between the engines only surfaces against
    // a real database.
    let track_id: MediaId = 3;
    let mut track = media_item(track_id, "A Coverless Track");
    track.kind = MediaKind::Audio;
    track.probe.album = Some("Conformance Album".into());
    track.probe.album_artist = Some("The Conformance Band".into());
    MediaStore::put(&store, track.clone()).await.unwrap();

    // The `match_external_id` a miss reached by the current lookup carries.
    const MISS_MARKER: &str = "miss-v2";
    let needing = MediaStore::audio_items_needing_art(&store, 10, 1_800_000_000, MISS_MARKER)
        .await
        .unwrap();
    assert!(
        needing.iter().any(|i| i.id == track_id),
        "a coverless audio track must be eligible for an album-art lookup"
    );
    assert!(
        needing.iter().all(|i| i.kind == MediaKind::Audio),
        "the query must not return movies or episodes"
    );

    // A cached cover flips has_primary_art, which must drop the track out —
    // otherwise every track of a resolved album re-runs a rate-limited search.
    MediaStore::set_artwork(
        &store,
        track_id,
        "Primary",
        "musicbrainz",
        "/cache/primary/audio/3.jpg",
    )
    .await
    .unwrap();
    assert!(
        MediaStore::get(&store, track_id)
            .await
            .unwrap()
            .has_primary_art,
        "a downloaded musicbrainz Primary is item-servable"
    );
    let after_art = MediaStore::audio_items_needing_art(&store, 10, 1_800_000_000, MISS_MARKER)
        .await
        .unwrap();
    assert!(
        !after_art.iter().any(|i| i.id == track_id),
        "a track that now has art must leave the eligible set"
    );

    // clear_provider_artwork — the escape hatch for a matcher whose PICKS were
    // wrong rather than whose data changed. Exercised here because it is two
    // statements against real tables and its ordering matters: the items must
    // be reset while the artwork rows still say which items they were.
    assert!(
        MediaStore::get(&store, track_id)
            .await
            .unwrap()
            .has_primary_art,
        "precondition: the track has provider art"
    );
    let cleared = MediaStore::clear_provider_artwork(&store, "musicbrainz")
        .await
        .unwrap();
    assert!(cleared >= 1, "the covered track must be reset");
    let after_clear = MediaStore::get(&store, track_id).await.unwrap();
    assert!(
        !after_clear.has_primary_art,
        "clearing provider art must drop the denormalised flag"
    );
    assert_eq!(after_clear.match_source, None, "and re-open the match");
    assert!(
        MediaStore::audio_items_needing_art(&store, 10, 1_800_000_000, MISS_MARKER)
            .await
            .unwrap()
            .iter()
            .any(|i| i.id == track_id),
        "a cleared track must be eligible again"
    );
    // Art from another provider is untouched — this clears one provider's
    // picks, not the artwork table.
    MediaStore::set_artwork(&store, track_id, "Backdrop", "tmdb", "/cache/b.jpg")
        .await
        .unwrap();
    MediaStore::clear_provider_artwork(&store, "musicbrainz")
        .await
        .unwrap();
    assert!(
        MediaStore::artwork_for(&store, track_id)
            .await
            .unwrap()
            .iter()
            .any(|(_, source, _)| source == "tmdb"),
        "another provider's artwork must survive"
    );

    // A stamped match state also removes it, so an unmatched album is not
    // re-searched on every pass.
    let unmatched_id: MediaId = 4;
    let mut unmatched = media_item(unmatched_id, "An Unmatchable Track");
    unmatched.kind = MediaKind::Audio;
    MediaStore::put(&store, unmatched).await.unwrap();
    MediaStore::set_item_match(
        &store,
        unmatched_id,
        "musicbrainz",
        MISS_MARKER,
        "none",
        None,
        1_900_000_000,
    )
    .await
    .unwrap();
    let after_stamp = MediaStore::audio_items_needing_art(&store, 10, 1_800_000_000, MISS_MARKER)
        .await
        .unwrap();
    assert!(
        !after_stamp.iter().any(|i| i.id == unmatched_id),
        "a freshly stamped track waits for the TTL, not the next pass"
    );

    // ...unless the verdict came from an OLDER query version. Otherwise an
    // improvement to the lookup does nothing for a month on exactly the albums
    // it fixes.
    // An unversioned stamp (what the very first release wrote) counts as
    // stale too — those are precisely the rows a bump needs to reach.
    MediaStore::set_item_match(
        &store,
        unmatched_id,
        "musicbrainz",
        "",
        "none",
        None,
        1_900_000_000,
    )
    .await
    .unwrap();
    assert!(
        MediaStore::audio_items_needing_art(&store, 10, 1_800_000_000, MISS_MARKER)
            .await
            .unwrap()
            .iter()
            .any(|i| i.id == unmatched_id),
        "an unversioned miss must be re-admitted"
    );
    MediaStore::set_item_match(
        &store,
        unmatched_id,
        "musicbrainz",
        "miss-v1",
        "none",
        None,
        1_900_000_000,
    )
    .await
    .unwrap();
    let after_bump = MediaStore::audio_items_needing_art(&store, 10, 1_800_000_000, MISS_MARKER)
        .await
        .unwrap();
    assert!(
        after_bump.iter().any(|i| i.id == unmatched_id),
        "a miss stamped by an older query version must be re-admitted at once"
    );

    // -----------------------------------------------------------------
    // 5. UserDataStore
    // -----------------------------------------------------------------
    let data = UserItemData {
        played: true,
        play_count: 3,
        last_played_position_ticks: 12_345,
        is_favorite: true,
        last_played_at: 1_700_000_500,
        ..Default::default()
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
        // Distinct from sess2's, so the round-trip below proves this column is
        // actually stored and re-read rather than defaulted.
        burn_subtitle_indices_json: "[3,7]".into(),
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
        burn_subtitle_indices_json: "[]".into(),
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

    // -----------------------------------------------------------------
    // SeriesMetadataStore (T9-series). series_needing_match aggregates
    // media_items.series_year (INT4 on postgres) — this section exercises the
    // exact query that a strict-typed backend rejects if the decode type is
    // wrong, so the two engines are proven at parity, not just "it compiles".
    // -----------------------------------------------------------------
    let mut ep = media_item(MediaId::from(9_100_001u64), "Series Ep");
    ep.kind = MediaKind::Episode;
    ep.series = Some(SeriesInfo {
        series_name: "Conformance Show".into(),
        season_number: Some(1),
        episode_number: Some(1),
        series_folder: Some("/media/conformance/Conformance Show (2001)".into()),
        series_year: Some(2001),
    });
    MediaStore::put(&store, ep).await.unwrap();

    let need = SeriesMetadataStore::series_needing_match(&store, 10, i64::MAX)
        .await
        .unwrap();
    let cand = need
        .iter()
        .find(|c| c.series_key == "/media/conformance/Conformance Show (2001)")
        .expect("the episode's show is eligible for enrichment");
    assert_eq!(cand.series_name, "Conformance Show");
    assert_eq!(cand.series_year, Some(2001));

    SeriesMetadataStore::upsert_series_metadata(
        &store,
        SeriesMetadata {
            series_key: "/media/conformance/Conformance Show (2001)".into(),
            series_name: "Conformance Show".into(),
            match_source: Some("search".into()),
            overview: Some("A show.".into()),
            genres: vec!["Drama".into()],
            metadata_refreshed_at: Some(1_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let got = SeriesMetadataStore::series_metadata_by_keys(
        &store,
        &["/media/conformance/Conformance Show (2001)".to_string()],
    )
    .await
    .unwrap();
    let m = got
        .get("/media/conformance/Conformance Show (2001)")
        .expect("upserted show reads back");
    assert_eq!(m.overview.as_deref(), Some("A show."));
    assert_eq!(m.genres, vec!["Drama".to_string()]);
    // Now enriched within the TTL → no longer eligible.
    let need2 = SeriesMetadataStore::series_needing_match(&store, 10, 500)
        .await
        .unwrap();
    assert!(!need2
        .iter()
        .any(|c| c.series_key == "/media/conformance/Conformance Show (2001)"));

    // -----------------------------------------------------------------
    // MediaSegmentStore (T86 / B123). The scan stamp is what separates
    // "analysed, found nothing" from "never analysed", and it had no
    // conformance coverage at all — its Postgres placeholders were only
    // ever type-checked, never executed.
    // -----------------------------------------------------------------
    let seg_item: MediaId = 951;
    assert_eq!(
        MediaSegmentStore::segment_scan_version(&store, seg_item)
            .await
            .unwrap(),
        None
    );
    MediaSegmentStore::set_media_segments(
        &store,
        seg_item,
        &[DetectedSegment {
            kind: MediaSegmentKind::Intro,
            start_ms: 12_000,
            end_ms: 32_000,
            detector: "chromaprint".into(),
            confidence: 0.75,
        }],
        SEGMENT_SCHEMA_VERSION,
    )
    .await
    .unwrap();
    MediaSegmentStore::set_segment_scan(&store, seg_item, SEGMENT_SCHEMA_VERSION)
        .await
        .unwrap();
    let segs = MediaSegmentStore::media_segments_for(&store, seg_item)
        .await
        .unwrap();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].start_ms, 12_000);
    assert_eq!(
        MediaSegmentStore::segment_scan_version(&store, seg_item)
            .await
            .unwrap(),
        Some(SEGMENT_SCHEMA_VERSION)
    );
    // Re-stamping the same item UPDATES rather than duplicating (the primary
    // key is the item), so a later algorithm version supersedes the old one.
    MediaSegmentStore::set_segment_scan(&store, seg_item, SEGMENT_SCHEMA_VERSION + 7)
        .await
        .unwrap();
    assert_eq!(
        MediaSegmentStore::segment_scan_version(&store, seg_item)
            .await
            .unwrap(),
        Some(SEGMENT_SCHEMA_VERSION + 7)
    );
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
