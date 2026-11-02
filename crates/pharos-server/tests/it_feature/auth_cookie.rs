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
    for url in &urls {
        assert!(url.contains(&want), "{url} lacks the token");
    }

    // The same file by cookie alone, as a <track> fetches it: served, and
    // never `public`, or a shared cache would replay it to an anonymous
    // request for the same token-free URL. Disarm check: with subtitles.rs
    // back on `public, max-age=3600` this fails.
    let path = urls
        .iter()
        .find(|u| u.contains("/Subtitles/"))
        .and_then(|u| u.split('?').next())
        .unwrap();
    let req = test::TestRequest::get()
        .uri(path)
        .insert_header(cookie(&f.admin_token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200, "{path}");
    let cache = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        cache.starts_with("private"),
        "{path}: Cache-Control {cache:?}"
    );
}

fn set_auth_cookies(
    resp: &actix_web::dev::ServiceResponse,
) -> Vec<actix_web::cookie::Cookie<'static>> {
    resp.response()
        .cookies()
        .filter(|c| c.name() == "JellyfinAuth")
        .map(|c| c.into_owned())
        .collect()
}

fn same_origin(req: test::TestRequest) -> test::TestRequest {
    req.insert_header(("Sec-Fetch-Site", "same-origin"))
}

/// A browser that signed in before the cookie existed still holds its token
/// in localStorage and sends it as a header. Its next authenticated request
/// must hand it the cookie, or every poster and subtitle 401s until it signs
/// out and in. Disarm check: with the `HeaderCredential` insert in
/// `note_header_credential` skipped, this fails.
#[actix_web::test]
async fn header_authenticated_request_without_the_cookie_gets_it() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    for (name, req) in [
        (
            "X-Emby-Token",
            test::TestRequest::get()
                .uri("/Users/Me")
                .insert_header(("X-Emby-Token", f.user_token.as_str())),
        ),
        (
            "Authorization",
            test::TestRequest::get().uri("/System/Info").insert_header((
                "Authorization",
                format!(r#"MediaBrowser Client="t", Token="{}""#, f.user_token),
            )),
        ),
    ] {
        let resp = test::call_service(&app, same_origin(req).to_request()).await;
        assert_eq!(resp.status().as_u16(), 200, "{name}");
        let set = set_auth_cookies(&resp);
        assert_eq!(set.len(), 1, "{name}: one JellyfinAuth Set-Cookie");
        let c = &set[0];
        assert_eq!(c.value(), f.user_token, "{name}");
        assert_eq!(c.http_only(), Some(true), "{name}");
        assert_eq!(
            c.same_site(),
            Some(actix_web::cookie::SameSite::Strict),
            "{name}"
        );
        assert_eq!(c.path(), Some("/"), "{name}");
        assert_eq!(
            c.max_age(),
            Some(actix_web::cookie::time::Duration::days(400)),
            "{name}"
        );
    }
}

/// A cookie for another token is never overwritten: a page that can make
/// the browser send someone else's token must not be able to swap the
/// victim's session for it (B253). Signing in replaces the cookie. Disarm
/// check: with `OtherCookie` answering `Set` this fails.
#[actix_web::test]
async fn a_cookie_holding_another_token_is_kept() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = same_origin(test::TestRequest::get())
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .insert_header(cookie(&f.admin_token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert!(set_auth_cookies(&resp).is_empty());
}

/// Session fixation (B253): a page on a sibling host embeds
/// `<img src="https://pharos/…?api_key=ATTACKER">`. The browser sends it as
/// a same-site request with no cookie, and pharos accepts the token. The
/// response must not plant the attacker's token as the victim's cookie,
/// whatever `Sec-Fetch-Site` says. Disarm check: with `note_header_credential`
/// noting `extract_token` again, this fails.
#[actix_web::test]
async fn an_api_key_never_sets_the_cookie() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let id = f.rich_item_id;
    for site in ["same-site", "cross-site", "same-origin"] {
        for uri in [
            format!("/Items/{id}/Images?api_key={}", f.admin_token),
            format!("/Users/Me?api_key={}", f.admin_token),
        ] {
            let req = test::TestRequest::get()
                .uri(&uri)
                .insert_header(("Sec-Fetch-Site", site))
                .insert_header(("Sec-Fetch-Dest", "image"))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status().as_u16(), 200, "{site} {uri}");
            assert!(set_auth_cookies(&resp).is_empty(), "{site} {uri}");
        }
    }
}

/// Only pharos's own page gets the cookie. A native client sends no
/// `Sec-Fetch-Site` and has no cookie jar; another site's page sends
/// `same-site` or `cross-site`. Disarm check: with the `Sec-Fetch-Site`
/// gate in `outcome` removed, this fails.
#[actix_web::test]
async fn a_header_token_from_anywhere_but_the_same_origin_gets_no_cookie() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    for site in [None, Some("same-site"), Some("cross-site"), Some("none")] {
        let mut req = test::TestRequest::get()
            .uri("/Users/Me")
            .insert_header(("X-Emby-Token", f.user_token.as_str()));
        if let Some(site) = site {
            req = req.insert_header(("Sec-Fetch-Site", site));
        }
        let resp = test::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status().as_u16(), 200, "{site:?}");
        assert!(set_auth_cookies(&resp).is_empty(), "{site:?}");
    }
}

#[actix_web::test]
async fn a_matching_cookie_is_left_alone() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = same_origin(test::TestRequest::get())
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .insert_header(cookie(&f.user_token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert!(set_auth_cookies(&resp).is_empty());
}

#[actix_web::test]
async fn no_cookie_is_set_without_a_validated_token_or_a_success() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let id = f.rich_item_id;
    // Cookie-only reads authenticate but are not header credentials.
    for (label, req) in [
        ("no credential", test::TestRequest::get().uri("/Users/Me")),
        (
            "invalid header token",
            test::TestRequest::get()
                .uri("/Users/Me")
                .insert_header(("X-Emby-Token", "not-a-token")),
        ),
        (
            "invalid api_key",
            test::TestRequest::get().uri("/Users/Me?api_key=not-a-token"),
        ),
        (
            "cookie only",
            test::TestRequest::get()
                .uri(&format!("/Items/{id}/Images"))
                .insert_header(cookie(&f.user_token)),
        ),
        (
            "valid token, non-2xx",
            test::TestRequest::get()
                .uri("/Items/999999999/Images/Primary")
                .insert_header(("X-Emby-Token", f.user_token.as_str())),
        ),
    ] {
        let resp = test::call_service(&app, same_origin(req).to_request()).await;
        assert!(set_auth_cookies(&resp).is_empty(), "{label}");
    }
    let req = test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", "not-a-token"))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 401);
}

#[actix_web::test]
async fn logout_clears_rather_than_backfills() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = same_origin(test::TestRequest::post())
        .uri("/Sessions/Logout")
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let set = set_auth_cookies(&resp);
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].value(), "");
}

/// The end of the chain: the backfilled cookie alone fetches an image.
#[actix_web::test]
async fn the_backfilled_cookie_fetches_an_image_without_a_header() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = same_origin(test::TestRequest::get())
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let set = set_auth_cookies(&resp);
    let c = set.first().expect("backfilled");
    let req = test::TestRequest::get()
        .uri(&format!("/Items/{}/Images", f.rich_item_id))
        .insert_header(cookie(c.value()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);
}

/// Every decision is countable by outcome, refusals included. Disarm check:
/// with the counter increment removed from `cookie_backfill.rs` this fails.
#[actix_web::test]
async fn backfill_decisions_are_counted_by_outcome() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    for (site, held) in [
        (Some("same-origin"), None),
        (Some("same-origin"), Some(f.user_token.as_str())),
        (Some("same-origin"), Some(f.admin_token.as_str())),
        (Some("same-site"), None),
        (None, None),
    ] {
        let mut req = test::TestRequest::get()
            .uri("/Users/Me")
            .insert_header(("X-Emby-Token", f.user_token.as_str()));
        if let Some(site) = site {
            req = req.insert_header(("Sec-Fetch-Site", site));
        }
        if let Some(t) = held {
            req = req.insert_header(cookie(t));
        }
        test::call_service(&app, req.to_request()).await;
    }
    let count = |outcome: &str| -> u64 {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(key, _, _, value)| {
                let labelled = key
                    .key()
                    .labels()
                    .any(|l| l.key() == "outcome" && l.value() == outcome);
                match value {
                    DebugValue::Counter(n)
                        if key.key().name() == "pharos_auth_cookie_backfill_total" && labelled =>
                    {
                        Some(n)
                    }
                    _ => None,
                }
            })
            .unwrap_or(0)
    };
    assert_eq!(
        [
            count("set"),
            count("held"),
            count("other_cookie"),
            count("not_same_origin")
        ],
        [1, 1, 1, 2]
    );
}
