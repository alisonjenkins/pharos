//! Per-client latency for the media routes (B1398, V1368).
//!
//! `http_request_duration_seconds{method,path}` cannot say WHICH client waited
//! on a slow segment, and every client shares one NAT address, so the
//! `pharos_media_request_seconds{route,client,cache}` histogram splits media
//! latency by the user-agent class ([`ClientClass`]) and by how the cache
//! satisfied the request. It is recorded by `RedMetrics` for every media
//! route; a handler that asked the segment cache reports the outcome with
//! [`note_segment_served`], everything else is `cache="none"`.

use std::time::Duration;

use actix_web::{HttpMessage, HttpRequest};
use pharos_cache::hls_cache::SegmentServed;

use crate::client_class::ClientClass;

/// Bounded `route` label: the kind of media request, never the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaRoute {
    Playlist,
    Init,
    Segment,
    AudioFile,
    Stream,
}

impl MediaRoute {
    pub const ALL: [Self; 5] = [
        Self::Playlist,
        Self::Init,
        Self::Segment,
        Self::AudioFile,
        Self::Stream,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Init => "init",
            Self::Segment => "segment",
            Self::AudioFile => "audio_file",
            Self::Stream => "stream",
        }
    }

    /// The media route a registered route pattern (`match_pattern`) belongs
    /// to; `None` for everything that is not HLS or a direct stream.
    pub fn from_pattern(pattern: &str) -> Option<Self> {
        if pattern.contains("/audio/{id}/stream") || pattern.contains("/videos/{id}/stream") {
            return Some(Self::Stream);
        }
        if !pattern.contains("/videos/{id}/") {
            return None;
        }
        if pattern.contains("/vp9/audio/{name}") {
            Some(Self::AudioFile)
        } else if pattern.ends_with(".m3u8") {
            Some(Self::Playlist)
        } else if pattern.ends_with("init.mp4") {
            Some(Self::Init)
        } else if pattern.ends_with(".ts") || pattern.ends_with(".m4s") {
            Some(Self::Segment)
        } else {
            None
        }
    }
}

/// Bounded `cache` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    Hit,
    Coalesced,
    Miss,
    /// The handler never asked the segment cache (playlists, direct stream).
    None,
}

impl CacheOutcome {
    pub const ALL: [Self; 4] = [Self::Hit, Self::Coalesced, Self::Miss, Self::None];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Coalesced => "coalesced",
            Self::Miss => "miss",
            Self::None => "none",
        }
    }
}

impl From<SegmentServed> for CacheOutcome {
    fn from(s: SegmentServed) -> Self {
        match s {
            SegmentServed::Hit => Self::Hit,
            SegmentServed::Coalesced => Self::Coalesced,
            SegmentServed::Miss => Self::Miss,
        }
    }
}

/// Request-extension slot the handler fills and `RedMetrics` reads back.
#[derive(Clone, Copy)]
struct ServedOutcome(CacheOutcome);

/// The cache outcome a handler recorded on this request, `None` if it never
/// asked the cache.
pub fn cache_outcome(req: &HttpRequest) -> CacheOutcome {
    req.extensions()
        .get::<ServedOutcome>()
        .map_or(CacheOutcome::None, |s| s.0)
}

/// An interactive segment (or init) was just served: hand the cache outcome to
/// `RedMetrics` and log it inside the HTTP request span, where the user agent
/// and request id are known (the cache's own lines run in a background span).
/// `waited` is how long the handler waited on the segment cache.
pub fn note_segment_served(req: &HttpRequest, seg: u32, served: SegmentServed, waited: Duration) {
    let cache = CacheOutcome::from(served);
    req.extensions_mut().insert(ServedOutcome(cache));
    let route = req
        .match_pattern()
        .as_deref()
        .and_then(MediaRoute::from_pattern)
        .unwrap_or(MediaRoute::Segment);
    tracing::info!(
        client = ClientClass::of(req).label(),
        route = route.label(),
        seg,
        cache = cache.label(),
        elapsed_ms = waited.as_millis() as u64,
        "media segment served"
    );
}

/// Record one finished media request. Only successful responses: a fast 404
/// would drag a client's latency down, and failures are already counted by
/// `http_requests_total{status}`.
pub fn record_request(route: MediaRoute, client: ClientClass, cache: CacheOutcome, secs: f64) {
    metrics::histogram!(
        "pharos_media_request_seconds",
        "route" => route.label(),
        "client" => client.label(),
        "cache" => cache.label(),
    )
    .record(secs);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::middleware::RedMetrics;
    use actix_web::{test, web, App, HttpResponse};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    /// A tracing callsite first hit by a parallel test with no subscriber
    /// caches "never interested"; tests that reach the serve line take turns.
    static CALLSITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn histogram_samples(
        snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
        labels: &[(&str, &str)],
    ) -> usize {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, _, _, _)| k.key().name() == "pharos_media_request_seconds")
            .filter(|(k, _, _, _)| {
                labels
                    .iter()
                    .all(|(n, v)| k.key().labels().any(|l| l.key() == *n && l.value() == *v))
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Histogram(h) => h.len(),
                _ => 0,
            })
            .sum()
    }

    #[::core::prelude::v1::test]
    fn every_registered_media_pattern_maps_to_its_route() {
        let cases = [
            ("/videos/{id}/master.m3u8", MediaRoute::Playlist),
            ("/videos/{id}/main.m3u8", MediaRoute::Playlist),
            ("/videos/{id}/variants/{variant}.m3u8", MediaRoute::Playlist),
            ("/videos/{id}/subtitles/{idx}.m3u8", MediaRoute::Playlist),
            ("/videos/{id}/vp9/audio.m3u8", MediaRoute::Playlist),
            (
                "/videos/{id}/h264cmaf/{rung}/main.m3u8",
                MediaRoute::Playlist,
            ),
            ("/videos/{id}/vp9/init.mp4", MediaRoute::Init),
            ("/videos/{id}/h264cmaf/{rung}/init.mp4", MediaRoute::Init),
            ("/videos/{id}/hls1/main/{seg}.ts", MediaRoute::Segment),
            ("/videos/{id}/hls1/{variant}/{seg}.ts", MediaRoute::Segment),
            ("/videos/{id}/vp9/{seg}.m4s", MediaRoute::Segment),
            (
                "/videos/{id}/h264cmaf/{rung}/{seg}.m4s",
                MediaRoute::Segment,
            ),
            ("/videos/{id}/vp9/audio/{name}", MediaRoute::AudioFile),
            ("/videos/{id}/stream", MediaRoute::Stream),
            ("/videos/{id}/stream.{ext}", MediaRoute::Stream),
            ("/audio/{id}/stream", MediaRoute::Stream),
        ];
        for (pattern, want) in cases {
            assert_eq!(MediaRoute::from_pattern(pattern), Some(want), "{pattern}");
        }
        for not_media in ["/items/{id}", "/videos/{id}/activeencodings", "unmatched"] {
            assert_eq!(MediaRoute::from_pattern(not_media), None, "{not_media}");
        }
    }

    #[::core::prelude::v1::test]
    fn media_labels_are_distinct() {
        let routes: std::collections::BTreeSet<_> =
            MediaRoute::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(routes.len(), MediaRoute::ALL.len());
        let caches: std::collections::BTreeSet<_> =
            CacheOutcome::ALL.iter().map(|c| c.label()).collect();
        assert_eq!(caches.len(), CacheOutcome::ALL.len());
        for s in SegmentServed::ALL {
            assert_ne!(CacheOutcome::from(s), CacheOutcome::None);
        }
    }

    /// The histogram carries the route, the client class and the cache outcome
    /// the handler reported — the split the dashboards are built on.
    // The lock only orders tests; the runtime is single-threaded, so holding
    // it across an await cannot deadlock.
    #[allow(clippy::await_holding_lock)]
    #[actix_web::test]
    async fn a_served_segment_is_timed_by_route_client_and_cache() {
        let _serial = CALLSITE.lock().unwrap_or_else(|e| e.into_inner());
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/videos/{id}/hls1/main/{seg}.ts",
            web::get().to(|req: HttpRequest| async move {
                note_segment_served(&req, 3, SegmentServed::Coalesced, Duration::from_millis(7));
                HttpResponse::Ok().body("seg")
            }),
        ))
        .await;
        let req = test::TestRequest::get()
            .uri("/videos/7/hls1/main/3.ts")
            .insert_header(("User-Agent", "Pharos Android TV"))
            .to_request();
        assert!(test::call_service(&app, req).await.status().is_success());

        let got = histogram_samples(
            &snapshotter,
            &[
                ("route", "segment"),
                ("client", "pharos_android"),
                ("cache", "coalesced"),
            ],
        );
        assert_eq!(got, 1, "one sample under the reported labels");
    }

    /// A route whose handler never asks the cache is `cache="none"`, and a
    /// route that is not media is not recorded at all.
    #[actix_web::test]
    async fn uncached_media_is_none_and_other_routes_are_not_recorded() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = test::init_service(
            App::new()
                .wrap(RedMetrics)
                .route(
                    "/videos/{id}/master.m3u8",
                    web::get().to(|| async { HttpResponse::Ok().body("#EXTM3U") }),
                )
                .route(
                    "/items/{id}",
                    web::get().to(|| async { HttpResponse::Ok().finish() }),
                ),
        )
        .await;
        for (uri, ua) in [
            ("/videos/7/master.m3u8", "Mozilla/5.0 (X11; Linux x86_64)"),
            ("/items/7", "Mozilla/5.0 (X11; Linux x86_64)"),
        ] {
            let req = test::TestRequest::get()
                .uri(uri)
                .insert_header(("User-Agent", ua))
                .to_request();
            test::call_service(&app, req).await;
        }
        assert_eq!(
            histogram_samples(
                &snapshotter,
                &[
                    ("route", "playlist"),
                    ("client", "browser"),
                    ("cache", "none")
                ]
            ),
            1
        );
        assert_eq!(
            histogram_samples(&snapshotter, &[]),
            1,
            "only the media request is recorded"
        );
    }

    /// A failed response is not latency: it would pull a client's p95 down.
    #[actix_web::test]
    async fn a_failed_media_response_is_not_timed() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let app = test::init_service(App::new().wrap(RedMetrics).route(
            "/videos/{id}/hls1/main/{seg}.ts",
            web::get().to(|| async { HttpResponse::NotFound().finish() }),
        ))
        .await;
        let req = test::TestRequest::get()
            .uri("/videos/7/hls1/main/3.ts")
            .to_request();
        test::call_service(&app, req).await;
        assert_eq!(histogram_samples(&snapshotter, &[]), 0);
    }

    /// The per-serve INFO line carries the client, route, segment, cache
    /// outcome and wait, from the request handler.
    // The lock only orders tests; the runtime is single-threaded, so holding
    // it across an await cannot deadlock.
    #[allow(clippy::await_holding_lock)]
    #[actix_web::test]
    async fn a_served_segment_is_logged_with_client_route_and_cache() {
        let _serial = CALLSITE.lock().unwrap_or_else(|e| e.into_inner());
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _g = tracing::subscriber::set_default(sub);
        let app = test::init_service(App::new().route(
            "/videos/{id}/vp9/{seg}.m4s",
            web::get().to(|req: HttpRequest| async move {
                note_segment_served(&req, 12, SegmentServed::Miss, Duration::from_millis(250));
                HttpResponse::Ok().finish()
            }),
        ))
        .await;
        let req = test::TestRequest::get()
            .uri("/videos/7/vp9/12.m4s")
            .insert_header(("User-Agent", "okhttp/4.12.0"))
            .to_request();
        test::call_service(&app, req).await;

        let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = log
            .lines()
            .find(|l| l.contains("media segment served"))
            .unwrap_or_else(|| panic!("no serve line in {log}"));
        for want in [
            "INFO",
            "client=\"jellyfin_android\"",
            "route=\"segment\"",
            "seg=12",
            "cache=\"miss\"",
            "elapsed_ms=250",
        ] {
            let want = want.replace('"', "");
            assert!(
                line.replace('"', "").contains(&want),
                "missing {want} in {line}"
            );
        }
    }
}
