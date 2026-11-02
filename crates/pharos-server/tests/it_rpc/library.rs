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
    use pharos_api::pharos_capnp::{item_id, opt_u16, opt_u32};
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
    let result = reply.get().unwrap().get_result().unwrap();
    let items_result::Page(page) = result.which().unwrap() else {
        panic!("latest answered an error");
    };
    let items = page.unwrap().get_items().unwrap();
    let opt16 = |o: opt_u16::Reader<'_>| match o.which().unwrap() {
        opt_u16::Some(n) => Some(u32::from(n)),
        opt_u16::None(()) => None,
    };
    let opt32 = |o: opt_u32::Reader<'_>| match o.which().unwrap() {
        opt_u32::Some(n) => Some(n),
        opt_u32::None(()) => None,
    };
    items
        .iter()
        .map(|item| {
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
    let project = |body: serde_json::Value| -> Vec<serde_json::Value> {
        body.as_array()
            .unwrap()
            .iter()
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
    };

    let native = wire_latest(&session, None).await;
    assert!(!native.is_empty());
    let web: serde_json::Value = test::call_and_read_body_json(&app, jellyfin(None)).await;
    assert_eq!(native, project(web));

    // The id `Library.views` hands out scopes `latest` to that library.
    let lib_a = WireId::parse(&fx.lib_a_wire).unwrap();
    let scoped = wire_latest(&session, Some(lib_a)).await;
    let web: serde_json::Value =
        test::call_and_read_body_json(&app, jellyfin(Some(&fx.lib_a_wire))).await;
    assert_eq!(scoped, project(web));
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
