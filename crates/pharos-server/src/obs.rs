//! Observability bootstrap. One-shot init (permitted lock per V18). Read path
//! is lock-free via `OnceLock<PrometheusHandle>` — render() does not contend.

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::{Once, OnceLock};
use tracing_subscriber::{prelude::*, EnvFilter};

/// Root-span builder that records the HTTP response status on every request
/// span AND emits a visible event for error statuses. `DefaultRootSpanBuilder`
/// records `http.status_code` on the span, but that attribute is only set at
/// request-end and never surfaces as a log line, so a 4xx/5xx (e.g. a client
/// hitting a route pharos doesn't implement) is invisible in the logs. This
/// makes failures greppable: `WARN ... http request completed with error
/// status ... status=404 ... http.target=/videos/activeencodings`.
pub struct StatusRootSpanBuilder;

impl tracing_actix_web::RootSpanBuilder for StatusRootSpanBuilder {
    fn on_request_start(request: &actix_web::dev::ServiceRequest) -> tracing::Span {
        tracing_actix_web::root_span!(request)
    }

    fn on_request_end<B: actix_web::body::MessageBody>(
        span: tracing::Span,
        outcome: &Result<actix_web::dev::ServiceResponse<B>, actix_web::Error>,
    ) {
        // A cheap handle clone so we can log after the default builder records
        // the standard otel fields (incl. http.status_code) onto the span.
        let logged = span.clone();
        tracing_actix_web::DefaultRootSpanBuilder::on_request_end(span, outcome);
        if let Ok(resp) = outcome {
            let status = resp.status().as_u16();
            if status >= 400 {
                let _e = logged.enter();
                tracing::warn!(status, "http request completed with error status");
            }
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ObsError {
    #[error("prometheus: {0}")]
    Prom(String),
    #[error("filter: {0}")]
    Filter(String),
    #[error("otlp: {0}")]
    Otlp(String),
}

static INIT: Once = Once::new();
static RESULT: OnceLock<Result<PrometheusHandle, ObsError>> = OnceLock::new();
/// Keeps the OTLP tracer provider alive for the process lifetime so the batch
/// span processor keeps flushing (dropping it would stop export).
static TRACER_PROVIDER: OnceLock<opentelemetry_sdk::trace::TracerProvider> = OnceLock::new();

/// Initialize tracing subscriber + Prometheus recorder. Idempotent.
/// Subsequent calls return the cached result, no recorder reinstall.
///
/// When `otlp_endpoint` is `Some`, spans are also exported over OTLP/gRPC to
/// that collector (e.g. Tempo) via a batch processor. Must be called from
/// within the tokio runtime (the gRPC exporter needs it).
pub fn init(log_level: &str, otlp_endpoint: Option<&str>) -> Result<PrometheusHandle, ObsError> {
    INIT.call_once(|| {
        let outcome = (|| -> Result<PrometheusHandle, ObsError> {
            let filter =
                EnvFilter::try_new(log_level).map_err(|e| ObsError::Filter(e.to_string()))?;
            let fmt_layer = tracing_subscriber::fmt::layer().json();
            // Optional OTLP span-export layer. `Option<Layer>` is itself a
            // Layer (None = no-op), so the registry composes uniformly.
            let otel_layer = match otlp_endpoint {
                Some(ep) => Some(build_otel_layer(ep)?),
                None => None,
            };
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .with(otel_layer)
                .try_init();
            install_panic_hook();
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

/// Route panics — including background `tokio::spawn` task panics that the
/// runtime otherwise SILENTLY swallows — into a structured `ERROR` tracing
/// event, so they surface in Loki + the OTLP traces instead of only stderr. The
/// previous default hook is chained (keeps the stderr message + any backtrace).
///
/// This is how a panicking background actor (e.g. the SyncPlay group actor)
/// becomes visible rather than the task just vanishing.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".into());
        let thread = std::thread::current();
        tracing::error!(
            panic.message = %message,
            panic.location = %location,
            panic.thread = thread.name().unwrap_or("unnamed"),
            "a thread/task panicked"
        );
        default_hook(info);
    }));
}

/// Build the OTLP/gRPC span-export layer pointed at `endpoint`, registering
/// the provider globally + in `TRACER_PROVIDER` so it outlives `init`. Generic
/// over the subscriber `S` so it composes onto the layered registry.
fn build_otel_layer<S>(
    endpoint: &str,
) -> Result<tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>, ObsError>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig as _;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| ObsError::Otlp(e.to_string()))?;
    let resource = opentelemetry_sdk::Resource::new(vec![opentelemetry::KeyValue::new(
        "service.name",
        "pharos",
    )]);
    let provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource)
        .build();
    let tracer = provider.tracer("pharos");
    opentelemetry::global::set_tracer_provider(provider.clone());
    let _ = TRACER_PROVIDER.set(provider);
    Ok(tracing_opentelemetry::layer().with_tracer(tracer))
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
        let h1 = init("info", None).unwrap();
        let h2 = init("info", None).unwrap();
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
