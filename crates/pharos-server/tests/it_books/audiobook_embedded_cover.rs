#![allow(clippy::unwrap_used, clippy::expect_used)]
//! B1437 / V1411 — an audiobook's embedded cover reaches the tile.
//!
//! Every audiobook part carries its cover as an `attached_pic` stream and none
//! has a sidecar, so `has_primary_art` stayed false and no album or part
//! advertised a Primary, although the image route extracts embedded art on
//! demand. Runs the real ffprobe, the real scan, the real sqlite store and the
//! real image cache, and fetches whatever the listing advertised (V82, B149).

use actix_web::{http::StatusCode, test, web, App};
use pharos_cache::image_cache::ImageCache;
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_scanner::{fs::FsScanner, FfmpegProber};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg on PATH");
    assert!(
        out.status.success(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One tiny mp3 part; with `cover`, a jpeg rides along as an attached_pic.
fn write_part(path: &Path, cover: Option<&Path>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let dest = path.to_str().unwrap();
    match cover {
        Some(cover) => ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-i",
            cover.to_str().unwrap(),
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c:a",
            "libmp3lame",
            "-c:v",
            "copy",
            "-id3v2_version",
            "3",
            "-disposition:v",
            "attached_pic",
            dest,
        ]),
        None => ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:a",
            "libmp3lame",
            dest,
        ]),
    }
}

fn auth_header(token: &str) -> (&'static str, String) {
    (
        "X-Emby-Authorization",
        format!(
            "MediaBrowser Client=\"t\", Device=\"d\", DeviceId=\"i\", Version=\"1\", Token=\"{token}\""
        ),
    )
}

#[actix_web::test]
async fn an_audiobook_with_embedded_covers_advertises_and_serves_a_primary() {
    let td = TempDir::new().unwrap();
    let books = td.path().join("books");
    let cover = td.path().join("cover.jpg");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=red:s=64x64:d=1",
        "-frames:v",
        "1",
        cover.to_str().unwrap(),
    ]);
    for (book, covered) in [("Covered Tale", true), ("Bare Tale", false)] {
        for part in ["1 One.mp3", "2 Two.mp3"] {
            write_part(
                &books.join("Ada Field").join(book).join(part),
                covered.then_some(cover.as_path()),
            );
        }
    }

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    FsScanner::new(FfmpegProber::new())
        .with_course_roots([books.clone()])
        .scan_into(&books, &stores)
        .await
        .unwrap();

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
    let token = stores.issue(uid, "t").await.unwrap().0.expose().to_string();
    let cache_dir = TempDir::new().unwrap();
    let state = web::Data::new(
        AppState::new(stores, "srv".into())
            .with_image_cache(ImageCache::new(cache_dir.path().to_path_buf())),
    );
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let body: serde_json::Value = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/Items?IncludeItemTypes=MusicAlbum&Recursive=true")
                .insert_header(auth_header(&token))
                .to_request(),
        )
        .await,
    )
    .await;
    let albums = body["Items"].as_array().expect("a list of albums");
    assert_eq!(albums.len(), 2, "both books list as albums: {body}");
    let mut advertised = Vec::new();
    for album in albums {
        let name = album["Name"].as_str().unwrap().to_string();
        let id = album["Id"].as_str().unwrap();
        let has_tag = album["ImageTags"].get("Primary").is_some();
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Items/{id}/Images/Primary"))
                .insert_header(auth_header(&token))
                .to_request(),
        )
        .await;
        if has_tag {
            advertised.push(name.clone());
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{name}: tag without an image"
            );
            assert!(
                !test::read_body(resp).await.is_empty(),
                "{name}: empty image"
            );
        } else {
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{name}: image without a tag"
            );
        }
    }
    assert_eq!(advertised, vec!["Covered Tale".to_string()]);
}
