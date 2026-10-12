//! Per-store-method latency: `pharos_store_query_seconds{method,outcome}`.
//!
//! Wraps the hot calls at the single `AnyStore` dispatch point, so each call is
//! timed once whichever backend serves it. `method` is a fixed `&'static str`
//! chosen at the call site, never derived from input, which bounds the label
//! to the number of wrapped methods.

use std::future::Future;
use std::time::Instant;

/// Await `fut` and record its wall time under `method`. Includes any wait for
/// a pool connection, which is what a caller experiences.
///
/// Recorded from a drop guard, because actix drops the request future when the
/// client disconnects and the slowest queries are the likeliest to be
/// abandoned. `outcome` is `ok`, `err` or `cancelled` (dropped mid-flight).
pub(crate) async fn timed<T, E>(
    method: &'static str,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let mut guard = TimingGuard {
        method,
        started: Instant::now(),
        outcome: "cancelled",
    };
    let out = fut.await;
    guard.outcome = if out.is_ok() { "ok" } else { "err" };
    out
}

struct TimingGuard {
    method: &'static str,
    started: Instant,
    outcome: &'static str,
}

impl Drop for TimingGuard {
    fn drop(&mut self) {
        metrics::histogram!(
            "pharos_store_query_seconds",
            "method" => self.method,
            "outcome" => self.outcome,
        )
        .record(self.started.elapsed().as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use pharos_core::{MediaQuery, MediaStore, UserDataStore, UserId};

    fn observations(
        snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
        method: &str,
    ) -> usize {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| {
                ck.key().name() == "pharos_store_query_seconds"
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "method" && l.value() == method)
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Histogram(h) => h.len(),
                _ => 0,
            })
            .sum()
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn timed_returns_the_value_and_records_one_observation() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let got = metrics::with_local_recorder(&recorder, || {
            runtime().block_on(timed("probe", async { Ok::<_, ()>(7) }))
        });
        assert_eq!(got, Ok(7));
        assert_eq!(observations(&snapshotter, "probe"), 1);
    }

    fn with_outcome(
        snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
        method: &str,
        outcome: &str,
    ) -> usize {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| {
                ck.key().name() == "pharos_store_query_seconds"
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "method" && l.value() == method)
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "outcome" && l.value() == outcome)
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Histogram(h) => h.len(),
                _ => 0,
            })
            .sum()
    }

    /// actix drops the request future when the client disconnects. The slowest
    /// queries are the ones most likely to be abandoned, so they must still be
    /// recorded, under their own outcome.
    #[test]
    fn a_dropped_call_is_recorded_as_cancelled_and_results_as_ok_or_err() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            runtime().block_on(async {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(10),
                    timed("slow", std::future::pending::<Result<(), ()>>()),
                )
                .await;
                let _ = timed("fine", async { Ok::<_, ()>(1) }).await;
                let _ = timed("bad", async { Err::<(), _>(()) }).await;
            });
        });
        assert_eq!(with_outcome(&snapshotter, "slow", "cancelled"), 1);
        assert_eq!(with_outcome(&snapshotter, "fine", "ok"), 1);
        assert_eq!(with_outcome(&snapshotter, "bad", "err"), 1);
    }

    /// The dispatch layer, not the caller, owns the timing: calling the
    /// store the way the server does must produce one observation per call.
    #[test]
    fn any_store_times_its_hot_calls_once_each() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            runtime().block_on(async {
                let store = crate::any::AnyStore::connect("sqlite::memory:")
                    .await
                    .unwrap();
                store.query(&MediaQuery::default()).await.unwrap();
                let _ = MediaStore::get(&store, 1).await;
                store.get_user_data(UserId::default(), 1).await.unwrap();
                store
                    .user_data_bulk(UserId::default(), &[1, 2])
                    .await
                    .unwrap();
                store.mark_seen_batch(&[], 1).await.unwrap();
                store.put_batch(&[]).await.unwrap();
            });
        });
        for method in [
            "query_items",
            "get_item",
            "get_user_data",
            "user_data_bulk",
            "mark_seen_batch",
            "put_batch",
        ] {
            assert_eq!(observations(&snapshotter, method), 1, "{method}");
        }
    }
}
