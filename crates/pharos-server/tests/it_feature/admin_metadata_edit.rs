//! Spec 015 US3 / ADR-0097: an admin edits an item's or a show's metadata,
//! the edit survives a rescan, and non-admins are refused (V1030–V1032).

use crate::common::{build_app, seed_rich};
use actix_web::test;
use pharos_core::{MediaItem, MediaKind, MediaMetadata, MediaStore, SeriesInfo};
use serde_json::{json, Value};

/// Sends `req` with `token` and answers the status and the JSON body
/// (`null` when there is none).
macro_rules! call {
    ($app:expr, $req:expr, $token:expr $(,)?) => {{
        let resp = test::call_service(
            $app,
            $req.insert_header(("X-Emby-Token", $token.as_str()))
                .to_request(),
        )
        .await;
        let status = resp.status().as_u16();
        let body = test::read_body(resp).await;
        (
            status,
            serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
        )
    }};
}

#[actix_web::test]
async fn an_edit_is_shown_and_survives_a_rescan() {
    let fx = seed_rich().await;
    let app = test::init_service(build_app(fx.state.clone())).await;
    let uri = format!("/Admin/Items/{}/Metadata", fx.rich_item_id);

    let (status, view) = call!(&app, test::TestRequest::get().uri(&uri), &fx.admin_token);
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["Target"], "item");
    assert_eq!(view["Kind"], "Movie");
    assert_eq!(view["Fields"]["Overview"], "A thoroughly annotated film.");
    assert_eq!(view["Locked"], json!([]));

    let (status, view) = call!(
        &app,
        test::TestRequest::post().uri(&uri).set_json(json!({
            "Fields": {"Title": "Edited Movie", "Overview": "Admin text.", "Tagline": null,
                       "ProductionYear": 2020, "PremiereDate": "2020-03-01"}
        })),
        &fx.admin_token,
    );
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["Name"], "Edited Movie");
    assert_eq!(view["Fields"]["PremiereDate"], "2020-03-01");
    assert_eq!(
        view["Locked"],
        json!([
            "Title",
            "Overview",
            "Tagline",
            "ProductionYear",
            "PremiereDate"
        ])
    );

    // The scanner writes the file's own metadata again.
    fx.state
        .stores
        .put(MediaItem {
            id: fx.rich_item_id,
            path: "/libA/rich-movie.mkv".into(),
            title: "Rich Movie".into(),
            kind: MediaKind::Movie,
            metadata: MediaMetadata {
                overview: Some("A thoroughly annotated film.".into()),
                official_rating: Some("R".into()),
                production_year: Some(2021),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();

    let req = test::TestRequest::get().uri(&format!("/Items/{}", fx.rich_item_id));
    let (status, item) = call!(&app, req, &fx.admin_token);
    assert_eq!(status, 200, "{item}");
    assert_eq!(item["Name"], "Edited Movie");
    assert_eq!(item["Overview"], "Admin text.");
    assert_eq!(item["ProductionYear"], 2020);
    assert_eq!(
        item["OfficialRating"], "R",
        "an unlocked field takes the rescan's value"
    );

    // Unlocking hands the field back to the next scan.
    let (status, view) = call!(
        &app,
        test::TestRequest::post()
            .uri(&uri)
            .set_json(json!({"Unlock": ["Overview"]})),
        &fx.admin_token,
    );
    assert_eq!(status, 200, "{view}");
    assert!(!view["Locked"]
        .as_array()
        .unwrap()
        .contains(&json!("Overview")));
    assert_eq!(view["Fields"]["Overview"], "Admin text.");

    // A forced sort title, then a blank one: back to the title's own, unlocked.
    let (status, view) = call!(
        &app,
        test::TestRequest::post()
            .uri(&uri)
            .set_json(json!({"Fields": {"SortName": "Movie, Edited"}})),
        &fx.admin_token,
    );
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["Fields"]["SortName"], "Movie, Edited");
    assert!(view["Locked"].to_string().contains("SortName"), "{view}");
    let (status, view) = call!(
        &app,
        test::TestRequest::post()
            .uri(&uri)
            .set_json(json!({"Fields": {"SortName": null}})),
        &fx.admin_token,
    );
    assert_eq!(status, 200, "{view}");
    assert_eq!(
        view["Fields"]["SortName"],
        pharos_core::sort_name("Edited Movie")
    );
    assert!(!view["Locked"].to_string().contains("SortName"), "{view}");
}

#[actix_web::test]
async fn a_refusal_names_the_field_and_value() {
    let fx = seed_rich().await;
    let app = test::init_service(build_app(fx.state.clone())).await;
    let uri = format!("/Admin/Items/{}/Metadata", fx.rich_item_id);
    for (body, field, needle) in [
        (
            json!({"Fields": {"ProductionYear": 19999}}),
            "ProductionYear",
            "19999",
        ),
        (
            json!({"Fields": {"CommunityRating": 12}}),
            "CommunityRating",
            "12",
        ),
        (json!({"Fields": {"Title": "  "}}), "Title", "empty"),
        (json!({"Fields": {"Genres": ["Drama"]}}), "Genres", "item"),
        (json!({"Fields": ["Title"]}), "Fields", "[\"Title\"]"),
        (json!({"Unlock": "Title"}), "Unlock", "\"Title\""),
    ] {
        let (status, err) = call!(
            &app,
            test::TestRequest::post().uri(&uri).set_json(&body),
            &fx.admin_token,
        );
        assert_eq!(status, 400, "{body}: {err}");
        assert_eq!(err["Field"], field, "{err}");
        assert!(err["Reason"].as_str().unwrap().contains(needle), "{err}");
    }
    let item = fx.state.stores.get(fx.rich_item_id).await.unwrap();
    assert!(item.locked.is_empty(), "a refused edit writes nothing");
}

#[actix_web::test]
async fn non_admins_are_refused_and_unknown_ids_are_404() {
    let fx = seed_rich().await;
    let app = test::init_service(build_app(fx.state.clone())).await;
    let uri = format!("/Admin/Items/{}/Metadata", fx.rich_item_id);
    let (status, _) = call!(&app, test::TestRequest::get().uri(&uri), &fx.user_token);
    assert_eq!(status, 403);
    let req = test::TestRequest::post()
        .uri(&uri)
        .set_json(json!({"Fields": {"Title": "x"}}));
    let (status, _) = call!(&app, req, &fx.user_token);
    assert_eq!(status, 403);
    let req = test::TestRequest::post()
        .uri(&format!("/Items/{}", fx.rich_item_id))
        .set_json(json!({"Name": "x"}));
    let (status, _) = call!(&app, req, &fx.user_token);
    assert_eq!(status, 403);
    assert_eq!(
        fx.state.stores.get(fx.rich_item_id).await.unwrap().title,
        "Rich Movie"
    );
    let (status, _) = call!(
        &app,
        test::TestRequest::get().uri("/Admin/Items/999999/Metadata"),
        &fx.admin_token,
    );
    assert_eq!(status, 404);
}

#[actix_web::test]
async fn a_show_is_edited_through_its_series_id() {
    let fx = seed_rich().await;
    fx.state
        .stores
        .put(MediaItem {
            id: 300,
            path: "/libA/Show/S01E01.mkv".into(),
            title: "Pilot".into(),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "Edit Show".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some("/libA/Show".into()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let app = test::init_service(build_app(fx.state.clone())).await;
    let series_id = pharos_jellyfin_api::dto::series_id_for_key(Some("/libA/Show"), "Edit Show");
    let uri = format!("/Admin/Items/{series_id}/Metadata");
    let (status, view) = call!(
        &app,
        test::TestRequest::post().uri(&uri).set_json(json!({
            "Fields": {"Overview": "A show about edits.", "Genres": ["Drama", "drama", "Crime"]}
        })),
        &fx.admin_token,
    );
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["Target"], "series");
    assert_eq!(view["Name"], "Edit Show");
    assert_eq!(view["Fields"]["Genres"], json!(["Drama", "Crime"]));
    assert_eq!(view["Locked"], json!(["Overview", "Genres"]));
    let (status, err) = call!(
        &app,
        test::TestRequest::post()
            .uri(&uri)
            .set_json(json!({"Fields": {"Title": "Renamed"}})),
        &fx.admin_token,
    );
    assert_eq!(status, 400, "a show's name is its identity: {err}");
}

#[actix_web::test]
async fn jellyfins_editor_save_writes_only_what_changed() {
    let fx = seed_rich().await;
    let app = test::init_service(build_app(fx.state.clone())).await;
    let req = test::TestRequest::get().uri(&format!("/Items/{}", fx.rich_item_id));
    let (_, mut dto) = call!(&app, req, &fx.admin_token);
    dto["Overview"] = json!("Changed in jellyfin-web.");
    let req = test::TestRequest::post()
        .uri(&format!("/Items/{}", fx.rich_item_id))
        .set_json(&dto);
    let (status, body) = call!(&app, req, &fx.admin_token);
    assert_eq!(status, 204, "{body}");
    let item = fx.state.stores.get(fx.rich_item_id).await.unwrap();
    assert_eq!(
        item.metadata.overview.as_deref(),
        Some("Changed in jellyfin-web.")
    );
    assert_eq!(
        item.locked.iter().map(|f| f.wire()).collect::<Vec<_>>(),
        ["Overview"],
        "the unchanged fields the editor posted back stay unlocked"
    );
}

#[actix_web::test]
async fn an_unchanged_tomatometer_is_not_saved_as_the_critic_rating() {
    let fx = seed_rich().await;
    let mut item = fx.state.stores.get(fx.rich_item_id).await.unwrap();
    item.metadata.critic_rating = Some(75.0);
    fx.state.stores.put(item).await.unwrap();
    pharos_core::RatingsStore::set_ratings(
        &fx.state.stores,
        &pharos_core::RatingsTarget::Item(fx.rich_item_id),
        pharos_core::ExternalRatings {
            tomatometer: Some(91),
            ..Default::default()
        },
        1_000,
    )
    .await
    .unwrap();
    let app = test::init_service(build_app(fx.state.clone())).await;
    let req = test::TestRequest::get().uri(&format!("/Items/{}", fx.rich_item_id));
    let (_, mut dto) = call!(&app, req, &fx.admin_token);
    assert_eq!(dto["CriticRating"], 91.0, "{dto}");
    dto["Overview"] = json!("Changed in jellyfin-web.");
    let req = test::TestRequest::post()
        .uri(&format!("/Items/{}", fx.rich_item_id))
        .set_json(&dto);
    let (status, body) = call!(&app, req, &fx.admin_token);
    assert_eq!(status, 204, "{body}");
    let item = fx.state.stores.get(fx.rich_item_id).await.unwrap();
    assert_eq!(item.metadata.critic_rating, Some(75.0));
    assert_eq!(
        item.locked.iter().map(|f| f.wire()).collect::<Vec<_>>(),
        ["Overview"],
        "the Tomatometer the answer showed is not an edit of the critic rating"
    );
}
