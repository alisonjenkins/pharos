//! Observability bootstrap. One-shot init (permitted lock per V18). Read path
//! is lock-free via `OnceLock<PrometheusHandle>` — render() does not contend.

use actix_web::HttpMessage;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use std::sync::{Once, OnceLock};
use std::time::Instant;
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
        request
            .extensions_mut()
            .insert(RequestStart(Instant::now()));
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
            let elapsed_ms = request_elapsed_ms(resp.request());
            let _e = logged.enter();
            if status >= 400 {
                tracing::warn!(
                    status,
                    elapsed_ms,
                    "http request completed with error status"
                );
            } else if log_all_requests() {
                // B68 — a strict client (jellyfin-sdk-kotlin: Android/Google TV)
                // can crash PARSING a 200 response (a missing kotlin-required
                // field), leaving zero error logs and no crash report on the
                // server. Since 2xx are normally silent, opt-in full request
                // logging surfaces the exact endpoint sequence so the crashing
                // 200 can be identified. Enable via PHAROS_LOG_ALL_REQUESTS=1.
                let req = resp.request();
                let ua = req
                    .headers()
                    .get(actix_web::http::header::USER_AGENT)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                tracing::info!(
                    status,
                    elapsed_ms,
                    method = %req.method(),
                    path = %req.path(),
                    ua,
                    "http request"
                );
            }
        }
    }
}

/// Opt-in flag (env `PHAROS_LOG_ALL_REQUESTS`) to log EVERY request, not just
/// errors — read once, cached. Off by default (2xx stay silent). Used to debug
/// a strict client crashing on a 200 response body (B68).
/// When the request arrived, so the completion line can say how long it
/// took: the Prometheus histogram gives the route's distribution, but only
/// the log can tie one slow request to its item, session and client.
#[derive(Clone, Copy)]
struct RequestStart(Instant);

fn request_elapsed_ms(req: &actix_web::HttpRequest) -> Option<u64> {
    let start = req.extensions().get::<RequestStart>().copied()?;
    Some(u64::try_from(start.0.elapsed().as_millis()).unwrap_or(u64::MAX))
}

fn log_all_requests() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("PHAROS_LOG_ALL_REQUESTS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
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

/// Upper bounds for every `*_seconds` metric: 1 ms (a primary-key DB read) to
/// 30 s (a slow transcode segment). Without explicit buckets the exporter
/// emits a summary, whose quantiles cannot be aggregated across pods.
const SECONDS_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

static INIT: Once = Once::new();
static RESULT: OnceLock<Result<PrometheusHandle, ObsError>> = OnceLock::new();
/// Reads /proc (cpu, rss, fds, threads) into gauges; refreshed per scrape.
static PROCESS: OnceLock<metrics_process::Collector> = OnceLock::new();
/// Keeps the OTLP tracer provider alive for the process lifetime so the batch
/// span processor keeps flushing (dropping it would stop export).
static TRACER_PROVIDER: OnceLock<opentelemetry_sdk::trace::SdkTracerProvider> = OnceLock::new();

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
            // NO `idle_timeout`, and some signals DEPEND on that. A boot-time
            // gauge is set once and never touched again — the device-weighting
            // pair `pharos_transcode_device_rate_unmeasured` and
            // `pharos_transcode_device_capacity_suppressed` both describe a
            // STATE that persists for the life of the process and is only
            // recoverable by deleting a file. An idle timeout would expire them
            // silently, and the alert that reads them would go quiet without
            // the condition having cleared. Adding one means re-publishing
            // those on a timer first.
            let handle = PrometheusBuilder::new()
                .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), SECONDS_BUCKETS)
                .map_err(|e| ObsError::Prom(e.to_string()))?
                .install_recorder()
                .map_err(|e| ObsError::Prom(e.to_string()))?;
            let collector = metrics_process::Collector::default();
            collector.describe();
            let _ = PROCESS.set(collector);
            Ok(handle)
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
    let resource = opentelemetry_sdk::Resource::builder()
        .with_attribute(opentelemetry::KeyValue::new("service.name", "pharos"))
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();
    let tracer = provider.tracer("pharos");
    opentelemetry::global::set_tracer_provider(provider.clone());
    let _ = TRACER_PROVIDER.set(provider);
    Ok(tracing_opentelemetry::layer().with_tracer(tracer))
}

/// Create the counters whose first event must not be their first sample, so
/// `rate()` / `increase()` see them from startup. Call once after [`init`].
pub fn register_static_series() {
    pharos_cache::hls_cache::register_segment_metrics();
}

/// Render Prometheus exposition text for `/metrics`. Lock-free.
pub fn render() -> String {
    match RESULT.get() {
        Some(Ok(h)) => {
            if let Some(process) = PROCESS.get() {
                process.collect();
            }
            h.render()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The completion line reports the request's own age, or nothing when
    /// the middleware never stamped the request (a synthetic one in tests).
    #[actix_web::test]
    async fn elapsed_is_read_from_the_stamp_the_start_hook_left() {
        use actix_web::test::TestRequest;
        let req = TestRequest::default().to_http_request();
        assert_eq!(super::request_elapsed_ms(&req), None);
        req.extensions_mut().insert(super::RequestStart(
            super::Instant::now() - std::time::Duration::from_millis(1500),
        ));
        let ms = super::request_elapsed_ms(&req).expect("stamped request reports its age");
        assert!((1500..2500).contains(&ms), "elapsed_ms={ms}");
    }

    #[test]
    fn init_returns_ok_and_is_idempotent() {
        let h1 = init("info", None).unwrap();
        let h2 = init("info", None).unwrap();
        // Both renders return a string (may be empty until metrics emitted).
        let _ = h1.render();
        let _ = h2.render();
    }

    /// Timing metrics must export as real histograms so quantiles aggregate
    /// across pods; a summary cannot be merged.
    #[test]
    fn seconds_metrics_render_as_histograms_not_summaries() {
        init("info", None).unwrap();
        metrics::histogram!("pharos_obs_probe_seconds", "k" => "v").record(0.02);
        let body = render();
        assert!(
            body.contains("# TYPE pharos_obs_probe_seconds histogram"),
            "not a histogram; rendered:\n{body}"
        );
        assert!(body.contains("pharos_obs_probe_seconds_bucket{k=\"v\",le=\"0.005\"}"));
        assert!(body.contains("le=\"30\""));
        assert!(
            body.contains("le=\"0.001\""),
            "sub-5ms store calls need a 1 ms bucket"
        );
        assert!(!body.contains("pharos_obs_probe_seconds{k=\"v\",quantile"));
    }

    /// Production showed only `outcome="ok"`; the short/filler/failed series
    /// must be on `/metrics` at zero from the first scrape.
    #[test]
    fn static_series_are_rendered_at_zero_from_startup() {
        init("info", None).unwrap();
        register_static_series();
        let body = render();
        for outcome in ["short", "filler", "failed", "cancelled"] {
            assert!(
                body.lines()
                    .any(|l| l.starts_with("pharos_segment_produced_total{")
                        && l.contains(&format!("outcome=\"{outcome}\""))
                        && l.ends_with(" 0")),
                "no zero series for {outcome}; rendered:\n{body}"
            );
        }
    }

    /// CPU, memory, fds and threads of the pharos process itself, refreshed on
    /// every scrape, so a leak or a CPU spike shows up without node-exporter.
    #[cfg(target_os = "linux")]
    #[test]
    fn process_metrics_are_rendered_on_every_scrape() {
        init("info", None).unwrap();
        let body = render();
        for name in [
            "process_cpu_seconds_total",
            "process_resident_memory_bytes",
            "process_open_fds",
            "process_threads",
        ] {
            assert!(body.contains(name), "missing {name}; rendered:\n{body}");
        }
    }

    #[test]
    fn render_returns_string_even_pre_init() {
        // pre-init may return "" depending on test order; just ensure no panic.
        let _ = render();
    }
}
