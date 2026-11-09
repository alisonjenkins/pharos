//! V820 — the TV player's fMP4 reader agrees with ffmpeg on the segments the
//! server actually serves.
//!
//! `pharos-hls`'s sample table, codec boxes and Annex B rewrite (spec 032
//! slice 3) were checked against fixtures straight from ffmpeg's HLS muxer.
//! The server does not serve those: each segment is an independent encode
//! with source-anchored `tfdt`, then `process_segment` splits off the init,
//! clamps a negative `tfdt` and drops `mfra`. This runs the production argv
//! through real ffmpeg, post-processes it as the h264cmaf and audio routes do,
//! and asks three things the native player depends on:
//!
//! * every sample the reader finds is a packet ffprobe finds, with the same
//!   size, key flag and decode time;
//! * decode times run on across segment boundaries, never backwards, so the
//!   feeder can queue segment N+1 behind N without re-anchoring;
//! * the video, rewritten to Annex B with the init's parameter sets in front
//!   (what `AMediaCodec` is handed as `csd-0`/`csd-1` and input buffers),
//!   decodes cleanly to one frame per sample.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use pharos_cache::{fmp4 as server_fmp4, HlsSegmentCache};
use pharos_hls::annexb;
use pharos_hls::fmp4::{self, CodecConfig, Sample, TrackKind};
use pharos_transcode::scheduler::{JobClass, TranscodeScheduler};
use pharos_transcode::{AudioDelivery, SegmentContainer, SegmentOpts, SegmentVideo};

use super::filler_after_hole::FfmpegSpawner;

const TITLE_SECS: f64 = 30.0;
/// Segments under test: enough for two boundaries.
const SEGS: u32 = 3;

fn rate() -> pharos_core::FrameRate {
    pharos_core::FrameRate::from_mille(23_976).unwrap()
}

fn run(cmd: &mut Command, what: &str) -> std::process::Output {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn {what} (run inside the devShell): {e}"));
    assert!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A 23.976 fps H.264 + AAC title, the shape of most of the library.
fn make_source(dir: &Path) -> PathBuf {
    let src = dir.join("src.mkv");
    run(
        Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!(
                "testsrc2=duration={TITLE_SECS}:size=128x72:rate=24000/1001"
            ))
            .args(["-f", "lavfi", "-i"])
            .arg(format!(
                "sine=frequency=440:sample_rate=48000:duration={TITLE_SECS}"
            ))
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-bf", "2"])
            .args(["-pix_fmt", "yuv420p", "-g", "48", "-c:a", "aac"])
            .arg(&src),
        "source synth",
    );
    src
}

fn video_opts(seg: u32) -> SegmentOpts {
    SegmentOpts {
        source_video_codec: None,
        source_video_kind: Default::default(),
        container: SegmentContainer::Fmp4,
        video: Some(SegmentVideo::H264),
        audio: AudioDelivery::Separate,
        video_bitrate_bps: Some(1_000_000),
        window: pharos_core::SegmentWindow::for_segment(seg, Some(rate()), Some(TITLE_SECS)),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        burn_intent: false,
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
        video_3d_format: None,
        stereo_output: Default::default(),
        tonemap: Default::default(),
        output_size: None,
    }
}

/// One packet as ffprobe reports it.
#[derive(Debug)]
struct Packet {
    dts: i64,
    size: u32,
    key: bool,
}

/// ffprobe's packets for the first stream of `init` + `media`, in decode
/// order, with `dts` in the stream's own timescale.
fn probe_packets(dir: &Path, name: &str, init: &[u8], media: &[u8]) -> Vec<Packet> {
    let path = dir.join(name);
    let mut all = init.to_vec();
    all.extend_from_slice(media);
    std::fs::write(&path, all).unwrap();
    let out = run(
        Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "0"])
            .args(["-show_entries", "packet=dts,size,flags", "-of", "csv=p=0"])
            .arg(&path),
        "ffprobe",
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.trim().split(',').collect();
            Packet {
                dts: f[0].parse().unwrap(),
                size: f[1].parse().unwrap(),
                key: f[2].starts_with('K'),
            }
        })
        .collect()
}

/// The reader's samples must be ffprobe's packets, one for one.
fn assert_matches_ffprobe(what: &str, samples: &[Sample], packets: &[Packet]) {
    assert_eq!(
        samples.len(),
        packets.len(),
        "{what}: the reader found {} samples, ffprobe {} packets",
        samples.len(),
        packets.len()
    );
    for (i, (s, p)) in samples.iter().zip(packets).enumerate() {
        assert_eq!(
            (s.size, s.sync, s.decode_time as i64),
            (p.size, p.key, p.dts),
            "{what} sample {i}: reader (size, key, dts) vs ffprobe {p:?}"
        );
    }
}

/// Segment N+1 must start where N ended, give or take the rounding of one
/// sample duration to the track's timescale, and never before it.
fn assert_runs_on(what: &str, segments: &[Vec<Sample>]) {
    for (n, pair) in segments.windows(2).enumerate() {
        let last = pair[0].last().expect("segment has samples");
        let end = last.decode_time + u64::from(last.duration);
        let next = pair[1].first().expect("segment has samples").decode_time;
        let gap = next as i64 - end as i64;
        assert!(
            (0..=i64::from(last.duration)).contains(&gap),
            "{what}: segment {} ends at {end} and segment {} starts at {next} \
             ({gap:+} ticks); the feeder would queue overlapping or gapped input",
            n,
            n + 1
        );
    }
}

/// The native player's input for the whole title so far: parameter sets
/// first, then every sample in Annex B, decoded by ffmpeg as raw H.264.
/// Returns the decoded frame count.
fn decode_annex_b(dir: &Path, avcc: &[u8], segments: &[(Vec<u8>, Vec<Sample>)]) -> usize {
    let ps = annexb::avc_parameter_sets(avcc).expect("avcC parses");
    let mut stream = Vec::new();
    for nal in ps.sps.iter().chain(&ps.pps) {
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(nal);
    }
    for (media, samples) in segments {
        for s in samples {
            let data = s.data(media).expect("sample inside its segment");
            annexb::to_annex_b(data, ps.length_size, &mut stream).expect("length-prefixed NALs");
        }
    }
    let path = dir.join("feed.h264");
    std::fs::write(&path, stream).unwrap();
    // ffprobe decodes every frame to count it; ffmpeg's own output stage
    // would drop or duplicate frames to fit a frame rate the raw stream lacks.
    let out = run(
        Command::new("ffprobe")
            .args(["-v", "error", "-f", "h264", "-count_frames"])
            .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
            .arg(&path),
        "decode Annex B",
    );
    assert!(
        out.stderr.is_empty(),
        "the Annex B feed must decode without errors: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("ffprobe frame count")
}

#[tokio::test]
async fn the_tv_reader_agrees_with_ffmpeg_on_served_video_segments() {
    let td = tempfile::tempdir().unwrap();
    let src = make_source(td.path());
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30).with_scheduler(
        TranscodeScheduler::spawn(
            pharos_transcode::device::DeviceTable::from_probe(&[], 4),
            Arc::new(FfmpegSpawner),
            pharos_transcode::scheduler::SchedConfig::default(),
        ),
    );

    // The init route serves segment 0's init for every media segment.
    let mut init = None;
    let mut segments = Vec::new();
    for seg in 0..SEGS {
        let raw = cache
            .segment_bytes(7, seg, &src, &video_opts(seg), JobClass::Interactive)
            .await
            .expect("segment encodes");
        let processed = server_fmp4::process_segment(&raw).expect("server post-processing");
        init.get_or_insert(processed.init);
        segments.push(processed.media);
    }
    let init = init.unwrap();

    let entries = fmp4::sample_entries(&init).expect("init sample entries");
    assert_eq!(entries.len(), 1, "the h264cmaf rendition is video only");
    let entry = &entries[0];
    assert_eq!(&entry.format, b"avc1");
    assert!(
        matches!(
            entry.kind,
            TrackKind::Video {
                width: 128,
                height: 72
            }
        ),
        "picture size from the sample entry: {:?}",
        entry.kind
    );
    let Some(CodecConfig::Avc(avcc)) = &entry.config else {
        panic!("avc1 carries avcC, got {:?}", entry.config);
    };

    let defaults = fmp4::track_defaults(&init).expect("trex");
    let mut parsed = Vec::new();
    for (seg, media) in segments.iter().enumerate() {
        let samples = fmp4::samples(media, &defaults).expect("served segment's sample table");
        assert!(
            samples.first().is_some_and(|s| s.sync),
            "segment {seg} must open on a key frame so the feeder can start or switch there"
        );
        let packets = probe_packets(td.path(), &format!("v{seg}.mp4"), &init, media);
        assert_matches_ffprobe(&format!("video segment {seg}"), &samples, &packets);
        parsed.push(samples);
    }
    assert_runs_on("video", &parsed);

    let fed: Vec<_> = segments
        .iter()
        .cloned()
        .zip(parsed.iter().cloned())
        .collect();
    let samples: usize = parsed.iter().map(Vec::len).sum();
    let frames = decode_annex_b(td.path(), avcc, &fed);
    assert_eq!(
        frames, samples,
        "every sample handed to the decoder must come out as one frame"
    );
}

#[tokio::test]
async fn the_tv_reader_agrees_with_ffmpeg_on_the_served_audio_rendition() {
    let td = tempfile::tempdir().unwrap();
    let src = make_source(td.path());
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);
    // Segment 0 starts the whole-file session, the one ordinary playback uses:
    // its fragments need no serve-side shift.
    let dir = cache
        .ensure_audio_hls_covering(&src, None, 8, None, Some(128_000), 0)
        .await
        .expect("audio session");
    let init = cache
        .audio_hls_file(&dir, "init.mp4")
        .await
        .expect("audio init")
        .bytes;

    let entries = fmp4::sample_entries(&init).expect("audio init sample entries");
    assert_eq!(entries.len(), 1, "the audio rendition is audio only");
    let entry = &entries[0];
    assert!(
        matches!(
            entry.kind,
            TrackKind::Audio {
                sample_rate: 48_000,
                ..
            }
        ),
        "audio sample entry: {:?}",
        entry.kind
    );
    assert!(
        matches!(
            entry.config,
            Some(CodecConfig::Aac { .. } | CodecConfig::Opus(_))
        ),
        "the decoder needs the codec box; got {:?} for {:?}",
        entry.config,
        std::str::from_utf8(&entry.format)
    );

    let defaults = fmp4::track_defaults(&init).expect("trex");
    let mut parsed = Vec::new();
    for seg in 0..SEGS {
        let media = cache
            .audio_hls_file(&dir, &format!("a{seg}.m4s"))
            .await
            .expect("audio fragment")
            .bytes;
        let samples = fmp4::samples(&media, &defaults).expect("served fragment's sample table");
        let packets = probe_packets(td.path(), &format!("a{seg}.mp4"), &init, &media);
        assert_matches_ffprobe(&format!("audio fragment {seg}"), &samples, &packets);
        parsed.push(samples);
    }
    assert_runs_on("audio", &parsed);
}
