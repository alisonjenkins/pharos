#![allow(clippy::unwrap_used, clippy::expect_used)]
//! T130 / ADR-0095: a user who is not an admin sees the devices they are
//! signed in on over the native API, and turns sound formats off for those
//! devices, and only those.
//! Disarm check: with `set_own_converted_audio` skipping its
//! `is_signed_in_on` test, `another_users_device_is_refused` writes the list.

use pharos_api::pharos_capnp::{done, error, my_devices_result, session};
use pharos_core::TokenStore as _;
use pharos_server::services::playback;

/// (id, name, current, converted audio) per device, in the order served.
async fn my_devices(session: &session::Client) -> Vec<(String, String, bool, Vec<String>)> {
    let mut req = session.my_devices_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        my_devices_result::Devices(list) => list
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d.get_id().unwrap().to_string().unwrap(),
                    d.get_name().unwrap().to_string().unwrap(),
                    d.get_current(),
                    d.get_converted_audio()
                        .unwrap()
                        .iter()
                        .map(|c| c.unwrap().to_string().unwrap())
                        .collect(),
                )
            })
            .collect(),
        my_devices_result::Error(_) => panic!("myDevices refused"),
    }
}

/// `Ok` or the refusal's kind.
async fn set(session: &session::Client, device: &str, codecs: &[&str]) -> Result<(), &'static str> {
    let mut req = session.set_device_converted_audio_request();
    req.get().init_ctx();
    req.get().set_device_id(device);
    let mut list = req.get().init_codecs(codecs.len() as u32);
    for (i, c) in codecs.iter().enumerate() {
        list.set(i as u32, c);
    }
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        done::Ok(()) => Ok(()),
        done::Error(e) => Err(match e.unwrap().get_kind().which().unwrap() {
            error::kind::NotPermitted(_) => "not_permitted",
            error::kind::Invalid(_) => "invalid",
            _ => "other",
        }),
    }
}

#[actix_web::test]
async fn a_user_lists_and_sets_their_own_devices() {
    let (server, fx) = crate::revocation::start().await;
    fx.state.stores.issue(fx.user_id, "phone").await.unwrap();
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;

    let devices = my_devices(&session).await;
    let ids: Vec<(&str, bool)> = devices.iter().map(|d| (d.0.as_str(), d.2)).collect();
    assert_eq!(ids, [("test", true), ("phone", false)], "{devices:?}");

    assert_eq!(set(&session, "phone", &["EAC3", "ac3"]).await, Ok(()));
    let phone = my_devices(&session)
        .await
        .into_iter()
        .find(|d| d.0 == "phone")
        .unwrap();
    assert_eq!(phone.3, ["ac3", "eac3"]);
    // PlaybackInfo and the native grant read the same stored list.
    assert_eq!(
        playback::denied_audio_codecs(&fx.state, "phone").await,
        ["ac3", "eac3"]
    );

    assert_eq!(set(&session, "phone", &[]).await, Ok(()));
    assert!(playback::denied_audio_codecs(&fx.state, "phone")
        .await
        .is_empty());

    assert_eq!(set(&session, "phone", &["aac"]).await, Err("invalid"));
    assert_eq!(set(&session, "phone", &["dts; drop"]).await, Err("invalid"));
}

#[actix_web::test]
async fn another_users_device_is_refused() {
    let (server, fx) = crate::revocation::start().await;
    fx.state.stores.issue(fx.admin_id, "tv").await.unwrap();
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;

    assert_eq!(set(&session, "tv", &["eac3"]).await, Err("not_permitted"));
    assert!(
        playback::denied_audio_codecs(&fx.state, "tv")
            .await
            .is_empty(),
        "nothing was written"
    );
    assert!(
        my_devices(&session).await.iter().all(|d| d.0 != "tv"),
        "the list holds only the user's own devices"
    );
    // A media token's device is the session's, never a device of its own.
    assert_eq!(
        set(&session, "pharos-native:media", &["eac3"]).await,
        Err("not_permitted")
    );
}
