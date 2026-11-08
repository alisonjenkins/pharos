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
/// names nobody is refused as not found, and search offers the person by
/// name with the same id. What is kept about them (their
/// biography, dates and ids) comes with the page.
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

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let kept = pharos_core::PersonDetails {
        biography: Some("Born by the sea.".into()),
        birth_date: Some("1956-07-09".into()),
        imdb_id: Some("nm0000158".into()),
        wikidata_id: Some("Q2263".into()),
        fetched_at: i64::try_from(now).unwrap(),
        ..pharos_core::PersonDetails::default()
    };
    fx.state
        .stores
        .put_person_details(&pharos_core::person_wire_id("Lead Actor"), &kept)
        .await
        .unwrap();

    let page = api.person(&call, id).await.expect("person");
    assert_eq!(page.name, "Lead Actor");
    assert_eq!(page.biography.as_deref(), Some("Born by the sea."));
    assert_eq!(page.birth_date.as_deref(), Some("1956-07-09"));
    assert_eq!(page.death_date, None, "an unknown date is not an empty one");
    assert_eq!(page.imdb_id.as_deref(), Some("nm0000158"));
    assert_eq!(page.wikidata_id.as_deref(), Some("Q2263"));
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

    let found = api.search(&call, "lead actor", 0).await.expect("search");
    let person = found
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Person)
        .expect("search offers the actor");
    assert_eq!((person.id, person.name.as_str()), (id, "Lead Actor"));

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

/// V720: a 2D film with a grouped half-SBS edition is badged 3D, lists both
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
            path: "/libA/Film (2009) - 3D.HSBS.mp4".into(),
            name: Some("3D HSBS".into()),
            video_3d_format: Some(Video3DFormat::HalfSideBySide),
            probe: Some(Box::new(probe(Some(Video3DFormat::HalfSideBySide)))),
            ..Default::default()
        },
        AlternateMediaSource {
            id: "3d-htab".into(),
            path: "/libA/Film (2009) - 3D.HTAB.mp4".into(),
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
    assert!(!only_2d.items.is_empty());
    assert!(only_2d.items.iter().all(|i| i.name != "Film"));

    let caps = Capabilities {
        video_codecs: vec!["h264".into()],
        audio_codecs: vec!["aac".into()],
        containers: vec!["mp4".into()],
        ..Capabilities::default()
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
    for (version, kind) in [("nope", "notFound"), ("3d-htab", "unavailable")] {
        let played = api
            .play(
                &call,
                ItemRef::Real(FILM),
                version,
                &caps,
                TrackPicks::default(),
            )
            .await;
        assert!(
            matches!(&played, Err(ClientError::Api { kind: k, .. }) if *k == kind),
            "{version}: {:?}",
            played.map(|(d, _)| d)
        );
        let detail = api
            .detail_version(&call, ItemRef::Real(FILM), version)
            .await;
        assert!(
            matches!(&detail, Err(ClientError::Api { kind: k, .. }) if *k == kind),
            "{version}: {detail:?}"
        );
    }
}

/// V721: each version carries what tells it apart (resolution, HDR, codecs,
/// audio, size), and its path on disk only when an admin asks; a detail of
/// one copy lists that file's subtitles, read through its own source.
#[actix_web::test]
async fn versions_describe_their_files_and_show_paths_to_admins_only() {
    use pharos_client::api::VersionAudio;
    use pharos_core::{
        AlternateMediaSource, AudioTrack, MediaItem, MediaKind, MediaProbe, SubtitleTrack,
    };

    const EPISODE: u64 = 311;
    let (server, fx) = crate::revocation::start().await;
    crate::library::seed_movies(&fx).await;
    let copy = MediaProbe {
        width: Some(3840),
        height: Some(2160),
        video_codec: Some("hevc".into()),
        color_transfer: Some("smpte2084".into()),
        container: Some("mkv".into()),
        size_bytes: Some(9_000),
        audio_tracks: vec![
            AudioTrack {
                codec: Some("eac3".into()),
                channels: Some(6),
                language: Some("eng".into()),
                ..Default::default()
            },
            AudioTrack {
                codec: Some("aac".into()),
                channels: Some(2),
                language: Some("jpn".into()),
                ..Default::default()
            },
        ],
        subtitle_tracks: vec![SubtitleTrack {
            stream_index: 3,
            language: Some("eng".into()),
            codec: Some("subrip".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let primary = MediaProbe {
        width: Some(1920),
        height: Some(1080),
        video_codec: Some("h264".into()),
        audio_codec: Some("aac".into()),
        audio_channels: Some(2),
        container: Some("mkv".into()),
        size_bytes: Some(4_000),
        alternate_sources: vec![
            AlternateMediaSource {
                id: "season-1".into(),
                path: "/libA/Show/Season 1/Show - S01E02.mkv".into(),
                name: Some("Season 1".into()),
                probe: Some(Box::new(copy)),
                ..Default::default()
            },
            AlternateMediaSource {
                id: "avi".into(),
                path: "/libA/Show/Season 01/Show - S01E02.avi".into(),
                name: Some("AVI".into()),
                video_codec: Some("mpeg4".into()),
                size_bytes: Some(700),
                probe: None,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    pharos_core::MediaStore::put(
        &fx.state.stores,
        MediaItem {
            id: EPISODE,
            path: "/libA/Show/Season 01/Show - S01E02.mkv".into(),
            title: "Episode".into(),
            kind: MediaKind::Episode,
            probe: primary,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    pharos_core::LibraryStore::backfill_library_ids(&fx.state.stores)
        .await
        .unwrap();
    let detail_as = |token: String, version: &'static str| {
        let server = &server;
        async move {
            let (client, _closed) = crate::revocation::connect(server).await;
            let session = crate::revocation::signed_in(&client, &token).await;
            NativeApi::new(&session)
                .detail_version(&CallInfo::default(), ItemRef::Real(EPISODE), version)
                .await
                .expect("detail")
        }
    };

    let admin = detail_as(fx.admin_token.clone(), "").await;
    let [primary, copy, avi] = admin.versions.as_slice() else {
        panic!("three versions: {:?}", admin.versions);
    };
    assert_eq!(
        (
            primary.width,
            primary.height,
            primary.video_codec.as_str(),
            primary.hdr.as_str()
        ),
        (1920, 1080, "h264", "")
    );
    assert_eq!(
        primary.audio,
        [VersionAudio {
            codec: "aac".into(),
            channels: 2,
            ..Default::default()
        }]
    );
    assert_eq!(primary.path, "/libA/Show/Season 01/Show - S01E02.mkv");
    assert_eq!(
        (
            copy.name.as_str(),
            copy.width,
            copy.hdr.as_str(),
            copy.size_bytes
        ),
        ("Season 1", 3840, "HDR10", 9_000)
    );
    let langs: Vec<_> = copy
        .audio
        .iter()
        .map(|a| (a.language.as_str(), a.channels))
        .collect();
    assert_eq!(langs, [("eng", 6), ("jpn", 2)]);
    assert_eq!(copy.path, "/libA/Show/Season 1/Show - S01E02.mkv");
    assert_eq!((avi.video_codec.as_str(), avi.size_bytes), ("mpeg4", 700));
    assert!(
        admin.subtitle_tracks.is_empty(),
        "{:?}",
        admin.subtitle_tracks
    );

    let viewer = detail_as(fx.user_token.clone(), "").await;
    assert_eq!(viewer.versions.len(), 3);
    assert!(
        viewer.versions.iter().all(|v| v.path.is_empty()),
        "paths reach admins only: {:?}",
        viewer.versions
    );

    let season_1 = detail_as(fx.user_token.clone(), "season-1").await;
    assert_eq!(
        season_1.versions, viewer.versions,
        "every version, whichever is asked for"
    );
    let urls: Vec<_> = season_1
        .subtitle_tracks
        .iter()
        .map(|t| t.vtt_url.clone().unwrap_or_default())
        .collect();
    let wire = pharos_jellyfin_api::dto::wire_item_id(EPISODE);
    assert_eq!(
        urls,
        [format!(
            "/videos/{wire}/{wire}-season-1/subtitles/3/stream.vtt"
        )]
    );
}
