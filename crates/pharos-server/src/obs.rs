//! Observability bootstrap. One-shot init (permitted lock per V18). Read path
//! is lock-free via `OnceLock<PrometheusHandle>` — render() does not contend.

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::{Once, OnceLock};
use tracing_subscriber::{prelude::*, EnvFilter};

#[derive(Debug, Clone, thiserror::Error)]
pub enum ObsError {
    #[error("prometheus: {0}")]
    Prom(String),
    #[error("filter: {0}")]
    Filter(String),
}

static INIT: Once = Once::new();
static RESULT: OnceLock<Result<PrometheusHandle, ObsError>> = OnceLock::new();

/// Initialize tracing subscriber + Prometheus recorder. Idempotent.
/// Subsequent calls return the cached result, no recorder reinstall.
pub fn init(log_level: &str) -> Result<PrometheusHandle, ObsError> {
    INIT.call_once(|| {
        let outcome = (|| -> Result<PrometheusHandle, ObsError> {
            let filter =
                EnvFilter::try_new(log_level).map_err(|e| ObsError::Filter(e.to_string()))?;
            let fmt_layer = tracing_subscriber::fmt::layer().json();
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .try_init();
            PrometheusBuilder::new()
                .install_recorder()
                .map_err(|e| ObsError::Prom(e.to_string()))
        })();
        let _ = RESULT.set(outcome);
    });
    match RESULT.get() {
        Some(Ok(h)) => Ok(h.clone()),
        Some(Err(e)) => Err(e.clone()),
        None => Err(ObsError::Prom("init race: result not set".into())),
    }
}

/// Render Prometheus exposition text for `/metrics`. Lock-free.
pub fn render() -> String {
    match RESULT.get() {
        Some(Ok(h)) => h.render(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn init_returns_ok_and_is_idempotent() {
        let h1 = init("info").unwrap();
        let h2 = init("info").unwrap();
        // Both renders return a string (may be empty until metrics emitted).
        let _ = h1.render();
        let _ = h2.render();
    }

    #[test]
    fn render_returns_string_even_pre_init() {
        // pre-init may return "" depending on test order; just ensure no panic.
        let _ = render();
    }
}
