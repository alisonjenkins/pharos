#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Progress reports are written to the database at most once per interval per
//! play session, except where waiting would lose something: the stop, a pause
//! flip and a seek. The session registry is updated by every report either
//! way; only the database write is held back.
//!
//! Writes are counted from the `record_playback_progress` store span, so the
//! assertions are about database work, not about a helper's internals.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserDataStore, UserId,
    UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    progress_writes::{flush_all, flush_all_within, flush_due},
    state::{AppState, Stores},
};
use tracing_subscriber::prelude::*;

#[path = "common/store_calls.rs"]
mod store_calls;
use store_calls::StoreCallLog;

const SEC: u64 = 10_000_000;
/// 60 s of media, so the 90% played threshold sits at 54 s.
const RUNTIME_MS: u64 = 60_000;

async fn seed() -> (web::Data<AppState>, String, UserId) {
    seed_with(None).await
}

async fn seed_with(interval: Option<std::time::Duration>) -> (web::Data<AppState>, String, UserId) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 1,
            path: "/m/a.mkv".into(),
            title: "A".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(RUNTIME_MS),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let mut state = AppState::new(stores, "srv".into());
    if let Some(i) = interval {
        state = state.with_progress_persist_interval(i);
    }
    let state = web::Data::new(state);
    (state, token.0.expose().to_string(), uid)
}

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state.clone())
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await
    };
}

macro_rules! post {
    ($app:expr, $token:expr, $uri:expr, $body:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::post()
                .uri($uri)
                .insert_header(("X-Emby-Token", $token.as_str()))
                .set_json($body)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 204, "{}", $uri);
    }};
}

macro_rules! progress {
    ($app:expr, $token:expr, $secs:expr) => {
        progress!($app, $token, $secs, false)
    };
    ($app:expr, $token:expr, $secs:expr, $paused:expr) => {
        post!(
            $app,
            $token,
            "/Sessions/Playing/Progress",
            serde_json::json!({
                "ItemId": "1", "PlaySessionId": "s1",
                "PositionTicks": $secs * SEC, "IsPaused": $paused
            })
        )
    };
}

macro_rules! stopped {
    ($app:expr, $token:expr, $secs:expr) => {
        post!(
            $app,
            $token,
            "/Sessions/Playing/Stopped",
            serde_json::json!({
                "ItemId": "1", "PlaySessionId": "s1", "PositionTicks": $secs * SEC
            })
        )
    };
}

fn writes(log: &StoreCallLog) -> usize {
    log.take()
        .iter()
        .filter(|c| *c == "record_playback_progress")
        .count()
}

async fn stored(state: &web::Data<AppState>, uid: UserId) -> pharos_core::UserItemData {
    state.stores.get_user_data(uid, 1).await.unwrap()
}

/// Make the stored row look written long ago. A held report is applied only
/// when strictly newer than the row, and a test runs inside one wall-clock
/// second, so without this the real clock decides whether the flush lands.
async fn age_row(state: &web::Data<AppState>, uid: UserId) {
    let mut data = stored(state, uid).await;
    data.last_played_at = 1;
    state.stores.set_user_data(uid, 1, data).await.unwrap();
}

#[actix_web::test]
async fn two_quick_ticks_write_once_but_the_registry_sees_both() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    post!(
        app,
        token,
        "/Sessions/Playing",
        serde_json::json!({"ItemId": "1", "PlaySessionId": "s1", "PositionTicks": 0u64})
    );
    let log = StoreCallLog::default();
    let _spans = tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));

    progress!(app, token, 10u64);
    progress!(app, token, 11u64);

    assert_eq!(writes(&log), 1, "the second tick is held, not written");
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        10 * SEC
    );
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Sessions")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v[0]["PositionTicks"],
        11 * SEC,
        "now-playing follows every report"
    );
}

#[actix_web::test]
async fn stopping_stores_the_stop_position_not_the_last_written_tick() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    stopped!(app, token, 12u64);

    let ud = stored(&state, uid).await;
    assert_eq!(ud.last_played_position_ticks, 12 * SEC);
    assert!(!ud.played);
    assert!(
        state.progress_writes.take_all().is_empty(),
        "the stop leaves nothing held that could follow it"
    );
}

#[actix_web::test]
async fn pausing_writes_at_once() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        10 * SEC
    );

    progress!(app, token, 12u64, true);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        12 * SEC
    );
    progress!(app, token, 13u64, true);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        12 * SEC,
        "ticks while paused are held again"
    );
    progress!(app, token, 13u64, false);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        13 * SEC,
        "unpausing writes at once too"
    );
}

#[actix_web::test]
async fn a_seek_writes_at_once() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 10u64);
    progress!(app, token, 40u64);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        40 * SEC
    );
    progress!(app, token, 5u64);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        5 * SEC
    );
}

#[actix_web::test]
async fn the_watched_threshold_fires_once_however_many_ticks_preceded_the_stop() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    for secs in [50u64, 51, 52, 53, 55, 56] {
        progress!(app, token, secs);
    }
    stopped!(app, token, 57u64);

    let ud = stored(&state, uid).await;
    assert!(ud.played, "stopped past 90% of runtime marks played");
    assert_eq!(ud.play_count, 1, "played once, not once per tick");
    assert_eq!(
        ud.last_played_position_ticks, 0,
        "a finished item has no resume point"
    );
}

#[actix_web::test]
async fn a_zero_interval_writes_every_report() {
    let (state, token, uid) = seed_with(Some(std::time::Duration::ZERO)).await;
    let app = app!(state);
    let log = StoreCallLog::default();
    let _spans = tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));
    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    progress!(app, token, 12u64);
    assert_eq!(writes(&log), 3);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        12 * SEC
    );
}

fn counter(
    snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
    outcome: &str,
    reason: &str,
) -> u64 {
    use metrics_util::debugging::DebugValue;
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(key, _, _, value)| {
            let has = |k: &str, v: &str| key.key().labels().any(|l| l.key() == k && l.value() == v);
            match value {
                DebugValue::Counter(n)
                    if key.key().name() == "pharos_progress_writes_total"
                        && has("outcome", outcome)
                        && has("reason", reason) =>
                {
                    Some(n)
                }
                _ => None,
            }
        })
        .unwrap_or(0)
}

fn persist_latency_samples(snapshotter: &pharos_test_metrics::CumulativeSnapshotter) -> usize {
    use metrics_util::debugging::DebugValue;
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(key, _, _, value)| match value {
            DebugValue::Histogram(h) if key.key().name() == "pharos_progress_persist_seconds" => {
                Some(h.len())
            }
            _ => None,
        })
        .unwrap_or(0)
}

#[actix_web::test]
async fn writes_are_counted_by_outcome_and_reason_and_timed() {
    let (state, token, _uid) = seed().await;
    let app = app!(state);
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    progress!(app, token, 12u64);
    progress!(app, token, 13u64, true);
    assert_eq!(counter(&snapshotter, "persisted", "first"), 1);
    assert_eq!(counter(&snapshotter, "coalesced", "held"), 2);
    assert_eq!(counter(&snapshotter, "persisted", "pause_change"), 1);
    assert_eq!(
        persist_latency_samples(&snapshotter),
        2,
        "one sample per write"
    );

    // The reports above ran inside one wall-clock second, so a held write
    // may find the row no older than itself and be dropped: either outcome
    // counts as the tick or shutdown having handled the report.
    let handled = |reason: &str| {
        counter(&snapshotter, "persisted", reason) + counter(&snapshotter, "dropped_stale", reason)
    };
    progress!(app, token, 13u64, true);
    flush_due(&state.progress_writes, &state.stores, later(20)).await;
    assert_eq!(handled("tick"), 1);
    progress!(app, token, 13u64, true);
    flush_all(&state.progress_writes, &state.stores).await;
    assert_eq!(handled("shutdown"), 1);
    assert_eq!(counter(&snapshotter, "failed", "tick"), 0);
}

#[actix_web::test]
async fn a_held_write_the_store_drops_is_counted_as_dropped_not_persisted() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    // Something far newer reached the row first (a stop on another replica).
    let far_future = i64::MAX / 2;
    state
        .stores
        .set_user_data(
            uid,
            1,
            pharos_core::UserItemData {
                last_played_at: far_future,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        flush_due(&state.progress_writes, &state.stores, later(20)).await,
        1
    );
    assert_eq!(counter(&snapshotter, "dropped_stale", "tick"), 1);
    assert_eq!(counter(&snapshotter, "persisted", "tick"), 0);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        0,
        "the newer row was kept"
    );
}

fn later(secs: u64) -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(secs)
}

#[actix_web::test]
async fn a_silent_session_has_its_held_position_written_by_the_tick() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        10 * SEC
    );

    age_row(&state, uid).await;
    let none_yet = flush_due(&state.progress_writes, &state.stores, later(1)).await;
    assert_eq!(none_yet, 0, "a report younger than the interval stays held");
    let written = flush_due(&state.progress_writes, &state.stores, later(20)).await;
    assert_eq!(written, 1);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        11 * SEC
    );
    assert_eq!(
        flush_due(&state.progress_writes, &state.stores, later(40)).await,
        0,
        "written once, not on every tick"
    );
}

#[actix_web::test]
async fn shutdown_writes_every_held_report() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    progress!(app, token, 12u64);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        10 * SEC
    );

    age_row(&state, uid).await;
    assert_eq!(flush_all(&state.progress_writes, &state.stores).await, 1);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        12 * SEC
    );
    assert_eq!(flush_all(&state.progress_writes, &state.stores).await, 0);
}

#[actix_web::test]
async fn nothing_flushed_after_the_stop_undoes_it() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 50u64);
    progress!(app, token, 52u64);
    // A tick already took the held report and is mid-write when the stop lands.
    let in_flight = state.progress_writes.take_due(later(20));
    assert_eq!(in_flight.len(), 1);
    // No pause for a new second: the stop usually lands in the same whole
    // second as the report it follows, and must still win.
    stopped!(app, token, 57u64);

    for (key, progress) in in_flight {
        // What the flush writes: a held report, applied only if newer.
        let held = pharos_core::PlaybackProgress {
            only_if_newer: true,
            ..progress
        };
        state
            .stores
            .record_playback_progress(key.user, key.item, held)
            .await
            .unwrap();
    }
    assert_eq!(
        flush_due(&state.progress_writes, &state.stores, later(60)).await,
        0
    );
    assert_eq!(flush_all(&state.progress_writes, &state.stores).await, 0);

    let ud = stored(&state, uid).await;
    assert!(ud.played);
    assert_eq!(ud.play_count, 1);
    assert_eq!(
        ud.last_played_position_ticks, 0,
        "the finished item has no resume point"
    );
}

#[actix_web::test]
async fn a_stop_without_a_play_session_id_still_beats_the_older_held_report() {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 50u64);
    progress!(app, token, 52u64);
    // The stop names no PlaySessionId, so it cannot forget the held report
    // (keyed by "s1"); the store's guard has to refuse it instead.
    post!(
        app,
        token,
        "/Sessions/Playing/Stopped",
        serde_json::json!({ "ItemId": "1", "PositionTicks": 57u64 * SEC })
    );
    flush_due(&state.progress_writes, &state.stores, later(60)).await;
    flush_all(&state.progress_writes, &state.stores).await;

    let ud = stored(&state, uid).await;
    assert!(ud.played);
    assert_eq!(ud.last_played_position_ticks, 0, "no resume point");
}

/// A store whose progress writes can be made to fail or never finish.
struct Wrapped {
    inner: Stores,
    mode: std::sync::Mutex<Mode>,
}

#[derive(Clone, Copy)]
enum Mode {
    Pass,
    Fail,
    Stall,
}

impl Wrapped {
    fn new(inner: &Stores) -> Self {
        Self {
            inner: inner.clone(),
            mode: std::sync::Mutex::new(Mode::Pass),
        }
    }
    fn set(&self, m: Mode) {
        *self.mode.lock().unwrap() = m;
    }
}

impl UserDataStore for Wrapped {
    async fn get_user_data(
        &self,
        user: UserId,
        item: pharos_core::MediaId,
    ) -> pharos_core::DomainResult<pharos_core::UserItemData> {
        self.inner.get_user_data(user, item).await
    }
    async fn record_playback_progress(
        &self,
        user: UserId,
        item: pharos_core::MediaId,
        progress: pharos_core::PlaybackProgress,
    ) -> pharos_core::DomainResult<pharos_core::UserItemData> {
        let mode = *self.mode.lock().unwrap();
        match mode {
            Mode::Pass => {
                self.inner
                    .record_playback_progress(user, item, progress)
                    .await
            }
            Mode::Fail => Err(pharos_core::DomainError::Backend("down".into())),
            Mode::Stall => std::future::pending().await,
        }
    }
    async fn set_user_data(
        &self,
        user: UserId,
        item: pharos_core::MediaId,
        data: pharos_core::UserItemData,
    ) -> pharos_core::DomainResult<()> {
        self.inner.set_user_data(user, item, data).await
    }
    async fn user_data_bulk(
        &self,
        user: UserId,
        items: &[pharos_core::MediaId],
    ) -> pharos_core::DomainResult<Vec<pharos_core::UserItemData>> {
        self.inner.user_data_bulk(user, items).await
    }
    async fn resumable_items(
        &self,
        user: UserId,
    ) -> pharos_core::DomainResult<Vec<pharos_core::MediaId>> {
        self.inner.resumable_items(user).await
    }
}

/// One held report for item 1, then the stored row aged so the flush lands.
async fn one_held_report() -> (web::Data<AppState>, Wrapped, UserId) {
    let (state, token, uid) = seed().await;
    let app = app!(state);
    progress!(app, token, 10u64);
    progress!(app, token, 11u64);
    age_row(&state, uid).await;
    let store = Wrapped::new(&state.stores);
    (state, store, uid)
}

#[actix_web::test]
async fn a_failed_write_keeps_the_held_report_for_the_next_flush() {
    let (state, store, uid) = one_held_report().await;
    store.set(Mode::Fail);
    assert_eq!(
        flush_due(&state.progress_writes, &store, later(20)).await,
        0
    );
    store.set(Mode::Pass);
    assert_eq!(
        flush_due(&state.progress_writes, &store, later(40)).await,
        1
    );
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        11 * SEC
    );
}

#[actix_web::test]
async fn a_flush_cancelled_mid_write_puts_the_taken_reports_back() {
    let (state, store, uid) = one_held_report().await;
    store.set(Mode::Stall);
    // The tick has taken the report out of the hold and is stuck writing it
    // when shutdown cancels it.
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        flush_due(&state.progress_writes, &store, later(20)),
    )
    .await;
    assert!(cancelled.is_err(), "the write never finished");

    store.set(Mode::Pass);
    assert_eq!(flush_all(&state.progress_writes, &store).await, 1);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        11 * SEC,
        "the taken report was still there for the shutdown flush"
    );
}

#[actix_web::test]
async fn the_shutdown_flush_gives_up_at_its_deadline_and_counts_what_it_abandoned() {
    let (state, store, uid) = one_held_report().await;
    store.set(Mode::Stall);
    let out = flush_all_within(
        &state.progress_writes,
        &store,
        std::time::Duration::from_millis(100),
    )
    .await;
    assert_eq!(out.written, 0);
    assert_eq!(out.abandoned, 1);
    assert_eq!(
        stored(&state, uid).await.last_played_position_ticks,
        10 * SEC,
        "nothing was written"
    );
}
