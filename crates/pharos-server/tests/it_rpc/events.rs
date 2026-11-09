//! Spec 017 US2 (T041–T043, T026a, T045): `Events.subscribe` pushes a user's
//! changes to their sink with a per-user `seq`, replays after `afterSeq`, and
//! resyncs from outside the window.

use std::rc::Rc;
use std::time::Duration;

use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_api::pharos_capnp::{event, event_sink, item_id, session, subscribe_result};
use pharos_api::{server, Connection};
use pharos_server::state::SocketBroadcast;
use tokio::sync::mpsc;

use crate::common::{seed_rich, Fixture};
use crate::telemetry::{connect, counter, session_for};

/// One pushed event as the test reads it.
#[derive(Clone, Debug, PartialEq)]
enum Got {
    UserData {
        seq: u64,
        id: u64,
        played: bool,
        favorite: bool,
    },
    Library {
        seq: u64,
        added: Vec<u64>,
    },
    Task {
        seq: u64,
        id: String,
    },
    Resync {
        seq: u64,
    },
}

impl Got {
    fn seq(&self) -> u64 {
        match self {
            Got::UserData { seq, .. }
            | Got::Library { seq, .. }
            | Got::Task { seq, .. }
            | Got::Resync { seq } => *seq,
        }
    }
}

struct TestSink(mpsc::UnboundedSender<Got>);

impl server::event_sink::Handler for TestSink {
    async fn push(
        self: Rc<Self>,
        params: event_sink::PushParams,
        results: &mut event_sink::PushResults,
    ) -> Result<(), capnp::Error> {
        for e in params.get()?.get_events()? {
            let seq = e.get_seq();
            let got = match e.which()? {
                event::UserData(u) => {
                    let u = u?;
                    let id = match u.get_id()?.which()? {
                        item_id::Real(n) | item_id::Synth(n) => n,
                    };
                    let s = u.get_state()?;
                    Got::UserData {
                        seq,
                        id,
                        played: s.get_played(),
                        favorite: s.get_favorite(),
                    }
                }
                event::LibraryChanged(l) => Got::Library {
                    seq,
                    added: l?
                        .get_added()?
                        .iter()
                        .map(|id| match id.which() {
                            Ok(item_id::Real(n)) => n,
                            _ => 0,
                        })
                        .collect(),
                },
                event::Task(t) => Got::Task {
                    seq,
                    id: t?.get_id()?.to_string()?,
                },
                event::Resync(()) => Got::Resync { seq },
            };
            let _ = self.0.send(got);
        }
        results.get().init_result().set_ok(());
        Ok(())
    }
}

/// A subscription, or the refusal's `Error.kind` name.
async fn subscribe(
    session: &session::Client,
    topics: &[&str],
    after_seq: u64,
) -> Result<(capnp::capability::Client, mpsc::UnboundedReceiver<Got>), String> {
    let events = session.events_request().send().pipeline.get_events();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut req = events.subscribe_request();
    req.get().init_ctx();
    req.get()
        .set_sink(server::event_sink::client(TestSink(tx), &Connection::new()));
    let mut list = req.get().init_topics(u32::try_from(topics.len()).unwrap());
    for (i, t) in (0u32..).zip(topics) {
        list.set(i, *t);
    }
    req.get().set_after_seq(after_seq);
    let reply = req.send().promise.await.expect("subscribe");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        subscribe_result::Subscription(sub) => Ok((sub.unwrap().client, rx)),
        subscribe_result::Error(e) => {
            let e = e.unwrap();
            let kind = match e.get_kind().which().unwrap() {
                pharos_api::pharos_capnp::error::kind::Limit(l) => {
                    format!(
                        "limit: {}",
                        l.unwrap().get_name().unwrap().to_str().unwrap()
                    )
                }
                pharos_api::pharos_capnp::error::kind::Invalid(i) => {
                    format!(
                        "invalid: {}",
                        i.unwrap().get_field().unwrap().to_str().unwrap()
                    )
                }
                pharos_api::pharos_capnp::error::kind::Revoked(()) => "revoked".to_owned(),
                _ => "other".to_owned(),
            };
            Err(kind)
        }
    }
}

async fn next(rx: &mut mpsc::UnboundedReceiver<Got>) -> Got {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an event within 5 s")
        .expect("sink open")
}

/// Nothing arrives within a short wait.
async fn quiet(rx: &mut mpsc::UnboundedReceiver<Got>) {
    if let Ok(Some(got)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        panic!("expected nothing, got {got:?}");
    }
}

fn played(fx: &Fixture, id: u64, played: bool, favorite: bool) {
    let dto = pharos_jellyfin_api::dto::UserItemDataDto::from_domain(
        id,
        pharos_core::UserItemData {
            played,
            is_favorite: favorite,
            ..Default::default()
        },
    );
    fx.state.notify_user_data_changed(
        &fx.user_id.0.simple().to_string(),
        vec![serde_json::to_value(&dto).unwrap()],
    );
}

/// A change made on another connection arrives as one `userData` event with
/// the next `seq`, and only on the changing user's stream.
/// Disarm check: with `EventLog::translate` ignoring `UserDataChanged`, the
/// first `next` times out.
#[actix_web::test]
async fn a_change_made_elsewhere_arrives_with_the_next_seq() {
    let fx = seed_rich().await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let mine = session_for(&connect(&fx), &fx.user_token).await;
            let admin = session_for(&connect(&fx), &fx.admin_token).await;
            let (_sub, mut rx) = subscribe(&mine, &[], 0).await.unwrap();
            let (_asub, mut arx) = subscribe(&admin, &[], 0).await.unwrap();
            let head = fx.state.events.head(fx.user_id).unwrap();

            // The real write path, through the native UserData on a second
            // connection of the same user.
            let other = session_for(&connect(&fx), &fx.user_token).await;
            let ud = other.user_data_request().send().pipeline.get_user_data();
            let mut req = ud.set_played_request();
            req.get().init_ctx();
            req.get().init_id().set_real(fx.rich_item_id);
            req.get().set_played(true);
            req.send().promise.await.unwrap();

            assert_eq!(
                next(&mut rx).await,
                Got::UserData {
                    seq: head + 1,
                    id: fx.rich_item_id,
                    played: true,
                    favorite: false
                }
            );
            quiet(&mut arx).await;
        })
        .await;
}

/// Resuming after a `seq` replays exactly what followed it; one older than
/// the window, or from before a restart, yields one `resync` and then live
/// events.
/// Disarm check: with `Stream::since` always answering `Live`, the replay
/// assertion fails.
#[actix_web::test]
async fn resume_replays_after_seq_and_resyncs_outside_the_window() {
    let fx = seed_rich().await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let s = session_for(&connect(&fx), &fx.user_token).await;
            let (sub, mut rx) = subscribe(&s, &[], 0).await.unwrap();
            played(&fx, 1, true, false);
            let first = next(&mut rx).await.seq();
            drop(sub);
            drop(rx);
            played(&fx, 2, true, false);
            played(&fx, 3, false, true);
            // Let the log read the bus before resuming.
            tokio::time::sleep(Duration::from_millis(50)).await;

            let (_sub, mut rx) = subscribe(&s, &[], first).await.unwrap();
            let replayed = [next(&mut rx).await, next(&mut rx).await];
            assert_eq!(
                replayed,
                [
                    Got::UserData {
                        seq: first + 1,
                        id: 2,
                        played: true,
                        favorite: false
                    },
                    Got::UserData {
                        seq: first + 2,
                        id: 3,
                        played: false,
                        favorite: true
                    },
                ]
            );

            let (_stale, mut stale_rx) = subscribe(&s, &[], 5).await.unwrap();
            assert_eq!(next(&mut stale_rx).await, Got::Resync { seq: first + 2 });
            played(&fx, 4, true, true);
            assert_eq!(next(&mut stale_rx).await.seq(), first + 3);
        })
        .await;
}

/// A subscriber gets only the topics it asked for, and an unknown topic is
/// refused `invalid`.
/// Disarm check: with the topic filter removed from the pump, the user data
/// change reaches the library-only subscriber.
#[actix_web::test]
async fn only_the_asked_topics_arrive() {
    let fx = seed_rich().await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let s = session_for(&connect(&fx), &fx.user_token).await;
            assert_eq!(
                subscribe(&s, &["userData", "weather"], 0).await.err(),
                Some("invalid: topics".to_owned())
            );
            let (_sub, mut rx) = subscribe(&s, &["library"], 0).await.unwrap();
            played(&fx, 1, true, false);
            fx.state.notify_library_delta(&[7], &[]);
            fx.state.notify_refresh_progress("lib", 10.0);
            match next(&mut rx).await {
                Got::Library { added, .. } => assert_eq!(added, vec![7]),
                other => panic!("expected the library change, got {other:?}"),
            }
            quiet(&mut rx).await;

            let (_tasks, mut trx) = subscribe(&s, &["task"], 0).await.unwrap();
            fx.state.notify_refresh_progress("lib", 20.0);
            assert!(matches!(next(&mut trx).await, Got::Task { id, .. } if id == "lib"));
        })
        .await;
}

/// The subscription past the per-connection cap answers `limit
/// subscriptions` and is counted; releasing one frees its slot, and issued
/// and released capabilities balance (T045's leak check).
/// Disarm check: with the cap check removed, the ninth subscribe succeeds;
/// with `Held`'s `Drop` removed, the slot stays taken.
#[actix_web::test]
async fn subscriptions_past_the_cap_are_refused_and_released_ones_free_a_slot() {
    use pharos_server::api::pharos::rpc::events::{
        CAPABILITIES_TOTAL, MAX_SUBSCRIPTIONS_PER_CONNECTION,
    };
    let fx = seed_rich().await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    tokio::task::LocalSet::new()
        .run_until(async {
            let s = session_for(&connect(&fx), &fx.user_token).await;
            let mut held = Vec::new();
            for _ in 0..MAX_SUBSCRIPTIONS_PER_CONNECTION {
                held.push(subscribe(&s, &[], 0).await.unwrap());
            }
            assert_eq!(
                subscribe(&s, &[], 0).await.err(),
                Some("limit: subscriptions".to_owned())
            );
            assert_eq!(
                counter(
                    &snapshotter,
                    "pharos_rpc_limit_total",
                    &[("limit", "subscriptions")]
                ),
                1
            );
            // Another connection has its own cap.
            let other = session_for(&connect(&fx), &fx.user_token).await;
            assert!(subscribe(&other, &[], 0).await.is_ok());

            drop(held.pop());
            tokio::time::sleep(Duration::from_millis(50)).await;
            let again = subscribe(&s, &[], 0).await;
            assert!(again.is_ok(), "{:?}", again.err());
            drop(again);
            drop(held);
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;
    let issued = counter(
        &snapshotter,
        CAPABILITIES_TOTAL,
        &[("type", "subscription"), ("transition", "issued")],
    );
    let released = counter(
        &snapshotter,
        CAPABILITIES_TOTAL,
        &[("type", "subscription"), ("transition", "released")],
    );
    assert_eq!(issued, u64::from(MAX_SUBSCRIPTIONS_PER_CONNECTION) + 2);
    // The other connection's subscription is still dropped with the LocalSet.
    assert!(
        released >= issued - 1,
        "issued {issued}, released {released}"
    );
}

/// Every delivered event is counted under its topic with its lag recorded,
/// and a resync counts `resync`.
/// Disarm check: with the `count` call removed from the pump's success arm,
/// the delivered counter stays 0.
#[actix_web::test]
async fn delivered_events_are_counted_with_their_lag() {
    use pharos_server::services::events::{EVENTS_TOTAL, EVENT_LAG_SECONDS};
    let fx = seed_rich().await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    tokio::task::LocalSet::new()
        .run_until(async {
            let s = session_for(&connect(&fx), &fx.user_token).await;
            let (_sub, mut rx) = subscribe(&s, &[], 0).await.unwrap();
            played(&fx, 1, true, false);
            next(&mut rx).await;
            let (_stale, mut stale) = subscribe(&s, &[], 1).await.unwrap();
            next(&mut stale).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;
    assert_eq!(
        counter(
            &snapshotter,
            EVENTS_TOTAL,
            &[("topic", "user_data"), ("outcome", "delivered")]
        ),
        1
    );
    assert_eq!(
        counter(
            &snapshotter,
            EVENTS_TOTAL,
            &[("topic", "resync"), ("outcome", "resync")]
        ),
        1
    );
    let lags = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| k.key().name() == EVENT_LAG_SECONDS)
        .map(|(.., v)| match v {
            DebugValue::Histogram(h) => h.len(),
            _ => 0,
        })
        .sum::<usize>();
    assert_eq!(lags, 2);
}

/// After `Session.logout` on the connection, the subscription stops: the
/// user's later changes are not pushed to a signed-out client.
/// Disarm check: with the `session.ended()` check removed from the pump,
/// the change after logout arrives.
#[actix_web::test]
async fn a_logged_out_session_stops_receiving_events() {
    let fx = seed_rich().await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let s = session_for(&client, &fx.user_token).await;
            let (_sub, mut rx) = subscribe(&s, &[], 0).await.unwrap();
            played(&fx, 1, true, false);
            next(&mut rx).await;
            let mut req = s.logout_request();
            req.get().init_ctx();
            req.send().promise.await.unwrap();
            played(&fx, 2, true, false);
            quiet(&mut rx).await;
            assert_eq!(
                subscribe(&s, &[], 0).await.err(),
                Some("revoked".to_owned())
            );
        })
        .await;
}

/// The bus announcements `/socket` alone uses are not events.
#[actix_web::test]
async fn socket_only_announcements_are_not_events() {
    let fx = seed_rich().await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let s = session_for(&connect(&fx), &fx.user_token).await;
            let (_sub, mut rx) = subscribe(&s, &[], 0).await.unwrap();
            let _ = fx.state.bus.send(SocketBroadcast::SessionsChanged);
            let _ = fx.state.bus.send(SocketBroadcast::CastChanged);
            quiet(&mut rx).await;
        })
        .await;
}
