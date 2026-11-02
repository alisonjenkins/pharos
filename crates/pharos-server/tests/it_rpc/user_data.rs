//! Spec 017 T036: a user's played and favourite state over the native API,
//! written through the same service as the Jellyfin routes.

use pharos_api::pharos_capnp::{error, session, user_state_result};
use pharos_core::{UserDataStore as _, UserPolicy, UserStore as _};
use pharos_jellyfin_api::dto::WireId;
use pharos_server::state::SocketBroadcast;

/// A play state as (played, favourite, play count, position in ms).
type State = (bool, bool, u32, u64);

enum Set {
    Played(bool),
    Favorite(bool),
}

async fn wire_set(session: &session::Client, id: WireId, set: Set) -> Result<State, String> {
    let user_data = session.user_data_request().send().pipeline.get_user_data();
    let set_id = |mut out: pharos_api::pharos_capnp::item_id::Builder<'_>| match id {
        WireId::Real(n) => out.set_real(n),
        WireId::Synth(n) => out.set_synth(n),
    };
    let reply = match set {
        Set::Played(played) => {
            let mut req = user_data.set_played_request();
            req.get().init_ctx();
            set_id(req.get().init_id());
            req.get().set_played(played);
            let reply = req.send().promise.await.expect("setPlayed");
            read_state(reply.get().unwrap().get_result().unwrap())
        }
        Set::Favorite(favorite) => {
            let mut req = user_data.set_favorite_request();
            req.get().init_ctx();
            set_id(req.get().init_id());
            req.get().set_favorite(favorite);
            let reply = req.send().promise.await.expect("setFavorite");
            read_state(reply.get().unwrap().get_result().unwrap())
        }
    };
    reply
}

fn read_state(result: user_state_result::Reader<'_>) -> Result<State, String> {
    match result.which().unwrap() {
        user_state_result::State(s) => {
            let s = s.unwrap();
            Ok((
                s.get_played(),
                s.get_favorite(),
                s.get_play_count(),
                s.get_position_ms(),
            ))
        }
        user_state_result::Error(e) => Err(match e.unwrap().get_kind().which().unwrap() {
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

/// `UserData` from a Jellyfin body as a [`State`].
fn jellyfin_state(user_data: &serde_json::Value) -> State {
    (
        user_data["Played"].as_bool().unwrap(),
        user_data["IsFavorite"].as_bool().unwrap(),
        u32::try_from(user_data["PlayCount"].as_u64().unwrap()).unwrap(),
        user_data["PlaybackPositionTicks"].as_u64().unwrap() / 10_000,
    )
}

/// T036 over the wire: each native write answers the state the Jellyfin
/// item route then reads, a Jellyfin write is what the next native write
/// starts from, and every write reaches the user's jellyfin-web sockets.
/// Disarm check: with `set_played` ignoring its `played` argument, or
/// `update` not announcing the change, the agreement fails.
#[actix_web::test]
async fn native_user_data_agrees_with_the_jellyfin_routes() {
    use actix_web::test;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let uid = fx.user_id.0.simple().to_string();
    let id = fx.rich_item_id;
    let mut changes = fx.state.bus.subscribe();
    let jellyfin = |req: test::TestRequest| {
        req.insert_header(("X-Emby-Token", fx.user_token.as_str()))
            .to_request()
    };
    let read = || {
        jellyfin(test::TestRequest::get().uri(&format!("/Users/{uid}/Items/{}", WireId::Real(id))))
    };

    let mut announced = || match changes.try_recv() {
        Ok(SocketBroadcast::UserDataChanged { user_id, entries }) => {
            assert_eq!(user_id, uid);
            assert_eq!(entries.len(), 1);
            jellyfin_state(&entries[0])
        }
        other => panic!("expected a UserDataChanged broadcast, got {other:?}"),
    };

    let played = wire_set(&session, WireId::Real(id), Set::Played(true)).await;
    assert_eq!(played, Ok((true, false, 1, 0)));
    assert_eq!(Ok(announced()), played);
    let web: serde_json::Value = test::call_and_read_body_json(&app, read()).await;
    assert_eq!(Ok(jellyfin_state(&web["UserData"])), played);

    let req = jellyfin(
        test::TestRequest::post().uri(&format!("/UserFavoriteItems/{}", WireId::Real(id))),
    );
    let web: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(jellyfin_state(&web), (true, true, 1, 0));
    announced();

    let unplayed = wire_set(&session, WireId::Real(id), Set::Played(false)).await;
    assert_eq!(unplayed, Ok((false, true, 1, 0)));
    assert_eq!(Ok(announced()), unplayed);
    let unfavourite = wire_set(&session, WireId::Real(id), Set::Favorite(false)).await;
    assert_eq!(unfavourite, Ok((false, false, 1, 0)));
    assert_eq!(Ok(announced()), unfavourite);
    let web: serde_json::Value = test::call_and_read_body_json(&app, read()).await;
    assert_eq!(Ok(jellyfin_state(&web["UserData"])), unfavourite);
}

/// T036: writing to an item the policy hides answers `notFound` exactly like
/// an id that names nothing, and writes nothing; a synthesised id has no
/// native write yet.
/// Disarm check: with the access check removed from
/// `services::user_data::update`, the hidden item is marked played.
#[actix_web::test]
async fn a_hidden_item_cannot_be_marked() {
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
    let not_found = Err("not found: item".to_owned());

    assert!(wire_set(
        &session,
        WireId::Real(fx.other_item_id),
        Set::Favorite(true)
    )
    .await
    .is_ok());
    for set in [Set::Played(true), Set::Favorite(true)] {
        assert_eq!(
            wire_set(&session, WireId::Real(fx.rich_item_id), set).await,
            not_found
        );
    }
    assert_eq!(
        wire_set(&session, WireId::Real(987_654_321), Set::Played(true)).await,
        not_found
    );
    let synth = WireId::parse(&fx.lib_b_wire).unwrap();
    assert_eq!(
        wire_set(&session, synth, Set::Played(true)).await,
        not_found
    );
    let stored = fx
        .state
        .stores
        .get_user_data(fx.user_id, fx.rich_item_id)
        .await
        .unwrap();
    assert!(!stored.played && !stored.is_favorite);
}
