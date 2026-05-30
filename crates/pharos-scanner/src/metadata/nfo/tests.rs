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
