#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Music browse parity (B47) — every request below is the EXACT query the
//! deployed jellyfin-web 10.11.8 bundle sends (extracted from
//! `itemDetails.*.chunk.js`), so these lock the real client flows:
//!
//! - artist detail children (`fe()`): `ParentId={artist}` +
//!   `SortBy=PremiereDate,ProductionYear,SortName` → ALBUM cards
//! - album detail children: `ParentId={album}` +
//!   `SortBy=ParentIndexNumber,IndexNumber,SortName` → tracks in track order
//! - "More From {artist}" (album page): `IncludeItemTypes=MusicAlbum,
//!   Recursive, ExcludeItemIds={album}, AlbumArtistIds={artist}`
//! - "Appears On" (artist page): same with `ContributingArtistIds={artist}`
//! - "More Like This": `/Items/{id}/Similar` on synth album/artist ids —
//!   music only, never TV/movies

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_jellyfin_api::dto::{album_id_for, artist_id_for};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

#[allow(clippy::too_many_arguments)]
fn track(
    id: u64,
    title: &str,
    album: Option<&str>,
    artist: &str,
    album_artist: &str,
    track_number: Option<u32>,
    year: Option<u32>,
    genre: &str,
) -> MediaItem {
    MediaItem {
        id,
        path: format!("/m/music/{id}.flac").into(),
        title: title.into(),
        kind: MediaKind::Audio,
        probe: MediaProbe {
            artist: Some(artist.into()),
            album: album.map(Into::into),
            album_artist: Some(album_artist.into()),
            genre: Some(genre.into()),
            track_number,
            year,
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn seed() -> (web::Data<AppState>, String) {
    use pharos_core::TokenStore;
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();

    // Limp Bizkit: two albums (out-of-alphabetical-order years + track
    // numbers deliberately shuffled by id), one loose track, one
    // appears-on compilation track. Rammstein: one album sharing genre.
    // Movie + Episode share the genre string to prove class gating.
    for m in [
        track(
            600,
            "Nookie",
            Some("Significant Other"),
            "Limp Bizkit",
            "Limp Bizkit",
            Some(3),
            Some(1999),
            "Nu Metal",
        ),
        track(
            601,
            "Break Stuff",
            Some("Significant Other"),
            "Limp Bizkit",
            "Limp Bizkit",
            Some(2),
            Some(1999),
            "Nu Metal",
        ),
        track(
            602,
            "My Way",
            Some("Chocolate Starfish"),
            "Limp Bizkit",
            "Limp Bizkit",
            Some(1),
            Some(2000),
            "Nu Metal",
        ),
        track(
            603,
            "Du Hast",
            Some("Sehnsucht"),
            "Rammstein",
            "Rammstein",
            Some(5),
            Some(1997),
            "Nu Metal",
        ),
        track(
            604,
            "Loose Demo",
            None,
            "Limp Bizkit",
            "Limp Bizkit",
            None,
            None,
            "Nu Metal",
        ),
        track(
            605,
            "Guest Verse",
            Some("Some Compilation"),
            "Limp Bizkit",
            "Various Artists",
            Some(9),
            Some(2001),
            "Nu Metal",
        ),
    ] {
        stores.put(m).await.unwrap();
    }
    // Video items with the SAME genre string — must never leak into music.
    stores
        .put(MediaItem {
            id: 700,
            path: "/m/movies/a.mkv".into(),
            title: "A Nu Metal Documentary".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                genre: Some("Nu Metal".into()),
                video_codec: Some("h264".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 701,
            path: "/m/tv/s01e01.mkv".into(),
            title: "ep".into(),
            kind: MediaKind::Episode,
            probe: MediaProbe {
                genre: Some("Nu Metal".into()),
                video_codec: Some("h264".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();

    let state = web::Data::new(AppState::new(stores, "test".into()));
    (state, token.0.expose().to_string())
}

fn build_app(
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

macro_rules! get_items {
    ($app:expr, $token:expr, $uri:expr) => {{
        let req = test::TestRequest::get()
            .uri($uri)
            .insert_header(("X-Emby-Token", $token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&$app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|e| {
            panic!(
                "non-JSON body for {}: {e}: {}",
                $uri,
                String::from_utf8_lossy(&body)
            )
        });
        v["Items"].as_array().cloned().unwrap_or_default()
    }};
}

fn names(items: &[serde_json::Value]) -> Vec<String> {
    items
        .iter()
        .map(|i| {
            format!(
                "{}:{}",
                i["Type"].as_str().unwrap_or("?"),
                i["Name"].as_str().unwrap_or("?")
            )
        })
        .collect()
}

#[actix_web::test]
async fn artist_children_are_albums_plus_loose_tracks() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let artist = artist_id_for("Limp Bizkit");
    // EXACT jellyfin-web artist-detail children query.
    let items = get_items!(app, token, &format!("/Items?ParentId={artist}&Fields=ItemCounts,PrimaryImageAspectRatio,CanDelete,MediaSourceCount&SortBy=PremiereDate,ProductionYear,SortName"));
    let got = names(&items);
    // Year order: Significant Other (1999) then Chocolate Starfish (2000),
    // then the loose (album-less) track as a plain Audio row.
    assert_eq!(
        got,
        vec![
            "MusicAlbum:Significant Other",
            "MusicAlbum:Chocolate Starfish",
            "Audio:Loose Demo",
        ],
        "artist children must be the discography, not raw tracks"
    );
    // Album cards need the child count + year for the card subtitle.
    assert_eq!(items[0]["ChildCount"], 2);
    assert_eq!(items[0]["ProductionYear"], 1999);
    assert_eq!(items[0]["AlbumArtists"][0]["Name"], "Limp Bizkit");
    // A synth album MUST advertise a Primary image tag, else clients never
    // request the cover and album cards render blank (B-music). The image
    // endpoint resolves the tag to a child track's artwork.
    let primary = items[0]["ImageTags"]["Primary"].as_str();
    assert!(
        primary.is_some_and(|t| !t.is_empty()),
        "synth album must advertise a non-empty Primary ImageTag, got {:?}",
        items[0]["ImageTags"]
    );
}

#[actix_web::test]
async fn album_children_are_tracks_in_track_order() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let album = album_id_for("Significant Other");
    // EXACT jellyfin-web album-detail children query.
    let items = get_items!(app, token, &format!("/Items?ParentId={album}&Fields=ItemCounts,PrimaryImageAspectRatio,CanDelete,MediaSourceCount&SortBy=ParentIndexNumber,IndexNumber,SortName"));
    assert_eq!(
        names(&items),
        vec!["Audio:Break Stuff", "Audio:Nookie"],
        "tracks must come back in TRACK order (2 then 3), not id/title order"
    );
    // The wire track numbers drive jellyfin-web's numbering column.
    assert_eq!(items[0]["IndexNumber"], 2);
    assert_eq!(items[1]["IndexNumber"], 3);
}

#[actix_web::test]
async fn more_from_artist_rail_is_albums_only() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let artist = artist_id_for("Limp Bizkit");
    let album = album_id_for("Significant Other");
    // EXACT "More From {artist}" query from the album detail page.
    let items = get_items!(app, token, &format!("/Items?IncludeItemTypes=MusicAlbum&Recursive=true&ExcludeItemIds={album}&SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending&AlbumArtistIds={artist}"));
    assert_eq!(
        names(&items),
        vec!["MusicAlbum:Chocolate Starfish"],
        "the rail must show the artist's OTHER albums — never TV/movies, \
         never other artists, never the page's own album"
    );
}

#[actix_web::test]
async fn appears_on_rail_shows_contributions_not_own_albums() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let artist = artist_id_for("Limp Bizkit");
    // EXACT "Appears On" query from the artist detail page.
    let items = get_items!(app, token, &format!("/Items?IncludeItemTypes=MusicAlbum&Recursive=true&ExcludeItemIds={artist}&SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending&ContributingArtistIds={artist}"));
    assert_eq!(
        names(&items),
        vec!["MusicAlbum:Some Compilation"],
        "Appears On = albums the artist performs on but doesn't own"
    );
}

#[actix_web::test]
async fn similar_on_music_is_music_only() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    // Album "More Like This" (with the album artist excluded, as the
    // client sends): the shared-genre Rammstein album — and NEVER the
    // same-genre movie/episode.
    let album = album_id_for("Significant Other");
    let artist = artist_id_for("Limp Bizkit");
    let items = get_items!(
        app,
        token,
        &format!("/Items/{album}/Similar?limit=12&ExcludeArtistIds={artist}")
    );
    let got = names(&items);
    assert!(
        got.iter().all(|n| n.starts_with("MusicAlbum:")),
        "album similar must be albums only, got {got:?}"
    );
    assert!(
        got.contains(&"MusicAlbum:Sehnsucht".to_string()),
        "shared-genre album expected, got {got:?}"
    );

    // Artist "More Like This": genre-adjacent artists.
    let items = get_items!(app, token, &format!("/Items/{artist}/Similar?limit=12"));
    let got = names(&items);
    assert!(
        got.iter().all(|n| n.starts_with("MusicArtist:")),
        "artist similar must be artists only, got {got:?}"
    );
    assert!(
        got.contains(&"MusicArtist:Rammstein".to_string()),
        "genre-adjacent artist expected, got {got:?}"
    );

    // Track-level similar: the same-genre VIDEO items must never leak in.
    let items = get_items!(
        app,
        token,
        "/Items/00000000000000000000000000000258/Similar?limit=12"
    ); // 600 hex
    let got = names(&items);
    assert!(
        got.iter().all(|n| n.starts_with("Audio:")),
        "audio similar must be music only (class gate), got {got:?}"
    );
}

/// B92 — the Android TV kotlin SDK re-serialises every id DASHED. The music
/// query filters (`AlbumArtistIds` / `ExcludeItemIds` via `id_set`) and the
/// synth-id `/Items/{id}/Similar` path (`music_similar`) compared against the
/// dashless `album_id_for` / `artist_id_for` hash, so a dashed id matched
/// nothing — an empty "More From" rail and empty "More Like This". Canonicalise
/// so the dashed forms resolve identically to the dashless.
#[actix_web::test]
async fn dashed_music_ids_resolve_in_filters_and_similar() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let dash = |s: String| {
        format!(
            "{}-{}-{}-{}-{}",
            &s[0..8],
            &s[8..12],
            &s[12..16],
            &s[16..20],
            &s[20..32],
        )
    };
    let artist = dash(artist_id_for("Limp Bizkit"));
    let album = dash(album_id_for("Significant Other"));
    // "More From {artist}" — dashed AlbumArtistIds + ExcludeItemIds (id_set).
    let items = get_items!(app, token, &format!("/Items?IncludeItemTypes=MusicAlbum&Recursive=true&ExcludeItemIds={album}&SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending&AlbumArtistIds={artist}"));
    assert_eq!(
        names(&items),
        vec!["MusicAlbum:Chocolate Starfish"],
        "dashed AlbumArtistIds + ExcludeItemIds must resolve like dashless: {items:?}"
    );
    // "More Like This" — dashed synth album id (music_similar `wanted`).
    let items = get_items!(
        app,
        token,
        &format!("/Items/{album}/Similar?limit=12&ExcludeArtistIds={artist}")
    );
    let got = names(&items);
    assert!(
        got.contains(&"MusicAlbum:Sehnsucht".to_string()),
        "dashed album Similar must resolve, got {got:?}"
    );
}
