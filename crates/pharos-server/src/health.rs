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
    /// Why the media share cannot be read, when it cannot (B1531). Set
    /// only by `/readyz`. It answers 503 with it, or 200 while
    /// `media_restart` is set (ADR-0156) or the share is absent (B1538).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
    /// Set while the pod stays Ready through a broken share because it asked
    /// for its own rollout restart (`requesting` or `requested`, ADR-0156).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_restart: Option<&'static str>,
}

/// The media share `/readyz` checks. Registered by the server binary; an app
/// without it (tests, tools) skips the check.
#[derive(Clone, Copy)]
pub struct MediaReadiness {
    pub share: &'static crate::media_share::MediaShare,
    /// When set, a share fault in a pod that has read its share keeps it
    /// Ready while its asked-for restart is pending (ADR-0156).
    pub restart: Option<&'static crate::self_restart::SelfRestart>,
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
                            media: None,
                            media_restart: None,
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

/// A point-in-time answer: a share fault latched after the media check
/// (during the second actor read below) shows on the next probe, not this
/// one. Re-reading again would only move the window, and a probe runs every
/// few seconds, so one late 200 costs nothing the next probe does not fix.
async fn readyz(
    handle: web::Data<ReadinessHandle>,
    media: Option<web::Data<MediaReadiness>>,
) -> impl Responder {
    let mut snap = match handle.snapshot().await {
        Ok(snap) => snap,
        Err(e) => return HttpResponse::InternalServerError().body(e.to_string()),
    };
    if let (true, Some(media)) = (snap.ready, media) {
        if let Err(e) = media.share.check().await {
            snap.media = Some(e.to_string());
            // An absent share leaves the pod Ready: browsing needs no share,
            // and a fresh mount would be just as empty (B1538).
            if !matches!(e, crate::media_share::ShareError::Absent { .. }) {
                snap.media_restart = stays_ready(&media, &e);
                snap.ready = snap.media_restart.is_some();
            }
        }
        // A drain may have begun while the check awaited; it wins over the
        // restart's stay-ready, which would otherwise answer 200.
        if snap.ready && handle.snapshot().await.map_or(true, |now| now.draining) {
            snap.draining = true;
            snap.ready = false;
            // Not staying ready after all; the cause under `media` stays.
            snap.media_restart = None;
        }
        // The restart may also have failed, or passed its deadline, during
        // that await: read it once more so a stale `requesting` never
        // answers 200.
        if snap.media_restart.is_some() {
            snap.media_restart = media
                .restart
                .filter(|r| r.keeps_ready())
                .and_then(|r| r.status());
            snap.ready = snap.media_restart.is_some();
        }
        record_media(&snap, media.share);
    }
    if snap.ready {
        HttpResponse::Ok().json(snap)
    } else {
        HttpResponse::ServiceUnavailable().json(snap)
    }
}

/// Whether a pod whose share check failed with `e` stays Ready, and why: the
/// share is broken (not merely slow), the pod has read it before, and it
/// asked for its own restart less than its give-up time ago (ADR-0156).
fn stays_ready(media: &MediaReadiness, e: &crate::media_share::ShareError) -> Option<&'static str> {
    use crate::media_share::ShareError;
    let restart = media.restart?;
    let broken = matches!(e, ShareError::Broken(_) | ShareError::Io { .. });
    if broken && media.share.was_whole() && restart.keeps_ready() {
        restart.status()
    } else {
        None
    }
}

/// Sets [`MEDIA_SHARE_OK`](crate::media_share::MEDIA_SHARE_OK) from a
/// checked snapshot, and logs why the share failed when it did.
fn record_media(snap: &Snapshot, share: &crate::media_share::MediaShare) {
    use crate::media_share::MEDIA_SHARE_OK;
    match &snap.media {
        // Only if no fault latched since the check: its 0 must stand.
        None => share.publish_ok(),
        Some(why) => {
            metrics::gauge!(MEDIA_SHARE_OK).set(0.0);
            tracing::warn!(
                error = %why,
                ready = snap.ready,
                restart = snap.media_restart,
                "readyz: media share unreadable"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use actix_web::{test, App};
    use std::time::Duration;

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

    fn build_app_with_media(
        handle: ReadinessHandle,
        media: &'static crate::media_share::MediaShare,
    ) -> App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        build_app_with_restart(handle, media, None)
    }

    fn build_app_with_restart(
        handle: ReadinessHandle,
        media: &'static crate::media_share::MediaShare,
        restart: Option<&'static crate::self_restart::SelfRestart>,
    ) -> App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        build_app(handle).app_data(web::Data::new(MediaReadiness {
            share: media,
            restart,
        }))
    }

    fn media_ok(recorder: &metrics_util::debugging::DebuggingRecorder) -> Option<f64> {
        use metrics_util::debugging::DebugValue;
        recorder
            .snapshotter()
            .snapshot()
            .into_vec()
            .into_iter()
            .find(|(k, ..)| k.key().name() == crate::media_share::MEDIA_SHARE_OK)
            .map(|(.., v)| match v {
                DebugValue::Gauge(g) => g.into_inner(),
                other => panic!("{other:?}"),
            })
    }

    /// B1531: a share a read found broken makes `/readyz` 503, naming the
    /// cause, and sets the gauge to 0; a readable one stays 200 with it at 1.
    #[actix_web::test]
    async fn readyz_follows_the_media_share_and_sets_its_gauge() {
        let share: &'static crate::media_share::MediaShare = Box::leak(Box::new(
            crate::media_share::MediaShare::new(crate::media_share::ROOT_TIMEOUT),
        ));
        let h = ReadinessHandle::spawn(&["x"]);
        h.mark("x").await.unwrap();
        let app = test::init_service(build_app_with_media(h, share)).await;

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let guard = metrics::set_default_local_recorder(&recorder);
        let req = test::TestRequest::get().uri("/readyz").to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 200);
        assert_eq!(media_ok(&recorder), Some(1.0));
        drop(guard);

        share.mark_broken("reading /m/a: Stale file handle".into());
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let req = test::TestRequest::get().uri("/readyz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 503);
        assert_eq!(media_ok(&recorder), Some(0.0));
        let body = test::read_body(resp).await;
        let txt = std::str::from_utf8(&body).unwrap();
        assert!(txt.contains("Stale file handle"), "got: {txt}");
    }

    /// B1538: a share whose roots hold no entry leaves `/readyz` at 200,
    /// naming the absence, with the gauge at 0.
    #[actix_web::test]
    async fn readyz_stays_up_on_an_absent_share_and_says_so() {
        let share: &'static crate::media_share::MediaShare = Box::leak(Box::new(
            crate::media_share::MediaShare::new(crate::media_share::ROOT_TIMEOUT),
        ));
        let empty = tempfile::tempdir().unwrap();
        share.set_roots(vec![empty.path().to_owned()]);
        let h = ReadinessHandle::spawn(&["x"]);
        h.mark("x").await.unwrap();
        let app = test::init_service(build_app_with_media(h, share)).await;
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let req = test::TestRequest::get().uri("/readyz").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(media_ok(&recorder), Some(0.0));
        let body = test::read_body(resp).await;
        let txt = std::str::from_utf8(&body).unwrap();
        assert!(txt.contains("the media share is absent"), "got: {txt}");
        assert!(!txt.contains("media_restart"), "got: {txt}");
    }

    /// ADR-0156: a pod that read its share and asked for its own restart
    /// stays Ready through the fault, saying so, with the gauge still 0. A
    /// pod that never read its share, or whose request gave up, does not.
    #[actix_web::test]
    async fn readyz_stays_up_while_an_asked_for_restart_is_pending() {
        use crate::self_restart::{SelfRestart, GIVE_UP_AFTER};
        let leak = || -> &'static crate::media_share::MediaShare {
            Box::leak(Box::new(crate::media_share::MediaShare::new(
                crate::media_share::ROOT_TIMEOUT,
            )))
        };
        let get = |share, restart| async move {
            let h = ReadinessHandle::spawn(&["x"]);
            h.mark("x").await.unwrap();
            let app = test::init_service(build_app_with_restart(h, share, restart)).await;
            let req = test::TestRequest::get().uri("/readyz").to_request();
            let resp = test::call_service(&app, req).await;
            let status = resp.status().as_u16();
            let body = test::read_body(resp).await;
            (status, String::from_utf8(body.to_vec()).unwrap())
        };

        let share = leak();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.mkv"), b"x").unwrap();
        share.set_roots(vec![root.path().to_owned()]);
        assert_eq!(get(share, None).await.0, 200, "a whole share is ready");
        share.mark_broken("reading /m/a: Stale file handle".into());
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let guard = metrics::set_default_local_recorder(&recorder);
        let (status, body) = get(share, Some(SelfRestart::requested(GIVE_UP_AFTER))).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"media_restart\":\"requested\""), "{body}");
        assert!(body.contains("Stale file handle"), "{body}");
        assert_eq!(media_ok(&recorder), Some(0.0));
        drop(guard);

        let (status, _) = get(share, Some(SelfRestart::requested(Duration::ZERO))).await;
        assert_eq!(status, 503, "past the give-up time the reaper takes over");

        let never_read = leak();
        never_read.mark_broken("reading /m/a: Stale file handle".into());
        let (status, _) = get(never_read, Some(SelfRestart::requested(GIVE_UP_AFTER))).await;
        assert_eq!(
            status, 503,
            "a pod that never read its share has nothing to keep up"
        );
    }
}
