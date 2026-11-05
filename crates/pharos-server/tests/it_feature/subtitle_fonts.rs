//! 026 FR-010 / V488 (B572): `/videos/{id}/subtitles/{index}/fonts/{n}` serves
//! a font attachment cut to the glyphs the track draws, behind item access,
//! and a media-scoped token (ADR-0058) reaches it.

use actix_web::{test, web};
use pharos_cache::ImageCache;
use pharos_core::{
    LibraryKind, LibraryStore as _, MediaAttachment, MediaItem, MediaKind, MediaProbe,
    MediaStore as _, SubtitleTrack, TokenScope, TokenStore as _,
};
use pharos_server::state::AppState;

use crate::common::{build_app, seed_rich, Fixture};

const ITEM: u64 = 310;
const FONT_INDEX: u32 = 5;
const PLEX: &[u8] = include_bytes!("../fixtures/IBMPlexMono-Ascii.ttf");
const SCRIPT: &str = "[Script Info]\nTitle: t\n\n[Events]\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hi\n";

struct Rig {
    fx: Fixture,
    state: web::Data<AppState>,
    _dir: tempfile::TempDir,
    _cache: tempfile::TempDir,
}

/// An item in its own library with an embedded font (already in the
/// attachment cache, so no ffmpeg runs), an embedded PGS track at 3, and a
/// `.ass` sidecar, which is subtitle index 4.
async fn rig() -> Rig {
    let fx = seed_rich().await;
    let dir = tempfile::tempdir().unwrap();
    fx.state
        .stores
        .upsert_library(
            "Scratch",
            dir.path().to_str().unwrap(),
            LibraryKind::Movies,
            &"d".repeat(32),
        )
        .await
        .unwrap();
    std::fs::write(dir.path().join("Film.mkv"), b"x").unwrap();
    std::fs::write(dir.path().join("Film.eng.ass"), SCRIPT).unwrap();
    fx.state
        .stores
        .put(MediaItem {
            id: ITEM,
            path: dir.path().join("Film.mkv"),
            title: "Film".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                subtitle_tracks: vec![SubtitleTrack {
                    stream_index: 3,
                    codec: Some("hdmv_pgs_subtitle".into()),
                    ..SubtitleTrack::default()
                }],
                attachments: vec![
                    MediaAttachment {
                        stream_index: FONT_INDEX,
                        filename: Some("Plex.ttf".into()),
                        mime_type: Some("application/x-truetype-font".into()),
                        codec: Some("ttf".into()),
                    },
                    MediaAttachment {
                        stream_index: 6,
                        filename: Some("cover.jpg".into()),
                        mime_type: Some("image/jpeg".into()),
                        codec: None,
                    },
                ],
                ..MediaProbe::default()
            },
            ..MediaItem::default()
        })
        .await
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let extracted = cache.path().join("attachments").join(ITEM.to_string());
    std::fs::create_dir_all(&extracted).unwrap();
    std::fs::write(extracted.join(FONT_INDEX.to_string()), PLEX).unwrap();
    std::fs::write(extracted.join("6"), b"JPEG").unwrap();
    let state = web::Data::new(
        AppState::new(fx.state.stores.clone(), "t".into())
            .with_image_cache(ImageCache::new(cache.path())),
    );
    Rig {
        fx,
        state,
        _dir: dir,
        _cache: cache,
    }
}

async fn get(rig: &Rig, uri: &str, token: Option<&str>) -> (u16, Vec<u8>) {
    let app = test::init_service(build_app(rig.state.clone())).await;
    let mut req = test::TestRequest::get().uri(uri);
    if let Some(token) = token {
        req = req.insert_header(("X-Emby-Token", token));
    }
    let resp = test::call_service(&app, req.to_request()).await;
    (resp.status().as_u16(), test::read_body(resp).await.to_vec())
}

fn maps(font: &[u8], c: char) -> bool {
    use skrifa::MetadataProvider as _;
    skrifa::FontRef::new(font)
        .unwrap()
        .charmap()
        .map(c)
        .is_some()
}

fn uri(subtitle: u32, font: u32) -> String {
    format!("/videos/{ITEM}/subtitles/{subtitle}/fonts/{font}")
}

/// Disarm check: serving the whole font fails the size and `Z` assertions.
#[actix_web::test]
async fn a_sidecar_tracks_font_is_cut_to_the_glyphs_it_draws() {
    let rig = rig().await;
    let token = rig.fx.admin_token.clone();
    let (status, body) = get(&rig, &uri(4, FONT_INDEX), Some(&token)).await;
    assert_eq!(status, 200);
    assert!(
        body.len() < PLEX.len() / 2,
        "{} of {}",
        body.len(),
        PLEX.len()
    );
    assert!(maps(&body, 'H') && maps(&body, 'i'));
    assert!(
        !maps(&body, 'Z'),
        "V488: a glyph the script never draws is kept"
    );
}

#[actix_web::test]
async fn the_font_route_refuses_what_it_cannot_serve() {
    let rig = rig().await;
    let token = rig.fx.admin_token.clone();
    assert_eq!(get(&rig, &uri(4, FONT_INDEX), None).await.0, 401);
    // Not a font, not an attachment, not a subtitle, an image subtitle.
    assert_eq!(get(&rig, &uri(4, 6), Some(&token)).await.0, 404);
    assert_eq!(get(&rig, &uri(4, 99), Some(&token)).await.0, 404);
    assert_eq!(get(&rig, &uri(77, FONT_INDEX), Some(&token)).await.0, 404);
    assert_eq!(get(&rig, &uri(3, FONT_INDEX), Some(&token)).await.0, 415);
}

/// ADR-0058: a native client's media token loads the fonts like it loads
/// images and streams.
#[actix_web::test]
async fn a_media_token_loads_a_subtitle_font() {
    let rig = rig().await;
    let token = rig
        .fx
        .state
        .stores
        .issue_with_ttl(rig.fx.admin_id, "media", 3600, TokenScope::Media)
        .await
        .unwrap();
    let uri = format!("{}?api_key={}", uri(4, FONT_INDEX), token.secret.0.expose());
    let (status, body) = get(&rig, &uri, None).await;
    assert_eq!(status, 200);
    assert!(maps(&body, 'H'));
}
