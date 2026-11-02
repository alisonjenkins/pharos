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
    let capped = library::latest(&fx.state, &user, None, &KindFilter::Unset, Some(500))
        .await
        .unwrap();
    assert_eq!(capped.len(), LATEST_MAX as usize);
}
