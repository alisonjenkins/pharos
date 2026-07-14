//! B/T86 — behavioral guard on the audio-fingerprint chain: real ffmpeg
//! audio → in-process libav decode → rusty-chromaprint → alignment. Proves
//! the decode/resample/fingerprint plumbing produces AcoustID-shaped points
//! at the expected rate and that two episodes sharing an audio segment align.
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_transcode::fingerprint::align::{compare, sample_duration_secs, AlignConfig};
use pharos_transcode::libav::fingerprint::fingerprint_window;
use std::process::Command;

/// A distinctive 25 s "intro": a frequency sweep chromaprint can fingerprint
/// (a flat tone or silence yields near-empty fingerprints).
fn make_intro(path: &std::path::Path) {
    let st = Command::new("ffmpeg")
        .args(["-v", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.4*sin(2*PI*(200+40*t)*t):d=25:s=44100",
        ])
        .args(["-ac", "2", "-y"])
        .arg(path)
        .status()
        .expect("spawn ffmpeg (run in devShell)");
    assert!(st.success(), "intro synth failed");
}

/// `[silence(pad_s)] + intro + [noise(tail_s)]` → an "episode".
fn make_episode(
    dir: &std::path::Path,
    name: &str,
    intro: &std::path::Path,
    pad_s: f64,
    tail_s: f64,
) -> std::path::PathBuf {
    let pad = dir.join(format!("{name}-pad.wav"));
    let tail = dir.join(format!("{name}-tail.wav"));
    let out = dir.join(format!("{name}.wav"));
    // Distinct noise per episode so only the intro is shared.
    for (p, filt, d) in [
        (&pad, "anullsrc=r=44100:cl=stereo".to_string(), pad_s),
        (
            &tail,
            format!(
                "aevalsrc=0.4*sin(2*PI*(900+{})*t):d={tail_s}:s=44100",
                name.len() * 30
            ),
            tail_s,
        ),
    ] {
        if d <= 0.0 {
            continue;
        }
        let st = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &filt,
                "-t",
                &d.to_string(),
                "-ac",
                "2",
                "-y",
            ])
            .arg(p)
            .status()
            .unwrap();
        assert!(st.success());
    }
    // Concat via the concat demuxer.
    let list = dir.join(format!("{name}-list.txt"));
    let mut listing = String::new();
    if pad_s > 0.0 {
        listing += &format!("file '{}'\n", pad.display());
    }
    listing += &format!("file '{}'\n", intro.display());
    if tail_s > 0.0 {
        listing += &format!("file '{}'\n", tail.display());
    }
    std::fs::write(&list, listing).unwrap();
    let st = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-ac", "2", "-y"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success(), "episode concat failed");
    out
}

#[test]
fn fingerprint_point_rate_matches_chromaprint_hop() {
    let dir = tempfile::tempdir().unwrap();
    let intro = dir.path().join("intro.wav");
    make_intro(&intro);
    let fp = fingerprint_window(&intro, 0, 20_000).expect("fingerprint");
    // ~20 s / 0.124 s ≈ 161 points; allow generous slack for edge trimming.
    let expected = 20.0 / sample_duration_secs();
    assert!(
        (fp.len() as f64) > expected * 0.5 && (fp.len() as f64) < expected * 1.5,
        "got {} points, expected ~{:.0}",
        fp.len(),
        expected
    );
}

#[test]
fn shared_intro_across_episodes_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let intro = dir.path().join("intro.wav");
    make_intro(&intro);
    // Episode A: intro at the very start; B: intro after 4 s of silence.
    let ep_a = make_episode(dir.path(), "a", &intro, 0.0, 15.0);
    let ep_b = make_episode(dir.path(), "b", &intro, 4.0, 15.0);

    let fp_a = fingerprint_window(&ep_a, 0, 40_000).expect("fp a");
    let fp_b = fingerprint_window(&ep_b, 0, 40_000).expect("fp b");

    let m = compare(&fp_a, &fp_b, &AlignConfig::default()).expect("shared intro found");
    // The 25 s intro clears the 15 s minimum.
    assert!(m.lhs.duration() > 15.0, "lhs dur {}", m.lhs.duration());
    // B's intro starts ~4 s in (past the 5 s snap it may or may not clear, so
    // just assert it's located later than A's, which is at the top).
    assert!(
        m.rhs.start >= m.lhs.start,
        "rhs {} lhs {}",
        m.rhs.start,
        m.lhs.start
    );
}

#[test]
fn unrelated_audio_does_not_falsely_match() {
    let dir = tempfile::tempdir().unwrap();
    let intro = dir.path().join("intro.wav");
    make_intro(&intro);
    let ep_a = make_episode(dir.path(), "a", &intro, 0.0, 10.0);
    // ep_b has NO shared intro — all distinct noise.
    let st = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.4*sin(2*PI*(1500+70*t)*t):d=35:s=44100",
            "-ac",
            "2",
            "-y",
        ])
        .arg(dir.path().join("b.wav"))
        .status()
        .unwrap();
    assert!(st.success());
    let fp_a = fingerprint_window(&ep_a, 0, 30_000).unwrap();
    let fp_b = fingerprint_window(&dir.path().join("b.wav"), 0, 30_000).unwrap();
    // No shared ≥15 s span → no match (or a spuriously-short one, rejected).
    assert!(compare(&fp_a, &fp_b, &AlignConfig::default()).is_none());
}
