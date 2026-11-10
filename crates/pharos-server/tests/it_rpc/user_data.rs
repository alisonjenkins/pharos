//! Spec 017 T036: a user's played and favourite state over the native API,
//! written through the same service as the Jellyfin routes.

use pharos_api::pharos_capnp::{error, session, user_state_result};
use pharos_core::{UserDataStore as _, UserPolicy, UserStore as _};
use pharos_jellyfin_api::dto::WireId;
use pharos_server::state::SocketBroadcast;

/// A play state as (played, favourite, play count, position in ms).
type State = (bool, bool, u32, u64);

#[derive(Clone, Copy, PartialEq)]
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
    match set {
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
    }
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
/// Disarm check: with `set_played` ignoring its `played` argument,
/// `apply_played` keeping the resume position, or `update` not announcing
/// the change, the agreement fails.
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

    // A resume position the writes must clear (B176): playing from the
    // start resets it, and so does unmarking.
    let (stores, user_id) = (&fx.state.stores, fx.user_id);
    let resume_at = |ticks: u64| async move {
        let mut data = stores.get_user_data(user_id, id).await.unwrap();
        data.last_played_position_ticks = ticks;
        stores.set_user_data(user_id, id, data).await.unwrap();
    };
    resume_at(600_000_000).await;
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

    resume_at(700_000_000).await;
    let unplayed = wire_set(&session, WireId::Real(id), Set::Played(false)).await;
    assert_eq!(unplayed, Ok((false, true, 1, 0)));
    assert_eq!(Ok(announced()), unplayed);
    let unfavourite = wire_set(&session, WireId::Real(id), Set::Favorite(false)).await;
    assert_eq!(unfavourite, Ok((false, false, 1, 0)));
    assert_eq!(Ok(announced()), unfavourite);
    let web: serde_json::Value = test::call_and_read_body_json(&app, read()).await;
    assert_eq!(Ok(jellyfin_state(&web["UserData"])), unfavourite);

    // Each play counts.
    let replayed = wire_set(&session, WireId::Real(id), Set::Played(true)).await;
    assert_eq!(replayed, Ok((true, false, 2, 0)));
    announced();
}

/// T036: writing to an item the policy hides answers `notFound` exactly like
/// an id that names nothing, and writes nothing, as does a synthesised id
/// that is no series or season.
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
    assert_eq!(
        wire_set(&session, synth, Set::Favorite(true)).await,
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

/// Marking a season or a series marks every episode in it: played sets
/// each one and counts the play, one already watched included as the Jellyfin
/// route does, and unplayed clears each one. Their detail
/// says watched once every episode in them is.
/// Disarm check: with the native `setPlayed` refusing synthesised ids again,
/// marking the season answers notFound; with `synth_played` answering
/// false, the season's detail says unwatched.
#[actix_web::test]
async fn a_season_or_series_marks_every_episode_in_it() {
    use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key};
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    crate::library::seed_tv_library(&fx).await;
    let played = |id: u64| {
        let fx = &fx;
        async move {
            let d = fx.state.stores.get_user_data(fx.user_id, id).await.unwrap();
            (d.played, d.play_count)
        }
    };

    let season = WireId::parse(&season_id_for_key(Some("/libTV/Show"), "Show", 1)).unwrap();
    assert_eq!(
        wire_set(&session, season, Set::Played(true)).await,
        Ok((true, false, 1, 0))
    );
    assert_eq!(played(601).await, (true, 1));
    assert_eq!(played(602).await, (true, 1));
    assert_eq!(played(603).await, (false, 0), "season 2 is not in season 1");
    assert_eq!(played(604).await, (false, 0), "another show");

    let show = WireId::parse(&series_id_for_key(Some("/libTV/Show"), "Show")).unwrap();
    let watched = |id: WireId| {
        let session = &session;
        async move { detail_played(session, id).await }
    };
    assert!(watched(season).await);
    assert!(!watched(show).await, "season 2 is unwatched");

    assert!(wire_set(&session, show, Set::Played(true)).await.is_ok());
    assert!(watched(show).await);
    assert_eq!(
        played(601).await,
        (true, 2),
        "already watched: counted again, as the Jellyfin route does"
    );
    assert_eq!(played(603).await, (true, 1));
    assert_eq!(
        wire_set(&session, show, Set::Played(false)).await,
        Ok((false, false, 0, 0))
    );
    for id in [601, 602, 603] {
        assert!(!played(id).await.0, "episode {id}");
    }
    assert!(!watched(show).await);
}

/// Whether `Library.detail` says `id` is watched.
async fn detail_played(session: &session::Client, id: WireId) -> bool {
    detail_marks(session, id).await.0
}

/// What `Library.detail` says of `id`: (watched, favourite).
async fn detail_marks(session: &session::Client, id: WireId) -> (bool, bool) {
    use pharos_api::pharos_capnp::detail_result;
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.detail_request();
    req.get().init_ctx();
    match id {
        WireId::Real(n) => req.get().init_id().set_real(n),
        WireId::Synth(n) => req.get().init_id().set_synth(n),
    }
    let reply = req.send().promise.await.expect("detail");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        detail_result::Detail(d) => {
            let summary = d.unwrap().get_summary().unwrap();
            if !summary.has_user_state() {
                return (false, false);
            }
            let state = summary.get_user_state().unwrap();
            (state.get_played(), state.get_favorite())
        }
        detail_result::Error(_) => panic!("detail {id}"),
    }
}

/// B1080: a show or season marked a favourite says so on its detail, while
/// every episode in it is one; un-marking one episode takes the mark off.
/// Disarm check: with `synth_marks` leaving `favorite` false, the show's
/// detail says it is no favourite right after it was marked.
#[actix_web::test]
async fn a_series_marked_favourite_says_so_on_its_detail() {
    use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key};
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    crate::library::seed_tv_library(&fx).await;

    let show = WireId::parse(&series_id_for_key(Some("/libTV/Show"), "Show")).unwrap();
    let season = WireId::parse(&season_id_for_key(Some("/libTV/Show"), "Show", 1)).unwrap();
    assert_eq!(detail_marks(&session, show).await, (false, false));
    assert_eq!(
        wire_set(&session, show, Set::Favorite(true)).await,
        Ok((false, true, 0, 0))
    );
    assert_eq!(detail_marks(&session, show).await, (false, true));
    assert_eq!(detail_marks(&session, season).await, (false, true));

    assert!(wire_set(&session, WireId::Real(603), Set::Favorite(false))
        .await
        .is_ok());
    assert_eq!(
        detail_marks(&session, show).await,
        (false, false),
        "season 2's episode is no longer a favourite"
    );
    assert_eq!(detail_marks(&session, season).await, (false, true));
}

/// B1085: marking an album or an artist a favourite marks every track under
/// it, and its detail says it is one while every track is; un-marking one
/// track takes the mark off the album and artist it is in.
/// Disarm check: with `music_folder_items` finding no tracks, marking the
/// album is refused as not found.
#[actix_web::test]
async fn an_album_or_artist_marked_favourite_marks_its_tracks() {
    use pharos_jellyfin_api::dto::{album_id_for, artist_id_for};
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    crate::library::seed_music_libraries(&fx).await;
    let favourite = |id: u64| {
        let fx = &fx;
        async move {
            fx.state
                .stores
                .get_user_data(fx.user_id, id)
                .await
                .unwrap()
                .is_favorite
        }
    };

    let first = WireId::parse(&album_id_for("First")).unwrap();
    let second = WireId::parse(&album_id_for("Second")).unwrap();
    let alpha = WireId::parse(&artist_id_for("Alpha")).unwrap();
    assert_eq!(detail_marks(&session, first).await, (false, false));
    assert!(wire_set(&session, first, Set::Favorite(true)).await.is_ok());
    assert_eq!(detail_marks(&session, first).await, (false, true));
    for id in [701, 702, 703] {
        assert!(favourite(id).await, "track {id} of the album");
    }
    assert!(!favourite(704).await, "another album's track");
    assert_eq!(
        detail_marks(&session, alpha).await,
        (false, false),
        "the artist's other album is no favourite"
    );

    assert!(wire_set(&session, alpha, Set::Favorite(true)).await.is_ok());
    assert_eq!(detail_marks(&session, alpha).await, (false, true));
    assert_eq!(detail_marks(&session, second).await, (false, true));
    assert!(!favourite(705).await, "another artist's track");

    assert!(wire_set(&session, WireId::Real(702), Set::Favorite(false))
        .await
        .is_ok());
    assert_eq!(detail_marks(&session, first).await, (false, false));
    assert_eq!(detail_marks(&session, alpha).await, (false, false));
    assert_eq!(detail_marks(&session, second).await, (false, true));
}

/// Pipelined writes to one item both land: a native client can send
/// setPlayed and setFavorite back to back, and the second must not write
/// over the first with the state it read before the first was stored.
/// Disarm check: without the write lock in `services::user_data::update`,
/// one of the two is lost.
#[actix_web::test]
async fn pipelined_writes_to_one_item_both_land() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let id = WireId::Real(fx.rich_item_id);
    for _ in 0..20 {
        let (played, favorite) = futures_util::join!(
            wire_set(&session, id, Set::Played(true)),
            wire_set(&session, id, Set::Favorite(true)),
        );
        assert!(played.is_ok() && favorite.is_ok());
        let stored = fx
            .state
            .stores
            .get_user_data(fx.user_id, fx.rich_item_id)
            .await
            .unwrap();
        assert!(stored.played && stored.is_favorite, "{stored:?}");
        let (unplayed, unfavourite) = futures_util::join!(
            wire_set(&session, id, Set::Played(false)),
            wire_set(&session, id, Set::Favorite(false)),
        );
        assert!(unplayed.is_ok() && unfavourite.is_ok());
        let stored = fx
            .state
            .stores
            .get_user_data(fx.user_id, fx.rich_item_id)
            .await
            .unwrap();
        assert!(!stored.played && !stored.is_favorite, "{stored:?}");
    }
}

/// T063: a series or season marked from the native API leaves the user's
/// stored state on every episode exactly as the Jellyfin route does, for
/// played and favourite, with one episode already watched beforehand. The
/// folder answer matches the route's body, and a hidden show stays unmarked.
/// Pins the route's outcome for the pre-watched episode: marking played counts
/// its play again. Disarm check: with the native favourite refusing
/// synthesised ids, or either side skipping a watched episode, the states differ.
#[actix_web::test]
async fn native_and_jellyfin_agree_on_marking_a_series_or_season() {
    use actix_web::test;
    use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key};
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    crate::library::seed_tv_library(&fx).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let episodes = [601_u64, 602, 603, 604];
    let snapshot = || async {
        let mut out = Vec::new();
        for id in episodes {
            let d = fx.state.stores.get_user_data(fx.user_id, id).await.unwrap();
            out.push((id, d.played, d.is_favorite, d.play_count));
        }
        out
    };
    let fx_ref = &fx;
    let reset = |starred: bool| async move {
        for id in episodes {
            let watched = id == 601;
            let data = pharos_core::UserItemData {
                played: watched,
                is_favorite: starred,
                play_count: u32::from(watched),
                ..Default::default()
            };
            fx_ref
                .state
                .stores
                .set_user_data(fx_ref.user_id, id, data)
                .await
                .unwrap();
        }
    };

    let season = season_id_for_key(Some("/libTV/Show"), "Show", 1);
    let show = series_id_for_key(Some("/libTV/Show"), "Show");
    for id in [&season, &show] {
        for (native, route, delete) in [
            (Set::Played(true), "UserPlayedItems", false),
            (Set::Played(false), "UserPlayedItems", true),
            (Set::Favorite(true), "UserFavoriteItems", false),
            (Set::Favorite(false), "UserFavoriteItems", true),
        ] {
            let starred = native == Set::Favorite(false);
            reset(starred).await;
            let before = snapshot().await;
            let answered = wire_set(&session, WireId::parse(id).unwrap(), native)
                .await
                .unwrap();
            let by_native = snapshot().await;

            reset(starred).await;
            let req = if delete {
                test::TestRequest::delete()
            } else {
                test::TestRequest::post()
            };
            let req = req
                .uri(&format!("/{route}/{id}"))
                .insert_header(("X-Emby-Token", fx.user_token.as_str()))
                .to_request();
            let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
            let by_jellyfin = snapshot().await;

            assert_eq!(by_native, by_jellyfin, "{id} {route} delete={delete}");
            if native == Set::Played(true) {
                assert_eq!(
                    by_jellyfin[0],
                    (601, true, false, 2),
                    "watched counted again"
                );
            }
            assert_ne!(
                by_native, before,
                "{id} {route} delete={delete} wrote nothing"
            );
            assert_eq!(
                (answered.0, answered.1, answered.2),
                (
                    body["Played"].as_bool().unwrap(),
                    body["IsFavorite"].as_bool().unwrap(),
                    u32::try_from(body["PlayCount"].as_u64().unwrap()).unwrap()
                ),
                "folder answer, {id} {route} delete={delete}"
            );
            assert_eq!(by_native[3], before[3], "another show is untouched");
        }
    }
}
