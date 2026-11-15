#![allow(clippy::unwrap_used, clippy::expect_used)]
//! B1451 / ADR-0138 — `GET /Items/{id}/Download?maxWidth=&maxHeight=` serves a
//! copy of an EPUB whose pictures are scaled to the box, and leaves the
//! original, every other entry, and every other request alone.
//!
//! The book is built here from generated pixels; no real title or file is used.

use std::io::{Cursor, Read, Write};

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
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const BIG: (u32, u32) = (2000, 2600);
const BOX: (u32, u32) = (1264, 1680);

/// Pseudo-random pixels, so the JPEG does not compress to nothing.
fn noise(w: u32, h: u32) -> image::RgbImage {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    image::RgbImage::from_fn(w, h, |x, y| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let n = (state >> 56) as u8;
        image::Rgb([
            ((x / 8) as u8).wrapping_add(n / 4),
            ((y / 8) as u8).wrapping_add(n / 4),
            n,
        ])
    })
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(noise(w, h))
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut out, 95,
        ))
        .unwrap();
    out
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(noise(w, h))
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

const CHAPTER: &[u8] = b"<html><body><p>Synthetic chapter.</p></body></html>";
const OPF: &[u8] = b"<package><manifest><item id=\"p\" href=\"big.jpg\" media-type=\"image/jpeg\"/></manifest></package>";

/// An EPUB with one `big` JPEG of `big` pixels, a small PNG, a chapter and an
/// OPF. `mimetype` is first and stored, as OCF requires.
fn epub(big: (u32, u32)) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("OEBPS/content.opf", deflated).unwrap();
    zip.write_all(OPF).unwrap();
    zip.start_file("OEBPS/ch1.xhtml", deflated).unwrap();
    zip.write_all(CHAPTER).unwrap();
    zip.start_file("OEBPS/big.jpg", stored).unwrap();
    zip.write_all(&jpeg(big.0, big.1)).unwrap();
    zip.start_file("OEBPS/small.png", stored).unwrap();
    zip.write_all(&png(120, 80)).unwrap();
    zip.finish().unwrap().into_inner()
}

struct Fixture {
    state: web::Data<AppState>,
    token: String,
    original: Vec<u8>,
    _dir: TempDir,
}

async fn seed(book: Vec<u8>, name: &str, kind: MediaKind, with_cache: bool) -> Fixture {
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
            kind,
            book: (kind == MediaKind::Book).then(|| BookMeta {
                format: BookFormat::Epub,
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
        (status, test::read_body(resp).await.to_vec())
    }};
}

fn slim_count(snapshotter: &pharos_test_metrics::CumulativeSnapshotter, outcome: &str) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            k.key().name() == "pharos_book_slim_total"
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

fn entry(zip: &mut ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    zip.by_name(name).unwrap().read_to_end(&mut out).unwrap();
    out
}

/// The tentpole: a valid zip, `mimetype` first and stored with no extra field,
/// the big picture inside the box and a JPEG, everything else byte-identical.
/// Disarm check: make `repack` copy the picture raw and the dimension
/// assertion fails.
#[actix_web::test]
async fn a_box_gives_a_valid_smaller_epub_with_pictures_inside_it() {
    let f = seed(epub(BIG), "Synthetic.epub", MediaKind::Book, true).await;
    let app = app!(f.state);
    let (status, body) = download!(
        app,
        f.token,
        format!("&maxWidth={}&maxHeight={}", BOX.0, BOX.1)
    );
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.len() < f.original.len(),
        "slimmed {} must be smaller than the original {}",
        body.len(),
        f.original.len()
    );

    assert_eq!(&body[..4], b"PK\x03\x04");
    assert_eq!(
        &body[30..38],
        b"mimetype",
        "mimetype must be the first entry"
    );
    assert_eq!(
        u16::from_le_bytes([body[8], body[9]]),
        0,
        "mimetype must be stored"
    );
    assert_eq!(
        u16::from_le_bytes([body[28], body[29]]),
        0,
        "OCF forbids an extra field on mimetype"
    );
    assert_eq!(&body[38..58], b"application/epub+zip");

    let mut zip = ZipArchive::new(Cursor::new(body)).unwrap();
    let mut orig = ZipArchive::new(Cursor::new(f.original.clone())).unwrap();
    assert_eq!(zip.len(), orig.len());
    for i in 0..orig.len() {
        let name = orig.name_for_index(i).unwrap().unwrap().into_owned();
        assert!(zip.by_name(&name).is_ok(), "entry {name} must survive");
    }
    for same in [
        "mimetype",
        "OEBPS/content.opf",
        "OEBPS/ch1.xhtml",
        "OEBPS/small.png",
    ] {
        assert_eq!(
            entry(&mut zip, same),
            entry(&mut orig, same),
            "{same} must be byte-identical"
        );
    }
    let picture = entry(&mut zip, "OEBPS/big.jpg");
    assert_eq!(
        image::guess_format(&picture).unwrap(),
        image::ImageFormat::Jpeg,
        "the name and the media type stay valid"
    );
    let (w, h) = image::load_from_memory(&picture).unwrap().dimensions_pair();
    assert!(w <= BOX.0 && h <= BOX.1, "{w}x{h} must fit {BOX:?}");
    assert!(w == BOX.0 || h == BOX.1, "{w}x{h} must fill one edge");
    assert!(picture.len() < BIG.0 as usize * BIG.1 as usize);

    // The original is untouched on disk.
    let on_disk = std::fs::read(
        MediaStore::get(&f.state.stores, 42)
            .await
            .map(|i| i.path)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk, f.original);
}

trait Pair {
    fn dimensions_pair(&self) -> (u32, u32);
}
impl Pair for image::DynamicImage {
    fn dimensions_pair(&self) -> (u32, u32) {
        (self.width(), self.height())
    }
}

#[actix_web::test]
async fn without_a_box_or_for_a_non_epub_the_bytes_are_the_original() {
    let f = seed(epub(BIG), "Synthetic.epub", MediaKind::Book, true).await;
    let app = app!(f.state);
    let (status, body) = download!(app, f.token, "");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, f.original, "no params: byte-identical");
    // Junk and zero bounds read as no box.
    let (_, body) = download!(app, f.token, "&maxWidth=abc&maxHeight=0");
    assert_eq!(body, f.original);

    let movie = seed(b"not a book".to_vec(), "Film.mkv", MediaKind::Movie, true).await;
    let app = app!(movie.state);
    let (status, body) = download!(app, movie.token, "&maxWidth=100&maxHeight=100");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, movie.original, "a non-EPUB ignores the box");
}

/// The second request is served from the cache: the counter reads one
/// `slimmed` then one `cache_hit`, and the bytes are the same.
/// Disarm check: skip the cache lookup in `get_or_slim` and `cache_hit` stays 0
/// while `slimmed` reads 2.
#[actix_web::test]
async fn a_second_request_hits_the_cache() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let f = seed(epub(BIG), "Synthetic.epub", MediaKind::Book, true).await;
    let app = app!(f.state);
    let params = "&maxWidth=1264&maxHeight=1680";

    let (_, first) = download!(app, f.token, params);
    assert_eq!(slim_count(&snapshotter, "slimmed"), 1);
    assert_eq!(slim_count(&snapshotter, "cache_hit"), 0);

    let (_, second) = download!(app, f.token, params);
    assert_eq!(slim_count(&snapshotter, "slimmed"), 1, "no second re-pack");
    assert_eq!(slim_count(&snapshotter, "cache_hit"), 1);
    assert_eq!(first, second);

    // Another box is another copy.
    let (_, other) = download!(app, f.token, "&maxWidth=600&maxHeight=800");
    assert_eq!(slim_count(&snapshotter, "slimmed"), 2);
    assert_ne!(first, other);
}

/// A book whose pictures already fit gets the original, and the decision is
/// remembered so the next request does not open the book.
#[actix_web::test]
async fn a_book_that_already_fits_is_served_as_it_is() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let f = seed(epub((400, 500)), "Synthetic.epub", MediaKind::Book, true).await;
    let app = app!(f.state);
    let params = "&maxWidth=1264&maxHeight=1680";

    let (_, body) = download!(app, f.token, params);
    assert_eq!(body, f.original);
    assert_eq!(slim_count(&snapshotter, "already_fits"), 1);
    let (_, body) = download!(app, f.token, params);
    assert_eq!(body, f.original);
    assert_eq!(slim_count(&snapshotter, "cache_hit_original"), 1);
}

#[actix_web::test]
async fn no_cache_directory_or_a_broken_book_serves_the_original() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let params = "&maxWidth=1264&maxHeight=1680";

    let f = seed(epub(BIG), "Synthetic.epub", MediaKind::Book, false).await;
    let app = app!(f.state);
    let (status, body) = download!(app, f.token, params);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, f.original);
    assert_eq!(slim_count(&snapshotter, "no_cache"), 1);

    let broken = seed(
        b"this is not a zip".to_vec(),
        "Synthetic.epub",
        MediaKind::Book,
        true,
    )
    .await;
    let app = app!(broken.state);
    let (status, body) = download!(app, broken.token, params);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, broken.original);
    assert_eq!(slim_count(&snapshotter, "failed"), 1);
}
