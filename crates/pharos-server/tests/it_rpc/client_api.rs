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
    };
    let detail = api
        .detail(&call, ItemRef::Real(MOVIE))
        .await
        .expect("detail");
    let audio: Vec<_> = detail.audio_tracks.iter().map(|t| t.index).collect();
    assert_eq!(audio, [1]);
    assert!(detail.subtitle_tracks.is_empty() && detail.segments.is_empty());
    let (decision, grant) = api
        .play(
            &call,
            ItemRef::Real(MOVIE),
            "",
            &caps,
            TrackPicks::default(),
        )
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

/// V291: a 2D film with a grouped half-SBS edition is badged 3D, lists both
/// versions, is found by the 3D filter, and plays the edition's file when
/// asked; a version it does not have, or cannot serve yet, is refused.
#[actix_web::test]
async fn a_3d_edition_is_badged_listed_filtered_and_played() {
    use pharos_client::api::{Listing, Stereo};
    use pharos_core::{AlternateMediaSource, MediaItem, MediaKind, MediaProbe, Video3DFormat};

    const FILM: u64 = 310;
    let (api, fx, _server) = api().await;
    let probe = |format| MediaProbe {
        duration_ms: Some(60_000),
        width: Some(1920),
        height: Some(1080),
        container: Some("mp4".into()),
        video_codec: Some("h264".into()),
        audio_codec: Some("aac".into()),
        video_3d_format: format,
        ..Default::default()
    };
    let mut primary = probe(None);
    primary.alternate_sources = vec![
        AlternateMediaSource {
            id: "3d-hsbs".into(),
            path: "/libA/Film (2009) [3D.HSBS].mp4".into(),
            name: Some("3D".into()),
            video_3d_format: Some(Video3DFormat::HalfSideBySide),
            probe: Some(Box::new(probe(Some(Video3DFormat::HalfSideBySide)))),
            ..Default::default()
        },
        AlternateMediaSource {
            id: "3d-htab".into(),
            path: "/libA/Film (2009) [3D.HTAB].mp4".into(),
            video_3d_format: Some(Video3DFormat::HalfTopAndBottom),
            // Probed before schema v9 kept an edition's own probe.
            probe: None,
            ..Default::default()
        },
    ];
    pharos_core::MediaStore::put(
        &fx.state.stores,
        MediaItem {
            id: FILM,
            path: "/libA/Film (2009).mp4".into(),
            title: "Film".into(),
            kind: MediaKind::Movie,
            probe: primary,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    pharos_core::LibraryStore::backfill_library_ids(&fx.state.stores)
        .await
        .unwrap();
    let call = CallInfo::default();

    let detail = api
        .detail(&call, ItemRef::Real(FILM))
        .await
        .expect("detail");
    assert_eq!(detail.item.stereo, Some(Stereo::HalfSideBySide));
    let versions: Vec<_> = detail
        .versions
        .iter()
        .map(|v| (v.id.as_str(), v.stereo, v.primary))
        .collect();
    assert_eq!(
        versions,
        [
            ("", None, true),
            ("3d-hsbs", Some(Stereo::HalfSideBySide), false),
            ("3d-htab", Some(Stereo::HalfTopAndBottom), false),
        ]
    );

    let library = api.home(&call).await.expect("home").rails[0].library.id;
    let mut listing = Listing::of(library, None, "SortName");
    listing.stereo = Some(true);
    let only_3d = api.query(&call, &listing, 0, 50).await.expect("3D only");
    let names: Vec<_> = only_3d.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["Film"]);
    listing.stereo = Some(false);
    let only_2d = api.query(&call, &listing, 0, 50).await.expect("2D only");
    assert!(only_2d.items.iter().all(|i| i.name != "Film"));

    let caps = Capabilities {
        video_codecs: vec!["h264".into()],
        audio_codecs: vec!["aac".into()],
        containers: vec!["mp4".into()],
        max_bitrate: 0,
    };
    let (decision, _grant) = api
        .play(
            &call,
            ItemRef::Real(FILM),
            "3d-hsbs",
            &caps,
            TrackPicks::default(),
        )
        .await
        .expect("negotiate the edition");
    assert!(
        decision.stream_url.contains("MediaSourceId=") && decision.stream_url.contains("-3d-hsbs"),
        "{}",
        decision.stream_url
    );
    let (primary, _grant) = api
        .play(&call, ItemRef::Real(FILM), "", &caps, TrackPicks::default())
        .await
        .expect("negotiate the primary");
    assert!(
        !primary.stream_url.contains("MediaSourceId"),
        "{}",
        primary.stream_url
    );
    let Err(err) = api
        .play(
            &call,
            ItemRef::Real(FILM),
            "nope",
            &caps,
            TrackPicks::default(),
        )
        .await
    else {
        panic!("negotiated a version the film does not have");
    };
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
    let Err(err) = api
        .play(
            &call,
            ItemRef::Real(FILM),
            "3d-htab",
            &caps,
            TrackPicks::default(),
        )
        .await
    else {
        panic!("granted an edition it cannot serve");
    };
    assert!(
        matches!(
            err,
            ClientError::Api {
                kind: "unavailable",
                ..
            }
        ),
        "{err:?}"
    );
}
