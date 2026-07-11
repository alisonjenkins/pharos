//! Health + readiness endpoints. Readiness state is owned by a single
//! `Readiness` actor (V18) — no shared `Mutex` on the request path.

use actix_web::{web, HttpResponse, Responder};
use serde::Serialize;
use std::collections::BTreeMap;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Serialize, Clone)]
pub struct ProbeStatus {
    pub name: &'static str,
    pub ok: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct Snapshot {
    pub ready: bool,
    pub probes: Vec<ProbeStatus>,
    /// Set once the process is draining (SIGTERM received). Forces `ready`
    /// false regardless of probe state so the load balancer stops routing
    /// new requests while in-flight ones finish (Phase B3).
    #[serde(default)]
    pub draining: bool,
}

#[derive(Debug, Serialize)]
struct Info {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadinessError {
    #[error("readiness actor dropped")]
    ActorDown,
    #[error("readiness reply dropped")]
    ReplyDropped,
}

enum Msg {
    Require(&'static str),
    Mark(&'static str),
    Drain,
    Snapshot(oneshot::Sender<Snapshot>),
}

#[derive(Clone)]
pub struct ReadinessHandle {
    tx: mpsc::Sender<Msg>,
}

impl ReadinessHandle {
    /// Spawn the actor with the given required probe names. The actor
    /// task lives for the lifetime of the process.
    pub fn spawn(required: &[&'static str]) -> Self {
        let (tx, mut rx) = mpsc::channel::<Msg>(64);
        let init: Vec<&'static str> = required.to_vec();
        tokio::spawn(async move {
            let mut state: BTreeMap<&'static str, bool> =
                init.into_iter().map(|n| (n, false)).collect();
            let mut draining = false;
            while let Some(msg) = rx.recv().await {
                match msg {
                    Msg::Require(n) => {
                        state.entry(n).or_insert(false);
                    }
                    Msg::Mark(n) => {
                        state.insert(n, true);
                    }
                    Msg::Drain => {
                        draining = true;
                    }
                    Msg::Snapshot(reply) => {
                        let probes: Vec<ProbeStatus> = state
                            .iter()
                            .map(|(n, ok)| ProbeStatus { name: n, ok: *ok })
                            .collect();
                        let ready = !draining && !probes.is_empty() && probes.iter().all(|p| p.ok);
                        let _ = reply.send(Snapshot {
                            ready,
                            probes,
                            draining,
                        });
                    }
                }
            }
        });
        Self { tx }
    }

    pub async fn require(&self, name: &'static str) -> Result<(), ReadinessError> {
        self.tx
            .send(Msg::Require(name))
            .await
            .map_err(|_| ReadinessError::ActorDown)
    }

    pub async fn mark(&self, name: &'static str) -> Result<(), ReadinessError> {
        self.tx
            .send(Msg::Mark(name))
            .await
            .map_err(|_| ReadinessError::ActorDown)
    }

    /// Flip the process into draining state — `/readyz` goes 503 from here on
    /// (Phase B3 graceful shutdown). Idempotent; there is no un-drain.
    pub async fn drain(&self) -> Result<(), ReadinessError> {
        self.tx
            .send(Msg::Drain)
            .await
            .map_err(|_| ReadinessError::ActorDown)
    }

    pub async fn snapshot(&self) -> Result<Snapshot, ReadinessError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::Snapshot(tx))
            .await
            .map_err(|_| ReadinessError::ActorDown)?;
        rx.await.map_err(|_| ReadinessError::ReplyDropped)
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/healthz", web::get().to(healthz))
        .route("/readyz", web::get().to(readyz))
        .route("/info", web::get().to(info));
}

async fn healthz() -> impl Responder {
    HttpResponse::Ok().content_type("text/plain").body("ok")
}

async fn info() -> impl Responder {
    HttpResponse::Ok().json(Info {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn readyz(handle: web::Data<ReadinessHandle>) -> impl Responder {
    match handle.snapshot().await {
        Ok(snap) if snap.ready => HttpResponse::Ok().json(snap),
        Ok(snap) => HttpResponse::ServiceUnavailable().json(snap),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use actix_web::{test, App};

    fn build_app(
        handle: ReadinessHandle,
    ) -> App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        App::new()
            .app_data(web::Data::new(handle))
            .configure(configure)
    }

    #[actix_web::test]
    async fn healthz_always_ok() {
        let h = ReadinessHandle::spawn(&["x"]);
        let app = test::init_service(build_app(h)).await;
        let req = test::TestRequest::get().uri("/healthz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn info_includes_name_and_version() {
        let h = ReadinessHandle::spawn(&["x"]);
        let app = test::init_service(build_app(h)).await;
        let req = test::TestRequest::get().uri("/info").to_request();
        let body = test::call_and_read_body(&app, req).await;
        let txt = std::str::from_utf8(&body).unwrap();
        assert!(txt.contains("\"name\":\"pharos-server\""), "got: {txt}");
        assert!(txt.contains("\"version\""), "got: {txt}");
    }

    #[actix_web::test]
    async fn readyz_503_when_required_probe_unmet() {
        let h = ReadinessHandle::spawn(&["scanner"]);
        let app = test::init_service(build_app(h)).await;
        let req = test::TestRequest::get().uri("/readyz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 503);
    }

    #[actix_web::test]
    async fn readyz_200_after_all_required_marked() {
        let h = ReadinessHandle::spawn(&["scanner", "store"]);
        h.mark("scanner").await.unwrap();
        h.mark("store").await.unwrap();
        let app = test::init_service(build_app(h)).await;
        let req = test::TestRequest::get().uri("/readyz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn readyz_503_when_no_probes_registered() {
        let h = ReadinessHandle::spawn(&[]);
        let app = test::init_service(build_app(h)).await;
        let req = test::TestRequest::get().uri("/readyz").to_request();
        let resp = test::call_service(&app, req).await;
        // No probes → not ready (avoid false positive on empty config).
        assert_eq!(resp.status(), 503);
    }

    #[actix_web::test]
    async fn readyz_503_after_drain_even_when_probes_met() {
        // Graceful drain (Phase B3): once SIGTERM flips the handle to
        // draining, /readyz must go unready so the LB stops routing new
        // requests, even though every startup probe is still marked ok.
        let h = ReadinessHandle::spawn(&["store"]);
        h.mark("store").await.unwrap();
        let app = test::init_service(build_app(h.clone())).await;

        let req = test::TestRequest::get().uri("/readyz").to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 200);

        h.drain().await.unwrap();
        let req = test::TestRequest::get().uri("/readyz").to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 503);
    }

    #[actix_web::test]
    async fn healthz_ok_even_while_draining() {
        // Liveness must stay green during drain — a draining pod is alive,
        // just not accepting new traffic; kubelet must not restart it.
        let h = ReadinessHandle::spawn(&["store"]);
        h.mark("store").await.unwrap();
        h.drain().await.unwrap();
        let app = test::init_service(build_app(h)).await;
        let req = test::TestRequest::get().uri("/healthz").to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 200);
    }
}
