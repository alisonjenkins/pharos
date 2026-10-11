//! Per-store-method latency: `pharos_store_query_seconds{method}`.
//!
//! Wraps the hot calls at the single `AnyStore` dispatch point, so each call is
//! timed once whichever backend serves it. `method` is a fixed `&'static str`
//! chosen at the call site, never derived from input, which bounds the label
//! to the number of wrapped methods.

use std::future::Future;
use std::time::Instant;

/// Await `fut` and record its wall time under `method`. Includes any wait for
/// a pool connection, which is what a caller experiences.
pub(crate) async fn timed<T>(method: &'static str, fut: impl Future<Output = T>) -> T {
    let started = Instant::now();
    let out = fut.await;
    metrics::histogram!("pharos_store_query_seconds", "method" => method)
        .record(started.elapsed().as_secs_f64());
    out
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
            runtime().block_on(timed("probe", async { 7 }))
        });
        assert_eq!(got, 7);
        assert_eq!(observations(&snapshotter, "probe"), 1);
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
            });
        });
        for method in [
            "query_items",
            "get_item",
            "get_user_data",
            "user_data_bulk",
            "mark_seen_batch",
        ] {
            assert_eq!(observations(&snapshotter, method), 1, "{method}");
        }
    }
}
