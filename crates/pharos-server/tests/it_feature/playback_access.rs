//! B277: the playback reports (`/Sessions/Playing`, `.../Progress`,
//! `.../Stopped`) answer an item the caller may not reach as 404 and write
//! nothing for it: no resume position, no user data row.
//! Disarm check: remove the `check_item` call in `services/play_state.rs`
//! (`report` and `stop`) and the `require_item_access` call in
//! `api/jellyfin/sessions.rs` (`playing_started`); the hidden-item test goes red.

use actix_web::test;
use pharos_core::{UserDataStore as _, UserItemData, UserPolicy, UserStore as _};

use crate::common::{build_app, seed_rich, Fixture};

async fn restrict_to_library_a(f: &Fixture) {
    let policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![f.lib_a_wire.clone()],
        ..UserPolicy::default()
    };
    f.state.stores.set_policy(f.user_id, policy).await.unwrap();
}

async fn stored(f: &Fixture, item: u64) -> UserItemData {
    f.state
        .stores
        .get_user_data(f.user_id, item)
        .await
        .unwrap_or_default()
}

#[actix_web::test]
async fn a_hidden_item_is_refused_by_every_playback_report_and_written_nothing() {
    let f = seed_rich().await;
    restrict_to_library_a(&f).await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let id = f.other_item_id;
    let position = 50_000_000u64;

    let requests = [
        (
            "/Sessions/Playing",
            serde_json::json!({ "ItemId": id.to_string(), "PlaySessionId": "hidden-start" }),
        ),
        (
            "/Sessions/Playing/Progress",
            serde_json::json!({
                "ItemId": id.to_string(),
                "PlaySessionId": "hidden-progress",
                "PositionTicks": position,
            }),
        ),
        (
            "/Sessions/Playing/Stopped",
            serde_json::json!({
                "ItemId": id.to_string(),
                "PlaySessionId": "hidden-stop",
                "PositionTicks": position,
            }),
        ),
    ];
    let mut failures = Vec::new();
    for (uri, body) in requests {
        let req = test::TestRequest::post()
            .uri(uri)
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .set_json(body)
            .to_request();
        let status = test::call_service(&app, req).await.status().as_u16();
        if status != 404 {
            failures.push(format!("POST {uri}: {status}"));
        }
    }
    assert!(
        failures.is_empty(),
        "hidden item not refused by the playback reports:\n{}",
        failures.join("\n")
    );
    assert_eq!(
        stored(&f, id).await,
        UserItemData::default(),
        "a report for a hidden item wrote user data"
    );
}

/// The same reports still record a playback of an item in an allowed library.
#[actix_web::test]
async fn a_visible_item_playback_is_still_recorded() {
    let f = seed_rich().await;
    restrict_to_library_a(&f).await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let id = f.rich_item_id;

    let progress = test::TestRequest::post()
        .uri("/Sessions/Playing/Progress")
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": id.to_string(),
            "PlaySessionId": "visible-progress",
            "PositionTicks": 10_000_000u64,
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, progress).await.status().as_u16(),
        204
    );
    assert_eq!(stored(&f, id).await.last_played_position_ticks, 10_000_000);

    let stopped = test::TestRequest::post()
        .uri("/Sessions/Playing/Stopped")
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": id.to_string(),
            "PlaySessionId": "visible-stop",
            "PositionTicks": 20_000_000u64,
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, stopped).await.status().as_u16(),
        204
    );
    assert_eq!(stored(&f, id).await.last_played_position_ticks, 20_000_000);
}
