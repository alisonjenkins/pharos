//! A person's page (`Library.person`): what it lists for whom, and the
//! `pharos_person_page_total{outcome}` count each answer leaves.

use pharos_core::{PersonKind, PersonRef, PersonStore as _, UserPolicy};
use pharos_server::services::person::{self, PersonError};

use crate::common::seed_rich;
use crate::telemetry::counter;

/// The page keeps to the viewer's policy: a viewer who may see every library
/// gets the film and the show, one limited to a library holding neither is
/// refused as if the person did not exist, and an unknown person is not
/// found. Each answer counts under its own outcome.
/// Disarm check: with `apply_policy_scope` skipped in `services::person`, the
/// restricted viewer gets the page and `hidden` stays 0; with the hidden check
/// counting any visible credit, the film they only directed lets them in.
#[actix_web::test]
async fn the_page_keeps_to_the_policy_and_counts_each_outcome() {
    let fx = seed_rich().await;
    crate::library::seed_movies(&fx).await;
    crate::library::seed_tv_library(&fx).await;
    let lead = |character: &str| PersonRef {
        name: "Lead Actor".into(),
        kind: PersonKind::Actor,
        character: Some(character.into()),
        ..PersonRef::default()
    };
    fx.state
        .stores
        .link_item_people(101, &[lead("Pilot")])
        .await
        .unwrap();
    fx.state
        .stores
        .link_item_people(604, &[lead("Elder")])
        .await
        .unwrap();
    // Directing what the restricted viewer can see does not make the page
    // theirs to see: only acting credits count.
    let directing = PersonRef {
        name: "Lead Actor".into(),
        kind: PersonKind::Director,
        role: Some("Director".into()),
        ..PersonRef::default()
    };
    fx.state
        .stores
        .link_item_people(201, &[directing])
        .await
        .unwrap();
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let restricted = pharos_core::User {
        policy: UserPolicy {
            enable_all_folders: false,
            enabled_folders: vec![fx.lib_b_wire.clone()],
            ..UserPolicy::default()
        },
        ..user.clone()
    };
    let wire_id = pharos_core::person_wire_id("Lead Actor");

    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let page = person::page(&fx.state, &user, &wire_id).await.unwrap();
    let films: Vec<_> = page.films.iter().map(|f| f.item.id).collect();
    assert_eq!(films, [101]);
    let shows: Vec<_> = page
        .shows
        .iter()
        .map(|s| (s.series_name.as_str(), s.characters.clone()))
        .collect();
    assert_eq!(shows, [("Mature", vec!["Elder".to_owned()])]);

    let hidden = person::page(&fx.state, &restricted, &wire_id).await;
    assert!(matches!(hidden, Err(PersonError::Hidden)), "{hidden:?}");

    let nobody = pharos_core::person_wire_id("Nobody At All");
    let missing = person::page(&fx.state, &user, &nobody).await;
    assert!(matches!(missing, Err(PersonError::NotFound)), "{missing:?}");

    let outcome = |o| counter(&snapshotter, "pharos_person_page_total", &[("outcome", o)]);
    assert_eq!(outcome("shown"), 1);
    assert_eq!(outcome("hidden"), 1);
    assert_eq!(outcome("not_found"), 1);
    assert_eq!(outcome("empty"), 0);
}
