#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 004-books (T060/T064) — a book's cover, from inside the file to the tile.
//!
//! Two failures this guards, both of which look like nothing going wrong:
//!
//! **B155.** `put` does not maintain the denormalised `has_primary_art`; only
//! `set_artwork` does. A cover written the other way is on disk, servable, and
//! never advertised — present but invisible, and no error anywhere.
//!
//! **B149.** The mirror image: advertising `ImageTags.Primary` for an item with
//! no cover makes every grid render fetch an image that 404s. Coverless books
//! are normal (a `.cbr` can never have one by design, and plenty of epubs
//! simply do not), so the pair must agree per item, not in general.
//!
//! These run the REAL sqlite store and the REAL image cache rather than fakes,
//! because `has_primary_art` is a denormalisation maintained in SQL — a fake
//! store would only replay this test's own assumption about it.

use actix_web::{http::StatusCode, test, web, App};
use pharos_cache::image_cache::ImageCache;
use pharos_core::{
    MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_scanner::fs::FsScanner;
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    book_cover::ImageCacheCoverSink,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use std::io::Write;
use std::sync::Arc;
use tempfile::TempDir;

const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

/// Real JPEG bytes. An extension-only stub would satisfy every assertion here
/// and then fail to decode in a browser — the shape these tests exist to catch.
fn jpeg() -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
    v.extend_from_slice(b"\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

fn opf(with_cover: bool) -> String {
    let (meta, item) = if with_cover {
        (
            r#"<meta name="cover" content="cov"/>"#,
            r#"<item id="cov" href="images/cover.jpg" media-type="image/jpeg"/>"#,
        )
    } else {
        ("", "")
    };
    format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>A Book</dc:title>{meta}
  </metadata>
  <manifest>{item}
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
</package>"#
    )
}

fn write_epub(path: &std::path::Path, with_cover: bool) {
    let f = std::fs::File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zw.start_file("mimetype", stored).unwrap();
    zw.write_all(b"application/epub+zip").unwrap();
    let opts = zip::write::SimpleFileOptions::default();
    zw.start_file("META-INF/container.xml", opts).unwrap();
    zw.write_all(CONTAINER.as_bytes()).unwrap();
    zw.start_file("OEBPS/content.opf", opts).unwrap();
    zw.write_all(opf(with_cover).as_bytes()).unwrap();
    if with_cover {
        zw.start_file("OEBPS/images/cover.jpg", opts).unwrap();
        zw.write_all(&jpeg()).unwrap();
    }
    zw.start_file("OEBPS/ch1.xhtml", opts).unwrap();
    zw.write_all(b"<html><body>text</body></html>").unwrap();
    zw.finish().unwrap();
}

fn write_cbz(path: &std::path::Path, pages: &[&str]) {
    let f = std::fs::File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    for name in pages {
        zw.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(&jpeg()).unwrap();
    }
    zw.finish().unwrap();
}

/// A prober that panics if reached (SC-002 — a book must never see ffmpeg).
#[derive(Clone, Default)]
struct NeverProber;

impl pharos_core::Prober for NeverProber {
    async fn probe(
        &self,
        path: &std::path::Path,
    ) -> pharos_core::DomainResult<pharos_core::ProbeInfo> {
        panic!("the prober must never see {}", path.display());
    }
}

struct Fixture {
    stores: Stores,
    _media: TempDir,
    cache_dir: TempDir,
}

/// Scan a directory holding a covered epub, a cover-less epub and a cbz,
/// through a real image-cache-backed cover sink.
async fn scan_books() -> Fixture {
    let media = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    write_epub(&media.path().join("Covered.epub"), true);
    write_epub(&media.path().join("Bare.epub"), false);
    write_cbz(
        &media.path().join("Comic.cbz"),
        &["page01.jpg", "page02.jpg"],
    );
    // Nothing a client can open, and nothing to take a cover from either.
    std::fs::write(media.path().join("Sideloaded.azw3"), b"not an azw3").unwrap();

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let sink = Arc::new(ImageCacheCoverSink::new(ImageCache::new(
        cache_dir.path().to_path_buf(),
    )));
    FsScanner::new(NeverProber)
        .with_cover_sink(sink)
        .scan_into(media.path(), &stores)
        .await
        .expect("scan must succeed");

    Fixture {
        stores,
        _media: media,
        cache_dir,
    }
}

async fn find(stores: &Stores, stem: &str) -> pharos_core::MediaItem {
    MediaStore::list(stores)
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.path.to_string_lossy().contains(stem))
        .unwrap_or_else(|| panic!("{stem} was not imported"))
}

/// T060 — the cover reaches the store through `set_artwork`, which is what
/// maintains `has_primary_art`.
///
/// **Disarm-verify** by swapping `set_artwork` for `put` in
/// `FsScanner::store_book_cover`: the bytes still land in the cache and the
/// artwork row still reads back, but `has_primary_art` stays false and the
/// assertion below goes red — which is exactly the invisible state B155
/// describes.
#[actix_web::test]
async fn an_epub_cover_is_registered_as_primary_art() {
    let fx = scan_books().await;

    let covered = find(&fx.stores, "Covered.epub").await;
    assert_eq!(covered.kind, MediaKind::Book);
    assert!(
        covered.has_primary_art,
        "a cover was extracted, so the DENORMALISED flag must be set — it is what \
         decides whether the tile is ever advertised (B155)"
    );

    // And the artwork row itself points at a real file in the cache.
    let art = fx.stores.artwork_for(covered.id).await.unwrap();
    let (_, source, locator) = art
        .iter()
        .find(|(role, _, _)| role == "Primary")
        .expect("a Primary artwork row must exist");
    assert_eq!(source, "local");
    assert!(
        std::path::Path::new(locator).exists(),
        "the recorded locator must be a file that exists, or every render 404s: {locator}"
    );
    assert!(
        locator.starts_with(&*fx.cache_dir.path().to_string_lossy()),
        "the cover belongs in the image cache, not beside the media: {locator}"
    );
    assert_eq!(
        std::fs::read(locator).unwrap(),
        jpeg(),
        "the bytes served must be the ones that were inside the epub"
    );

    // The comic takes its cover from the first page, by the same route.
    let comic = find(&fx.stores, "Comic.cbz").await;
    assert!(
        comic.has_primary_art,
        "a cbz's first page is its cover; a comic grid with no tiles is the \
         symptom of this being false"
    );
}

/// The negative half. Without it, setting `has_primary_art = true` for every
/// book would pass the test above.
#[actix_web::test]
async fn a_coverless_book_claims_no_primary_art() {
    let fx = scan_books().await;

    for stem in ["Bare.epub", "Sideloaded.azw3"] {
        let item = find(&fx.stores, stem).await;
        assert_eq!(item.kind, MediaKind::Book, "{stem}");
        assert!(
            !item.has_primary_art,
            "{stem} has no cover, so it must not claim one — advertising a tag \
             that 404s on every render is B149"
        );
        assert!(
            fx.stores
                .artwork_for(item.id)
                .await
                .unwrap()
                .iter()
                .all(|(role, _, _)| role != "Primary"),
            "{stem} must have no Primary artwork row at all"
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

/// T064 — the tag and the bytes must agree, per item.
///
/// Complements the DTO-shape test in `book_offers_nothing_to_play.rs`: that one
/// asserts a hand-seeded row's payload, this one runs the whole scan-to-wire
/// path and then actually FETCHES what the payload advertised. An
/// advertised-then-404 pair is invisible server-side — it shows up as a grid of
/// broken tiles and a wall of 404s in someone else's log.
#[actix_web::test]
async fn the_primary_tag_and_the_primary_image_always_agree() {
    let fx = scan_books().await;

    let auth = BuiltinAuth::new(fx.stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    fx.stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = fx.stores.issue(uid, "t").await.unwrap();
    let token = token.0.expose().to_string();

    let state = web::Data::new(
        AppState::new(fx.stores.clone(), "srv".into())
            .with_image_cache(ImageCache::new(fx.cache_dir.path().to_path_buf())),
    );
    let app = test::init_service(app(state)).await;

    let req = test::TestRequest::get()
        .uri("/Items?IncludeItemTypes=Book")
        .insert_header(auth_header!(&token))
        .to_request();
    let body: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;
    let items = body["Items"].as_array().expect("a list of books");
    assert_eq!(items.len(), 4, "all four books must be listed: {body}");

    let mut advertised = 0;
    for item in items {
        let id = item["Id"].as_str().unwrap();
        let name = item["Name"].as_str().unwrap_or_default().to_string();
        let has_tag = item["ImageTags"].get("Primary").is_some();

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Items/{id}/Images/Primary"))
                .insert_header(auth_header!(&token))
                .to_request(),
        )
        .await;

        if has_tag {
            advertised += 1;
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{name} advertises a Primary tag but its image does not resolve — \
                 the B149 pair, one 404 per grid render"
            );
            assert!(
                !test::read_body(resp).await.is_empty(),
                "{name} served an empty Primary image"
            );
        } else {
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{name} advertises no Primary tag, so nothing may be served for it \
                 either — a client that guesses the URL must get a straight answer"
            );
        }

        // Whatever the cover situation, a book has no frames to take these from.
        for role in ["Backdrop", "Thumb"] {
            assert!(
                item["ImageTags"].get(role).is_none(),
                "{name} must not advertise {role}: a book has no video frames"
            );
        }
    }

    assert_eq!(
        advertised, 2,
        "exactly the covered epub and the cbz carry a tag; if this is 4 the \
         negative case has stopped being tested"
    );
}
