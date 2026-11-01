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
