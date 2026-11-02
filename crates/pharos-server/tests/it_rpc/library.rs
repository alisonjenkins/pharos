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

/// Seeds three episodes of "Show" in /libA and one of "Another" in /libB,
/// with "Show" S01E01 already played.
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
        }),
        ..MediaItem::default()
    };
    for item in [
        episode(401, "libA", "Show", 1),
        episode(403, "libA", "Show", 3),
        episode(402, "libA", "Show", 2),
        episode(501, "libB", "Another", 1),
    ] {
        fx.state.stores.put(item).await.unwrap();
    }
    fx.state.stores.backfill_library_ids().await.unwrap();
    fx.state
        .stores
        .set_user_data(
            fx.user_id,
            401,
            pharos_core::UserItemData {
                played: true,
                ..pharos_core::UserItemData::default()
            },
        )
        .await
        .unwrap();
}

/// T032: the first unplayed episode of each series, ordered by series name,
/// scoped to one series on request, only where the policy allows.
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
    assert_eq!(ids(all), [501, 402]);

    let one = library::next_up(&fx.state, &user, None, Some(0))
        .await
        .unwrap();
    assert_eq!(ids(one), [501], "a limit below 1 still lists one");

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
        wire_detail(&session, WireId::Real(103)).await.unwrap()["Chapters"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

/// T034: an item the policy hides answers `notFound`, exactly like an id
/// that names nothing, so a detail call reveals nothing about what exists;
/// a synthesised id has no detail yet.
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
    let synth = WireId::parse(&fx.lib_b_wire).unwrap();
    assert_eq!(
        wire_detail(&session, synth).await,
        Err("not found: item".to_owned())
    );
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

/// T038 parity: `Library.search` lists the same item ids, in the same order,
/// as `/Search/Hints` lists as item hints (its aggregate hints come after).
/// Disarm check: a `search` that reverses the store order breaks the
/// comparison.
#[actix_web::test]
async fn native_search_agrees_with_the_jellyfin_search_hints_route() {
    use actix_web::test;

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
            .collect();
        assert!(!ids.is_empty(), "{term}: the term must match something");
        assert_eq!(ids, hinted, "{term}");
    }
}
