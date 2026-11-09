//! B469/V412 — a window whose source video stops part-way keeps the real
//! frames before the hole, and the filler after them joins on one clock.
//!
//! Real ffmpeg end to end: the unit test in `hls_cache` checks where the
//! filler is asked to start, but only a real encode shows that the two halves
//! decode as one stream (no repeated decode time where they meet) and that
//! the picture before the hole survives.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use std::sync::Arc;

use pharos_cache::HlsSegmentCache;
use pharos_transcode::protocol::{DeviceId, JobSpec, OutputSink, WorkerId};
use pharos_transcode::scheduler::{
    JobClass, RunFuture, SpawnFuture, TranscodeScheduler, Worker, WorkerRunResult, WorkerSpawner,
};
use pharos_transcode::{AudioDelivery, SegmentContainer, SegmentOpts, SegmentVideo};

/// Where the source's video stops, inside segment `SEG`.
const VIDEO_ENDS: f64 = 27.5;
/// Where it picks up again, past the end of `SEG`.
const VIDEO_RESUMES: f64 = 40.0;
/// 24.024 s to 30.030 s on a 6.006 s grid.
const SEG: u32 = 4;
const TITLE_SECS: f64 = 60.0;

fn rate() -> pharos_core::FrameRate {
    pharos_core::FrameRate::from_mille(23_976).unwrap()
}

fn make_source(dir: &Path) -> PathBuf {
    let src = dir.join("src.mkv");
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(format!(
            "testsrc2=duration={TITLE_SECS}:size=128x72:rate=24000/1001,\
             select='not(between(t,{VIDEO_ENDS},{VIDEO_RESUMES}))'"
        ))
        .args([
            "-fps_mode",
            "passthrough",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
        ])
        .args(["-pix_fmt", "yuv420p", "-g", "48", "-y"])
        .arg(&src)
        .output()
        .expect("spawn ffmpeg (is it on PATH? run inside the devShell)");
    assert!(
        out.status.success(),
        "source synth failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    src
}

/// Runs each job's production argv through the real `ffmpeg` on the CPU, in
/// process. The cache only detects a short window, and only reaches the
/// filler, behind a scheduler.
pub(crate) struct FfmpegSpawner;
impl WorkerSpawner for FfmpegSpawner {
    fn spawn(&self, id: WorkerId) -> SpawnFuture {
        Box::pin(async move { Ok(Box::new(FfmpegWorker(id)) as Box<dyn Worker>) })
    }
}
struct FfmpegWorker(WorkerId);
impl Worker for FfmpegWorker {
    fn id(&self) -> WorkerId {
        self.0
    }
    fn run<'a>(&'a mut self, job: JobSpec) -> RunFuture<'a> {
        Box::pin(async move {
            let OutputSink::FileDirect { path } = &job.sink else {
                return WorkerRunResult::Died;
            };
            let args = pharos_transcode::ffmpeg_transcode_args(
                &job.input.to_string_lossy(),
                &job.opts,
                DeviceId::Cpu,
                &path.to_string_lossy(),
                job.decode_offload,
                job.filler,
            );
            let out = tokio::process::Command::new("ffmpeg")
                .args(&args)
                .output()
                .await
                .expect("spawn ffmpeg");
            assert!(
                out.status.success(),
                "ffmpeg failed: {}\nargs: {}",
                String::from_utf8_lossy(&out.stderr),
                args.join(" ")
            );
            let out_bytes = std::fs::metadata(path).map_or(0, |m| m.len());
            WorkerRunResult::Done { out_bytes }
        })
    }
}

fn opts() -> SegmentOpts {
    SegmentOpts {
        source_video_codec: None,
        source_video_kind: Default::default(),
        container: SegmentContainer::Fmp4,
        video: Some(SegmentVideo::H264),
        audio: AudioDelivery::Separate,
        video_bitrate_bps: Some(1_000_000),
        window: pharos_core::SegmentWindow::for_segment(SEG, Some(rate()), Some(TITLE_SECS)),
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

/// Video packet decode times, in file order.
fn packet_dts(path: &Path) -> Vec<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "packet=dts_time",
        ])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(out.status.success(), "ffprobe failed on {}", path.display());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// `(pts, mean luma)` of every decoded video frame.
fn frame_luma(path: &Path) -> Vec<(f64, f64)> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-vf",
            "signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-",
        ])
        .args(["-f", "null", "-"])
        .output()
        .expect("spawn ffmpeg");
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "the joined segment must decode cleanly: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut frames = Vec::new();
    let mut pts = None;
    for line in text.lines() {
        if let Some(t) = line.split("pts_time:").nth(1) {
            pts = t.trim().parse().ok();
        } else if let Some(y) = line.strip_prefix("lavfi.signalstats.YAVG=") {
            frames.push((pts.take().expect("pts before YAVG"), y.parse().unwrap()));
        }
    }
    frames
}

#[tokio::test]
async fn a_window_cut_short_by_a_hole_keeps_its_picture_and_joins_the_filler_on_one_clock() {
    let td = tempfile::tempdir().unwrap();
    let src = make_source(td.path());
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30).with_scheduler(
        TranscodeScheduler::spawn(
            pharos_transcode::device::DeviceTable::from_probe(&[], 4),
            Arc::new(FfmpegSpawner),
            pharos_transcode::scheduler::SchedConfig::default(),
        ),
    );
    let window = opts().window;

    let bytes = cache
        .segment_bytes(42, SEG, &src, &opts(), JobClass::Interactive)
        .await
        .expect("a window cut short by a hole still serves");
    let seg = td.path().join("seg.mp4");
    std::fs::write(&seg, bytes).unwrap();

    let dts = packet_dts(&seg);
    let backwards: Vec<_> = dts.windows(2).filter(|p| p[1] <= p[0]).collect();
    assert!(
        backwards.is_empty(),
        "decode times must rise across the join, but {backwards:?} do not"
    );

    let frames = frame_luma(&seg);
    let expected = (window.duration_seconds() * rate().fps()).round() as usize;
    assert!(
        frames.len() + 1 >= expected && frames.len() <= expected + 1,
        "the window holds {expected} frames (±1), got {}",
        frames.len()
    );
    // ffmpeg rebases decoded times to the segment's start.
    let hole_at = VIDEO_ENDS - window.start_seconds();
    // testsrc2 averages well above black (luma 16); the filler is black.
    let real = frames
        .iter()
        .filter(|(t, _)| *t < hole_at - 0.1)
        .collect::<Vec<_>>();
    let lit = real.iter().filter(|(_, y)| *y > 30.0).count();
    assert!(
        !real.is_empty() && lit == real.len(),
        "the {} frames before the hole must keep their picture, {lit} do",
        real.len()
    );
    let after = frames.iter().filter(|(t, _)| *t > hole_at + 0.1);
    assert!(
        after.clone().all(|(_, y)| *y < 30.0) && after.count() > 0,
        "after the hole the window is filler"
    );
}
