//! `/admin/series-identity/*` (spec 033 §7, V896): admin only, and a move
//! made through it shows up on the overview as a split and an override.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, SeriesInfo, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const FLASH: &str = "/tv/The Flash";

async fn seed(admin: bool) -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "boss".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    for (id, episode) in [(1, 1), (2, 2)] {
        MediaStore::put(
            &stores,
            MediaItem {
                id,
                path: format!("{FLASH}/Season 1/The Flash - S01E0{episode}.mkv").into(),
                title: format!("Episode {episode}"),
                kind: MediaKind::Episode,
                series: Some(SeriesInfo {
                    series_name: "The Flash".into(),
                    season_number: Some(1),
                    episode_number: Some(episode),
                    series_folder: Some(FLASH.into()),
                    series_year: None,
                    series_key: None,
                }),
                ..MediaItem::default()
            },
        )
        .await
        .unwrap();
    }
    let token = stores.issue(uid, "test").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state)
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await
    };
}

fn requests(token: &str) -> Vec<test::TestRequest> {
    let auth = ("X-Emby-Token", token.to_string());
    vec![
        test::TestRequest::get().uri("/admin/series-identity"),
        test::TestRequest::get().uri("/admin/series-identity/search?Name=Flash"),
        test::TestRequest::post()
            .uri("/admin/series-identity/pick")
            .set_json(serde_json::json!({"SeriesKey": FLASH, "Provider": "tvdb", "Id": "1"})),
        test::TestRequest::post()
            .uri("/admin/series-identity/confirm")
            .set_json(serde_json::json!({"SeriesKey": FLASH})),
        test::TestRequest::post()
            .uri("/admin/series-identity/move")
            .set_json(serde_json::json!({"Folder": FLASH, "Paths": [], "TargetYear": 2014})),
        test::TestRequest::post()
            .uri("/admin/series-identity/keep-together")
            .set_json(serde_json::json!({"Folder": FLASH})),
        test::TestRequest::delete().uri("/admin/series-identity/overrides/1"),
    ]
    .into_iter()
    .map(|r| r.insert_header(auth.clone()))
    .collect()
}

#[actix_web::test]
async fn a_non_admin_is_refused_every_show_identity_call() {
    let (state, token) = seed(false).await;
    let app = app!(state);
    for req in requests(&token) {
        let req = req.to_request();
        let uri = req.uri().to_string();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 403, "{uri}");
    }
}

#[actix_web::test]
async fn a_move_shows_on_the_overview_and_can_be_removed() {
    let (state, token) = seed(true).await;
    let app = app!(state);
    let auth = ("X-Emby-Token", token.as_str());
    let moved = format!("{FLASH}/Season 1/The Flash - S01E02.mkv");

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/admin/series-identity/move")
            .insert_header(auth)
            .set_json(serde_json::json!({
                "Folder": FLASH, "Paths": [moved], "TargetYear": 2014, "TargetName": "The Flash"
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let done: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(done["Moved"], serde_json::json!([moved]));

    let v: serde_json::Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri("/admin/series-identity")
            .insert_header(auth)
            .to_request(),
    )
    .await;
    let groups = &v["Splits"][0]["Groups"];
    assert_eq!(v["Splits"][0]["Folder"], FLASH, "{v}");
    assert_eq!(groups.as_array().map(Vec::len), Some(2), "{v}");
    let over = &v["Overrides"][0];
    assert_eq!(over["Path"], moved.as_str(), "{v}");
    assert_eq!(over["Source"], "admin");
    assert_eq!(over["CreatedBy"], "boss");
    assert_eq!(over["KeepsTogether"], false);

    let id = over["Id"].as_i64().unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/admin/series-identity/overrides/{id}"))
            .insert_header(auth)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let v: serde_json::Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri("/admin/series-identity")
            .insert_header(auth)
            .to_request(),
    )
    .await;
    assert_eq!(v["Splits"], serde_json::json!([]), "{v}");
    assert_eq!(v["Overrides"], serde_json::json!([]), "{v}");

    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/admin/series-identity/overrides/{id}"))
            .insert_header(auth)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 404);
}
