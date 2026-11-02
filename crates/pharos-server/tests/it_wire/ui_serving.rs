//! pharos-ui served under `/ui/` (spec 009 contracts/serving.md, V204, V219):
//! the SPA's routes fall back to `index.html` for a browser, `/ui` moves to
//! `/ui/`, and `/` stays the core root.

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

/// A browser's deep link under `/ui/` gets the SPA; `/ui/` is the index. The
/// same path outside the mount is no UI route. Disarm check: record nothing
/// in `index` and the counter assertion reads 0.
#[actix_web::test]
async fn the_spa_is_served_under_ui_only() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let dir = tempfile::tempdir().unwrap();
    let app = app!(bundle(dir.path()));
    for uri in [
        "/ui/",
        "/ui/browse/x",
        "/ui/title/123?sort=name",
        "/UI/Browse/ABC",
    ] {
        let resp = test::call_service(&app, html(uri).to_request()).await;
        assert_eq!(resp.status(), 200, "{uri}");
        assert_eq!(cache_control(&resp), "no-cache", "{uri}");
        assert_eq!(test::read_body(resp).await, INDEX.as_bytes(), "{uri}");
    }
    assert_eq!(served_total(&snapshotter, "index"), 4);

    // `/` is the core root, not the UI, and a root deep link is no UI route.
    let resp = test::call_service(&app, html("/").to_request()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(test::read_body(resp).await, "pharos".as_bytes());
    let resp = test::call_service(&app, html("/browse/x").to_request()).await;
    assert_eq!(resp.status(), 404);
    assert_ne!(test::read_body(resp).await, INDEX.as_bytes());
    assert_eq!(served_total(&snapshotter, "index"), 4);
}

/// The fallback never answers an API client, a non-GET or a file-shaped path
/// with HTML, and an API route keeps answering itself. Disarm check: drop the
/// `Accept` test and `/ui/title/1` as JSON reads 200.
#[actix_web::test]
async fn api_routes_and_api_clients_never_get_the_spa() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let dir = tempfile::tempdir().unwrap();
    let app = app!(bundle(dir.path()));

    let resp = test::call_service(&app, json("/ui/title/1").to_request()).await;
    assert_eq!(resp.status(), 404);
    let resp = test::call_service(&app, html("/ui/no/such/thing.json").to_request()).await;
    assert_eq!(
        resp.status(),
        404,
        "a path with an extension is a file, not a route"
    );
    let resp = test::call_service(&app, html("/ui/assets/missing.js").to_request()).await;
    assert_eq!(resp.status(), 404);
    let resp = test::call_service(
        &app,
        test::TestRequest::post().uri("/ui/title/1").to_request(),
    )
    .await;
    assert_eq!(resp.status(), 404);
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
            .uri("/ui/assets/pharos-ui_bg-dxhA1b2.wasm")
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
        test::TestRequest::get().uri("/ui/favicon.ico").to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(cache_control(&resp), "no-cache");
    // Unhashed, so a new release must reach the cache under the same URL.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/ui/assets/pharos.css")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(cache_control(&resp), "no-cache");
    assert_eq!(served_total(&snapshotter, "asset"), 1);
    assert_eq!(served_total(&snapshotter, "file"), 2);
}

/// `/ui` answers 308 to `/ui/`, query kept. Disarm check: drop `record` from
/// `redirect` and the counter reads 0.
#[actix_web::test]
async fn the_bare_mount_redirects_to_the_slash() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let dir = tempfile::tempdir().unwrap();
    let app = app!(bundle(dir.path()));
    for (from, to) in [("/ui", "/ui/"), ("/ui?t=30", "/ui/?t=30")] {
        let resp = test::call_service(&app, html(from).to_request()).await;
        assert_eq!(resp.status(), 308, "{from}");
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), to, "{from}");
    }
    assert_eq!(served_total(&snapshotter, "redirect"), 2);
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
        "/ui/../secret.txt",
        "/ui/%2e%2e/secret.txt",
        "/ui/assets/..%2f..%2fsecret.txt",
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
    for uri in ["/ui/", "/ui/title/1", "/ui/assets/app.js"] {
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
        ("308 redirect", html("/ui").to_request(), 308),
        (
            "404",
            test::TestRequest::get().uri("/ui/missing.js").to_request(),
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
