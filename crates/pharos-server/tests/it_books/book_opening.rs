#![allow(clippy::unwrap_used, clippy::expect_used)]
//! V1448 — `GET /Items/{id}/Opening` serves a book's first prose as plain
//! text without the whole file: bounded, cover and contents pages skipped,
//! unsupported formats 404, every verdict on `pharos_book_opening_total`.

use actix_web::{http::StatusCode, test, web, App};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_core::{
    BookFormat, BookMeta, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore,
    UserId, UserPolicy, UserRecord, UserStore,
};
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

fn page(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body>{body}</body></html>"#
    )
}

/// An EPUB whose spine is `chapters` in order, each `(file name, xhtml)`.
fn write_epub(path: &std::path::Path, chapters: &[(&str, String)]) {
    let mut zw = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zw.start_file("mimetype", stored).unwrap();
    zw.write_all(b"application/epub+zip").unwrap();
    let opts = zip::write::SimpleFileOptions::default();
    zw.start_file("META-INF/container.xml", opts).unwrap();
    zw.write_all(CONTAINER.as_bytes()).unwrap();
    let manifest: String = chapters
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            format!(r#"<item id="c{i}" href="{name}" media-type="application/xhtml+xml"/>"#)
        })
        .collect();
    let spine: String = (0..chapters.len())
        .map(|i| format!(r#"<itemref idref="c{i}"/>"#))
        .collect();
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="u">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Fixture</dc:title><dc:identifier id="u">x</dc:identifier><dc:language>en</dc:language></metadata>
<manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
    );
    zw.start_file("OEBPS/content.opf", opts).unwrap();
    zw.write_all(opf.as_bytes()).unwrap();
    for (name, xhtml) in chapters {
        zw.start_file(format!("OEBPS/{name}"), opts).unwrap();
        zw.write_all(xhtml.as_bytes()).unwrap();
    }
    zw.finish().unwrap();
}

async fn seed(file: &std::path::Path, format: BookFormat) -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
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
    stores
        .put(MediaItem {
            locked: Default::default(),
            id: 42,
            path: file.to_path_buf(),
            title: "Fixture".into(),
            kind: MediaKind::Book,
            book: Some(BookMeta {
                format,
                ..Default::default()
            }),
            probe: MediaProbe::default(),
            series: None,
            created_at: Some(1_700_000_000),
            metadata: Default::default(),
            has_primary_art: false,
            art_version: 0,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
            edition: None,
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string())
}

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state)
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await
    };
}

macro_rules! get {
    ($app:expr, $token:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::get()
                .uri(&format!("/Items/42/Opening?api_key={}", $token))
                .to_request(),
        )
        .await;
        let status = resp.status();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
        (status, ct, body)
    }};
}

fn opening_total(snapshotter: &pharos_test_metrics::CumulativeSnapshotter, outcome: &str) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_book_opening_total"
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

const PROSE: &str = "It was a bright cold day in the made-up harbour town, and the clocks \
    were striking an hour nobody could agree on.";

/// Disarm check: drop the length filter in `epub_opening` and the cover page
/// (an image only) still yields nothing, but the title page text leaks in.
#[actix_web::test]
async fn a_cover_title_and_contents_page_are_skipped() {
    let td = TempDir::new().unwrap();
    let file = td.path().join("fixture.epub");
    write_epub(
        &file,
        &[
            (
                "cover.xhtml",
                page(r#"<div><img src="cover.jpg" alt="Cover"/></div>"#),
            ),
            ("title.xhtml", page("<h1>Fixture</h1><p>A. Writer</p>")),
            (
                "toc.xhtml",
                page(r#"<nav><ol><li>One</li><li>Two</li></ol></nav>"#),
            ),
            (
                "ch1.xhtml",
                page(&format!(
                    "<h2>Chapter One</h2><p>{PROSE}</p><p>Second &amp; last paragraph, with an \
                     entity and enough words to stand as prose by itself.</p>"
                )),
            ),
        ],
    );
    let (state, token) = seed(&file, BookFormat::Epub).await;
    let app = app!(state);
    let (status, ct, body) = get!(app, token);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "text/plain; charset=utf-8");
    assert!(body.starts_with("Chapter One\n\n"), "{body:?}");
    assert!(body.contains(PROSE), "{body:?}");
    assert!(body.contains("\n\nSecond & last paragraph"), "{body:?}");
    assert!(!body.contains("A. Writer"), "{body:?}");
    assert!(!body.contains("Two"), "contents list leaked: {body:?}");
}

#[actix_web::test]
async fn a_long_book_is_cut_at_the_bound_on_a_word() {
    let td = TempDir::new().unwrap();
    let file = td.path().join("fixture.epub");
    let words = "lantern ".repeat(2000);
    write_epub(
        &file,
        &[
            ("a.xhtml", page(&format!("<p>{words}</p>"))),
            ("b.xhtml", page(&format!("<p>{words}</p>"))),
        ],
    );
    let (state, token) = seed(&file, BookFormat::Epub).await;
    let app = app!(state);
    let (status, _, body) = get!(app, token);
    assert_eq!(status, StatusCode::OK);
    let n = body.chars().count();
    assert!(n <= 6000 && n > 5900, "{n} chars");
    assert!(body.ends_with("lantern…"), "{:?}", &body[body.len() - 20..]);
}

#[actix_web::test]
async fn a_text_book_serves_its_first_characters() {
    let td = TempDir::new().unwrap();
    let file = td.path().join("fixture.txt");
    let long = format!(
        "{PROSE}\r\n\r\n\r\n\r\nSecond paragraph.\r\n{}",
        "word ".repeat(5000)
    );
    std::fs::write(&file, &long).unwrap();
    let (state, token) = seed(&file, BookFormat::Unreadable).await;
    let app = app!(state);
    let (status, ct, body) = get!(app, token);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "text/plain; charset=utf-8");
    assert!(
        body.starts_with(&format!("{PROSE}\n\nSecond paragraph.\nword word")),
        "{:?}",
        &body[..120]
    );
    assert!(body.chars().count() <= 6000);
}

#[actix_web::test]
async fn a_pdf_has_no_opening_and_an_epub_without_prose_has_none_either() {
    let td = TempDir::new().unwrap();
    let pdf = td.path().join("fixture.pdf");
    std::fs::write(&pdf, b"%PDF-1.4 not really").unwrap();
    let (state, token) = seed(&pdf, BookFormat::Pdf).await;
    let app = app!(state);
    assert_eq!(get!(app, token).0, StatusCode::NOT_FOUND);

    let covers = td.path().join("covers.epub");
    write_epub(
        &covers,
        &[("cover.xhtml", page(r#"<img src="cover.jpg" alt=""/>"#))],
    );
    let (state, token) = seed(&covers, BookFormat::Epub).await;
    let app = app!(state);
    assert_eq!(get!(app, token).0, StatusCode::NOT_FOUND);

    let broken = td.path().join("broken.epub");
    std::fs::write(&broken, b"not a zip").unwrap();
    let (state, token) = seed(&broken, BookFormat::Epub).await;
    let app = app!(state);
    assert_eq!(get!(app, token).0, StatusCode::NOT_FOUND);
}

/// Disarm check: remove the `counter!` in `record` and every total reads 0.
#[actix_web::test]
async fn every_verdict_is_counted() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let td = TempDir::new().unwrap();

    let txt = td.path().join("a.txt");
    std::fs::write(&txt, PROSE).unwrap();
    let pdf = td.path().join("a.pdf");
    std::fs::write(&pdf, b"%PDF").unwrap();
    let empty = td.path().join("empty.epub");
    write_epub(&empty, &[("c.xhtml", page("<p>Hi</p>"))]);
    let broken = td.path().join("broken.epub");
    std::fs::write(&broken, b"nope").unwrap();

    for (file, format) in [
        (&txt, BookFormat::Unreadable),
        (&txt, BookFormat::Unreadable),
        (&pdf, BookFormat::Pdf),
        (&empty, BookFormat::Epub),
        (&broken, BookFormat::Epub),
    ] {
        let (state, token) = seed(file, format).await;
        let app = app!(state);
        get!(app, token);
    }
    assert_eq!(opening_total(&snapshotter, "served"), 2);
    assert_eq!(opening_total(&snapshotter, "unsupported"), 1);
    assert_eq!(opening_total(&snapshotter, "no_prose"), 1);
    assert_eq!(opening_total(&snapshotter, "unreadable"), 1);
}

#[actix_web::test]
async fn a_request_without_a_token_is_refused() {
    let td = TempDir::new().unwrap();
    let txt = td.path().join("a.txt");
    std::fs::write(&txt, PROSE).unwrap();
    let (state, _) = seed(&txt, BookFormat::Unreadable).await;
    let app = app!(state);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/Items/42/Opening")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
