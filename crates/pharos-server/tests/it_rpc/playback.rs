//! Spec 017 T037: a native player negotiating an item and reporting where it
//! is, through the same services as `/Items/{id}/PlaybackInfo` and
//! `/Sessions/Playing`.

use actix_web::test;
use pharos_api::pharos_capnp::{
    decision_result, done, error, negotiate_result, playback_grant, session, stream_choice,
};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore as _, UserDataStore as _, UserPolicy,
    UserStore as _,
};
use pharos_jellyfin_api::dto::{wire_item_id, WireId};
use pharos_server::services::playback::{self, Capabilities, NegotiateRequest, PlaybackSwitches};

use crate::common::Fixture;

const MOVIE: u64 = 300;
const BOOK: u64 = 301;
const NATIVE_UA: &str = "pharos-native-test/1.0";

async fn seed_playable(fx: &Fixture) {
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
            })
        }
        decision_result::Error(e) => Err(denial(e.unwrap())),
    }
}

async fn negotiate(
    session: &session::Client,
    id: WireId,
    caps: &Capabilities,
    audio: Choice,
    subtitle: Choice,
) -> Result<(playback_grant::Client, Decided), String> {
    let playback = session.playback_request().send().pipeline.get_playback();
    let mut req = playback.negotiate_request();
    let mut p = req.get();
    p.reborrow().init_ctx();
    {
        let mut out = p.reborrow().init_id();
        match id {
            WireId::Real(n) => out.set_real(n),
            WireId::Synth(n) => out.set_synth(n),
        }
    }
    {
        let mut c = p.reborrow().init_caps();
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
    }
    set_choice(p.reborrow().init_audio(), audio);
    set_choice(p.reborrow().init_subtitle(), subtitle);
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
    let profile = caps.profile();
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
        let profile = caps.profile();
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
