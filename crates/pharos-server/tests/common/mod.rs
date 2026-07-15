//! Shared fixture for the jellyfin-web feature-inventory suites
//! (`jellyfin_feature_*.rs`). Extends the standard per-file `seed()` with
//! richer material so the metadata / policy / library tests have something
//! to assert against:
//!
//! - a **rich item** carrying linked people / studios / tags / genres and
//!   descriptive [`MediaMetadata`] (overview, ratings, provider ids),
//! - **two** typed libraries (`/libA`, `/libB`) so library-access
//!   enforcement has two folders to distinguish,
//! - **two** users — one admin, one non-admin — so policy round-trips and
//!   enforcement checks have distinct subjects.
//!
//! Every assertion in the suites is made against the **Jellyfin wire JSON**
//! (HTTP response bodies), never pharos internal types, so the `#[ignore]`d
//! backlog tests compile today even though the internal model has not yet
//! grown the fields they exercise.
#![allow(dead_code)]

use actix_web::{web, App};
use pharos_core::{
    GenreStore, LibraryKind, LibraryStore, MediaItem, MediaKind, MediaMetadata, MediaStore,
    PersonKind, PersonRef, PersonStore, ProviderIds, SecretString, StudioStore, TagStore,
    TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

/// Everything the feature suites need, handed back from [`seed_rich`].
pub struct Fixture {
    pub state: web::Data<AppState>,
    /// Bearer token for the admin user.
    pub admin_token: String,
    pub admin_id: UserId,
    /// Bearer token for the non-admin user.
    pub user_token: String,
    pub user_id: UserId,
    /// The movie under `/libA` with linked people/studios/tags/genres and
    /// populated descriptive metadata.
    pub rich_item_id: u64,
    /// Plain movie under `/libB`, used by the library-access enforcement test.
    pub other_item_id: u64,
    /// Stable wire ids of the two libraries (`EnabledFolders` candidates).
    pub lib_a_wire: String,
    pub lib_b_wire: String,
}

/// Seed an in-memory store with the rich fixture and wrap it in `AppState`.
pub async fn seed_rich() -> Fixture {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();

    // Two users: one admin, one restricted.
    let admin_id = UserId::new();
    stores
        .create(UserRecord {
            id: admin_id,
            name: "ali".into(),
            password_hash: hash.clone(),
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let user_id = UserId::new();
    stores
        .create(UserRecord {
            id: user_id,
            name: "guest".into(),
            password_hash: hash,
            policy: UserPolicy { admin: false },
        })
        .await
        .unwrap();
    let admin_token = stores
        .issue(admin_id, "test")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();
    let user_token = stores
        .issue(user_id, "test")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();

    // Two typed libraries. Wire ids are arbitrary-but-stable here; the
    // handlers key libraries by root path, and tests that need the real id
    // read it back from `/Library/VirtualFolders`.
    let lib_a_wire = "a".repeat(32);
    let lib_b_wire = "b".repeat(32);
    stores
        .upsert_library("Movies A", "/libA", LibraryKind::Movies, &lib_a_wire)
        .await
        .unwrap();
    stores
        .upsert_library("Movies B", "/libB", LibraryKind::Movies, &lib_b_wire)
        .await
        .unwrap();

    // Rich item under /libA.
    let rich_item_id: u64 = 100;
    stores
        .put(MediaItem {
            id: rich_item_id,
            path: "/libA/rich-movie.mkv".into(),
            title: "Rich Movie".into(),
            kind: MediaKind::Movie,
            metadata: MediaMetadata {
                overview: Some("A thoroughly annotated film.".into()),
                official_rating: Some("PG-13".into()),
                community_rating: Some(8.4),
                production_year: Some(2021),
                provider_ids: ProviderIds {
                    imdb: Some("tt1234567".into()),
                    tmdb: Some("55555".into()),
                    ..Default::default()
                },
                production_locations: vec!["USA".into()],
                trailers: vec!["https://youtu.be/dQw4w9WgXcQ".into()],
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();

    // Plain item under /libB (for folder-access enforcement).
    let other_item_id: u64 = 200;
    stores
        .put(MediaItem {
            id: other_item_id,
            path: "/libB/other-movie.mkv".into(),
            title: "Other Movie".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();

    // Link the rich item's associations.
    stores
        .link_item_people(
            rich_item_id,
            &[
                PersonRef {
                    name: "Jane Star".into(),
                    role: Some("Protagonist".into()),
                    kind: PersonKind::Actor,
                    character: Some("Protagonist".into()),
                    ..Default::default()
                },
                PersonRef {
                    name: "Dir Ector".into(),
                    kind: PersonKind::Director,
                    ..Default::default()
                },
            ],
        )
        .await
        .unwrap();
    stores
        .link_item_studios(rich_item_id, &["Pharos Studios".to_string()])
        .await
        .unwrap();
    stores
        .link_item_tags(rich_item_id, &["award-winner".to_string()])
        .await
        .unwrap();
    stores
        .link_item_genres(rich_item_id, &["Action".to_string(), "Drama".to_string()])
        .await
        .unwrap();

    // Path-prefix backfill so each item resolves to its library.
    stores.backfill_library_ids().await.unwrap();

    let state = web::Data::new(AppState::new(stores, "test".into()));
    let libraries = state.stores.libraries().await.unwrap();
    state.set_libraries(libraries);

    Fixture {
        state,
        admin_token,
        admin_id,
        user_token,
        user_id,
        rich_item_id,
        other_item_id,
        lib_a_wire,
        lib_b_wire,
    }
}

/// The standard app wiring used by every feature suite: lowercase-path
/// middleware + the full Jellyfin route table. Mirrors `jellyfin_items.rs`.
pub fn build_app(
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
