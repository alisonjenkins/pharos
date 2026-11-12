//! Request-level RED metrics (Rate, Errors, Duration). Records:
//! - `http_requests_total{method,path,status}` counter
//! - `http_request_duration_seconds{method,path}` histogram
//! - `http_client_aborted_total{method,path,client,phase}` counter
//!
//! Path label uses the route-match pattern (e.g. `/Items/{id}`) so label
//! cardinality stays bounded.
//!
//! The abort counter exists because a request the client GIVES UP ON produces
//! no response, so it reaches neither of the other two: it is invisible to
//! pharos and appears only in the reverse proxy's log as a 499. During the
//! browser-playback outage the sole evidence that anything was wrong was ~190
//! such aborts on one init segment — visible in Angie, absent from pharos.

use crate::client_class::ClientClass;
use actix_web::{
    body::{BodySize, BoxBody, EitherBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    Error,
};
use std::{
    future::{ready, Future, Ready},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

/// How long a request must have been running for its abandonment to be worth a
/// WARN. Below this a client giving up is routine — a seek cancels its
/// in-flight segment, a page navigation drops pending art — and logging it
/// would bury the interesting case. Above it, the client waited this long and
/// pharos still had nothing, which is a server problem by definition.
const ABORT_WARN_AFTER: Duration = Duration::from_secs(5);

/// Where in a request's life the client gave up: `handler` (no response yet)
/// or `body` (headers sent, body not finished). Bounded `phase` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbortPhase {
    Handler,
    Body,
}

impl AbortPhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Handler => "handler",
            Self::Body => "body",
        }
    }
}

fn record_abort(
    method: &str,
    path: &str,
    client: ClientClass,
    phase: AbortPhase,
    elapsed: Duration,
) {
    metrics::counter!(
        "http_client_aborted_total",
        "method" => method.to_owned(),
        "path" => path.to_owned(),
        "client" => client.label(),
        "phase" => phase.label(),
    )
    .increment(1);
    let (client, phase) = (client.label(), phase.label());
    if elapsed >= ABORT_WARN_AFTER {
        tracing::warn!(
            method,
            path,
            client,
            phase,
            elapsed_ms = elapsed.as_millis() as u64,
            "client gave up and disconnected before pharos finished responding"
        );
    } else {
        tracing::debug!(
            method,
            path,
            client,
            phase,
            elapsed_ms = elapsed.as_millis() as u64,
            "client disconnected before pharos finished responding"
        );
    }
}

/// Fires when a request's future is dropped before the handler returned —
/// i.e. the client disconnected while pharos was still working.
///
/// Dropping is the ONLY signal available, and actix gives it only when the
/// connection is torn down while the handler runs. By default actix keeps a
/// half-closed HTTP/1 connection (`h1_allow_half_closed`) and lets the handler
/// finish, so behind a reverse proxy that closes its side on a 499 this guard
/// rarely fires; the client's departure then shows up only as a failed write,
/// which drops the response body — see [`AbortBody`].
struct AbortGuard {
    method: String,
    path: String,
    client: ClientClass,
    start: Instant,
    completed: bool,
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        record_abort(
            &self.method,
            &self.path,
            self.client,
            AbortPhase::Handler,
            self.start.elapsed(),
        );
    }
}

/// Response body of a media route, counting the client that never read it to
/// the end. The handler has already returned by then, so [`AbortGuard`] is
/// disarmed: a segment served as headers plus a multi-MB body, abandoned
/// mid-transfer, was invisible to every pharos signal and surfaced only as a
/// 499 in the proxy log.
struct AbortBody {
    inner: BoxBody,
    method: String,
    path: String,
    client: ClientClass,
    start: Instant,
    /// A natural end or an error was seen: a definite outcome already
    /// reported elsewhere, so the drop that follows is not an abandonment.
    settled: bool,
}

impl MessageBody for AbortBody {
    type Error = Box<dyn std::error::Error>;

    fn size(&self) -> BodySize {
        self.inner.size()
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<actix_web::web::Bytes, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_next(cx);
        if matches!(polled, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            this.settled = true;
        }
        polled
    }
}

impl Drop for AbortBody {
    fn drop(&mut self) {
        if !self.settled {
            record_abort(
                &self.method,
                &self.path,
                self.client,
                AbortPhase::Body,
                self.start.elapsed(),
            );
        }
    }
}

/// Path label for requests that match no route. The raw path is attacker
/// controlled (`/.env`, `/.git/config`, ...) and would mint a series each.
const UNMATCHED_PATH: &str = "unmatched";

/// Standard methods verbatim; anything else is `OTHER`, for the same reason.
fn method_label(method: &actix_web::http::Method) -> String {
    use actix_web::http::Method;
    let standard = [
        Method::GET,
        Method::HEAD,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::PATCH,
        Method::OPTIONS,
    ];
    if standard.contains(method) {
        method.to_string()
    } else {
        "OTHER".to_string()
    }
}

pub struct RedMetrics;

impl<S, B> Transform<S, ServiceRequest> for RedMetrics
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = RedMetricsMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(RedMetricsMiddleware {
            inner: Rc::new(service),
        }))
    }
}

pub struct RedMetricsMiddleware<S> {
    inner: Rc<S>,
}

impl<S, B> Service<ServiceRequest> for RedMetricsMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let method = method_label(req.method());
        let path = req
            .match_pattern()
            .unwrap_or_else(|| UNMATCHED_PATH.to_string());
        let media_route = crate::media_telemetry::MediaRoute::from_pattern(&path);
        let client = crate::client_class::ClientClass::of(req.request());
        let inner = self.inner.clone();
        let start = Instant::now();
        Box::pin(async move {
            // Armed for the whole handler; disarmed only once a response
            // exists. If the client disconnects first this future is dropped
            // mid-await and the guard is the only thing that still runs.
            let mut guard = AbortGuard {
                method: method.clone(),
                path: path.clone(),
                client,
                start,
                completed: false,
            };
            let res = inner.call(req).await;
            guard.completed = true;
            let elapsed = start.elapsed().as_secs_f64();
            let status = match &res {
                Ok(r) => r.status().as_u16(),
                Err(_) => 500,
            };
            if let (Some(route), Ok(r)) = (media_route, &res) {
                if r.status().is_success() {
                    crate::media_telemetry::record_request(
                        route,
                        client,
                        crate::media_telemetry::cache_outcome(r.request()),
                        elapsed,
                    );
                }
            }
            metrics::counter!(
                "http_requests_total",
                "method" => method.clone(),
                "path" => path.clone(),
                "status" => status.to_string(),
            )
            .increment(1);
            metrics::histogram!(
                "http_request_duration_seconds",
                "method" => method.clone(),
                "path" => path.clone(),
            )
            .record(elapsed);
            // Only a GET on a media route: other bodies are small, and HEAD
            // and 204/304 responses have a body actix never polls, which
            // would read as an abandonment.
            let watch_body = media_route.is_some() && method == "GET";
            res.map(|r| {
                r.map_body(|head, body| {
                    let streams = !matches!(body.size(), BodySize::None | BodySize::Sized(0))
                        && !matches!(head.status.as_u16(), 204 | 304);
                    if watch_body && streams {
                        EitherBody::right(BoxBody::new(AbortBody {
                            inner: body.boxed(),
                            method,
                            path,
                            client,
                            start,
                            settled: false,
                        }))
                    } else {
                        EitherBody::left(body)
                    }
                })
            })
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use actix_web::{test, web, App, HttpResponse};

    #[actix_web::test]
    async fn middleware_records_counter_and_histogram() {
        let _ = crate::obs::init("info", None);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/ping",
            web::get().to(|| async { HttpResponse::Ok().body("pong") }),
        ))
        .await;
        let req = test::TestRequest::get().uri("/ping").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body = crate::obs::render();
        assert!(
            body.contains("http_requests_total"),
            "missing counter; rendered:\n{body}"
        );
        assert!(
            body.contains("http_request_duration_seconds"),
            "missing histogram; rendered:\n{body}"
        );
        assert!(
            body.contains("path=\"/ping\""),
            "missing path label; rendered:\n{body}"
        );
        assert!(
            body.contains("status=\"200\""),
            "missing status label; rendered:\n{body}"
        );
    }

    /// The outage shape: a handler that never finishes, and a client that
    /// walks away. Dropping the future is exactly what actix does on
    /// disconnect, so the guard must still count it.
    #[actix_web::test]
    async fn abandoning_a_request_mid_handler_is_counted() {
        let _ = crate::obs::init("info", None);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/videos/{id}/never",
            web::get().to(|| async {
                // Never resolves — stands in for a segment transcode the
                // client outlives.
                std::future::pending::<()>().await;
                HttpResponse::Ok().finish()
            }),
        ))
        .await;
        let req = test::TestRequest::get().uri("/videos/7/never").to_request();
        let call = test::call_service(&app, req);
        // Give the handler a poll, then abandon it exactly as a disconnect
        // would.
        let mut call = Box::pin(call);
        let polled = futures_util::poll!(&mut call);
        assert!(polled.is_pending(), "handler should not have completed");
        drop(call);

        let body = crate::obs::render();
        assert!(
            body.contains("http_client_aborted_total"),
            "abandoned request not counted; rendered:\n{body}"
        );
        assert!(
            body.contains("path=\"/videos/{id}/never\""),
            "abort counter missing route label; rendered:\n{body}"
        );
    }

    /// A request that COMPLETES must never be counted as an abort — otherwise
    /// the counter measures traffic, not failure.
    #[actix_web::test]
    async fn a_completed_request_is_not_counted_as_an_abort() {
        let _ = crate::obs::init("info", None);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/finishes",
            web::get().to(|| async { HttpResponse::Ok().finish() }),
        ))
        .await;
        let req = test::TestRequest::get().uri("/finishes").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body = crate::obs::render();
        assert!(
            !body.contains("path=\"/finishes\"")
                || !body
                    .lines()
                    .any(|l| l.starts_with("http_client_aborted_total") && l.contains("/finishes")),
            "completed request counted as an abort; rendered:\n{body}"
        );
    }

    /// Scanner traffic (`/.env`, `/.git/config`, ...) matches no route; its raw
    /// path must not become a label, or every probe mints a histogram's worth
    /// of series. Non-standard methods collapse the same way.
    #[actix_web::test]
    async fn unmatched_paths_and_odd_methods_share_one_series() {
        let _ = crate::obs::init("info", None);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/known",
            web::get().to(|| async { HttpResponse::Ok().finish() }),
        ))
        .await;
        for junk in ["/.env", "/.git/config", "/wp-login.php"] {
            let req = test::TestRequest::get().uri(junk).to_request();
            test::call_service(&app, req).await;
        }
        let req = test::TestRequest::default()
            .method(actix_web::http::Method::from_bytes(b"BREW").unwrap())
            .uri("/coffee")
            .to_request();
        test::call_service(&app, req).await;

        let body = crate::obs::render();
        for raw in ["/.env", "/.git/config", "/wp-login.php", "/coffee", "BREW"] {
            assert!(!body.contains(raw), "{raw} leaked into labels:\n{body}");
        }
        let series: Vec<&str> = body
            .lines()
            .filter(|l| l.starts_with("http_requests_total{") && l.contains("status=\"404\""))
            .collect();
        assert!(
            series.iter().any(|l| l.contains("path=\"unmatched\"")),
            "no unmatched series: {series:?}"
        );
        assert!(series.iter().any(|l| l.contains("method=\"OTHER\"")));
    }

    #[actix_web::test]
    async fn middleware_labels_use_route_pattern_not_concrete_uri() {
        let _ = crate::obs::init("info", None);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/Items/{id}",
            web::get().to(|| async { HttpResponse::Ok().finish() }),
        ))
        .await;
        let req = test::TestRequest::get().uri("/Items/12345").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body = crate::obs::render();
        assert!(body.contains("path=\"/Items/{id}\""), "rendered:\n{body}");
        assert!(!body.contains("path=\"/Items/12345\""), "raw URI leaked");
    }

    fn aborts(
        snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
        labels: &[(&str, &str)],
    ) -> u64 {
        use metrics_util::debugging::DebugValue;
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, _, _, _)| k.key().name() == "http_client_aborted_total")
            .filter(|(k, _, _, _)| {
                labels
                    .iter()
                    .all(|(n, v)| k.key().labels().any(|l| l.key() == *n && l.value() == *v))
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Counter(n) => n,
                _ => 0,
            })
            .sum()
    }

    /// A segment whose body the client stops reading: headers went out, the
    /// handler returned, and only the dropped body can say so.
    fn stalled_body() -> HttpResponse {
        let chunks = futures_util::stream::once(std::future::ready(Ok::<_, Error>(
            actix_web::web::Bytes::from_static(b"first chunk"),
        )))
        .chain(futures_util::stream::pending());
        HttpResponse::Ok().streaming(chunks)
    }

    use futures_util::StreamExt;

    #[actix_web::test]
    async fn a_media_body_dropped_mid_transfer_is_counted_for_its_client() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/videos/{id}/hls1/main/{seg}.ts",
            web::get().to(|| async { stalled_body() }),
        ))
        .await;
        let req = test::TestRequest::get()
            .uri("/videos/7/hls1/main/3.ts")
            .insert_header(("User-Agent", "Pharos Android TV"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success(), "headers were sent");
        let labels = [
            ("path", "/videos/{id}/hls1/main/{seg}.ts"),
            ("client", "pharos_android"),
            ("phase", "body"),
        ];
        assert_eq!(aborts(&snapshotter, &labels), 0, "still streaming");
        drop(resp);
        assert_eq!(
            aborts(&snapshotter, &labels),
            1,
            "a body dropped before its end must count as an abort"
        );
    }

    #[actix_web::test]
    async fn a_media_body_read_to_the_end_is_not_an_abort() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/videos/{id}/hls1/main/{seg}.ts",
            web::get().to(|| async { HttpResponse::Ok().body("whole segment") }),
        ))
        .await;
        let req = test::TestRequest::get()
            .uri("/videos/7/hls1/main/3.ts")
            .to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        assert_eq!(&body[..], b"whole segment");
        assert_eq!(aborts(&snapshotter, &[]), 0);
    }

    /// Only media routes pay for the watcher, and a dropped body elsewhere
    /// (a small JSON reply) is not a viewer giving up.
    #[actix_web::test]
    async fn a_dropped_body_on_a_non_media_route_is_not_counted() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = test::init_service(
            App::new()
                .wrap(RedMetrics)
                .route("/items/{id}", web::get().to(|| async { stalled_body() })),
        )
        .await;
        let req = test::TestRequest::get().uri("/items/7").to_request();
        drop(test::call_service(&app, req).await);
        assert_eq!(aborts(&snapshotter, &[]), 0);
    }

    /// The handler-phase abort carries the client too, and says it was the
    /// handler that was abandoned.
    #[actix_web::test]
    async fn an_abandoned_handler_is_counted_with_client_and_phase() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/videos/{id}/hls1/main/{seg}.ts",
            web::get().to(|| async {
                std::future::pending::<()>().await;
                HttpResponse::Ok().finish()
            }),
        ))
        .await;
        let req = test::TestRequest::get()
            .uri("/videos/7/hls1/main/3.ts")
            .insert_header(("User-Agent", "okhttp/4.12.0"))
            .to_request();
        let mut call = Box::pin(test::call_service(&app, req));
        assert!(futures_util::poll!(&mut call).is_pending());
        drop(call);
        assert_eq!(
            aborts(
                &snapshotter,
                &[("client", "jellyfin_android"), ("phase", "handler")]
            ),
            1
        );
    }
}
