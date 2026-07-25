//! Request-level RED metrics (Rate, Errors, Duration). Records:
//! - `http_requests_total{method,path,status}` counter
//! - `http_request_duration_seconds{method,path}` histogram
//! - `http_client_aborted_total{method,path}` counter
//!
//! Path label uses the route-match pattern (e.g. `/Items/{id}`) so label
//! cardinality stays bounded.
//!
//! The abort counter exists because a request the client GIVES UP ON produces
//! no response, so it reaches neither of the other two: it is invisible to
//! pharos and appears only in the reverse proxy's log as a 499. During the
//! browser-playback outage the sole evidence that anything was wrong was ~190
//! such aborts on one init segment — visible in Angie, absent from pharos.

use actix_web::{
    body::MessageBody,
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

/// Fires when a request's future is dropped before the handler returned —
/// i.e. the client disconnected while pharos was still working.
///
/// Dropping is the ONLY signal available: actix cancels the in-flight service
/// future on disconnect, so no handler code runs afterwards and no response
/// status is ever produced. Without this, a request that pharos never finished
/// is indistinguishable from one that never arrived.
struct AbortGuard {
    method: String,
    path: String,
    start: Instant,
    completed: bool,
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let elapsed = self.start.elapsed();
        metrics::counter!(
            "http_client_aborted_total",
            "method" => self.method.clone(),
            "path" => self.path.clone(),
        )
        .increment(1);
        if elapsed >= ABORT_WARN_AFTER {
            tracing::warn!(
                method = %self.method,
                path = %self.path,
                elapsed_ms = elapsed.as_millis() as u64,
                "client gave up waiting and disconnected before pharos responded"
            );
        } else {
            tracing::debug!(
                method = %self.method,
                path = %self.path,
                elapsed_ms = elapsed.as_millis() as u64,
                "client disconnected before pharos responded"
            );
        }
    }
}

pub struct RedMetrics;

impl<S, B> Transform<S, ServiceRequest> for RedMetrics
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
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
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let method = req.method().to_string();
        let path = req
            .match_pattern()
            .unwrap_or_else(|| req.path().to_string());
        let inner = self.inner.clone();
        let start = Instant::now();
        Box::pin(async move {
            // Armed for the whole handler; disarmed only once a response
            // exists. If the client disconnects first this future is dropped
            // mid-await and the guard is the only thing that still runs.
            let mut guard = AbortGuard {
                method: method.clone(),
                path: path.clone(),
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
            metrics::counter!(
                "http_requests_total",
                "method" => method.clone(),
                "path" => path.clone(),
                "status" => status.to_string(),
            )
            .increment(1);
            metrics::histogram!(
                "http_request_duration_seconds",
                "method" => method,
                "path" => path,
            )
            .record(elapsed);
            res
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
}
