//! 009 US1 over HTTP: the TV session lists the cast's owner and item, and a
//! play request aimed at it starts a cast.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::{CastRig, TV};
use pharos_cast::events::CastReason;
use pharos_server::cast_service::StartCast;
use pharos_server::state::SocketBroadcast;
use std::time::Duration;

fn start_request(p: &cast_rig::Person) -> StartCast {
    StartCast {
        user_id: p.id,
        user_name: p.name.clone(),
        owner_sync_key: p.key.clone(),
        tv_name: TV.into(),
    }
}

async fn wait_for_tv_owner(rig: &CastRig, expected: &str) -> serde_json::Value {
    for _ in 0..250 {
        let tv = rig.tv_session(&rig.bob).await;
        if tv["UserName"] == expected {
            return tv;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("TV session never showed owner {expected:?}");
}

#[actix_web::test]
async fn the_tv_session_shows_the_owner_and_item_while_a_cast_runs_and_not_after() {
    let mut rig = CastRig::new().await;
    assert_eq!(rig.tv_session(&rig.bob).await["UserName"], "");
    rig.bus_events();

    rig.service.start(start_request(&rig.alice)).await.unwrap();
    let tv = wait_for_tv_owner(&rig, "alice").await;
    assert_eq!(tv["UserId"], rig.alice.id.0.simple().to_string());
    assert_eq!(tv["NowPlayingItemId"], cast_rig::ITEM);
    assert!(
        rig.bus_events()
            .iter()
            .any(|b| matches!(b, SocketBroadcast::SessionsChanged)),
        "sockets are told to re-read Sessions when a cast starts"
    );

    rig.service.stop(rig.alice.id, CastReason::UserStop).await;
    wait_for_tv_owner(&rig, "").await;
    let tv = rig.tv_session(&rig.bob).await;
    assert!(
        tv["NowPlayingItemId"].is_null(),
        "no item once the cast ended"
    );
    assert!(
        rig.bus_events()
            .iter()
            .any(|b| matches!(b, SocketBroadcast::SessionsChanged)),
        "sockets are told to re-read Sessions when a cast ends"
    );
}
