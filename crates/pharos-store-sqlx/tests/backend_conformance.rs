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
    CollectionStore, DetectedSegment, ExternalRatings, FoldResume, GenreStore, LibraryKind,
    LibraryStore, LocalizedText, MediaId, MediaItem, MediaKind, MediaMetadata, MediaProbe,
    MediaQuery, MediaSegmentKind, MediaSegmentStore, MediaStore, MetadataTranslationStore,
    PersistedSyncGroup, PersistedTranscodeSession, PersonKind, PersonRef, PersonStore,
    PlaylistStore, PreferenceStore, RatingsStore, RatingsTarget, SecretString, SeriesInfo,
    SeriesMetadata, SeriesMetadataStore, StudioStore, SubtitleStyling, SubtitleStylingStore,
    SyncGroupStore, TagStore, TokenStore, TranscodeSessionStore, TranslationOwner, UserDataStore,
    UserId, UserItemData, UserPolicy, UserPrefStore, UserRecord, UserStore, SEGMENT_SCHEMA_VERSION,
};
use pharos_store_sqlx::RuntimeConfig;

/// A minimal-but-valid MediaItem satisfying every NOT NULL column, mirroring
/// the helper shape used by `tests/it_sqlite/media_query.rs`.
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
        edition: None,
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

/// `item` as `MediaStore::query_shallow` returns it: its stream and long
/// text fields empty.
fn shallow_of(mut item: MediaItem) -> MediaItem {
    item.probe.subtitle_tracks = Default::default();
    item.probe.audio_tracks = Default::default();
    item.probe.attachments = Default::default();
    item.probe.chapters = Default::default();
    item.probe.alternate_sources = Default::default();
    item.probe.synopsis = None;
    item.metadata.overview = None;
    item.metadata.tagline = None;
    item.metadata.provider_ids = Default::default();
    item.metadata.production_locations = Vec::new();
    item.metadata.trailers = Vec::new();
    item
}

async fn run_conformance<S>(store: S)
where
    S: pharos_core::MediaStore
        + pharos_core::UserStore
        + pharos_core::TokenStore
        + pharos_core::UserDataStore
        + pharos_core::PreferenceStore
        + pharos_core::UserPrefStore
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
        + pharos_core::RatingsStore
        + pharos_core::MetadataTranslationStore
        + pharos_core::MediaSegmentStore
        + pharos_core::SubtitleStylingStore
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

    let secret_1 = store
        .load_or_create_secret("secret-a", &[1; 32])
        .await
        .unwrap();
    assert_eq!(secret_1, vec![1; 32]);
    let secret_2 = store
        .load_or_create_secret("secret-a", &[2; 32])
        .await
        .unwrap();
    assert_eq!(
        secret_2,
        vec![1; 32],
        "existing secret must win over a later fresh value"
    );
    let secret_other = store
        .load_or_create_secret("secret-b", &[3; 32])
        .await
        .unwrap();
    assert_eq!(secret_other, vec![3; 32]);

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

    // T68 (V577): the failed-login counter counts up, resets, is per user,
    // and a missing user is an error rather than a silent no-op.
    assert_eq!(
        UserStore::record_failed_login(&store, uid).await.unwrap(),
        1
    );
    assert_eq!(
        UserStore::record_failed_login(&store, uid).await.unwrap(),
        2
    );
    UserStore::reset_failed_logins(&store, uid).await.unwrap();
    assert_eq!(
        UserStore::record_failed_login(&store, uid).await.unwrap(),
        1
    );
    UserStore::reset_failed_logins(&store, uid).await.unwrap();
    assert!(matches!(
        UserStore::record_failed_login(&store, UserId::new()).await,
        Err(pharos_core::AuthError::UserNotFound)
    ));
    assert!(matches!(
        UserStore::reset_failed_logins(&store, UserId::new()).await,
        Err(pharos_core::AuthError::UserNotFound)
    ));

    // disable_user sets only is_disabled: the rest of the stored policy
    // survives, it is idempotent, and a missing user is an error.
    UserStore::set_policy(
        &store,
        uid,
        UserPolicy {
            admin: true,
            max_active_sessions: 3,
            blocked_tags: vec!["kept".to_string()],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    UserStore::disable_user(&store, uid).await.unwrap();
    UserStore::disable_user(&store, uid).await.unwrap();
    let disabled = UserStore::get(&store, uid).await.unwrap();
    assert!(
        disabled.policy.is_disabled,
        "disable_user must set the flag"
    );
    assert!(disabled.policy.admin);
    assert_eq!(disabled.policy.max_active_sessions, 3);
    assert_eq!(disabled.policy.blocked_tags, vec!["kept".to_string()]);
    assert!(matches!(
        UserStore::disable_user(&store, UserId::new()).await,
        Err(pharos_core::AuthError::UserNotFound)
    ));
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

    // T68 (V579): the session count is distinct full-scope devices, minus the
    // one asked about; API keys and restricted tokens are not sessions.
    let capped = user_record("session-count-user");
    let cid = capped.id;
    UserStore::create(&store, capped).await.unwrap();
    for device in ["dev-a", "dev-a", "dev-b", "apikey:ci"] {
        TokenStore::issue(&store, cid, device).await.unwrap();
    }
    TokenStore::issue_with_ttl(
        &store,
        cid,
        "cast:Den",
        3600,
        pharos_core::TokenScope::Playback,
    )
    .await
    .unwrap();
    TokenStore::issue_with_ttl(&store, cid, "dev-gone", 0, pharos_core::TokenScope::Full)
        .await
        .unwrap();
    let sessions = |device: &'static str| {
        let store = &store;
        async move {
            TokenStore::count_sessions_excluding_device(store, cid, device)
                .await
                .unwrap()
        }
    };
    assert_eq!(sessions("dev-a").await, 1, "dev-b only");
    assert_eq!(sessions("dev-new").await, 2, "dev-a and dev-b");

    // Login lookup: case-insensitive, every case variant returned, exact first.
    UserStore::create(&store, user_record("Mixed-Case"))
        .await
        .unwrap();
    UserStore::create(&store, user_record("mixed-case"))
        .await
        .unwrap();
    let names = |found: Vec<UserRecord>| found.into_iter().map(|u| u.name).collect::<Vec<_>>();
    assert_eq!(
        names(
            UserStore::lookup_by_name_ignore_case(&store, "MIXED-CASE")
                .await
                .unwrap()
        ),
        ["Mixed-Case", "mixed-case"]
    );
    assert_eq!(
        names(
            UserStore::lookup_by_name_ignore_case(&store, "mixed-case")
                .await
                .unwrap()
        ),
        ["mixed-case", "Mixed-Case"],
        "the exact match comes first"
    );
    assert_eq!(
        names(
            UserStore::lookup_by_name_ignore_case(&store, "CONFORMANCE-USER")
                .await
                .unwrap()
        ),
        ["conformance-user"]
    );
    assert!(UserStore::lookup_by_name_ignore_case(&store, "nobody-here")
        .await
        .unwrap()
        .is_empty());

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
    // A token id stands in for the token and dies with it, and is not the
    // token itself.
    let token2_id = TokenStore::token_id(&store, token2.0.expose());
    assert_ne!(token2_id, token2.0.expose());
    assert_eq!(
        TokenStore::resolve_token_id(&store, &token2_id)
            .await
            .unwrap(),
        uid
    );
    TokenStore::revoke(&store, token2.0.expose()).await.unwrap();
    assert!(
        TokenStore::resolve_token_id(&store, &token2_id)
            .await
            .is_err(),
        "a revoked token's id must not resolve"
    );
    assert!(
        TokenStore::resolve(&store, token2.0.expose())
            .await
            .is_err(),
        "resolving a value-revoked token must fail"
    );

    // revoke_sessions_except spares only `keep`, and only for that user.
    let keep = TokenStore::issue(&store, uid, "keep-device").await.unwrap();
    let other = TokenStore::issue(&store, uid, "other-device")
        .await
        .unwrap();
    let api_key = TokenStore::issue(&store, uid, "apikey:conformance")
        .await
        .unwrap();
    // Only the exact prefix marks a key: the backends must agree that a
    // differently cased lookalike is an ordinary session.
    let lookalike = TokenStore::issue(&store, uid, "APIKEY:conformance")
        .await
        .unwrap();
    let stranger_rec = user_record("conformance-stranger");
    let stranger_id = stranger_rec.id;
    UserStore::create(&store, stranger_rec).await.unwrap();
    let stranger = TokenStore::issue(&store, stranger_id, "stranger-device")
        .await
        .unwrap();
    let dropped = TokenStore::revoke_sessions_except(&store, uid, Some(keep.0.expose()))
        .await
        .unwrap();
    assert_eq!(
        dropped, 2,
        "revoke_sessions_except must drop only the others"
    );
    assert!(TokenStore::resolve(&store, lookalike.0.expose())
        .await
        .is_err());
    TokenStore::resolve(&store, keep.0.expose()).await.unwrap();
    assert!(TokenStore::resolve(&store, other.0.expose()).await.is_err());
    TokenStore::resolve(&store, stranger.0.expose())
        .await
        .expect("another user's token must survive");
    let dropped = TokenStore::revoke_sessions_except(&store, uid, None)
        .await
        .unwrap();
    assert_eq!(dropped, 1, "None keeps nothing");
    assert!(TokenStore::resolve(&store, keep.0.expose()).await.is_err());
    TokenStore::resolve(&store, api_key.0.expose())
        .await
        .expect("an API key is not a session and must survive");

    // B543 / V438 — a child token goes with its parent, whichever way the
    // parent goes; an unrelated token and a stranger's token stay.
    {
        use pharos_core::{TokenId, TokenScope, TokenSelector};
        let child_of = |parent: TokenId| {
            let store = &store;
            async move {
                TokenStore::issue_child_with_ttl(
                    store,
                    uid,
                    "child-device",
                    3600,
                    TokenScope::Media,
                    &parent,
                )
                .await
                .unwrap()
            }
        };
        let live = |secret: &pharos_core::AuthToken| {
            let store = &store;
            let secret = secret.0.expose().to_owned();
            async move { TokenStore::resolve(store, &secret).await.is_ok() }
        };

        // By secret.
        let parent = TokenStore::issue_with_ttl(&store, uid, "p-secret", 3600, TokenScope::Full)
            .await
            .unwrap();
        let child = child_of(parent.id.clone()).await;
        assert_eq!(
            TokenStore::count_live_children(
                &store,
                uid,
                TokenSelector::Token(parent.secret.0.expose())
            )
            .await
            .unwrap(),
            1
        );
        TokenStore::revoke(&store, parent.secret.0.expose())
            .await
            .unwrap();
        assert!(!live(&child.secret).await, "revoke takes the child");

        // By id.
        let parent = TokenStore::issue_with_ttl(&store, uid, "p-id", 3600, TokenScope::Full)
            .await
            .unwrap();
        let child = child_of(parent.id.clone()).await;
        TokenStore::revoke_by_id(&store, &parent.id).await.unwrap();
        assert!(!live(&child.secret).await, "revoke_by_id takes the child");

        // By device; the child's own device id does not match.
        let parent = TokenStore::issue_with_ttl(&store, uid, "p-device", 3600, TokenScope::Full)
            .await
            .unwrap();
        let child = child_of(parent.id.clone()).await;
        assert_eq!(
            TokenStore::count_live_children(&store, uid, TokenSelector::Device("p-device"))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            TokenStore::revoke_tokens_by_device(&store, uid, "p-device")
                .await
                .unwrap(),
            1
        );
        assert!(
            !live(&child.secret).await,
            "revoke_tokens_by_device takes the child"
        );

        // By session sweep; the kept token's own children are not counted.
        let parent = TokenStore::issue_with_ttl(&store, uid, "p-sweep", 3600, TokenScope::Full)
            .await
            .unwrap();
        let child = child_of(parent.id.clone()).await;
        let keep = TokenStore::issue(&store, uid, "p-keep").await.unwrap();
        assert_eq!(
            TokenStore::count_live_children(
                &store,
                uid,
                TokenSelector::AllExcept(Some(keep.0.expose()))
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            TokenStore::count_live_children(
                &store,
                uid,
                TokenSelector::AllExcept(Some(parent.secret.0.expose()))
            )
            .await
            .unwrap(),
            0,
            "the kept parent's child is not counted"
        );
        TokenStore::revoke_sessions_except(&store, uid, Some(keep.0.expose()))
            .await
            .unwrap();
        assert!(!live(&child.secret).await, "the sweep takes the child");
        assert!(live(&keep).await);

        // An expired child is not counted; a missing parent is refused; a
        // child of one parent survives another parent's revoke.
        let parent = TokenStore::issue_with_ttl(&store, uid, "p-exp", 3600, TokenScope::Full)
            .await
            .unwrap();
        let other = TokenStore::issue_with_ttl(&store, uid, "p-other", 3600, TokenScope::Full)
            .await
            .unwrap();
        let expired = TokenStore::issue_child_with_ttl(
            &store,
            uid,
            "child-device",
            -10,
            TokenScope::Media,
            &parent.id,
        )
        .await
        .unwrap();
        let live_child = child_of(other.id.clone()).await;
        assert_eq!(
            TokenStore::count_live_children(
                &store,
                uid,
                TokenSelector::Token(parent.secret.0.expose())
            )
            .await
            .unwrap(),
            0
        );
        TokenStore::revoke(&store, parent.secret.0.expose())
            .await
            .unwrap();
        assert!(!live(&expired.secret).await);
        assert!(
            live(&live_child.secret).await,
            "another parent's child stays"
        );
        assert!(
            TokenStore::issue_child_with_ttl(
                &store,
                uid,
                "child-device",
                60,
                TokenScope::Media,
                &TokenId("nope".into()),
            )
            .await
            .is_err(),
            "a child needs a stored parent"
        );

        // A child never outlives its parent, and an expired or missing
        // parent issues nothing.
        let short = TokenStore::issue_with_ttl(&store, uid, "p-short", 10, TokenScope::Full)
            .await
            .unwrap();
        let clamped = TokenStore::issue_child_with_ttl(
            &store,
            uid,
            "child-device",
            3600,
            TokenScope::Media,
            &short.id,
        )
        .await
        .unwrap();
        assert!(
            clamped.expires_at <= short.expires_at,
            "child {} outlives parent {}",
            clamped.expires_at,
            short.expires_at
        );
        let roomy = TokenStore::issue_child_with_ttl(
            &store,
            uid,
            "child-device",
            5,
            TokenScope::Media,
            &short.id,
        )
        .await
        .unwrap();
        assert!(roomy.expires_at < short.expires_at, "a shorter ttl is kept");
        let gone = TokenStore::issue_with_ttl(&store, uid, "p-gone", -10, TokenScope::Full)
            .await
            .unwrap();
        assert!(matches!(
            TokenStore::issue_child_with_ttl(
                &store,
                uid,
                "child-device",
                60,
                TokenScope::Media,
                &gone.id,
            )
            .await,
            Err(pharos_core::AuthError::InvalidToken)
        ));
        assert!(matches!(
            TokenStore::issue_child_with_ttl(
                &store,
                uid,
                "child-device",
                60,
                TokenScope::Media,
                &TokenId("nope".into()),
            )
            .await,
            Err(pharos_core::AuthError::InvalidToken)
        ));

        // Deleting the user takes parent and child.
        let doomed = user_record("doomed");
        let did = doomed.id;
        UserStore::create(&store, doomed).await.unwrap();
        let p = TokenStore::issue_with_ttl(&store, did, "d", 3600, TokenScope::Full)
            .await
            .unwrap();
        let c = TokenStore::issue_child_with_ttl(
            &store,
            did,
            "child-device",
            60,
            TokenScope::Media,
            &p.id,
        )
        .await
        .unwrap();
        UserStore::delete(&store, did).await.unwrap();
        assert!(!live(&c.secret).await, "user delete takes the child");
        assert!(!live(&p.secret).await);
    }

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

    // B811 / V761 — a batch fetch skips ids with no row and returns the same
    // row `get` does, on both backends (order is covered in it_sqlite).
    let batch = MediaStore::get_many(&store, &[990_999, item_id, 990_998])
        .await
        .unwrap();
    assert_eq!(batch, vec![item.clone()]);
    assert!(MediaStore::get_many(&store, &[]).await.unwrap().is_empty());

    // B831 / V781 — newest-first with the id tiebreak and a limit is answered
    // by the store, and agrees with sorting the full list in memory.
    let mut every = MediaStore::list(&store).await.unwrap();
    every.sort_by(|a, b| {
        b.created_at
            .unwrap_or(i64::MIN)
            .cmp(&a.created_at.unwrap_or(i64::MIN))
            .then(b.id.cmp(&a.id))
    });
    let newest = MediaStore::query(
        &store,
        &MediaQuery {
            limit: Some(3),
            sort: vec![
                (
                    pharos_core::SortKey::DateCreated,
                    pharos_core::SortDir::Desc,
                ),
                (pharos_core::SortKey::Id, pharos_core::SortDir::Desc),
            ],
            ..MediaQuery::default()
        },
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        newest.iter().map(|i| i.id).collect::<Vec<_>>(),
        every.iter().take(3).map(|i| i.id).collect::<Vec<_>>()
    );

    // B257 — the row holding a path is found by the path, whatever its id.
    assert_eq!(
        MediaStore::id_at_path(&store, &item.path).await.unwrap(),
        Some(item_id)
    );
    assert_eq!(
        MediaStore::id_at_path(
            &store,
            std::path::Path::new("/media/conformance/absent.mkv")
        )
        .await
        .unwrap(),
        None
    );

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
    // ADR-0062: a swept item's translation rows go with it; a kept item's stay.
    let stray_owner = TranslationOwner::Item(stray_id);
    let kept_owner = TranslationOwner::Item(item_id);
    let row = LocalizedText {
        language: "eng".into(),
        overview: Some("text".into()),
        ..Default::default()
    };
    for owner in [&stray_owner, &kept_owner] {
        MetadataTranslationStore::upsert_translations(&store, owner, std::slice::from_ref(&row), 1)
            .await
            .unwrap();
    }
    let swept = MediaStore::sweep_unseen(&store, scan_id, "/media/conformance")
        .await
        .unwrap();
    let left = MetadataTranslationStore::translations_for(
        &store,
        &[stray_owner, kept_owner.clone()],
        &["eng".to_string()],
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        left.iter().map(|(o, _)| o.clone()).collect::<Vec<_>>(),
        vec![kept_owner],
        "a swept item's translation rows are cleared"
    );
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

    // A playback report moves position and timestamp only; played, count,
    // favourite and the stored tracks are the report's to leave alone.
    let after_progress = UserDataStore::record_playback_progress(
        &store,
        uid,
        item_id,
        pharos_core::PlaybackProgress {
            position_ticks: 99_000,
            at: 1_700_000_900,
            audio_stream_index: Some(4),
            subtitle_stream_index: None,
            only_if_newer: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        after_progress,
        UserItemData {
            last_played_position_ticks: 99_000,
            last_played_at: 1_700_000_900,
            audio_stream_index: Some(4),
            ..data
        }
    );
    assert_eq!(
        UserDataStore::get_user_data(&store, uid, item_id)
            .await
            .unwrap(),
        after_progress
    );
    // A held report older than the stored row changes nothing.
    let stale = UserDataStore::record_playback_progress(
        &store,
        uid,
        item_id,
        pharos_core::PlaybackProgress {
            position_ticks: 1,
            at: 1_700_000_100,
            audio_stream_index: Some(9),
            subtitle_stream_index: None,
            only_if_newer: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(stale, after_progress);
    // The same second is not newer either.
    let same_second = UserDataStore::record_playback_progress(
        &store,
        uid,
        item_id,
        pharos_core::PlaybackProgress {
            position_ticks: 2,
            at: 1_700_000_900,
            audio_stream_index: None,
            subtitle_stream_index: None,
            only_if_newer: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(same_second, after_progress);
    assert_eq!(
        UserDataStore::get_user_data(&store, uid, item_id)
            .await
            .unwrap(),
        after_progress
    );

    // Folding a copy's row into its item keeps the furthest state of the
    // two: played and favourite stay set, resume follows the later play.
    let copy_id: MediaId = 9_001;
    MediaStore::put(&store, media_item(copy_id, "Copy Of Item"))
        .await
        .unwrap();
    let copy = UserItemData {
        played: false,
        play_count: 1,
        last_played_position_ticks: 7_000,
        is_favorite: false,
        last_played_at: 1_700_001_000,
        ..Default::default()
    };
    UserDataStore::set_user_data(&store, uid, copy_id, copy)
        .await
        .unwrap();
    MediaStore::merge_user_data(&store, copy_id, item_id, FoldResume::Carry)
        .await
        .unwrap();
    MediaStore::merge_user_data(&store, copy_id, item_id, FoldResume::Carry)
        .await
        .unwrap();
    let merged = UserDataStore::get_user_data(&store, uid, item_id)
        .await
        .unwrap();
    assert_eq!(
        (merged.played, merged.play_count, merged.is_favorite),
        (true, 3, true)
    );
    assert_eq!(
        (merged.last_played_position_ticks, merged.last_played_at),
        (7_000, 1_700_001_000)
    );
    // Into a row that does not exist: nothing is written.
    MediaStore::merge_user_data(&store, copy_id, 9_999, FoldResume::Carry)
        .await
        .unwrap();
    // A double episode folded into one of its episodes: its later position
    // is in another timeline, so the item keeps its own.
    let double_id: MediaId = 9_002;
    MediaStore::put(&store, media_item(double_id, "Double Episode"))
        .await
        .unwrap();
    let double = UserItemData {
        play_count: 5,
        last_played_position_ticks: 99_000,
        last_played_at: 1_700_002_000,
        ..Default::default()
    };
    UserDataStore::set_user_data(&store, uid, double_id, double)
        .await
        .unwrap();
    MediaStore::merge_user_data(&store, double_id, item_id, FoldResume::Drop)
        .await
        .unwrap();
    let folded = UserDataStore::get_user_data(&store, uid, item_id)
        .await
        .unwrap();
    assert_eq!(folded.play_count, 5);
    assert_eq!(
        (folded.last_played_position_ticks, folded.last_played_at),
        (7_000, 1_700_001_000),
        "the play time stays the item's own, so a later fold of a copy that \
         shares its timeline still compares against it"
    );
    // Into an item with no state of its own, the position starts at zero.
    let fresh_id: MediaId = 9_003;
    MediaStore::put(&store, media_item(fresh_id, "Fresh Episode"))
        .await
        .unwrap();
    MediaStore::merge_user_data(&store, double_id, fresh_id, FoldResume::Drop)
        .await
        .unwrap();
    let fresh = UserDataStore::get_user_data(&store, uid, fresh_id)
        .await
        .unwrap();
    assert_eq!(
        (
            fresh.play_count,
            fresh.last_played_position_ticks,
            fresh.last_played_at
        ),
        (5, 0, 0)
    );
    // A copy that shares the item's timeline, played before the double,
    // still carries its resume point in.
    let single_copy_id: MediaId = 9_004;
    MediaStore::put(&store, media_item(single_copy_id, "Fresh Episode Copy"))
        .await
        .unwrap();
    let single_copy = UserItemData {
        play_count: 1,
        last_played_position_ticks: 4_000,
        last_played_at: 1_700_001_500,
        ..Default::default()
    };
    UserDataStore::set_user_data(&store, uid, single_copy_id, single_copy)
        .await
        .unwrap();
    MediaStore::merge_user_data(&store, single_copy_id, fresh_id, FoldResume::Carry)
        .await
        .unwrap();
    let carried = UserDataStore::get_user_data(&store, uid, fresh_id)
        .await
        .unwrap();
    assert_eq!(
        (carried.last_played_position_ticks, carried.last_played_at),
        (4_000, 1_700_001_500),
        "a dropped double's play time must not outrank a later carried copy"
    );

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

    // UserPrefStore: absent, set, overwrite, bytes verbatim, per-user, per-key.
    let other = user_record("pref-other");
    let other_id = other.id;
    UserStore::create(&store, other).await.unwrap();
    assert!(
        UserPrefStore::get_user_pref(&store, uid, "diagnostics.opt_in")
            .await
            .unwrap()
            .is_none()
    );
    UserPrefStore::set_user_pref(&store, uid, "diagnostics.opt_in", b"true")
        .await
        .unwrap();
    UserPrefStore::set_user_pref(&store, uid, "diagnostics.opt_in", b"false")
        .await
        .unwrap();
    UserPrefStore::set_user_pref(&store, uid, "binary", &[0, 255, 1])
        .await
        .unwrap();
    assert_eq!(
        UserPrefStore::get_user_pref(&store, uid, "diagnostics.opt_in")
            .await
            .unwrap()
            .as_deref(),
        Some(&b"false"[..]),
        "a second set replaces the first"
    );
    assert_eq!(
        UserPrefStore::get_user_pref(&store, uid, "binary")
            .await
            .unwrap()
            .as_deref(),
        Some(&[0u8, 255, 1][..])
    );
    assert!(
        UserPrefStore::get_user_pref(&store, other_id, "diagnostics.opt_in")
            .await
            .unwrap()
            .is_none(),
        "one user's pref is not another's"
    );
    // One key across users in one read; a user without it is absent.
    UserPrefStore::set_user_pref(&store, other_id, "binary", b"theirs")
        .await
        .unwrap();
    // A persistent postgres keeps earlier runs' rows, so only this run's
    // two users are compared.
    let mut by_key = UserPrefStore::user_prefs_for_key(&store, "binary")
        .await
        .unwrap();
    by_key.retain(|(u, _)| *u == uid || *u == other_id);
    by_key.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        by_key,
        vec![(uid, vec![0u8, 255, 1]), (other_id, b"theirs".to_vec())]
    );
    assert!(
        UserPrefStore::user_prefs_for_key(&store, "diagnostics.opt_in")
            .await
            .unwrap()
            .iter()
            .filter(|(u, _)| *u == uid || *u == other_id)
            .map(|(u, v)| (*u, v.as_slice()))
            .eq([(uid, &b"false"[..])]),
        "only the user who set it is listed, with the latest value"
    );
    // Deleted once: the second delete finds nothing.
    assert!(UserPrefStore::delete_user_pref(&store, other_id, "binary")
        .await
        .unwrap());
    assert!(!UserPrefStore::delete_user_pref(&store, other_id, "binary")
        .await
        .unwrap());
    assert!(UserPrefStore::get_user_pref(&store, other_id, "binary")
        .await
        .unwrap()
        .is_none());

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
    let guest = PersonRef {
        name: "Conformance Guest".into(),
        kind: PersonKind::GuestStar,
        character: Some("Stranger".into()),
        ..Default::default()
    };
    let person = PersonRef {
        character: Some("Hero".into()),
        sort_order: Some(0),
        ..person
    };
    let director = PersonRef {
        name: "Conformance Director".into(),
        kind: PersonKind::Director,
        role: Some("Director".into()),
        ..Default::default()
    };
    let accented = PersonRef {
        name: "Élodie Conformance".into(),
        kind: PersonKind::GuestStar,
        ..Default::default()
    };
    PersonStore::link_item_people(
        &store,
        item_id,
        &[person.clone(), guest, director, accented],
    )
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
    // A show's cast is built from its episodes' actor credits only.
    let credits = PersonStore::actor_credits_for_items(&store, &[item_id])
        .await
        .unwrap();
    assert_eq!(credits.len(), 1, "{credits:?}");
    assert_eq!(credits[0].0, item_id);
    assert_eq!(credits[0].1.name, "Conformance Actor");
    assert_eq!(credits[0].1.character.as_deref(), Some("Hero"));
    assert!(PersonStore::actor_credits_for_items(&store, &[])
        .await
        .unwrap()
        .is_empty());
    // A person's page lists their credits with who they play, guest spots included.
    let guest_credits =
        PersonStore::credits_for_person(&store, &person_wire_id("Conformance Guest"))
            .await
            .unwrap();
    assert_eq!(guest_credits.len(), 1, "{guest_credits:?}");
    assert_eq!(guest_credits[0].0, item_id);
    assert_eq!(guest_credits[0].1.kind, PersonKind::GuestStar);
    assert_eq!(guest_credits[0].1.character.as_deref(), Some("Stranger"));
    assert!(
        PersonStore::credits_for_person(&store, &person_wire_id("Nobody Credited"))
            .await
            .unwrap()
            .is_empty()
    );
    // Actors are found by every word of their name, crew left out.
    let named = |words: &[&str], limit| {
        let words: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
        let store = &store;
        async move {
            PersonStore::actors_named(store, &words, limit)
                .await
                .unwrap()
                .into_iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        named(&["conformance"], 10).await,
        [
            "Conformance Actor",
            "Conformance Guest",
            "Élodie Conformance"
        ],
        "the director is not an actor"
    );
    assert!(
        named(&["élodie"], 10)
            .await
            .contains(&"Élodie Conformance".to_owned()),
        "a capital outside ASCII is still found"
    );
    assert_eq!(named(&["GUEST", "conf"], 10).await, ["Conformance Guest"]);
    assert_eq!(named(&["conformance"], 1).await.len(), 1);
    assert!(
        named(&["conf%"], 10).await.is_empty(),
        "a % is matched literally"
    );
    assert!(named(&[], 10).await.is_empty());
    // A person's provider details are kept, and replaced whole.
    // A fresh id per run, so a persistent postgres starts with none kept.
    let lead = person_wire_id(&format!("Details Probe {}", UserId::new().0.simple()));
    assert_eq!(
        PersonStore::person_details(&store, &lead).await.unwrap(),
        None
    );
    let details = pharos_core::PersonDetails {
        biography: Some("Born by the sea.".into()),
        birth_date: Some("1970-01-02".into()),
        imdb_id: Some("nm0000001".into()),
        wikidata_id: Some("Q42".into()),
        fetched_at: 1_700_000_000,
        ..Default::default()
    };
    PersonStore::put_person_details(&store, &lead, &details)
        .await
        .unwrap();
    assert_eq!(
        PersonStore::person_details(&store, &lead).await.unwrap(),
        Some(details)
    );
    let nothing = pharos_core::PersonDetails {
        fetched_at: 1_800_000_000,
        ..Default::default()
    };
    PersonStore::put_person_details(&store, &lead, &nothing)
        .await
        .unwrap();
    assert_eq!(
        PersonStore::person_details(&store, &lead).await.unwrap(),
        Some(nothing)
    );

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
        assigned.assigned >= 1,
        "backfill must assign at least the conformance item"
    );
    let again = LibraryStore::backfill_library_ids(&store).await.unwrap();
    assert_eq!(
        again.changed, 0,
        "a repeat backfill over an unchanged library rewrites no row"
    );
    let lib_item_ids = LibraryStore::item_ids_for_library(&store, lib_wire_id)
        .await
        .unwrap();
    assert!(lib_item_ids.contains(&item_id));
    assert!(LibraryStore::library_has_items(&store, lib_wire_id)
        .await
        .unwrap());
    assert!(
        !LibraryStore::library_has_items(&store, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap(),
        "an unknown library has no items"
    );
    assert_eq!(
        LibraryStore::library_wire_id_of(&store, item_id)
            .await
            .unwrap()
            .as_deref(),
        Some(lib_wire_id)
    );
    assert_eq!(
        LibraryStore::library_wire_id_of(&store, item_id + 1_000_000)
            .await
            .unwrap(),
        None,
        "an unknown item belongs to no library"
    );

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
        TranscodeSessionStore::get_transcode_session(&store, psid, 0)
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
        holder: Some(pharos_core::SessionHolder {
            user_id: pharos_core::UserId::new(),
            token_id: "holder-token-id".into(),
        }),
    };
    TranscodeSessionStore::upsert_transcode_session(&store, psid, &sess, 100)
        .await
        .unwrap();
    let got = TranscodeSessionStore::get_transcode_session(&store, psid, 0)
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
        // Overwriting with no holder clears it rather than keeping sess's.
        holder: None,
    };
    TranscodeSessionStore::upsert_transcode_session(&store, psid, &sess2, 200)
        .await
        .unwrap();
    assert_eq!(
        TranscodeSessionStore::get_transcode_session(&store, psid, 0)
            .await
            .unwrap()
            .unwrap(),
        sess2,
        "upsert must overwrite an existing play session"
    );
    // A row last written before the caller's cutoff reads as absent.
    assert!(
        TranscodeSessionStore::get_transcode_session(&store, psid, 201)
            .await
            .unwrap()
            .is_none(),
        "a row written before the cutoff must not be returned"
    );
    assert!(
        TranscodeSessionStore::get_transcode_session(&store, psid, 200)
            .await
            .unwrap()
            .is_some()
    );

    // Prune below the row's updated_at (200) is a no-op; above it removes.
    let pruned_none = TranscodeSessionStore::prune_transcode_sessions(&store, 150)
        .await
        .unwrap();
    assert_eq!(
        pruned_none, 0,
        "prune cutoff below updated_at removes nothing"
    );
    assert!(
        TranscodeSessionStore::get_transcode_session(&store, psid, 0)
            .await
            .unwrap()
            .is_some()
    );
    let pruned = TranscodeSessionStore::prune_transcode_sessions(&store, 300)
        .await
        .unwrap();
    assert_eq!(pruned, 1, "prune cutoff above updated_at removes the row");
    assert!(
        TranscodeSessionStore::get_transcode_session(&store, psid, 0)
            .await
            .unwrap()
            .is_none()
    );

    // Explicit remove path (re-insert, then delete).
    TranscodeSessionStore::upsert_transcode_session(&store, psid, &sess, 400)
        .await
        .unwrap();
    TranscodeSessionStore::remove_transcode_session(&store, psid)
        .await
        .unwrap();
    assert!(
        TranscodeSessionStore::get_transcode_session(&store, psid, 0)
            .await
            .unwrap()
            .is_none()
    );

    // -----------------------------------------------------------------
    // 10b. TranscodeSessionStore::trim_transcode_sessions_for_holder (B279)
    // A holder's live rows are bounded in the durable store, not only in
    // one replica's memory. Fresh ids per run: postgres persists across runs.
    // -----------------------------------------------------------------
    let trim_user = UserId::new();
    let other_user = UserId::new();
    let tag = trim_user.0.simple().to_string();
    let row_for = |user: UserId| PersistedTranscodeSession {
        media_id: item_id,
        decision_json: r#"{"Transcode":{"target_container":"mp4"}}"#.into(),
        source_probe_json: r#"{"container":"mkv"}"#.into(),
        burn_subtitle_indices_json: "[]".into(),
        holder: Some(pharos_core::SessionHolder {
            user_id: user,
            token_id: "trim-token-id".into(),
        }),
    };
    let (trim_row, other_row) = (row_for(trim_user), row_for(other_user));
    let trim_ids: Vec<String> = (0..34).map(|i| format!("trim-{tag}-{i:02}")).collect();
    for (i, id) in trim_ids.iter().enumerate() {
        let ts = 1000 + i as i64;
        TranscodeSessionStore::upsert_transcode_session(&store, id, &trim_row, ts)
            .await
            .unwrap();
    }
    for i in 0..3 {
        let id = format!("trim-other-{tag}-{i}");
        TranscodeSessionStore::upsert_transcode_session(&store, &id, &other_row, 1000 + i)
            .await
            .unwrap();
    }
    // Written before the cutoff below: not live, so neither counted nor trimmed.
    let stale_id = format!("trim-stale-{tag}");
    TranscodeSessionStore::upsert_transcode_session(&store, &stale_id, &trim_row, 10)
        .await
        .unwrap();

    let newest = trim_ids.last().unwrap();
    let trimmed = TranscodeSessionStore::trim_transcode_sessions_for_holder(
        &store, trim_user, newest, 32, 500,
    )
    .await
    .unwrap();
    assert_eq!(
        trimmed,
        trim_ids[..2].to_vec(),
        "34 live rows trimmed to 32 must remove exactly the two oldest, oldest first"
    );
    for id in &trim_ids[..2] {
        assert!(
            TranscodeSessionStore::get_transcode_session(&store, id, 0)
                .await
                .unwrap()
                .is_none(),
            "a trimmed row must no longer authorize"
        );
    }
    for id in trim_ids[2..].iter().chain(std::iter::once(&stale_id)) {
        assert!(
            TranscodeSessionStore::get_transcode_session(&store, id, 0)
                .await
                .unwrap()
                .is_some(),
            "an untrimmed row ({id}) must survive the trim"
        );
    }
    for i in 0..3 {
        let id = format!("trim-other-{tag}-{i}");
        assert!(
            TranscodeSessionStore::get_transcode_session(&store, &id, 0)
                .await
                .unwrap()
                .is_some(),
            "another holder's rows must be untouched"
        );
    }
    let again = TranscodeSessionStore::trim_transcode_sessions_for_holder(
        &store, trim_user, newest, 32, 500,
    )
    .await
    .unwrap();
    assert!(again.is_empty(), "a repeated trim must be a no-op");

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
        series_key: None,
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
    // RatingsStore (B341): shows lead the due list, a film carries its IMDb
    // id, and a lookup's scores survive a rescan's put and a re-enrichment's
    // upsert, because neither names the ratings columns.
    // -----------------------------------------------------------------
    let show_key = "/media/conformance/Conformance Show (2001)".to_string();
    let mut film = media_item(9501, "Rated Film");
    film.metadata.production_year = Some(1999);
    film.metadata.provider_ids.imdb = Some("tt0133093".into());
    MediaStore::put(&store, film.clone()).await.unwrap();
    let due = RatingsStore::needing_ratings(&store, 1000, 500)
        .await
        .unwrap();
    let show_at = due
        .iter()
        .position(|d| d.target == RatingsTarget::Series(show_key.clone()))
        .expect("the show is due");
    let film_due = due
        .iter()
        .find(|d| d.target == RatingsTarget::Item(9501))
        .expect("the film is due");
    assert!(
        due.iter()
            .take(show_at)
            .all(|d| matches!(d.target, RatingsTarget::Series(_))),
        "shows come before films"
    );
    assert_eq!(film_due.imdb_id.as_deref(), Some("tt0133093"));
    assert_eq!(film_due.title, "Rated Film");
    assert_eq!(film_due.year, Some(1999));
    let show_due = &due[show_at];
    assert_eq!(show_due.title, "Conformance Show");
    assert_eq!(show_due.year, Some(2001));
    assert_eq!(show_due.imdb_id, None);

    let scores = ExternalRatings {
        imdb: Some(8.7),
        tomatometer: Some(83),
    };
    RatingsStore::set_ratings(&store, &RatingsTarget::Item(9501), scores, 1_000)
        .await
        .unwrap();
    RatingsStore::set_ratings(
        &store,
        &RatingsTarget::Series(show_key.clone()),
        scores,
        1_000,
    )
    .await
    .unwrap();
    let due = RatingsStore::needing_ratings(&store, 1000, 500)
        .await
        .unwrap();
    assert!(!due.iter().any(|d| d.target == RatingsTarget::Item(9501)));
    assert!(!due
        .iter()
        .any(|d| d.target == RatingsTarget::Series(show_key.clone())));
    let due = RatingsStore::needing_ratings(&store, 1000, 2_000)
        .await
        .unwrap();
    assert!(
        due.iter().any(|d| d.target == RatingsTarget::Item(9501)),
        "due again after the TTL"
    );

    MediaStore::put(&store, film).await.unwrap();
    let got = MediaStore::get(&store, 9501).await.unwrap();
    assert_eq!(
        got.metadata.ratings, scores,
        "a rescan's put keeps the scores"
    );
    SeriesMetadataStore::upsert_series_metadata(
        &store,
        SeriesMetadata {
            series_key: show_key.clone(),
            series_name: "Conformance Show".into(),
            match_source: Some("search".into()),
            metadata_refreshed_at: Some(1_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let got = SeriesMetadataStore::series_metadata_by_keys(&store, std::slice::from_ref(&show_key))
        .await
        .unwrap();
    assert_eq!(
        got[&show_key].ratings, scores,
        "a re-enrichment keeps the scores"
    );
    RatingsStore::set_ratings(&store, &RatingsTarget::Item(424_242), scores, 1_000)
        .await
        .unwrap();

    // -----------------------------------------------------------------
    // MetadataTranslationStore (ADR-0062, B564): a matched show and episode
    // are gaps until a row for the language exists; a recorded miss closes
    // the gap until it ages past the cutoff; reads return the asked-for
    // language plus the original.
    // -----------------------------------------------------------------
    // A show key fresh per run (as the audio-analysis section does with its
    // id), so an `eng` row a previous run left in a persistent postgres
    // cannot close this run's gap. The episode keeps its fixed id: it never
    // gets an `eng` row.
    let tr_show_key = format!(
        "/media/conformance/Translation Show {}",
        UserId::new().0.simple()
    );
    SeriesMetadataStore::upsert_series_metadata(
        &store,
        SeriesMetadata {
            series_key: tr_show_key.clone(),
            series_name: "Conformance Show".into(),
            match_provider: Some("tvdb".into()),
            match_external_id: Some("76666".into()),
            match_source: Some("search".into()),
            metadata_refreshed_at: Some(1_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    MediaStore::set_item_match(
        &store,
        9_100_001,
        "tvdb",
        "76666",
        "search",
        Some(0.9),
        1_000,
    )
    .await
    .unwrap();
    let gaps = MetadataTranslationStore::translation_gaps(&store, "tvdb", "eng", 0, 100)
        .await
        .unwrap();
    let show_owner = TranslationOwner::Series(tr_show_key.clone());
    let ep_owner = TranslationOwner::Item(9_100_001);
    let show_gap = gaps
        .iter()
        .find(|g| g.owner == show_owner)
        .expect("a matched show with no row is a gap");
    assert_eq!(show_gap.series_external_id, "76666");
    let ep_gap = gaps
        .iter()
        .find(|g| g.owner == ep_owner)
        .expect("a matched episode with no row is a gap");
    assert_eq!((ep_gap.season, ep_gap.episode), (Some(1), Some(1)));
    assert!(
        gaps.iter().position(|g| g.owner == show_owner)
            < gaps.iter().position(|g| g.owner == ep_owner),
        "shows come before episodes"
    );

    let eng = LocalizedText {
        language: "eng".into(),
        title: Some("Dragon Ball".into()),
        overview: Some("Goku sets out.".into()),
        is_original: false,
    };
    let jpn = LocalizedText {
        language: "jpn".into(),
        title: Some("ドラゴンボール".into()),
        overview: Some("悟空".into()),
        is_original: true,
    };
    let fra_missing = LocalizedText {
        language: "fra".into(),
        ..Default::default()
    };
    MetadataTranslationStore::upsert_translations(
        &store,
        &show_owner,
        &[eng.clone(), jpn.clone(), fra_missing.clone()],
        2_000,
    )
    .await
    .unwrap();
    MetadataTranslationStore::upsert_translations(
        &store,
        &ep_owner,
        std::slice::from_ref(&fra_missing),
        2_000,
    )
    .await
    .unwrap();
    // Upsert replaces in place.
    let eng2 = LocalizedText {
        overview: Some("Goku sets out again.".into()),
        ..eng.clone()
    };
    MetadataTranslationStore::upsert_translations(
        &store,
        &show_owner,
        std::slice::from_ref(&eng2),
        2_500,
    )
    .await
    .unwrap();

    let gaps = MetadataTranslationStore::translation_gaps(&store, "tvdb", "eng", 0, 100)
        .await
        .unwrap();
    assert!(
        !gaps.iter().any(|g| g.owner == show_owner),
        "eng row closes the show's gap"
    );
    assert!(
        gaps.iter().any(|g| g.owner == ep_owner),
        "episode still has no eng row"
    );
    let fra = MetadataTranslationStore::translation_gaps(&store, "tvdb", "fra", 1_000, 100)
        .await
        .unwrap();
    assert!(
        !fra.iter()
            .any(|g| g.owner == ep_owner || g.owner == show_owner),
        "a recent miss is not asked again"
    );
    let fra = MetadataTranslationStore::translation_gaps(&store, "tvdb", "fra", 3_000, 100)
        .await
        .unwrap();
    assert!(
        fra.iter().any(|g| g.owner == ep_owner),
        "a miss older than the cutoff is asked again"
    );

    let mut rows = MetadataTranslationStore::translations_for(
        &store,
        &[show_owner.clone(), ep_owner.clone()],
        &["eng".to_string()],
        true,
    )
    .await
    .unwrap();
    rows.sort_by(|a, b| a.1.language.cmp(&b.1.language));
    assert_eq!(
        rows,
        vec![(show_owner.clone(), eng2), (show_owner.clone(), jpn)],
        "the asked-for language and the original, nothing else"
    );
    let none = MetadataTranslationStore::translations_for(
        &store,
        &[ep_owner],
        &["deu".to_string()],
        false,
    )
    .await
    .unwrap();
    assert!(none.is_empty());
    // More owners than either engine binds in one query: read in chunks, so
    // a whole-library episode list still finds its rows.
    let mut many: Vec<TranslationOwner> = (0..40_000u64)
        .map(|i| TranslationOwner::Series(format!("absent-{i}")))
        .collect();
    many.push(show_owner.clone());
    let found = MetadataTranslationStore::translations_for(&store, &many, &[], true)
        .await
        .unwrap();
    assert_eq!(found.len(), 1, "40k owners: {found:?}");
    assert_eq!(found[0].0, show_owner);

    // -----------------------------------------------------------------
    // SortKey::ParentalRating: the CASE ranking must order identically on
    // both engines — known ratings by maturity (case and surrounding spaces
    // ignored), unknown / empty / NULL lowest, ties by id.
    // -----------------------------------------------------------------
    let rating_rows: [(MediaId, Option<&str>); 8] = [
        (9601, Some("NC-17")),
        (9602, Some(" pg-13 ")),
        (9603, Some("tv-ma")),
        (9604, Some("G")),
        (9605, Some("Not Rated")),
        (9606, Some("")),
        (9607, None),
        (9608, Some("PG-13\n")),
    ];
    for (id, rating) in rating_rows {
        let mut item = media_item(id, &format!("Rating {id}"));
        item.metadata.official_rating = rating.map(str::to_string);
        MediaStore::put(&store, item).await.unwrap();
    }
    for (dir, expected) in [
        (
            pharos_core::SortDir::Asc,
            [9605, 9606, 9607, 9608, 9604, 9602, 9603, 9601],
        ),
        (
            pharos_core::SortDir::Desc,
            [9601, 9603, 9602, 9604, 9605, 9606, 9607, 9608],
        ),
    ] {
        let (items, _) = MediaStore::query(
            &store,
            &MediaQuery {
                sort: vec![(pharos_core::SortKey::ParentalRating, dir)],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let got: Vec<MediaId> = items
            .iter()
            .map(|i| i.id)
            .filter(|id| (9601..=9608).contains(id))
            .collect();
        assert_eq!(got, expected, "ParentalRating {dir:?} order");
    }

    // -----------------------------------------------------------------
    // Tag policy scope (B266): `BlockedTags` hides an item carrying any of
    // them, `AllowedTags` shows only items carrying one; names match without
    // regard to case.
    // -----------------------------------------------------------------
    for (id, tags) in [
        (9701 as MediaId, vec!["Gore"]),
        (9702, vec!["family"]),
        (9703, vec![]),
        (9704, vec!["Family", "gore"]),
    ] {
        MediaStore::put(&store, media_item(id, &format!("Tagged {id}")))
            .await
            .unwrap();
        let tags: Vec<String> = tags.into_iter().map(String::from).collect();
        TagStore::link_item_tags(&store, id, &tags).await.unwrap();
    }
    for (blocked, allowed, expected) in [
        (vec!["GORE"], vec![], vec![9702, 9703]),
        (vec![], vec!["FAMILY"], vec![9702, 9704]),
        (vec!["gore"], vec!["family"], vec![9702]),
    ] {
        let scope = pharos_core::TagScope {
            blocked_lc: blocked.iter().map(|t| t.to_lowercase()).collect(),
            allowed_lc: allowed.iter().map(|t| t.to_lowercase()).collect(),
        };
        let scoped = MediaQuery {
            tag_scope: Some(scope.clone()),
            ..Default::default()
        };
        let (items, _) = MediaStore::query(&store, &scoped).await.unwrap();
        // query_ids answers the same ids in the same order, paged or not.
        // Pinned to this block's ids: other tests write rows into the shared
        // postgres database while this runs.
        let own = MediaQuery {
            filters: pharos_core::MediaFilters {
                ids: (9701..=9704).collect(),
                ids_present: true,
                ..Default::default()
            },
            ..scoped.clone()
        };
        let paged = MediaQuery {
            start_index: 1,
            limit: Some(2),
            ..own.clone()
        };
        let (whole, _) = MediaStore::query(&store, &own).await.unwrap();
        let (page, _) = MediaStore::query(&store, &paged).await.unwrap();
        for (q, rows) in [(&own, &whole), (&paged, &page)] {
            assert_eq!(
                MediaStore::query_ids(&store, q).await.unwrap(),
                rows.iter().map(|i| i.id).collect::<Vec<_>>(),
                "query_ids {scope:?}"
            );
            let (shallow, shallow_total) = MediaStore::query_shallow(&store, q).await.unwrap();
            let (_, total) = MediaStore::query(&store, q).await.unwrap();
            assert_eq!(shallow_total, total, "query_shallow total {scope:?}");
            assert_eq!(
                shallow,
                rows.iter().cloned().map(shallow_of).collect::<Vec<_>>(),
                "query_shallow {scope:?}"
            );
        }
        let mut got: Vec<MediaId> = items
            .iter()
            .map(|i| i.id)
            .filter(|id| (9701..=9704).contains(id))
            .collect();
        got.sort_unstable();
        assert_eq!(got, expected, "{scope:?}");
    }

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

    // B289 — the stored editions read back on their own, and `scan_state`
    // reports whether a row has any so the scanner can skip the read.
    let with_editions: MediaId = 7000;
    let without_editions: MediaId = 7001;
    let editions = vec![pharos_core::AlternateMediaSource {
        id: "3d".into(),
        path: "/media/conformance-editions/Film - 3D.mkv".into(),
        size_bytes: Some(4242),
        ..Default::default()
    }];
    for (id, alts) in [
        (with_editions, editions.clone()),
        (without_editions, Vec::new()),
    ] {
        let mut it = media_item(id, "Editions");
        it.path = format!("/media/conformance-editions/{id}.mkv").into();
        it.probe.alternate_sources = alts;
        MediaStore::put(&store, it).await.unwrap();
    }
    let edition_scan =
        MediaStore::begin_scan(&store, std::path::Path::new("/media/conformance-editions"))
            .await
            .unwrap();
    MediaStore::mark_seen_batch(
        &store,
        &[
            (with_editions, 1_700_000_300, 1),
            (without_editions, 1_700_000_300, 1),
        ],
        edition_scan,
    )
    .await
    .unwrap();
    assert_eq!(
        MediaStore::alternate_sources(&store, with_editions)
            .await
            .unwrap(),
        editions,
        "a row with editions returns them"
    );
    assert!(
        MediaStore::alternate_sources(&store, without_editions)
            .await
            .unwrap()
            .is_empty(),
        "a row without editions returns none"
    );
    assert!(
        MediaStore::alternate_sources(&store, 7999)
            .await
            .unwrap()
            .is_empty(),
        "an absent row returns none"
    );
    let with_state = MediaStore::scan_state(&store, with_editions)
        .await
        .unwrap()
        .unwrap();
    assert!(
        with_state.has_editions,
        "scan_state flags a row with editions"
    );
    let without_state = MediaStore::scan_state(&store, without_editions)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !without_state.has_editions,
        "scan_state does not flag a row without editions"
    );

    // -----------------------------------------------------------------
    // Spec 024 / V385 — audio analysis: written by `put_audio_analysis` only,
    // kept across a re-`put`, valid only while the file signature matches.
    // -----------------------------------------------------------------
    // A fresh id per run (as section 10b does with its tag), so a second run
    // against a persistent postgres does not find last run's analysis. The
    // shift keeps it, and `song + 1`, inside i64.
    let song: MediaId = (UserId::new().0.as_u128() as u64) >> 2;
    let mut item = media_item(song, "song");
    item.kind = MediaKind::Audio;
    item.path = format!("/media/conformance/song-{song}.flac").into();
    MediaStore::put(&store, item.clone()).await.unwrap();
    assert_eq!(
        MediaStore::get(&store, song)
            .await
            .unwrap()
            .probe
            .audio_analysis,
        None,
        "a new song is unanalysed"
    );
    let analysis = pharos_core::AudioAnalysis::from_readings(
        Some((128.5, pharos_core::TempoSource::Tag)),
        Some(0.72),
        1,
    );
    MediaStore::put_audio_analysis(&store, song, analysis, (1_700_000_000, 4_096))
        .await
        .unwrap();
    let got = MediaStore::get(&store, song).await.unwrap().probe;
    assert_eq!(got.audio_analysis, Some(analysis));
    assert_eq!(got.bpm(), Some(128.5));
    assert_eq!(got.tempo_source(), Some(pharos_core::TempoSource::Tag));
    assert_eq!(got.energy(), Some(0.72));
    // The list path decodes the same columns.
    let listed = MediaStore::list(&store).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|i| i.id == song)
            .unwrap()
            .probe
            .audio_analysis,
        Some(analysis)
    );

    // A re-probe `put` carrying no analysis keeps the stored one.
    MediaStore::put(&store, item.clone()).await.unwrap();
    assert_eq!(
        MediaStore::get(&store, song)
            .await
            .unwrap()
            .probe
            .audio_analysis,
        Some(analysis),
        "put never writes the analysis columns"
    );

    // The scan stamps the signature the analysis read: still valid.
    let scan = MediaStore::begin_scan(&store, std::path::Path::new("/media/conformance"))
        .await
        .unwrap();
    MediaStore::mark_seen(&store, song, scan, 1_700_000_000, 4_096)
        .await
        .unwrap();
    assert_eq!(
        MediaStore::get(&store, song)
            .await
            .unwrap()
            .probe
            .audio_analysis,
        Some(analysis)
    );
    // The file changes: the analysis no longer applies.
    MediaStore::mark_seen(&store, song, scan, 1_700_000_060, 4_096)
        .await
        .unwrap();
    assert_eq!(
        MediaStore::get(&store, song)
            .await
            .unwrap()
            .probe
            .audio_analysis,
        None,
        "a changed file reads as unanalysed"
    );

    // "Found nothing" is stored, so the song is not decoded again.
    let nothing = pharos_core::AudioAnalysis {
        version: 1,
        ..Default::default()
    };
    MediaStore::put_audio_analysis(&store, song, nothing, (1_700_000_060, 4_096))
        .await
        .unwrap();
    assert_eq!(
        MediaStore::get(&store, song)
            .await
            .unwrap()
            .probe
            .audio_analysis,
        Some(nothing)
    );
    // An absent row is a no-op.
    MediaStore::put_audio_analysis(&store, song + 1, analysis, (1, 1))
        .await
        .unwrap();

    // -----------------------------------------------------------------
    // Picture check (black bars): written by `put_picture_check` only, kept
    // across a re-`put`, valid only while the file signature matches.
    // -----------------------------------------------------------------
    let episode: MediaId = (UserId::new().0.as_u128() as u64) >> 2;
    let mut item = media_item(episode, "bars");
    item.path = format!("/media/conformance/bars-{episode}.mkv").into();
    MediaStore::put(&store, item.clone()).await.unwrap();
    assert_eq!(
        MediaStore::get(&store, episode)
            .await
            .unwrap()
            .probe
            .picture_check,
        None,
        "new file is unchecked"
    );
    let bars = pharos_core::PictureCheck {
        version: 1,
        area: Some(pharos_core::PictureArea {
            x: 242,
            y: 0,
            width: 1436,
            height: 1080,
        }),
    };
    MediaStore::put_picture_check(&store, episode, bars, (1_700_000_000, 4_096))
        .await
        .unwrap();
    let got = MediaStore::get(&store, episode).await.unwrap().probe;
    assert_eq!(got.picture_check, Some(bars));
    assert_eq!(got.picture_area(), bars.area);
    let listed = MediaStore::list(&store).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|i| i.id == episode)
            .unwrap()
            .probe
            .picture_check,
        Some(bars),
        "the list path decodes the same columns"
    );
    MediaStore::put(&store, item.clone()).await.unwrap();
    assert_eq!(
        MediaStore::get(&store, episode)
            .await
            .unwrap()
            .probe
            .picture_check,
        Some(bars),
        "put never writes the picture columns"
    );
    let scan = MediaStore::begin_scan(&store, std::path::Path::new("/media/conformance"))
        .await
        .unwrap();
    MediaStore::mark_seen(&store, episode, scan, 1_700_000_000, 4_096)
        .await
        .unwrap();
    assert_eq!(
        MediaStore::get(&store, episode)
            .await
            .unwrap()
            .probe
            .picture_check,
        Some(bars)
    );
    MediaStore::mark_seen(&store, episode, scan, 1_700_000_060, 4_096)
        .await
        .unwrap();
    assert_eq!(
        MediaStore::get(&store, episode)
            .await
            .unwrap()
            .probe
            .picture_check,
        None,
        "a changed file reads as unchecked"
    );
    let no_bars = pharos_core::PictureCheck {
        version: 1,
        area: None,
    };
    MediaStore::put_picture_check(&store, episode, no_bars, (1_700_000_060, 4_096))
        .await
        .unwrap();
    let got = MediaStore::get(&store, episode).await.unwrap().probe;
    assert_eq!(got.picture_check, Some(no_bars), "no bars is stored too");
    assert_eq!(got.picture_area(), None);
    MediaStore::put_picture_check(&store, episode + 1, bars, (1, 1))
        .await
        .unwrap();

    // -----------------------------------------------------------------
    // Spec 028 — subtitle styling: one row per item, replaced by a new
    // source choice, plan_json left out of the index, gone with the item.
    // Last, because the cascade check sweeps the whole conformance root.
    // -----------------------------------------------------------------
    let film: MediaId = (UserId::new().0.as_u128() as u64) >> 2;
    MediaStore::put(&store, media_item(film, "styled"))
        .await
        .unwrap();
    assert_eq!(
        SubtitleStylingStore::subtitle_styling(&store, film)
            .await
            .unwrap(),
        None
    );
    let styled = SubtitleStyling {
        item_id: film,
        source_index: 2,
        source_mtime: 1_700_000_000,
        source_size: u64::from(u32::MAX) + 7,
        sidecar_mtime: Some(1_700_000_001),
        sidecar_size: Some(321),
        director_version: 1,
        outcome: "styled".into(),
        plan_json: Some(r#"{"effects":[]}"#.into()),
        styled_at: 1_700_000_100,
    };
    SubtitleStylingStore::put_subtitle_styling(&store, styled.clone())
        .await
        .unwrap();
    assert_eq!(
        SubtitleStylingStore::subtitle_styling(&store, film)
            .await
            .unwrap(),
        Some(styled.clone())
    );
    let index = SubtitleStylingStore::subtitle_styling_index(&store)
        .await
        .unwrap();
    let indexed = index.iter().find(|r| r.item_id == film).unwrap();
    assert_eq!(indexed.plan_json, None, "the index never loads plans");
    assert_eq!(
        SubtitleStyling {
            plan_json: None,
            ..styled.clone()
        },
        *indexed
    );
    // Same key: an upsert.
    let redone = SubtitleStyling {
        director_version: 2,
        styled_at: 1_700_000_200,
        ..styled.clone()
    };
    SubtitleStylingStore::put_subtitle_styling(&store, redone.clone())
        .await
        .unwrap();
    assert_eq!(
        SubtitleStylingStore::subtitle_styling(&store, film)
            .await
            .unwrap(),
        Some(redone)
    );
    // A new source replaces the old row; a version-only row has no plan or sidecar.
    let version_only = SubtitleStyling {
        source_index: 5,
        sidecar_mtime: None,
        sidecar_size: None,
        outcome: "undecodable".into(),
        plan_json: None,
        ..styled
    };
    SubtitleStylingStore::put_subtitle_styling(&store, version_only.clone())
        .await
        .unwrap();
    assert_eq!(
        SubtitleStylingStore::subtitle_styling(&store, film)
            .await
            .unwrap(),
        Some(version_only)
    );
    let index = SubtitleStylingStore::subtitle_styling_index(&store)
        .await
        .unwrap();
    assert_eq!(index.iter().filter(|r| r.item_id == film).count(), 1);
    // Deleting the item cascades.
    let sweep = MediaStore::begin_scan(&store, std::path::Path::new("/media/conformance"))
        .await
        .unwrap();
    let swept = MediaStore::sweep_unseen(&store, sweep, "/media/conformance")
        .await
        .unwrap();
    assert!(swept.contains(&film));
    assert_eq!(
        SubtitleStylingStore::subtitle_styling(&store, film)
            .await
            .unwrap(),
        None
    );
}

/// Spec 033 — the series split's store contract: a put keeps a key the
/// split wrote, the split's writes land, overrides are kept once, and the
/// match pass sees a split folder as two shows.
async fn run_series_identity_conformance<S>(store: S)
where
    S: pharos_core::MediaStore
        + pharos_core::SeriesMetadataStore
        + pharos_core::SeriesIdentityStore
        + pharos_core::MetadataTranslationStore
        + Send
        + Sync,
{
    use pharos_core::{OverrideSource, SeriesKeyUpdate, SeriesOverride};
    const ROOT: &str = "/media/series-identity";
    const FOLDER: &str = "/media/series-identity/The Flash";
    let split = format!("{FOLDER}\u{1f}1990");
    let ep = |id: u64, file: &str, year: Option<u32>| {
        let mut item = media_item(MediaId::from(id), file);
        item.path = format!("{FOLDER}/Season 1/{file}").into();
        item.kind = MediaKind::Episode;
        item.series = Some(SeriesInfo {
            series_name: "The Flash".into(),
            season_number: Some(1),
            episode_number: Some(1),
            series_folder: Some(FOLDER.into()),
            series_year: year,
            series_key: None,
        });
        item
    };
    let old = ep(9_300_001, "The Flash (1990) - S01E01.mkv", None);
    let new = ep(9_300_002, "The Flash (2014) - S01E01.mkv", None);
    MediaStore::put(&store, old.clone()).await.unwrap();
    MediaStore::put(&store, new.clone()).await.unwrap();

    let mut rows = store.series_identity_rows(ROOT).await.unwrap();
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r.series_key.is_none() && r.series_year.is_none()));
    assert!(store
        .series_identity_rows("/media/elsewhere")
        .await
        .unwrap()
        .is_empty());

    let updates = [
        SeriesKeyUpdate {
            path: rows[0].path.clone(),
            series_key: Some(split.clone()),
            series_year: Some(1990),
        },
        SeriesKeyUpdate {
            path: rows[1].path.clone(),
            series_key: None,
            series_year: Some(2014),
        },
    ];
    assert_eq!(store.set_series_keys(&updates).await.unwrap(), 2);
    assert_eq!(
        store.set_series_keys(&updates).await.unwrap(),
        0,
        "idempotent"
    );

    // A re-probe writes no key and the folder's (absent) year; the split's
    // key and stated year stay.
    MediaStore::put(&store, old.clone()).await.unwrap();
    MediaStore::put_batch(&store, std::slice::from_ref(&new))
        .await
        .unwrap();
    let got = MediaStore::get(&store, old.id).await.unwrap();
    let series = got.series.unwrap();
    assert_eq!(series.series_key.as_deref(), Some(split.as_str()));
    assert_eq!(series.series_year, Some(1990));
    let got = MediaStore::get(&store, new.id).await.unwrap();
    assert_eq!(got.series.unwrap().series_year, Some(2014));

    // A moved file (new folder) loses the old folder's split.
    let mut moved = old.clone();
    if let Some(s) = moved.series.as_mut() {
        s.series_folder = Some(format!("{ROOT}/The Flash (1990)"));
        s.series_year = Some(1990);
    }
    MediaStore::put(&store, moved).await.unwrap();
    let got = MediaStore::get(&store, old.id).await.unwrap();
    assert_eq!(got.series.unwrap().series_key, None);
    MediaStore::put(&store, old.clone()).await.unwrap();
    store.set_series_keys(&[updates[0].clone()]).await.unwrap();

    // The match pass sees two shows, each with its own year.
    let need = SeriesMetadataStore::series_needing_match(&store, 1000, i64::MAX)
        .await
        .unwrap();
    let year_of = |key: &str| {
        need.iter()
            .find(|c| c.series_key == key)
            .map(|c| c.series_year)
    };
    assert_eq!(year_of(FOLDER), Some(Some(2014)));
    assert_eq!(year_of(&split), Some(Some(1990)));

    // Overrides: one owner row per folder and source.
    let owner = SeriesOverride {
        id: 0,
        series_folder: FOLDER.into(),
        path: None,
        target_key: FOLDER.into(),
        target_name: None,
        target_year: Some(2014),
        source: OverrideSource::Scanner,
        created_by: None,
        created_at: 1_700_000_000,
    };
    assert!(store.insert_series_override(owner.clone()).await.unwrap());
    assert!(!store
        .insert_series_override(SeriesOverride {
            target_year: Some(1990),
            ..owner.clone()
        })
        .await
        .unwrap());
    assert!(store
        .insert_series_override(SeriesOverride {
            source: OverrideSource::Admin,
            created_by: Some("admin".into()),
            ..owner.clone()
        })
        .await
        .unwrap());
    let all = store.series_overrides(&[FOLDER.to_string()]).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].target_year, Some(2014));
    assert_eq!(all[0].source, OverrideSource::Scanner);
    assert_eq!(all[1].source, OverrideSource::Admin);
    assert!(store.series_overrides(&[]).await.unwrap().is_empty());

    // The stored match's premiere year, by key.
    SeriesMetadataStore::upsert_series_metadata(
        &store,
        pharos_core::SeriesMetadata {
            series_key: FOLDER.into(),
            series_name: "The Flash".into(),
            match_source: Some("search".into()),
            // 2014-10-07T00:00:00Z
            premiere_date: Some(1_412_640_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let years = store
        .series_premiere_years(&[FOLDER.to_string(), split.clone()])
        .await
        .unwrap();
    assert_eq!(years.get(FOLDER), Some(&2014));
    assert_eq!(years.get(&split), None);

    // V892 — an episode waits for its show: eligible only once the show (by
    // its identity key) has a match row.
    let waiting = |items: &[MediaItem]| items.iter().any(|i| i.id == old.id);
    let need = MediaStore::items_needing_match(&store, 1000, i64::MAX)
        .await
        .unwrap();
    assert!(!waiting(&need), "the split 1990 group has no match row yet");
    assert!(
        need.iter().any(|i| i.id == new.id),
        "the folder's show is matched"
    );
    SeriesMetadataStore::upsert_series_metadata(
        &store,
        pharos_core::SeriesMetadata {
            series_key: split.clone(),
            series_name: "The Flash".into(),
            match_source: Some("none".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let need = MediaStore::items_needing_match(&store, 1000, i64::MAX)
        .await
        .unwrap();
    assert!(waiting(&need));

    // V891 — a show's episodes by identity key, the local side of episode fit.
    let eps = store.series_episodes(&split).await.unwrap();
    assert_eq!(
        eps,
        vec![pharos_core::SeriesEpisodeRow {
            id: old.id,
            path: old.path.to_string_lossy().into_owned(),
            season: Some(1),
            episode: Some(1),
            premiere_date: None,
            match_source: None,
            match_provider: None,
            match_external_id: None,
        }]
    );
    let eps = store.series_episodes(FOLDER).await.unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0].path, new.path.to_string_lossy());
    assert!(store.series_episodes("/nowhere").await.unwrap().is_empty());

    // V891 — a flagged match round-trips, and the next upsert clears it.
    let flagged = pharos_core::SeriesMetadata {
        series_key: FOLDER.into(),
        series_name: "The Flash".into(),
        match_source: Some("search".into()),
        match_flag: Some("ambiguous".into()),
        match_runner_up_id: Some("78650".into()),
        match_runner_up_score: Some(0.25),
        ..Default::default()
    };
    SeriesMetadataStore::upsert_series_metadata(&store, flagged.clone())
        .await
        .unwrap();
    let by_key = |m: std::collections::HashMap<String, pharos_core::SeriesMetadata>| {
        m.get(FOLDER).cloned().unwrap()
    };
    let got = by_key(
        SeriesMetadataStore::series_metadata_by_keys(&store, &[FOLDER.to_string()])
            .await
            .unwrap(),
    );
    assert_eq!(got.match_flag.as_deref(), Some("ambiguous"));
    assert_eq!(got.match_runner_up_id.as_deref(), Some("78650"));
    assert_eq!(got.match_runner_up_score, Some(0.25));
    let keys = [FOLDER.to_string(), split.clone()];
    assert_eq!(
        store.ambiguous_series(&keys).await.unwrap(),
        std::collections::HashSet::from([FOLDER.to_string()])
    );
    assert!(store.ambiguous_series(&[]).await.unwrap().is_empty());
    SeriesMetadataStore::upsert_series_metadata(
        &store,
        pharos_core::SeriesMetadata {
            match_flag: None,
            match_runner_up_id: None,
            match_runner_up_score: None,
            ..flagged
        },
    )
    .await
    .unwrap();
    let got = by_key(
        SeriesMetadataStore::series_metadata_by_keys(&store, &[FOLDER.to_string()])
            .await
            .unwrap(),
    );
    assert_eq!(
        (
            got.match_flag,
            got.match_runner_up_id,
            got.match_runner_up_score
        ),
        (None, None, None)
    );
    assert!(store.ambiguous_series(&keys).await.unwrap().is_empty());

    // V940 — episode reconciliation marks, re-admission, and translation deletion.
    assert_eq!(store.mark_series_reconcile(&split, 100).await.unwrap(), 1);
    assert_eq!(
        store.series_reconcile_pending(10).await.unwrap(),
        vec![(old.id, 100)]
    );
    MediaStore::put(&store, old.clone()).await.unwrap();
    assert_eq!(
        store.series_reconcile_pending(10).await.unwrap(),
        vec![(old.id, 100)]
    );
    assert!(!store.clear_series_reconcile(old.id, 99).await.unwrap());
    assert_eq!(
        store.series_reconcile_pending(10).await.unwrap(),
        vec![(old.id, 100)]
    );
    assert!(store.clear_series_reconcile(old.id, 100).await.unwrap());
    assert!(store.series_reconcile_pending(10).await.unwrap().is_empty());

    assert_eq!(store.mark_reconcile_items(&[], 200).await.unwrap(), 0);
    assert_eq!(store.mark_reconcile_items(&[new.id], 200).await.unwrap(), 1);
    assert_eq!(
        store.series_reconcile_pending(10).await.unwrap(),
        vec![(new.id, 200)]
    );
    assert!(store.clear_series_reconcile(new.id, 200).await.unwrap());
    MediaStore::set_item_match(&store, new.id, "tvdb", "279121", "manual", None, 300)
        .await
        .unwrap();
    assert_eq!(store.mark_reconcile_items(&[new.id], 400).await.unwrap(), 0);
    assert!(store.series_reconcile_pending(10).await.unwrap().is_empty());
    let by_path = [
        old.path.to_string_lossy().into_owned(),
        new.path.to_string_lossy().into_owned(),
        "/tv/nowhere.mkv".to_string(),
    ];
    assert_eq!(store.mark_reconcile_paths(&[], 450).await.unwrap(), 0);
    assert_eq!(store.mark_reconcile_paths(&by_path, 500).await.unwrap(), 1);
    assert_eq!(
        store.series_reconcile_pending(10).await.unwrap(),
        vec![(old.id, 500)]
    );
    assert!(store.clear_series_reconcile(old.id, 500).await.unwrap());

    MediaStore::set_artwork(&store, old.id, "Primary", "tvdb", "/cache/old-still.jpg")
        .await
        .unwrap();
    MediaStore::set_artwork(&store, old.id, "Thumb", "local", "/tv/x-thumb.jpg")
        .await
        .unwrap();
    assert_eq!(
        store.delete_provider_artwork(old.id).await.unwrap(),
        vec![("Primary".to_string(), "/cache/old-still.jpg".to_string())]
    );
    assert_eq!(
        MediaStore::artwork_for(&store, old.id).await.unwrap(),
        vec![(
            "Thumb".to_string(),
            "local".to_string(),
            "/tv/x-thumb.jpg".to_string()
        )]
    );

    assert_eq!(store.readmit_series(&[]).await.unwrap(), 0);
    assert_eq!(
        store.readmit_series(&[FOLDER.to_string()]).await.unwrap(),
        1
    );

    let owner = TranslationOwner::Item(old.id);
    MetadataTranslationStore::upsert_translations(
        &store,
        &owner,
        &[LocalizedText {
            language: "eng".into(),
            title: Some("x".into()),
            ..Default::default()
        }],
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        MetadataTranslationStore::delete_translations(&store, &owner)
            .await
            .unwrap(),
        1
    );
    assert!(MetadataTranslationStore::translations_for(
        &store,
        std::slice::from_ref(&owner),
        &["eng".to_string()],
        true,
    )
    .await
    .unwrap()
    .is_empty());

    const SHOW_A: &str = "/tv/Show A";
    const SHOW_A_EP: &str = "/tv/Show A/S01E01.mkv";
    const SHOW_B: &str = "/tv/Show B";
    assert!(store
        .insert_series_override(SeriesOverride {
            id: 0,
            series_folder: SHOW_A.into(),
            path: None,
            target_key: SHOW_A.into(),
            target_name: Some("Show A".into()),
            target_year: Some(2005),
            source: OverrideSource::Scanner,
            created_by: None,
            created_at: 1_700_000_010,
        })
        .await
        .unwrap());
    let first_folder_admin = store
        .replace_admin_override(SeriesOverride {
            id: 0,
            series_folder: SHOW_A.into(),
            path: None,
            target_key: format!("{SHOW_A}\u{1f}2005"),
            target_name: Some("Show A (2005)".into()),
            target_year: Some(2005),
            source: OverrideSource::Scanner,
            created_by: Some("alice".into()),
            created_at: 1_700_000_020,
        })
        .await
        .unwrap();
    let second_folder_admin = store
        .replace_admin_override(SeriesOverride {
            id: 999,
            series_folder: SHOW_A.into(),
            path: None,
            target_key: format!("{SHOW_A}\u{1f}2020"),
            target_name: Some("Show A (2020)".into()),
            target_year: Some(2020),
            source: OverrideSource::Scanner,
            created_by: Some("bob".into()),
            created_at: 1_700_000_030,
        })
        .await
        .unwrap();
    assert_ne!(first_folder_admin, second_folder_admin);

    let all_overrides = store.all_series_overrides().await.unwrap();
    let show_a_folder_admin: Vec<&SeriesOverride> = all_overrides
        .iter()
        .filter(|o| {
            o.series_folder == SHOW_A && o.path.is_none() && o.source == OverrideSource::Admin
        })
        .collect();
    assert_eq!(show_a_folder_admin.len(), 1);
    assert_eq!(show_a_folder_admin[0].id, second_folder_admin);
    assert_eq!(
        show_a_folder_admin[0].target_key,
        format!("{SHOW_A}\u{1f}2020")
    );
    assert!(all_overrides.iter().any(|o| {
        o.series_folder == SHOW_A && o.path.is_none() && o.source == OverrideSource::Scanner
    }));

    store
        .replace_admin_override(SeriesOverride {
            id: 0,
            series_folder: SHOW_A.into(),
            path: Some(SHOW_A_EP.into()),
            target_key: format!("{SHOW_A}\u{1f}2005"),
            target_name: None,
            target_year: Some(2005),
            source: OverrideSource::Admin,
            created_by: Some("alice".into()),
            created_at: 1_700_000_040,
        })
        .await
        .unwrap();
    let path_admin_id = store
        .replace_admin_override(SeriesOverride {
            id: 0,
            series_folder: SHOW_A.into(),
            path: Some(SHOW_A_EP.into()),
            target_key: format!("{SHOW_A}\u{1f}2020"),
            target_name: None,
            target_year: Some(2020),
            source: OverrideSource::Admin,
            created_by: Some("bob".into()),
            created_at: 1_700_000_050,
        })
        .await
        .unwrap();

    let all_overrides = store.all_series_overrides().await.unwrap();
    let show_a_path_admin: Vec<&SeriesOverride> = all_overrides
        .iter()
        .filter(|o| {
            o.series_folder == SHOW_A
                && o.path.as_deref() == Some(SHOW_A_EP)
                && o.source == OverrideSource::Admin
        })
        .collect();
    assert_eq!(show_a_path_admin.len(), 1);
    assert_eq!(show_a_path_admin[0].id, path_admin_id);
    assert_eq!(
        show_a_path_admin[0].target_key,
        format!("{SHOW_A}\u{1f}2020")
    );
    assert!(all_overrides
        .windows(2)
        .all(|w| (w[0].created_at, w[0].id) >= (w[1].created_at, w[1].id)));
    let show_a_times: Vec<i64> = all_overrides
        .iter()
        .filter(|o| o.series_folder == SHOW_A)
        .map(|o| o.created_at)
        .collect();
    assert_eq!(
        show_a_times,
        vec![1_700_000_050, 1_700_000_030, 1_700_000_010]
    );

    let deleted = store
        .delete_series_override(path_admin_id)
        .await
        .unwrap()
        .expect("path override exists");
    assert_eq!(deleted.id, path_admin_id);
    assert_eq!(deleted.path.as_deref(), Some(SHOW_A_EP));
    assert_eq!(
        store.delete_series_override(path_admin_id).await.unwrap(),
        None
    );
    assert!(store
        .all_series_overrides()
        .await
        .unwrap()
        .iter()
        .all(|o| o.id != path_admin_id));

    SeriesMetadataStore::upsert_series_metadata(
        &store,
        pharos_core::SeriesMetadata {
            series_key: SHOW_A.into(),
            series_name: "Show A".into(),
            match_source: Some("search".into()),
            match_flag: Some("ambiguous".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    SeriesMetadataStore::upsert_series_metadata(
        &store,
        pharos_core::SeriesMetadata {
            series_key: SHOW_B.into(),
            series_name: "Show B".into(),
            match_source: Some("search".into()),
            match_flag: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        store.flagged_series_keys().await.unwrap(),
        vec![SHOW_A.to_string()]
    );
    SeriesMetadataStore::upsert_series_metadata(
        &store,
        pharos_core::SeriesMetadata {
            series_key: SHOW_A.into(),
            series_name: "Show A".into(),
            match_source: Some("search".into()),
            match_flag: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let manual_ep = ep(9_300_003, "The Flash - S01E02.mkv", Some(2014));
    let search_ep = ep(9_300_004, "The Flash - S01E03.mkv", Some(2014));
    MediaStore::put(&store, manual_ep.clone()).await.unwrap();
    MediaStore::put(&store, search_ep.clone()).await.unwrap();
    MediaStore::set_item_match(
        &store,
        manual_ep.id,
        "tvdb",
        "123",
        "manual",
        Some(1.0),
        600,
    )
    .await
    .unwrap();
    MediaStore::set_item_match(
        &store,
        search_ep.id,
        "tvdb",
        "456",
        "search",
        Some(0.9),
        600,
    )
    .await
    .unwrap();
    assert!(store.clear_manual_matches(&[]).await.unwrap().is_empty());
    assert_eq!(
        store
            .clear_manual_matches(&[search_ep.id, manual_ep.id])
            .await
            .unwrap(),
        vec![(manual_ep.id, "123".to_string())]
    );
    let got_manual = MediaStore::get(&store, manual_ep.id).await.unwrap();
    assert_eq!(got_manual.match_source, None);
    assert_eq!(got_manual.match_provider, None);
    assert_eq!(got_manual.match_external_id, None);
    assert_eq!(got_manual.match_confidence, None);
    let got_search = MediaStore::get(&store, search_ep.id).await.unwrap();
    assert_eq!(got_search.match_source.as_deref(), Some("search"));
    assert_eq!(got_search.match_provider.as_deref(), Some("tvdb"));
    assert_eq!(got_search.match_external_id.as_deref(), Some("456"));
    assert_eq!(got_search.match_confidence, Some(0.9));

    let sweep = MediaStore::begin_scan(&store, std::path::Path::new(ROOT))
        .await
        .unwrap();
    MediaStore::sweep_unseen(&store, sweep, ROOT).await.unwrap();
}

/// `ReadPositionStore` (023): the contract that keeps a book's reading
/// position apart from its playback state (V354) and drives "Continue
/// reading".
async fn run_read_position_conformance<S>(store: S)
where
    S: pharos_core::MediaStore
        + pharos_core::UserStore
        + pharos_core::UserDataStore
        + pharos_core::ReadPositionStore
        + Send
        + Sync,
{
    use pharos_core::ReadPosition;
    let uid = user_record("reader");
    let uid = {
        let id = uid.id;
        UserStore::create(&store, uid).await.unwrap();
        id
    };
    let other = user_record("other");
    let other = {
        let id = other.id;
        UserStore::create(&store, other).await.unwrap();
        id
    };
    let (a, b, c): (MediaId, MediaId, MediaId) = (8_101, 8_102, 8_103);
    for (id, title) in [(a, "Book A"), (b, "Book B"), (c, "Book C")] {
        MediaStore::put(&store, media_item(id, title))
            .await
            .unwrap();
    }
    let pos = |locator: &str, fraction: f32, updated_at: i64| ReadPosition {
        locator: locator.into(),
        fraction,
        updated_at,
    };

    assert_eq!(store.get_read_position(uid, a).await.unwrap(), None);
    assert_eq!(
        store.read_positions_bulk(uid, &[a, b]).await.unwrap(),
        vec![None, None]
    );
    assert!(store
        .read_positions_bulk(uid, &[])
        .await
        .unwrap()
        .is_empty());

    store
        .set_read_position(uid, a, pos("epub:2:10", 0.25, 100))
        .await
        .unwrap();
    store
        .set_read_position(uid, b, pos("epub:0:0", 0.5, 300))
        .await
        .unwrap();
    store
        .set_read_position(uid, c, pos("epub:1:1", 0.0, 400))
        .await
        .unwrap();
    store
        .set_read_position(other, a, pos("epub:9:9", 0.9, 500))
        .await
        .unwrap();
    assert_eq!(
        store.get_read_position(uid, a).await.unwrap(),
        Some(pos("epub:2:10", 0.25, 100))
    );
    // A second save replaces the first.
    store
        .set_read_position(uid, a, pos("epub:3:0", 0.75, 200))
        .await
        .unwrap();
    assert_eq!(
        store
            .read_positions_bulk(uid, &[c, a, 9_999_999])
            .await
            .unwrap(),
        vec![
            Some(pos("epub:1:1", 0.0, 400)),
            Some(pos("epub:3:0", 0.75, 200)),
            None
        ]
    );

    // Part read: fraction above 0, newest save first, one user's own.
    assert_eq!(store.part_read_items(uid, 10).await.unwrap(), vec![b, a]);
    assert_eq!(store.part_read_items(uid, 1).await.unwrap(), vec![b]);
    assert_eq!(store.part_read_items(other, 10).await.unwrap(), vec![a]);

    // Saving a position does not touch the playback state, and a played book
    // drops out of the list without losing its position.
    assert_eq!(
        UserDataStore::get_user_data(&store, uid, a).await.unwrap(),
        UserItemData::default()
    );
    UserDataStore::set_user_data(
        &store,
        uid,
        b,
        UserItemData {
            played: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(store.part_read_items(uid, 10).await.unwrap(), vec![a]);
    assert!(store.get_read_position(uid, b).await.unwrap().is_some());
    assert_eq!(
        UserDataStore::get_user_data(&store, uid, b)
            .await
            .unwrap()
            .last_played_position_ticks,
        0
    );
}

/// A database of its own for one postgres conformance arm, dropped and
/// recreated each run. The arms run side by side (nextest runs each test in its
/// own process) and each asserts on whole-table state, such as "newest three
/// items" or "the runtime config is still the default", so one shared database
/// let one arm's rows land in another's view mid-assertion, and a retry saw
/// the first try's rows.
#[cfg(feature = "postgres")]
async fn fresh_postgres_url(shared: &str, arm: &str) -> String {
    use sqlx::Connection;
    let (base, _) = shared.rsplit_once('/').expect("postgres url has a db path");
    let db = format!("pharos_conformance_{arm}");
    let mut admin = sqlx::postgres::PgConnection::connect(&format!("{base}/postgres"))
        .await
        .expect("connect postgres admin db");
    // DROP/CREATE DATABASE cannot run inside a transaction.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {db}")))
        .execute(&mut admin)
        .await
        .expect("drop conformance db");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&mut admin)
        .await
        .expect("create conformance db");
    format!("{base}/{db}")
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_read_position_conformance() {
    let s = pharos_store_sqlx::sqlite::SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    run_read_position_conformance(s).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_read_position_conformance() {
    let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!("SKIP postgres_read_position_conformance: PHAROS_TEST_POSTGRES_URL unset");
        return;
    };
    let url = fresh_postgres_url(&url, "read_position").await;
    let p = pharos_store_sqlx::postgres::PostgresStore::connect(&url)
        .await
        .expect("connect postgres");
    run_read_position_conformance(p).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_series_identity_conformance() {
    let s = pharos_store_sqlx::sqlite::SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    run_series_identity_conformance(s).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_series_identity_conformance() {
    let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!("SKIP postgres_series_identity_conformance: PHAROS_TEST_POSTGRES_URL unset");
        return;
    };
    let url = fresh_postgres_url(&url, "series_identity").await;
    let p = pharos_store_sqlx::postgres::PostgresStore::connect(&url)
        .await
        .expect("connect postgres");
    run_series_identity_conformance(p).await;
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
    let url = fresh_postgres_url(&url, "main").await;
    let p = pharos_store_sqlx::postgres::PostgresStore::connect_with(
        &url,
        pharos_store_sqlx::PoolConfig {
            max_connections: 5,
            acquire_timeout: std::time::Duration::from_secs(7),
        },
    )
    .await
    .expect("connect postgres");
    // B836 — a configured pool is the one the postgres store opens.
    assert_eq!(p.pool().options().get_max_connections(), 5);
    assert_eq!(
        p.pool().options().get_acquire_timeout(),
        std::time::Duration::from_secs(7)
    );
    run_conformance(p).await;
}
