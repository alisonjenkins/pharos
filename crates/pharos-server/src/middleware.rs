//! Request-level RED metrics (Rate, Errors, Duration). Records:
//! - `http_requests_total{method,path,status}` counter
//! - `http_request_duration_seconds{method,path}` histogram
//!
//! Path label uses the route-match pattern (e.g. `/Items/{id}`) so label
//! cardinality stays bounded.

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
    time::Instant,
};

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
            let res = inner.call(req).await;
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
        let _ = crate::obs::init("info");
        let app = test::init_service(
            App::new()
                .wrap(RedMetrics)
                .route("/ping", web::get().to(|| async { HttpResponse::Ok().body("pong") })),
        )
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
        assert!(body.contains("path=\"/ping\""), "missing path label; rendered:\n{body}");
        assert!(body.contains("status=\"200\""), "missing status label; rendered:\n{body}");
    }

    #[actix_web::test]
    async fn middleware_labels_use_route_pattern_not_concrete_uri() {
        let _ = crate::obs::init("info");
        let app = test::init_service(
            App::new()
                .wrap(RedMetrics)
                .route("/Items/{id}", web::get().to(|| async { HttpResponse::Ok().finish() })),
        )
        .await;
        let req = test::TestRequest::get().uri("/Items/12345").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body = crate::obs::render();
        assert!(body.contains("path=\"/Items/{id}\""), "rendered:\n{body}");
        assert!(!body.contains("path=\"/Items/12345\""), "raw URI leaked");
    }
}
