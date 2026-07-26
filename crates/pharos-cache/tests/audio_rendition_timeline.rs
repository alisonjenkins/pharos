//! The demuxed audio rendition must sit on the source timeline.
//!
//! Two ways it did not (B120, B121), both invisible to any assertion that only
//! looked at fragment SIZES or timestamps:
//!
//! * a title whose audio track starts after its video (Rogue One: eac3 at
//!   1.700 s, video at 0) had that offset silently discarded by ffmpeg, so
//!   every fragment of the whole-file session carried content 1.7 s later than
//!   the grid position it is served at — audio ahead of picture for the whole
//!   film;
//! * a fragment written by a SEEK session carried its offset within that
//!   session rather than on the timeline, so it buffered minutes early.
//!
//! Both are content/anchor bugs, so both tests here decode real audio and ask
//! where a known beep lands, rather than trusting any header.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pharos_cache::HlsSegmentCache;

/// Absolute time of the beep in the generated source, in seconds.
const BEEP_AT: f64 = 14.0;
/// How late the source's audio track starts against its video.
const AUDIO_START: f64 = 1.7;
/// Fragment under test — `BEEP_AT` falls inside it on a 6 s grid.
const SEG: u32 = 2;

fn ffmpeg(args: &[&str], what: &str) {
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn ffmpeg for {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A source whose VIDEO starts at 0 and whose AUDIO stream starts at
/// `AUDIO_START`, carrying a half-second beep at `BEEP_AT` and silence
/// elsewhere. `-itsoffset` on the audio input is what makes the track's
/// `start_time` genuinely late — padding it with `adelay` instead would leave
/// `start_time` at 0 and reproduce nothing.
fn make_late_audio_source(dir: &Path) -> PathBuf {
    let v = dir.join("v.mkv");
    let a = dir.join("a.mka");
    let src = dir.join("src.mkv");
    ffmpeg(
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25:duration=40",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            v.to_str().expect("utf-8 path"),
        ],
        "video",
    );
    let beep_from = BEEP_AT - AUDIO_START;
    let beep_to = beep_from + 0.5;
    ffmpeg(
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000:duration=40",
            "-af",
            &format!(
                "volume=enable=between(t\\,{beep_from}\\,{beep_to}):volume=1,\
                 volume=enable=1-between(t\\,{beep_from}\\,{beep_to}):volume=0"
            ),
            "-c:a",
            "aac",
            a.to_str().expect("utf-8 path"),
        ],
        "audio",
    );
    ffmpeg(
        &[
            "-v",
            "error",
            "-y",
            "-i",
            v.to_str().expect("utf-8 path"),
            "-itsoffset",
            &format!("{AUDIO_START}"),
            "-i",
            a.to_str().expect("utf-8 path"),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c",
            "copy",
            src.to_str().expect("utf-8 path"),
        ],
        "mux",
    );
    src
}

/// Where the beep sits inside a decoded fragment, in seconds from its start.
fn beep_offset_in(fragment: &Path) -> f64 {
    let raw = fragment.with_extension("raw");
    ffmpeg(
        &[
            "-v",
            "error",
            "-y",
            "-i",
            fragment.to_str().expect("utf-8 path"),
            "-ac",
            "1",
            "-ar",
            "8000",
            "-f",
            "s16le",
            raw.to_str().expect("utf-8 path"),
        ],
        "decode fragment",
    );
    let bytes = std::fs::read(&raw).expect("read decoded fragment");
    let samples: Vec<f64> = bytes
        .chunks_exact(2)
        .map(|c| f64::from(i16::from_le_bytes([c[0], c[1]])).abs())
        .collect();
    assert!(!samples.is_empty(), "fragment decoded to nothing");
    // Envelope, then the first crossing of half the peak: the beep's ONSET,
    // not its centre.
    let w = 80;
    let env: Vec<f64> = (0..samples.len())
        .map(|i| {
            let lo = i.saturating_sub(w / 2);
            let hi = (i + w / 2).min(samples.len());
            samples[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
        })
        .collect();
    let peak = env.iter().cloned().fold(0.0f64, f64::max);
    assert!(
        peak > 0.0,
        "fragment is digital silence — no beep to locate"
    );
    let onset = env.iter().position(|v| *v > peak * 0.5).unwrap_or(0);
    onset as f64 / 8000.0
}

/// Concatenate the rendition's init with one media fragment so it can be
/// decoded standalone.
async fn fragment_file(cache: &HlsSegmentCache, dir: &Path, out: &Path, seg: u32) {
    let init = cache
        .audio_hls_file(dir, "init.mp4")
        .await
        .expect("init produced");
    let media = cache
        .audio_hls_file(dir, &format!("a{seg}.m4s"))
        .await
        .expect("fragment produced");
    let mut all = init.bytes;
    all.extend_from_slice(&media.bytes);
    std::fs::write(out, all).expect("write fragment");
}

/// B120 — the whole-file session must keep the audio track's own start time.
///
/// Without the pad the fragment holds source `[AUDIO_START + SEG*6, …)`, so the
/// beep lands `AUDIO_START` earlier inside it than it should and the audio runs
/// ahead of the picture for the entire title.
#[tokio::test]
async fn a_late_starting_audio_track_still_lands_under_its_own_video() {
    let td = tempfile::tempdir().expect("tempdir");
    let src = make_late_audio_source(td.path());
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);

    // Segment 0 is what spawns the WHOLE-FILE session — the one that drops the
    // offset. Asking for `SEG` directly spawns a session seeked to it, which
    // lands honestly and would test nothing.
    let dir = cache
        .ensure_audio_hls_covering(&src, 42, None, Some(128_000), 0)
        .await
        .expect("audio session");
    let frag = td.path().join("frag.mp4");
    fragment_file(&cache, &dir, &frag, SEG).await;

    let want = BEEP_AT - f64::from(SEG) * 6.0;
    let got = beep_offset_in(&frag);
    assert!(
        (got - want).abs() < 0.05,
        "fragment a{SEG} should hold source [{}, …) so the beep sits {want:.3}s in, \
         but it sits {got:.3}s in — the audio track's {AUDIO_START}s start was dropped, \
         putting audio {:.3}s ahead of picture",
        f64::from(SEG) * 6.0,
        want - got,
    );
}

/// B121 — a fragment produced by a SEEK session must be served anchored to the
/// timeline, not to that session's own first sample.
///
/// The bytes on disk carry `(index - session_start) * 6 s`; only the server
/// knows the session start, so the cache must report it. A caller that trusts
/// the raw `tfdt` buffers this fragment at 0:00.
#[tokio::test]
async fn a_seek_sessions_fragment_reports_the_session_it_came_from() {
    let td = tempfile::tempdir().expect("tempdir");
    let src = make_late_audio_source(td.path());
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);

    // Any non-zero target spawns a session seeked to it, so the fragment two
    // along carries "two segments in" rather than its timeline position.
    let seek_at = 3u32;
    let want_seg = seek_at + 2;
    let dir = cache
        .ensure_audio_hls_covering(&src, 43, None, Some(128_000), seek_at)
        .await
        .expect("audio session");
    let got = cache
        .audio_hls_file(&dir, &format!("a{want_seg}.m4s"))
        .await
        .expect("fragment produced");
    assert_eq!(
        got.session_start_seg, seek_at,
        "a{want_seg} should come from the session seeked to {seek_at}"
    );
    let tfdt = first_tfdt(&got.bytes).expect("fragment carries a tfdt");
    // What ffmpeg wrote is the fragment's index WITHIN its session. Asserting
    // it pins the reason `session_start_seg` has to be reported at all: the
    // bytes alone cannot say where on the timeline this fragment belongs. If a
    // future ffmpeg anchors it absolutely, this fails and the serve-side shift
    // should be deleted rather than double-applied.
    let want = f64::from(want_seg - got.session_start_seg) * 6.0;
    let got_secs = tfdt as f64 / 48_000.0;
    assert!(
        (got_secs - want).abs() < 0.05,
        "a{want_seg} came from the session starting at {}, so its raw tfdt should be \
         {want:.3}s (its offset within that session) but was {got_secs:.3}s",
        got.session_start_seg,
    );
}

/// `baseMediaDecodeTime` of the first `tfdt` in a fragment, in timescale ticks.
/// A deliberately minimal box walk — the fragment is `moof(traf(tfdt))`+`mdat`.
fn first_tfdt(data: &[u8]) -> Option<u64> {
    let at = data.windows(4).position(|w| w == b"tfdt").map(|i| i + 4)?;
    let version = *data.get(at)?;
    if version == 1 {
        Some(u64::from_be_bytes(
            data.get(at + 4..at + 12)?.try_into().ok()?,
        ))
    } else {
        Some(u64::from(u32::from_be_bytes(
            data.get(at + 4..at + 8)?.try_into().ok()?,
        )))
    }
}
