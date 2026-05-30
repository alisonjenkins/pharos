//! LIB-D4 — sidecar artwork detection unit tests over a tmpdir of fixture
//! image files.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use pharos_core::{ArtworkRole, ArtworkSource, MediaProbe, SeriesInfo};
use std::fs;
use tempfile::tempdir;

/// Touch an empty file at `dir/<name>` so `is_file()` resolves true.
fn touch(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, b"x").unwrap();
    p
}

/// Build a request for `path` of the given `kind`, with no series.
fn req<'a>(path: &'a Path, kind: MediaKind, probe: &'a MediaProbe) -> MetadataRequest<'a> {
    MetadataRequest {
        path,
        kind,
        probe,
        series: None,
    }
}

/// Find the ref for `role` in `refs`, asserting it points at `expected`.
fn assert_local(refs: &[ArtworkRef], role: ArtworkRole, expected: &Path) {
    let found = refs
        .iter()
        .find(|a| a.role == role)
        .unwrap_or_else(|| panic!("no {role:?} ref in {refs:?}"));
    match &found.source {
        ArtworkSource::LocalFile(p) => assert_eq!(p, expected, "wrong path for {role:?}"),
        other => panic!("{role:?} not a local file: {other:?}"),
    }
}

#[test]
fn detects_generic_movie_sidecars() {
    let dir = tempdir().unwrap();
    let media = touch(dir.path(), "The Matrix (1999).mkv");
    let poster = touch(dir.path(), "poster.jpg");
    let fanart = touch(dir.path(), "fanart.jpg");
    let banner = touch(dir.path(), "banner.jpg");
    let logo = touch(dir.path(), "logo.png");
    let disc = touch(dir.path(), "disc.png");

    let probe = MediaProbe::default();
    let refs = detect_artwork(&req(&media, MediaKind::Movie, &probe));

    assert_local(&refs, ArtworkRole::Primary, &poster);
    assert_local(&refs, ArtworkRole::Backdrop, &fanart);
    assert_local(&refs, ArtworkRole::Banner, &banner);
    assert_local(&refs, ArtworkRole::Logo, &logo);
    assert_local(&refs, ArtworkRole::Disc, &disc);
}

#[test]
fn per_file_basename_beats_generic_poster() {
    let dir = tempdir().unwrap();
    let media = touch(dir.path(), "Movie.mkv");
    // Both present: the <basename>-poster.jpg convention wins.
    let _generic = touch(dir.path(), "poster.jpg");
    let specific = touch(dir.path(), "Movie-poster.jpg");

    let probe = MediaProbe::default();
    let refs = detect_artwork(&req(&media, MediaKind::Movie, &probe));

    assert_local(&refs, ArtworkRole::Primary, &specific);
}

#[test]
fn folder_and_cover_fallback_for_primary() {
    let dir = tempdir().unwrap();
    let media = touch(dir.path(), "audio.flac");
    // No poster.jpg; folder.jpg is the next Primary candidate.
    let folder = touch(dir.path(), "folder.jpg");

    let probe = MediaProbe::default();
    let refs = detect_artwork(&req(&media, MediaKind::Audio, &probe));

    assert_local(&refs, ArtworkRole::Primary, &folder);
}

#[test]
fn thumb_uses_basename_dash_thumb() {
    let dir = tempdir().unwrap();
    let media = touch(dir.path(), "Show - S01E01.mkv");
    let thumb = touch(dir.path(), "Show - S01E01-thumb.jpg");

    let probe = MediaProbe::default();
    let refs = detect_artwork(&req(&media, MediaKind::Episode, &probe));

    assert_local(&refs, ArtworkRole::Thumb, &thumb);
}

#[test]
fn episode_resolves_series_folder_art() {
    let root = tempdir().unwrap();
    let show = root.path().join("Show (2020)");
    let season = show.join("Season 01");
    fs::create_dir_all(&season).unwrap();

    let media = touch(&season, "Show - S01E01.mkv");
    // Series-level poster lives in the show folder, not beside the episode.
    let series_poster = touch(&show, "poster.jpg");

    let probe = MediaProbe::default();
    let series = SeriesInfo {
        series_name: "Show".into(),
        series_folder: Some(show.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let request = MetadataRequest {
        path: &media,
        kind: MediaKind::Episode,
        probe: &probe,
        series: Some(&series),
    };
    let refs = detect_artwork(&request);

    assert_local(&refs, ArtworkRole::Primary, &series_poster);
}

#[test]
fn item_level_art_outranks_series_folder() {
    let root = tempdir().unwrap();
    let show = root.path().join("Show (2020)");
    let season = show.join("Season 01");
    fs::create_dir_all(&season).unwrap();

    let media = touch(&season, "Show - S01E01.mkv");
    // Both a per-episode poster and a show-folder poster exist; the
    // episode-adjacent one wins (item-level dir probed first).
    let episode_poster = touch(&season, "Show - S01E01-poster.jpg");
    let _show_poster = touch(&show, "poster.jpg");

    let probe = MediaProbe::default();
    let series = SeriesInfo {
        series_name: "Show".into(),
        series_folder: Some(show.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let request = MetadataRequest {
        path: &media,
        kind: MediaKind::Episode,
        probe: &probe,
        series: Some(&series),
    };
    let refs = detect_artwork(&request);

    // Exactly one Primary ref, and it is the episode-adjacent file.
    let primaries: Vec<_> = refs
        .iter()
        .filter(|a| a.role == ArtworkRole::Primary)
        .collect();
    assert_eq!(primaries.len(), 1, "deduped to one Primary: {refs:?}");
    assert_local(&refs, ArtworkRole::Primary, &episode_poster);
}

#[test]
fn no_sidecars_yields_empty() {
    let dir = tempdir().unwrap();
    let media = touch(dir.path(), "lonely.mkv");

    let probe = MediaProbe::default();
    let refs = detect_artwork(&req(&media, MediaKind::Movie, &probe));

    assert!(refs.is_empty(), "expected no art, got {refs:?}");
}

#[tokio::test]
async fn fetch_never_errors_and_carries_only_artwork() {
    let dir = tempdir().unwrap();
    let media = touch(dir.path(), "Movie.mkv");
    touch(dir.path(), "poster.jpg");

    let probe = MediaProbe::default();
    let result = SidecarArtworkProvider::new()
        .fetch(&req(&media, MediaKind::Movie, &probe))
        .await
        .expect("sidecar fetch never errors");

    assert_eq!(result.artwork.len(), 1);
    // Pure artwork provider: no scalar metadata bleeds in.
    assert_eq!(result.title, None);
    assert!(result.genres.is_empty());
}

#[test]
fn webp_and_clearlogo_variants_detected() {
    let dir = tempdir().unwrap();
    let media = touch(dir.path(), "Movie.mkv");
    // poster.webp (extension variant) + clearlogo.png (alt logo base).
    let poster = touch(dir.path(), "poster.webp");
    let clearlogo = touch(dir.path(), "clearlogo.png");

    let probe = MediaProbe::default();
    let refs = detect_artwork(&req(&media, MediaKind::Movie, &probe));

    assert_local(&refs, ArtworkRole::Primary, &poster);
    assert_local(&refs, ArtworkRole::Logo, &clearlogo);
}
