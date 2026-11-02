//! pharos-ui served at the root (spec 009 contracts/serving.md, V204): the
//! SPA's routes fall back to `index.html` for a browser, never shadow an API
//! route, and `/ui/*` moves to `/*`.

use actix_web::{http::header, test, App};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_server::{config::NativeApiConfig, middleware::LowercasePath, router, ui};
use std::path::Path;
use std::sync::Arc;

const INDEX: &str = "<!doctype html><title>pharos-ui</title>";
const WASM: &[u8] = b"\0asm-bytes";

/// A bundle laid out like `dx build`'s `public/`.
fn bundle(dir: &Path) -> Arc<ui::UiBundle> {
    std::fs::write(dir.join("index.html"), INDEX).unwrap();
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("assets/pharos-ui_bg-dxhA1b2.wasm"), WASM).unwrap();
    std::fs::write(dir.join("assets/pharos.css"), "body{}").unwrap();
    std::fs::write(dir.join("favicon.ico"), "ico").unwrap();
    Arc::new(ui::UiBundle::load(dir).unwrap())
}

/// The production app's routes, from the composition `main` uses.
macro_rules! app {
    ($bundle:expr) => {
        app!($bundle, true)
    };
    ($bundle:expr, $native_api:expr) => {
        test::init_service(
            App::new()
                .wrap(LowercasePath)
                .configure(router::configure_app(
                    Some($bundle),
                    NativeApiConfig {
                        enabled: $native_api,
                        ..NativeApiConfig::default()
                    },
                    pharos_server::api::pharos::rpc::limits::ConnectionLimits::default(),
                )),
        )
        .await
    };
}

fn html(uri: &str) -> test::TestRequest {
    test::TestRequest::get()
        .uri(uri)
        .insert_header((header::ACCEPT, "text/html,application/xhtml+xml,*/*;q=0.8"))
}

fn json(uri: &str) -> test::TestRequest {
    test::TestRequest::get()
        .uri(uri)
        .insert_header((header::ACCEPT, "application/json"))
}

fn cache_control(resp: &actix_web::dev::ServiceResponse) -> String {
    resp.headers()
        .get(header::CACHE_CONTROL)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

fn served_total(snapshotter: &pharos_test_metrics::CumulativeSnapshotter, outcome: &str) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_ui_requests_total"
                && key
                    .labels()
                    .any(|l| l.key() == "outcome" && l.value() == outcome)
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(n) => n,
            other => panic!("{other:?}"),
        })
        .sum()
}

/// Every first segment the UI routes (spec 009 data-model §Route) reaches the
/// SPA through the whole API router, in any case. Disarm check: register
/// `/title/{id}` as an API route and this names it.
#[actix_web::test]
async fn no_api_route_shadows_a_ui_route() {
    let dir = tempfile::tempdir().unwrap();
    let app = app!(bundle(dir.path()));
    let mut shadowed = Vec::new();
    for first in ui::UI_ROUTES {
        for uri in [
            format!("/{first}"),
            format!("/{first}/123?sort=name"),
            format!("/{}/ABC", first.to_uppercase()),
        ] {
            let resp = test::call_service(&app, html(&uri).to_request()).await;
            let status = resp.status();
            let body = test::read_body(resp).await;
            if status != 200 || body != INDEX.as_bytes() {
                shadowed.push(format!("{uri}: {status}"));
            }
        }
    }
    let resp = test::call_service(&app, html("/").to_request()).await;
    assert_eq!(cache_control(&resp), "no-cache");
    assert_eq!(test::read_body(resp).await, INDEX.as_bytes());
    assert!(shadowed.is_empty(), "{}", shadowed.join("\n"));
}

/// The fallback never answers an API route or an API client with HTML.
/// Disarm check: drop the `Accept` test and `/title/1` as JSON reads 200.
#[actix_web::test]
async fn api_routes_and_api_clients_never_get_the_spa() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let dir = tempfile::tempdir().unwrap();
    let app = app!(bundle(dir.path()));

    let resp = test::call_service(&app, json("/title/1").to_request()).await;
    assert_eq!(resp.status(), 404);
    let resp = test::call_service(&app, html("/no/such/thing.json").to_request()).await;
    assert_eq!(
        resp.status(),
        404,
        "a path with an extension is a file, not a route"
    );
    let resp = test::call_service(&app, html("/assets/missing.js").to_request()).await;
    assert_eq!(resp.status(), 404);
    let resp =
        test::call_service(&app, test::TestRequest::post().uri("/title/1").to_request()).await;
    assert_ne!(resp.status(), 200);
    // An API route keeps answering itself, even to a browser.
    let resp = test::call_service(&app, html("/metrics").to_request()).await;
    assert_eq!(resp.status(), 200);
    assert_ne!(test::read_body(resp).await, INDEX.as_bytes());
    let resp = test::call_service(&app, html("/pharos/rpc").to_request()).await;
    assert_ne!(test::read_body(resp).await, INDEX.as_bytes());
    assert_eq!(served_total(&snapshotter, "not_found"), 4);
}

/// pharos-ui tells "native API off" from "on, but the socket was refused"
/// by `GET /pharos/session`: 404 off, 405 on. Its probe asks for JSON, so the
/// SPA fallback never answers it. Disarm check: route `GET /pharos/session`
/// and the enabled arm stops reading 405.
#[actix_web::test]
async fn the_session_probe_says_whether_the_native_api_is_on() {
    for (enabled, want) in [(false, 404), (true, 405)] {
        let dir = tempfile::tempdir().unwrap();
        let app = app!(bundle(dir.path()), enabled);
        let resp = test::call_service(&app, json("/pharos/session").to_request()).await;
        assert_eq!(resp.status(), want, "native api enabled = {enabled}");
    }
}

/// Content-hashed assets cache forever; everything else, unhashed files
/// under `assets/` included, revalidates. A path the
/// LowercasePath middleware folded still finds its file. Disarm check: serve
/// assets with `no-cache` and the immutable assertion fails.
#[actix_web::test]
async fn assets_are_immutable_and_survive_lowercasing() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let dir = tempfile::tempdir().unwrap();
    let app = app!(bundle(dir.path()));

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/assets/pharos-ui_bg-dxhA1b2.wasm")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(cache_control(&resp), "public, max-age=31536000, immutable");
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/wasm"
    );
    assert_eq!(test::read_body(resp).await, WASM);

    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/favicon.ico").to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(cache_control(&resp), "no-cache");
    // Unhashed, so a new release must reach the cache under the same URL.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/assets/pharos.css")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(cache_control(&resp), "no-cache");
    assert_eq!(served_total(&snapshotter, "asset"), 1);
    assert_eq!(served_total(&snapshotter, "file"), 2);
}

/// The old mount moves: `/ui/<rest>` answers 308 to `/<rest>`, query kept.
#[actix_web::test]
async fn the_ui_mount_redirects_to_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let app = app!(bundle(dir.path()));
    for (from, to) in [
        ("/ui", "/"),
        ("/ui/", "/"),
        ("/ui/title/7?t=30", "/title/7?t=30"),
    ] {
        let resp = test::call_service(&app, html(from).to_request()).await;
        assert_eq!(resp.status(), 308, "{from}");
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), to, "{from}");
    }
}

/// A path outside the bundle is never read, however it is spelled.
#[actix_web::test]
async fn a_path_outside_the_bundle_is_not_served() {
    let outer = tempfile::tempdir().unwrap();
    std::fs::write(outer.path().join("secret.txt"), "secret").unwrap();
    let dir = outer.path().join("public");
    std::fs::create_dir_all(&dir).unwrap();
    let app = app!(bundle(&dir));
    for uri in [
        "/../secret.txt",
        "/%2e%2e/secret.txt",
        "/assets/..%2f..%2fsecret.txt",
    ] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        let body = test::read_body(resp).await;
        assert_ne!(body, "secret".as_bytes(), "{uri}");
    }
}

/// A bundle with no `index.html` is refused at startup, naming the path.
#[actix_web::test]
async fn a_bundle_without_an_index_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let err = ui::UiBundle::load(dir.path()).unwrap_err().to_string();
    assert!(err.contains("index.html"), "{err}");
}

/// Every UI response carries a strict policy: script only from this origin,
/// wasm, and `index.html`'s own inline loader by hash; never framed. Disarm
/// check: drop the hash and the inline loader's `sha256-` source is missing.
#[actix_web::test]
async fn ui_responses_carry_a_strict_content_security_policy() {
    use base64::Engine as _;
    use sha2::Digest as _;
    let loader = "import init from \"/assets/app.js\"; init();";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("index.html"),
        format!(
            "<html><head><script src=\"/assets/x.js\"></script>\
             <script type=\"module\">{loader}</script></head></html>"
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/app.js"), "").unwrap();
    let app = app!(Arc::new(ui::UiBundle::load(dir.path()).unwrap()));
    let want = format!(
        "'sha256-{}'",
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(loader))
    );
    for uri in ["/", "/title/1", "/assets/app.js"] {
        let resp = test::call_service(&app, html(uri).to_request()).await;
        let h = resp.headers();
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap();
        for part in [
            "default-src 'self'",
            "frame-ancestors 'none'",
            "object-src 'none'",
            "base-uri 'none'",
            "connect-src 'self'",
            want.as_str(),
        ] {
            assert!(csp.contains(part), "{uri}: {part} missing from {csp}");
        }
        assert!(!csp.contains("'unsafe-eval'"), "{csp}");
        assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
    }
}

/// A redirect and a 404 carry the same security headers as a served page,
/// not only the files `respond` serves. Disarm check: drop `secured` from
/// `redirect` or `fallback` and that reply's headers go missing.
#[actix_web::test]
async fn redirects_and_not_found_carry_the_security_headers() {
    let dir = tempfile::tempdir().unwrap();
    let app = app!(bundle(dir.path()));
    let replies = [
        ("308 redirect", html("/ui/title/7").to_request(), 308),
        (
            "404",
            test::TestRequest::get().uri("/missing.js").to_request(),
            404,
        ),
    ];
    for (name, req, status) in replies {
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), status, "{name}");
        let h = resp.headers();
        assert!(
            h.get(header::CONTENT_SECURITY_POLICY).is_some(),
            "{name}: CSP"
        );
        assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY", "{name}");
        assert_eq!(
            h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff",
            "{name}"
        );
        assert_eq!(
            h.get(header::REFERRER_POLICY).unwrap(),
            "same-origin",
            "{name}"
        );
    }
}
