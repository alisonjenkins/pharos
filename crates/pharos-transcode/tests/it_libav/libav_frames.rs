//! Phase 1 — in-process image + trickplay parity. Synthesises a known
//! fixture and asserts the libav helpers emit valid JPEGs / the expected
//! sprite-sheet count, byte-shape-equivalent to the spawn path.
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn synth_fixture(path: &Path, secs: u32) {
    let dur = format!("testsrc=duration={secs}:size=320x240:rate=10");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &dur,
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg fixture");
    assert!(status.success(), "fixture generation failed");
}

/// A JPEG begins with SOI 0xFFD8 and ends with EOI 0xFFD9.
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 4
        && bytes[0] == 0xFF
        && bytes[1] == 0xD8
        && bytes[bytes.len() - 2] == 0xFF
        && bytes[bytes.len() - 1] == 0xD9
}

#[test]
fn extract_image_emits_valid_jpeg() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("fixture.webm");
    synth_fixture(&input, 2);
    let out = dir.path().join("thumb.jpg");

    pharos_transcode::libav::image::extract_image(&input, Some(1000), 480, 3, &out)
        .expect("extract ok");

    let bytes = std::fs::read(&out).expect("read jpeg");
    assert!(is_jpeg(&bytes), "not a JPEG ({} bytes)", bytes.len());

    // 480px wide, aspect-preserved from 320x240 → 360 tall (even).
    let dims = jpeg_dimensions(&bytes).expect("dims");
    assert_eq!(dims.0, 480, "width");
    assert_eq!(dims.1, 360, "height");
}

#[test]
fn extract_image_rejects_garbage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bad = dir.path().join("garbage.webm");
    std::fs::write(&bad, b"definitely not media").expect("write");
    let out = dir.path().join("thumb.jpg");

    let err = pharos_transcode::libav::image::extract_image(&bad, None, 480, 3, &out)
        .expect_err("should fail");
    assert!(
        matches!(
            err,
            pharos_transcode::libav::frames::FrameError::BadInput(_)
        ),
        "expected BadInput, got {err:?}"
    );
}

#[test]
fn trickplay_emits_expected_sheets() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("fixture.webm");
    // 20s @ 1 sample/sec, 2x2 grid → 4 thumbs/sheet → 5 sheets.
    synth_fixture(&input, 20);
    let out_dir = dir.path().join("sprites");

    // interval 1000ms, width 160, grid 2, thumb_count 20 (20s/1s), max 8 sheets.
    let produced = pharos_transcode::libav::trickplay::trickplay_sprite(
        &input, 1000, 160, 2, 20, 8, 5, &out_dir,
    )
    .expect("trickplay ok");

    assert!(produced >= 1, "produced = {produced}");
    // Each produced sheet exists, is a valid JPEG, 0-based.
    for i in 0..produced {
        let p = out_dir.join(format!("{i}.jpg"));
        let bytes = std::fs::read(&p).unwrap_or_else(|_| panic!("read sheet {i}"));
        assert!(is_jpeg(&bytes), "sheet {i} not a JPEG");
    }
    // 20 seek-sampled thumbs / 4 per sheet = 5 sheets — the seek driver must
    // match the old fps-filter count exactly (bounded by thumb_count, not EOF).
    assert_eq!(produced, 5, "expected 5 sheets, got {produced}");
}

#[test]
fn trickplay_seek_is_bounded_by_thumb_count() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("fixture.webm");
    // 20s source, but only sample the first 6 thumbs (0..6s). The seek driver
    // must stop at thumb_count — NOT walk to EOF — so a whole-file decode's
    // extra thumbs never appear. 6 thumbs / 4 per 2x2 sheet = 2 sheets, the
    // last one partial (padded on flush).
    synth_fixture(&input, 20);
    let out_dir = dir.path().join("sprites");

    let produced = pharos_transcode::libav::trickplay::trickplay_sprite(
        &input, 1000, 160, 2, 6, 8, 5, &out_dir,
    )
    .expect("trickplay ok");

    assert_eq!(
        produced, 2,
        "6 thumbs @ 2x2 must yield exactly 2 sheets, got {produced}"
    );
    for i in 0..produced {
        let bytes = std::fs::read(out_dir.join(format!("{i}.jpg")))
            .unwrap_or_else(|_| panic!("read sheet {i}"));
        assert!(is_jpeg(&bytes), "sheet {i} not a JPEG");
    }
}

#[test]
fn waveform_emits_target_bins() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let wav = dir.path().join("tone.wav");
    // 2s mono 8kHz sine at 0.5 amplitude → known, non-silent level.
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2:sample_rate=8000",
            "-af",
            "volume=0.5",
            "-ac",
            "1",
        ])
        .arg(&wav)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());

    // 8000 Hz * 2s = 16000 samples; 1600/bin → ~10 bins available.
    let bins = pharos_transcode::libav::waveform::waveform_rms(&wav, 1600, 8).expect("waveform");
    assert_eq!(bins.len(), 8, "exactly target_bins");
    // A steady tone yields non-silent, negative, finite dBFS readings in
    // a sane range. The exact level depends on the source amplitude; the
    // contract is "consistent, audible level per bin".
    let nonzero: Vec<f32> = bins.iter().copied().filter(|b| *b != 0.0).collect();
    assert!(!nonzero.is_empty(), "all silent: {bins:?}");
    for b in &nonzero {
        assert!(
            b.is_finite() && (-60.0..0.0).contains(b),
            "dB out of range: {b} ({bins:?})"
        );
    }
    // Steady tone → all bins within ~1 dB of each other.
    let min = nonzero.iter().copied().fold(f32::INFINITY, f32::min);
    let max = nonzero.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    assert!(
        (max - min) < 1.0,
        "tone not steady: {min}..{max} ({bins:?})"
    );
}

/// Minimal JPEG SOF parser → (width, height). Scans for the SOF0/2 marker.
fn jpeg_dimensions(b: &[u8]) -> Option<(u16, u16)> {
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        // SOF0..SOF3 / SOF5..SOF7 / SOF9..SOF11 hold the frame dims.
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]);
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]);
            return Some((w, h));
        }
        // Skip this segment by its length.
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        i += 2 + len;
    }
    None
}
