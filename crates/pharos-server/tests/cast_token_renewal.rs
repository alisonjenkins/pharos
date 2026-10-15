//! A cast outlives its first TV token: the token is renewed before it
//! expires, the old one is revoked and the TV is handed the new one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::{counter, eventually, CastRig};
use std::time::Duration;

fn renewed() -> u64 {
    counter("pharos_cast_seat_total", &[("event", "token_renewed")])
}

#[actix_web::test]
async fn a_cast_keeps_working_past_the_first_tokens_expiry() {
    let rig = CastRig::with_cast_settings("tv_token_lifetime_secs = 4").await;
    let before = renewed();
    assert_eq!(rig.play_on_tv(&rig.alice).await.0, 204);
    eventually("TV playing", || rig.receiver.state().playing).await;
    let first = rig.tv_token();
    assert_eq!(rig.token_status(&first).await, 200);

    // Renewal is due at half the lifetime (2 s); the first token expires between 3 s and 4 s.
    tokio::time::sleep(Duration::from_millis(2_600)).await;
    assert_eq!(
        rig.token_status(&first).await,
        401,
        "the first token is revoked on renewal, before it expires"
    );
    let current = rig
        .receiver
        .commands()
        .last()
        .map(|c| c.access_token.expose().to_owned())
        .expect("the TV was sent commands");
    assert_ne!(current, first, "the TV was handed a new token");
    assert_eq!(rig.token_status(&current).await, 200);

    tokio::time::sleep(Duration::from_millis(2_400)).await;
    let latest = rig
        .receiver
        .commands()
        .last()
        .map(|c| c.access_token.expose().to_owned())
        .expect("the TV was sent commands");
    assert_eq!(
        rig.token_status(&latest).await,
        200,
        "the cast still authenticates after the first token's lifetime"
    );
    assert_eq!(rig.service.snapshot().await.len(), 1, "still one seat");
    assert!(renewed() > before, "a token_renewed event was recorded");
}
