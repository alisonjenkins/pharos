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
    // Who the queue is full of. `pending_jobs` says the queue is deep; only
    // this says whether the depth is client requests (real overload — add
    // capacity) or speculative warm-up sitting in front of them (a scheduling
    // defect — the segment a browser is blocked on waits behind segments
    // nobody asked for). The two look identical in every other signal.
    metrics::gauge!("pharos_transcode_pending_by_class", "class" => "interactive")
        .set(snap.pending_interactive as f64);
    metrics::gauge!("pharos_transcode_pending_by_class", "class" => "background")
        .set(snap.pending_background as f64);
    // A deep queue that drains fast is healthy; a shallow one whose head has
    // been waiting a minute is not. Depth alone cannot tell them apart.
    metrics::gauge!("pharos_transcode_pending_oldest_seconds")
        .set(snap.oldest_pending_ms.unwrap_or(0) as f64 / 1000.0);
    metrics::gauge!("pharos_transcode_inflight_jobs").set(snap.inflight as f64);
    // Live streams hold a device permit for as long as the client reads, and
    // report no job completion — so they are occupancy with nothing to
    // attribute it to. `in_use` minus inflight minus this is unexplained.
    metrics::gauge!("pharos_transcode_live_streams").set(snap.live_streams as f64);
    publish_device_gauges(&snap.devices);
}

/// Per-device gauges from one scheduler snapshot.
///
/// Split out from [`sample_scheduler`] so it can be asserted without standing
/// up an `AppState` and a scheduler: the interesting claim is what gets
/// published for a given device row, and that is entirely a function of this
/// loop.
fn publish_device_gauges(devices: &[pharos_transcode::scheduler::DeviceStat]) {
    for d in devices {
        let device = d.id.to_string();
        metrics::gauge!("pharos_transcode_device_capacity", "device" => device.clone())
            .set(d.capacity as f64);
        // Spec 007 — capacity is what a device CAN run; the weight is the share
        // of shared-init renditions the boot probe decided to give it, and the
        // two differ by the measured speed ratio. Placement bands on the
        // weight, so a rendition landing somewhere surprising is either a
        // misplacement or a mis-weighting — and without this gauge published
        // beside the pin counter those are one observation. On hardware nobody
        // has tested, that is the first question anyone asks.
        metrics::gauge!("pharos_transcode_device_weight", "device" => device.clone())
            .set(f64::from(d.weight));
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
        .configure(crate::api::pharos::remote_items::routes)
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

    /// ODD (spec 007) — the weight is the input placement bands on, and it is
    /// derived at boot from a probe nobody watches. Publishing the capacity
    /// beside it is not enough: on a table where every device happened to
    /// measure the same speed the two coincide, which is exactly the case a
    /// dashboard would look right in while a mis-weighting went unnoticed.
    ///
    /// So the fixture makes them differ. The device is given 2 permits and a
    /// weight of 8, and both numbers are asserted in the rendered exposition:
    /// a gauge wired to `capacity` renders 2, and one that is not wired at all
    /// renders nothing.
    #[actix_web::test]
    async fn the_device_weight_is_published_and_is_not_the_capacity() {
        use pharos_transcode::protocol::DeviceId;
        use pharos_transcode::scheduler::DeviceStat;

        let _ = crate::obs::init("info", None);
        publish_device_gauges(&[DeviceStat {
            id: DeviceId::Cpu,
            capacity: 2,
            weight: 8,
            in_use: 0,
            in_cooldown: false,
            inflight_interactive: 0,
            inflight_background: 0,
        }]);
        let rendered = crate::obs::render();
        let weight_line = rendered
            .lines()
            .find(|l| l.starts_with("pharos_transcode_device_weight{"))
            .unwrap_or_else(|| {
                panic!(
                    "pharos_transcode_device_weight must be published — without it a \
                     misplacement cannot be told from a mis-weighting.\n{rendered}"
                )
            });
        assert!(
            weight_line.contains(r#"device="cpu""#),
            "the weight must be per-device: {weight_line}"
        );
        assert!(
            weight_line.ends_with(" 8"),
            "the weight gauge must carry the placement weight, not the permit \
             count: {weight_line}"
        );
        // ...and the capacity gauge still says 2, so the two are genuinely
        // distinct series rather than one renamed.
        assert!(
            rendered
                .lines()
                .any(|l| l.starts_with("pharos_transcode_device_capacity{") && l.ends_with(" 2")),
            "the capacity gauge must be unchanged:\n{rendered}"
        );
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
