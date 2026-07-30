#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 004-books (T049) — the whole chain, scan to wire, with a real epub on disk.
//!
//! This is the automatable part of quickstart.md §1–§6. Every other book test
//! seeds the store directly, which means none of them prove the SCAN produces a
//! row a client can act on. Here a real zip is written to a temp directory, a
//! real `FsScanner` walks it, and the resulting rows are served over the real
//! HTTP surface.
//!
//! It stops short of the reader itself: `bookPlayer` is epub.js in a browser, so
//! no in-process Rust test can mount it. That last step is NOT manual though —
//! `compat-playwright/tests/books.spec.ts` (T050) drives real Chromium against
//! unmodified jellyfin-web. This file asserts everything the browser depends ON,
//! so when the Playwright spec fails, these assertions localise it.

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{
    MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_scanner::fs::FsScanner;
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use std::io::Write;
use tempfile::TempDir;

const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

const OPF: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>Dune</dc:title>
    <dc:creator>Frank Herbert</dc:creator>
    <dc:publisher>Chilton Books</dc:publisher>
    <meta name="calibre:series" content="Dune Chronicles"/>
    <meta name="calibre:series_index" content="1"/>
  </metadata>
  <manifest><item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/></manifest>
</package>"#;

/// A spec-shaped epub: uncompressed `mimetype` first, then container, then OPF.
fn write_epub(path: &std::path::Path) {
    let f = std::fs::File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zw.start_file("mimetype", stored).unwrap();
    zw.write_all(b"application/epub+zip").unwrap();
    for (name, body) in [
        ("META-INF/container.xml", CONTAINER),
        ("OEBPS/content.opf", OPF),
        (
            "OEBPS/ch1.xhtml",
            "<html><body><p>A desert planet.</p></body></html>",
        ),
    ] {
        zw.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    zw.finish().unwrap();
}

/// A `Prober` that PANICS if called. The scan below writes real book rows, so
/// this proves SC-002 end to end rather than by counting: if anything routes a
/// book to ffmpeg, the test dies rather than quietly succeeding.
#[derive(Clone, Default)]
struct NeverProber;

impl pharos_core::Prober for NeverProber {
    async fn probe(
        &self,
        path: &std::path::Path,
    ) -> pharos_core::DomainResult<pharos_core::ProbeInfo> {
        panic!(
            "SC-002 — the prober must never see a book, but was handed {}",
            path.display()
        );
    }
}

fn app(
    state: web::Data<AppState>,
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
        .app_data(state)
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
}

macro_rules! auth_header {
    ($token:expr) => {
        (
            "X-Emby-Authorization",
            format!(
                "MediaBrowser Client=\"t\", Device=\"d\", DeviceId=\"i\", Version=\"1\", Token=\"{}\"",
                $token
            ),
        )
    };
}

#[actix_web::test]
async fn a_real_epub_survives_scan_to_wire() {
    let td = TempDir::new().unwrap();
    let epub = td.path().join("Dune.epub");
    write_epub(&epub);
    let epub_bytes = std::fs::read(&epub).unwrap();

    // ---- quickstart §1: the scan imports it, with no ffmpeg anywhere near it.
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let scanned = FsScanner::new(NeverProber)
        .scan_into(td.path(), &stores)
        .await
        .expect("scan must succeed");
    assert_eq!(
        scanned.added.len(),
        1,
        "exactly the epub must be imported: {scanned:?}"
    );

    let all = MediaStore::list(&stores).await.unwrap();
    let book = all
        .iter()
        .find(|i| i.kind == MediaKind::Book)
        .expect("the scan must have produced a Book row");
    assert_eq!(book.title, "Dune", "title comes from the filename stem");
    let bm = book.book.as_ref().expect("a Book row must carry BookMeta");
    assert_eq!(bm.format, pharos_core::BookFormat::Epub);
    assert_eq!(
        bm.author.as_deref(),
        Some("Frank Herbert"),
        "the OPF was actually read during the scan, not just parseable in isolation"
    );
    assert_eq!(bm.series_name.as_deref(), Some("Dune Chronicles"));
    assert_eq!(bm.series_index, Some(1));

    // ---- serve it.
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let token = token.0.expose().to_string();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(app(state)).await;

    // ---- quickstart §3: the three gates, as the client asks for them.
    let req = test::TestRequest::get()
        .uri("/Items?IncludeItemTypes=Book&Fields=CanDownload,Path")
        .insert_header(auth_header!(&token))
        .to_request();
    let body: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;
    let item = &body["Items"][0];

    assert_eq!(item["Type"], "Book");
    assert_eq!(item["MediaType"], "Book", "gate 1");
    let path = item["Path"]
        .as_str()
        .expect("gate 2 — Path must be present");
    assert!(
        path.ends_with("epub"),
        "gate 2 — every reader tests Path against an extension, got {path:?}"
    );
    assert_eq!(item["CanDownload"], true);
    assert_eq!(
        item["MediaSources"].as_array().map(Vec::len),
        Some(0),
        "SC-004 — nothing to transcode"
    );
    assert_eq!(item["RunTimeTicks"], 0);
    // SeriesName / IndexNumber projection is US3's (T068). The BookMeta assertions
    // above already prove the scan READ the series out of the OPF; putting it on
    // the wire is a separate step, and asserting it here would fail for the right
    // reason at the wrong time.

    // ---- quickstart §5: PlaybackInfo offers no source.
    let id = item["Id"].as_str().unwrap().to_string();
    let req = test::TestRequest::post()
        .uri(&format!("/Items/{id}/PlaybackInfo"))
        .insert_header(auth_header!(&token))
        .to_request();
    let pb: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;
    assert_eq!(pb["MediaSources"].as_array().map(Vec::len), Some(0));

    // ---- quickstart §6: gate 3, the bytes. Query auth only, as the reader does.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items/{id}/Download?api_key={token}"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "gate 3");
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/epub+zip")
    );
    let served = test::read_body(resp).await;
    assert_eq!(
        served.as_ref(),
        epub_bytes.as_slice(),
        "the client must receive the epub byte for byte, or epub.js cannot unzip it"
    );

    // The bytes really are a readable epub — the thing bookPlayer will do next.
    let cursor = std::io::Cursor::new(served.to_vec());
    let mut zip = zip::ZipArchive::new(cursor).expect("served bytes must be a valid zip");
    assert!(
        (0..zip.len()).any(|i| zip
            .by_index_raw(i)
            .map(|e| e.name() == "OEBPS/content.opf")
            .unwrap_or(false)),
        "the served archive must still contain its OPF"
    );
}

#[actix_web::test]
async fn a_mobi_is_listed_but_not_presented_as_readable() {
    // The other half of the extension set: indexed and downloadable, never
    // claimed as readable, because no client ships a reader for it.
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("Sideloaded.azw3"), b"not really an azw3").unwrap();

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    FsScanner::new(NeverProber)
        .scan_into(td.path(), &stores)
        .await
        .unwrap();

    let all = MediaStore::list(&stores).await.unwrap();
    let item = all.first().expect("the azw3 must still be imported");
    assert_eq!(item.kind, MediaKind::Book);
    let bm = item.book.as_ref().unwrap();
    assert_eq!(bm.format, pharos_core::BookFormat::Unreadable);
    assert!(
        !bm.format.readable_by_client(),
        "no client can open an azw3, and the model must say so"
    );
}
