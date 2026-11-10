//! Logs, traces (ADR-0105) and Prometheus metrics for the service.

use std::sync::OnceLock;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Songs by how they ended: `ok`, `ffmpeg` (422) or `internal` (500).
pub const SONGS_TOTAL: &str = "pharos_aligner_songs_total";
/// Wall time of one song, the wait for the lock included.
pub const SONG_SECONDS: &str = "pharos_aligner_song_seconds";
/// A song takes seconds to minutes on the CPU.
const SONG_BUCKETS: &[f64] = &[1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0];

/// Keeps the tracer provider alive, so its batch processor keeps exporting.
static PROVIDER: OnceLock<opentelemetry_sdk::trace::SdkTracerProvider> = OnceLock::new();

/// Installs logs and, with `otlp_endpoint`, span export over OTLP/gRPC; true
/// when spans are exported. Logs are plain `key=value` lines, as the Python
/// service wrote, so the existing LogQL line filters keep matching; `RUST_LOG`
/// overrides `info`. A bad endpoint costs the traces, not the service. Call
/// inside the actix runtime: the gRPC exporter's connection runs on it.
pub fn init(otlp_endpoint: Option<&str>) -> bool {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let (otel, failed) = match otlp_endpoint.map(otel_layer) {
        Some(Ok(layer)) => (Some(layer), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    let traced = otel.is_some();
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false),
        )
        .with(otel)
        .try_init();
    if let (Some(endpoint), Some(e)) = (otlp_endpoint, failed) {
        tracing::warn!(endpoint, error = %e, "traces off: exporter setup failed");
    }
    traced
}

fn otel_layer<S>(
    endpoint: &str,
) -> Result<tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>, String>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig as _;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| e.to_string())?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_attribute(opentelemetry::KeyValue::new(
            "service.name",
            "pharos-aligner",
        ))
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();
    let tracer = provider.tracer("pharos_aligner");
    let _ = PROVIDER.set(provider);
    Ok(tracing_opentelemetry::layer().with_tracer(tracer))
}

/// Flushes spans still queued for export; call once at exit.
pub fn shutdown() {
    if let Some(provider) = PROVIDER.get() {
        if let Err(e) = provider.shutdown() {
            eprintln!("trace export shutdown: {e}");
        }
    }
}

struct HeaderExtractor<'a>(&'a actix_web::http::header::HeaderMap);

impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .map(actix_web::http::header::HeaderName::as_str)
            .collect()
    }
}

/// Parents `span` on the caller's span named by a W3C `traceparent`, so the
/// song joins the server's trace; a missing or malformed header leaves it a root.
pub fn set_remote_parent(span: &tracing::Span, headers: &actix_web::http::header::HeaderMap) {
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let parent = opentelemetry_sdk::propagation::TraceContextPropagator::new()
        .extract(&HeaderExtractor(headers));
    if parent.span().span_context().is_valid() {
        let _ = span.set_parent(parent);
    }
}

/// The recorder behind `/metrics`, plus the /proc collector it refreshes per scrape.
pub struct Metrics {
    handle: PrometheusHandle,
    process: metrics_process::Collector,
}

impl Metrics {
    pub fn install() -> Result<Self, String> {
        let handle = PrometheusBuilder::new()
            .set_buckets_for_metric(Matcher::Full(SONG_SECONDS.into()), SONG_BUCKETS)
            .and_then(PrometheusBuilder::install_recorder)
            .map_err(|e| format!("prometheus recorder: {e}"))?;
        let process = metrics_process::Collector::default();
        process.describe();
        metrics::describe_counter!(SONGS_TOTAL, "Songs aligned, by outcome");
        metrics::describe_histogram!(
            SONG_SECONDS,
            metrics::Unit::Seconds,
            "Wall time of one song"
        );
        Ok(Self { handle, process })
    }

    pub fn render(&self) -> String {
        self.process.collect();
        self.handle.render()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::set_remote_parent;

    #[test]
    fn a_traceparent_joins_the_callers_trace() {
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let mut headers = actix_web::http::header::HeaderMap::new();
            headers.insert(
                "traceparent".parse().unwrap(),
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                    .parse()
                    .unwrap(),
            );
            let span = tracing::info_span!("aligner.align");
            set_remote_parent(&span, &headers);
            let trace_id = span.context().span().span_context().trace_id();
            assert_eq!(trace_id.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");

            headers.insert("traceparent".parse().unwrap(), "garbage".parse().unwrap());
            let root = tracing::info_span!("aligner.align");
            set_remote_parent(&root, &headers);
            let own = root.context().span().span_context().trace_id();
            assert_ne!(own.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
        });
    }
}
