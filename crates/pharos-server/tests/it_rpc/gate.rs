//! The native API ships dark (ADR-0010: every push to main deploys): with
//! `[server.native_api].enabled = false` its routes do not exist.

use actix_web::{test, App};
use pharos_server::api::pharos::rpc;
use pharos_server::config::NativeApiConfig;

use crate::common::seed_rich;

async fn status(enabled: bool, method: &str, path: &str) -> u16 {
    let fx = seed_rich().await;
    let cfg = NativeApiConfig {
        enabled,
        ..NativeApiConfig::default()
    };
    let app = test::init_service(App::new().app_data(fx.state).configure(rpc::configure(
        cfg,
        rpc::limits::ConnectionLimits::default(),
    )))
    .await;
    let req = match method {
        "POST" => test::TestRequest::post(),
        _ => test::TestRequest::get(),
    }
    .uri(path)
    .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

#[actix_web::test]
async fn native_routes_do_not_exist_when_disabled() {
    assert_eq!(status(false, "GET", "/pharos/rpc").await, 404);
    assert_eq!(status(false, "POST", "/pharos/session").await, 404);
}

#[actix_web::test]
async fn native_routes_exist_when_enabled() {
    // A plain GET is not a WebSocket upgrade: refused, but routed.
    assert_ne!(status(true, "GET", "/pharos/rpc").await, 404);
}
