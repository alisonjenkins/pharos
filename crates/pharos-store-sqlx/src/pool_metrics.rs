//! Connection-pool saturation metrics, shared by the sqlite and postgres pools.
//!
//! Published every [`SAMPLE_INTERVAL`] by a background task per pool:
//! - `pharos_db_pool_size{backend}` / `_idle` / `_in_use` / `_max` gauges
//! - `pharos_db_pool_acquire_seconds{backend}` histogram
//! - `pharos_db_pool_acquire_errors_total{backend}` counter
//!
//! sqlx has no hook around its internal `acquire`, so the wait histogram comes
//! from a probe: each sample acquires (and immediately releases) one
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

/// One sample: gauges, then the timed acquire probe.
pub(crate) async fn sample_pool<DB: Database>(backend: &'static str, pool: &Pool<DB>) {
    record_pool_state(
        backend,
        pool.size(),
        pool.num_idle(),
        pool.options().get_max_connections(),
    );
    let started = Instant::now();
    match pool.acquire().await {
        Ok(conn) => {
            drop(conn);
            metrics::histogram!("pharos_db_pool_acquire_seconds", "backend" => backend)
                .record(started.elapsed().as_secs_f64());
        }
        Err(error) => {
            metrics::counter!("pharos_db_pool_acquire_errors_total", "backend" => backend)
                .increment(1);
            tracing::warn!(backend, %error, "pool acquire probe failed");
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
            sample_pool(backend, &pool).await;
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
                sample_pool("sqlite", &pool).await;
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
