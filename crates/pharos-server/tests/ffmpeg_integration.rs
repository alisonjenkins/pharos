#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Real-ffmpeg integration smoke tests (T38).
//!
//! All tests are `#[ignore]` so the default `cargo nextest run` stays
//! fast. Run on-demand with:
//!
//!   nix develop --command cargo nextest run --run-ignored only \
//!     -p pharos-server --test ffmpeg_integration
//!
//! Fixtures come from the `pharosIntegrationFixtures` nix derivation
//! (built once, cached in /nix/store, exported as
//! `PHAROS_TEST_FIXTURES` by the devShell). Tests skip when the env
//! var isn't set — keeps the suite hermetic against ffmpeg version
//! drift on the host.

use pharos_cache::ImageCache;
use pharos_core::{MediaKind, Prober};
use pharos_scanner::FfmpegProber;
use pharos_transcode::{FfmpegTranscoder, TranscodeOptions};
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;

/// Path to the static fixture corpus. `None` when running outside
/// the devShell (or the package hasn't been built) — tests early-skip.
fn fixtures_dir() -> Option<PathBuf> {
    std::env::var_os("PHAROS_TEST_FIXTURES").map(PathBuf::from)
}

fn fixture(name: &str) -> Option<PathBuf> {
    let dir = fixtures_dir()?;
    let p = dir.join(name);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// True if ffmpeg resolves on PATH AND the fixture corpus exists.
/// Tests `early-return` on either missing.
fn ffmpeg_available() -> bool {
    fn ok(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    ok("ffmpeg") && ok("ffprobe") && fixtures_dir().is_some()
}

/// Static fixture from `pharosIntegrationFixtures`. Resolves to the
/// /nix/store copy, encoded once at flake-build time.
fn make_video_fixture(_dir: &Path) -> PathBuf {
    fixture("video.webm").expect("PHAROS_TEST_FIXTURES/video.webm missing")
}

fn make_audio_fixture(_dir: &Path) -> PathBuf {
    fixture("audio.webm").expect("PHAROS_TEST_FIXTURES/audio.webm missing")
}

#[tokio::test]
#[ignore = "requires ffmpeg/ffprobe on PATH"]
async fn probe_real_video_classifies_as_movie() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg/ffprobe not found");
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_video_fixture(td.path());
    let probe = FfmpegProber::new().probe(&fixture).await.unwrap();
    assert_eq!(probe.kind, MediaKind::Movie);
    // 3 s ±200 ms tolerance — VP9 encoder pads slightly.
    let d = probe.duration_ms().unwrap_or(0);
    assert!((2_800..=3_200).contains(&d), "duration_ms={d}");
    let container = probe.container().unwrap_or_default().to_string();
    assert!(
        container.contains("matroska") || container.contains("webm"),
        "container={container}"
    );
}

#[tokio::test]
#[ignore = "requires ffmpeg/ffprobe on PATH"]
async fn probe_real_audio_classifies_as_audio() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_audio_fixture(td.path());
    let probe = FfmpegProber::new().probe(&fixture).await.unwrap();
    assert_eq!(probe.kind, MediaKind::Audio);
}

#[tokio::test]
#[ignore = "requires ffmpeg on PATH"]
async fn image_cache_extracts_primary_jpeg_from_real_video() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_video_fixture(td.path());
    let cache_dir = td.path().join("cache");
    // Fixture is 3 s — seek to 1 s so ffmpeg can decode a frame.
    let cache = ImageCache::new(&cache_dir).with_seek_seconds(1);
    let p = cache.primary(1, MediaKind::Movie, &fixture).await.unwrap();
    let bytes = tokio::fs::read(&p).await.unwrap();
    // JPEG SOI marker.
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "expected JPEG magic");
    assert!(
        bytes.len() > 256,
        "tiny jpeg unexpected: {} bytes",
        bytes.len()
    );
    // Second call hits the cache — file mtime should not advance, but
    // simpler check: it just resolves to the same path without
    // erroring (which it would on a missing ffmpeg).
    let p2 = cache
        .primary(1, MediaKind::Movie, Path::new("/no/such/source"))
        .await
        .unwrap();
    assert_eq!(p, p2);
}

#[tokio::test]
#[ignore = "requires ffmpeg on PATH"]
async fn transcoder_streams_bytes_from_real_video() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_video_fixture(td.path());
    let opts = TranscodeOptions {
        container: pharos_transcode::Container::Mkv,
        video: Some(pharos_transcode::VideoCodec::Copy),
        audio: Some(pharos_transcode::AudioCodec::Copy),
        video_bitrate_bps: None,
        audio_bitrate_bps: None,
        start_position_ticks: 0,
        duration_ticks: None,
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        continuous_audio_path: None,
    };
    let mut stream = FfmpegTranscoder::new()
        .transcode(&fixture, &opts)
        .await
        .unwrap();
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await.unwrap();
    assert!(n > 0, "expected at least one byte from ffmpeg stdout");
    // Matroska EBML header begins with 0x1A 0x45 0xDF 0xA3.
    assert_eq!(&buf[..4], &[0x1A, 0x45, 0xDF, 0xA3], "expected EBML magic");
}

/// Static `subbed.webm` from the nix fixtures derivation.
fn make_subtitled_video_fixture(_dir: &Path) -> PathBuf {
    fixture("subbed.webm").expect("PHAROS_TEST_FIXTURES/subbed.webm missing")
}

#[tokio::test]
#[ignore = "requires ffmpeg/ffprobe on PATH"]
async fn probe_subtitle_tracks_extracts_embedded_webvtt() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_subtitled_video_fixture(td.path());
    let probe = FfmpegProber::new().probe(&fixture).await.unwrap();
    assert!(
        !probe.probe.subtitle_tracks.is_empty(),
        "expected at least one subtitle track in {fixture:?}, got {:?}",
        probe.probe.subtitle_tracks
    );
    let st = &probe.probe.subtitle_tracks[0];
    assert_eq!(st.codec.as_deref(), Some("webvtt"));
    assert_eq!(st.language.as_deref(), Some("eng"));
}

#[tokio::test]
#[ignore = "requires ffmpeg on PATH"]
async fn ffmpeg_extracts_webvtt_from_embedded_stream() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_subtitled_video_fixture(td.path());
    // Drive the same shell-out the subtitles handler does, asserting
    // the stdout starts with `WEBVTT`.
    let out = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(&fixture)
        .args(["-map", "0:s:0", "-c:s", "webvtt", "-f", "webvtt", "pipe:1"])
        .output()
        .await
        .expect("spawn ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.trim_start().starts_with("WEBVTT"),
        "extraction didn't start with WEBVTT magic: {body:.200}"
    );
    assert!(body.contains("Hello pharos"), "cue line missing: {body}");
}

/// Static `withcover.mp3` from the nix fixtures derivation. MP3 with
/// an ID3v2 attached_pic JPEG; the ImageCache audio path picks it up.
fn make_audio_fixture_with_cover(_dir: &Path) -> PathBuf {
    fixture("withcover.mp3").expect("PHAROS_TEST_FIXTURES/withcover.mp3 missing")
}

#[tokio::test]
#[ignore = "requires ffmpeg on PATH"]
async fn image_cache_extracts_audio_cover_art() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_audio_fixture_with_cover(td.path());
    let cache_dir = td.path().join("cache");
    let cache = ImageCache::new(&cache_dir);
    let p = cache
        .primary(2, MediaKind::Audio, &fixture)
        .await
        .expect("audio cover extracts");
    let bytes = tokio::fs::read(&p).await.unwrap();
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "JPEG magic missing");
    assert!(
        bytes.len() > 64,
        "extracted cover too small: {} bytes",
        bytes.len()
    );
}

/// Static `dualaudio.mkv` from the nix fixtures derivation. Two
/// audio tracks (440 Hz / 880 Hz) over a VP9 video. Drives W1.
fn make_dual_audio_fixture(_dir: &Path) -> PathBuf {
    fixture("dualaudio.mkv").expect("PHAROS_TEST_FIXTURES/dualaudio.mkv missing")
}

/// Static `dualsubs.mkv` from the nix fixtures derivation. Two
/// embedded WebVTT subtitle tracks. Drives W2.
fn make_dual_subtitle_fixture(_dir: &Path) -> PathBuf {
    fixture("dualsubs.mkv").expect("PHAROS_TEST_FIXTURES/dualsubs.mkv missing")
}

/// Read the entire transcode stream into a Vec<u8> (bounded by
/// fixture length, so always small).
async fn drain_transcode_stream(
    mut stream: pharos_transcode::TranscodeStream,
) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// W1 — switching `AudioStreamIndex` produces different output bytes
/// because ffmpeg muxes the chosen track. We don't decode the audio
/// (that would require a separate analyser); the byte-diff is enough
/// to prove the `-map 0:a:N` knob landed.
#[tokio::test]
#[ignore = "requires ffmpeg on PATH"]
async fn transcoder_honours_audio_stream_index() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_dual_audio_fixture(td.path());

    let common = TranscodeOptions {
        container: pharos_transcode::Container::Mpegts,
        video: Some(pharos_transcode::VideoCodec::H264),
        audio: Some(pharos_transcode::AudioCodec::Aac),
        video_bitrate_bps: Some(500_000),
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: 0,
        duration_ticks: Some(20_000_000), // 2 seconds
        audio_source_stream_index: Some(0),
        burn_subtitle_stream_index: None,
        continuous_audio_path: None,
    };
    let track0_stream = FfmpegTranscoder::new()
        .transcode(&fixture, &common)
        .await
        .unwrap();
    let track0 = drain_transcode_stream(track0_stream).await.unwrap();

    let mut alt = common.clone();
    alt.audio_source_stream_index = Some(1);
    let track1_stream = FfmpegTranscoder::new()
        .transcode(&fixture, &alt)
        .await
        .unwrap();
    let track1 = drain_transcode_stream(track1_stream).await.unwrap();

    assert!(!track0.is_empty(), "track 0 produced no bytes");
    assert!(!track1.is_empty(), "track 1 produced no bytes");
    assert_ne!(
        track0, track1,
        "AudioStreamIndex switch produced identical output bytes"
    );
}

/// W2 — burning a subtitle into the video changes the encoded
/// stream. With + without burn-in must differ; track 0 vs track 1
/// burn-in must also differ.
#[tokio::test]
#[ignore = "requires ffmpeg on PATH"]
async fn transcoder_honours_burn_subtitle_index() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_dual_subtitle_fixture(td.path());

    let base = TranscodeOptions {
        container: pharos_transcode::Container::Mpegts,
        video: Some(pharos_transcode::VideoCodec::H264),
        audio: Some(pharos_transcode::AudioCodec::Aac),
        video_bitrate_bps: Some(500_000),
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: 0,
        duration_ticks: Some(20_000_000),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        continuous_audio_path: None,
    };
    let no_burn_stream = FfmpegTranscoder::new()
        .transcode(&fixture, &base)
        .await
        .unwrap();
    let no_burn = drain_transcode_stream(no_burn_stream).await.unwrap();

    let mut with_sub_a = base.clone();
    with_sub_a.burn_subtitle_stream_index = Some(0);
    let sub_a_stream = FfmpegTranscoder::new()
        .transcode(&fixture, &with_sub_a)
        .await
        .unwrap();
    let sub_a = drain_transcode_stream(sub_a_stream).await.unwrap();

    let mut with_sub_b = base.clone();
    with_sub_b.burn_subtitle_stream_index = Some(1);
    let sub_b_stream = FfmpegTranscoder::new()
        .transcode(&fixture, &with_sub_b)
        .await
        .unwrap();
    let sub_b = drain_transcode_stream(sub_b_stream).await.unwrap();

    assert!(!no_burn.is_empty(), "no-burn produced no bytes");
    assert!(!sub_a.is_empty(), "subtitle 0 burn produced no bytes");
    assert!(!sub_b.is_empty(), "subtitle 1 burn produced no bytes");
    assert_ne!(no_burn, sub_a, "burning subtitle 0 didn't change output");
    assert_ne!(
        sub_a, sub_b,
        "subtitle 0 vs 1 burn produced identical output"
    );
}
