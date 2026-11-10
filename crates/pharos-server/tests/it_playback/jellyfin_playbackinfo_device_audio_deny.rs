#![allow(clippy::unwrap_used, clippy::expect_used)]
//! T130 / V1010–V1012: an admin turns off direct play of an audio codec for
//! one device. That device's PlaybackInfo then remuxes the audio, and every
//! other device keeps its own profile's verdict.

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState, state::Stores,
};

/// A TV that honestly lists E-AC-3 and Matroska.
const TV_PROFILE: &str = r#"{"DeviceProfile":{
  "DirectPlayProfiles":[{"Type":"Video","Container":"mkv,mp4","VideoCodec":"h264","AudioCodec":"aac,ac3,eac3"}],
  "TranscodingProfiles":[{"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"eac3,aac"}]
}}"#;

const MOVIE: u64 = 1;

struct Fixture {
    state: web::Data<AppState>,
    /// The admin's token on the TV (`device "tv"`).
    tv: String,
    /// The same admin on a laptop (`device "laptop"`).
    laptop: String,
}

async fn setup() -> Fixture {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "admin".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..UserPolicy::default()
            },
        })
        .await
        .unwrap();
    let tv = stores.issue(uid, "tv").await.unwrap();
    let laptop = stores.issue(uid, "laptop").await.unwrap();
    stores
        .put(MediaItem {
            id: MOVIE,
            path: "/a-beautiful-mind.mkv".into(),
            title: "A Beautiful Mind".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(8_000_000),
                container: Some("matroska,webm".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("eac3".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    Fixture {
        state: web::Data::new(AppState::new(stores, "t".into())),
        tv: tv.0.expose().to_string(),
        laptop: laptop.0.expose().to_string(),
    }
}

async fn call(
    fx: &Fixture,
    req: test::TestRequest,
    token: &str,
) -> (StatusCode, serde_json::Value) {
    let app = test::init_service(
        App::new()
            .app_data(fx.state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let resp = test::call_service(
        &app,
        req.insert_header(("X-Emby-Token", token)).to_request(),
    )
    .await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn playback_info(fx: &Fixture, token: &str) -> serde_json::Value {
    let req = test::TestRequest::post()
        .uri(&format!("/Items/{MOVIE}/PlaybackInfo"))
        .insert_header(("content-type", "application/json"))
        .set_payload(TV_PROFILE);
    call(fx, req, token).await.1
}

async fn set_options(fx: &Fixture, device: &str, body: &str) -> StatusCode {
    let req = test::TestRequest::post()
        .uri(&format!("/Devices/Options?id={device}"))
        .insert_header(("content-type", "application/json"))
        .set_payload(body.to_owned());
    call(fx, req, &fx.tv).await.0
}

async fn devices(fx: &Fixture) -> serde_json::Value {
    call(fx, test::TestRequest::get().uri("/Devices"), &fx.tv)
        .await
        .1
}

fn direct_play(v: &serde_json::Value) -> bool {
    v["MediaSources"][0]["SupportsDirectPlay"]
        .as_bool()
        .unwrap()
}

fn device<'a>(list: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    list["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["Id"] == id)
        .unwrap()
}

#[actix_web::test]
async fn a_denied_codec_stops_direct_play_on_that_device_only() {
    let fx = setup().await;
    assert!(direct_play(&playback_info(&fx, &fx.tv).await), "control");

    assert_eq!(
        set_options(&fx, "tv", r#"{"NoDirectPlayAudioCodecs":["EAC3"]}"#).await,
        StatusCode::NO_CONTENT
    );

    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let v = playback_info(&fx, &fx.tv).await;
    let ms = &v["MediaSources"][0];
    assert!(!direct_play(&v), "{v}");
    assert_eq!(ms["SupportsDirectStream"], false, "{v}");
    assert!(ms["TranscodingUrl"].as_str().is_some(), "{v}");
    let counted = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .any(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_playback_decision_total"
                && key
                    .labels()
                    .any(|l| l.key() == "direct_play_block" && l.value() == "audio_codec_denied")
        });
    assert!(counted, "the decision counter names the device setting");

    assert!(
        direct_play(&playback_info(&fx, &fx.laptop).await),
        "another device keeps its own verdict"
    );
}

#[actix_web::test]
async fn the_device_list_shows_the_setting_and_an_empty_list_clears_it() {
    let fx = setup().await;
    assert_eq!(
        set_options(&fx, "tv", r#"{"CustomName":"Living room TV"}"#).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        set_options(
            &fx,
            "tv",
            r#"{"NoDirectPlayAudioCodecs":["eac3"," AC3 ","eac3"]}"#
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let list = devices(&fx).await;
    let tv = device(&list, "tv");
    assert_eq!(
        tv["NoDirectPlayAudioCodecs"],
        serde_json::json!(["ac3", "eac3"])
    );
    assert_eq!(tv["Name"], "Living room TV", "the name is left alone");
    assert!(
        device(&list, "laptop")
            .get("NoDirectPlayAudioCodecs")
            .is_none(),
        "an unset device carries no extension field"
    );

    assert_eq!(
        set_options(&fx, "tv", r#"{"NoDirectPlayAudioCodecs":[]}"#).await,
        StatusCode::NO_CONTENT
    );
    assert!(device(&devices(&fx).await, "tv")
        .get("NoDirectPlayAudioCodecs")
        .is_none());
    assert!(direct_play(&playback_info(&fx, &fx.tv).await));
}

#[actix_web::test]
async fn a_malformed_codec_is_refused_and_nothing_is_written() {
    let fx = setup().await;
    assert_eq!(
        set_options(
            &fx,
            "tv",
            r#"{"CustomName":"x","NoDirectPlayAudioCodecs":["eac3","dts; drop"]}"#
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    let list = devices(&fx).await;
    let tv = device(&list, "tv");
    assert!(tv.get("NoDirectPlayAudioCodecs").is_none(), "{tv}");
    assert_eq!(
        tv["Name"], "tv",
        "the name in the same body was not written"
    );
}

/// AAC is what every conversion produces, so it cannot be turned off.
#[actix_web::test]
async fn aac_cannot_be_turned_off() {
    let fx = setup().await;
    assert_eq!(
        set_options(&fx, "tv", r#"{"NoDirectPlayAudioCodecs":["eac3","AAC"]}"#).await,
        StatusCode::BAD_REQUEST
    );
    assert!(device(&devices(&fx).await, "tv")
        .get("NoDirectPlayAudioCodecs")
        .is_none());
}

/// A body that does not parse is refused, not read as "no fields" and
/// answered as if it had been saved.
#[actix_web::test]
async fn a_malformed_body_is_refused() {
    let fx = setup().await;
    assert_eq!(
        set_options(&fx, "tv", r#"{"NoDirectPlayAudioCodecs":"eac3"}"#).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        set_options(&fx, "tv", "{not json").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(set_options(&fx, "tv", "").await, StatusCode::NO_CONTENT);
}

/// A list saved before AAC was refused still holds it. The device list drops
/// it, so the admin page's next save (the whole list) is not refused for it.
#[actix_web::test]
async fn a_saved_aac_entry_is_ignored_and_does_not_block_the_next_save() {
    use pharos_store_sqlx::ServerConfigStore as _;
    let fx = setup().await;
    fx.state
        .stores
        .set_named_config(
            &pharos_server::services::playback::denied_audio_codecs_key("tv"),
            r#"["aac","eac3"]"#,
        )
        .await
        .unwrap();
    let list = devices(&fx).await;
    let shown = device(&list, "tv")["NoDirectPlayAudioCodecs"].clone();
    assert_eq!(shown, serde_json::json!(["eac3"]));
    let mut next: Vec<String> = serde_json::from_value(shown).unwrap();
    next.push("ac3".into());
    let body = serde_json::json!({ "NoDirectPlayAudioCodecs": next }).to_string();
    assert_eq!(set_options(&fx, "tv", &body).await, StatusCode::NO_CONTENT);
}

/// A user who is not an admin manages the devices they are signed in on, and
/// only those: the list shows them alone, and another user's device is
/// refused with nothing written (ADR-0095).
#[actix_web::test]
async fn a_user_manages_the_devices_they_are_signed_in_on_and_no_others() {
    let fx = setup().await;
    let auth = BuiltinAuth::new(fx.state.stores.clone());
    let hash = auth.hash_password(&SecretString::new("q")).unwrap();
    let jana = UserId::new();
    fx.state
        .stores
        .create(UserRecord {
            id: jana,
            name: "jana".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let phone = fx.state.stores.issue(jana, "phone").await.unwrap();
    let phone = phone.0.expose().to_string();

    let set = |device: &'static str, body: &'static str| {
        test::TestRequest::post()
            .uri(&format!("/Devices/Options?id={device}"))
            .insert_header(("content-type", "application/json"))
            .set_payload(body)
    };
    assert_eq!(
        call(
            &fx,
            set("phone", r#"{"NoDirectPlayAudioCodecs":["eac3"]}"#),
            &phone
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &fx,
            set("tv", r#"{"NoDirectPlayAudioCodecs":["dts"]}"#),
            &phone
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(
        device(&devices(&fx).await, "tv")
            .get("NoDirectPlayAudioCodecs")
            .is_none(),
        "the refused body wrote nothing"
    );

    let (status, mine) = call(&fx, test::TestRequest::get().uri("/Devices"), &phone).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = mine["Items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["Id"].as_str())
        .collect();
    assert_eq!(ids, ["phone"], "{mine}");
    assert_eq!(
        device(&mine, "phone")["NoDirectPlayAudioCodecs"],
        serde_json::json!(["eac3"])
    );
    // The admin's list still has every device, the phone's setting included.
    assert_eq!(
        device(&devices(&fx).await, "phone")["NoDirectPlayAudioCodecs"],
        serde_json::json!(["eac3"])
    );
}
