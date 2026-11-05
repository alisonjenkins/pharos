//! LIB-D2 — NFO reader unit tests over real-shape Kodi fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use pharos_core::{ArtworkRole, ArtworkSource, MediaProbe, PersonKind, SeriesInfo};
use tempfile::tempdir;

/// A trimmed but realistic Kodi `movie.nfo`.
const MOVIE_NFO: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<movie>
  <title>The Matrix</title>
  <originaltitle>The Matrix</originaltitle>
  <tagline>Free your mind.</tagline>
  <plot>A computer hacker learns the true nature of his reality.</plot>
  <outline>Neo discovers the Matrix.</outline>
  <year>1999</year>
  <premiered>1999-03-31</premiered>
  <rating>8.7</rating>
  <criticrating>83</criticrating>
  <mpaa>Rated R</mpaa>
  <genre>Action</genre>
  <genre>Sci-Fi</genre>
  <studio>Warner Bros.</studio>
  <tag>cyberpunk</tag>
  <country>USA</country>
  <country>Australia</country>
  <trailer>plugin://plugin.video.youtube/?action=play_video&amp;videoid=m8e-FF8MsqU</trailer>
  <set>The Matrix Collection</set>
  <uniqueid type="tmdb">603</uniqueid>
  <uniqueid type="imdb">tt0133093</uniqueid>
  <thumb aspect="poster">https://example.test/poster.jpg</thumb>
  <fanart>https://example.test/fanart.jpg</fanart>
  <director>Lana Wachowski</director>
  <director>Lilly Wachowski</director>
  <credits>The Wachowskis</credits>
  <actor>
    <name>Keanu Reeves</name>
    <role>Neo</role>
    <order>0</order>
    <thumb>http://img/keanu.jpg</thumb>
  </actor>
  <actor>
    <name>Laurence Fishburne</name>
    <role>Morpheus</role>
    <order>1</order>
  </actor>
</movie>
"#;

/// A `tvshow.nfo` with the structured `<ratings>` form and a country mpaa.
const TVSHOW_NFO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tvshow>
  <title>Breaking Bad</title>
  <plot>A chemistry teacher turns to crime.</plot>
  <premiered>2008-01-20</premiered>
  <mpaa>US:TV-MA</mpaa>
  <genre>Drama</genre>
  <genre>Crime</genre>
  <studio>AMC</studio>
  <ratings>
    <rating name="tvdb" max="10" default="true">
      <value>9.4</value>
      <votes>1234</votes>
    </rating>
  </ratings>
  <uniqueid type="tvdb">81189</uniqueid>
</tvshow>
"#;

/// An episode NFO — sparse, leaning on the show NFO for studio/rating.
const EPISODE_NFO: &str = r#"<?xml version="1.0"?>
<episodedetails>
  <title>Pilot</title>
  <plot>Walter White starts cooking.</plot>
  <aired>2008-01-20</aired>
  <genre>Drama</genre>
</episodedetails>
"#;

fn probe() -> MediaProbe {
    MediaProbe::default()
}

#[tokio::test]
async fn movie_nfo_maps_all_common_fields() {
    let dir = tempdir().unwrap();
    let media = dir.path().join("The Matrix (1999).mkv");
    std::fs::write(dir.path().join("The Matrix (1999).nfo"), MOVIE_NFO).unwrap();

    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Movie,
        probe: &p,
        series: None,
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();

    assert_eq!(r.title.as_deref(), Some("The Matrix"));
    assert_eq!(r.tagline.as_deref(), Some("Free your mind."));
    assert_eq!(
        r.overview.as_deref(),
        Some("A computer hacker learns the true nature of his reality.")
    );
    assert_eq!(r.production_year, Some(1999));
    // 1999-03-31 -> unix seconds (UTC midnight).
    assert_eq!(r.premiere_date, Some(922_838_400));
    assert_eq!(r.community_rating, Some(8.7));
    assert_eq!(r.critic_rating, Some(83.0));
    // "Rated R" -> "R".
    assert_eq!(r.official_rating.as_deref(), Some("R"));
    assert_eq!(r.genres, vec!["Action", "Sci-Fi"]);
    assert_eq!(r.studios, vec!["Warner Bros."]);
    assert_eq!(r.tags, vec!["cyberpunk"]);
    assert_eq!(r.collections, vec!["The Matrix Collection"]);
    // T67 — repeated <country> → production_locations; <trailer> → trailers.
    assert_eq!(r.production_locations, vec!["USA", "Australia"]);
    assert_eq!(
        r.trailers,
        vec!["plugin://plugin.video.youtube/?action=play_video&videoid=m8e-FF8MsqU"]
    );
    assert_eq!(r.provider_ids.tmdb.as_deref(), Some("603"));
    assert_eq!(r.provider_ids.imdb.as_deref(), Some("tt0133093"));

    // Artwork: poster (Primary) + fanart (Backdrop) as URLs.
    assert!(r.artwork.iter().any(|a| a.role == ArtworkRole::Primary
        && a.source == ArtworkSource::Url("https://example.test/poster.jpg".into())));
    assert!(r.artwork.iter().any(|a| a.role == ArtworkRole::Backdrop
        && a.source == ArtworkSource::Url("https://example.test/fanart.jpg".into())));

    // People: two directors, a writer (credits), two actors with characters.
    assert!(r
        .people
        .iter()
        .any(|p| p.name == "Lana Wachowski" && p.kind == PersonKind::Director));
    assert!(r
        .people
        .iter()
        .any(|p| p.name == "The Wachowskis" && p.kind == PersonKind::Writer));
    let neo = r
        .people
        .iter()
        .find(|p| p.name == "Keanu Reeves")
        .expect("Keanu present");
    assert_eq!(neo.kind, PersonKind::Actor);
    assert_eq!(neo.character.as_deref(), Some("Neo"));
    // LIB-C2 — the actor's <thumb> headshot URL is captured.
    assert_eq!(neo.thumb.as_deref(), Some("http://img/keanu.jpg"));
    assert_eq!(neo.sort_order, Some(0));
}

#[tokio::test]
async fn audio_track_ignores_album_nfo_title_and_year() {
    // An `album.nfo` describes the ALBUM. Folding its `<title>` (the album
    // name) / `<year>` (a reissue year) into a track made every song share
    // the album name + wrong year (B-music). Audio must consult only a
    // track-level sidecar, so a bare `album.nfo` contributes nothing here.
    let dir = tempdir().unwrap();
    let media = dir.path().join("02 Stars.mp3");
    std::fs::write(
        dir.path().join("album.nfo"),
        r#"<?xml version="1.0"?>
<album>
  <title>Stars</title>
  <year>2008</year>
  <premiered>2008-01-01</premiered>
</album>
"#,
    )
    .unwrap();

    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Audio,
        probe: &p,
        series: None,
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();
    // album.nfo is NOT consulted for the track: no title/year leak.
    assert_eq!(r.title, None);
    assert_eq!(r.production_year, None);
}

#[tokio::test]
async fn audio_track_still_reads_its_own_sidecar_nfo() {
    // A genuine per-track `<track>.nfo` IS honoured (track-scoped metadata).
    let dir = tempdir().unwrap();
    let media = dir.path().join("02 Stars.mp3");
    std::fs::write(
        dir.path().join("02 Stars.nfo"),
        r#"<?xml version="1.0"?>
<musicvideo>
  <title>Stars (Live)</title>
</musicvideo>
"#,
    )
    .unwrap();

    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Audio,
        probe: &p,
        series: None,
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();
    assert_eq!(r.title.as_deref(), Some("Stars (Live)"));
}

#[tokio::test]
async fn movie_falls_back_to_movie_nfo_in_dir() {
    let dir = tempdir().unwrap();
    let media = dir.path().join("feature.mkv");
    // No `feature.nfo`; only `movie.nfo`.
    std::fs::write(dir.path().join("movie.nfo"), MOVIE_NFO).unwrap();

    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Movie,
        probe: &p,
        series: None,
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();
    assert_eq!(r.title.as_deref(), Some("The Matrix"));
}

#[tokio::test]
async fn tvshow_structured_ratings_and_country_mpaa() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("tvshow.nfo"), TVSHOW_NFO).unwrap();
    // A request keyed by the series folder, no episode sibling.
    let media = dir.path().join("Episode.mkv");
    let series = SeriesInfo {
        series_name: "Breaking Bad".into(),
        series_folder: Some(dir.path().to_string_lossy().into_owned()),
        ..SeriesInfo::default()
    };
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Episode,
        probe: &p,
        series: Some(&series),
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();

    // Structured <ratings><value> picked up as community rating.
    assert_eq!(r.community_rating, Some(9.4));
    // US:TV-MA -> TV-MA.
    assert_eq!(r.official_rating.as_deref(), Some("TV-MA"));
    assert_eq!(r.provider_ids.tvdb.as_deref(), Some("81189"));
    assert!(r.genres.contains(&"Crime".to_string()));
}

#[tokio::test]
async fn episode_merges_show_nfo_underneath() {
    let dir = tempdir().unwrap();
    // Episode sibling NFO + show-level tvshow.nfo.
    let media = dir.path().join("Breaking Bad S01E01.mkv");
    std::fs::write(dir.path().join("Breaking Bad S01E01.nfo"), EPISODE_NFO).unwrap();
    std::fs::write(dir.path().join("tvshow.nfo"), TVSHOW_NFO).unwrap();

    let series = SeriesInfo {
        series_name: "Breaking Bad".into(),
        series_folder: Some(dir.path().to_string_lossy().into_owned()),
        ..SeriesInfo::default()
    };
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Episode,
        probe: &p,
        series: Some(&series),
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();

    // Episode-level title/plot win.
    assert_eq!(r.title.as_deref(), Some("Pilot"));
    assert_eq!(r.overview.as_deref(), Some("Walter White starts cooking."));
    // Show-level fields backfill: studio + rating + tvdb id from tvshow.nfo.
    assert_eq!(r.studios, vec!["AMC"]);
    assert_eq!(r.community_rating, Some(9.4));
    assert_eq!(r.provider_ids.tvdb.as_deref(), Some("81189"));
    // Genres union: episode "Drama" + show "Drama","Crime" (deduped).
    assert!(r.genres.contains(&"Drama".to_string()));
    assert!(r.genres.contains(&"Crime".to_string()));
    assert_eq!(r.genres.iter().filter(|g| *g == "Drama").count(), 1);
}

#[tokio::test]
async fn an_episode_without_its_own_nfo_is_not_titled_with_the_show() {
    let dir = tempdir().unwrap();
    let media = dir
        .path()
        .join("Breaking Bad - S01E02 - Cat's in the Bag.mkv");
    std::fs::write(dir.path().join("tvshow.nfo"), TVSHOW_NFO).unwrap();

    let series = SeriesInfo {
        series_name: "Breaking Bad".into(),
        series_folder: Some(dir.path().to_string_lossy().into_owned()),
        ..SeriesInfo::default()
    };
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Episode,
        probe: &p,
        series: Some(&series),
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();

    assert_eq!(r.title, None);
    assert_eq!(r.overview, None);
    assert_eq!(r.premiere_date, None);
    // Show-wide fields still apply to the episode.
    assert_eq!(r.studios, vec!["AMC"]);
    assert_eq!(r.provider_ids.tvdb.as_deref(), Some("81189"));
}

#[tokio::test]
async fn absent_nfo_yields_empty_result_not_error() {
    let dir = tempdir().unwrap();
    let media = dir.path().join("orphan.mkv");
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Movie,
        probe: &p,
        series: None,
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();
    assert_eq!(r, MetadataResult::default());
}

#[tokio::test]
async fn malformed_nfo_yields_err_not_panic() {
    let dir = tempdir().unwrap();
    let media = dir.path().join("broken.mkv");
    // Truncated / unbalanced XML — quick-xml reports an EOF inside a tag.
    std::fs::write(
        dir.path().join("broken.nfo"),
        "<movie><title>Unclosed</title><plot>no end tag and <bad",
    )
    .unwrap();
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Movie,
        probe: &p,
        series: None,
    };
    let res = NfoProvider::new().fetch(&req).await;
    assert!(res.is_err(), "malformed NFO must yield Err, got {res:?}");
}

#[tokio::test]
async fn unknown_and_extra_elements_are_ignored() {
    let dir = tempdir().unwrap();
    let media = dir.path().join("weird.mkv");
    std::fs::write(
        dir.path().join("weird.nfo"),
        r#"<movie>
            <title>Weird</title>
            <madeupfield>ignore me</madeupfield>
            <nested><deep>nope</deep></nested>
            <year>2010</year>
        </movie>"#,
    )
    .unwrap();
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Movie,
        probe: &p,
        series: None,
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();
    assert_eq!(r.title.as_deref(), Some("Weird"));
    assert_eq!(r.production_year, Some(2010));
}

/// Directly exercise the date helper for a couple of known epochs.
#[test]
fn date_parsing_known_values() {
    assert_eq!(parse_date_unix("1970-01-01"), Some(0));
    assert_eq!(parse_date_unix("2000-01-01"), Some(946_684_800));
    assert_eq!(parse_date_unix("1999-03-31"), Some(922_838_400));
    // With a time component and `T` separator.
    assert_eq!(parse_date_unix("2008-01-20T00:00:00"), Some(1_200_787_200));
    // Garbage -> None, never panics.
    assert_eq!(parse_date_unix("not-a-date"), None);
    assert_eq!(parse_date_unix(""), None);
}

/// A `<uniqueid>` with no type but a `tt…` value is recognised as IMDb;
/// a bare `<id>` defaults to TMDB.
#[tokio::test]
async fn uniqueid_typeless_and_bare_id() {
    let dir = tempdir().unwrap();
    let media = dir.path().join("ids.mkv");
    std::fs::write(
        dir.path().join("ids.nfo"),
        r#"<movie>
            <title>Ids</title>
            <id>12345</id>
            <uniqueid>tt9999999</uniqueid>
        </movie>"#,
    )
    .unwrap();
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Movie,
        probe: &p,
        series: None,
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();
    assert_eq!(r.provider_ids.tmdb.as_deref(), Some("12345"));
    assert_eq!(r.provider_ids.imdb.as_deref(), Some("tt9999999"));
}

/// LIB-C5 — Jellyfin/Kodi also write box-set membership nested as
/// `<set><name>Name</name></set>` (and `<set><overview>…</overview>`).
/// The nested `<name>` must route to collections, distinct from the flat
/// `<set>Name</set>` form (and never collide with an `<actor><name>`).
#[tokio::test]
async fn nested_set_name_form_parses_into_collections() {
    let dir = tempdir().unwrap();
    let media = dir.path().join("Movie (2017).mkv");
    std::fs::write(
        dir.path().join("Movie (2017).nfo"),
        r#"<?xml version="1.0"?>
<movie>
  <title>Movie</title>
  <set>
    <name>The Matrix Collection</name>
    <overview>ignored</overview>
  </set>
  <collection>Wachowski Films</collection>
  <actor>
    <name>Keanu Reeves</name>
    <role>Neo</role>
  </actor>
</movie>"#,
    )
    .unwrap();
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Movie,
        probe: &p,
        series: None,
    };
    let r = NfoProvider::new().fetch(&req).await.unwrap();
    assert_eq!(
        r.collections,
        vec!["The Matrix Collection", "Wachowski Films"],
        "nested <set><name> + flat <collection>"
    );
    // The actor's nested <name> stays a person, not a collection.
    assert!(r.people.iter().any(|pe| pe.name == "Keanu Reeves"));
    assert!(!r.collections.iter().any(|c| c == "Keanu Reeves"));
}

/// B558 — trimmed from the deployed library: Jellyfin identified the
/// `TV/Dragon Ball` folder as Super Dragon Ball Heroes and wrote that show's
/// NFOs and stills into it.
const FOREIGN_TVSHOW_NFO: &str = r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<tvshow>
  <plot>Trunks returns from the future to train with Goku and Vegeta.</plot>
  <title>Super Dragon Ball Heroes</title>
  <originaltitle>スーパードラゴンボールヒーローズ</originaltitle>
  <tmdbid>80020</tmdbid>
  <tvdbid>347734</tvdbid>
  <art>
    <poster>/media/TV/Dragon Ball/folder.jpg</poster>
    <fanart>/media/TV/Dragon Ball/fanart.jpg</fanart>
  </art>
  <id>347734</id>
</tvshow>
"#;

const FOREIGN_EPISODE_NFO: &str = r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<episodedetails>
  <plot>Goku and Vegeta's training with Whis and Beerus is interrupted.</plot>
  <title>Goku vs. Goku! A Super Battle Begins on Prison Planet!</title>
  <imdbid>tt8594374</imdbid>
  <tvdbid>6692765</tvdbid>
  <art>
    <poster>/media/TV/Dragon Ball/Season 01/Dragon Ball - S01E01 - Secret of the Dragon Balls-thumb.jpg</poster>
  </art>
  <showtitle>Super Dragon Ball Heroes</showtitle>
  <episode>1</episode>
  <season>1</season>
</episodedetails>
"#;

/// A show folder holding `tvshow.nfo` and a `Season 01` with one episode, its
/// NFO and its `-thumb.jpg`. Returns (tempdir, episode path, series info).
fn dragon_ball(show_nfo: &str, episode_nfo: &str) -> (tempfile::TempDir, PathBuf, SeriesInfo) {
    let dir = tempdir().unwrap();
    let season = dir.path().join("Season 01");
    std::fs::create_dir(&season).unwrap();
    let stem = "Dragon Ball - S01E01 - Secret of the Dragon Balls";
    let media = season.join(format!("{stem}.mkv"));
    std::fs::write(&media, b"").unwrap();
    std::fs::write(season.join(format!("{stem}.nfo")), episode_nfo).unwrap();
    std::fs::write(season.join(format!("{stem}-thumb.jpg")), b"jpg").unwrap();
    std::fs::write(dir.path().join("tvshow.nfo"), show_nfo).unwrap();
    std::fs::write(dir.path().join("folder.jpg"), b"jpg").unwrap();
    std::fs::write(dir.path().join("logo.png"), b"png").unwrap();
    let series = SeriesInfo {
        series_name: "Dragon Ball".into(),
        season_number: Some(1),
        episode_number: Some(1),
        series_folder: Some(dir.path().to_string_lossy().into_owned()),
        ..SeriesInfo::default()
    };
    (dir, media, series)
}

#[tokio::test]
async fn nfos_naming_another_show_are_ignored_with_their_art() {
    let (_dir, media, series) = dragon_ball(FOREIGN_TVSHOW_NFO, FOREIGN_EPISODE_NFO);
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Episode,
        probe: &p,
        series: Some(&series),
    };

    let r = NfoProvider::new().fetch(&req).await.unwrap();
    assert_eq!(r.title, None);
    assert_eq!(r.overview, None);
    assert_eq!(r.provider_ids, ProviderIds::default());

    let foreign = identity::foreign_nfos(&req);
    assert!(foreign.show && foreign.episode);
    let art = crate::metadata::sidecar::detect_artwork(&req);
    assert!(art.is_empty(), "the other show's images were kept: {art:?}");
}

#[tokio::test]
async fn nfos_naming_the_show_are_kept_with_their_art() {
    let show = FOREIGN_TVSHOW_NFO
        .replace("Super Dragon Ball Heroes", "Dragon Ball")
        .replace("スーパードラゴンボールヒーローズ", "ドラゴンボール");
    let episode = FOREIGN_EPISODE_NFO.replace("Super Dragon Ball Heroes", "Dragon Ball");
    let (_dir, media, series) = dragon_ball(&show, &episode);
    let p = probe();
    let req = MetadataRequest {
        path: &media,
        kind: MediaKind::Episode,
        probe: &p,
        series: Some(&series),
    };

    let r = NfoProvider::new().fetch(&req).await.unwrap();
    assert_eq!(r.provider_ids.tvdb.as_deref(), Some("6692765"));
    assert_eq!(
        identity::foreign_nfos(&req),
        identity::ForeignNfos::default()
    );
    let art = crate::metadata::sidecar::detect_artwork(&req);
    let local = |suffix: &str| {
        art.iter().any(|a| {
            matches!(&a.source, ArtworkSource::LocalFile(p)
            if p.to_string_lossy().ends_with(suffix))
        })
    };
    assert!(
        local("-thumb.jpg") && local("folder.jpg") && local("logo.png"),
        "{art:?}"
    );
}

/// The show names the deployed library's `tvshow.nfo` files carry for folders
/// whose name differs from them (2026-11-05). Every one but the last two names
/// the folder's show; those two are scraper misidentifications.
#[test]
fn the_show_name_check_passes_the_librarys_legitimate_variants() {
    let pairs = [
        ("Alien - Earth", "Alien: Earth"),
        ("Cagney & Lacey", "Cagney & Lacey"),
        ("Code Geass", "Code Geass: Lelouch of the Rebellion"),
        ("DARK MOON - THE BLOOD ALTAR", "DARK MOON: THE BLOOD ALTAR"),
        ("Fullmetal Alchemist", "Fullmetal Alchemist: Brotherhood"),
        (
            "KonoSuba – God’s blessing on this wonderful world!!",
            "KONOSUBA - God's blessing on this wonderful world!",
        ),
        ("Lie to me", "Lie to Me"),
        (
            "Marvel's Agents of S.H.I.E.L.D",
            "Marvel's Agents of S.H.I.E.L.D.",
        ),
        ("Mrs Browns Boys", "Mrs Brown's Boys"),
        ("One Piece.old", "One Piece old"),
        ("PLUR1BUS", "Pluribus"),
        (
            "Star Trek The Next Generation",
            "Star Trek: The Next Generation",
        ),
        (
            "Person.of.Interest.S01E03.720p.HDTV.x264-IMMERSE",
            "Person of Interest",
        ),
    ];
    for (folder, nfo) in pairs {
        assert!(
            !identity::names_another_show(folder, None, [&nfo.to_string()]),
            "{folder:?} should accept {nfo:?}"
        );
    }
    for (folder, nfo) in [
        ("Dragon Ball", "Super Dragon Ball Heroes"),
        ("Forever (US)", "Us Forever"),
    ] {
        assert!(
            identity::names_another_show(folder, None, [&nfo.to_string()]),
            "{folder:?} should reject {nfo:?}"
        );
    }
}

#[test]
fn the_show_name_check_rejects_a_conflicting_region_or_year() {
    // (series name, series year, NFO show name, names another show)
    let cases = [
        ("The Office (US)", None, "The Office (UK)", true),
        ("The Office (US)", None, "The Office (us)", false),
        ("The Office (US)", None, "The Office", false),
        ("The Office", None, "The Office (UK)", false),
        ("Doctor Who", Some(2005), "Doctor Who (1963)", true),
        ("Doctor Who", Some(2005), "Doctor Who (2005)", false),
        ("Doctor Who", Some(2005), "Doctor Who", false),
        ("Doctor Who", None, "Doctor Who (1963)", false),
        (
            "Battlestar Galactica",
            Some(2003),
            "Battlestar Galactica",
            false,
        ),
        (
            "Party Political Broadcast",
            Some(2006),
            "Party Political Broadcast (2006)",
            false,
        ),
    ];
    for (series, year, nfo, foreign) in cases {
        assert_eq!(
            identity::names_another_show(series, year, [&nfo.to_string()]),
            foreign,
            "{series:?} ({year:?}) against {nfo:?}"
        );
    }
}

#[test]
fn the_show_name_check_reads_an_nfo_with_a_mismatched_end_tag() {
    let nfo =
        "<tvshow><plot>Trunks returns.</plott><title>Super Dragon Ball Heroes</title></tvshow>";
    let names = identity::parse_names(nfo.as_bytes()).expect("parse_nfo reads this NFO");
    assert_eq!(names.title.as_deref(), Some("Super Dragon Ball Heroes"));
}
