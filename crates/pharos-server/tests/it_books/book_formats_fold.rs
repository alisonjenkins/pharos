#![allow(clippy::unwrap_used, clippy::expect_used)]
//! V1391 — one book in several formats side by side is one item: a real scan
//! of a temp folder holding an epub and a pdf of one title writes a single
//! row whose pdf is a version, and the download route serves that version's
//! bytes when the client names it.

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{
    MediaKind, MediaStore, ReadPosition, ReadPositionStore, SecretString, TokenStore,
    UserDataStore, UserId, UserItemData, UserPolicy, UserRecord, UserStore,
};
use pharos_scanner::fs::FsScanner;
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use tempfile::TempDir;

use super::book_end_to_end::{write_epub, NeverProber};

/// A one-page PDF with no info dictionary, so it names no author.
fn write_pdf(path: &std::path::Path) {
    let objects: [&[u8]; 3] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] >>",
    ];
    let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let startxref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    std::fs::write(path, out).unwrap();
}

async fn user(stores: &Stores) -> (UserId, String) {
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
    (uid, token.0.expose().to_string())
}

fn books(all: &[pharos_core::MediaItem]) -> Vec<&pharos_core::MediaItem> {
    all.iter().filter(|i| i.kind == MediaKind::Book).collect()
}

#[actix_web::test]
async fn an_epub_and_a_pdf_of_one_title_are_one_item_with_two_versions() {
    let td = TempDir::new().unwrap();
    let epub = td.path().join("Widget Cookbook.epub");
    let pdf = td.path().join("Widget Cookbook.pdf");
    write_epub(&epub);
    write_pdf(&pdf);
    let pdf_bytes = std::fs::read(&pdf).unwrap();

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    FsScanner::new(NeverProber)
        .scan_into(td.path(), &stores)
        .await
        .expect("scan must succeed");

    let all = MediaStore::list(&stores).await.unwrap();
    let books = books(&all);
    assert_eq!(books.len(), 1, "one tile for one book: {books:#?}");
    let book = books[0];
    assert_eq!(
        book.path, epub,
        "the epub is the primary: the reader opens it best"
    );
    let versions: Vec<_> = book.probe.versions().collect();
    assert_eq!(versions.len(), 1, "{versions:#?}");
    let pdf_version = versions[0];
    assert_eq!(pdf_version.path, pdf);
    assert_eq!(pdf_version.name.as_deref(), Some("PDF"));
    assert_eq!(pdf_version.container.as_deref(), Some("pdf"));
    assert!(
        pdf_version.probe.is_some(),
        "a version carries a probe so a request naming it is served from its file"
    );

    let (_, token) = user(&stores).await;
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/Items/{}/Download?api_key={token}&mediaSourceId={}-{}",
                book.id, book.id, pdf_version.id
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/pdf")
    );
    let served = test::read_body(resp).await;
    assert_eq!(
        served.as_ref(),
        pdf_bytes.as_slice(),
        "the version's own bytes"
    );
}

/// A book already listed on its own keeps its viewers' marks when its other
/// format arrives and it becomes a version: the folded row's state moves to
/// the item that survives.
#[actix_web::test]
async fn a_folded_row_hands_its_marks_to_the_surviving_item() {
    let td = TempDir::new().unwrap();
    let pdf = td.path().join("Widget Cookbook.pdf");
    write_pdf(&pdf);
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let scanner = FsScanner::new(NeverProber);
    scanner.scan_into(td.path(), &stores).await.unwrap();
    let (uid, _) = user(&stores).await;
    let pdf_row = stores
        .id_at_path(&pdf)
        .await
        .unwrap()
        .expect("the pdf's row");
    stores
        .set_user_data(
            uid,
            pdf_row,
            UserItemData {
                played: true,
                is_favorite: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    stores
        .set_read_position(
            uid,
            pdf_row,
            ReadPosition {
                locator: "pdf1:4".into(),
                fraction: 0.4,
                updated_at: 1_700_000_000,
            },
        )
        .await
        .unwrap();

    let epub = td.path().join("Widget Cookbook.epub");
    write_epub(&epub);
    scanner.scan_into(td.path(), &stores).await.unwrap();

    let all = MediaStore::list(&stores).await.unwrap();
    let books = books(&all);
    assert_eq!(books.len(), 1, "{books:#?}");
    assert_eq!(books[0].path, epub);
    assert_eq!(
        stores.id_at_path(&pdf).await.unwrap(),
        None,
        "the pdf is a version now, not a row"
    );
    let kept = stores.get_user_data(uid, books[0].id).await.unwrap();
    assert!(kept.played && kept.is_favorite, "{kept:?}");
    let place = stores.get_read_position(uid, books[0].id).await.unwrap();
    assert_eq!(
        place.map(|p| (p.locator, p.fraction)),
        Some(("pdf1:4".to_string(), 0.4)),
        "the reading place moves with the row"
    );
}
