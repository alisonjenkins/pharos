//! B254: an embedded attachment's type is whatever the media file says. An
//! HTML or SVG attachment served inline would run on pharos's origin and can
//! read the signed-in user's token. Only fonts are served inline; every
//! attachment carries a script-less sandbox CSP; every response is `nosniff`.

use actix_web::{http::header, test, web};
use pharos_cache::ImageCache;
use pharos_core::{MediaAttachment, MediaItem, MediaKind, MediaProbe, MediaStore};
use pharos_server::state::AppState;

use crate::common::{build_app, seed_rich};

const ITEM: u64 = 300;
/// (stream index, mimetype, body).
const ATTACHMENTS: &[(u32, &str, &[u8])] = &[
    (3, "text/html", b"<script>alert(document.domain)</script>"),
    (
        4,
        "image/svg+xml",
        b"<svg xmlns='http://www.w3.org/2000/svg' onload='alert(1)'/>",
    ),
    (5, "application/x-truetype-font", b"FONT"),
];

/// The fixture with an item carrying the three attachments, already
/// extracted into the cache so no ffmpeg run is needed.
async fn seed() -> (
    crate::common::Fixture,
    web::Data<AppState>,
    tempfile::TempDir,
) {
    let f = seed_rich().await;
    f.state
        .stores
        .put(MediaItem {
            id: ITEM,
            path: "/libA/with-attachments.mkv".into(),
            title: "Attachments".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                attachments: ATTACHMENTS
                    .iter()
                    .map(|&(idx, mime, _)| MediaAttachment {
                        stream_index: idx,
                        filename: None,
                        mime_type: Some(mime.into()),
                        codec: None,
                    })
                    .collect(),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let dir = cache.path().join("attachments").join(ITEM.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    for (idx, _, body) in ATTACHMENTS {
        std::fs::write(dir.join(idx.to_string()), body).unwrap();
    }
    let state = web::Data::new(
        AppState::new(f.state.stores.clone(), "t".into())
            .with_image_cache(ImageCache::new(cache.path())),
    );
    (f, state, cache)
}

fn header_of(resp: &actix_web::dev::ServiceResponse, name: header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_owned())
}

/// Disarm check: with the `CONTENT_DISPOSITION` insert removed from
/// `deliver_attachment`, or the CSP insert removed, this fails.
#[actix_web::test]
async fn html_and_svg_attachments_download_in_a_sandbox() {
    let (f, state, _cache) = seed().await;
    let app = test::init_service(build_app(state)).await;
    for &(idx, mime, body) in &ATTACHMENTS[..2] {
        let req = test::TestRequest::get()
            .uri(&format!("/Videos/{ITEM}/{ITEM}/Attachments/{idx}"))
            .insert_header(("X-Emby-Token", f.admin_token.as_str()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 200, "{mime}");
        assert_eq!(
            header_of(&resp, header::CONTENT_DISPOSITION).as_deref(),
            Some("attachment"),
            "{mime}"
        );
        assert_eq!(
            header_of(&resp, header::CONTENT_SECURITY_POLICY).as_deref(),
            Some("default-src 'none'; sandbox"),
            "{mime}"
        );
        assert_eq!(
            header_of(&resp, header::X_CONTENT_TYPE_OPTIONS).as_deref(),
            Some("nosniff"),
            "{mime}"
        );
        assert_eq!(test::read_body(resp).await.as_ref(), body, "{mime}");
    }
}

/// SubtitlesOctopus loads fonts by URL, so a font stays inline, but still
/// sandboxed and `nosniff`.
#[actix_web::test]
async fn a_font_attachment_stays_inline() {
    let (f, state, _cache) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let (idx, _, body) = ATTACHMENTS[2];
    let req = test::TestRequest::get()
        .uri(&format!("/Videos/{ITEM}/Attachments/{idx}"))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(header_of(&resp, header::CONTENT_DISPOSITION), None);
    assert_eq!(
        header_of(&resp, header::CONTENT_SECURITY_POLICY).as_deref(),
        Some("default-src 'none'; sandbox")
    );
    assert_eq!(
        header_of(&resp, header::X_CONTENT_TYPE_OPTIONS).as_deref(),
        Some("nosniff")
    );
    assert_eq!(test::read_body(resp).await.as_ref(), body);
}

/// Every route answers `nosniff`, byte routes and JSON alike, errors
/// included. Disarm check: with `nosniff()` unwrapped from the app, this
/// fails.
#[actix_web::test]
async fn every_response_is_nosniff() {
    let f = seed_rich().await;
    let id = f.rich_item_id;
    let app = test::init_service(build_app(f.state.clone())).await;
    for uri in [
        "/Users/Me".to_owned(),
        format!("/Items/{id}/Images"),
        "/Items/999999999/Images/Primary".to_owned(),
        format!("/Videos/{id}/Subtitles/0/Stream.vtt"),
    ] {
        let req = test::TestRequest::get()
            .uri(&uri)
            .insert_header(("X-Emby-Token", f.admin_token.as_str()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            header_of(&resp, header::X_CONTENT_TYPE_OPTIONS).as_deref(),
            Some("nosniff"),
            "{uri}: status {}",
            resp.status()
        );
    }
}
