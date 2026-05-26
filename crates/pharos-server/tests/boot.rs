#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::App;
use pharos_server::{config::Config, obs, router};

#[actix_web::test]
async fn server_boots_and_serves_root() {
    let _ = obs::init("info");
    let app =
        actix_web::test::init_service(App::new().configure(router::configure)).await;
    let req = actix_web::test::TestRequest::get().uri("/").to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

#[actix_web::test]
async fn metrics_endpoint_serves() {
    let _ = obs::init("info");
    let app =
        actix_web::test::init_service(App::new().configure(router::configure)).await;
    let req = actix_web::test::TestRequest::get()
        .uri("/metrics")
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

#[test]
fn config_example_parses() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config.example.toml");
    let cfg = Config::from_path(&path).unwrap();
    assert_eq!(cfg.server.bind, "0.0.0.0:8096");
}
