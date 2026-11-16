#![allow(clippy::unwrap_used, clippy::expect_used)]
//! V1460 / ADR-0144 — `GET /Items/{id}/Download?pages=pictures` on a PDF
//! serves its pages drawn as pictures in a CBZ, fitted to `maxWidth` and
//! `maxHeight`, for a reader with no PDF engine. A request without it, and
//! any other format, is served as before.
//!
//! The PDF is built here from drawing operators; no real title or file is used.

use std::io::{Cursor, Read};

use actix_web::{http::StatusCode, test, web, App};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_core::{
    BookFormat, BookMeta, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore,
    UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    book_convert::ConvertedBookCache,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use tempfile::TempDir;
use zip::ZipArchive;

/// A PDF of `pages` US-letter pages, each with a black square.
fn pdf(pages: usize) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    let mut object = |out: &mut Vec<u8>, body: String| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", offsets.len()).as_bytes());
    };
    object(&mut out, "<< /Type /Catalog /Pages 2 0 R >>".to_owned());
    let kids: String = (0..pages)
        .map(|i| format!("{} 0 R", 3 + 2 * i))
        .collect::<Vec<_>>()
        .join(" ");
    object(
        &mut out,
        format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>"),
    );
    for i in 0..pages {
        object(
            &mut out,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R >>",
                4 + 2 * i
            ),
        );
        let content = "0 0 0 rg 72 72 200 200 re f";
        object(
            &mut out,
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        );
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for at in &offsets {
        out.extend_from_slice(format!("{at:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    out
}

struct Fixture {
    state: web::Data<AppState>,
    token: String,
    original: Vec<u8>,
    _dir: TempDir,
}

async fn seed(book: Vec<u8>, name: &str, format: BookFormat, with_cache: bool) -> Fixture {
    let dir = TempDir::new().unwrap();
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
    let path = dir.path().join(name);
    std::fs::write(&path, &book).unwrap();
    stores
        .put(MediaItem {
            locked: Default::default(),
            id: 42,
            path,
            title: "Synthetic".into(),
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
    let mut state = AppState::new(stores, "srv".into());
    if with_cache {
        state = state.with_book_cache(ConvertedBookCache::new(dir.path().join("books")));
    }
    Fixture {
        state: web::Data::new(state),
        token: token.0.expose().to_string(),
        original: book,
        _dir: dir,
    }
}

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state.clone())
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await
    };
}

macro_rules! download {
    ($app:expr, $token:expr, $params:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::get()
                .uri(&format!("/Items/42/Download?api_key={}{}", $token, $params))
                .to_request(),
        )
        .await;
        let status = resp.status();
        let kind = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        (status, kind, test::read_body(resp).await.to_vec())
    }};
}

fn pages_count(snapshotter: &pharos_test_metrics::CumulativeSnapshotter, outcome: &str) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            k.key().name() == "pharos_book_pages_total"
                && k.key()
                    .labels()
                    .any(|l| l.key() == "outcome" && l.value() == outcome)
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(n) => n,
            other => panic!("{other:?}"),
        })
        .sum()
}

const ASK: &str = "&pages=pictures&maxWidth=600&maxHeight=800";

/// Disarm check: drop the `pages` branch in `serve_item_file` and the PDF
/// itself comes back, failing the zip assertion.
#[actix_web::test]
async fn a_pdf_asked_for_as_pictures_is_a_cbz_of_its_pages_in_the_box() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let f = seed(pdf(3), "Synthetic.pdf", BookFormat::Pdf, true).await;
    let app = app!(f.state);

    let (status, kind, body) = download!(app, f.token, ASK);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kind, "application/vnd.comicbook+zip");
    let mut zip = ZipArchive::new(Cursor::new(body.clone())).unwrap();
    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index_raw(i).unwrap().name().unwrap().into_owned())
        .collect();
    assert_eq!(names, ["page-0001.png", "page-0002.png", "page-0003.png"]);
    let mut page = Vec::new();
    zip.by_index(0).unwrap().read_to_end(&mut page).unwrap();
    let page = image::load_from_memory(&page).unwrap();
    assert!(page.width() <= 600 && page.height() <= 800);
    assert_eq!(pages_count(&snapshotter, "drawn"), 1);

    let (_, _, again) = download!(app, f.token, ASK);
    assert_eq!(again, body, "the same copy");
    assert_eq!(pages_count(&snapshotter, "drawn"), 1, "drawn once");
    assert_eq!(pages_count(&snapshotter, "cache_hit"), 1);
}

/// The reader fetches in pieces; each is a range of the one copy.
#[actix_web::test]
async fn a_piece_of_the_pictures_is_a_range_of_the_same_copy() {
    let f = seed(pdf(2), "Synthetic.pdf", BookFormat::Pdf, true).await;
    let app = app!(f.state);
    let (_, _, whole) = download!(app, f.token, ASK);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items/42/Download?api_key={}{ASK}", f.token))
            .insert_header(("Range", "bytes=10-"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    let piece = test::read_body(resp).await.to_vec();
    assert_eq!(piece, whole[10..]);
}

#[actix_web::test]
async fn a_plain_download_of_a_pdf_is_the_pdf() {
    let f = seed(pdf(1), "Synthetic.pdf", BookFormat::Pdf, true).await;
    let app = app!(f.state);
    let (status, kind, body) = download!(app, f.token, "&maxWidth=600&maxHeight=800");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kind, "application/pdf");
    assert_eq!(body, f.original);
}

#[actix_web::test]
async fn a_pdf_that_does_not_open_is_refused_with_the_reason_and_counted() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let f = seed(
        b"not a pdf".to_vec(),
        "Synthetic.pdf",
        BookFormat::Pdf,
        true,
    )
    .await;
    let app = app!(f.state);
    let (status, _, body) = download!(app, f.token, ASK);
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("open the PDF"), "{body}");
    assert!(!body.contains("Synthetic"), "no path in the answer: {body}");
    assert_eq!(pages_count(&snapshotter, "failed"), 1);
}

#[actix_web::test]
async fn with_no_book_cache_pictures_are_unavailable() {
    let f = seed(pdf(1), "Synthetic.pdf", BookFormat::Pdf, false).await;
    let app = app!(f.state);
    let (status, _, _) = download!(app, f.token, ASK);
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
