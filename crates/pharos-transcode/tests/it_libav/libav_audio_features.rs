//! Spec 024 — the libav half of tempo and energy: the BPM tag is read, only the
//! requested window is decoded, and the pure analyser sees mono PCM at its own
//! rate (so a known beat reads at its tempo).
#![cfg(all(unix, feature = "backend-lib"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_transcode::libav::audio_features::audio_features;
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

/// 40 s of a 60 Hz kick every 0.5 s (120 BPM), stereo 44.1 kHz, so the
/// worker has to downmix and resample. `tags` are `-metadata` pairs.
fn kick_track(path: &Path, tags: &[&str]) {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "aevalsrc=0.8*sin(2*PI*60*t)*exp(-30*mod(t\\,0.5)):s=44100:d=40",
        "-ac",
        "2",
    ]);
    for t in tags {
        cmd.args(["-metadata", t]);
    }
    let status = cmd.arg(path).status().expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg fixture");
}

#[test]
fn an_untagged_song_has_its_tempo_estimated_from_the_window() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let flac = dir.path().join("kick.flac");
    kick_track(&flac, &[]);
    let f = audio_features(&flac, 10_000, 20_000).unwrap();
    assert_eq!(f.tag_bpm, None);
    let bpm = f.analysed_bpm.expect("a steady kick has a tempo");
    assert!((bpm - 120.0).abs() <= 1.0, "read {bpm}");
    assert!(f.energy.is_some());
    // Only the window was decoded.
    assert!(
        (f.analysed_seconds - 20.0).abs() < 0.5,
        "decoded {} s",
        f.analysed_seconds
    );
}

/// A stream whose timestamps start well past 0 (here ~101 s, as in a
/// transport-stream rip) still has its window measured from the song's start.
/// Taken as absolute, every frame would sit past the window's end and the
/// song would be stored as analysed with nothing in it, for good.
///
/// The first 30 s are silent and the window is 35–55 s, so a failed seek
/// that fell back to the top would read 20 s of silence and find no tempo.
#[test]
fn a_stream_with_a_late_origin_is_windowed_from_its_own_start() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ts = dir.path().join("late.ts");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.8*sin(2*PI*60*t)*exp(-30*mod(t\\,0.5))*gte(t\\,30):s=44100:d=60",
            "-ac",
            "2",
            "-c:a",
            "mp2",
            "-output_ts_offset",
            "100",
        ])
        .arg(&ts)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg fixture");
    let f = audio_features(&ts, 35_000, 20_000).unwrap();
    assert!(
        (f.analysed_seconds - 20.0).abs() < 0.5,
        "decoded {} s of a 20 s window",
        f.analysed_seconds
    );
    let bpm = f.analysed_bpm.expect("a steady kick has a tempo");
    assert!((bpm - 120.0).abs() <= 1.0, "read {bpm}");
}

#[test]
fn a_bpm_tag_is_read_and_skips_the_estimator() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let flac = dir.path().join("tagged.flac");
    kick_track(&flac, &["BPM=128"]);
    let f = audio_features(&flac, 10_000, 20_000).unwrap();
    assert_eq!(f.tag_bpm, Some(128.0));
    assert_eq!(
        f.analysed_bpm, None,
        "the tag wins; the estimator is skipped"
    );
    assert!(f.energy.is_some(), "energy still needs the audio");
}

#[test]
fn a_file_with_no_audio_is_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.mp3");
    std::fs::write(&junk, b"not audio at all").unwrap();
    let err = audio_features(&junk, 0, 30_000).unwrap_err();
    assert!(
        matches!(
            err,
            pharos_transcode::libav::frames::FrameError::BadInput(_)
        ),
        "{err:?}"
    );
}

/// Manual tuning aid, not a gate: print what the analyser makes of real songs.
/// `PHAROS_AUDIO_SAMPLES=~/Music cargo nextest run -p pharos-transcode
/// --run-ignored only -E 'test(real_songs)' --no-capture`
#[test]
#[ignore = "reads a local music folder; run by hand to tune"]
fn real_songs() {
    let Ok(dir) = std::env::var("PHAROS_AUDIO_SAMPLES") else {
        eprintln!("set PHAROS_AUDIO_SAMPLES to a folder of songs");
        return;
    };
    let mut stack = vec![std::path::PathBuf::from(dir)];
    let mut n = 0;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !["mp3", "flac", "m4a", "ogg", "opus", "wav"].contains(&ext) || n >= 40 {
                continue;
            }
            n += 1;
            let t = std::time::Instant::now();
            match audio_features(&p, 60_000, 30_000) {
                Ok(f) => eprintln!(
                    "{:>6.1} ms  tag {:>6?}  est {:>6?}  conf {:.2}  energy {:?}  {}",
                    t.elapsed().as_secs_f64() * 1000.0,
                    f.tag_bpm,
                    f.analysed_bpm,
                    f.tempo_confidence,
                    f.energy,
                    p.display()
                ),
                Err(e) => eprintln!("ERR {e:?} {}", p.display()),
            }
        }
    }
}
