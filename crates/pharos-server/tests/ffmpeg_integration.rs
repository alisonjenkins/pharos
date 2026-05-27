#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Real-ffmpeg integration smoke tests (T38).
//!
//! All tests are `#[ignore]` so the default `cargo nextest run` stays
//! fast. Run on-demand with:
//!
//!   nix develop --command cargo nextest run --run-ignored only \
//!     -p pharos-server --test ffmpeg_integration
//!
//! Fixtures are generated on-the-fly from `lavfi testsrc` so there's
//! nothing checked in. Each test creates a `tempfile::TempDir`,
//! synthesises a WebM via ffmpeg's testsrc, and tears the dir down at
//! end of scope.

use pharos_core::{MediaKind, Prober};
use pharos_scanner::FfmpegProber;
use pharos_server::image_cache::ImageCache;
use pharos_transcode::{FfmpegTranscoder, TranscodeOptions};
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;

/// True if both `ffmpeg` and `ffprobe` resolve on PATH. Lets the
/// tests no-op gracefully on systems without ffmpeg (CI matrix
/// without nix devShell, for instance).
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
    ok("ffmpeg") && ok("ffprobe")
}

/// Synthesise a tiny WebM (VP9 + Opus, 320x240 @ 3s) using lavfi.
/// Returns the file path.
async fn make_video_fixture(dir: &Path) -> PathBuf {
    let out = dir.join("fixture.webm");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=3:size=320x240:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
            "-c:v",
            "libvpx-vp9",
            "-b:v",
            "200k",
            "-c:a",
            "libopus",
            "-shortest",
        ])
        .arg(&out)
        .status()
        .await
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to build fixture");
    out
}

/// Synthesise an audio-only Opus-in-WebM fixture for the
/// audio-prober test.
async fn make_audio_fixture(dir: &Path) -> PathBuf {
    let out = dir.join("fixture-audio.webm");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:a",
            "libopus",
        ])
        .arg(&out)
        .status()
        .await
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to build audio fixture");
    out
}

#[tokio::test]
#[ignore = "requires ffmpeg/ffprobe on PATH"]
async fn probe_real_video_classifies_as_movie() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg/ffprobe not found");
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_video_fixture(td.path()).await;
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
    let fixture = make_audio_fixture(td.path()).await;
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
    let fixture = make_video_fixture(td.path()).await;
    let cache_dir = td.path().join("cache");
    // Fixture is 3 s — seek to 1 s so ffmpeg can decode a frame.
    let cache = ImageCache::new(&cache_dir).with_seek_seconds(1);
    let p = cache.primary(1, MediaKind::Movie, &fixture).await.unwrap();
    let bytes = tokio::fs::read(&p).await.unwrap();
    // JPEG SOI marker.
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "expected JPEG magic");
    assert!(bytes.len() > 256, "tiny jpeg unexpected: {} bytes", bytes.len());
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
    let fixture = make_video_fixture(td.path()).await;
    let opts = TranscodeOptions {
        container: pharos_transcode::Container::Mkv,
        video: Some(pharos_transcode::VideoCodec::Copy),
        audio: Some(pharos_transcode::AudioCodec::Copy),
        video_bitrate_bps: None,
        audio_bitrate_bps: None,
        start_position_ticks: 0,
        duration_ticks: None,
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

/// Synthesise a WebM with an embedded WebVTT subtitle track so the
/// extraction endpoint has something to extract from.
async fn make_subtitled_video_fixture(dir: &Path) -> PathBuf {
    let vtt = dir.join("subs.vtt");
    tokio::fs::write(
        &vtt,
        "WEBVTT\n\n00:00:00.500 --> 00:00:02.000\nHello pharos\n",
    )
    .await
    .unwrap();
    let out = dir.join("subbed.webm");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=3:size=320x240:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
            "-i",
        ])
        .arg(&vtt)
        .args([
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-row-mt",
            "1",
            "-b:v",
            "200k",
            "-c:a",
            "libopus",
            "-c:s",
            "webvtt",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-map",
            "2:s:0",
            "-metadata:s:s:0",
            "language=eng",
            "-shortest",
        ])
        .arg(&out)
        .status()
        .await
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to build subtitled fixture");
    out
}

#[tokio::test]
#[ignore = "requires ffmpeg/ffprobe on PATH"]
async fn probe_subtitle_tracks_extracts_embedded_webvtt() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_subtitled_video_fixture(td.path()).await;
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
    let fixture = make_subtitled_video_fixture(td.path()).await;
    // Drive the same shell-out the subtitles handler does, asserting
    // the stdout starts with `WEBVTT`.
    let out = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-i",
        ])
        .arg(&fixture)
        .args([
            "-map", "0:s:0", "-c:s", "webvtt", "-f", "webvtt", "pipe:1",
        ])
        .output()
        .await
        .expect("spawn ffmpeg");
    assert!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.trim_start().starts_with("WEBVTT"),
        "extraction didn't start with WEBVTT magic: {body:.200}"
    );
    assert!(body.contains("Hello pharos"), "cue line missing: {body}");
}

/// Build an MP3 with an embedded cover image (`attached_pic`) via
/// `ffmpeg -attach`. The ImageCache audio path uses `-map 0:v?`
/// which picks up the attached picture stream.
async fn make_audio_fixture_with_cover(dir: &Path) -> PathBuf {
    // Generate a 1×1 magenta JPEG via ffmpeg lavfi so the test stays
    // hermetic (no checked-in binary fixture).
    let cover = dir.join("cover.jpg");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=magenta:s=64x64:d=1",
            "-frames:v",
            "1",
            "-f",
            "image2",
        ])
        .arg(&cover)
        .status()
        .await
        .expect("spawn ffmpeg cover");
    assert!(status.success(), "ffmpeg failed to build cover.jpg");

    let out = dir.join("withcover.mp3");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-i",
        ])
        .arg(&cover)
        .args([
            "-map", "0:a:0", "-map", "1:v:0",
            "-c:a", "libmp3lame", "-b:a", "64k",
            "-c:v", "mjpeg",
            // ID3v2 attached_pic disposition for embedded cover art.
            "-disposition:v:0", "attached_pic",
            "-id3v2_version", "3",
            "-shortest",
        ])
        .arg(&out)
        .status()
        .await
        .expect("spawn ffmpeg cover-mp3");
    assert!(status.success(), "ffmpeg failed to build covered mp3");
    out
}

#[tokio::test]
#[ignore = "requires ffmpeg on PATH"]
async fn image_cache_extracts_audio_cover_art() {
    if !ffmpeg_available() {
        return;
    }
    let td = TempDir::new().unwrap();
    let fixture = make_audio_fixture_with_cover(td.path()).await;
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
