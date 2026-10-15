//! The production `CastV2Transport` against `MockTv` over real TLS.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use pharos_cast::castv2::{CastV2Config, CastV2Transport, TlsPolicy};
use pharos_cast::messages::{
    AccessToken, CommandBody, PlayItem, PlayNowOptions, ReceiverCommand, ReceiverReport,
    SeekOptions,
};
use pharos_cast::mock_tv::{LaunchBehavior, MockTv};
use pharos_cast::transport::{CastError, CastTransport, TransportEvent};

const APP: &str = "F007D354";

fn command(body: CommandBody) -> ReceiverCommand {
    ReceiverCommand {
        body,
        server_address: "http://pharos.lan:8096".into(),
        access_token: AccessToken::new("tok"),
        receiver_name: None,
        user_id: None,
        device_id: None,
        server_id: None,
        server_version: None,
        max_bitrate: None,
    }
}

fn play_now(start_ms: i64) -> ReceiverCommand {
    command(CommandBody::PlayNow(PlayNowOptions {
        items: vec![PlayItem {
            id: "item-1".into(),
            kind: "Movie".into(),
        }],
        start_index: 0,
        start_position_ticks: start_ms * 10_000,
        media_source_id: None,
        audio_stream_index: None,
        subtitle_stream_index: None,
    }))
}

fn transport(tv: &MockTv, policy: TlsPolicy) -> CastV2Transport {
    CastV2Transport::with_config(
        "127.0.0.1",
        tv.addr().port(),
        policy,
        CastV2Config {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(2),
            ping_interval: Duration::from_millis(100),
            heartbeat_timeout: Duration::from_secs(2),
        },
    )
}

async fn launched(tv: &MockTv) -> CastV2Transport {
    let mut t = transport(tv, TlsPolicy::AcceptAny);
    t.connect().await.unwrap();
    t.launch(APP).await.unwrap();
    t
}

async fn next_report(t: &mut CastV2Transport) -> ReceiverReport {
    match tokio::time::timeout(Duration::from_secs(5), t.recv())
        .await
        .expect("a report within 5 s")
    {
        TransportEvent::Report(r) => r,
        other => panic!("expected a report, got {other:?}"),
    }
}

#[tokio::test]
async fn play_now_starts_the_model_and_reports_flow_back_each_second() {
    let tv = MockTv::start().await.unwrap();
    let mut t = launched(&tv).await;
    t.send(&play_now(30_000)).await.unwrap();

    assert!(matches!(
        next_report(&mut t).await,
        ReceiverReport::PlaybackStart(_)
    ));
    assert!(matches!(
        next_report(&mut t).await,
        ReceiverReport::PlayStateChange(_)
    ));
    let started = tokio::time::Instant::now();
    let progress = next_report(&mut t).await;
    assert!(matches!(progress, ReceiverReport::PlaybackProgress(_)));
    assert!(started.elapsed() >= Duration::from_millis(800));
    let ms = progress.play_state().unwrap().position_ticks.unwrap() / 10_000;
    assert!((30_900..32_000).contains(&ms), "{ms}");

    assert_eq!(tv.commands(), vec![play_now(30_000)]);
    assert!(tv.malformed().is_empty());
    assert_eq!(tv.state().item_id.as_deref(), Some("item-1"));
}

#[tokio::test]
async fn pause_seek_and_the_remote_reach_the_model_and_come_back() {
    let tv = MockTv::start().await.unwrap();
    let mut t = launched(&tv).await;
    t.send(&play_now(0)).await.unwrap();
    t.send(&command(CommandBody::Pause)).await.unwrap();
    t.send(&command(CommandBody::Seek(SeekOptions { position: 90.0 })))
        .await
        .unwrap();
    for _ in 0..50 {
        if tv.state().position_ms == 90_000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(tv.state().paused);
    assert_eq!(tv.state().position_ms, 90_000);

    tv.receiver().user_seek(Duration::from_secs(10));
    loop {
        let r = next_report(&mut t).await;
        if r.play_state().and_then(|p| p.position_ticks) == Some(100_000_000) {
            break;
        }
    }
}

#[tokio::test]
async fn a_pinned_fingerprint_matches_and_a_wrong_one_is_refused() {
    let tv = MockTv::start().await.unwrap();
    let mut ok = transport(&tv, TlsPolicy::PinSha256(tv.fingerprint()));
    ok.connect().await.unwrap();
    ok.close().await;
    let mut bad = transport(&tv, TlsPolicy::PinSha256([9; 32]));
    assert!(matches!(
        bad.connect().await,
        Err(CastError::Connect { .. })
    ));
}

#[tokio::test]
async fn dropping_the_connection_closes_the_transport_and_a_new_one_resumes() {
    let tv = MockTv::start().await.unwrap();
    let mut t = launched(&tv).await;
    t.send(&play_now(5_000)).await.unwrap();
    assert!(matches!(
        next_report(&mut t).await,
        ReceiverReport::PlaybackStart(_)
    ));
    tv.drop_connection();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), t.recv())
            .await
            .unwrap()
        {
            TransportEvent::Closed { .. } => break,
            TransportEvent::Report(_) => {}
            other => panic!("{other:?}"),
        }
    }
    let mut again = launched(&tv).await;
    assert_eq!(tv.connections(), 2);
    assert!(tv.state().playing, "the TV kept playing while disconnected");
    again.close().await;
}

#[tokio::test]
async fn a_refused_launch_names_the_receiver_error_and_a_delayed_one_waits() {
    let tv = MockTv::start().await.unwrap();
    tv.set_launch(LaunchBehavior::Refuse);
    let mut t = transport(&tv, TlsPolicy::AcceptAny);
    t.connect().await.unwrap();
    let err = t.launch(APP).await.unwrap_err().to_string();
    assert!(err.contains("LAUNCH_ERROR"), "{err}");

    tv.set_launch(LaunchBehavior::Delay(Duration::from_millis(400)));
    let started = tokio::time::Instant::now();
    t.launch(APP).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(tv.launches(), 2);
}

#[tokio::test]
async fn heartbeat_pings_are_answered() {
    let tv = MockTv::start().await.unwrap();
    let mut t = launched(&tv).await;
    // recv drives the connection; nothing else arrives, so it times out.
    let _ = tokio::time::timeout(Duration::from_millis(500), t.recv()).await;
    assert!(tv.pings() >= 2, "pings {}", tv.pings());
}
