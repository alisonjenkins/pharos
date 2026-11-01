//! V191 / B250: the `JellyfinAuth` cookie set at sign-in authenticates only
//! GET and HEAD requests on routes that serve an item's bytes to element
//! URLs. A browser sends it on every same-site request, including one a
//! sibling subdomain's page makes, so it must not reach a write or a JSON
//! API read. Disarm check: with the cookie read back in `extract_token`, every
//! `false` row below answers non-401.

use actix_web::{http::header, test};

use crate::common::{build_app, seed_rich};

fn cookie(token: &str) -> (header::HeaderName, String) {
    (header::COOKIE, format!("JellyfinAuth={token}"))
}

#[actix_web::test]
async fn sign_in_sets_a_strict_http_only_cookie_and_logout_clears_it() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateByName")
        .insert_header((
            "X-Emby-Authorization",
            r#"MediaBrowser Client="t", Device="d", DeviceId="dev1", Version="1""#,
        ))
        .set_json(serde_json::json!({ "Username": "guest", "Pw": "hunter2" }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let set = resp
        .response()
        .cookies()
        .find(|c| c.name() == "JellyfinAuth")
        .expect("JellyfinAuth set at sign-in")
        .into_owned();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(set.value(), body["AccessToken"].as_str().unwrap());
    assert_eq!(set.http_only(), Some(true));
    assert_eq!(set.same_site(), Some(actix_web::cookie::SameSite::Strict));
    assert_eq!(set.path(), Some("/"));

    let req = test::TestRequest::post()
        .uri("/Sessions/Logout")
        .insert_header(("X-Emby-Token", set.value()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 204);
    let cleared = resp
        .response()
        .cookies()
        .find(|c| c.name() == "JellyfinAuth")
        .expect("JellyfinAuth cleared at logout");
    assert_eq!(cleared.value(), "");
    assert_eq!(cleared.path(), Some("/"));
}

#[actix_web::test]
async fn the_cookie_reaches_only_reads_of_item_bytes() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let id = f.rich_item_id;
    let uid = f.user_id.0.simple();
    let mut wrong = Vec::new();
    // (method, uri, accepted): accepted means the cookie got past auth.
    for (method, uri, accepted) in [
        ("GET", format!("/Items/{id}/Images"), true),
        ("HEAD", format!("/Items/{id}/Images/Primary"), true),
        ("GET", format!("/Videos/{id}/Subtitles/0/Stream.vtt"), true),
        ("GET", format!("/Videos/{id}/Trickplay/320/0.jpg"), true),
        ("GET", "/Users/Me".to_owned(), false),
        ("GET", format!("/Users/{uid}/Items/{id}"), false),
        ("POST", format!("/UserFavoriteItems/{id}"), false),
        ("DELETE", format!("/Items/{id}/Images/Primary"), false),
        (
            "DELETE",
            "/Videos/ActiveEncodings?PlaySessionId=x".to_owned(),
            false,
        ),
    ] {
        let req = test::TestRequest::default()
            .method(method.parse().unwrap())
            .uri(&uri)
            .insert_header(cookie(&f.user_token))
            .to_request();
        let status = test::call_service(&app, req).await.status().as_u16();
        if (status == 401) == accepted {
            wrong.push(format!("{method} {uri}: {status}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "cookie scope wrong:\n{}",
        wrong.join("\n")
    );
}

/// Subtitle and font URLs in PlaybackInfo carry the caller's token, so a
/// `<track>` or SubtitlesOctopus fetch from any client authenticates.
/// Disarm check: with the `api_key` append in `playback_info` skipped, the
/// subtitle URL lacks the token.
#[actix_web::test]
async fn playback_info_delivery_urls_carry_the_callers_token() {
    use pharos_core::{MediaAttachment, MediaItem, MediaKind, MediaProbe, MediaStore as _};
    let f = seed_rich().await;
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("subbed.mkv");
    std::fs::write(&video, b"x").unwrap();
    std::fs::write(
        dir.path().join("subbed.eng.vtt"),
        b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhi\n",
    )
    .unwrap();
    f.state
        .stores
        .put(MediaItem {
            id: 500,
            path: video,
            title: "Subbed".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                video_codec: Some("h264".into()),
                attachments: vec![MediaAttachment {
                    stream_index: 3,
                    filename: Some("font.ttf".into()),
                    mime_type: Some("font/ttf".into()),
                    codec: Some("ttf".into()),
                }],
                ..MediaProbe::default()
            },
            ..MediaItem::default()
        })
        .await
        .unwrap();
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Items/500/PlaybackInfo")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(serde_json::json!({}))
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let source = &body["MediaSources"][0];
    let urls: Vec<&str> = source["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .chain(source["MediaAttachments"].as_array().unwrap())
        .filter_map(|s| s["DeliveryUrl"].as_str())
        .collect();
    assert!(urls.len() >= 2, "{body}");
    let want = format!("api_key={}", f.admin_token);
    for url in urls {
        assert!(url.contains(&want), "{url} lacks the token");
    }
}
