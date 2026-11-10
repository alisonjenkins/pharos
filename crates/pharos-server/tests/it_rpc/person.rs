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

/// A provider that knows person `1`, fails on `2`, has lost `3`, and counts
/// every ask.
struct FakeDetails(std::sync::atomic::AtomicUsize);

impl pharos_server::person_details::PersonDetailsSource for FakeDetails {
    fn fetch<'a>(
        &'a self,
        tmdb_id: &'a str,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<pharos_server::person_details::Fetched, pharos_server::person_details::DetailsMiss>,
    > {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let answer = match tmdb_id {
            "1" => Ok(pharos_server::person_details::Fetched {
                biography: Some("Born by the sea.".into()),
                birth_date: Some("1956-07-09".into()),
                imdb_id: Some("nm0000158".into()),
                wikidata_id: Some("Q2263".into()),
                ..Default::default()
            }),
            "3" => Err(pharos_server::person_details::DetailsMiss::NotFound),
            _ => Err(pharos_server::person_details::DetailsMiss::Transport),
        };
        Box::pin(async move { answer })
    }
}

/// The page carries what the provider says about the person, asks once and
/// then shows what it kept, and still shows the page when the provider
/// fails or the person has no TMDB id. Each answer counts under
/// `pharos_person_details_total{outcome}`.
/// A provider that has lost someone leaves their kept details in place.
/// Disarm check: with `put_person_details` skipped, the second visit asks
/// again and `kept` stays 0; with the kept-on-not-found arm removed, Gone
/// Actor's biography is wiped.
#[actix_web::test]
async fn the_page_carries_the_providers_details_and_keeps_them() {
    let source = std::sync::Arc::new(FakeDetails(std::sync::atomic::AtomicUsize::new(0)));
    let fx = crate::common::seed_rich_with({
        let source = source.clone();
        move |state| state.with_person_details_source(source)
    })
    .await;
    crate::library::seed_movies(&fx).await;
    let actor = |name: &str, tmdb: Option<&str>| PersonRef {
        name: name.into(),
        kind: PersonKind::Actor,
        character: Some("Pilot".into()),
        provider_ids: tmdb.map(Into::into),
        ..PersonRef::default()
    };
    fx.state
        .stores
        .link_item_people(
            101,
            &[
                actor("Known Actor", Some("tmdb:1")),
                actor("Flaky Actor", Some("tmdb:2")),
                actor("Local Actor", None),
                actor("Gone Actor", Some("tmdb:3")),
            ],
        )
        .await
        .unwrap();
    let user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    let id = pharos_core::person_wire_id;

    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let known = person::page(&fx.state, &user, &id("Known Actor"))
        .await
        .unwrap();
    let details = known.details.expect("details");
    assert_eq!(details.biography.as_deref(), Some("Born by the sea."));
    assert_eq!(details.imdb_id.as_deref(), Some("nm0000158"));
    assert_eq!(details.wikidata_id.as_deref(), Some("Q2263"));
    let again = person::page(&fx.state, &user, &id("Known Actor"))
        .await
        .unwrap();
    assert_eq!(again.details.map(|d| d.biography), Some(details.biography));

    let flaky = person::page(&fx.state, &user, &id("Flaky Actor"))
        .await
        .unwrap();
    assert_eq!(flaky.details, None);
    assert_eq!(flaky.films.len(), 1, "the page shows without the provider");
    let local = person::page(&fx.state, &user, &id("Local Actor"))
        .await
        .unwrap();
    assert_eq!(local.details, None);

    // Stale kept details survive a provider that no longer has the person.
    let stale = pharos_core::PersonDetails {
        biography: Some("Kept from before.".into()),
        fetched_at: 0,
        ..pharos_core::PersonDetails::default()
    };
    fx.state
        .stores
        .put_person_details(&id("Gone Actor"), &stale)
        .await
        .unwrap();
    let gone = person::page(&fx.state, &user, &id("Gone Actor"))
        .await
        .unwrap();
    let gone = gone.details.expect("kept details");
    assert_eq!(gone.biography.as_deref(), Some("Kept from before."));
    assert!(gone.fetched_at > 0, "the refresh is recorded");

    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 3);
    let outcome = |o| {
        counter(
            &snapshotter,
            "pharos_person_details_total",
            &[("outcome", o)],
        )
    };
    assert_eq!(outcome("fetched"), 1);
    assert_eq!(outcome("kept"), 1);
    assert_eq!(outcome("transport"), 1);
    assert_eq!(outcome("no_provider_id"), 1);
    assert_eq!(outcome("not_found"), 1);
}

/// Search offers the actors whose name holds the words typed, leaving out
/// crew and anyone who acts only in what the viewer may not see.
/// Disarm check: with `acts_in_sight` skipped in `services::search`, the
/// restricted viewer is offered Lead Actor.
#[actix_web::test]
async fn search_offers_actors_the_viewer_may_see() {
    use pharos_server::services::search::{self, Hit};
    let fx = seed_rich().await;
    crate::library::seed_movies(&fx).await;
    let credit = |name: &str, kind| PersonRef {
        name: name.into(),
        kind,
        character: Some("Pilot".into()),
        ..PersonRef::default()
    };
    fx.state
        .stores
        .link_item_people(101, &[credit("Lead Actor", PersonKind::Actor)])
        .await
        .unwrap();
    fx.state
        .stores
        .link_item_people(
            201,
            &[
                credit("Second Lead", PersonKind::Actor),
                credit("Lead Director", PersonKind::Director),
            ],
        )
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
    let people = |hits: Vec<Hit>| -> Vec<String> {
        hits.into_iter()
            .filter_map(|hit| match hit {
                Hit::Person(_, p) => Some(p.name),
                _ => None,
            })
            .collect()
    };

    let all = search::search(&fx.state, &user, "lead", 0).await.unwrap();
    assert_eq!(people(all), ["Lead Actor", "Second Lead"]);
    let some = search::search(&fx.state, &restricted, "lead", 0)
        .await
        .unwrap();
    assert_eq!(people(some), ["Second Lead"]);
    let short = search::search(&fx.state, &user, "le", 0).await.unwrap();
    assert!(people(short).is_empty(), "two letters look for no one");
}
