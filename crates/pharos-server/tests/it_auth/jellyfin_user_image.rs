#![allow(clippy::unwrap_used, clippy::expect_used)]
//! V362: a user's avatar on Jellyfin's routes. Set by the user (or an
//! admin) as base64, served anonymously by user id as the server's own
//! re-encode, advertised as `UserDto.PrimaryImageTag`, and removable.

use std::io::Cursor;

use actix_web::{http::StatusCode, test, web, App};
use base64::Engine as _;
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

struct Seeded {
    state: web::Data<AppState>,
    admin: (UserId, String),
    guest: (UserId, String),
}

async fn user(stores: &Stores, name: &str, admin: bool) -> (UserId, String) {
    let auth = BuiltinAuth::new(stores.clone());
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: name.into(),
            password_hash: auth.hash_password(&SecretString::new("p")).unwrap(),
            policy: UserPolicy {
                admin,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, name).await.unwrap();
    (uid, token.0.expose().to_string())
}

async fn seed() -> Seeded {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let admin = user(&stores, "boss", true).await;
    let guest = user(&stores, "guest", false).await;
    let state = web::Data::new(AppState::new(stores, "t".into()));
    Seeded {
        state,
        admin,
        guest,
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

fn photo() -> Vec<u8> {
    let img = image::RgbImage::from_pixel(900, 600, image::Rgb([200, 40, 40]));
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::from(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn hex(uid: UserId) -> String {
    uid.0.simple().to_string()
}

#[actix_web::test]
async fn a_user_sets_their_avatar_and_anyone_can_fetch_the_re_encode() {
    let s = seed().await;
    let app = test::init_service(app(s.state.clone())).await;
    let b64 = base64::engine::general_purpose::STANDARD.encode(photo());
    let req = test::TestRequest::post()
        .uri("/UserImage")
        .insert_header(("X-Emby-Token", s.guest.1.as_str()))
        .insert_header(("Content-Type", "image/png"))
        .set_payload(b64)
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );

    // The user record now advertises a tag.
    let req = test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", s.guest.1.as_str()))
        .to_request();
    let me: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let tag = me["PrimaryImageTag"].as_str().unwrap().to_owned();

    // So does the admin's user listing, read in one go, and only for them.
    let listed = |app, token: String| async move {
        let req = test::TestRequest::get()
            .uri("/Users")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let users: Vec<serde_json::Value> = test::call_and_read_body_json(app, req).await;
        users
            .into_iter()
            .map(|u| {
                let name = u["Name"].as_str().unwrap().to_owned();
                (name, u["PrimaryImageTag"].as_str().map(str::to_owned))
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let tags = listed(&app, s.admin.1.clone()).await;
    assert_eq!(tags["guest"].as_deref(), Some(tag.as_str()));
    assert_eq!(tags["boss"], None);

    // Anonymous, by id, on either spelling: a square JPEG the server wrote.
    for uri in [
        format!("/UserImage?userId={}&tag={tag}", hex(s.guest.0)),
        format!("/Users/{}/Images/Primary?tag={tag}", hex(s.guest.0)),
    ] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(&uri).to_request()).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let h = resp.headers();
        assert_eq!(h.get("content-type").unwrap(), "image/jpeg");
        assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
        assert!(h
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("immutable"));
        let body = test::read_body(resp).await;
        let back = image::load_from_memory(&body).unwrap();
        assert_eq!((back.width(), back.height()), (512, 512));
    }

    // Removed, it is gone and the tag with it.
    let req = test::TestRequest::delete()
        .uri("/UserImage")
        .insert_header(("X-Emby-Token", s.guest.1.as_str()))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );
    let uri = format!("/UserImage?userId={}", hex(s.guest.0));
    let resp = test::call_service(&app, test::TestRequest::get().uri(&uri).to_request()).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let req = test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", s.guest.1.as_str()))
        .to_request();
    let me: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert!(me.get("PrimaryImageTag").is_none(), "{me}");
    assert_eq!(listed(&app, s.admin.1.clone()).await["guest"], None);
}

#[actix_web::test]
async fn only_the_user_or_an_admin_may_change_it() {
    let s = seed().await;
    let app = test::init_service(app(s.state.clone())).await;
    let b64 = base64::engine::general_purpose::STANDARD.encode(photo());
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/Images/Primary", hex(s.admin.0)))
        .insert_header(("X-Emby-Token", s.guest.1.as_str()))
        .set_payload(b64.clone())
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::FORBIDDEN
    );
    let req = test::TestRequest::delete()
        .uri(&format!("/UserImage?userId={}", hex(s.admin.0)))
        .insert_header(("X-Emby-Token", s.guest.1.as_str()))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::FORBIDDEN
    );

    let req = test::TestRequest::post()
        .uri(&format!("/UserImage?userId={}", hex(s.guest.0)))
        .insert_header(("X-Emby-Token", s.admin.1.as_str()))
        .set_payload(b64)
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );
    assert!(pharos_server::avatar::load(&s.state.stores, s.guest.0)
        .await
        .unwrap()
        .is_some());
}

#[actix_web::test]
async fn what_is_not_a_picture_is_refused_and_nothing_is_stored() {
    let s = seed().await;
    let app = test::init_service(app(s.state.clone())).await;
    for body in [
        b"<svg xmlns='http://www.w3.org/2000/svg' onload='alert(1)'/>".to_vec(),
        base64::engine::general_purpose::STANDARD
            .encode("<html><script>alert(1)</script></html>")
            .into_bytes(),
        Vec::new(),
    ] {
        let req = test::TestRequest::post()
            .uri("/UserImage")
            .insert_header(("X-Emby-Token", s.guest.1.as_str()))
            .insert_header(("Content-Type", "image/png"))
            .set_payload(body)
            .to_request();
        assert_eq!(
            test::call_service(&app, req).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert!(pharos_server::avatar::load(&s.state.stores, s.guest.0)
        .await
        .unwrap()
        .is_none());
}

#[actix_web::test]
async fn another_image_type_is_not_found() {
    let s = seed().await;
    let app = test::init_service(app(s.state.clone())).await;
    let uri = format!("/Users/{}/Images/Backdrop", hex(s.guest.0));
    let resp = test::call_service(&app, test::TestRequest::get().uri(&uri).to_request()).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
