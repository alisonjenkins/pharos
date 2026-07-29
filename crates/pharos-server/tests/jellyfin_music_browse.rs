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
    // Give "Significant Other" a real cover so the Primary-tag assertion below
    // tests the contract rather than the old unconditional stamp. Both its
    // tracks are covered because the representative is chosen by TITLE, not by
    // id — art on the wrong one leaves the album looking bare.
    for id in [600, 601] {
        pharos_core::MediaStore::set_artwork(
            &state.stores,
            id,
            "Primary",
            "local",
            &format!("/cache/primary/audio/{id}.jpg"),
        )
        .await
        .unwrap();
    }
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
    // An album whose tracks HAVE a cover must advertise a Primary tag, else
    // clients never request it and the cards render blank (B130's class). The
    // image endpoint resolves the tag to that child track's artwork.
    let primary = items[0]["ImageTags"]["Primary"].as_str();
    assert!(
        primary.is_some_and(|t| !t.is_empty()),
        "an album with cover art must advertise a Primary ImageTag, got {:?}",
        items[0]["ImageTags"]
    );
    // ...and the album whose tracks have none must NOT (B149) — the converse,
    // asserted here so the two rules are stated side by side rather than in
    // separate files where one could be relaxed without the other.
    assert!(
        items[1]["ImageTags"].get("Primary").is_none(),
        "an album with no cover must not advertise one: {:?}",
        items[1]["ImageTags"]
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

// B146 — the Android TV album detail screen calls `item.userData.isFavorite()`
// unconditionally (`ItemListFragment.addButtons`), so a synth item that omits
// `UserData` takes the app down with a NullPointerException the moment it is
// opened. Proven from a live logcat trace, and the same crash class as B68
// (a library folder missing its UserData killed the same client).
//
// Asserted across every music browse surface, because the field is set in the
// one constructor they all share — this is what proves that.
#[actix_web::test]
async fn every_synth_music_item_carries_user_data() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    for uri in [
        "/Albums",
        "/Artists",
        "/Artists/AlbumArtists",
        "/MusicGenres",
        "/Items?IncludeItemTypes=MusicAlbum&Recursive=true",
    ] {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(uri)
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = v["Items"]
            .as_array()
            .unwrap_or_else(|| panic!("{uri}: no Items"));
        assert!(!items.is_empty(), "{uri} returned nothing to check");
        for it in items {
            let ud = &it["UserData"];
            assert!(
                ud.is_object(),
                "{uri}: {} has no UserData — the native app dereferences it \
                 unconditionally and crashes: {it}",
                it["Name"]
            );
            // The app also reads these two off it, so a present-but-empty
            // object would be the same crash one field further in.
            assert!(ud["IsFavorite"].is_boolean(), "{uri}: {ud}");
            assert_eq!(ud["ItemId"], it["Id"], "{uri}: UserData must name its item");
        }
    }
}

// B149 — an advertised Primary tag is a promise the image route can keep.
// `synth_album_dto` used to emit one unconditionally, derived from a hash of
// the album id, so every client fetched a cover for albums that had none and
// the Google TV app logged a burst of HTTP 404s per screen of tiles. This is
// the mirror of B130 (art that could be served but was never advertised), and
// worse: the client pays for the request and still shows a blank.
#[actix_web::test]
async fn an_album_with_no_art_advertises_no_primary_tag() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // The seeded tracks carry no embedded picture and no sidecar, so nothing
    // in this library can serve an album cover.
    for uri in [
        "/Albums",
        "/Items?IncludeItemTypes=MusicAlbum&Recursive=true",
    ] {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(uri)
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = v["Items"].as_array().unwrap();
        assert!(!items.is_empty(), "{uri} returned nothing to check");
        for it in items {
            let primary = it["ImageTags"].get("Primary");
            assert!(
                primary.is_none(),
                "{uri}: {} advertises Primary {primary:?} with no bytes behind it",
                it["Name"]
            );
        }
    }
}

// B151 — jellyfin-androidtv builds a MusicAlbum card's title as
// `artists?.joinToString() ?: albumArtists?.joinToString() ?: albumArtist`
// plus the name. `albumArtists` is a List<NameGuidPair>, so joinToString calls
// Kotlin's data-class toString on each element: with `Artists` absent the album
// grid showed `NameGuidPair(name=Owl City, id=…) - Ocean Eyes` where the album
// name belongs. Every surface that offers AlbumArtists must also offer Artists.
#[actix_web::test]
async fn a_music_album_offers_its_artists_as_plain_strings() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let artist = artist_id_for("Limp Bizkit");
    for uri in [
        "/Albums".to_string(),
        "/Items?IncludeItemTypes=MusicAlbum&Recursive=true".to_string(),
        format!("/Items?ParentId={artist}"),
    ] {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&uri)
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let albums: Vec<_> = v["Items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["Type"] == "MusicAlbum")
            .collect();
        assert!(!albums.is_empty(), "{uri} returned no albums to check");
        for it in albums {
            if it.get("AlbumArtists").is_none() {
                continue; // an album with no artist tag offers neither
            }
            let artists = it.get("Artists").unwrap_or_else(|| {
                panic!("{uri}: {} offers AlbumArtists but no Artists — the card title becomes a struct dump: {it}", it["Name"])
            });
            let list = artists.as_array().expect("Artists must be an array");
            assert!(
                list.iter().all(|a| a.is_string()),
                "{uri}: Artists must be plain strings, got {artists}"
            );
            assert_eq!(
                list.first().and_then(|a| a.as_str()),
                it["AlbumArtists"][0]["Name"].as_str(),
                "{uri}: Artists and AlbumArtists must name the same act"
            );
        }
    }
}

// B153 — jellyfin-web's album page asks for the album's music videos:
//   /Users/{u}/Items?IncludeItemTypes=MusicVideo&Recursive=true&AlbumIds=…
// pharos stores no MusicVideos, and "zero recognised kinds" was read as "no
// kind filter", so the honest answer (an empty list) came back as the ENTIRE
// library — 51.6 MB of JSON on the real server, on every album page open.
#[actix_web::test]
async fn an_unstored_item_type_matches_nothing_rather_than_everything() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // `/Users/{id}/Items` — the shape jellyfin-web actually sends — shares
    // `run_items_list` with `/Items`, so exercising the latter covers both.
    for uri in [
        "/Items?IncludeItemTypes=MusicVideo&Recursive=true&SortBy=SortName".to_string(),
        // The "Latest" rail takes a different code path to the same question.
        "/Items/Latest?IncludeItemTypes=MusicVideo".to_string(),
        // Several unknown types together are still nothing.
        "/Items?IncludeItemTypes=MusicVideo,Book,Photo&Recursive=true".to_string(),
    ] {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&uri)
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // `/Items/Latest` returns a bare array; the rest return an envelope.
        let items = v
            .get("Items")
            .and_then(|i| i.as_array())
            .or_else(|| v.as_array())
            .unwrap_or_else(|| panic!("{uri}: unexpected shape {v}"));
        assert!(
            items.is_empty(),
            "{uri}: asking for a type pharos stores none of must return nothing, got {} items",
            items.len()
        );
    }

    // A type pharos SYNTHESISES is not "nothing" — Series tiles are folded out
    // of stored episode rows further down the same path, so naming one must
    // leave the query unfiltered rather than empty. The first version of this
    // fix was a blanket "unknown type means nothing" and broke exactly that.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?IncludeItemTypes=MusicAlbum&Recursive=true")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        !v["Items"].as_array().unwrap().is_empty(),
        "a synthesised type must still be answered, not emptied"
    );

    // ...and a type that IS stored still works, so this did not just switch
    // the filter off in the other direction.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?IncludeItemTypes=Audio&Recursive=true")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        !v["Items"].as_array().unwrap().is_empty(),
        "a stored type must still return its items"
    );
}
