//! Spec 017 T037: a native player negotiating an item and reporting where it
//! is, through the same services as `/Items/{id}/PlaybackInfo` and
//! `/Sessions/Playing`.

use actix_web::test;
use pharos_api::pharos_capnp::{
    capabilities, decision_result, done, error, negotiate_result, playback_grant, session,
    start_tracks_result, stream_choice,
};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore as _, TokenStore as _, UserDataStore as _,
    UserPolicy, UserStore as _,
};
use pharos_jellyfin_api::dto::{wire_item_id, WireId};
use pharos_server::services::playback::{self, Capabilities, NegotiateRequest, PlaybackSwitches};

use crate::common::Fixture;

pub(crate) const MOVIE: u64 = 300;
const BOOK: u64 = 301;
const NATIVE_UA: &str = "pharos-native-test/1.0";
/// The release grace the tests that wait out a released grant run with.
const TEST_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

pub(crate) async fn seed_playable(fx: &Fixture) {
    let stores = &fx.state.stores;
    stores
        .put(MediaItem {
            id: MOVIE,
            path: "/libA/native.mp4".into(),
            title: "Native".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                container: Some("mp4".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: BOOK,
            path: "/libA/native.epub".into(),
            title: "Native Book".into(),
            kind: MediaKind::Book,
            ..Default::default()
        })
        .await
        .unwrap();
}

fn caps(video: &[&str], audio: &[&str], containers: &[&str]) -> Capabilities {
    let own = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect();
    Capabilities {
        video_codecs: own(video),
        audio_codecs: own(audio),
        containers: own(containers),
        max_bitrate: 0,
        hdr: true,
        image_subtitles: Vec::new(),
    }
}

#[derive(Clone, Copy)]
enum Choice {
    Default,
    Off,
    Index(u32),
}

/// A decision as the wire carries it.
#[derive(Debug, PartialEq)]
struct Decided {
    method: String,
    block: String,
    downgrade: String,
    url: String,
    psid: String,
    audio: Option<u32>,
    subtitle: Option<u32>,
}

fn denial(e: error::Reader<'_>) -> String {
    let text = |t: capnp::Result<capnp::text::Reader<'_>>| t.unwrap().to_str().unwrap().to_owned();
    match e.get_kind().which().unwrap() {
        error::kind::NotFound(what) => format!("not found: {}", text(what)),
        error::kind::NotPermitted(what) => format!("not permitted: {}", text(what)),
        error::kind::Unavailable(what) => format!("unavailable: {}", text(what)),
        error::kind::Revoked(()) => "revoked".to_owned(),
        _ => "other".to_owned(),
    }
}

fn read_decision(result: decision_result::Reader<'_>) -> Result<Decided, String> {
    let text = |t: capnp::Result<capnp::text::Reader<'_>>| t.unwrap().to_str().unwrap().to_owned();
    match result.which().unwrap() {
        decision_result::Decision(d) => {
            let d = d.unwrap();
            Ok(Decided {
                method: text(d.get_method()),
                block: text(d.get_direct_play_block()),
                downgrade: text(d.get_downgrade()),
                url: text(d.get_stream_url()),
                psid: text(d.get_play_session().unwrap().get_value()),
                audio: opt(d.get_audio().unwrap()),
                subtitle: opt(d.get_subtitle().unwrap()),
            })
        }
        decision_result::Error(e) => Err(denial(e.unwrap())),
    }
}

fn opt(o: pharos_api::pharos_capnp::opt_u32::Reader<'_>) -> Option<u32> {
    match o.which().unwrap() {
        pharos_api::pharos_capnp::opt_u32::Some(v) => Some(v),
        pharos_api::pharos_capnp::opt_u32::None(()) => None,
    }
}

async fn negotiate(
    session: &session::Client,
    id: WireId,
    caps: &Capabilities,
    audio: Choice,
    subtitle: Choice,
) -> Result<(playback_grant::Client, Decided), String> {
    negotiate_at(
        pharos_api::pharos_capnp::SCHEMA_VERSION,
        session,
        id,
        caps,
        audio,
        subtitle,
    )
    .await
}

/// `negotiate` from a client built against `schema_version`.
async fn negotiate_at(
    schema_version: u32,
    session: &session::Client,
    id: WireId,
    caps: &Capabilities,
    audio: Choice,
    subtitle: Choice,
) -> Result<(playback_grant::Client, Decided), String> {
    negotiate_version(schema_version, session, id, caps, audio, subtitle, "").await
}

/// `negotiate_at` for one of the item's versions; `""` plays the primary.
async fn negotiate_version(
    schema_version: u32,
    session: &session::Client,
    id: WireId,
    caps: &Capabilities,
    audio: Choice,
    subtitle: Choice,
    version: &str,
) -> Result<(playback_grant::Client, Decided), String> {
    let playback = session.playback_request().send().pipeline.get_playback();
    let mut req = playback.negotiate_request();
    let mut p = req.get();
    p.reborrow().init_ctx().set_schema_version(schema_version);
    {
        let mut out = p.reborrow().init_id();
        match id {
            WireId::Real(n) => out.set_real(n),
            WireId::Synth(n) => out.set_synth(n),
        }
    }
    set_caps(p.reborrow().init_caps(), caps);
    set_choice(p.reborrow().init_audio(), audio);
    set_choice(p.reborrow().init_subtitle(), subtitle);
    p.set_version(version);
    let reply = req.send().promise.await.expect("negotiate");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        negotiate_result::Grant(grant) => {
            let grant = grant.unwrap();
            let decided = grant.decision_request();
            let decided = decided.send().promise.await.expect("decision");
            let decided = read_decision(decided.get().unwrap().get_result().unwrap())?;
            Ok((grant, decided))
        }
        negotiate_result::Error(e) => Err(denial(e.unwrap())),
    }
}

fn set_caps(mut c: capabilities::Builder<'_>, caps: &Capabilities) {
    let set = |list: &[String], mut out: capnp::text_list::Builder<'_>| {
        for (i, s) in list.iter().enumerate() {
            out.set(i as u32, s.as_str());
        }
    };
    set(
        &caps.video_codecs,
        c.reborrow()
            .init_video_codecs(caps.video_codecs.len() as u32),
    );
    set(
        &caps.audio_codecs,
        c.reborrow()
            .init_audio_codecs(caps.audio_codecs.len() as u32),
    );
    set(
        &caps.containers,
        c.reborrow().init_containers(caps.containers.len() as u32),
    );
    c.set_max_bitrate(caps.max_bitrate);
    c.set_hdr(caps.hdr);
    set(
        &caps.image_subtitles,
        c.reborrow()
            .init_image_subtitles(caps.image_subtitles.len() as u32),
    );
}

/// `Playback.startTracks`: the audio and subtitle a default negotiation
/// would start `id` on, as `(audio, subtitle)`.
async fn start_tracks(
    session: &session::Client,
    id: u64,
    caps: &Capabilities,
) -> Result<(Option<u32>, Option<u32>), String> {
    let playback = session.playback_request().send().pipeline.get_playback();
    let mut req = playback.start_tracks_request();
    let mut p = req.get();
    p.reborrow()
        .init_ctx()
        .set_schema_version(pharos_api::pharos_capnp::SCHEMA_VERSION);
    p.reborrow().init_id().set_real(id);
    set_caps(p.reborrow().init_caps(), caps);
    let reply = req.send().promise.await.expect("startTracks");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        start_tracks_result::Tracks(t) => {
            let t = t.unwrap();
            Ok((opt(t.get_audio().unwrap()), opt(t.get_subtitle().unwrap())))
        }
        start_tracks_result::Error(e) => Err(denial(e.unwrap())),
    }
}

fn set_choice(mut out: stream_choice::Builder<'_>, choice: Choice) {
    match choice {
        Choice::Default => out.set_default(()),
        Choice::Off => out.set_none(()),
        Choice::Index(i) => out.set_index(i),
    }
}

fn read_done(result: done::Reader<'_>) -> Result<(), String> {
    match result.which().unwrap() {
        done::Ok(()) => Ok(()),
        done::Error(e) => Err(denial(e.unwrap())),
    }
}

async fn report(
    grant: &playback_grant::Client,
    position_ms: u64,
    paused: bool,
) -> Result<(), String> {
    let mut req = grant.report_request();
    req.get().init_ctx();
    req.get().set_position_ms(position_ms);
    req.get().set_paused(paused);
    let reply = req.send().promise.await.expect("report");
    read_done(reply.get().unwrap().get_result().unwrap())
}

async fn stop(grant: &playback_grant::Client, position_ms: u64) -> Result<(), String> {
    let mut req = grant.stop_request();
    req.get().init_ctx();
    req.get().set_position_ms(position_ms);
    let reply = req.send().promise.await.expect("stop");
    read_done(reply.get().unwrap().get_result().unwrap())
}

/// The same capabilities as the Jellyfin `DeviceProfile` body a native app
/// would post.
fn device_profile_json(caps: &Capabilities) -> serde_json::Value {
    let profile = caps.profile(false);
    let direct: Vec<_> = profile
        .direct_play_profiles
        .iter()
        .map(|p| {
            serde_json::json!({
                "Container": p.container, "VideoCodec": p.video_codec,
                "AudioCodec": p.audio_codec, "Type": p.kind,
            })
        })
        .collect();
    let transcoding: Vec<_> = profile
        .transcoding_profiles
        .iter()
        .map(|p| {
            serde_json::json!({
                "Container": p.container, "VideoCodec": p.video_codec,
                "AudioCodec": p.audio_codec, "Protocol": p.protocol, "Type": p.kind,
            })
        })
        .collect();
    serde_json::json!({ "DeviceProfile": {
        "DirectPlayProfiles": direct,
        "TranscodingProfiles": transcoding,
    }})
}

/// A transcoding URL with its session id and token taken out, so two
/// negotiations of the same thing compare equal.
fn normalised(url: &str, psid: &str) -> String {
    url.replace(psid, "PSID")
        .split('&')
        .filter(|kv| !kv.starts_with("api_key="))
        .collect::<Vec<_>>()
        .join("&")
}

/// T037/US4 over the wire: for each kind of decision, the native grant
/// carries the label `pharos_playback_decision_total` counts for the same
/// profile, and the same URL `/Items/{id}/PlaybackInfo` hands a native app
/// (direct play the tag-authorized file, otherwise the transcoding URL
/// without the token).
/// Disarm check: with `write_decision` sending `direct_play_block` as the
/// method, or `grant` leaving the user's audio pick off the transcoding URL,
/// the agreement fails.
#[actix_web::test]
async fn native_negotiation_agrees_with_playback_info() {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let item = pharos_core::MediaStore::get(&fx.state.stores, MOVIE)
        .await
        .unwrap();
    let wire = wire_item_id(MOVIE);

    let cases = [
        caps(&["h264"], &["aac"], &["mp4"]),
        caps(&["h264"], &["aac"], &["mkv"]),
        caps(&["h264"], &["opus"], &["mp4"]),
        caps(&["vp9"], &["opus"], &["webm"]),
    ];
    let mut methods = Vec::new();
    for caps in &cases {
        let (_grant, native) = negotiate(
            &session,
            WireId::Real(MOVIE),
            caps,
            Choice::Default,
            Choice::Default,
        )
        .await
        .unwrap();
        let profile = caps.profile(false);
        let service = playback::negotiate(
            &fx.state,
            &item,
            &NegotiateRequest {
                profile: &profile,
                switches: PlaybackSwitches::default(),
                explicit_subtitle_index: None,
                stereo_mode: None,
                client_is_remote: false,
                is_web_client: false,
            },
        );
        assert_eq!(
            (
                native.method.as_str(),
                native.block.as_str(),
                native.downgrade.as_str()
            ),
            (
                service.decision_kind(),
                service.direct_play_block.label(),
                service.downgrade.unwrap_or("none"),
            ),
            "{caps:?}"
        );

        let web: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::post()
                .uri(&format!("/Items/{wire}/PlaybackInfo"))
                .insert_header(("X-Emby-Token", fx.user_token.as_str()))
                .insert_header(("User-Agent", NATIVE_UA))
                .set_json(device_profile_json(caps))
                .to_request(),
        )
        .await;
        let source = &web["MediaSources"][0];
        if native.method == "direct_play" {
            assert_eq!(source["SupportsDirectPlay"], true, "{caps:?}: {source}");
            assert!(source["TranscodingUrl"].is_null(), "{caps:?}: {source}");
            assert_eq!(
                native.url,
                format!("/videos/{wire}/stream?static=true&tag={}", native.psid)
            );
        } else {
            let jellyfin = source["TranscodingUrl"].as_str().unwrap();
            let jellyfin_psid = web["PlaySessionId"].as_str().unwrap();
            assert!(!native.url.contains("api_key"), "{}", native.url);
            assert_eq!(
                normalised(&native.url, &native.psid),
                normalised(jellyfin, jellyfin_psid),
                "{caps:?}"
            );
        }
        methods.push(native.method);
    }
    assert_eq!(
        methods,
        ["direct_play", "video_remux", "audio_remux", "transcode"],
        "the cases cover every decision"
    );
}

/// T037: the grant's stream capability opens the file without a token,
/// progress reports and the stop move the resume point as `/Sessions/Playing`
/// does, and the stop ends the capability (B256).
/// Disarm check: with `TICKS_PER_MS` at 1, or `Grant::stop` calling
/// `play_state::report`, the position or the closed capability fails.
#[actix_web::test]
async fn reports_and_stop_move_the_resume_point_and_end_the_stream() {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;

    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let (grant, decided) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    assert_eq!(decided.method, "direct_play");
    let stream = || test::TestRequest::get().uri(&decided.url).to_request();
    // The fixture's file is not on disk: anything but 401 got past the check.
    let opened = test::call_service(&app, stream()).await.status();
    assert_ne!(opened, actix_web::http::StatusCode::UNAUTHORIZED);

    report(&grant, 30_000, true).await.unwrap();
    stop(&grant, 45_000).await.unwrap();
    let data = fx
        .state
        .stores
        .get_user_data(fx.user_id, MOVIE)
        .await
        .unwrap();
    assert_eq!(data.last_played_position_ticks, 450_000_000);
    assert!(!data.played);

    let closed = test::call_service(&app, stream()).await.status();
    assert_eq!(closed, actix_web::http::StatusCode::UNAUTHORIZED);
}

/// T066: a grant lists the player in `/Sessions` as `/Sessions/Playing`
/// does (user, the token's device, client, item), a report moves its
/// position and pause, and the stop removes it, as Jellyfin's stop does.
/// Disarm check: with `start_session` not called, the first listing is empty;
/// with `Grant::stop` skipping `play_state::stop`, the last one still lists it.
#[actix_web::test]
async fn a_native_grant_is_listed_in_sessions_until_it_stops() {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let listed = || async {
        let req = test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", fx.admin_token.as_str()))
            .to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        body.as_array().unwrap().clone()
    };
    assert!(listed().await.is_empty());

    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let (grant, decided) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    let entries = listed().await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry["Id"], decided.psid.as_str());
    assert_eq!(entry["UserName"], "guest");
    assert_eq!(entry["UserId"], fx.user_id.0.simple().to_string());
    // The device the token was issued to, never a client-stated one.
    assert_eq!(entry["DeviceId"], "test");
    assert_eq!(entry["Client"], "Pharos");
    assert_eq!(entry["NowPlayingItemId"], wire_item_id(MOVIE));
    assert_eq!(entry["PositionTicks"], 0);
    // Nothing can send a native player commands, so no remote is offered.
    assert_eq!(entry["SupportsRemoteControl"], false);

    report(&grant, 30_000, true).await.unwrap();
    let entries = listed().await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["PositionTicks"], 300_000_000u64);
    assert_eq!(entries[0]["IsPaused"], true);

    stop(&grant, 45_000).await.unwrap();
    assert!(listed().await.is_empty());
}

/// T066: a grant released without a stop, as a closed tab releases it, ends
/// its `/Sessions` entry too. Disarm check: with `Grant`'s `Drop` returning
/// early, the entry stays.
#[actix_web::test]
async fn a_released_native_grant_leaves_sessions() {
    let (server, fx) = crate::revocation::start_with_release_grace(TEST_GRACE).await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let listed = || async {
        let req = test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", fx.admin_token.as_str()))
            .to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        body.as_array().unwrap().len()
    };
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let (grant, _) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    report(&grant, 30_000, false).await.unwrap();
    assert_eq!(listed().await, 1);

    drop(grant);
    // The end runs after the grant goes, so wait for it.
    let mut left = listed().await;
    for _ in 0..100 {
        if left == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        left = listed().await;
    }
    assert_eq!(left, 0, "the released grant's session is still listed");
}

/// B465: a grant released without a stop, as a reload or a dropped
/// connection releases it, ends its playback: the stream capability closes
/// and the last reported position is saved. A grant never reported on closes
/// its stream but leaves the stored position alone. One whose token was
/// revoked before it was released still ends its stream capability.
/// Disarm check: with `Grant`'s `Drop` returning early, the stream stays open;
/// with it skipping `play_state::end` for a revoked session, the last one does.
#[actix_web::test]
async fn a_released_grant_ends_its_playback_at_the_last_report() {
    let (server, fx) = crate::revocation::start_with_release_grace(TEST_GRACE).await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let play = || {
        negotiate(
            &session,
            WireId::Real(MOVIE),
            &direct,
            Choice::Default,
            Choice::Default,
        )
    };
    let position = || async {
        fx.state
            .stores
            .get_user_data(fx.user_id, MOVIE)
            .await
            .unwrap()
            .last_played_position_ticks
    };
    // The stop on release runs after the grant goes, so wait for its write.
    let settles_at = |ticks: u64| {
        let position = &position;
        async move {
            for _ in 0..100 {
                if position().await == ticks {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            false
        }
    };
    let closed = |url: String| {
        let app = &app;
        async move {
            for _ in 0..100 {
                let status =
                    test::call_service(app, test::TestRequest::get().uri(&url).to_request())
                        .await
                        .status();
                if status == actix_web::http::StatusCode::UNAUTHORIZED {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            false
        }
    };

    let (grant, decided) = play().await.unwrap();
    report(&grant, 30_000, false).await.unwrap();
    drop(grant);
    assert!(
        closed(decided.url).await,
        "the released grant's stream closes"
    );
    assert!(settles_at(300_000_000).await, "the last report is saved");

    let (grant, decided) = play().await.unwrap();
    drop(grant);
    assert!(
        closed(decided.url).await,
        "an unreported grant's stream closes"
    );
    assert_eq!(
        position().await,
        300_000_000,
        "no report, no position written"
    );

    let (grant, decided) = play().await.unwrap();
    report(&grant, 60_000, false).await.unwrap();
    fx.state.stores.revoke(&fx.user_token).await.unwrap();
    drop(grant);
    // A revoked token refuses the stream URL on its own, so look at the
    // capability itself.
    let mut ended = false;
    for _ in 0..100 {
        if fx
            .state
            .transcode_sessions
            .get(pharos_core::PlaySessionId::from(decided.psid.clone()))
            .await
            .unwrap()
            .is_none()
        {
            ended = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        ended,
        "a revoked session's released grant still ends its stream"
    );
}

/// T037: what a native player may not negotiate answers the way a detail
/// call does, so an id says nothing about what exists; a book is refused.
/// Disarm check: with the access check removed from `services::library::
/// detail`, the hidden item is granted.
#[actix_web::test]
async fn hidden_missing_and_unplayable_items_are_refused() {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let any = caps(&["h264"], &["aac"], &["mp4"]);
    let refused = |id| {
        let (session, any) = (&session, &any);
        async move {
            negotiate(session, id, any, Choice::Default, Choice::Off)
                .await
                .map(|(_, d)| d)
                .unwrap_err()
        }
    };

    assert_eq!(
        refused(WireId::Real(BOOK)).await,
        "not permitted: streaming a book"
    );
    assert_eq!(refused(WireId::Real(987_654_321)).await, "not found: item");
    let synth = WireId::parse(&fx.lib_b_wire).unwrap();
    assert_eq!(refused(synth).await, "not found: item");
    let no_audio = negotiate(
        &session,
        WireId::Real(MOVIE),
        &any,
        Choice::Off,
        Choice::Off,
    )
    .await
    .map(|(_, d)| d)
    .unwrap_err();
    assert_eq!(no_audio, "not permitted: playing with no audio track");

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
    assert_eq!(refused(WireId::Real(MOVIE)).await, "not found: item");
    assert!(negotiate(
        &session,
        WireId::Real(fx.other_item_id),
        &any,
        Choice::Index(1),
        Choice::Default
    )
    .await
    .is_ok());
}

/// 017 T067: the decision names the tracks the playback starts on, and a
/// report carries a switch the player made without renegotiating, so the
/// next playback starts on it.
/// Disarm check: with `Grant::at` sending no tracks, the remembered
/// subtitle stays unset; with `write_decision` leaving the picks out, the
/// decision names none.
#[actix_web::test]
async fn the_decision_names_its_tracks_and_reports_remember_a_switch() {
    use pharos_core::{AudioTrack, SubtitleTrack};

    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let mut item = pharos_core::MediaStore::get(&fx.state.stores, MOVIE)
        .await
        .unwrap();
    item.probe.audio_tracks = vec![
        AudioTrack {
            stream_index: 1,
            codec: Some("aac".into()),
            is_default: true,
            ..AudioTrack::default()
        },
        AudioTrack {
            stream_index: 2,
            codec: Some("aac".into()),
            ..AudioTrack::default()
        },
    ];
    item.probe.subtitle_tracks = vec![SubtitleTrack {
        stream_index: 3,
        codec: Some("subrip".into()),
        ..SubtitleTrack::default()
    }];
    fx.state.stores.put(item).await.unwrap();
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;

    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let (grant, decided) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Index(2),
        Choice::Off,
    )
    .await
    .unwrap();
    assert_eq!((decided.audio, decided.subtitle), (Some(2), None));
    assert_ne!(
        decided.method, "direct_play",
        "a player starts the file on track 1, so track 2 must be streamed: {decided:?}"
    );
    assert!(decided.url.contains("AudioStreamIndex=2"), "{decided:?}");
    let (_grant, first) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Index(1),
        Choice::Off,
    )
    .await
    .unwrap();
    assert_eq!(first.method, "direct_play", "{first:?}");

    let mut req = grant.report_request();
    req.get().init_ctx();
    req.get().set_position_ms(10_000);
    set_choice(req.get().init_audio(), Choice::Default);
    set_choice(req.get().init_subtitle(), Choice::Index(3));
    let reply = req.send().promise.await.expect("report");
    read_done(reply.get().unwrap().get_result().unwrap()).unwrap();
    let data = fx
        .state
        .stores
        .get_user_data(fx.user_id, MOVIE)
        .await
        .unwrap();
    assert_eq!(data.subtitle_stream_index, Some(3));

    let (_grant, again) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    assert_eq!(again.subtitle, Some(3), "{again:?}");
}

/// A remembered track index names a stream of the primary file, so a
/// version (another file, another stream layout) starts on its own default,
/// and its reports leave the item's remembered tracks alone.
/// Disarm check: with `pick_audio` keeping the remembered index for a
/// version, the version starts on track 2; with `Grant::at` passing a
/// version's tracks, the report overwrites the remembered 2.
#[actix_web::test]
async fn a_version_neither_uses_nor_overwrites_remembered_tracks() {
    use pharos_core::{AlternateMediaSource, AudioTrack};

    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let mut item = pharos_core::MediaStore::get(&fx.state.stores, MOVIE)
        .await
        .unwrap();
    let tracks = vec![
        AudioTrack {
            stream_index: 1,
            codec: Some("aac".into()),
            is_default: true,
            ..AudioTrack::default()
        },
        AudioTrack {
            stream_index: 2,
            codec: Some("aac".into()),
            ..AudioTrack::default()
        },
    ];
    item.probe.audio_tracks = tracks.clone();
    let copy = item.probe.clone();
    item.probe.alternate_sources = vec![AlternateMediaSource {
        id: "copy".into(),
        path: "/libA/copy/native.mp4".into(),
        name: Some("Copy".into()),
        probe: Some(Box::new(copy)),
        ..Default::default()
    }];
    fx.state.stores.put(item).await.unwrap();
    let mut data = fx
        .state
        .stores
        .get_user_data(fx.user_id, MOVIE)
        .await
        .unwrap();
    data.audio_stream_index = Some(2);
    fx.state
        .stores
        .set_user_data(fx.user_id, MOVIE, data)
        .await
        .unwrap();
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let schema = pharos_api::pharos_capnp::SCHEMA_VERSION;

    let (_grant, primary) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    assert_eq!(primary.audio, Some(2), "{primary:?}");

    let (grant, version) = negotiate_version(
        schema,
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
        "copy",
    )
    .await
    .unwrap();
    assert_eq!(version.audio, Some(1), "{version:?}");

    let mut req = grant.report_request();
    req.get().init_ctx();
    req.get().set_position_ms(5_000);
    set_choice(req.get().init_audio(), Choice::Index(1));
    set_choice(req.get().init_subtitle(), Choice::Off);
    let reply = req.send().promise.await.expect("report");
    read_done(reply.get().unwrap().get_result().unwrap()).unwrap();
    let data = fx
        .state
        .stores
        .get_user_data(fx.user_id, MOVIE)
        .await
        .unwrap();
    assert_eq!(
        (data.audio_stream_index, data.subtitle_stream_index),
        (Some(2), None)
    );
    assert_eq!(data.last_played_position_ticks, 5_000 * 10_000);
}

/// MOVIE with a `dvd_subtitle` at stream 3 and an ASS at stream 4.
async fn seed_image_subtitle(fx: &Fixture) {
    use pharos_core::SubtitleTrack;
    seed_playable(fx).await;
    let mut item = pharos_core::MediaStore::get(&fx.state.stores, MOVIE)
        .await
        .unwrap();
    item.probe.subtitle_tracks = vec![
        SubtitleTrack {
            stream_index: 3,
            codec: Some("dvd_subtitle".into()),
            ..SubtitleTrack::default()
        },
        SubtitleTrack {
            stream_index: 4,
            codec: Some("ass".into()),
            ..SubtitleTrack::default()
        },
    ];
    fx.state.stores.put(item).await.unwrap();
}

/// Spec 019 FR-003: a player that declares an image subtitle codec, on a
/// server that extracts it, direct plays with that subtitle on: it draws the
/// PNGs itself. Declaring it switches no text subtitle to a burn.
/// Disarm check: with `Capabilities::profile` leaving `image_subtitles` out,
/// the first decision is `subtitle_burn`; leaving out the `ass` entry, the
/// ASS one is.
#[cfg(feature = "ffmpeg-lib")]
#[actix_web::test]
async fn a_player_that_draws_image_subtitles_keeps_direct_play() {
    let cache = tempfile::tempdir().unwrap();
    let (server, fx) = crate::revocation::start_drawing_images(cache.path()).await;
    seed_image_subtitle(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let mut draws = caps(&["h264"], &["aac"], &["mp4"]);
    draws.image_subtitles = vec!["dvd_subtitle".into()];

    for subtitle in [3, 4] {
        let (_grant, decided) = negotiate(
            &session,
            WireId::Real(MOVIE),
            &draws,
            Choice::Default,
            Choice::Index(subtitle),
        )
        .await
        .unwrap();
        assert_eq!(
            (decided.method.as_str(), decided.downgrade.as_str()),
            ("direct_play", "none"),
            "subtitle {subtitle}: {decided:?}"
        );
        assert_eq!(decided.subtitle, Some(subtitle));
    }
}

/// Spec 019 FR-004: an image subtitle still burns for a player that does
/// not declare it, and on a server that cannot extract it.
/// Disarm check: with `grant` building the profile with `draws_images: true`
/// regardless of the server, the second negotiation direct plays.
#[actix_web::test]
async fn an_image_subtitle_burns_unless_both_sides_can_draw_it() {
    let mut draws = caps(&["h264"], &["aac"], &["mp4"]);
    draws.image_subtitles = vec!["dvd_subtitle".into()];
    let silent = caps(&["h264"], &["aac"], &["mp4"]);

    let cases: Vec<(&str, bool, &Capabilities)> = vec![
        ("player declares nothing", true, &silent),
        ("server cannot extract", false, &draws),
    ];
    for (case, server_extracts, player) in cases {
        #[cfg(feature = "ffmpeg-lib")]
        let cache = tempfile::tempdir().unwrap();
        let (server, fx) = if server_extracts {
            #[cfg(feature = "ffmpeg-lib")]
            {
                crate::revocation::start_drawing_images(cache.path()).await
            }
            #[cfg(not(feature = "ffmpeg-lib"))]
            {
                continue;
            }
        } else {
            crate::revocation::start().await
        };
        seed_image_subtitle(&fx).await;
        let (client, _closed) = crate::revocation::connect(&server).await;
        let session = crate::revocation::signed_in(&client, &fx.user_token).await;
        let (_grant, decided) = negotiate(
            &session,
            WireId::Real(MOVIE),
            player,
            Choice::Default,
            Choice::Index(3),
        )
        .await
        .unwrap();
        assert_eq!(decided.downgrade, "subtitle_burn", "{case}: {decided:?}");
        assert_eq!(decided.subtitle, Some(3), "{case}");
    }
}

/// B559 / V469: a client older than image delivery (schemaVersion 15) is
/// sent `burn` for an image track by `Library.detail`, so negotiation burns
/// it too even when that client declares an image codec; on schema 16 the
/// same declaration direct plays.
/// Disarm check: with `read_capabilities` keeping `image_subtitles` for every
/// schema, the schema 15 negotiation direct plays.
#[cfg(feature = "ffmpeg-lib")]
#[actix_web::test]
async fn an_image_subtitle_burns_for_a_client_older_than_image_delivery() {
    let cache = tempfile::tempdir().unwrap();
    let (server, fx) = crate::revocation::start_drawing_images(cache.path()).await;
    seed_image_subtitle(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let mut draws = caps(&["h264"], &["aac"], &["mp4"]);
    draws.image_subtitles = vec!["dvd_subtitle".into()];

    for (schema_version, downgrade) in [(15, "subtitle_burn"), (16, "none")] {
        let (_grant, decided) = negotiate_at(
            schema_version,
            &session,
            WireId::Real(MOVIE),
            &draws,
            Choice::Default,
            Choice::Index(3),
        )
        .await
        .unwrap();
        assert_eq!(
            decided.downgrade, downgrade,
            "schema {schema_version}: {decided:?}"
        );
        assert_eq!(decided.subtitle, Some(3), "schema {schema_version}");
    }
}

/// 028 FR-017 (B604): naming the styled track negotiates its source track
/// (text, drawn by the client), and the grant echoes the styled pick. With no
/// current plan the pick is dropped and the server's default choice applies,
/// so a viewer who remembered the styled track does not start with none.
#[actix_web::test]
async fn the_styled_track_negotiates_as_its_source_track() {
    use pharos_core::SubtitleStylingStore as _;

    let (server, fx) = crate::revocation::start().await;
    let (dir, item) = crate::styled_track::seed(&fx, 402).await;
    let sidecar = dir.path().join("Show.eng.ass");
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let id = WireId::Real(402);
    let styled_slot = 4;

    let (_g, none) = negotiate(
        &session,
        id,
        &direct,
        Choice::Default,
        Choice::Index(styled_slot),
    )
    .await
    .unwrap();
    let (_g, default) = negotiate(&session, id, &direct, Choice::Default, Choice::Default)
        .await
        .unwrap();
    assert_eq!(
        none.subtitle, default.subtitle,
        "no plan: the pick is dropped and the default applies"
    );

    fx.state
        .stores
        .put_subtitle_styling(crate::styled_track::styling_row(&item, 3, Some(&sidecar)))
        .await
        .unwrap();
    let (styled_grant, styled) = negotiate(
        &session,
        id,
        &direct,
        Choice::Default,
        Choice::Index(styled_slot),
    )
    .await
    .unwrap();
    let (_g, source) = negotiate(&session, id, &direct, Choice::Default, Choice::Index(3))
        .await
        .unwrap();
    assert_eq!(styled.subtitle, Some(styled_slot), "the pick is echoed");
    assert_eq!(source.subtitle, Some(3));
    assert_eq!(
        (&styled.method, &styled.block, &styled.downgrade),
        (&source.method, &source.block, &source.downgrade),
        "the styled pick decides like its source track"
    );
    assert_eq!(
        normalised(&styled.url, &styled.psid),
        normalised(&source.url, &source.psid)
    );

    // The player reports the styled pick, so the next playback remembers it
    // and must start on the same track instead of with no subtitle.
    let mut req = styled_grant.report_request();
    req.get().init_ctx();
    req.get().set_position_ms(10_000);
    set_choice(req.get().init_audio(), Choice::Default);
    set_choice(req.get().init_subtitle(), Choice::Index(styled_slot));
    let reply = req.send().promise.await.expect("report");
    read_done(reply.get().unwrap().get_result().unwrap()).unwrap();
    let (_g, resumed) = negotiate(&session, id, &direct, Choice::Default, Choice::Default)
        .await
        .unwrap();
    assert_eq!(resumed.subtitle, Some(styled_slot), "{resumed:?}");
    assert_eq!(resumed.method, source.method);
}

/// B853 (V803): with nothing chosen, a playback whose default subtitle is the
/// styled track's source starts on the styled track, and `startTracks` says
/// so. A stated pick, the source itself or off, still wins.
#[actix_web::test]
async fn the_styled_track_is_the_default_where_its_source_would_show() {
    use pharos_core::{MediaStore as _, SubtitleStylingStore as _};

    let (server, fx) = crate::revocation::start().await;
    let (dir, mut item) = crate::styled_track::seed(&fx, 403).await;
    // No sidecar, and an embedded English ASS flagged default (an anime
    // release's usual shape), so the default rules pick it and the styled
    // slot is the first past the embedded streams.
    std::fs::remove_file(dir.path().join("Show.eng.ass")).unwrap();
    item.probe.subtitle_tracks[0].is_default = true;
    item.probe.subtitle_tracks[0].language = Some("eng".into());
    item.probe.audio_tracks = vec![pharos_core::AudioTrack {
        stream_index: 1,
        codec: Some("aac".into()),
        is_default: true,
        ..pharos_core::AudioTrack::default()
    }];
    fx.state.stores.put(item.clone()).await.unwrap();
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let id = WireId::Real(403);
    let styled_slot = 3;

    let (_g, plain) = negotiate(&session, id, &direct, Choice::Default, Choice::Default)
        .await
        .unwrap();
    assert_eq!(
        plain.subtitle,
        Some(2),
        "the flagged track is the default without a plan"
    );

    fx.state
        .stores
        .put_subtitle_styling(crate::styled_track::styling_row(&item, 2, None))
        .await
        .unwrap();
    let (_g, styled) = negotiate(&session, id, &direct, Choice::Default, Choice::Default)
        .await
        .unwrap();
    assert_eq!(styled.subtitle, Some(styled_slot), "{styled:?}");
    assert_eq!(styled.method, plain.method, "it decides like its source");
    assert_eq!(
        start_tracks(&session, 403, &direct).await.unwrap().1,
        Some(styled_slot)
    );
    let (_g, source) = negotiate(&session, id, &direct, Choice::Default, Choice::Index(2))
        .await
        .unwrap();
    assert_eq!(source.subtitle, Some(2), "the source picked by name stays");
    let (_g, off) = negotiate(&session, id, &direct, Choice::Default, Choice::Off)
        .await
        .unwrap();
    assert_eq!(off.subtitle, None, "off stays off");

    // A plan made from another track leaves the default alone.
    fx.state
        .stores
        .put_subtitle_styling(crate::styled_track::styling_row(&item, 7, None))
        .await
        .unwrap();
    let (_g, other) = negotiate(&session, id, &direct, Choice::Default, Choice::Default)
        .await
        .unwrap();
    assert_eq!(other.subtitle, Some(2), "{other:?}");
}

const HDR_MOVIE: u64 = 302;

async fn seed_hdr10(fx: &Fixture) {
    fx.state
        .stores
        .put(MediaItem {
            id: HDR_MOVIE,
            path: "/libA/hdr.mp4".into(),
            title: "HDR".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(3840),
                height: Some(2160),
                bitrate_bps: Some(4_000_000),
                container: Some("mp4".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                color_transfer: Some("smpte2084".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
}

/// T065, with `enforce_video_range` on: a player that reports no HDR display is not handed an HDR10 source to
/// direct play; one that does, and an SDR source for either, are unchanged.
/// The refusal names the range as `direct_play_block="video_range"`, the label
/// `pharos_playback_decision_total` counts.
/// Disarm check: with `Capabilities::profile` adding no `VideoRangeType`
/// condition, the hdr=false case direct-plays and the assertion fails.
#[actix_web::test]
async fn an_sdr_only_player_is_not_offered_an_hdr_source_to_direct_play() {
    let (server, fx) = crate::revocation::start_enforcing_video_range().await;
    seed_playable(&fx).await;
    seed_hdr10(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;

    let sdr_display = Capabilities {
        hdr: false,
        ..caps(&["h264"], &["aac"], &["mp4"])
    };
    let hdr_display = caps(&["h264"], &["aac"], &["mp4"]);
    let decide = |id: u64, caps: Capabilities| {
        let session = &session;
        async move {
            negotiate(
                session,
                WireId::Real(id),
                &caps,
                Choice::Default,
                Choice::Default,
            )
            .await
            .unwrap()
            .1
        }
    };

    let blocked = decide(HDR_MOVIE, sdr_display.clone()).await;
    assert_eq!(blocked.method, "transcode");
    assert_eq!(blocked.block, "video_range");
    assert_eq!(
        decide(HDR_MOVIE, hdr_display.clone()).await.method,
        "direct_play"
    );
    assert_eq!(
        decide(MOVIE, sdr_display.clone()).await.method,
        "direct_play"
    );
    assert_eq!(decide(MOVIE, hdr_display).await.method, "direct_play");

    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let item = pharos_core::MediaStore::get(&fx.state.stores, HDR_MOVIE)
        .await
        .unwrap();
    let profile = sdr_display.profile(false);
    let negotiated = playback::negotiate(
        &fx.state,
        &item,
        &NegotiateRequest {
            profile: &profile,
            switches: PlaybackSwitches::default(),
            explicit_subtitle_index: None,
            stereo_mode: None,
            client_is_remote: false,
            is_web_client: false,
        },
    );
    negotiated.record();
    assert_eq!(
        negotiated.direct_play_block,
        pharos_jellyfin_api::device_profile::DirectPlayBlock::VideoRange {
            source: "HDR10".into()
        }
    );
    let counted = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .any(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_playback_decision_total"
                && key
                    .labels()
                    .any(|l| l.key() == "direct_play_block" && l.value() == "video_range")
        });
    assert!(counted, "the decision counter carries video_range");
}

/// T065: with enforcement off (the default) an `hdr=false` player still
/// direct-plays an HDR10 source, and the would-block is counted by range.
/// Disarm check: without the `video_range_unenforced` bookkeeping in
/// `playback::negotiate`, the counter reads nothing.
#[actix_web::test]
async fn video_range_unenforced_plays_on_and_counts_the_would_block() {
    let (_server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    seed_hdr10(&fx).await;
    let hdr_item = pharos_core::MediaStore::get(&fx.state.stores, HDR_MOVIE)
        .await
        .unwrap();
    let sdr_item = pharos_core::MediaStore::get(&fx.state.stores, MOVIE)
        .await
        .unwrap();
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let profile = Capabilities {
        hdr: false,
        ..caps(&["h264"], &["aac"], &["mp4"])
    }
    .profile(false);
    let run = |item: &MediaItem| {
        let negotiated = playback::negotiate(
            &fx.state,
            item,
            &NegotiateRequest {
                profile: &profile,
                switches: PlaybackSwitches::default(),
                explicit_subtitle_index: None,
                stereo_mode: None,
                client_is_remote: false,
                is_web_client: false,
            },
        );
        negotiated.record();
        negotiated
    };
    let hdr = run(&hdr_item);
    assert_eq!(hdr.decision_kind(), "direct_play");
    assert_eq!(hdr.video_range_unenforced.as_deref(), Some("HDR10"));
    let sdr = run(&sdr_item);
    assert_eq!(sdr.decision_kind(), "direct_play");
    assert_eq!(sdr.video_range_unenforced, None);

    let counted: u64 = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_video_range_unenforced_total"
                && key
                    .labels()
                    .any(|l| l.key() == "range" && l.value() == "HDR10")
        })
        .map(|(.., v)| match v {
            metrics_util::debugging::DebugValue::Counter(n) => n,
            other => panic!("{other:?}"),
        })
        .sum();
    assert_eq!(counted, 1, "one HDR10 would-block, none for the SDR source");
}

/// `Playback.startTracks` answers the tracks a default negotiation starts
/// on: the language rules, then the remembered audio and subtitle, then a
/// remembered subtitle-off. Each answer is checked against the grant's own
/// decision, not a recomputation.
#[actix_web::test]
async fn start_tracks_answer_what_a_default_negotiation_would_play() {
    use pharos_core::{AudioTrack, SubtitleTrack};

    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let mut item = pharos_core::MediaStore::get(&fx.state.stores, MOVIE)
        .await
        .unwrap();
    item.probe.audio_tracks = vec![
        AudioTrack {
            stream_index: 1,
            codec: Some("aac".into()),
            is_default: true,
            ..AudioTrack::default()
        },
        AudioTrack {
            stream_index: 2,
            codec: Some("aac".into()),
            ..AudioTrack::default()
        },
    ];
    item.probe.subtitle_tracks = vec![SubtitleTrack {
        stream_index: 3,
        codec: Some("subrip".into()),
        is_default: true,
        ..SubtitleTrack::default()
    }];
    fx.state.stores.put(item).await.unwrap();
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);

    // Nothing remembered: the language rules pick the default tracks.
    let (_grant, played) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    assert_eq!(
        (played.audio, played.subtitle),
        (Some(1), Some(3)),
        "{played:?}"
    );
    assert_eq!(
        start_tracks(&session, MOVIE, &direct).await,
        Ok((played.audio, played.subtitle))
    );

    // A report remembers audio track 2 and subtitle track 3.
    let (grant, _) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Index(2),
        Choice::Index(3),
    )
    .await
    .unwrap();
    let mut req = grant.report_request();
    req.get().init_ctx();
    req.get().set_position_ms(5_000);
    set_choice(req.get().init_audio(), Choice::Index(2));
    set_choice(req.get().init_subtitle(), Choice::Index(3));
    let reply = req.send().promise.await.expect("report");
    read_done(reply.get().unwrap().get_result().unwrap()).unwrap();
    let (grant, played) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    assert_eq!(
        (played.audio, played.subtitle),
        (Some(2), Some(3)),
        "{played:?}"
    );
    assert_eq!(
        start_tracks(&session, MOVIE, &direct).await,
        Ok((played.audio, played.subtitle))
    );

    // A report that turns subtitles off remembers the off, and it sticks.
    let mut req = grant.report_request();
    req.get().init_ctx();
    req.get().set_position_ms(6_000);
    set_choice(req.get().init_audio(), Choice::Default);
    set_choice(req.get().init_subtitle(), Choice::Off);
    let reply = req.send().promise.await.expect("report");
    read_done(reply.get().unwrap().get_result().unwrap()).unwrap();
    let data = fx
        .state
        .stores
        .get_user_data(fx.user_id, MOVIE)
        .await
        .unwrap();
    assert_eq!(data.subtitle_stream_index, Some(-1), "{data:?}");
    let (_grant, played) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    assert_eq!(
        (played.audio, played.subtitle),
        (Some(2), None),
        "{played:?}"
    );
    assert_eq!(
        start_tracks(&session, MOVIE, &direct).await,
        Ok((played.audio, played.subtitle))
    );

    // A book has no tracks to play.
    assert!(start_tracks(&session, BOOK, &direct).await.is_err());
}

/// T130 / V1011: the native grant reads the deny list of the device its token
/// was issued to, as PlaybackInfo does, so the two APIs agree.
/// Disarm check: without the lookup in `playback::grant`, the second
/// negotiation direct-plays.
#[actix_web::test]
async fn a_device_audio_deny_list_stops_a_native_direct_play() {
    use pharos_store_sqlx::ServerConfigStore as _;
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    // AAC is the conversion codec and a saved deny entry for it is ignored
    // (V1012), so deny the source's E-AC-3 instead.
    fx.state
        .stores
        .put(MediaItem {
            id: MOVIE,
            path: "/libA/native.mp4".into(),
            title: "Native".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                container: Some("mp4".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("eac3".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["eac3"], &["mp4"]);
    let decide = || async {
        negotiate(
            &session,
            WireId::Real(MOVIE),
            &direct,
            Choice::Default,
            Choice::Default,
        )
        .await
        .unwrap()
        .1
    };
    assert_eq!(decide().await.method, "direct_play", "control");

    fx.state
        .stores
        .set_named_config(&playback::denied_audio_codecs_key("test"), r#"["eac3"]"#)
        .await
        .unwrap();
    let denied = decide().await;
    assert_ne!(denied.method, "direct_play");
    assert_eq!(denied.block, "audio_codec_denied");
}

async fn queue_hint(grant: &playback_grant::Client) -> Result<(u32, u64, u32, bool), String> {
    let mut req = grant.queue_hint_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.expect("queue hint");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        pharos_api::pharos_capnp::queue_hint_result::Hint(h) => {
            let h = h.unwrap();
            Ok((
                h.get_queued(),
                h.get_waited_ms(),
                h.get_others_queued(),
                h.get_busy(),
            ))
        }
        pharos_api::pharos_capnp::queue_hint_result::Error(e) => Err(denial(e.unwrap())),
    }
}

/// 038 FR-402, V1217: a player waiting on its stream can ask what it waits
/// behind while the wait is on. A server with no encoder queue answers that
/// nothing is queued, rather than failing. The queue's own counts are pinned
/// in the scheduler's `stream_wait` test.
#[actix_web::test]
async fn a_grant_answers_what_its_playback_waits_behind() {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let (grant, _) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    assert_eq!(queue_hint(&grant).await, Ok((0, 0, 0, false)));
}

/// What `pharos_grant_release_total{outcome}` has counted so far. The
/// registry is process-global, so tests compare before and after.
fn released_count(outcome: &str) -> u64 {
    let _ = pharos_server::obs::init("info", None);
    pharos_server::obs::render()
        .lines()
        .filter(|l| {
            l.starts_with("pharos_grant_release_total{")
                && l.contains(&format!("outcome=\"{outcome}\""))
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

async fn stream_is_live(fx: &Fixture, psid: &str) -> bool {
    fx.state
        .transcode_sessions
        .get(pharos_core::PlaySessionId::from(psid))
        .await
        .unwrap()
        .is_some()
}

async fn settles_gone(fx: &Fixture, psid: &str) -> bool {
    for _ in 0..150 {
        if !stream_is_live(fx, psid).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// B1397: a player whose connection dropped under a live stream negotiates
/// again, and the playback it was on continues under the same session id: the
/// segment URLs it already holds would 410 under a new one.
/// Disarm check: with `adopt_released` returning `None`, the second negotiate
/// mints a new id and the first stream is ended.
#[actix_web::test]
async fn a_negotiate_after_a_release_adopts_the_same_stream() {
    let adopted_before = released_count("adopted");
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let app = test::init_service(crate::common::build_app(fx.state.clone())).await;
    let sessions_listed = || async {
        let req = test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", fx.admin_token.as_str()))
            .to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        body.as_array().unwrap().len()
    };
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let play = || {
        negotiate(
            &session,
            WireId::Real(MOVIE),
            &direct,
            Choice::Default,
            Choice::Default,
        )
    };

    let (grant, first) = play().await.unwrap();
    report(&grant, 30_000, false).await.unwrap();
    drop(grant);
    let (grant, second) = play().await.unwrap();

    assert_eq!(second.psid, first.psid, "the playback keeps its session id");
    assert_eq!(second.url, first.url, "and its stream url");
    assert!(
        stream_is_live(&fx, &first.psid).await,
        "the stream stays open"
    );
    assert_eq!(sessions_listed().await, 1, "one entry, not two");
    assert!(
        released_count("adopted") > adopted_before,
        "the adoption is counted"
    );

    // The adopted grant is a grant like any other: its stop ends the playback.
    stop(&grant, 45_000).await.unwrap();
    assert!(
        settles_gone(&fx, &first.psid).await,
        "its stop ends the stream"
    );
    assert_eq!(sessions_listed().await, 0);
}

/// B1397: a negotiate for a different stream does not take the held one over:
/// the held playback ends at once, grace or not, and the new one is its own.
/// Disarm check: with the shape comparison removed, the transcode negotiate
/// adopts the direct-play session.
#[actix_web::test]
async fn a_negotiate_for_another_stream_ends_the_held_one() {
    let mismatched_before = released_count("ended_mismatch");
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let transcode = caps(&["vp9"], &["opus"], &["webm"]);

    let (grant, first) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    drop(grant);
    let (_grant, second) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &transcode,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();

    assert_ne!(
        first.method, second.method,
        "the control: a different stream"
    );
    assert_ne!(second.psid, first.psid);
    assert!(
        settles_gone(&fx, &first.psid).await,
        "the held stream ends at once, far inside the 30 s grace"
    );
    assert!(stream_is_live(&fx, &second.psid).await);
    assert!(released_count("ended_mismatch") > mismatched_before);
}

/// B1397: another device's negotiate never takes a grant it did not hold; the
/// held one runs out its grace.
/// Disarm check: with the device left out of the key, the laptop adopts it.
#[actix_web::test]
async fn another_device_does_not_adopt_a_released_grant() {
    let (server, fx) = crate::revocation::start_with_release_grace(TEST_GRACE).await;
    seed_playable(&fx).await;
    let after_grace_before = released_count("ended_after_grace");
    let (client, _closed) = crate::revocation::connect(&server).await;
    let tv = crate::revocation::signed_in(&client, &fx.user_token).await;
    let laptop_token = fx.state.stores.issue(fx.user_id, "laptop").await.unwrap();
    let (client2, _closed2) = crate::revocation::connect(&server).await;
    let laptop = crate::revocation::signed_in(&client2, laptop_token.0.expose()).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let (grant, first) = play_on(&tv, &direct).await.unwrap();
    drop(grant);
    let (_grant, second) = play_on(&laptop, &direct).await.unwrap();

    assert_ne!(second.psid, first.psid, "the laptop plays its own stream");
    assert!(
        settles_gone(&fx, &first.psid).await,
        "the tv's runs out its grace"
    );
    assert!(stream_is_live(&fx, &second.psid).await);
    assert!(released_count("ended_after_grace") > after_grace_before);
}

/// B1397: a released grant nobody adopts ends when the grace is up, counted.
/// Disarm check: with the expiry task not ending it, the stream stays open.
#[actix_web::test]
async fn a_released_grant_nobody_adopts_ends_after_the_grace() {
    let (server, fx) = crate::revocation::start_with_release_grace(TEST_GRACE).await;
    seed_playable(&fx).await;
    let after_grace_before = released_count("ended_after_grace");
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let (grant, first) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    drop(grant);
    assert!(
        stream_is_live(&fx, &first.psid).await,
        "held, not ended, inside the grace"
    );
    assert!(settles_gone(&fx, &first.psid).await);
    assert!(released_count("ended_after_grace") > after_grace_before);
}

/// B1397: a revoked token does not adopt; what it held ends with the grace.
/// Disarm check: with the adoption run before the token check, the revoked
/// session's negotiate would take the stream.
#[actix_web::test]
async fn a_revoked_token_does_not_adopt_a_released_grant() {
    let (server, fx) = crate::revocation::start_with_release_grace(TEST_GRACE).await;
    seed_playable(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);
    let (grant, first) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    drop(grant);
    fx.state.stores.revoke(&fx.user_token).await.unwrap();
    let again = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await;
    assert!(again.is_err(), "a revoked session negotiates nothing");
    assert!(settles_gone(&fx, &first.psid).await);
}

async fn play_on(
    session: &session::Client,
    caps: &Capabilities,
) -> Result<(playback_grant::Client, Decided), String> {
    negotiate(
        session,
        WireId::Real(MOVIE),
        caps,
        Choice::Default,
        Choice::Default,
    )
    .await
}

/// `MOVIE` with two audio tracks and one text subtitle the player draws
/// itself, so a track switch does not change what the server streams.
async fn seed_tracks(fx: &Fixture) {
    use pharos_core::{AudioTrack, SubtitleTrack};
    let mut item = pharos_core::MediaStore::get(&fx.state.stores, MOVIE)
        .await
        .unwrap();
    item.probe.audio_tracks = vec![
        AudioTrack {
            stream_index: 1,
            codec: Some("aac".into()),
            is_default: true,
            ..AudioTrack::default()
        },
        AudioTrack {
            stream_index: 2,
            codec: Some("aac".into()),
            ..AudioTrack::default()
        },
    ];
    item.probe.subtitle_tracks = vec![SubtitleTrack {
        stream_index: 3,
        codec: Some("subrip".into()),
        ..SubtitleTrack::default()
    }];
    fx.state.stores.put(item).await.unwrap();
}

async fn report_tracks(grant: &playback_grant::Client, audio: Choice, subtitle: Choice) {
    let mut req = grant.report_request();
    req.get().init_ctx();
    req.get().set_position_ms(10_000);
    set_choice(req.get().init_audio(), audio);
    set_choice(req.get().init_subtitle(), subtitle);
    let reply = req.send().promise.await.expect("report");
    read_done(reply.get().unwrap().get_result().unwrap()).unwrap();
}

/// B1397: a subtitle the player draws itself, switched mid-playback and so
/// remembered, is not a different stream: the reconnect still adopts.
/// Disarm check: with the subtitle compared as picked, the negotiate resolves
/// the remembered track, the shapes differ and a new session id is minted.
#[actix_web::test]
async fn a_remembered_unburned_subtitle_does_not_block_adoption() {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    seed_tracks(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);

    let (grant, first) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Off,
    )
    .await
    .unwrap();
    assert_eq!(first.subtitle, None);
    report_tracks(&grant, Choice::Default, Choice::Index(3)).await;
    drop(grant);
    let (_grant, again) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    assert_eq!(again.psid, first.psid, "{again:?}");
    assert_eq!(again.url, first.url);
}

/// B1397: an audio track switched mid-playback and so remembered does not
/// make the reconnect a different stream: the player is still pulling the one
/// it holds, and the grant names the tracks that stream carries.
/// Disarm check: with the held tracks not used for a default pick, the
/// negotiate resolves the remembered track, the decision differs and a new
/// session id is minted.
#[actix_web::test]
async fn a_remembered_audio_switch_does_not_block_adoption() {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    seed_tracks(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);

    let (grant, first) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Off,
    )
    .await
    .unwrap();
    assert_eq!(first.method, "direct_play", "{first:?}");
    report_tracks(&grant, Choice::Index(2), Choice::Off).await;
    drop(grant);
    let (_grant, again) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Default,
        Choice::Default,
    )
    .await
    .unwrap();
    assert_eq!(again.psid, first.psid, "{again:?}");
    assert_eq!(again.audio, first.audio, "the tracks the stream carries");
}

/// B1397: naming a different audio track on the reconnect is a different
/// stream, so the held one ends and a new session is minted.
/// Disarm check: with audio left out of the shape, the new track is adopted
/// onto the old stream.
#[actix_web::test]
async fn an_explicit_other_audio_track_is_not_adopted() {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    seed_tracks(&fx).await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let direct = caps(&["h264"], &["aac"], &["mp4"]);

    let (grant, first) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Index(1),
        Choice::Off,
    )
    .await
    .unwrap();
    drop(grant);
    let (_grant, second) = negotiate(
        &session,
        WireId::Real(MOVIE),
        &direct,
        Choice::Index(2),
        Choice::Off,
    )
    .await
    .unwrap();
    assert_ne!(second.psid, first.psid, "{second:?}");
    assert_eq!(second.audio, Some(2));
    assert!(settles_gone(&fx, &first.psid).await);
}
