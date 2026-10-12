//! Connection-pool saturation metrics, shared by the sqlite and postgres pools.
//!
//! Published every [`SAMPLE_INTERVAL`] by a background task per pool:
//! - `pharos_db_pool_size{backend}` / `_idle` / `_in_use` / `_max` gauges
//! - `pharos_db_pool_acquire_seconds{backend}` histogram
//! - `pharos_db_pool_acquire_errors_total{backend}` counter
//!
//! sqlx has no hook around its internal `acquire`, so the wait histogram comes
//! from a probe (bounded by [`PROBE_TIMEOUT`]): each sample acquires (and immediately releases) one
//! connection and times it. On a saturated pool the probe queues like any
//! request, so its wait is what a new request would see right now; on an idle
//! pool it reads ~0. It is a sample, not a per-request measurement.

use std::time::{Duration, Instant};

use sqlx::{Database, Pool};

/// Fine enough to catch a pool saturating for a few seconds, coarse enough
/// that the probe is negligible load.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Set the saturation gauges for one pool. `in_use` is derived so the three
/// series always add up.
fn record_pool_state(backend: &'static str, size: u32, idle: usize, max: u32) {
    let idle = u32::try_from(idle).unwrap_or(u32::MAX).min(size);
    metrics::gauge!("pharos_db_pool_size", "backend" => backend).set(f64::from(size));
    metrics::gauge!("pharos_db_pool_idle", "backend" => backend).set(f64::from(idle));
    metrics::gauge!("pharos_db_pool_in_use", "backend" => backend)
        .set(f64::from(size.saturating_sub(idle)));
    metrics::gauge!("pharos_db_pool_max", "backend" => backend).set(f64::from(max));
}

/// How long a probe waits for a connection before it is recorded as a miss.
/// sqlx's own acquire timeout is 30 s, so without this the sampler would sit
/// blocked, and the gauges frozen, for exactly as long as the pool is
/// saturated. The probe also keeps at least one connection from being reaped
/// by the idle timer and, on Postgres, makes sqlx ping the connection it
/// hands out (`test_before_acquire`), so a dead socket shows up here.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// One sample: gauges first (they never wait on the pool), then the timed
/// acquire probe, bounded by `probe_timeout`. A timeout records the timeout
/// itself as the wait and counts as an error.
pub(crate) async fn sample_pool<DB: Database>(
    backend: &'static str,
    pool: &Pool<DB>,
    probe_timeout: Duration,
) {
    record_pool_state(
        backend,
        pool.size(),
        pool.num_idle(),
        pool.options().get_max_connections(),
    );
    let started = Instant::now();
    let outcome = tokio::time::timeout(probe_timeout, pool.acquire()).await;
    let waited = started.elapsed().as_secs_f64();
    metrics::histogram!("pharos_db_pool_acquire_seconds", "backend" => backend).record(waited);
    match outcome {
        Ok(Ok(conn)) => drop(conn),
        Ok(Err(error)) => {
            metrics::counter!("pharos_db_pool_acquire_errors_total", "backend" => backend)
                .increment(1);
            tracing::warn!(backend, %error, waited, "pool acquire probe failed");
        }
        Err(_) => {
            metrics::counter!("pharos_db_pool_acquire_errors_total", "backend" => backend)
                .increment(1);
            tracing::warn!(
                backend,
                waited,
                "pool acquire probe timed out; pool saturated"
            );
        }
    }
}

/// Sample `pool` forever; the task ends when the pool is closed.
pub(crate) fn spawn_pool_sampler<DB: Database>(backend: &'static str, pool: Pool<DB>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SAMPLE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if pool.is_closed() {
                break;
            }
            sample_pool(backend, &pool, PROBE_TIMEOUT).await;
        }
    });
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use sqlx::sqlite::SqlitePoolOptions;

    type Rows = Vec<(String, String, DebugValue)>;

    fn gauge(rows: &Rows, name: &str, backend: &str) -> Option<f64> {
        rows.iter().find_map(|(n, b, v)| match v {
            DebugValue::Gauge(g) if n == name && b == backend => Some(g.into_inner()),
            _ => None,
        })
    }

    /// Opening a store starts the sampler, so a deployment gets the series
    /// without any caller remembering to ask.
    #[test]
    fn connecting_a_sqlite_store_starts_the_sampler() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let _store =
                    crate::sqlite::SqliteStore::connect_without_backfill("sqlite::memory:")
                        .await
                        .unwrap();
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
        });
        let seen = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .any(|(ck, ..)| ck.key().name() == "pharos_db_pool_max");
        assert!(seen, "no pool gauges after connect");
    }

    /// On a saturated pool the probe must not hang for sqlx's 30 s acquire
    /// timeout: the gauges are already published, the wait is recorded at the
    /// probe timeout, and the miss is counted.
    #[test]
    fn a_saturated_pool_records_the_probe_timeout_instead_of_hanging() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = Instant::now();
        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let pool = SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect("sqlite::memory:")
                    .await
                    .unwrap();
                let _held = pool.acquire().await.unwrap();
                sample_pool("sqlite", &pool, Duration::from_millis(100)).await;
            });
        });
        assert!(started.elapsed() < Duration::from_secs(5), "probe hung");
        let rows = snapshotter.snapshot().into_vec();
        let waits: Vec<f64> = rows
            .iter()
            .find_map(|(ck, _, _, v)| match v {
                DebugValue::Histogram(h) if ck.key().name() == "pharos_db_pool_acquire_seconds" => {
                    Some(h.iter().map(|x| x.into_inner()).collect())
                }
                _ => None,
            })
            .unwrap_or_default();
        assert_eq!(waits.len(), 1, "the timed-out probe must be observed");
        assert!(waits.iter().all(|w| *w >= 0.09), "recorded {waits:?}");
        let errors = rows.iter().find_map(|(ck, _, _, v)| match v {
            DebugValue::Counter(n) if ck.key().name() == "pharos_db_pool_acquire_errors_total" => {
                Some(*n)
            }
            _ => None,
        });
        assert_eq!(errors, Some(1));
        let in_use = rows.iter().find_map(|(ck, _, _, v)| match v {
            DebugValue::Gauge(g) if ck.key().name() == "pharos_db_pool_in_use" => {
                Some(g.into_inner())
            }
            _ => None,
        });
        assert_eq!(in_use, Some(1.0));
    }

    #[test]
    fn a_held_connection_shows_as_in_use_and_the_probe_is_timed() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let pool = SqlitePoolOptions::new()
                    .max_connections(3)
                    .connect("sqlite::memory:")
                    .await
                    .unwrap();
                let held = pool.acquire().await.unwrap();
                let released = pool.acquire().await.unwrap();
                drop(released);
                // sqlx returns a dropped connection to the pool from a task.
                tokio::time::sleep(Duration::from_millis(100)).await;
                sample_pool("sqlite", &pool, PROBE_TIMEOUT).await;
                drop(held);
            });
        });
        let rows: Rows = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(ck, _, _, v)| {
                let backend = ck
                    .key()
                    .labels()
                    .find(|l| l.key() == "backend")
                    .map(|l| l.value().to_string())
                    .unwrap_or_default();
                (ck.key().name().to_string(), backend, v)
            })
            .collect();
        assert_eq!(gauge(&rows, "pharos_db_pool_max", "sqlite"), Some(3.0));
        assert_eq!(gauge(&rows, "pharos_db_pool_size", "sqlite"), Some(2.0));
        assert_eq!(gauge(&rows, "pharos_db_pool_idle", "sqlite"), Some(1.0));
        assert_eq!(gauge(&rows, "pharos_db_pool_in_use", "sqlite"), Some(1.0));
        let waits = rows.iter().find_map(|(n, _, v)| match v {
            DebugValue::Histogram(h) if n == "pharos_db_pool_acquire_seconds" => Some(h.len()),
            _ => None,
        });
        assert_eq!(waits, Some(1), "one probe, one observation");
    }
}
