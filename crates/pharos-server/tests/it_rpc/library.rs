//! Spec 017 phase 4: the library service and `Library` over the native API.
//! Each behaviour has one service-level test here; the Jellyfin tests for
//! the same behaviour stay unchanged (US4).

use pharos_api::pharos_capnp::{items_result, session, Kind, LibraryKind as WireKind};
use pharos_core::{LibraryKind, UserPolicy, UserStore as _};
use pharos_server::services::library;

use crate::common::seed_rich;

/// T029: both typed libraries for an unrestricted user; only the allowed one
/// for a folder-restricted user (V184).
#[actix_web::test]
async fn views_list_the_libraries_the_policy_allows() {
    let fx = seed_rich().await;
    let all = library::views(&fx.state, &UserPolicy::default()).await;
    let names: Vec<_> = all.iter().map(|v| (v.name.as_str(), v.kind)).collect();
    assert_eq!(
        names,
        [
            ("Movies A", LibraryKind::Movies),
            ("Movies B", LibraryKind::Movies)
        ]
    );

    let restricted = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![fx.lib_a_wire.clone()],
        ..UserPolicy::default()
    };
    let seen = library::views(&fx.state, &restricted).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, fx.lib_a_wire);
}

/// The library names and kinds `Library.views` answers on `session`.
async fn wire_views(session: &session::Client) -> Vec<(String, WireKind)> {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.views_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.expect("views");
    let result = reply.get().unwrap().get_result().unwrap();
    let items_result::Page(page) = result.which().unwrap() else {
        panic!("views answered an error");
    };
    let page = page.unwrap();
    let items = page.get_items().unwrap();
    assert_eq!(page.get_total(), items.len());
    items
        .iter()
        .map(|item| {
            assert_eq!(item.get_kind().unwrap(), Kind::Library);
            (
                item.get_name().unwrap().to_string().unwrap(),
                item.get_library_kind().unwrap(),
            )
        })
        .collect()
}

/// T029 over the wire: a session lists its user's libraries, and a policy
/// changed (or the account disabled) while the session is open applies to
/// its next call.
/// Disarm check: with `views` reading `UserPolicy::default()` instead of the
/// store's, the restricted listing still shows "Movies B" and this fails.
#[actix_web::test]
async fn native_views_follow_the_users_current_policy() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;

    assert_eq!(
        wire_views(&session).await,
        [
            ("Movies A".to_owned(), WireKind::Movies),
            ("Movies B".to_owned(), WireKind::Movies),
        ]
    );

    let restricted = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![fx.lib_a_wire.clone()],
        ..UserPolicy::default()
    };
    fx.state
        .stores
        .set_policy(fx.user_id, restricted.clone())
        .await
        .unwrap();
    assert_eq!(
        wire_views(&session).await,
        [("Movies A".to_owned(), WireKind::Movies)]
    );

    let disabled = UserPolicy {
        is_disabled: true,
        ..restricted
    };
    fx.state
        .stores
        .set_policy(fx.user_id, disabled)
        .await
        .unwrap();
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.views_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.expect("views");
    let result = reply.get().unwrap().get_result().unwrap();
    assert!(
        matches!(result.which().unwrap(), items_result::Error(_)),
        "a disabled user's open session must stop listing libraries"
    );
}

/// Over the wire a view carries its library's id in the variant
/// `WireId::parse` gives it and the same Primary tag the Jellyfin listing
/// advertises, with a URL that serves it.
/// Disarm check: with `write_view` skipping `images`, or writing every id as
/// `real`, this fails.
#[actix_web::test]
async fn native_views_carry_the_library_id_and_its_poster() {
    use pharos_api::pharos_capnp::item_id;
    use pharos_jellyfin_api::dto::WireId;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let expected = library::views(&fx.state, &UserPolicy::default()).await;

    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.views_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.expect("views");
    let result = reply.get().unwrap().get_result().unwrap();
    let items_result::Page(page) = result.which().unwrap() else {
        panic!("views answered an error");
    };
    let items = page.unwrap().get_items().unwrap();
    assert_eq!(items.len() as usize, expected.len());
    for (item, view) in items.iter().zip(&expected) {
        let id = match item.get_id().unwrap().which().unwrap() {
            item_id::Real(id) => WireId::Real(id),
            item_id::Synth(id) => WireId::Synth(id),
        };
        assert_eq!(Some(id), WireId::parse(&view.id), "{}", view.name);

        let images = item.get_images().unwrap();
        let tag = view
            .primary_tag
            .as_deref()
            .expect("a seeded movie library has a poster");
        // ADR-0073: the native API hands out the same signed tag as the
        // Jellyfin routes (V712).
        let tag = fx.state.image_tag_key.sign(&view.id, "primary", tag);
        assert_eq!(images.len(), 1, "{}", view.name);
        let image = images.get(0);
        assert_eq!(image.get_kind().unwrap().to_str().unwrap(), "Primary");
        assert_eq!(image.get_tag().unwrap().to_str().unwrap(), tag);
        assert_eq!(
            image.get_url().unwrap().to_str().unwrap(),
            format!("/Items/{}/Images/Primary?tag={tag}", view.id)
        );
    }
}

/// T030: newest first, scoped to the parent, only what the policy allows,
/// and never more than `LATEST_MAX`.
#[actix_web::test]
async fn latest_lists_the_newest_items_the_policy_allows() {
    use library::{KindFilter, LATEST_MAX};
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _};

    let fx = seed_rich().await;
    // Far-future timestamps, so these outrank the fixture's own rows.
    for (id, created_at) in [
        (101, 4_000_000_030),
        (102, 4_000_000_020),
        (201, 4_000_000_010),
    ] {
        let lib = if id < 200 { "libA" } else { "libB" };
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/{lib}/m{id}.mkv").into(),
                title: format!("M{id}"),
                kind: MediaKind::Movie,
                created_at: Some(created_at),
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let ids = |rows: Vec<(pharos_core::MediaItem, pharos_core::UserItemData)>| {
        rows.into_iter().map(|(i, _)| i.id).collect::<Vec<_>>()
    };

    let all = library::latest(&fx.state, &user, None, &KindFilter::Unset, None)
        .await
        .unwrap();
    // Newest first.
    assert_eq!(ids(all), [101, 102, 201, 200, 100]);

    let a = library::latest(
        &fx.state,
        &user,
        Some(&fx.lib_a_wire),
        &KindFilter::Unset,
        Some(1),
    )
    .await
    .unwrap();
    assert_eq!(ids(a), [101]);

    let none = library::latest(&fx.state, &user, None, &KindFilter::MatchesNothing, None)
        .await
        .unwrap();
    assert!(none.is_empty());

    let restricted = pharos_core::User {
        policy: UserPolicy {
            enable_all_folders: false,
            enabled_folders: vec![fx.lib_b_wire.clone()],
            ..UserPolicy::default()
        },
        ..user.clone()
    };
    let b = library::latest(&fx.state, &restricted, None, &KindFilter::Unset, None)
        .await
        .unwrap();
    assert_eq!(ids(b), [201, 200]);
    let hidden = library::latest(
        &fx.state,
        &restricted,
        Some(&fx.lib_a_wire),
        &KindFilter::Unset,
        None,
    )
    .await
    .unwrap();
    assert!(hidden.is_empty(), "a forbidden parent lists nothing");

    let fx = seed_rich().await;
    for id in 1000..1000 + u64::from(LATEST_MAX) {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/libA/bulk{id}.mkv").into(),
                title: format!("B{id}"),
                kind: MediaKind::Movie,
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let capped = library::latest(&fx.state, &user, None, &KindFilter::Unset, Some(500))
        .await
        .unwrap();
    assert_eq!(capped.len(), LATEST_MAX as usize);
}

/// The shape of "latest" beyond the policy: ties on the timestamp fall to the
/// higher id, a kind filter keeps only those kinds, a series or season parent
/// lists only its episodes, and a limit of 0 lists nothing.
#[actix_web::test]
async fn latest_orders_ties_by_id_and_scopes_to_kinds_and_series() {
    use library::KindFilter;
    use pharos_core::{MediaItem, MediaKind, MediaStore as _, SeriesInfo};
    use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key};

    let fx = seed_rich().await;
    let episode = |id: u64, show: &str, season: u32, at: i64| MediaItem {
        id,
        path: format!("/libA/{show}/S{season}/e{id}.mkv").into(),
        title: format!("E{id}"),
        kind: MediaKind::Episode,
        created_at: Some(at),
        series: Some(SeriesInfo {
            series_name: show.into(),
            season_number: Some(season),
            episode_number: Some(1),
            series_folder: Some(format!("/libA/{show}")),
            series_year: None,
            series_key: None,
        }),
        ..MediaItem::default()
    };
    let movie = |id: u64, at: i64| MediaItem {
        id,
        path: format!("/libA/m{id}.mkv").into(),
        title: format!("M{id}"),
        kind: MediaKind::Movie,
        created_at: Some(at),
        ..MediaItem::default()
    };
    let at = 4_100_000_000;
    for item in [
        movie(301, at),
        movie(302, at),
        episode(311, "Alpha", 1, at + 1),
        episode(312, "Alpha", 2, at + 3),
        episode(313, "Beta", 1, at + 2),
    ] {
        fx.state.stores.put(item).await.unwrap();
    }
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let ids = |rows: Vec<(MediaItem, pharos_core::UserItemData)>| {
        rows.into_iter().map(|(i, _)| i.id).collect::<Vec<_>>()
    };

    let all = library::latest(&fx.state, &user, None, &KindFilter::Unset, Some(5))
        .await
        .unwrap();
    assert_eq!(ids(all), [312, 313, 311, 302, 301]);

    let movies = library::latest(
        &fx.state,
        &user,
        None,
        &KindFilter::Kinds(vec![MediaKind::Movie]),
        Some(2),
    )
    .await
    .unwrap();
    assert_eq!(ids(movies), [302, 301]);

    let alpha = series_id_for_key(Some("/libA/Alpha"), "Alpha");
    let show = library::latest(&fx.state, &user, Some(&alpha), &KindFilter::Unset, None)
        .await
        .unwrap();
    assert_eq!(ids(show), [312, 311]);

    let season = season_id_for_key(Some("/libA/Alpha"), "Alpha", 1);
    let one = library::latest(&fx.state, &user, Some(&season), &KindFilter::Unset, None)
        .await
        .unwrap();
    assert_eq!(ids(one), [311]);

    let unknown = library::latest(
        &fx.state,
        &user,
        Some("ffffffffffffffffffffffffffffffff"),
        &KindFilter::Unset,
        None,
    )
    .await
    .unwrap();
    assert!(unknown.is_empty(), "an unknown parent lists nothing");

    let zero = library::latest(&fx.state, &user, None, &KindFilter::Unset, Some(0))
        .await
        .unwrap();
    assert!(zero.is_empty());
}

/// `Library.latest` on `session`, as the Jellyfin fields the test compares.
async fn wire_latest(
    session: &session::Client,
    parent: Option<pharos_jellyfin_api::dto::WireId>,
) -> Vec<serde_json::Value> {
    use pharos_jellyfin_api::dto::WireId;
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.latest_request();
    req.get().init_ctx();
    if let Some(parent) = parent {
        let mut id = req.get().init_parent_id();
        match parent {
            WireId::Real(n) => id.set_real(n),
            WireId::Synth(n) => id.set_synth(n),
        }
    }
    let reply = req.send().promise.await.expect("latest");
    summaries(reply.get().unwrap().get_result().unwrap()).1
}

/// The page's total and its items as the Jellyfin fields the tests compare.
fn summaries(result: items_result::Reader<'_>) -> (u32, Vec<serde_json::Value>) {
    let items_result::Page(page) = result.which().unwrap() else {
        panic!("the listing answered an error");
    };
    let page = page.unwrap();
    let items = page.get_items().unwrap().iter().map(summary).collect();
    (page.get_total(), items)
}

/// One `ItemSummary` as the Jellyfin fields the tests compare.
fn summary(item: pharos_api::pharos_capnp::item_summary::Reader<'_>) -> serde_json::Value {
    use pharos_api::pharos_capnp::{item_id, opt_u16, opt_u32};
    use pharos_jellyfin_api::dto::WireId;
    let opt16 = |o: opt_u16::Reader<'_>| match o.which().unwrap() {
        opt_u16::Some(n) => Some(u32::from(n)),
        opt_u16::None(()) => None,
    };
    let opt32 = |o: opt_u32::Reader<'_>| match o.which().unwrap() {
        opt_u32::Some(n) => Some(n),
        opt_u32::None(()) => None,
    };
    let id = match item.get_id().unwrap().which().unwrap() {
        item_id::Real(n) => WireId::Real(n),
        item_id::Synth(n) => WireId::Synth(n),
    };
    let images = item.get_images().unwrap();
    serde_json::json!({
        "Id": id.to_string(),
        "Name": item.get_name().unwrap().to_str().unwrap(),
        "ProductionYear": item.has_year().then(|| opt16(item.get_year().unwrap())).flatten(),
        "SeriesName": Some(item.get_series_name().unwrap().to_str().unwrap()).filter(|s| !s.is_empty()),
        "IndexNumber": item.has_index_number().then(|| opt32(item.get_index_number().unwrap())).flatten(),
        "ParentIndexNumber": item.has_parent_index_number().then(|| opt32(item.get_parent_index_number().unwrap())).flatten(),
        "Primary": (!images.is_empty()).then(|| images.get(0).get_tag().unwrap().to_str().unwrap().to_owned()),
        "CriticRating": item.has_critic_rating().then(|| opt16(item.get_critic_rating().unwrap())).flatten(),
        "ImdbRating": item.has_imdb_rating().then(|| opt16(item.get_imdb_rating().unwrap())).flatten(),
    })
}

/// A Jellyfin `BaseItemDto` list reduced to the fields `summaries` reads.
fn project(dtos: &[serde_json::Value]) -> Vec<serde_json::Value> {
    dtos.iter()
        .map(|d| {
            serde_json::json!({
                "Id": d["Id"],
                "Name": d["Name"],
                "ProductionYear": d.get("ProductionYear").cloned().unwrap_or_default(),
                "SeriesName": d.get("SeriesName").cloned().unwrap_or_default(),
                "IndexNumber": d.get("IndexNumber").cloned().unwrap_or_default(),
                "ParentIndexNumber": d.get("ParentIndexNumber").cloned().unwrap_or_default(),
                "Primary": d["ImageTags"].get("Primary").cloned().unwrap_or_default(),
                "CriticRating": d
                    .get("CriticRating")
                    .and_then(|r| r.as_f64())
                    .map(f64::round)
                    .filter(|r| (0.0..=100.0).contains(r))
                    .map(|r| r as u32),
                "ImdbRating": d
                    .get("PharosImdbRating")
                    .and_then(|r| r.as_f64())
                    .map(|r| (r * 10.0).round() as u32),
            })
        })
        .collect()
}

/// T030 over the wire: `Library.latest` lists what the Jellyfin Latest route
/// lists for the same user and parent, in the same order with the same
/// display fields; a library id taken from `Library.views` scopes it; a
/// forbidden library lists nothing.
/// Disarm check: with `write_item` skipping the Primary image, the year or
/// the series name, the agreement assertion fails.
#[actix_web::test]
async fn native_latest_agrees_with_the_jellyfin_latest_route() {
    use actix_web::test;
    use pharos_core::{LibraryStore as _, MediaStore as _};
    use pharos_jellyfin_api::dto::WireId;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    // An episode, so the series name and index fields are compared too.
    let episode: u64 = 150;
    fx.state
        .stores
        .put(pharos_core::MediaItem {
            id: episode,
            path: "/libA/Show/Season 01/show-s01e02.mkv".into(),
            title: "Second".into(),
            kind: pharos_core::MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "Show".into(),
                season_number: Some(1),
                episode_number: Some(2),
                series_folder: Some("/libA/Show".into()),
                series_year: Some(2001),
                series_key: None,
            }),
            ..pharos_core::MediaItem::default()
        })
        .await
        .unwrap();
    fx.state.stores.backfill_library_ids().await.unwrap();
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let uid = fx.user_id.0.simple();
    let jellyfin = |parent: Option<&str>| {
        let uri = match parent {
            Some(p) => format!("/Users/{uid}/Items/Latest?ParentId={p}"),
            None => format!("/Users/{uid}/Items/Latest"),
        };
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("X-Emby-Token", fx.user_token.as_str()))
            .to_request()
    };
    let native = wire_latest(&session, None).await;
    assert!(!native.is_empty());
    let web: serde_json::Value = test::call_and_read_body_json(&app, jellyfin(None)).await;
    assert_eq!(native, project(web.as_array().unwrap()));

    // The id `Library.views` hands out scopes `latest` to that library.
    let lib_a = WireId::parse(&fx.lib_a_wire).unwrap();
    let scoped = wire_latest(&session, Some(lib_a)).await;
    let web: serde_json::Value =
        test::call_and_read_body_json(&app, jellyfin(Some(&fx.lib_a_wire))).await;
    assert_eq!(scoped, project(web.as_array().unwrap()));
    let mut ids: Vec<_> = scoped.iter().map(|i| i["Id"].clone()).collect();
    ids.sort_by_key(|v| v.to_string());
    assert_eq!(
        ids,
        [fx.rich_item_id, episode].map(|n| serde_json::json!(WireId::Real(n).to_string()))
    );
    assert!(
        scoped
            .iter()
            .any(|i| i["SeriesName"] == "Show" && i["IndexNumber"] == 2),
        "{scoped:?}"
    );

    fx.state
        .stores
        .set_policy(
            fx.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![fx.lib_b_wire.clone()],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    assert!(wire_latest(&session, Some(lib_a)).await.is_empty());
    let all = wire_latest(&session, None).await;
    assert_eq!(
        all.iter().map(|i| i["Id"].clone()).collect::<Vec<_>>(),
        [serde_json::json!(WireId::Real(fx.other_item_id).to_string())]
    );
}

/// Marks `ids` part-watched for the fixture's user, the first most recent.
async fn start_watching(fx: &crate::common::Fixture, ids: &[u64]) {
    use pharos_core::UserDataStore as _;
    for (n, id) in ids.iter().enumerate() {
        fx.state
            .stores
            .set_user_data(
                fx.user_id,
                *id,
                pharos_core::UserItemData {
                    last_played_position_ticks: 600_000_000,
                    last_played_at: 2_000_000_000 - n as i64,
                    ..pharos_core::UserItemData::default()
                },
            )
            .await
            .unwrap();
    }
}

/// T031: what the user started and has not finished, of the kinds asked
/// for, paged with the unpaged total, and only what the policy allows.
#[actix_web::test]
async fn resume_lists_what_the_user_started_that_the_policy_allows() {
    use library::KindFilter;
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _};

    let fx = seed_rich().await;
    fx.state
        .stores
        .put(MediaItem {
            id: 300,
            path: "/libA/song.flac".into(),
            title: "Song".into(),
            kind: MediaKind::Audio,
            ..MediaItem::default()
        })
        .await
        .unwrap();
    fx.state.stores.backfill_library_ids().await.unwrap();
    start_watching(&fx, &[fx.rich_item_id, fx.other_item_id, 300]).await;
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let ids = |page: &library::ListPage| page.items.iter().map(|(i, _)| i.id).collect::<Vec<_>>();

    let all = library::resume(&fx.state, &user, &KindFilter::Unset, 0, None)
        .await
        .unwrap();
    assert_eq!(all.total, 3);
    let mut seen = ids(&all);
    seen.sort_unstable();
    assert_eq!(seen, [fx.rich_item_id, fx.other_item_id, 300]);
    assert!(all
        .items
        .iter()
        .all(|(_, ud)| ud.last_played_position_ticks == 600_000_000));

    let audio = library::resume(
        &fx.state,
        &user,
        &KindFilter::Kinds(vec![MediaKind::Audio]),
        0,
        None,
    )
    .await
    .unwrap();
    assert_eq!((ids(&audio), audio.total), (vec![300], 1));

    let none = library::resume(&fx.state, &user, &KindFilter::MatchesNothing, 0, None)
        .await
        .unwrap();
    assert_eq!((none.items.len(), none.total), (0, 0));

    let second = library::resume(&fx.state, &user, &KindFilter::Unset, 1, Some(1))
        .await
        .unwrap();
    assert_eq!((second.items.len(), second.total, second.start), (1, 3, 1));
    assert_eq!(ids(&second), [ids(&all)[1]]);

    let past_end = library::resume(&fx.state, &user, &KindFilter::Unset, 9, Some(5))
        .await
        .unwrap();
    assert_eq!((past_end.items.len(), past_end.start), (0, 3));

    let restricted = pharos_core::User {
        policy: UserPolicy {
            enable_all_folders: false,
            enabled_folders: vec![fx.lib_a_wire.clone()],
            ..UserPolicy::default()
        },
        ..user
    };
    let a = library::resume(&fx.state, &restricted, &KindFilter::Unset, 0, None)
        .await
        .unwrap();
    let mut seen = ids(&a);
    seen.sort_unstable();
    assert_eq!((seen, a.total), (vec![fx.rich_item_id, 300], 2));
}

/// B310: the home page resumes films and episodes in one row and music in
/// another, so a part-played song never shows under "Continue watching".
#[actix_web::test]
async fn home_resumes_music_apart_from_what_is_watched() {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _};

    let fx = seed_rich().await;
    fx.state
        .stores
        .put(MediaItem {
            id: 300,
            path: "/libA/song.flac".into(),
            title: "Song".into(),
            kind: MediaKind::Audio,
            ..MediaItem::default()
        })
        .await
        .unwrap();
    fx.state.stores.backfill_library_ids().await.unwrap();
    start_watching(&fx, &[300, fx.rich_item_id]).await;
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();

    let home = library::home(&fx.state, &user, None, None, None)
        .await
        .unwrap();
    let ids = |page: &library::ListPage| page.items.iter().map(|(i, _)| i.id).collect::<Vec<_>>();
    assert_eq!(ids(&home.resume), [fx.rich_item_id]);
    assert_eq!(ids(&home.listening), [300]);
}

/// `Library.resume` on `session` for `kinds`, as its total and items.
async fn wire_resume(session: &session::Client, kinds: &[Kind]) -> (u32, Vec<serde_json::Value>) {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.resume_request();
    req.get().init_ctx();
    let mut list = req.get().init_kinds(kinds.len() as u32);
    for (i, kind) in kinds.iter().enumerate() {
        list.set(i as u32, *kind);
    }
    let reply = req.send().promise.await.expect("resume");
    summaries(reply.get().unwrap().get_result().unwrap())
}

/// T031 over the wire: `Library.resume` lists what the Jellyfin Resume route
/// lists for the same user and media types, with the same total; kinds that
/// are never stored match nothing; a policy change applies to the next call.
/// Disarm check: with `kind_filter` reading an empty list as `MatchesNothing`,
/// the unfiltered call lists nothing and the agreement fails.
#[actix_web::test]
async fn native_resume_agrees_with_the_jellyfin_resume_route() {
    use actix_web::test;
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    fx.state
        .stores
        .put(MediaItem {
            id: 300,
            path: "/libA/song.flac".into(),
            title: "Song".into(),
            kind: MediaKind::Audio,
            ..MediaItem::default()
        })
        .await
        .unwrap();
    fx.state.stores.backfill_library_ids().await.unwrap();
    start_watching(&fx, &[fx.rich_item_id, fx.other_item_id, 300]).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let uid = fx.user_id.0.simple();
    let jellyfin = |media_types: &str| {
        test::TestRequest::get()
            .uri(&format!(
                "/Users/{uid}/Items/Resume?MediaTypes={media_types}"
            ))
            .insert_header(("X-Emby-Token", fx.user_token.as_str()))
            .to_request()
    };

    for (kinds, media_types) in [
        (&[][..], ""),
        (&[Kind::Movie, Kind::Episode][..], "Video"),
        (&[Kind::Audio][..], "Audio"),
    ] {
        let (total, native) = wire_resume(&session, kinds).await;
        let web: serde_json::Value =
            test::call_and_read_body_json(&app, jellyfin(media_types)).await;
        assert_eq!(
            native,
            project(web["Items"].as_array().unwrap()),
            "{media_types}"
        );
        assert_eq!(u64::from(total), web["TotalRecordCount"].as_u64().unwrap());
    }
    assert_eq!(wire_resume(&session, &[]).await.0, 3);
    assert_eq!(
        wire_resume(&session, &[Kind::Series, Kind::Album]).await,
        (0, Vec::new())
    );

    fx.state
        .stores
        .set_policy(
            fx.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![fx.lib_b_wire.clone()],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    let (total, items) = wire_resume(&session, &[]).await;
    assert_eq!(total, 1);
    assert_eq!(
        items[0]["Id"],
        pharos_jellyfin_api::dto::WireId::Real(fx.other_item_id).to_string()
    );
}

/// Seeds three episodes of "Show" in /libA and two of "Another" in /libB,
/// with "Show" S01E01 played and then "Another" S01E01 played after it.
async fn seed_shows(fx: &crate::common::Fixture) {
    use pharos_core::{
        LibraryStore as _, MediaItem, MediaKind, MediaStore as _, SeriesInfo, UserDataStore as _,
    };
    let episode = |id: u64, lib: &str, show: &str, ep: u32| MediaItem {
        id,
        path: format!("/{lib}/{show}/Season 01/e{ep}.mkv").into(),
        title: format!("{show} {ep}"),
        kind: MediaKind::Episode,
        series: Some(SeriesInfo {
            series_name: show.into(),
            season_number: Some(1),
            episode_number: Some(ep),
            series_folder: Some(format!("/{lib}/{show}")),
            series_year: None,
            series_key: None,
        }),
        ..MediaItem::default()
    };
    for item in [
        episode(401, "libA", "Show", 1),
        episode(403, "libA", "Show", 3),
        episode(402, "libA", "Show", 2),
        episode(501, "libB", "Another", 1),
        episode(502, "libB", "Another", 2),
    ] {
        fx.state.stores.put(item).await.unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    for (id, at) in [(401, 100), (501, 200)] {
        fx.state
            .stores
            .set_user_data(
                fx.user_id,
                id,
                pharos_core::UserItemData {
                    played: true,
                    last_played_at: at,
                    ..pharos_core::UserItemData::default()
                },
            )
            .await
            .unwrap();
    }
}

/// T032 / spec 038: the episode after the last played one in each started
/// series, the series watched most recently first, scoped to one series on
/// request, only where the policy allows.
#[actix_web::test]
async fn next_up_picks_the_first_unplayed_episode_of_each_visible_series() {
    let fx = seed_rich().await;
    seed_shows(&fx).await;
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let ids = |page: library::ListPage| page.items.iter().map(|(i, _)| i.id).collect::<Vec<_>>();

    let all = library::next_up(&fx.state, &user, None, None)
        .await
        .unwrap();
    assert_eq!(all.total, 2);
    assert_eq!(ids(all), [502, 402]);

    let one = library::next_up(&fx.state, &user, None, Some(0))
        .await
        .unwrap();
    assert_eq!(ids(one), [502], "a limit below 1 still lists one");

    let show = pharos_jellyfin_api::dto::series_id_for_key(Some("/libA/Show"), "Show");
    let scoped = library::next_up(&fx.state, &user, Some(&show), None)
        .await
        .unwrap();
    assert_eq!(ids(scoped), [402]);

    let restricted = pharos_core::User {
        policy: UserPolicy {
            enable_all_folders: false,
            enabled_folders: vec![fx.lib_a_wire.clone()],
            ..UserPolicy::default()
        },
        ..user
    };
    let a = library::next_up(&fx.state, &restricted, None, None)
        .await
        .unwrap();
    assert_eq!(ids(a), [402]);
}

/// One show in /libA: season 0 special 1, then S01E01–E03 and S02E01.
async fn seed_one_show(fx: &crate::common::Fixture) {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _, SeriesInfo};
    let episode = |id: u64, season: u32, ep: u32| MediaItem {
        id,
        path: format!("/libA/Ladder/Season {season:02}/e{ep}.mkv").into(),
        title: format!("Ladder {season}x{ep}"),
        kind: MediaKind::Episode,
        series: Some(SeriesInfo {
            series_name: "Ladder".into(),
            season_number: Some(season),
            episode_number: Some(ep),
            series_folder: Some("/libA/Ladder".into()),
            series_year: None,
            series_key: None,
        }),
        ..MediaItem::default()
    };
    for item in [
        episode(600, 0, 1),
        episode(611, 1, 1),
        episode(612, 1, 2),
        episode(613, 1, 3),
        episode(621, 2, 1),
    ] {
        fx.state.stores.put(item).await.unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
}

async fn watch(fx: &crate::common::Fixture, id: u64, played: bool, position: u64, at: i64) {
    use pharos_core::UserDataStore as _;
    fx.state
        .stores
        .set_user_data(
            fx.user_id,
            id,
            pharos_core::UserItemData {
                played,
                last_played_position_ticks: position,
                last_played_at: at,
                ..pharos_core::UserItemData::default()
            },
        )
        .await
        .unwrap();
}

/// Spec 038 FR-101–FR-104 (ADR-0113), on one fixture per case so the
/// cached item list is fresh for each.
#[actix_web::test]
async fn next_up_follows_the_last_played_episode() {
    async fn case(watched: &[(u64, bool, u64, i64)]) -> Vec<u64> {
        let fx = seed_rich().await;
        seed_one_show(&fx).await;
        for &(id, played, position, at) in watched {
            watch(&fx, id, played, position, at).await;
        }
        let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
            .await
            .unwrap();
        library::next_up(&fx.state, &user, None, None)
            .await
            .unwrap()
            .items
            .iter()
            .map(|(i, _)| i.id)
            .collect()
    }
    // FR-101: nothing watched, nothing listed.
    assert_eq!(case(&[]).await, Vec::<u64>::new());
    // FR-102: the episode after the last played, not the lowest unplayed.
    assert_eq!(case(&[(612, true, 0, 10)]).await, [613]);
    // ...across a season boundary.
    assert_eq!(case(&[(613, true, 0, 10)]).await, [621]);
    // The most recent play is the anchor, even when it is an earlier episode.
    assert_eq!(case(&[(613, true, 0, 10), (611, true, 0, 20)]).await, [612]);
    // Played episodes after the anchor are skipped.
    assert_eq!(case(&[(611, true, 0, 20), (612, true, 0, 10)]).await, [613]);
    // A special is never next after a regular episode...
    assert_eq!(case(&[(611, true, 0, 10)]).await, [612]);
    // ...but a played special leads on to the regular season.
    assert_eq!(case(&[(600, true, 0, 10)]).await, [611]);
    // FR-101: everything after the last played is watched, so left out.
    assert_eq!(case(&[(621, true, 0, 10)]).await, Vec::<u64>::new());
    // FR-104: an episode in progress belongs to Continue Watching.
    assert_eq!(case(&[(612, false, 5_000, 10)]).await, Vec::<u64>::new());
    assert_eq!(
        case(&[(611, true, 0, 10), (612, false, 5_000, 5)]).await,
        Vec::<u64>::new()
    );
}

/// FR-103: series ordered by their latest play, newest first.
#[actix_web::test]
async fn next_up_lists_the_series_watched_last_first() {
    let fx = seed_rich().await;
    seed_shows(&fx).await;
    seed_one_show(&fx).await;
    watch(&fx, 611, true, 0, 150).await;
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let ids: Vec<u64> = library::next_up(&fx.state, &user, None, None)
        .await
        .unwrap()
        .items
        .iter()
        .map(|(i, _)| i.id)
        .collect();
    // "Another" at 200, "Ladder" at 150, "Show" at 100.
    assert_eq!(ids, [502, 612, 402]);
}

/// The Series page asks with `SeriesId`: an unstarted series shows its first
/// regular episode, and one in progress shows the episode to resume.
#[actix_web::test]
async fn scoped_next_up_lists_the_first_or_the_resumed_episode() {
    let show = pharos_jellyfin_api::dto::series_id_for_key(Some("/libA/Ladder"), "Ladder");
    for (watched, want) in [(None, 611), (Some((612, false, 5_000, 10)), 612)] {
        let fx = seed_rich().await;
        seed_one_show(&fx).await;
        if let Some((id, played, position, at)) = watched {
            watch(&fx, id, played, position, at).await;
        }
        let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
            .await
            .unwrap();
        let page = library::next_up(&fx.state, &user, Some(&show), None)
            .await
            .unwrap();
        let ids: Vec<u64> = page.items.iter().map(|(i, _)| i.id).collect();
        assert_eq!(ids, [want], "{watched:?}");
    }
}

/// FR-105: Next Up reads the viewer's started rows, and a bulk read for its
/// picks alone, never one keyed by every visible item.
#[actix_web::test]
async fn next_up_reads_the_viewers_rows_not_every_items() {
    use tracing_subscriber::prelude::*;
    let fx = seed_rich().await;
    seed_shows(&fx).await;
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let calls = crate::store_calls::StoreCallLog::default();
    let _spans =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(calls.clone()));
    let page = library::next_up(&fx.state, &user, None, None)
        .await
        .unwrap();
    let made = calls.take();
    assert!(made.iter().any(|c| c == "started_items"), "{made:?}");
    let bulk: Vec<u64> = calls
        .take_counts()
        .into_iter()
        .filter(|(m, _)| m == "user_data_bulk")
        .map(|(_, n)| n)
        .collect();
    assert_eq!(bulk, [page.items.len() as u64], "only the picks are bound");
}

/// A pick keeps its own row: a favourite unplayed episode is listed as one.
#[actix_web::test]
async fn next_up_carries_the_picks_own_play_state() {
    use pharos_core::UserDataStore as _;
    let fx = seed_rich().await;
    seed_one_show(&fx).await;
    watch(&fx, 611, true, 0, 10).await;
    let favourite = pharos_core::UserItemData {
        is_favorite: true,
        audio_stream_index: Some(2),
        ..pharos_core::UserItemData::default()
    };
    fx.state
        .stores
        .set_user_data(fx.user_id, 612, favourite)
        .await
        .unwrap();
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let page = library::next_up(&fx.state, &user, None, None)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].0.id, 612);
    assert_eq!(page.items[0].1, favourite);
}

/// A Next Up that cannot read the viewer's rows is counted as an error.
#[actix_web::test]
async fn a_failed_next_up_is_counted() {
    let fx = seed_rich().await;
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    #[cfg(feature = "postgres")]
    let sqlite = match &fx.state.stores {
        pharos_store_sqlx::any::AnyStore::Sqlite(s) => s,
        pharos_store_sqlx::any::AnyStore::Postgres(_) => panic!("the fixture is sqlite"),
    };
    #[cfg(not(feature = "postgres"))]
    let sqlite = &fx.state.stores;
    sqlx::query("DROP TABLE user_data")
        .execute(sqlite.pool())
        .await
        .unwrap();
    assert!(library::next_up(&fx.state, &user, None, None)
        .await
        .is_err());
    assert_eq!(next_up_count(&snap, "error"), 1);
}

fn next_up_count(snap: &pharos_test_metrics::CumulativeSnapshotter, outcome: &str) -> u64 {
    use metrics_util::debugging::DebugValue;
    snap.snapshot()
        .into_vec()
        .into_iter()
        .filter(|(ck, ..)| {
            ck.key().name() == "pharos_next_up_total"
                && ck
                    .key()
                    .labels()
                    .any(|l| l.key() == "outcome" && l.value() == outcome)
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(n) => n,
            _ => 0,
        })
        .sum()
}

/// Spec 038 T038-101: each Next Up listing is counted by outcome.
/// Disarm check: with `NextUpOutcome::record` a no-op, both counts read 0.
#[actix_web::test]
async fn next_up_is_counted_by_outcome() {
    let fx = seed_rich().await;
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    seed_shows(&fx).await;
    let some = library::next_up(&fx.state, &user, None, None)
        .await
        .unwrap();
    assert!(!some.items.is_empty());
    assert_eq!(next_up_count(&snap, "listed"), 1);

    let unknown = pharos_jellyfin_api::dto::series_id_for_key(Some("/libA/Nothing"), "Nothing");
    let none = library::next_up(&fx.state, &user, Some(&unknown), None)
        .await
        .unwrap();
    assert!(none.items.is_empty());
    assert_eq!(next_up_count(&snap, "empty"), 1);
    assert_eq!(next_up_count(&snap, "error"), 0);
}

/// `Library.nextUp` on `session`, as its total and items.
async fn wire_next_up(session: &session::Client, limit: u32) -> (u32, Vec<serde_json::Value>) {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.next_up_request();
    req.get().init_ctx();
    req.get().set_limit(limit);
    let reply = req.send().promise.await.expect("nextUp");
    summaries(reply.get().unwrap().get_result().unwrap())
}

/// T032 over the wire: `Library.nextUp` lists what `/Shows/NextUp` lists for
/// the same user, with the same total, and a policy change applies to the
/// next call.
/// Disarm check: with `next_up` passing `Some(1)` as the limit, the
/// agreement fails (one series instead of two).
#[actix_web::test]
async fn native_next_up_agrees_with_the_jellyfin_next_up_route() {
    use actix_web::test;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_shows(&fx).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Shows/NextUp")
        .insert_header(("X-Emby-Token", fx.user_token.as_str()))
        .to_request();
    let web: serde_json::Value = test::call_and_read_body_json(&app, req).await;

    let (total, native) = wire_next_up(&session, 0).await;
    assert_eq!(native.len(), 2);
    assert_eq!(native, project(web["Items"].as_array().unwrap()));
    assert_eq!(u64::from(total), web["TotalRecordCount"].as_u64().unwrap());
    assert_eq!(wire_next_up(&session, 1).await.1.len(), 1);

    fx.state
        .stores
        .set_policy(
            fx.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![fx.lib_a_wire.clone()],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    let (_, items) = wire_next_up(&session, 0).await;
    assert_eq!(
        items
            .iter()
            .map(|i| i["SeriesName"].clone())
            .collect::<Vec<_>>(),
        [serde_json::json!("Show")]
    );
}

/// Seeds movies 101 "Alpha" and 102 "Beta" in /libA and 201 "Gamma" in
/// /libB beside the fixture's own rows, with 102 a favourite.
pub(crate) async fn seed_movies(fx: &crate::common::Fixture) {
    use pharos_core::{
        LibraryStore as _, MediaItem, MediaKind, MediaStore as _, UserDataStore as _,
    };
    for (id, title) in [(101, "Alpha"), (102, "Beta"), (201, "Gamma")] {
        let lib = if id < 200 { "libA" } else { "libB" };
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/{lib}/{title}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    fx.state
        .stores
        .set_user_data(
            fx.user_id,
            102,
            pharos_core::UserItemData {
                is_favorite: true,
                ..pharos_core::UserItemData::default()
            },
        )
        .await
        .unwrap();
}

/// T033: the listing pages with the unpaged total, and a restricted user
/// sees only the libraries the policy allows, in the total and under a
/// forbidden parent alike.
/// Disarm check: with `apply_policy_scope` skipped in
/// `services::library::query`, the restricted totals count /libA.
#[actix_web::test]
async fn query_pages_and_keeps_to_the_policy() {
    let fx = seed_rich().await;
    seed_movies(&fx).await;
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let ids = |page: &library::QueryPage| page.items.iter().map(|i| i.id).collect::<Vec<_>>();
    let by_name = |start, limit| library::ItemQuery {
        sort_by: Some("SortName".into()),
        start_index: start,
        limit,
        ..library::ItemQuery::default()
    };

    let all = library::query(&fx.state, &user, None, &by_name(0, None))
        .await
        .unwrap();
    assert_eq!(all.total, 5);
    let second = library::query(&fx.state, &user, None, &by_name(1, Some(2)))
        .await
        .unwrap();
    assert_eq!((second.total, second.start), (5, 1));
    assert_eq!(ids(&second), ids(&all)[1..3]);

    let restricted = pharos_core::User {
        policy: UserPolicy {
            enable_all_folders: false,
            enabled_folders: vec![fx.lib_b_wire.clone()],
            ..UserPolicy::default()
        },
        ..user.clone()
    };
    let b = library::query(&fx.state, &restricted, None, &by_name(0, None))
        .await
        .unwrap();
    assert_eq!(b.total, 2);
    assert!(ids(&b).iter().all(|id| *id >= 200), "{:?}", ids(&b));
    let hidden = library::query(
        &fx.state,
        &restricted,
        Some(&fx.lib_a_wire),
        &by_name(0, None),
    )
    .await
    .unwrap();
    assert_eq!((hidden.total, ids(&hidden)), (0, vec![]));
}

/// One `Library.query` call on `session`.
#[allow(clippy::too_many_arguments)]
async fn wire_query(
    session: &session::Client,
    parent: Option<&str>,
    kinds: &[Kind],
    sort_by: &str,
    descending: bool,
    name_prefix: &str,
    filters: &[&str],
    start: u32,
    limit: u32,
) -> (u32, Vec<serde_json::Value>) {
    use pharos_jellyfin_api::dto::WireId;
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.query_request();
    req.get().init_ctx();
    let mut q = req.get().init_q();
    if let Some(parent) = parent {
        let mut id = q.reborrow().init_parent_id();
        match WireId::parse(parent).expect("parent is a wire id") {
            WireId::Real(n) => id.set_real(n),
            WireId::Synth(n) => id.set_synth(n),
        }
    }
    let mut list = q.reborrow().init_kinds(kinds.len() as u32);
    for (i, kind) in kinds.iter().enumerate() {
        list.set(i as u32, *kind);
    }
    q.set_sort_by(sort_by);
    q.set_descending(descending);
    q.set_name_starts_with(name_prefix);
    let mut list = q.reborrow().init_filters(filters.len() as u32);
    for (i, f) in filters.iter().enumerate() {
        list.set(i as u32, *f);
    }
    q.set_start(start);
    q.set_limit(limit);
    let reply = req.send().promise.await.expect("query");
    summaries(reply.get().unwrap().get_result().unwrap())
}

/// A `Library.query` call's arguments and the `/Items` query that should
/// list the same: parent, kinds, sort, descending, prefix, filters, start,
/// limit, then the Jellyfin query string.
type Shape<'a> = (
    Option<&'a str>,
    &'a [Kind],
    &'a str,
    bool,
    &'a str,
    &'a [&'a str],
    u32,
    u32,
    String,
);

/// T033 over the wire: `Library.query` lists what `/Users/{id}/Items` lists
/// for the same parent, kinds, sort, name prefix, filters and page, with the
/// same total; a limit of 0 is the default page.
/// Disarm check: with `query` dropping `descending` (or the filters), the
/// matching shape disagrees.
#[actix_web::test]
async fn native_query_agrees_with_the_jellyfin_items_route() {
    use actix_web::test;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_movies(&fx).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let uid = fx.user_id.0.simple();
    let lib_a = fx.lib_a_wire.clone();

    let shapes: Vec<Shape<'_>> = vec![
        (
            None,
            &[],
            "SortName",
            false,
            "",
            &[],
            0,
            0,
            "SortBy=SortName&Limit=100".into(),
        ),
        (
            None,
            &[],
            "SortName",
            true,
            "",
            &[],
            0,
            50,
            "SortBy=SortName&SortOrder=Descending&Limit=50".into(),
        ),
        (
            Some(&lib_a),
            &[Kind::Movie],
            "SortName",
            false,
            "",
            &[],
            0,
            50,
            format!("ParentId={lib_a}&IncludeItemTypes=Movie&SortBy=SortName&Limit=50"),
        ),
        (
            None,
            &[],
            "SortName",
            false,
            "B",
            &[],
            0,
            50,
            "SortBy=SortName&NameStartsWith=B&Limit=50".into(),
        ),
        (
            None,
            &[],
            "SortName",
            false,
            "",
            &["IsFavorite"],
            0,
            50,
            "SortBy=SortName&Filters=IsFavorite&Limit=50".into(),
        ),
        (
            None,
            &[],
            "SortName",
            false,
            "",
            &[],
            1,
            2,
            "SortBy=SortName&StartIndex=1&Limit=2".into(),
        ),
    ];
    for (parent, kinds, sort, desc, prefix, filters, start, limit, jf) in shapes {
        let (total, native) = wire_query(
            &session, parent, kinds, sort, desc, prefix, filters, start, limit,
        )
        .await;
        let req = test::TestRequest::get()
            .uri(&format!("/Users/{uid}/Items?{jf}"))
            .insert_header(("X-Emby-Token", fx.user_token.as_str()))
            .to_request();
        let web: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        assert!(!native.is_empty(), "{jf}: the shape must list something");
        assert_eq!(native, project(web["Items"].as_array().unwrap()), "{jf}");
        assert_eq!(
            u64::from(total),
            web["TotalRecordCount"].as_u64().unwrap(),
            "{jf}"
        );
    }
}

/// `Library.detail` on `session` for `id`: the detail as the Jellyfin fields
/// the test compares, or the error kind's `notFound` / `unavailable` text.
async fn wire_detail(
    session: &session::Client,
    id: pharos_jellyfin_api::dto::WireId,
) -> Result<serde_json::Value, String> {
    use pharos_api::pharos_capnp::{detail_result, error};
    use pharos_jellyfin_api::dto::WireId;
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.detail_request();
    req.get().init_ctx();
    let mut out = req.get().init_id();
    match id {
        WireId::Real(n) => out.set_real(n),
        WireId::Synth(n) => out.set_synth(n),
    }
    let reply = req.send().promise.await.expect("detail");
    let result = reply.get().unwrap().get_result().unwrap();
    match result.which().unwrap() {
        detail_result::Detail(d) => {
            let d = d.unwrap();
            let mut row = summary(d.get_summary().unwrap());
            row["Overview"] = d.get_overview().unwrap().to_str().unwrap().into();
            row["Genres"] = d
                .get_genres()
                .unwrap()
                .iter()
                .map(|g| g.unwrap().to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
                .into();
            row["Chapters"] = d
                .get_chapters()
                .unwrap()
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "Name": c.get_name().unwrap().to_str().unwrap(),
                        "StartMs": c.get_start_ms(),
                    })
                })
                .collect::<Vec<_>>()
                .into();
            Ok(row)
        }
        detail_result::Error(e) => Err(match e.unwrap().get_kind().which().unwrap() {
            error::kind::NotFound(what) => {
                format!("not found: {}", what.unwrap().to_str().unwrap())
            }
            error::kind::Unavailable(what) => {
                format!("unavailable: {}", what.unwrap().to_str().unwrap())
            }
            _ => "other".to_owned(),
        }),
    }
}

/// T034 over the wire: `Library.detail` shows what `/Users/{id}/Items/{id}`
/// shows for the same user and item: its summary, overview, genres and
/// chapters.
/// Disarm check: with `detail` leaving out the chapters (or the genres),
/// the agreement fails.
#[actix_web::test]
async fn native_detail_agrees_with_the_jellyfin_item_route() {
    use actix_web::test;
    use pharos_core::{LibraryStore as _, MediaChapter, MediaItem, MediaKind, MediaStore as _};
    use pharos_jellyfin_api::dto::WireId;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let mut item = MediaItem {
        id: 103,
        path: "/libA/Chaptered.mkv".into(),
        title: "Chaptered".into(),
        kind: MediaKind::Movie,
        ..MediaItem::default()
    };
    item.metadata.overview = Some("Told in two parts.".into());
    item.metadata.critic_rating = Some(83.4);
    item.probe.genre = Some("Drama".into());
    item.probe.chapters = vec![
        MediaChapter {
            start_ms: 0,
            end_ms: 60_000,
            title: "Opening".into(),
        },
        MediaChapter {
            start_ms: 60_000,
            end_ms: 120_000,
            title: "Close".into(),
        },
    ];
    fx.state.stores.put(item).await.unwrap();
    fx.state.stores.backfill_library_ids().await.unwrap();
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let uid = fx.user_id.0.simple();

    for id in [103, fx.rich_item_id] {
        let native = wire_detail(&session, WireId::Real(id)).await.unwrap();
        let req = test::TestRequest::get()
            .uri(&format!("/Users/{uid}/Items/{}", WireId::Real(id)))
            .insert_header(("X-Emby-Token", fx.user_token.as_str()))
            .to_request();
        let web: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        let mut want = project(std::slice::from_ref(&web)).remove(0);
        want["Overview"] = web
            .get("Overview")
            .and_then(|o| o.as_str())
            .unwrap_or("")
            .into();
        want["Genres"] = web["Genres"].clone();
        want["Chapters"] = web["Chapters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                serde_json::json!({
                    "Name": c["Name"],
                    "StartMs": c["StartPositionTicks"].as_u64().unwrap() / 10_000,
                })
            })
            .collect::<Vec<_>>()
            .into();
        assert_eq!(native, want, "item {id}");
    }
    assert_eq!(
        wire_detail(&session, WireId::Real(103)).await.unwrap()["CriticRating"],
        83
    );
    assert_eq!(
        wire_detail(&session, WireId::Real(103)).await.unwrap()["Chapters"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

/// T034: an item the policy hides answers `notFound`, exactly like an id
/// that names nothing, so a detail call reveals nothing about what exists;
/// the same holds for a library view the policy hides.
/// Disarm check: with the access check removed from
/// `services::library::detail`, the hidden item answers its detail.
#[actix_web::test]
async fn a_hidden_item_answers_like_a_missing_one() {
    use pharos_jellyfin_api::dto::WireId;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    fx.state
        .stores
        .set_policy(
            fx.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![fx.lib_b_wire.clone()],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    assert!(wire_detail(&session, WireId::Real(fx.other_item_id))
        .await
        .is_ok());
    let hidden = wire_detail(&session, WireId::Real(fx.rich_item_id)).await;
    let missing = wire_detail(&session, WireId::Real(987_654_321)).await;
    assert_eq!(hidden, Err("not found: item".to_owned()));
    assert_eq!(hidden, missing);
    let hidden_view = WireId::parse(&fx.lib_a_wire).unwrap();
    assert_eq!(
        wire_detail(&session, hidden_view).await,
        Err("not found: item".to_owned())
    );
    let allowed_view = WireId::parse(&fx.lib_b_wire).unwrap();
    assert_eq!(
        wire_detail(&session, allowed_view).await.unwrap()["Name"],
        "Movies B"
    );
}

/// A refused item call is visible: one INFO line with the call, the id kind,
/// the reason and the id, and a count under
/// `pharos_rpc_item_refused_total{call,id_kind,reason}`; a real and a synth
/// refusal count under distinct series.
/// Disarm check: with `record_refusal` not incrementing the counter (or not
/// logging), this fails.
#[actix_web::test]
async fn refused_item_calls_are_logged_and_counted() {
    use crate::telemetry::{connect, counter, session_for, subscriber, Captured};
    use pharos_jellyfin_api::dto::WireId;

    let fx = crate::common::seed_rich().await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let synth = WireId::Synth(0x1234_5678);
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            assert!(wire_detail(&session, synth).await.is_err());
            assert!(wire_detail(&session, WireId::Real(987_654_321))
                .await
                .is_err());
        })
        .await;
    let series = |id_kind, reason| {
        counter(
            &snapshotter,
            "pharos_rpc_item_refused_total",
            &[("call", "detail"), ("id_kind", id_kind), ("reason", reason)],
        )
    };
    assert_eq!(series("synth", "not_found"), 1);
    assert_eq!(series("real", "not_found"), 1);
    assert_eq!(series("real", "synth_hidden"), 0);
    let text = logs.text();
    assert!(
        text.contains("INFO")
            && text.contains("native item call: refused")
            && text.contains("id_kind=\"synth\"")
            && text.contains("reason=\"not_found\"")
            && text.contains(&format!("item.id={synth}")),
        "{text}"
    );
}

/// Adds a TV library at /libTV with "Show" (two seasons, G) and "Mature"
/// (one season, R), and returns its wire id.
pub(crate) async fn seed_tv_library(fx: &crate::common::Fixture) -> String {
    use pharos_core::{
        LibraryKind, LibraryStore as _, MediaItem, MediaKind, MediaMetadata, MediaStore as _,
        SeriesInfo,
    };
    let wire = "c".repeat(32);
    fx.state
        .stores
        .upsert_library("TV", "/libTV", LibraryKind::TvShows, &wire)
        .await
        .unwrap();
    let episodes = [
        (601, "Show", 1, 1, "G"),
        (602, "Show", 1, 2, "G"),
        (603, "Show", 2, 1, "G"),
        (604, "Mature", 1, 1, "R"),
    ];
    for (id, show, season, ep, rating) in episodes {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/libTV/{show}/Season {season:02}/e{ep}.mkv").into(),
                title: format!("{show} {season}x{ep}"),
                kind: MediaKind::Episode,
                metadata: MediaMetadata {
                    official_rating: Some(rating.into()),
                    ..MediaMetadata::default()
                },
                series: Some(SeriesInfo {
                    series_name: show.into(),
                    season_number: Some(season),
                    episode_number: Some(ep),
                    series_folder: Some(format!("/libTV/{show}")),
                    series_year: None,
                    series_key: None,
                }),
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    let libraries = fx.state.stores.libraries().await.unwrap();
    fx.state.set_libraries(libraries);
    wire
}

/// What the native UI does to open a library, a series and a season: detail
/// for the title, then the listing. Both must answer at every level, as a
/// seeded user, and name the same things the level above listed.
/// Disarm check: with `Library.detail` refusing synthesised ids again, the
/// first `wire_detail` fails.
#[actix_web::test]
async fn browsing_a_library_a_series_and_a_season_answers_detail_then_listing() {
    use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key, WireId};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let tv = seed_tv_library(&fx).await;

    let list = |parent: String| {
        let session = &session;
        async move {
            let (_, items) = wire_query(
                session,
                Some(&parent),
                &[],
                "SortName",
                false,
                "",
                &[],
                0,
                0,
            )
            .await;
            items
                .iter()
                .map(|i| {
                    (
                        i["Id"].as_str().unwrap().to_owned(),
                        i["Name"].as_str().unwrap().to_owned(),
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    let detail = |id: String| {
        let session = &session;
        async move {
            wire_detail(session, WireId::parse(&id).unwrap())
                .await
                .unwrap_or_else(|e| panic!("detail {id}: {e}"))
        }
    };

    assert_eq!(detail(tv.clone()).await["Name"], "TV");
    let in_library = list(tv).await;
    assert_eq!(in_library.len(), 4, "{in_library:?}");

    // The ids a title screen reaches through an episode's parents.
    let show_id = series_id_for_key(Some("/libTV/Show"), "Show");
    assert_eq!(detail(show_id.clone()).await["Name"], "Show");
    assert_eq!(list(show_id).await.len(), 3);

    let season_id = season_id_for_key(Some("/libTV/Show"), "Show", 1);
    let season = detail(season_id.clone()).await;
    assert_eq!(season["Name"], "Season 1");
    assert_eq!(season["SeriesName"], "Show");
    assert_eq!(list(season_id).await.len(), 2);
}

/// The parent `Library.detail` names for `id`, as a wire id.
async fn wire_parent(
    session: &session::Client,
    id: pharos_jellyfin_api::dto::WireId,
) -> Option<String> {
    use pharos_api::pharos_capnp::{detail_result, item_id};
    use pharos_jellyfin_api::dto::WireId;
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.detail_request();
    req.get().init_ctx();
    let mut out = req.get().init_id();
    match id {
        WireId::Real(n) => out.set_real(n),
        WireId::Synth(n) => out.set_synth(n),
    }
    let reply = req.send().promise.await.expect("detail");
    let result = reply.get().unwrap().get_result().unwrap();
    let detail_result::Detail(d) = result.which().unwrap() else {
        panic!("detail {id} refused");
    };
    let summary = d.unwrap().get_summary().unwrap();
    if !summary.has_parent_id() {
        return None;
    }
    Some(
        match summary.get_parent_id().unwrap().which().unwrap() {
            item_id::Real(n) => WireId::Real(n),
            item_id::Synth(n) => WireId::Synth(n),
        }
        .to_string(),
    )
}

/// B318: every step of the UI's breadcrumb trail names the step above it,
/// up to the innermost library: an episode its season, a season its series,
/// a series and a film their library, even under a mixed library that holds
/// them all.
/// Disarm check: with `series_dto` leaving `parent_id` unset, the series
/// names no parent; with `library_parent_id` taking the first root that
/// matches, the series names the mixed library.
#[actix_web::test]
async fn detail_names_each_parent_up_to_the_innermost_library() {
    use pharos_core::{LibraryKind, LibraryStore as _};
    use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key, WireId};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    fx.state
        .stores
        .upsert_library("Everything", "/", LibraryKind::Mixed, &"d".repeat(32))
        .await
        .unwrap();
    let tv = seed_tv_library(&fx).await;
    seed_movies(&fx).await;
    let film_library = fx
        .state
        .libraries()
        .iter()
        .find(|l| l.root_path == "/libA")
        .map(|l| l.wire_id.clone());

    let parent = |id: String| wire_parent(&session, WireId::parse(&id).unwrap());
    let show = series_id_for_key(Some("/libTV/Show"), "Show");
    let season = season_id_for_key(Some("/libTV/Show"), "Show", 1);
    assert_eq!(
        parent(WireId::Real(601).to_string()).await,
        Some(season.clone())
    );
    assert_eq!(parent(season).await, Some(show.clone()));
    assert_eq!(parent(show).await, Some(tv));
    assert!(film_library.is_some(), "the fixture has no /libA library");
    assert_eq!(parent(WireId::Real(101).to_string()).await, film_library);
}

/// A user who may not see a library, or whose parental cap rules out a
/// series' episodes, is refused the synthesised detail exactly as for an id
/// that names nothing, and the refusal is counted.
/// Disarm check: with `synth_item_json` skipping its access checks, the
/// restricted user reads the library's, series' and season's detail.
#[actix_web::test]
async fn synthesised_detail_follows_library_access_and_the_parental_cap() {
    use crate::telemetry::counter;
    use pharos_jellyfin_api::dto::{series_id_for_key, WireId};

    let fx = crate::common::seed_rich().await;
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let tv = seed_tv_library(&fx).await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = crate::telemetry::connect(&fx);
            let session = crate::telemetry::session_for(&client, &fx.user_token).await;
            let series = |name: &str| {
                let folder = format!("/libTV/{name}");
                WireId::parse(&series_id_for_key(Some(&folder), name)).unwrap()
            };
            let (show, mature) = (series("Show"), series("Mature"));
            let missing = WireId::Synth(0x1234_5678);
            let not_found = Err("not found: item".to_owned());

            // Capped at G: the R series answers like a missing id, the G one stays.
            fx.state
                .stores
                .set_policy(
                    fx.user_id,
                    UserPolicy {
                        max_parental_rating: Some(0),
                        ..UserPolicy::default()
                    },
                )
                .await
                .unwrap();
            assert!(wire_detail(&session, show).await.is_ok());
            assert_eq!(wire_detail(&session, mature).await, not_found);
            assert_eq!(wire_detail(&session, missing).await, not_found);

            // Limited to the movie libraries: the TV library and its series vanish.
            fx.state
                .stores
                .set_policy(
                    fx.user_id,
                    UserPolicy {
                        enable_all_folders: false,
                        enabled_folders: vec![fx.lib_a_wire.clone()],
                        ..UserPolicy::default()
                    },
                )
                .await
                .unwrap();
            let tv_view = WireId::parse(&tv).unwrap();
            assert_eq!(wire_detail(&session, tv_view).await, not_found);
            assert_eq!(wire_detail(&session, show).await, not_found);
        })
        .await;

    let refused = |reason| {
        counter(
            &snapshotter,
            "pharos_rpc_item_refused_total",
            &[("call", "detail"), ("id_kind", "synth"), ("reason", reason)],
        )
    };
    assert!(refused("synth_hidden") >= 2, "hidden series are counted");
    assert!(refused("not_found") >= 2, "missing ids are counted");
}

/// How many rpc `Call` messages `bytes` (whole capnp messages, back to back)
/// carries.
fn calls_in(bytes: &[u8]) -> usize {
    use capnp_rpc::rpc_capnp::message;
    let mut rest = bytes;
    let mut calls = 0;
    while !rest.is_empty() {
        let msg = capnp::serialize::read_message_from_flat_slice(
            &mut rest,
            capnp::message::ReaderOptions::new(),
        )
        .expect("whole capnp messages");
        let root: message::Reader<'_> = msg.get_root().unwrap();
        if matches!(root.which(), Ok(message::Call(_))) {
            calls += 1;
        }
    }
    calls
}

/// T035 / SC-001 over native TCP: after the bootstrap, opening home is one
/// flight. `resume → session.library → library.home` all leave the client
/// before the server's first reply arrives, and the answer fills every rail.
/// Disarm check: with `library.home` awaited after the resume instead of
/// pipelined on it, the first flight carries one call, not three.
#[actix_web::test]
async fn home_in_one_batch() {
    use pharos_api::pharos_capnp::home_result;

    let (server, fx) = crate::revocation::start().await;
    seed_movies(&fx).await;
    seed_shows(&fx).await;
    start_watching(&fx, &[fx.rich_item_id]).await;
    let tap = crate::revocation::Tap::default();
    let (client, _closed) =
        crate::revocation::connect_tapped(&server, None, None, Some(tap.clone())).await;
    // The bootstrap's own round trip, before the flight under test.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !tap.borrow().iter().any(|(sent, _)| !sent) {
        assert!(std::time::Instant::now() < deadline, "no bootstrap reply");
        actix_web::rt::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tap.borrow_mut().clear();

    let mut resume = client.resume_request();
    resume.get().init_ctx();
    resume.get().set_device_token(&fx.user_token);
    let session = resume.send().pipeline.get_result().get_session();
    let library = session.library_request().send().pipeline.get_library();
    let mut home = library.home_request();
    home.get().init_ctx();
    let reply = home.send().promise.await.expect("home");

    let first_flight: Vec<u8> = tap
        .borrow()
        .iter()
        .take_while(|(sent, _)| *sent)
        .flat_map(|(_, bytes)| bytes.clone())
        .collect();
    assert_eq!(
        calls_in(&first_flight),
        3,
        "resume, library and home in one flight"
    );

    let home_result::Home(home) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("home answered an error");
    };
    let home = home.unwrap();
    let rails = home.get_rails().unwrap();
    assert_eq!(rails.len(), 2, "one rail per library");
    for rail in rails.iter() {
        assert!(
            !rail.get_latest().unwrap().is_empty(),
            "every rail lists its newest"
        );
    }
    assert_eq!(home.get_resume().unwrap().len(), 1);
    assert!(home.get_listening().unwrap().is_empty());
    assert!(!home.get_next_up().unwrap().is_empty());
}

/// Stores `n` movies titled `Zeta 1`… in libA; the search tests share the term.
async fn seed_zeta(fx: &crate::common::Fixture, n: u64) {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _};
    for i in 1..=n {
        fx.state
            .stores
            .put(MediaItem {
                id: 300 + i,
                path: format!("/libA/Zeta {i}.mkv").into(),
                title: format!("Zeta {i}"),
                kind: MediaKind::Movie,
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
}

/// One `Library.search` call on `session`.
async fn wire_search(
    session: &session::Client,
    text: &str,
    limit: u32,
) -> (u32, Vec<serde_json::Value>) {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.search_request();
    req.get().init_ctx();
    req.get().set_text(text);
    req.get().set_limit(limit);
    let reply = req.send().promise.await.expect("search");
    summaries(reply.get().unwrap().get_result().unwrap())
}

/// T038: `Library.search` answers the items whose title matches, encoded as
/// `Library.query` encodes them, and blank text answers nothing.
/// Disarm check: with `search` not reading `text`, the first assertion sees
/// every movie, not just Alpha.
#[actix_web::test]
async fn native_search_finds_matches_encoded_like_query() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_movies(&fx).await;

    let (total, found) = wire_search(&session, "Alpha", 0).await;
    let (_, listed) = wire_query(&session, None, &[], "SortName", false, "", &[], 0, 0).await;
    let alpha: Vec<_> = listed
        .into_iter()
        .filter(|s| s["Name"] == "Alpha")
        .collect();
    assert_eq!(alpha.len(), 1);
    assert_eq!(
        (total, &found),
        (1, &alpha),
        "search must encode like query"
    );

    assert_eq!(wire_search(&session, "   ", 0).await, (0, vec![]));
    assert_eq!(wire_search(&session, "", 0).await, (0, vec![]));
}

/// T038 security: a term that matches an item in a library the user may not
/// see answers nothing, while the same term in a visible library still does.
/// Disarm check: with the visibility filter removed from
/// `services::library::search_hits`, the first assertion finds Alpha.
#[actix_web::test]
async fn native_search_leaves_out_a_hidden_library() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_movies(&fx).await;
    fx.state
        .stores
        .set_policy(
            fx.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![fx.lib_b_wire.clone()],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(wire_search(&session, "Alpha", 0).await, (0, vec![]));
    let (_, gamma) = wire_search(&session, "Gamma", 0).await;
    assert_eq!(
        gamma.iter().map(|s| s["Name"].clone()).collect::<Vec<_>>(),
        [serde_json::json!("Gamma")]
    );
}

/// A user whose policy caps the parental rating gets only the matches at or
/// below the cap: the `G` item answers, the `R` item with the same term does
/// not.
/// Disarm check: with `visible_items` ignoring `max_parental_rating`, the
/// capped assertion finds both items.
#[actix_web::test]
async fn native_search_leaves_out_an_item_above_the_parental_limit() {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaMetadata, MediaStore as _};
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    for (id, title, rating) in [(401, "Alpha Kids", "G"), (402, "Alpha Adult", "R")] {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/libA/{title}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                metadata: MediaMetadata {
                    official_rating: Some(rating.into()),
                    ..MediaMetadata::default()
                },
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();

    assert_eq!(wire_search(&session, "Alpha", 0).await.0, 2);
    fx.state
        .stores
        .set_policy(
            fx.user_id,
            UserPolicy {
                max_parental_rating: Some(0),
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();

    let (total, found) = wire_search(&session, "Alpha", 0).await;
    let names: Vec<_> = found.iter().map(|s| s["Name"].clone()).collect();
    assert_eq!(
        (total, names),
        (1, vec![serde_json::json!("Alpha Kids")]),
        "the G-capped user must not be served the R-rated match"
    );
}

/// T038: `limit` caps the answer and the total counts what was answered;
/// 0 is the default page, which holds all five here.
#[actix_web::test]
async fn native_search_honours_the_limit() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_zeta(&fx, 5).await;

    let (total, two) = wire_search(&session, "Zeta", 2).await;
    assert_eq!((total, two.len()), (2, 2));
    let (total, all) = wire_search(&session, "Zeta", 0).await;
    assert_eq!((total, all.len()), (5, 5));
}

/// T038 parity: `Library.search` lists the same stored item ids, in the same
/// order, as `/Search/Hints` lists as item hints (the shows, artists and
/// albums V518 puts ahead of them are left out of the comparison).
/// Disarm check: a `search` that reverses the store order breaks the
/// comparison.
#[actix_web::test]
async fn native_search_agrees_with_the_jellyfin_search_hints_route() {
    use actix_web::test;
    use pharos_jellyfin_api::dto::WireId;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_movies(&fx).await;
    seed_zeta(&fx, 5).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;

    for term in ["Zeta", "a"] {
        let (_, native) = wire_search(&session, term, 3).await;
        let req = test::TestRequest::get()
            .uri(&format!("/Search/Hints?searchTerm={term}&Limit=3"))
            .insert_header(("X-Emby-Token", fx.user_token.as_str()))
            .to_request();
        let web: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        let hinted: Vec<String> = web["SearchHints"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h["IsFolder"] == false)
            .map(|h| h["ItemId"].as_str().unwrap().to_owned())
            .collect();
        let ids: Vec<String> = native
            .iter()
            .map(|s| s["Id"].as_str().unwrap().to_owned())
            .filter(|id| matches!(WireId::parse(id), Some(WireId::Real(_))))
            .collect();
        assert!(!ids.is_empty(), "{term}: the term must match something");
        assert_eq!(ids, hinted, "{term}");
    }
}

/// The ids a `Library.search` for `text` lists, in order.
async fn searched(session: &session::Client, text: &str) -> Vec<String> {
    let (_, found) = wire_search(session, text, 0).await;
    found
        .iter()
        .map(|s| s["Id"].as_str().unwrap().to_owned())
        .collect()
}

/// V518: a show's name finds the show ahead of its episodes, and a trailing
/// `s01` or `s01e02` (`s1`, `s01e2`, `s1 e2`) finds that season or episode.
/// Disarm check: with `services::search` leaving out shows, "Show" lists
/// only episodes and the first assertion fails.
#[actix_web::test]
async fn native_search_names_a_show_then_its_season_or_episode() {
    use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key, WireId};
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_shows(&fx).await;
    let show = series_id_for_key(Some("/libA/Show"), "Show");
    let season = season_id_for_key(Some("/libA/Show"), "Show", 1);
    let real = |id| WireId::Real(id).to_string();

    let found = searched(&session, "show").await;
    assert_eq!(found.first(), Some(&show), "{found:?}");
    assert!(found[1..].iter().all(|id| *id != show));
    for text in ["Show s01", "show s1", "SHOW S1"] {
        assert_eq!(
            searched(&session, text).await.first(),
            Some(&season),
            "{text}"
        );
    }
    for text in ["Show s01e02", "show s01e2", "show s1 e2"] {
        assert_eq!(
            searched(&session, text).await.first(),
            Some(&real(402)),
            "{text}"
        );
    }
    assert!(searched(&session, "show s02").await.is_empty());
}

/// V518: an artist's name finds the artist and then their albums, and an
/// album's name finds the album ahead of tracks of the same name.
/// Disarm check: with `music_groups_named` leaving out an exact artist's own
/// albums, "alpha" lists only the artist.
#[actix_web::test]
async fn native_search_names_an_artist_with_albums_and_an_album_before_its_tracks() {
    use pharos_jellyfin_api::dto::{album_id_for, artist_id_for, WireId};
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_music_libraries(&fx).await;

    let found = searched(&session, "alpha").await;
    assert_eq!(
        found[..3],
        [
            artist_id_for("Alpha"),
            album_id_for("First"),
            album_id_for("Second")
        ],
        "{found:?}"
    );
    let found = searched(&session, "second").await;
    assert_eq!(
        found[..2],
        [album_id_for("Second"), WireId::Real(702).to_string()],
        "{found:?}"
    );
}

/// V558: an album is named by words from its name and its artist's, in any
/// order and any case ("go moby" finds "Go: The Very Best of Moby"), and,
/// when nothing matches as typed, with a typo in a word.
/// Disarm check: with the store's search term matched as one phrase again,
/// "beta third" names nothing; with the loose pass dropped, "thrid beta"
/// names nothing.
#[actix_web::test]
async fn native_search_names_an_album_by_words_from_its_name_and_artist() {
    use pharos_jellyfin_api::dto::album_id_for;
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_music_libraries(&fx).await;

    for text in [
        "beta third",
        "Third BETA",
        "thrid beta",
        "beta thrd",
        "btea third",
    ] {
        assert_eq!(
            searched(&session, text).await.first(),
            Some(&album_id_for("Third")),
            "{text}"
        );
    }
    // Every word must be found: "beta" alone is not the album "First".
    assert!(!searched(&session, "beta first")
        .await
        .contains(&album_id_for("First")));
}

/// V518: a show, artist or album in a library the user may not see is never
/// named by a search, and one in a library they may see still is.
/// Disarm check: with `apply_policy_scope` dropped from `music_groups_named`,
/// "alpha" names the hidden artist. A show is scoped twice, by
/// `shows_matching` and by the tile's own listing.
#[actix_web::test]
async fn native_search_names_no_show_artist_or_album_from_a_hidden_library() {
    use pharos_jellyfin_api::dto::{album_id_for, artist_id_for, series_id_for_key};
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_shows(&fx).await;
    seed_music_libraries(&fx).await;
    fx.state
        .stores
        .set_policy(
            fx.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![fx.lib_b_wire.clone(), "e".repeat(32)],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();

    let hidden = [
        series_id_for_key(Some("/libA/Show"), "Show"),
        artist_id_for("Alpha"),
        album_id_for("First"),
    ];
    for text in ["show", "alpha", "first", "show s01"] {
        let found = searched(&session, text).await;
        assert!(
            found.iter().all(|id| !hidden.contains(id)),
            "{text}: {found:?}"
        );
    }
    assert_eq!(
        searched(&session, "another").await.first(),
        Some(&series_id_for_key(Some("/libB/Another"), "Another"))
    );
    assert_eq!(
        searched(&session, "gamma").await.first(),
        Some(&artist_id_for("Gamma"))
    );
}

/// The names (and index numbers) a `Library.query` page lists, in order.
async fn listed(
    session: &session::Client,
    parent: &str,
    kinds: &[Kind],
    sort_by: &str,
) -> Vec<(String, Option<u64>)> {
    let (_, items) = wire_query(session, Some(parent), kinds, sort_by, false, "", &[], 0, 0).await;
    items
        .iter()
        .map(|i| {
            (
                i["Name"].as_str().unwrap().to_owned(),
                i["IndexNumber"].as_u64(),
            )
        })
        .collect()
}

/// The error kind a `Library.query` answers instead of a page.
async fn query_refusal(session: &session::Client, parent: &str, kinds: &[Kind]) -> String {
    use pharos_api::pharos_capnp::error;
    use pharos_jellyfin_api::dto::WireId;
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.query_request();
    req.get().init_ctx();
    let mut q = req.get().init_q();
    match WireId::parse(parent).unwrap() {
        WireId::Real(n) => q.reborrow().init_parent_id().set_real(n),
        WireId::Synth(n) => q.reborrow().init_parent_id().set_synth(n),
    }
    let mut list = q.init_kinds(kinds.len() as u32);
    for (i, kind) in kinds.iter().enumerate() {
        list.set(i as u32, *kind);
    }
    let reply = req.send().promise.await.unwrap();
    let items_result::Error(e) = reply.get().unwrap().get_result().unwrap().which().unwrap() else {
        panic!("the listing answered a page");
    };
    match e.unwrap().get_kind().which().unwrap() {
        error::kind::NotPermitted(what) => {
            format!("not permitted: {}", what.unwrap().to_str().unwrap())
        }
        error::kind::Unavailable(what) => {
            format!("unavailable: {}", what.unwrap().to_str().unwrap())
        }
        _ => "other".to_owned(),
    }
}

/// The native browse hierarchy for TV (V230): a library's series, a series'
/// seasons in number order, a season's episodes in episode order, each a
/// grouping the same `Library.query` answers when asked for the kind.
/// Seasons outside a series are refused, not folded across shows.
/// Disarm check: with `grouping_of` answering `None`, the series listing is
/// empty (B153's "matches nothing").
#[actix_web::test]
async fn a_tv_library_lists_its_series_then_seasons_then_episodes() {
    use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let tv = seed_tv_library(&fx).await;

    assert_eq!(
        listed(&session, &tv, &[Kind::Series], "SortName").await,
        [("Mature".to_owned(), None), ("Show".to_owned(), None)]
    );

    let show = series_id_for_key(Some("/libTV/Show"), "Show");
    assert_eq!(
        listed(&session, &show, &[Kind::Season], "").await,
        [
            ("Season 1".to_owned(), Some(1)),
            ("Season 2".to_owned(), Some(2))
        ]
    );

    let season = season_id_for_key(Some("/libTV/Show"), "Show", 1);
    let episodes = listed(&session, &season, &[], "ParentIndexNumber,IndexNumber").await;
    let numbers: Vec<_> = episodes.iter().map(|(_, n)| *n).collect();
    assert_eq!(numbers, [Some(1), Some(2)], "{episodes:?}");

    assert_eq!(
        query_refusal(&session, &tv, &[Kind::Season]).await,
        "not permitted: seasons outside a series"
    );
}

/// A user capped below a series' rating sees no tile for it: the grouping
/// folds only the episodes the policy lets through (012 FR-007).
#[actix_web::test]
async fn grouped_listings_follow_the_parental_cap() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let tv = seed_tv_library(&fx).await;
    fx.state
        .stores
        .set_policy(
            fx.user_id,
            UserPolicy {
                max_parental_rating: Some(0),
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        listed(&session, &tv, &[Kind::Series], "SortName").await,
        [("Show".to_owned(), None)]
    );
}

/// The series tiles a TV library lists name the same things, in the same
/// order, as the Jellyfin route lists for `IncludeItemTypes=Series`; a page
/// holds the tiles it was asked for while the total counts them all; and a
/// user barred from the library is listed none.
#[actix_web::test]
async fn series_tiles_agree_with_the_jellyfin_route_page_and_follow_library_access() {
    use actix_web::test;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let tv = seed_tv_library(&fx).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let series = |start, limit| {
        wire_query(
            &session,
            Some(&tv),
            &[Kind::Series],
            "SortName",
            false,
            "",
            &[],
            start,
            limit,
        )
    };

    let (total, native) = series(0, 0).await;
    assert_eq!(total, 2);
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Users/{}/Items?ParentId={tv}&IncludeItemTypes=Series&SortBy=SortName",
            fx.user_id.0.simple()
        ))
        .insert_header(("X-Emby-Token", fx.user_token.as_str()))
        .to_request();
    let web: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(native, project(web["Items"].as_array().unwrap()));

    let (total, first) = series(0, 1).await;
    let (_, second) = series(1, 1).await;
    assert_eq!((total, first.len(), second.len()), (2, 1, 1));
    assert_ne!(first[0]["Id"], second[0]["Id"]);

    fx.state
        .stores
        .set_policy(
            fx.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![fx.lib_a_wire.clone()],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(series(0, 0).await.0, 0);
}

/// Adds music libraries at /libMusic (artists "Alpha", with albums "First"
/// on two discs and "Second", and "Beta", album "Third") and /libOther
/// (artist "Gamma"), and returns the first's wire id.
pub async fn seed_music_libraries(fx: &crate::common::Fixture) -> String {
    use pharos_core::{
        LibraryKind, LibraryStore as _, MediaItem, MediaKind, MediaProbe, MediaStore as _,
    };
    let wire = "d".repeat(32);
    fx.state
        .stores
        .upsert_library("Music", "/libMusic", LibraryKind::Music, &wire)
        .await
        .unwrap();
    fx.state
        .stores
        .upsert_library("Other", "/libOther", LibraryKind::Music, &"e".repeat(32))
        .await
        .unwrap();
    let tracks = [
        (701, "libMusic", "Alpha", "First", 2, 1, "Disc two opener"),
        (702, "libMusic", "Alpha", "First", 1, 2, "Second song"),
        (703, "libMusic", "Alpha", "First", 1, 1, "Opener"),
        (704, "libMusic", "Alpha", "Second", 1, 1, "Only song"),
        (705, "libMusic", "Beta", "Third", 1, 1, "Beta song"),
        (706, "libOther", "Gamma", "Elsewhere", 1, 1, "Gamma song"),
    ];
    for (id, lib, artist, album, disc, track, title) in tracks {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/{lib}/{artist}/{album}/{disc}-{track}.flac").into(),
                title: title.into(),
                kind: MediaKind::Audio,
                probe: MediaProbe {
                    artist: Some(artist.into()),
                    album_artist: Some(artist.into()),
                    album: Some(album.into()),
                    disc_number: Some(disc),
                    track_number: Some(track),
                    ..MediaProbe::default()
                },
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    let libraries = fx.state.stores.libraries().await.unwrap();
    fx.state.set_libraries(libraries);
    wire
}

/// The native browse hierarchy for music (V230): a library's artists (only
/// its own), an artist's albums, an album's tracks in disc then track order.
/// Disarm check: with `scoped_tracks` ignoring the parent, "Gamma" from the
/// other library joins the artist list.
#[actix_web::test]
async fn a_music_library_lists_its_artists_then_albums_then_tracks() {
    use pharos_jellyfin_api::dto::{album_id_for, artist_id_for};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let music = seed_music_libraries(&fx).await;
    let names =
        |rows: Vec<(String, Option<u64>)>| rows.into_iter().map(|(n, _)| n).collect::<Vec<_>>();

    assert_eq!(
        names(listed(&session, &music, &[Kind::Artist], "").await),
        ["Alpha", "Beta"]
    );
    assert_eq!(
        names(listed(&session, &music, &[Kind::Album], "SortName").await),
        ["First", "Second", "Third"]
    );
    assert_eq!(
        names(
            listed(
                &session,
                &artist_id_for("Alpha"),
                &[Kind::Album],
                "SortName"
            )
            .await
        ),
        ["First", "Second"]
    );
    let (_, beta) = wire_query(
        &session,
        Some(&music),
        &[Kind::Artist],
        "",
        false,
        "b",
        &[],
        0,
        0,
    )
    .await;
    assert_eq!(
        beta.len(),
        1,
        "the letter picker filters artists by name: {beta:?}"
    );
    let (_, desc) = wire_query(
        &session,
        Some(&music),
        &[Kind::Artist],
        "",
        true,
        "",
        &[],
        0,
        0,
    )
    .await;
    assert_eq!(desc[0]["Name"], "Beta", "descending lists artists Z to A");
    assert_eq!(
        names(
            listed(
                &session,
                &album_id_for("First"),
                &[],
                "ParentIndexNumber,IndexNumber"
            )
            .await
        ),
        ["Opener", "Second song", "Disc two opener"]
    );
}

/// B580: a track's breadcrumb trail climbs to the Music library: a track
/// names its album, an album its artist (or its library when untagged), an
/// artist the library holding most of its tracks.
/// Disarm check: with `synth_album_or_artist` leaving `parent_id` unset,
/// the album names no parent and the trail stops there.
#[actix_web::test]
async fn a_tracks_parents_climb_through_album_and_artist_to_its_library() {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaProbe, MediaStore as _};
    use pharos_jellyfin_api::dto::{album_id_for, artist_id_for, WireId};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let music = seed_music_libraries(&fx).await;
    // An album with no artist tags, and one more "Alpha" song in the other
    // library (Alpha keeps four of five in Music).
    for (id, path, artist, album) in [
        (707, "/libMusic/Loose/1-1.flac", None, "Loose"),
        (
            708,
            "/libOther/Alpha/Stray/1-1.flac",
            Some("Alpha"),
            "Stray",
        ),
    ] {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: path.into(),
                title: "Song".into(),
                kind: MediaKind::Audio,
                probe: MediaProbe {
                    artist: artist.map(Into::into),
                    album_artist: artist.map(Into::into),
                    album: Some(album.into()),
                    ..MediaProbe::default()
                },
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    let parent = |id: String| wire_parent(&session, WireId::parse(&id).unwrap());

    assert_eq!(
        parent(WireId::Real(701).to_string()).await,
        Some(album_id_for("First"))
    );
    assert_eq!(
        parent(album_id_for("First")).await,
        Some(artist_id_for("Alpha"))
    );
    assert_eq!(parent(album_id_for("Loose")).await, Some(music.clone()));
    assert_eq!(parent(artist_id_for("Alpha")).await, Some(music));
    assert_eq!(
        parent(artist_id_for("Gamma")).await,
        Some("e".repeat(32)),
        "an artist sits under the library that holds it"
    );
}

/// Every native `Library.query` counts under
/// `pharos_rpc_query_total{shape,outcome}`: which listing answered it and
/// whether it had a page, a refusal counted as `error`.
/// Disarm check: with `write_grouped` skipping `record_query`, the series
/// and season counts stay at 0.
#[actix_web::test]
async fn native_queries_are_counted_by_shape_and_outcome() {
    use crate::telemetry::counter;
    use pharos_jellyfin_api::dto::series_id_for_key;

    let fx = crate::common::seed_rich().await;
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let tv = seed_tv_library(&fx).await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = crate::telemetry::connect(&fx);
            let session = crate::telemetry::session_for(&client, &fx.user_token).await;
            listed(&session, &tv, &[Kind::Series], "").await;
            let show = series_id_for_key(Some("/libTV/Show"), "Show");
            listed(&session, &show, &[Kind::Season], "").await;
            listed(&session, &show, &[], "").await;
            query_refusal(&session, &tv, &[Kind::Season]).await;
        })
        .await;
    let count = |shape, outcome| {
        counter(
            &snapshotter,
            "pharos_rpc_query_total",
            &[("shape", shape), ("outcome", outcome)],
        )
    };
    assert_eq!(count("series", "ok"), 1);
    assert_eq!(count("season", "ok"), 1);
    assert_eq!(count("season", "error"), 1);
    assert_eq!(count("stored", "ok"), 1);
}

/// 013 FR-003/FR-005 over the wire: `Library.detail` lists the tracks
/// PlaybackInfo lists and the segments `/MediaSegments/{id}` serves, so the
/// native player and jellyfin-web offer the same picks and skips. A text
/// subtitle comes with its WebVTT; an image one plays only burned in.
/// Disarm check: with `detail` writing no segments, or a PGS track given a
/// URL, the comparison fails.
#[actix_web::test]
async fn native_detail_lists_the_tracks_and_segments_the_jellyfin_routes_serve() {
    use actix_web::test;
    use pharos_api::pharos_capnp::{detail_result, SegmentKind, SubtitleDelivery};
    use pharos_core::{
        AudioTrack, DetectedSegment, MediaChapter, MediaItem, MediaKind, MediaSegmentKind,
        MediaSegmentStore as _, MediaStore as _, SubtitleTrack,
    };
    use pharos_jellyfin_api::dto::WireId;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let mut item = MediaItem {
        id: 104,
        path: "/libA/Tracked.mkv".into(),
        title: "Tracked".into(),
        kind: MediaKind::Movie,
        ..MediaItem::default()
    };
    item.probe.duration_ms = Some(1_800_000);
    item.probe.video_codec = Some("h264".into());
    item.probe.audio_tracks = vec![
        AudioTrack {
            stream_index: 1,
            codec: Some("aac".into()),
            language: Some("eng".into()),
            is_default: true,
            ..AudioTrack::default()
        },
        AudioTrack {
            stream_index: 2,
            codec: Some("ac3".into()),
            language: Some("jpn".into()),
            title: Some("Japanese".into()),
            ..AudioTrack::default()
        },
    ];
    item.probe.subtitle_tracks = vec![
        SubtitleTrack {
            stream_index: 3,
            codec: Some("subrip".into()),
            language: Some("eng".into()),
            is_forced: true,
            ..SubtitleTrack::default()
        },
        SubtitleTrack {
            stream_index: 4,
            codec: Some("hdmv_pgs_subtitle".into()),
            language: Some("eng".into()),
            ..SubtitleTrack::default()
        },
    ];
    item.probe.chapters = vec![
        MediaChapter {
            start_ms: 0,
            end_ms: 90_000,
            title: "Opening".into(),
        },
        MediaChapter {
            start_ms: 90_000,
            end_ms: 1_800_000,
            title: "Part One".into(),
        },
    ];
    fx.state.stores.put(item).await.unwrap();
    fx.state
        .stores
        .set_media_segments(
            104,
            &[DetectedSegment {
                kind: MediaSegmentKind::Outro,
                start_ms: 1_700_000,
                end_ms: 1_790_000,
                detector: "blackframe".into(),
                confidence: 0.8,
            }],
            pharos_core::SEGMENT_SCHEMA_VERSION,
        )
        .await
        .unwrap();
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;

    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.detail_request();
    req.get().init_ctx();
    req.get().init_id().set_real(104);
    let reply = req.send().promise.await.expect("detail");
    let detail_result::Detail(d) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("detail refused");
    };
    let d = d.unwrap();
    let text = |t: capnp::Result<capnp::text::Reader<'_>>| t.unwrap().to_str().unwrap().to_owned();

    let audio: Vec<(u32, String, bool)> = d
        .get_audio_tracks()
        .unwrap()
        .iter()
        .map(|t| (t.get_index(), text(t.get_language()), t.get_is_default()))
        .collect();
    let subtitles: Vec<(u32, bool, SubtitleDelivery, String)> = d
        .get_subtitle_tracks()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t.get_index(),
                t.get_is_forced(),
                t.get_delivery().unwrap(),
                text(t.get_url()),
            )
        })
        .collect();
    let id = WireId::Real(104);
    assert_eq!(
        audio,
        vec![(1, "eng".to_owned(), true), (2, "jpn".to_owned(), false)]
    );
    assert_eq!(
        subtitles,
        vec![
            (
                3,
                true,
                SubtitleDelivery::External,
                format!("/videos/{id}/subtitles/3/stream.vtt")
            ),
            (4, false, SubtitleDelivery::Burn, String::new()),
        ]
    );

    // The same streams PlaybackInfo hands jellyfin-web, by index and kind.
    let req = test::TestRequest::post()
        .uri(&format!("/Items/{id}/PlaybackInfo"))
        .insert_header(("X-Emby-Token", fx.user_token.as_str()))
        .set_json(serde_json::json!({}))
        .to_request();
    let info: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let jf: Vec<(String, u64)> = info["MediaSources"][0]["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["Type"] != "Video")
        .map(|s| {
            (
                s["Type"].as_str().unwrap().to_owned(),
                s["Index"].as_u64().unwrap(),
            )
        })
        .collect();
    let native: Vec<(String, u64)> = audio
        .iter()
        .map(|a| ("Audio".to_owned(), u64::from(a.0)))
        .chain(
            subtitles
                .iter()
                .map(|s| ("Subtitle".to_owned(), u64::from(s.0))),
        )
        .collect();
    assert_eq!(native, jf);

    // The WebVTT route serves the URL the detail names (the fixture has no
    // file: anything but 401/404-route got past auth and routing).
    let vtt = test::TestRequest::get()
        .uri(&subtitles[0].3)
        .insert_header(("X-Emby-Token", fx.user_token.as_str()))
        .to_request();
    let status = test::call_service(&app, vtt).await.status();
    assert_ne!(status, actix_web::http::StatusCode::UNAUTHORIZED);

    let segments: Vec<(SegmentKind, u64, u64)> = d
        .get_segments()
        .unwrap()
        .iter()
        .map(|s| (s.get_kind().unwrap(), s.get_start_ms(), s.get_end_ms()))
        .collect();
    assert_eq!(
        segments,
        vec![
            (SegmentKind::Intro, 0, 90_000),
            (SegmentKind::Outro, 1_700_000, 1_790_000)
        ]
    );
    let req = test::TestRequest::get()
        .uri(&format!("/MediaSegments/{id}"))
        .insert_header(("X-Emby-Token", fx.user_token.as_str()))
        .to_request();
    let jf: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let jf: Vec<(String, u64, u64)> = jf["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["Type"].as_str().unwrap().to_owned(),
                s["StartTicks"].as_u64().unwrap() / 10_000,
                s["EndTicks"].as_u64().unwrap() / 10_000,
            )
        })
        .collect();
    assert_eq!(
        jf,
        vec![
            ("Intro".to_owned(), 0, 90_000),
            ("Outro".to_owned(), 1_700_000, 1_790_000)
        ]
    );
    // Trickplay is off in this fixture, so no sprite set is advertised.
    assert_eq!(d.get_trickplay().unwrap().len(), 0);
}

/// A native browse is visible: one count per call under
/// `pharos_rpc_browse_total{level,kind}`, whichever listing answers it, and a
/// line with the parent, level and result count; an empty container logs at
/// INFO. It complements `pharos_rpc_query_total`, which says which listing
/// answered.
/// Disarm check: with `record_browse` not incrementing the counter (or not
/// logging), this fails.
#[actix_web::test]
async fn native_browses_are_counted_by_level_and_kind() {
    use crate::telemetry::{connect, counter, session_for, subscriber, Captured};
    use pharos_core::{LibraryKind, LibraryStore as _};
    use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key};

    let fx = crate::common::seed_rich().await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let tv = seed_tv_library(&fx).await;
    let empty = "d".repeat(32);
    fx.state
        .stores
        .upsert_library("Empty TV", "/libEmpty", LibraryKind::TvShows, &empty)
        .await
        .unwrap();
    let libraries = fx.state.stores.libraries().await.unwrap();
    fx.state.set_libraries(libraries);
    let series = series_id_for_key(Some("/libTV/Show"), "Show");
    let season = season_id_for_key(Some("/libTV/Show"), "Show", 1);
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            listed(&session, &fx.lib_a_wire, &[], "").await;
            listed(&session, &tv, &[], "").await;
            listed(&session, &tv, &[Kind::Series], "").await;
            listed(&session, &empty, &[Kind::Series], "").await;
            listed(&session, &series, &[Kind::Season], "").await;
            listed(&session, &season, &[], "").await;
            let _ = wire_query(&session, None, &[], "", false, "", &[], 0, 0).await;
        })
        .await;
    let browses = |level, kind| {
        counter(
            &snapshotter,
            "pharos_rpc_browse_total",
            &[("level", level), ("kind", kind)],
        )
    };
    assert_eq!(browses("library", "movies"), 1);
    assert_eq!(browses("library", "tvshows"), 3);
    assert_eq!(browses("series", "tvshows"), 1);
    assert_eq!(browses("season", "tvshows"), 1);
    assert_eq!(browses("other", "none"), 1);
    assert_eq!(browses("season", "movies"), 0);
    let text = logs.text();
    assert!(
        text.contains("INFO")
            && text.contains("native browse: empty listing")
            && text.contains(&format!("parent=\"{empty}\""))
            && text.contains("level=\"library\"")
            && text.contains("count=0"),
        "{text}"
    );
    assert!(
        text.contains("DEBUG")
            && text.contains(&format!("parent=\"{season}\""))
            && text.contains("native browse"),
        "{text}"
    );
}

/// The names a `Library.query` page lists under `parent` for one genre and
/// the play-state `filters`, by name.
async fn listed_in(
    session: &session::Client,
    parent: &str,
    kinds: &[Kind],
    genre: &str,
    filters: &[&str],
) -> Vec<String> {
    use pharos_jellyfin_api::dto::WireId;
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.query_request();
    req.get().init_ctx();
    let mut q = req.get().init_q();
    match WireId::parse(parent).unwrap() {
        WireId::Real(n) => q.reborrow().init_parent_id().set_real(n),
        WireId::Synth(n) => q.reborrow().init_parent_id().set_synth(n),
    }
    let mut list = q.reborrow().init_kinds(kinds.len() as u32);
    for (i, kind) in kinds.iter().enumerate() {
        list.set(i as u32, *kind);
    }
    let mut list = q.reborrow().init_filters(filters.len() as u32);
    for (i, f) in filters.iter().enumerate() {
        list.set(i as u32, *f);
    }
    q.set_sort_by("SortName");
    q.set_genre(genre);
    let reply = req.send().promise.await.expect("query");
    let (_, items) = summaries(reply.get().unwrap().get_result().unwrap());
    items
        .iter()
        .map(|i| i["Name"].as_str().unwrap().to_owned())
        .collect()
}

/// The library filter bar's genre (B312/V252): a film is in every genre it
/// is linked to, so "Action, Sci-Fi" is in both; a series is in its show
/// metadata's genres, whatever its episodes carry.
/// Disarm check: with `build_media_query` leaving `genre_wire_id` unset the
/// films list unfiltered; with the series genre check skipped "Mature" is
/// listed under Comedy.
#[actix_web::test]
async fn a_listing_keeps_to_one_genre() {
    use pharos_core::{GenreStore as _, SeriesMetadata, SeriesMetadataStore as _};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_movies(&fx).await;
    let links: [(u64, &[&str]); 2] = [(101, &["Action", "Sci-Fi"]), (102, &["Drama"])];
    for (id, genres) in links {
        let names: Vec<String> = genres.iter().map(|g| (*g).to_owned()).collect();
        fx.state.stores.link_item_genres(id, &names).await.unwrap();
    }
    let tv = seed_tv_library(&fx).await;
    for (show, genre) in [("Show", "Comedy"), ("Mature", "Drama")] {
        fx.state
            .stores
            .upsert_series_metadata(SeriesMetadata {
                series_key: format!("/libTV/{show}"),
                series_name: show.into(),
                genres: vec![genre.into()],
                ..SeriesMetadata::default()
            })
            .await
            .unwrap();
    }

    let lib_a = fx.lib_a_wire.clone();
    assert_eq!(
        listed_in(&session, &lib_a, &[], "Sci-Fi", &[]).await,
        ["Alpha"]
    );
    // seed_rich's film is Action and Drama too.
    assert_eq!(
        listed_in(&session, &lib_a, &[], "Action", &[]).await,
        ["Alpha", "Rich Movie"]
    );
    assert_eq!(
        listed_in(&session, &lib_a, &[], "Drama", &[]).await,
        ["Beta", "Rich Movie"]
    );
    assert_eq!(listed_in(&session, &lib_a, &[], "", &[]).await.len(), 3);
    assert_eq!(
        listed_in(&session, &tv, &[Kind::Series], "Comedy", &[]).await,
        ["Show"]
    );
    assert_eq!(
        listed_in(&session, &tv, &[Kind::Series], "", &[]).await,
        ["Mature", "Show"]
    );
}

/// The library filter bar's date orders (B316): `descending` means newest
/// first for both, whatever the Jellyfin route's legacy reading of a lone
/// `DateCreated` (which lists newest first when asked ascending).
/// Disarm check: with `read_query` passing one sort-order token, date added
/// lists oldest first when asked newest first.
#[actix_web::test]
async fn date_orders_list_newest_first_when_descending() {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaMetadata, MediaStore as _};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    // Name order is neither date order: Middle, Newest, Older.
    for (id, title, at) in [
        (701, "Older", 1_000),
        (702, "Middle", 2_000),
        (703, "Newest", 3_000),
    ] {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/libA/{title}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                created_at: Some(at),
                metadata: MediaMetadata {
                    premiere_date: Some(at),
                    production_year: Some(2000 + u32::try_from(at / 1_000).unwrap()),
                    ..MediaMetadata::default()
                },
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    let lib_a = fx.lib_a_wire.clone();
    let ours = ["Older", "Middle", "Newest"];
    for sort_by in [
        "DateCreated,SortName",
        "DateCreated",
        "PremiereDate,ProductionYear,SortName",
    ] {
        for (descending, want) in [
            (true, ["Newest", "Middle", "Older"]),
            (false, ["Older", "Middle", "Newest"]),
        ] {
            let (_, page) = wire_query(
                &session,
                Some(&lib_a),
                &[],
                sort_by,
                descending,
                "",
                &[],
                0,
                100,
            )
            .await;
            let names: Vec<&str> = page
                .iter()
                .filter_map(|i| i["Name"].as_str())
                .filter(|n| ours.contains(n))
                .collect();
            assert_eq!(names, want, "{sort_by}, descending {descending}");
        }
    }
}

/// The library filter bar's watched state (B312/V252): `IsPlayed` and
/// `IsUnplayed` narrow films by their own play state and series tiles by
/// whether every episode is played.
/// Disarm check: with the series play-state check in `virtual_shows_page`
/// skipped, both series list under either filter.
#[actix_web::test]
async fn a_listing_keeps_to_watched_or_unwatched() {
    use pharos_core::UserDataStore as _;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_movies(&fx).await;
    let tv = seed_tv_library(&fx).await;
    // Alpha, and every episode of "Show" but none of "Mature".
    for id in [101, 601, 602, 603] {
        fx.state
            .stores
            .set_user_data(
                fx.user_id,
                id,
                pharos_core::UserItemData {
                    played: true,
                    ..pharos_core::UserItemData::default()
                },
            )
            .await
            .unwrap();
    }

    let lib_a = fx.lib_a_wire.clone();
    let played = listed_in(&session, &lib_a, &[], "", &["IsPlayed"]).await;
    assert_eq!(played, ["Alpha"]);
    let unplayed = listed_in(&session, &lib_a, &[], "", &["IsUnplayed"]).await;
    assert!(!unplayed.contains(&"Alpha".to_owned()), "{unplayed:?}");
    assert!(unplayed.contains(&"Beta".to_owned()), "{unplayed:?}");
    assert_eq!(
        listed_in(&session, &tv, &[Kind::Series], "", &["IsPlayed"]).await,
        ["Show"]
    );
    assert_eq!(
        listed_in(&session, &tv, &[Kind::Series], "", &["IsUnplayed"]).await,
        ["Mature"]
    );
}

/// B1199/V1159: `IsResumable` is the filter bar's "In progress": a film part
/// watched and not finished, and a series tile with something watched (an
/// episode played or part watched) and something still to watch.
/// Disarm check: with the started check in `virtual_shows_page` skipped,
/// both series list.
#[actix_web::test]
async fn a_listing_keeps_to_in_progress() {
    use pharos_core::UserDataStore as _;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_movies(&fx).await;
    let tv = seed_tv_library(&fx).await;
    let store = |id: u64, data: pharos_core::UserItemData| {
        let fx = &fx;
        async move {
            fx.state
                .stores
                .set_user_data(fx.user_id, id, data)
                .await
                .unwrap();
        }
    };
    let part_watched = pharos_core::UserItemData {
        last_played_position_ticks: 600_000_000,
        ..pharos_core::UserItemData::default()
    };
    let played = pharos_core::UserItemData {
        played: true,
        ..pharos_core::UserItemData::default()
    };
    // Alpha finished, Beta part watched; one episode of "Show" played.
    store(101, played).await;
    store(102, part_watched).await;
    store(601, played).await;

    let lib_a = fx.lib_a_wire.clone();
    assert_eq!(
        listed_in(&session, &lib_a, &[], "", &["IsResumable"]).await,
        ["Beta"]
    );
    let in_progress = || listed_in(&session, &tv, &[Kind::Series], "", &["IsResumable"]);
    assert_eq!(in_progress().await, ["Show"]);
    // A part-watched episode starts a show too.
    store(604, part_watched).await;
    assert_eq!(in_progress().await, ["Mature", "Show"]);
    // Every episode played: finished, no longer in progress.
    store(602, played).await;
    store(603, played).await;
    assert_eq!(in_progress().await, ["Mature"]);
}

/// B1081: `IsFavorite` lists a series tile while every episode in it is a
/// favourite, which is what marking the show writes; one favourite episode
/// does not make its show one. `IsNotFavorite` lists the rest.
/// Disarm check: with the favourite check in `virtual_shows_page` skipped,
/// both series list under either filter.
#[actix_web::test]
async fn a_listing_keeps_to_favourite_series() {
    use pharos_core::UserDataStore as _;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let tv = seed_tv_library(&fx).await;
    let favourite = pharos_core::UserItemData {
        is_favorite: true,
        ..pharos_core::UserItemData::default()
    };
    let mark = |id: u64| {
        let fx = &fx;
        async move {
            fx.state
                .stores
                .set_user_data(fx.user_id, id, favourite)
                .await
                .unwrap();
        }
    };
    // Season 1 of "Show" only: its season 2 episode is no favourite.
    mark(601).await;
    mark(602).await;
    assert!(
        listed_in(&session, &tv, &[Kind::Series], "", &["IsFavorite"])
            .await
            .is_empty()
    );
    mark(603).await;
    assert_eq!(
        listed_in(&session, &tv, &[Kind::Series], "", &["IsFavorite"]).await,
        ["Show"]
    );
    assert_eq!(
        listed_in(&session, &tv, &[Kind::Series], "", &["IsNotFavorite"]).await,
        ["Mature"]
    );
    // The Favourites page asks with no parent, one kind at a time.
    let names = |page: Vec<serde_json::Value>| -> Vec<String> {
        page.iter()
            .map(|i| i["Name"].as_str().unwrap().to_owned())
            .collect()
    };
    let (_, shows) = wire_query(
        &session,
        None,
        &[Kind::Series],
        "SortName",
        false,
        "",
        &["IsFavorite"],
        0,
        50,
    )
    .await;
    assert_eq!(names(shows), ["Show"]);
    let (total, episodes) = wire_query(
        &session,
        None,
        &[Kind::Episode],
        "SortName",
        false,
        "",
        &["IsFavorite"],
        0,
        50,
    )
    .await;
    assert_eq!(total, 3, "{:?}", names(episodes));
}

/// B1085: album and artist listings keep to the favourite ones, an album
/// or artist being one while every track under it is, as a series is.
/// Disarm check: with `GroupMarks::keeps` keeping every group, "Third"
/// joins the favourite albums.
#[actix_web::test]
async fn a_listing_keeps_to_favourite_albums_and_artists() {
    use pharos_core::UserDataStore as _;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let music = seed_music_libraries(&fx).await;
    let favourite = pharos_core::UserItemData {
        is_favorite: true,
        ..pharos_core::UserItemData::default()
    };
    for id in [701, 702, 703] {
        fx.state
            .stores
            .set_user_data(fx.user_id, id, favourite)
            .await
            .unwrap();
    }
    assert_eq!(
        listed_in(&session, &music, &[Kind::Album], "", &["IsFavorite"]).await,
        ["First"]
    );
    assert_eq!(
        listed_in(&session, &music, &[Kind::Album], "", &["IsNotFavorite"]).await,
        ["Second", "Third"]
    );
    assert!(
        listed_in(&session, &music, &[Kind::Artist], "", &["IsFavorite"])
            .await
            .is_empty(),
        "Alpha's \"Only song\" is no favourite"
    );
    fx.state
        .stores
        .set_user_data(fx.user_id, 704, favourite)
        .await
        .unwrap();
    assert_eq!(
        listed_in(&session, &music, &[Kind::Artist], "", &["IsFavorite"]).await,
        ["Alpha"]
    );
    // The Favourites page asks with no parent, one kind at a time.
    let names = |page: Vec<serde_json::Value>| -> Vec<String> {
        page.iter()
            .map(|i| i["Name"].as_str().unwrap().to_owned())
            .collect()
    };
    let (_, albums) = wire_query(
        &session,
        None,
        &[Kind::Album],
        "SortName",
        false,
        "",
        &["IsFavorite"],
        0,
        50,
    )
    .await;
    assert_eq!(names(albums), ["First", "Second"]);
}

/// One `Library.index` call: each letter and where it starts, then genres.
async fn wire_index(
    session: &session::Client,
    parent: &str,
    kinds: &[Kind],
    genre: &str,
) -> (Vec<(String, u32)>, Vec<String>) {
    use pharos_api::pharos_capnp::index_result;
    use pharos_jellyfin_api::dto::WireId;
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.index_request();
    req.get().init_ctx();
    let mut q = req.get().init_q();
    match WireId::parse(parent).unwrap() {
        WireId::Real(n) => q.reborrow().init_parent_id().set_real(n),
        WireId::Synth(n) => q.reborrow().init_parent_id().set_synth(n),
    }
    let mut list = q.reborrow().init_kinds(kinds.len() as u32);
    for (i, kind) in kinds.iter().enumerate() {
        list.set(i as u32, *kind);
    }
    q.set_sort_by("SortName");
    q.set_genre(genre);
    let reply = req.send().promise.await.expect("index");
    let index_result::Index(index) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("index failed");
    };
    let index = index.unwrap();
    let letters = index
        .get_letters()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l.get_letter().unwrap().to_str().unwrap().to_owned(),
                l.get_start(),
            )
        })
        .collect();
    let genres = index
        .get_genres()
        .unwrap()
        .iter()
        .map(|g| g.unwrap().to_str().unwrap().to_owned())
        .collect();
    (letters, genres)
}

/// The letter index (B312/V252): each letter's start is the offset of its
/// first item in the same listing `Library.query` pages, so a page asked for
/// from there opens on that letter; it follows the genre, and the genres it
/// offers do not.
/// Disarm check: with `index` listing unsorted (or ignoring the genre), the
/// page at a letter's start opens on another letter.
#[actix_web::test]
async fn letters_start_where_the_listing_pages_them() {
    use pharos_core::{GenreStore as _, SeriesMetadata, SeriesMetadataStore as _};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_movies(&fx).await;
    fx.state
        .stores
        .link_item_genres(102, &["Drama".to_owned()])
        .await
        .unwrap();
    let lib_a = fx.lib_a_wire.clone();

    let (letters, genres) = wire_index(&session, &lib_a, &[], "").await;
    assert!(genres.contains(&"Drama".to_owned()), "{genres:?}");
    assert!(!letters.is_empty());
    for (letter, start) in &letters {
        let (_, page) = wire_query(
            &session,
            Some(&lib_a),
            &[],
            "SortName",
            false,
            "",
            &[],
            *start,
            1,
        )
        .await;
        let name = page[0]["Name"].as_str().unwrap();
        let first = pharos_core::sort_name(name)
            .chars()
            .next()
            .unwrap()
            .to_ascii_uppercase();
        assert_eq!(first.to_string(), *letter, "{name} at {start}");
    }
    let (drama, drama_genres) = wire_index(&session, &lib_a, &[], "Drama").await;
    assert_eq!(drama.first().map(|(_, s)| *s), Some(0));
    assert_eq!(drama_genres, genres);

    let tv = seed_tv_library(&fx).await;
    fx.state
        .stores
        .upsert_series_metadata(SeriesMetadata {
            series_key: "/libTV/Show".into(),
            series_name: "Show".into(),
            genres: vec!["Comedy".into()],
            ..SeriesMetadata::default()
        })
        .await
        .unwrap();
    let (letters, genres) = wire_index(&session, &tv, &[Kind::Series], "").await;
    assert_eq!(letters, [("M".to_owned(), 0), ("S".to_owned(), 1)]);
    assert_eq!(genres, ["Comedy"]);
}

/// B341/V281: a film's and a show's OMDb scores reach the native summary. A
/// film's OMDb Tomatometer outranks its NFO critic score, and the show's
/// scores, stored on its metadata row, reach its synthesised tile.
/// Disarm check: with `write_item` skipping `imdbRating`, or `series_dto`
/// leaving out `critic_rating`, the assertions fail.
#[actix_web::test]
async fn omdb_scores_reach_films_and_shows() {
    use pharos_core::{
        ExternalRatings, RatingsStore as _, RatingsTarget, SeriesMetadata, SeriesMetadataStore as _,
    };
    use pharos_jellyfin_api::dto::WireId;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_movies(&fx).await;
    let mut alpha = pharos_core::MediaStore::get(&fx.state.stores, 101)
        .await
        .unwrap();
    alpha.metadata.critic_rating = Some(50.0);
    pharos_core::MediaStore::put(&fx.state.stores, alpha)
        .await
        .unwrap();
    let scores = ExternalRatings {
        imdb: Some(8.7),
        tomatometer: Some(83),
    };
    fx.state
        .stores
        .set_ratings(&RatingsTarget::Item(101), scores, 1_000)
        .await
        .unwrap();
    let tv = seed_tv_library(&fx).await;
    fx.state
        .stores
        .upsert_series_metadata(SeriesMetadata {
            series_key: "/libTV/Show".into(),
            series_name: "Show".into(),
            ..SeriesMetadata::default()
        })
        .await
        .unwrap();
    fx.state
        .stores
        .set_ratings(
            &RatingsTarget::Series("/libTV/Show".into()),
            ExternalRatings {
                imdb: Some(9.5),
                tomatometer: Some(96),
            },
            1_000,
        )
        .await
        .unwrap();

    let film = wire_detail(&session, WireId::Real(101)).await.unwrap();
    assert_eq!(
        (&film["ImdbRating"], &film["CriticRating"]),
        (&87.into(), &83.into())
    );
    let (_, shows) = wire_query(
        &session,
        Some(&tv),
        &[Kind::Series],
        "SortName",
        false,
        "",
        &[],
        0,
        50,
    )
    .await;
    let show = shows.iter().find(|s| s["Name"] == "Show").unwrap();
    assert_eq!(
        (&show["ImdbRating"], &show["CriticRating"]),
        (&95.into(), &96.into())
    );
    let mature = shows.iter().find(|s| s["Name"] == "Mature").unwrap();
    assert!(mature["ImdbRating"].is_null() && mature["CriticRating"].is_null());
}

/// Spec 019: on a server that extracts image subtitles, the detail offers an
/// image subtitle as the PNG manifest the player draws, and a text one still
/// as WebVTT. A client built before schemaVersion 16 cannot read
/// `SubtitleDelivery.image`, so it, and one that sends no version, gets
/// `burn` as before.
/// Disarm check: with `write_tracks` writing `Drawn::Images` as `burn`, or
/// `detail` passing `draws_images: false`, the v16 PGS track reads `burn`;
/// with the schemaVersion gate dropped, the v15 one reads `image`.
#[cfg(feature = "ffmpeg-lib")]
#[actix_web::test]
async fn native_detail_offers_image_subtitles_to_draw_when_the_server_extracts_them() {
    use pharos_api::pharos_capnp::{detail_result, SubtitleDelivery};
    use pharos_core::{MediaItem, MediaKind, MediaStore as _, SubtitleTrack};
    use pharos_jellyfin_api::dto::WireId;

    let cache = tempfile::tempdir().unwrap();
    let (server, fx) = crate::revocation::start_drawing_images(cache.path()).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let mut item = MediaItem {
        id: 104,
        path: "/libA/Tracked.mkv".into(),
        title: "Tracked".into(),
        kind: MediaKind::Movie,
        ..MediaItem::default()
    };
    item.probe.video_codec = Some("h264".into());
    item.probe.subtitle_tracks = vec![
        SubtitleTrack {
            stream_index: 3,
            codec: Some("subrip".into()),
            ..SubtitleTrack::default()
        },
        SubtitleTrack {
            stream_index: 4,
            codec: Some("hdmv_pgs_subtitle".into()),
            ..SubtitleTrack::default()
        },
    ];
    fx.state.stores.put(item).await.unwrap();

    let library = session.library_request().send().pipeline.get_library();
    let id = WireId::Real(104);
    let vtt = (
        3,
        SubtitleDelivery::External,
        format!("/videos/{id}/subtitles/3/stream.vtt"),
    );
    for (schema_version, pgs) in [
        (
            16,
            (
                4,
                SubtitleDelivery::Image,
                format!("/videos/{id}/subtitles/4/images.json"),
            ),
        ),
        (15, (4, SubtitleDelivery::Burn, String::new())),
        (0, (4, SubtitleDelivery::Burn, String::new())),
    ] {
        let mut req = library.detail_request();
        req.get().init_ctx().set_schema_version(schema_version);
        req.get().init_id().set_real(104);
        let reply = req.send().promise.await.expect("detail");
        let detail_result::Detail(d) = reply.get().unwrap().get_result().unwrap().which().unwrap()
        else {
            panic!("detail refused");
        };
        let subtitles: Vec<(u32, SubtitleDelivery, String)> = d
            .unwrap()
            .get_subtitle_tracks()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t.get_index(),
                    t.get_delivery().unwrap(),
                    t.get_url().unwrap().to_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            subtitles,
            vec![vtt.clone(), pgs],
            "schemaVersion {schema_version}"
        );
    }
}

/// ADR-0062 / B564: a show's and an episode's description follow the
/// signed-in user's description language, through the native detail and the
/// Jellyfin item, episode-list and series routes alike; a user on the
/// default sees the default columns.
/// Disarm check: with `localize_items_in` / `localize_series_in` returning
/// early, the Japanese user sees the English text everywhere.
#[actix_web::test]
async fn descriptions_follow_the_users_language() {
    use actix_web::test;
    use pharos_core::{
        LocalizedText, MediaStore as _, MetadataTranslationStore as _, PreferenceStore as _,
        SeriesMetadata, SeriesMetadataStore as _, TranslationOwner,
    };
    use pharos_jellyfin_api::dto::{series_id_for_key, WireId};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    seed_tv_library(&fx).await;
    let stores = &fx.state.stores;
    let mut ep = pharos_core::MediaStore::get(stores, 601).await.unwrap();
    ep.metadata.overview = Some("Bulma meets Goku.".into());
    stores.put(ep).await.unwrap();
    stores
        .upsert_series_metadata(SeriesMetadata {
            series_key: "/libTV/Show".into(),
            series_name: "Show".into(),
            overview: Some("Goku sets out.".into()),
            ..SeriesMetadata::default()
        })
        .await
        .unwrap();
    let jpn = |overview: &str| LocalizedText {
        language: "jpn".into(),
        title: None,
        overview: Some(overview.into()),
        is_original: true,
    };
    stores
        .upsert_translations(&TranslationOwner::Item(601), &[jpn("ブルマが…")], 1)
        .await
        .unwrap();
    stores
        .upsert_translations(
            &TranslationOwner::Series("/libTV/Show".into()),
            &[jpn("悟空の冒険。")],
            1,
        )
        .await
        .unwrap();

    let uid = fx.user_id.0.simple().to_string();
    let series = series_id_for_key(Some("/libTV/Show"), "Show");
    let series_wire = WireId::parse(&series).unwrap();
    let jellyfin = |uri: String| {
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("X-Emby-Token", fx.user_token.as_str()))
            .to_request()
    };
    let item = format!("/Users/{uid}/Items/{}", WireId::Real(601));
    let episodes = format!("/Shows/{series}/Episodes?UserId={uid}&Season=1");
    let show = format!("/Users/{uid}/Items/{series}");

    macro_rules! seen {
        () => {{
            let native_ep = wire_detail(&session, WireId::Real(601)).await.unwrap();
            let native_show = wire_detail(&session, series_wire).await.unwrap();
            let jf_ep: serde_json::Value =
                test::call_and_read_body_json(&app, jellyfin(item.clone())).await;
            let jf_list: serde_json::Value =
                test::call_and_read_body_json(&app, jellyfin(episodes.clone())).await;
            let jf_show: serde_json::Value =
                test::call_and_read_body_json(&app, jellyfin(show.clone())).await;
            let listed = jf_list["Items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["Id"] == WireId::Real(601).to_string())
                .unwrap()["Overview"]
                .clone();
            [
                native_ep["Overview"].clone(),
                jf_ep["Overview"].clone(),
                listed,
                native_show["Overview"].clone(),
                jf_show["Overview"].clone(),
            ]
        }};
    }

    let default = seen!();
    assert_eq!(
        default,
        [
            "Bulma meets Goku.",
            "Bulma meets Goku.",
            "Bulma meets Goku.",
            "Goku sets out.",
            "Goku sets out."
        ]
        .map(serde_json::Value::from)
    );

    stores
        .set_user_configuration(fx.user_id, r#"{"DescriptionLanguagePreference":"jpn"}"#)
        .await
        .unwrap();
    let japanese = seen!();
    assert_eq!(
        japanese,
        [
            "ブルマが…",
            "ブルマが…",
            "ブルマが…",
            "悟空の冒険。",
            "悟空の冒険。"
        ]
        .map(serde_json::Value::from)
    );
}

/// One `Library.query` on `parent`, name-ordered, with `collapseCollections`.
async fn folded_query(
    session: &session::Client,
    parent: &str,
    start: u32,
    limit: u32,
) -> (u32, Vec<serde_json::Value>) {
    use pharos_jellyfin_api::dto::WireId;
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.query_request();
    req.get().init_ctx();
    let mut q = req.get().init_q();
    match WireId::parse(parent).expect("parent is a wire id") {
        WireId::Real(n) => q.reborrow().init_parent_id().set_real(n),
        WireId::Synth(n) => q.reborrow().init_parent_id().set_synth(n),
    }
    q.set_sort_by("SortName");
    q.set_collapse_collections(true);
    q.set_start(start);
    q.set_limit(limit);
    let reply = req.send().promise.await.expect("query");
    summaries(reply.get().unwrap().get_result().unwrap())
}

/// B1086, V1044: with `collapseCollections` a listing shows each box set as
/// one tile, where its first film stood, once the listing holds two or more
/// of its films; a lone member stays a film. The total, the pages and the
/// letter index count tiles, the tile opens on the set's films, and each
/// folded listing counts under `pharos_collection_fold_total{outcome}`.
/// Disarm check: with `fold_into_collections` folding nothing, the tile is
/// missing and "Delta" is listed.
#[actix_web::test]
async fn a_listing_folds_the_films_of_a_box_set_into_one_tile() {
    use pharos_core::{
        CollectionStore as _, LibraryStore as _, MediaItem, MediaKind, MediaStore as _,
    };

    let fx = crate::common::seed_rich().await;
    seed_movies(&fx).await;
    for (id, title) in [(103, "Delta"), (104, "Epsilon")] {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/libA/{title}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    let set = fx
        .state
        .stores
        .create_collection("Set One", &[103, 101])
        .await
        .unwrap();
    // Gamma is in the other library, so this listing holds one film of it.
    fx.state
        .stores
        .create_collection("Lone Set", &[104, 201])
        .await
        .unwrap();
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let lib_a = fx.lib_a_wire.clone();
    let names = |items: &[serde_json::Value]| -> Vec<String> {
        items
            .iter()
            .map(|i| i["Name"].as_str().unwrap().to_owned())
            .collect()
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = crate::telemetry::connect(&fx);
            let session = crate::telemetry::session_for(&client, &fx.user_token).await;
            let (total, items) = folded_query(&session, &lib_a, 0, 0).await;
            assert_eq!(names(&items), ["Set One", "Beta", "Epsilon", "Rich Movie"]);
            assert_eq!(total, 4);
            assert_eq!(items[0]["Id"], set.wire_id.as_str());
            let (total, page) = folded_query(&session, &lib_a, 1, 1).await;
            assert_eq!((total, names(&page)), (4, vec!["Beta".to_owned()]));
            let (_, plain) = wire_query(
                &session,
                Some(&lib_a),
                &[],
                "SortName",
                false,
                "",
                &[],
                0,
                0,
            )
            .await;
            assert_eq!(
                names(&plain),
                ["Alpha", "Beta", "Delta", "Epsilon", "Rich Movie"]
            );
            let (_, opened) = wire_query(
                &session,
                Some(&set.wire_id),
                &[],
                "SortName",
                false,
                "",
                &[],
                0,
                0,
            )
            .await;
            // A set lists its films in the set's own order.
            assert_eq!(names(&opened), ["Delta", "Alpha"]);
        })
        .await;
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let q = library::ItemQuery {
        sort_by: Some("SortName".into()),
        collapse_collections: true,
        ..library::ItemQuery::default()
    };
    let index = library::index(&fx.state, &user, Some(&lib_a), None, &q)
        .await
        .unwrap();
    let letters: Vec<(&str, u32)> = index
        .letters
        .iter()
        .map(|(l, s)| (l.as_str(), *s))
        .collect();
    assert_eq!(letters, [("A", 0), ("B", 1), ("E", 2), ("R", 3)]);
    assert_eq!(
        crate::telemetry::counter(
            &snapshotter,
            "pharos_collection_fold_total",
            &[("outcome", "ok")],
        ),
        2
    );
}

/// A failed series or album tile read lists each fold's newest child rather
/// than failing the rail, and with it the whole home screen.
#[actix_web::test]
async fn a_failed_tile_read_lists_the_newest_child_instead() {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _};
    use pharos_server::services::library::LatestTile;
    let fx = seed_rich().await;
    for (id, ep) in [(401u64, 1u32), (402, 2), (403, 3)] {
        let item = MediaItem {
            id,
            path: format!("/libA/Show/Season 01/e{ep}.mkv").into(),
            title: format!("Show {ep}"),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "Show".into(),
                season_number: Some(1),
                episode_number: Some(ep),
                series_folder: Some("/libA/Show".into()),
                series_year: None,
                series_key: None,
            }),
            created_at: Some(1_900_000_000 + id as i64),
            ..MediaItem::default()
        };
        fx.state.stores.put(item).await.unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    #[cfg(feature = "postgres")]
    let pharos_store_sqlx::any::AnyStore::Sqlite(sqlite) = &fx.state.stores
    else {
        panic!("the fixture is sqlite");
    };
    #[cfg(not(feature = "postgres"))]
    let sqlite = &fx.state.stores;
    sqlx::query("DROP TABLE series_metadata")
        .execute(sqlite.pool())
        .await
        .unwrap();
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let tiles = library::latest_tiles(&fx.state, &user, None, &library::KindFilter::Unset, None)
        .await
        .expect("the rail still lists");
    let ids: Vec<u64> = tiles
        .iter()
        .filter_map(|t| match t {
            LatestTile::Item(item, _) => Some(item.id),
            LatestTile::Group { .. } => None,
        })
        .collect();
    assert_eq!(ids.first(), Some(&403), "the fold's newest child: {ids:?}");
    assert!(!ids.contains(&402) && !ids.contains(&401), "{ids:?}");
}

/// Spec 038 FR-106: a latest rail folds one show's new episodes and one
/// album's new tracks into a tile each, placed where the newest child was,
/// with how many there are; a lone new episode and a film list as items.
#[actix_web::test]
async fn latest_rails_fold_new_episodes_and_tracks_into_one_tile() {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaProbe, MediaStore as _};
    use pharos_server::services::library::LatestTile;
    let fx = seed_rich().await;
    let at = |id: u64| Some(1_800_000_000 + id as i64);
    let episode = |id: u64, show: &str, ep: u32, when: u64| MediaItem {
        id,
        path: format!("/libA/{show}/Season 01/e{ep}.mkv").into(),
        title: format!("{show} {ep}"),
        kind: MediaKind::Episode,
        series: Some(pharos_core::SeriesInfo {
            series_name: show.into(),
            season_number: Some(1),
            episode_number: Some(ep),
            series_folder: Some(format!("/libA/{show}")),
            series_year: None,
            series_key: None,
        }),
        created_at: at(when),
        ..MediaItem::default()
    };
    let track = |id: u64, album: &str| MediaItem {
        id,
        path: format!("/libB/{album}/{id}.flac").into(),
        title: format!("Track {id}"),
        kind: MediaKind::Audio,
        probe: MediaProbe {
            album: Some(album.into()),
            album_artist: Some("A Band".into()),
            ..MediaProbe::default()
        },
        created_at: at(id),
        ..MediaItem::default()
    };
    for item in [
        episode(401, "Show", 1, 401),
        episode(402, "Show", 2, 402),
        episode(403, "Show", 3, 403),
        episode(501, "Another", 1, 1),
        episode(502, "Another", 2, 2),
        track(701, "First Record"),
        track(702, "First Record"),
        track(703, "Single"),
    ] {
        fx.state.stores.put(item).await.unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let metrics_guard = metrics::set_default_local_recorder(&recorder);
    let tiles = library::latest_tiles(&fx.state, &user, None, &library::KindFilter::Unset, None)
        .await
        .unwrap();
    let counted = |tile: &str| -> u64 {
        use metrics_util::debugging::DebugValue;
        snap.snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| {
                ck.key().name() == "pharos_latest_tiles_total"
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "tile" && l.value() == tile)
            })
            .map(|(.., v)| match v {
                DebugValue::Counter(n) => n,
                _ => 0,
            })
            .sum()
    };
    // Disarm check: with the counter removed every count reads 0.
    assert_eq!(counted("series"), 2, "Show and Another");
    assert_eq!(counted("album"), 1);
    assert_eq!(counted("item"), (tiles.len() - 3) as u64);
    drop(metrics_guard);
    let shape: Vec<String> = tiles
        .iter()
        .map(|t| match t {
            LatestTile::Item(item, _) => format!("item {}", item.id),
            LatestTile::Group { dto, new_children } => {
                format!(
                    "{} {} x{new_children}",
                    dto["Type"].as_str().unwrap(),
                    dto["Name"].as_str().unwrap()
                )
            }
        })
        .collect();
    assert_eq!(
        shape.get(..3),
        Some(
            &[
                "item 703".to_string(),
                "MusicAlbum First Record x2".to_string(),
                "Series Show x3".to_string(),
            ][..]
        ),
        "{shape:?}"
    );
    assert!(
        !shape.iter().any(|s| s == "item 401" || s == "item 701"),
        "no child is listed beside its group: {shape:?}"
    );
    let LatestTile::Group { dto, .. } = &tiles[2] else {
        panic!("{shape:?}");
    };
    assert_eq!(dto["ChildCount"], 3);
    assert_eq!(
        dto["DateCreated"],
        pharos_jellyfin_api::dto::format_iso8601(1_800_000_403)
    );

    let one = library::latest_tiles(&fx.state, &user, None, &library::KindFilter::Unset, Some(2))
        .await
        .unwrap();
    assert_eq!(one.len(), 2, "the limit counts tiles, not items");
}

/// FR-106 on the wire: the home rail carries the group as its series with
/// `newChildren`, and `/Items/Latest` folds unless `GroupItems=false`.
#[actix_web::test]
async fn home_rails_and_jellyfin_latest_carry_the_fold() {
    use actix_web::test;
    use pharos_api::pharos_capnp::{home_result, Kind};

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    seed_shows(&fx).await;
    let library = session.library_request().send().pipeline.get_library();
    let mut home = library.home_request();
    home.get().init_ctx();
    let reply = home.send().promise.await.expect("home");
    let home_result::Home(home) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("home answered an error");
    };
    let rails = home.unwrap().get_rails().unwrap();
    let mut groups = Vec::new();
    for rail in rails.iter() {
        for tile in rail.get_latest().unwrap().iter() {
            if tile.get_kind().unwrap() == Kind::Series {
                groups.push((
                    tile.get_name().unwrap().to_str().unwrap().to_owned(),
                    tile.get_new_children(),
                ));
            }
        }
    }
    groups.sort();
    assert_eq!(groups, [("Another".to_owned(), 2), ("Show".to_owned(), 3)]);

    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let latest = |q: &'static str| {
        test::TestRequest::get()
            .uri(&format!("/Items/Latest?ParentId={}{q}", fx.lib_a_wire))
            .insert_header(("X-Emby-Token", fx.user_token.as_str()))
            .to_request()
    };
    let folded: serde_json::Value = test::call_and_read_body_json(&app, latest("")).await;
    let types: Vec<&str> = folded
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["Type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"Series"), "{types:?}");
    assert!(!types.contains(&"Episode"), "{types:?}");
    let flat: serde_json::Value =
        test::call_and_read_body_json(&app, latest("&GroupItems=false")).await;
    let flat_eps = flat
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["Type"] == "Episode")
        .count();
    assert_eq!(flat_eps, 3);
}

/// Adds a Books library at /libBooks with a book and a course ("Intro to
/// Widgets": module "01 - Basics" with two lessons, a "03" module with one),
/// and returns its wire id.
async fn seed_books_library(fx: &crate::common::Fixture) -> String {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _, SeriesInfo};
    let wire = "d".repeat(32);
    fx.state
        .stores
        .upsert_library("Reading", "/libBooks", LibraryKind::Books, &wire)
        .await
        .unwrap();
    fx.state
        .stores
        .put(MediaItem {
            id: 701,
            path: "/libBooks/Widget Handbook.epub".into(),
            title: "Widget Handbook".into(),
            kind: MediaKind::Book,
            ..MediaItem::default()
        })
        .await
        .unwrap();
    let lessons = [
        (
            702,
            "01 - Basics/01 - Introduction.mp4",
            1,
            1,
            "Introduction",
        ),
        (703, "01 - Basics/02 - Parts.mp4", 1, 2, "Parts"),
        (704, "03/001 Fitting.mp4", 3, 1, "Fitting"),
    ];
    for (id, rel, season, ep, title) in lessons {
        fx.state
            .stores
            .put(MediaItem {
                id,
                path: format!("/libBooks/Intro to Widgets/{rel}").into(),
                title: title.into(),
                kind: MediaKind::Episode,
                series: Some(SeriesInfo {
                    series_name: "Intro to Widgets".into(),
                    season_number: Some(season),
                    episode_number: Some(ep),
                    series_folder: Some("/libBooks/Intro to Widgets".into()),
                    series_year: None,
                    series_key: None,
                }),
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    let libraries = fx.state.stores.libraries().await.unwrap();
    fx.state.set_libraries(libraries);
    wire
}

/// V1393 — a Books library lists its books, its courses as series, a
/// course's seasons by their module folders; a TV library lists no course.
/// Disarm check: with `books_default` keeping `Unset`, the first listing
/// holds the lessons too; with `season_name` always "Season N", the module
/// name is missing.
#[actix_web::test]
async fn a_books_library_lists_books_and_its_courses_apart() {
    use pharos_jellyfin_api::dto::series_id_for_key;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let tv = seed_tv_library(&fx).await;
    let books = seed_books_library(&fx).await;

    assert_eq!(
        listed(&session, &books, &[], "SortName").await,
        [("Widget Handbook".to_owned(), None)]
    );
    assert_eq!(
        listed(&session, &books, &[Kind::Series], "SortName").await,
        [("Intro to Widgets".to_owned(), None)]
    );
    let course = series_id_for_key(Some("/libBooks/Intro to Widgets"), "Intro to Widgets");
    assert_eq!(
        listed(&session, &course, &[Kind::Season], "").await,
        [
            ("Basics".to_owned(), Some(1)),
            ("Season 3".to_owned(), Some(3))
        ]
    );
    assert_eq!(
        listed(&session, &tv, &[Kind::Series], "SortName").await,
        [("Mature".to_owned(), None), ("Show".to_owned(), None)],
        "a course is never a TV library's show"
    );
}
