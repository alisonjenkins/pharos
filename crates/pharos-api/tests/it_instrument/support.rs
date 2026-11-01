//! Captures what a call records: JSON log lines (span closes included) and
//! metrics, both installed for the current thread only.

use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use metrics_util::CompositeKey;
use pharos_api::pharos_capnp::call_context;
use serde_json::Value;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::MakeWriter;

pub const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
pub const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
pub const DEVICE: &str = "device-7";

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buffer {
    type Writer = Buffer;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Everything recorded while [`observe`] ran.
pub struct Observed {
    pub logs: String,
    metrics: Vec<(CompositeKey, DebugValue)>,
}

impl Observed {
    fn find(&self, name: &str, labels: &[(&str, &str)]) -> Option<&DebugValue> {
        self.metrics.iter().find_map(|(key, value)| {
            let k = key.key();
            let matches = k.name() == name
                && labels.len() == k.labels().count()
                && labels
                    .iter()
                    .all(|(lk, lv)| k.labels().any(|l| l.key() == *lk && l.value() == *lv));
            matches.then_some(value)
        })
    }

    pub fn counter(&self, name: &str, labels: &[(&str, &str)]) -> Option<u64> {
        match self.find(name, labels)? {
            DebugValue::Counter(n) => Some(*n),
            _ => None,
        }
    }

    pub fn histogram_samples(&self, name: &str, labels: &[(&str, &str)]) -> usize {
        match self.find(name, labels) {
            Some(DebugValue::Histogram(h)) => h.len(),
            _ => 0,
        }
    }

    /// Every series of `name`, as `(labels, value)`.
    pub fn counters_named(&self, name: &str) -> Vec<(Vec<(String, String)>, u64)> {
        self.metrics
            .iter()
            .filter_map(|(key, value)| match value {
                DebugValue::Counter(n) if key.key().name() == name => Some((
                    key.key()
                        .labels()
                        .map(|l| (l.key().to_owned(), l.value().to_owned()))
                        .collect(),
                    *n,
                )),
                _ => None,
            })
            .collect()
    }

    pub fn lines(&self) -> Vec<Value> {
        self.logs
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The first log event whose message is `message`.
    pub fn event(&self, message: &str) -> Option<Value> {
        self.lines()
            .into_iter()
            .find(|l| l["fields"]["message"] == message)
    }

    /// The close event of the span named `name`.
    pub fn span_close(&self, name: &str) -> Option<Value> {
        self.lines()
            .into_iter()
            .find(|l| l["fields"]["message"] == "close" && l["span"]["name"] == name)
    }
}

/// Runs `f` with a JSON log subscriber filtered by `filter` and a metrics
/// recorder, both scoped to this thread.
pub fn observe<R>(filter: &str, f: impl FnOnce() -> R) -> (R, Observed) {
    let buffer = Buffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_span_events(FmtSpan::CLOSE)
        .with_current_span(true)
        .with_span_list(false)
        .with_writer(buffer.clone())
        .finish();
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let out = tracing::subscriber::with_default(subscriber, || {
        metrics::with_local_recorder(&recorder, f)
    });
    let logs = String::from_utf8_lossy(&buffer.0.lock().unwrap_or_else(PoisonError::into_inner))
        .into_owned();
    let metrics = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .map(|(k, _, _, v)| (k, v))
        .collect();
    (out, Observed { logs, metrics })
}

/// capnp-rpc capabilities are `!Send`: drive them on one thread.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(fut)
}

pub fn fill_ctx(mut ctx: call_context::Builder<'_>) {
    ctx.set_traceparent(TRACEPARENT);
    ctx.set_ui_route("/home");
    ctx.set_ui_action("open");
    ctx.set_client_version("test/1");
    ctx.init_device().set_value(DEVICE);
}
