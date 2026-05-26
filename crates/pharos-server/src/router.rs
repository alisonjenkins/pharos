use actix_web::{web, HttpResponse, Responder};

async fn root() -> impl Responder {
    HttpResponse::Ok().body("pharos")
}

async fn metrics() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(crate::obs::render())
}

/// Wire core routes. Health endpoints are wired separately via `health::configure`
/// so they can be reused/mounted independently.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/", web::get().to(root))
        .route("/metrics", web::get().to(metrics))
        .configure(crate::health::configure)
        .configure(crate::api::jellyfin::configure);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use actix_web::{test, App};

    #[actix_web::test]
    async fn root_responds_200() {
        let app = test::init_service(App::new().configure(configure)).await;
        let req = test::TestRequest::get().uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn metrics_responds_200() {
        let _ = crate::obs::init("info");
        let app = test::init_service(App::new().configure(configure)).await;
        let req = test::TestRequest::get().uri("/metrics").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }
}
