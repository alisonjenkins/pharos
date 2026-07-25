use actix_web::{web, HttpResponse, Responder};

async fn root() -> impl Responder {
    HttpResponse::Ok().body("pharos")
}

/// Sample the transcode scheduler into gauges, immediately before rendering.
///
/// `SchedSnapshot` has always carried exactly the saturation state a stalling
/// playback question needs — per-device capacity and occupancy, whether a
/// device is in cooldown, and the queue depth — and nothing consumed it, so
/// "were the encoders saturated at 20:14?" was unanswerable after the fact.
/// Per-request `queue_wait_ms` shows a job WAITED; only this shows what it
/// waited behind.
///
/// Sampled at scrape time rather than on a timer: the value is then exactly as
/// old as the scrape, and an idle server does no work.
async fn sample_scheduler(state: Option<&crate::state::AppState>) {
    let Some(sched) = state.and_then(|s| s.transcode_scheduler.as_ref()) else {
        return;
    };
    let Some(snap) = sched.snapshot().await else {
        return;
    };
    metrics::gauge!("pharos_transcode_pending_jobs").set(snap.pending as f64);
    metrics::gauge!("pharos_transcode_idle_workers").set(snap.idle_workers as f64);
    for d in snap.devices {
        let device = d.id.to_string();
        metrics::gauge!("pharos_transcode_device_capacity", "device" => device.clone())
            .set(d.capacity as f64);
        metrics::gauge!("pharos_transcode_device_in_use", "device" => device.clone())
            .set(d.in_use as f64);
        // A device silently sidelined by cooldown looks identical to one that
        // is merely idle, and it is the reason a GPU-backed deployment can
        // quietly serve everything from the CPU.
        metrics::gauge!("pharos_transcode_device_cooldown", "device" => device)
            .set(u8::from(d.in_cooldown) as f64);
    }
}

async fn metrics(state: Option<web::Data<crate::state::AppState>>) -> impl Responder {
    sample_scheduler(state.as_deref().map(|s| &**s)).await;
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
        .configure(crate::api::jellyfin::configure)
        .configure(crate::dlna::register)
        .configure(pharos_sync::ws::register);
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
        let _ = crate::obs::init("info", None);
        let app = test::init_service(App::new().configure(configure)).await;
        let req = test::TestRequest::get().uri("/metrics").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }
}
