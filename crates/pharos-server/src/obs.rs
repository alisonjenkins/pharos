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
        let span = tracing_actix_web::root_span!(request);
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let parent = parent_context(request.headers());
        if parent.span().span_context().is_valid() {
            let _ = span.set_parent(parent);
        }
        span
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

/// Metrics whose values routinely exceed 30 s (an anchor "grows without
/// bound", a scan runs for minutes, a transcode queues behind others): in the
/// 30 s ladder they all land in +Inf and `histogram_quantile` caps at 30 s.
const LONG_RUNNING_SECONDS: &[&str] = &[
    "pharos_syncplay_hydrate_anchor_age_seconds",
    "pharos_syncplay_socket_gone_seconds",
    "pharos_syncplay_buffering_freeze_seconds",
    "pharos_syncplay_gate_wait_seconds",
    "pharos_syncplay_resume_catch_up_seconds",
    "pharos_segment_transcode_seconds",
    "pharos_transcode_encode_seconds",
    "pharos_transcode_queue_wait_seconds",
    "pharos_scan_duration_seconds",
    "pharos_media_integrity_seconds",
];

const LONG_SECONDS_BUCKETS: &[f64] = &[
    0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

/// Indexed key reads take 0.05-0.5 ms, below the first general bucket.
const SUB_MS_BUCKETS: &[f64] = &[
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0,
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
    init_with_sampling(log_level, otlp_endpoint, 1.0)
}

/// [`init`] with an OTLP `sampling_ratio` (0.0..=1.0): the fraction of traces
/// STARTED here that are exported. A sampled remote parent is always kept.
pub fn init_with_sampling(
    log_level: &str,
    otlp_endpoint: Option<&str>,
    sampling_ratio: f64,
) -> Result<PrometheusHandle, ObsError> {
    INIT.call_once(|| {
        let outcome = (|| -> Result<PrometheusHandle, ObsError> {
            let filter =
                EnvFilter::try_new(log_level).map_err(|e| ObsError::Filter(e.to_string()))?;
            let fmt_layer = tracing_subscriber::fmt::layer().json();
            // Optional OTLP span-export layer. `Option<Layer>` is itself a
            // Layer (None = no-op), so the registry composes uniformly.
            let otel_layer = match otlp_endpoint {
                Some(ep) => Some(build_otel_layer(ep, sampling_ratio)?),
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
            // Exact-name matchers outrank the `_seconds` suffix fallback.
            let mut builder = PrometheusBuilder::new();
            for name in LONG_RUNNING_SECONDS {
                builder = builder
                    .set_buckets_for_metric(Matcher::Full((*name).into()), LONG_SECONDS_BUCKETS)
                    .map_err(|e| ObsError::Prom(e.to_string()))?;
            }
            let handle = builder
                .set_buckets_for_metric(
                    Matcher::Full("pharos_store_query_seconds".into()),
                    SUB_MS_BUCKETS,
                )
                .map_err(|e| ObsError::Prom(e.to_string()))?
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

struct HeaderExtractor<'a>(&'a actix_web::http::header::HeaderMap);

impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// The remote parent named by a W3C `traceparent` header, or an empty context
/// when there is none (or it is malformed). Without this the parent-based
/// sampler never sees a parent and every request starts a fresh trace.
fn parent_context(headers: &actix_web::http::header::HeaderMap) -> opentelemetry::Context {
    use opentelemetry::propagation::TextMapPropagator as _;
    opentelemetry_sdk::propagation::TraceContextPropagator::new().extract(&HeaderExtractor(headers))
}

/// Parent-based sampler: an incoming sampled `traceparent` is honoured so a
/// distributed trace is never cut in half; only root spans started here are
/// thinned to `ratio`. At 1.0 this is the SDK default (parent-based always-on).
fn sampler_for(ratio: f64) -> opentelemetry_sdk::trace::Sampler {
    use opentelemetry_sdk::trace::Sampler;
    let root = if ratio >= 1.0 {
        Sampler::AlwaysOn
    } else {
        Sampler::TraceIdRatioBased(ratio.max(0.0))
    };
    Sampler::ParentBased(Box::new(root))
}

/// Build the OTLP/gRPC span-export layer pointed at `endpoint`, registering
/// the provider globally + in `TRACER_PROVIDER` so it outlives `init`. Generic
/// over the subscriber `S` so it composes onto the layered registry.
fn build_otel_layer<S>(
    endpoint: &str,
    sampling_ratio: f64,
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
        .with_sampler(sampler_for(sampling_ratio))
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

    fn decide(sampler: &opentelemetry_sdk::trace::Sampler, parent_sampled: Option<bool>) -> bool {
        use opentelemetry::trace::{
            SpanContext, SpanId, SpanKind, TraceContextExt, TraceFlags, TraceId, TraceState,
        };
        use opentelemetry_sdk::trace::{SamplingDecision, ShouldSample};
        let parent = parent_sampled.map(|sampled| {
            let flags = if sampled {
                TraceFlags::SAMPLED
            } else {
                TraceFlags::default()
            };
            opentelemetry::Context::new().with_remote_span_context(SpanContext::new(
                TraceId::from(7u128),
                SpanId::from(7u64),
                flags,
                true,
                TraceState::default(),
            ))
        });
        let result = sampler.should_sample(
            parent.as_ref(),
            TraceId::from(0xFFFF_FFFF_FFFF_FFFF_u128),
            "t",
            &SpanKind::Server,
            &[],
            &[],
        );
        result.decision == SamplingDecision::RecordAndSample
    }

    fn bucket_bounds(body: &str, metric: &str) -> Vec<String> {
        body.lines()
            .filter(|l| l.starts_with(&format!("{metric}_bucket{{")))
            .filter_map(|l| l.split("le=\"").nth(1)?.split('"').next().map(String::from))
            .collect()
    }

    /// A 10-minute-stale anchor in the 30 s ladder lands in +Inf and
    /// `histogram_quantile` caps at 30 s, hiding the failure. Long-running
    /// metrics need a ladder that reaches an hour; indexed key reads need one
    /// that resolves 0.1 ms.
    #[test]
    fn each_metric_family_gets_the_ladder_that_fits_it() {
        init("info", None).unwrap();
        metrics::histogram!("pharos_syncplay_hydrate_anchor_age_seconds").record(600.0);
        metrics::histogram!("pharos_segment_transcode_seconds").record(90.0);
        metrics::histogram!("pharos_store_query_seconds", "method" => "m").record(0.0002);
        metrics::histogram!("http_request_duration_seconds", "method" => "GET", "path" => "/x")
            .record(0.2);
        let body = render();
        for long in [
            "pharos_syncplay_hydrate_anchor_age_seconds",
            "pharos_segment_transcode_seconds",
        ] {
            assert!(
                bucket_bounds(&body, long).contains(&"3600".to_string()),
                "{long} must reach 1 h"
            );
        }
        assert!(bucket_bounds(&body, "pharos_store_query_seconds").contains(&"0.0001".to_string()));
        let http = bucket_bounds(&body, "http_request_duration_seconds");
        assert!(http.contains(&"30".to_string()) && !http.contains(&"3600".to_string()));
    }

    #[test]
    fn a_w3c_traceparent_header_becomes_the_remote_parent() {
        use opentelemetry::trace::TraceContextExt;
        let mut headers = actix_web::http::header::HeaderMap::new();
        headers.insert(
            actix_web::http::header::HeaderName::from_static("traceparent"),
            actix_web::http::header::HeaderValue::from_static(
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            ),
        );
        let cx = parent_context(&headers);
        let sc = cx.span().span_context().clone();
        assert!(sc.is_valid() && sc.is_sampled() && sc.is_remote());
        assert_eq!(
            sc.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
    }

    #[test]
    fn no_or_garbage_traceparent_yields_no_parent() {
        use opentelemetry::trace::TraceContextExt;
        let mut headers = actix_web::http::header::HeaderMap::new();
        assert!(!parent_context(&headers).span().span_context().is_valid());
        headers.insert(
            actix_web::http::header::HeaderName::from_static("traceparent"),
            actix_web::http::header::HeaderValue::from_static("not-a-traceparent"),
        );
        assert!(!parent_context(&headers).span().span_context().is_valid());
    }

    #[test]
    fn ratio_one_keeps_every_trace_and_zero_keeps_none() {
        assert!(decide(&sampler_for(1.0), None));
        assert!(!decide(&sampler_for(0.0), None));
    }

    /// Thinning must never orphan a distributed trace: a sampled parent stays
    /// sampled, an unsampled one stays dropped, whatever the local ratio.
    #[test]
    fn a_sampled_parent_is_kept_even_at_ratio_zero() {
        assert!(decide(&sampler_for(0.0), Some(true)));
        assert!(!decide(&sampler_for(1.0), Some(false)));
    }

    #[test]
    fn render_returns_string_even_pre_init() {
        // pre-init may return "" depending on test order; just ensure no panic.
        let _ = render();
    }
}
