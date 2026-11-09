//! 009 `PharosApi` over the native API: the calls pharos-ui makes, through
//! `pharos_client::api::NativeApi`, against a real server.

use pharos_client::api::{
    Capabilities, ItemKind, ItemRef, NativeApi, PharosApi, PlaybackGrant, TrackPicks,
};
use pharos_client::{CallInfo, ClientError};
use pharos_core::UserDataStore as _;

use crate::playback::{seed_playable, MOVIE};

async fn api() -> (NativeApi, crate::common::Fixture, actix_test::TestServer) {
    let (server, fx) = crate::revocation::start().await;
    crate::library::seed_movies(&fx).await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    (NativeApi::new(&session), fx, server)
}

/// Home, a library, a title: what the UI's first three screens show.
#[actix_web::test]
async fn home_browse_and_detail_read_the_library() {
    let (api, _fx, _server) = api().await;
    let call = CallInfo::default();

    let home = api.home(&call).await.expect("home");
    let names: Vec<_> = home.rails.iter().map(|r| r.library.name.as_str()).collect();
    assert_eq!(names, ["Movies A", "Movies B"]);
    let rail = &home.rails[0];
    assert_eq!(rail.library.kind, ItemKind::Library);
    assert!(!rail.latest.is_empty());

    let page = api
        .browse(&call, rail.library.id, 0, 50)
        .await
        .expect("browse");
    let titles: Vec<_> = page.items.iter().map(|i| i.name.as_str()).collect();
    assert!(
        titles.windows(2).all(|w| w[0] <= w[1]),
        "sorted by name: {titles:?}"
    );
    assert!(
        titles.contains(&"Alpha") && titles.contains(&"Beta"),
        "{titles:?}"
    );
    assert!(!titles.contains(&"Gamma"), "Gamma is in the other library");
    assert_eq!(page.total as usize, page.items.len());
    let beta = page.items.iter().find(|i| i.name == "Beta").unwrap();
    assert!(beta.favorite);

    let detail = api.detail(&call, beta.id).await.expect("detail");
    assert_eq!(detail.item.id, beta.id);
    assert_eq!(detail.item.name, "Beta");
}

/// The diagnostics opt-in as the UI reads and writes it: unset, set, and an
/// unknown key refused as an API error.
#[actix_web::test]
async fn prefs_read_and_write_the_diagnostics_opt_in() {
    let (api, _fx, _server) = api().await;
    let call = CallInfo::default();
    assert_eq!(api.pref(&call, "diagnostics.opt_in").await.unwrap(), None);
    api.set_pref(&call, "diagnostics.opt_in", b"true")
        .await
        .expect("stored");
    assert_eq!(
        api.pref(&call, "diagnostics.opt_in").await.unwrap(),
        Some(b"true".to_vec())
    );
    let err = api
        .set_pref(&call, "nope", b"x")
        .await
        .expect_err("unknown key");
    assert!(
        matches!(
            err,
            ClientError::Api {
                kind: "notPermitted",
                ..
            }
        ),
        "{err:?}"
    );
}

/// The watched toggle as the UI drives it: a title, then a whole season,
/// and the detail read afterwards agrees.
#[actix_web::test]
async fn set_played_marks_a_title_and_a_season() {
    let (api, fx, _server) = api().await;
    let call = CallInfo::default();
    let alpha = ItemRef::Real(MOVIE);
    assert!(api.set_played(&call, alpha, true).await.expect("watched"));
    assert!(api.detail(&call, alpha).await.unwrap().item.played);
    assert!(!api
        .set_played(&call, alpha, false)
        .await
        .expect("unwatched"));
    assert!(!api.detail(&call, alpha).await.unwrap().item.played);

    crate::library::seed_tv_library(&fx).await;
    let season = pharos_jellyfin_api::dto::season_id_for_key(Some("/libTV/Show"), "Show", 1);
    let season = match pharos_jellyfin_api::dto::WireId::parse(&season).unwrap() {
        pharos_jellyfin_api::dto::WireId::Synth(n) => ItemRef::Synth(n),
        pharos_jellyfin_api::dto::WireId::Real(n) => ItemRef::Real(n),
    };
    assert!(api.set_played(&call, season, true).await.expect("season"));
    assert!(api.detail(&call, season).await.unwrap().item.played);
    let stored = fx
        .state
        .stores
        .get_user_data(fx.user_id, 601)
        .await
        .unwrap();
    assert!(stored.played);
}

#[actix_web::test]
async fn a_missing_item_is_an_api_error_not_a_transport_failure() {
    let (api, _fx, _server) = api().await;
    let err = api
        .detail(&CallInfo::default(), ItemRef::Real(999_999))
        .await
        .expect_err("no such item");
    assert!(
        matches!(
            err,
            ClientError::Api {
                kind: "notFound",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.needs_sign_in());
}

/// Watch: negotiate, then report and stop through the grant.
#[actix_web::test]
async fn play_negotiates_and_reports_progress() {
    let (api, fx, _server) = api().await;
    let call = CallInfo::default();
    let caps = Capabilities {
        video_codecs: vec!["h264".into()],
        audio_codecs: vec!["aac".into()],
        containers: vec!["mp4".into()],
        max_bitrate: 0,
        hdr: true,
        image_subtitles: Vec::new(),
    };
    let detail = api
        .detail(&call, ItemRef::Real(MOVIE))
        .await
        .expect("detail");
    let audio: Vec<_> = detail.audio_tracks.iter().map(|t| t.index).collect();
    assert_eq!(audio, [1]);
    assert!(detail.subtitle_tracks.is_empty() && detail.segments.is_empty());
    let (decision, grant) = api
        .play(&call, ItemRef::Real(MOVIE), &caps, TrackPicks::default())
        .await
        .expect("negotiate");
    assert_eq!(decision.method, "direct_play");
    assert!(!decision.is_hls());
    assert!(
        decision.stream_url.starts_with('/'),
        "{}",
        decision.stream_url
    );
    assert!(!decision.play_session.is_empty());
    assert_eq!((decision.audio, decision.subtitle), (Some(1), None));

    grant
        .report(&call, 10_000, false, TrackPicks::default())
        .await
        .expect("report");
    grant.stop(&call, 20_000).await.expect("stop");
    let data = fx
        .state
        .stores
        .get_user_data(fx.user_id, MOVIE)
        .await
        .unwrap();
    assert_eq!(data.last_played_position_ticks, 200_000_000);
}

/// Cast as the title page reads it: an episode lists its actors then its
/// guest stars (crew left out), with a photo only for a portrait the server
/// passes through; a series ranks its regulars across its episodes.
#[actix_web::test]
async fn detail_carries_the_cast_and_a_series_ranks_its_regulars() {
    use pharos_client::api::CreditKind;
    use pharos_core::{PersonKind, PersonRef, PersonStore as _};

    let (api, fx, _server) = api().await;
    crate::library::seed_tv_library(&fx).await;
    let call = CallInfo::default();
    let person =
        |name: &str, kind, character: Option<&str>, order, thumb: Option<&str>| PersonRef {
            name: name.into(),
            kind,
            character: character.map(str::to_string),
            sort_order: Some(order),
            thumb: thumb.map(str::to_string),
            ..PersonRef::default()
        };
    let lead = person(
        "Lead Actor",
        PersonKind::Actor,
        Some("Captain"),
        0,
        Some("https://image.tmdb.org/t/p/w185/lead.jpg"),
    );
    let second = person(
        "Second Actor",
        PersonKind::Actor,
        Some("Mate"),
        1,
        Some("https://img.example/s.jpg"),
    );
    let guest = person(
        "Guest Star",
        PersonKind::GuestStar,
        Some("Stranger"),
        2,
        None,
    );
    let director = person("Some Director", PersonKind::Director, None, 3, None);
    let links = [
        (601, vec![lead.clone(), guest, director]),
        (602, vec![lead.clone(), second.clone()]),
        (603, vec![second, lead]),
    ];
    for (id, people) in links {
        fx.state.stores.link_item_people(id, &people).await.unwrap();
    }

    let episode = api
        .detail(&call, ItemRef::Real(601))
        .await
        .expect("episode");
    let cast: Vec<_> = episode
        .cast
        .iter()
        .map(|c| (c.name.as_str(), c.character.as_str(), c.kind))
        .collect();
    assert_eq!(
        cast,
        [
            ("Lead Actor", "Captain", CreditKind::Actor),
            ("Guest Star", "Stranger", CreditKind::GuestStar),
        ]
    );
    let photo = episode.cast[0].image_url.as_deref().unwrap_or_default();
    let lead_wire = pharos_core::person_wire_id("Lead Actor");
    assert!(
        photo.starts_with(&format!("/Items/{lead_wire}/Images/Primary?tag=")),
        "{photo}"
    );
    assert_eq!(episode.cast[1].image_url, None);

    let home = api.home(&call).await.expect("home");
    let tv = home
        .rails
        .iter()
        .find(|r| r.library.name == "TV")
        .expect("tv rail")
        .library
        .id;
    let shows = api
        .list(&call, tv, Some(ItemKind::Series), "SortName", 0, 50)
        .await
        .expect("shows");
    let show = shows
        .items
        .iter()
        .find(|i| i.name == "Show")
        .expect("show")
        .id;
    let series = api.detail(&call, show).await.expect("series");
    let regulars: Vec<_> = series
        .cast
        .iter()
        .map(|c| (c.name.as_str(), c.character.as_str()))
        .collect();
    assert_eq!(
        regulars,
        [("Lead Actor", "Captain"), ("Second Actor", "Mate")]
    );
    // img.example is not a provider host: the CSP would block its redirect.
    assert_eq!(series.cast[1].image_url, None);
}

/// A person's page as the UI reads it from a cast credit: the credit names
/// the person, and their page lists the film and the show's episodes they act
/// in, with who they play, leaving out what they only directed. An id that
/// names nobody is refused as not found.
/// Disarm check: with `write_cast` not setting `personId`, `lead.person` is
/// `None` and the page cannot be asked for.
#[actix_web::test]
async fn a_cast_credit_opens_the_page_of_what_that_person_appears_in() {
    use pharos_client::api::CreditKind;
    use pharos_core::{PersonKind, PersonRef, PersonStore as _};

    let (api, fx, _server) = api().await;
    crate::library::seed_tv_library(&fx).await;
    let call = CallInfo::default();
    let acting = |kind, character: &str| PersonRef {
        name: "Lead Actor".into(),
        kind,
        character: Some(character.into()),
        ..PersonRef::default()
    };
    let directing = PersonRef {
        name: "Lead Actor".into(),
        kind: PersonKind::Director,
        role: Some("Director".into()),
        ..PersonRef::default()
    };
    let links = [
        (101, vec![acting(PersonKind::Actor, "Pilot")]),
        (102, vec![directing]),
        (602, vec![acting(PersonKind::Actor, "Captain")]),
        (601, vec![acting(PersonKind::GuestStar, "Captain")]),
    ];
    for (id, people) in links {
        fx.state.stores.link_item_people(id, &people).await.unwrap();
    }

    let episode = api
        .detail(&call, ItemRef::Real(602))
        .await
        .expect("episode");
    let lead = episode.cast.first().expect("lead credit");
    let id = lead.person.expect("the credit names the person");

    let page = api.person(&call, id).await.expect("person");
    assert_eq!(page.name, "Lead Actor");
    assert_eq!(page.id, Some(id));
    let films: Vec<_> = page
        .films
        .iter()
        .map(|f| (f.item.name.as_str(), f.character.as_str()))
        .collect();
    assert_eq!(films, [("Alpha", "Pilot")], "Beta is only directed");
    let [show] = page.shows.as_slice() else {
        panic!("one show: {:?}", page.shows);
    };
    assert_eq!(show.series.name, "Show");
    assert_eq!(show.series.kind, ItemKind::Series);
    assert_eq!(show.characters, ["Captain"]);
    let episodes: Vec<_> = show.episodes.iter().map(|e| (e.item.id, e.kind)).collect();
    assert_eq!(
        episodes,
        [
            (ItemRef::Real(601), CreditKind::GuestStar),
            (ItemRef::Real(602), CreditKind::Actor),
        ]
    );

    let nobody = pharos_core::person_wire_id("Nobody At All");
    let nobody = pharos_jellyfin_api::dto::WireId::parse(&nobody).expect("wire id");
    let pharos_jellyfin_api::dto::WireId::Synth(n) = nobody else {
        panic!("a person's id is synthesised");
    };
    for nobody in [ItemRef::Synth(n), ItemRef::Real(101)] {
        let err = api.person(&call, nobody).await.unwrap_err();
        assert!(
            matches!(&err, ClientError::Api { kind, .. } if *kind == "notFound"),
            "{nobody}: {err:?}"
        );
    }
}
