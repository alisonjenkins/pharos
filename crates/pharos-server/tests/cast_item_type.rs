//! A `Load` reaches the TV typed by the item's stored kind: music as `Audio`,
//! everything else as `Video`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::CastRig;
use pharos_cast::messages::CommandBody;
use pharos_core::MediaKind;
use pharos_server::cast_service::StartCast;

async fn play_now_item(kind: MediaKind) -> (String, String) {
    let (rig, wire) = CastRig::with_stored_item(kind).await;
    rig.service
        .start(StartCast {
            user_id: rig.alice.id,
            user_name: rig.alice.name.clone(),
            owner_sync_key: rig.alice.key.clone(),
            tv_name: cast_rig::TV.into(),
        })
        .await
        .unwrap();
    cast_rig::eventually("PlayNow on the emulator", || {
        rig.receiver
            .commands()
            .iter()
            .any(|c| c.body.name() == "PlayNow")
    })
    .await;
    let sent = rig
        .receiver
        .commands()
        .into_iter()
        .find_map(|c| match c.body {
            CommandBody::PlayNow(opts) => opts.items.into_iter().next(),
            _ => None,
        })
        .expect("a PlayNow with an item");
    assert_eq!(sent.id, wire);
    (sent.id, sent.kind)
}

#[actix_web::test]
async fn a_music_cast_is_sent_to_the_tv_as_audio() {
    assert_eq!(play_now_item(MediaKind::Audio).await.1, "Audio");
}

#[actix_web::test]
async fn a_movie_cast_is_sent_to_the_tv_as_video() {
    assert_eq!(play_now_item(MediaKind::Movie).await.1, "Video");
}
