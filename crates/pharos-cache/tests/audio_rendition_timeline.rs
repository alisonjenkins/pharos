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
        .ensure_audio_hls_covering(&src, None, 42, None, Some(128_000), 0)
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
        .ensure_audio_hls_covering(&src, None, 43, None, Some(128_000), seek_at)
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

/// Where the source's audio packets stop and resume, in seconds: a zeroed
/// stretch of the file, as in Buffy S02E11 (B468).
const HOLE: (f64, f64) = (8.0, 12.5);
/// A hole shorter than ffmpeg's default 100 ms `min_hard_comp`, inside a3.
const SHORT_HOLE: (f64, f64) = (20.0, 20.05);

/// An audio-only source whose packet timestamps jump over `hole`, silent but
/// for a half-second beep at `beep_at`: `aselect` without `asetpts` drops the
/// frames but keeps the survivors' timestamps.
fn make_holed_audio_source(dir: &Path, hole: (f64, f64), beep_at: f64) -> PathBuf {
    let src = dir.join("holed.mka");
    let (from, to) = hole;
    let beep_to = beep_at + 0.5;
    ffmpeg(
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!(
                "sine=frequency=1000:sample_rate=48000:duration=40:samples_per_frame=480,\
                 volume=enable=1-between(t\\,{beep_at}\\,{beep_to}):volume=0,\
                 aselect=not(between(t\\,{from}\\,{to}))"
            ),
            "-c:a",
            "pcm_s16le",
            src.to_str().expect("utf-8 path"),
        ],
        "holed source",
    );
    src
}

/// Seconds of audio a standalone fragment decodes to.
fn decoded_secs(fragment: &Path) -> f64 {
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
    std::fs::metadata(&raw).expect("decoded fragment").len() as f64 / 2.0 / 8000.0
}

/// B468/V411 — a hole in the source's audio must come out as silence, so every
/// fragment decodes to its full window.
///
/// Without the fill the mp4 muxer stretched the last sample before the hole
/// across it: the fragment's timeline still spanned the window but held only
/// the audio up to the hole. Firefox resuming inside that one long sample after
/// a stall froze, or played the post-hole audio early.
#[tokio::test]
async fn a_hole_in_the_source_audio_is_filled_with_silence() {
    let td = tempfile::tempdir().expect("tempdir");
    let src = make_holed_audio_source(td.path(), HOLE, 7.0);
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);
    let dir = cache
        .ensure_audio_hls_covering(&src, None, 44, None, Some(128_000), 0)
        .await
        .expect("audio session");
    // a1 spans [6, 12): the hole starts 2 s in and runs past its end.
    let frag = td.path().join("holed.mp4");
    fragment_file(&cache, &dir, &frag, 1).await;
    let got = decoded_secs(&frag);
    let want = HlsSegmentCache::AUDIO_SEGMENT_SECONDS;
    assert!(
        (got - want).abs() < 0.1,
        "a1 covers [6, 12) and should decode to {want:.3}s of audio, silence included, \
         but decoded to {got:.3}s — the hole at {:.1}–{:.1}s was not filled",
        HOLE.0,
        HOLE.1,
    );
}

/// B472 — a hole shorter than 100 ms is filled too.
///
/// `aresample=async=1` alone pads only past its 100 ms `min_hard_comp`; a
/// shorter hole was closed up, leaving everything after it that much early.
#[tokio::test]
async fn a_hole_shorter_than_100ms_is_filled_with_silence() {
    let td = tempfile::tempdir().expect("tempdir");
    let beep_at = 21.0;
    let src = make_holed_audio_source(td.path(), SHORT_HOLE, beep_at);
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);
    let dir = cache
        .ensure_audio_hls_covering(&src, None, 45, None, Some(128_000), 0)
        .await
        .expect("audio session");
    // a3 spans [18, 24) and holds the whole 50 ms hole, then the beep.
    let frag = td.path().join("short_hole.mp4");
    fragment_file(&cache, &dir, &frag, 3).await;
    let want = beep_at - 18.0;
    let got = beep_offset_in(&frag);
    assert!(
        (got - want).abs() < 0.02,
        "the beep after a {:.0} ms hole should sit {want:.3}s into a3 but sits {got:.3}s \
         in — the hole was closed up instead of filled",
        (SHORT_HOLE.1 - SHORT_HOLE.0) * 1000.0,
    );
}

/// B473 — a seek session that starts inside a hole fills the hole's tail at
/// its start, so what follows the hole stays at its true time.
#[tokio::test]
async fn a_seek_session_starting_inside_a_hole_fills_its_start_with_silence() {
    let td = tempfile::tempdir().expect("tempdir");
    let beep_at = 13.0;
    let src = make_holed_audio_source(td.path(), HOLE, beep_at);
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);
    // a2 spans [12, 18); the hole runs to 12.5, so the session's first packet
    // arrives 0.5 s after the point it seeked to.
    let dir = cache
        .ensure_audio_hls_covering(&src, None, 46, None, Some(128_000), 2)
        .await
        .expect("seek session");
    let frag = td.path().join("seek_in_hole.mp4");
    fragment_file(&cache, &dir, &frag, 2).await;
    let want = beep_at - 12.0;
    let got = beep_offset_in(&frag);
    assert!(
        (got - want).abs() < 0.02,
        "a2's beep should sit {want:.3}s in, after {:.1}s of silence, but sits {got:.3}s in",
        HOLE.1 - 12.0,
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
