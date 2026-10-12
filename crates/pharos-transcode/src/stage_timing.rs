//! Per-job stage timing for ffmpeg encodes.
//!
//! A VAAPI segment takes about as long as a CPU one, and nothing recorded
//! where the time goes. This module turns ffmpeg's own machine-readable
//! output into a bounded set of stage durations:
//!
//! - **basic** (default, always on): process spawn, spawn to first encoded
//!   frame, total wall, frames, effective fps and speed. Sourced from the
//!   `-progress` sidecar the file-sink argv already writes plus two
//!   `Instant`s in the worker. No extra stderr output.
//! - **detailed** (diagnostic): adds `-benchmark -benchmark_all` and parses
//!   its stderr lines into decode / encode / flush seconds. ffmpeg 9 does not
//!   report per-filter time or input-open time, so everything else in the
//!   process wall lands in the `other` stage (open + demux + filters + mux).
//!
//! The parsers are pure so they are tested against captured ffmpeg output.

use crate::hwaccel::HwAccel;
use crate::options::VideoCodec;
use crate::protocol::DeviceId;
use serde::{Deserialize, Serialize};

/// Environment variable the server sets on `transcode-worker` so the worker
/// process (which has no config) knows the mode.
pub const ENV_STAGE_TIMING: &str = "PHAROS_TRANSCODE_STAGE_TIMING";

/// How much per-job timing to collect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StageTimingMode {
    /// No timing at all.
    Off,
    /// Spawn, first frame, total, frames, fps, speed. Default.
    #[default]
    Basic,
    /// `Basic` plus `-benchmark_all` per-stage seconds. Adds ~1.7k stderr
    /// lines per 6 s segment, so it is a diagnostic mode.
    Detailed,
}

impl StageTimingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Basic => "basic",
            Self::Detailed => "detailed",
        }
    }

    /// Parse the env/config spelling; unknown values are `None` so the caller
    /// can say so instead of silently picking a mode.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "basic" => Some(Self::Basic),
            "detailed" => Some(Self::Detailed),
            _ => None,
        }
    }

    /// Mode from [`ENV_STAGE_TIMING`]; unset or unparsable means the default.
    pub fn from_env() -> Self {
        std::env::var(ENV_STAGE_TIMING)
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_default()
    }
}

/// The bounded `stage` label values. Renaming one breaks a dashboard
/// silently, so the strings are spelled out in [`Stage::label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    /// `Command::spawn` returning (fork + exec).
    Spawn,
    /// Spawn until ffmpeg's progress report first shows an encoded frame.
    FirstFrame,
    /// Spawn until exit.
    Total,
    DecodeVideo,
    DecodeAudio,
    EncodeVideo,
    EncodeAudio,
    Flush,
    /// Process wall minus the benchmarked stages: input open, demux,
    /// filters and mux, which `-benchmark_all` does not separate.
    Other,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::FirstFrame => "first_frame",
            Self::Total => "total",
            Self::DecodeVideo => "decode_video",
            Self::DecodeAudio => "decode_audio",
            Self::EncodeVideo => "encode_video",
            Self::EncodeAudio => "encode_audio",
            Self::Flush => "flush",
            Self::Other => "other",
        }
    }
}

/// What the last `-progress` block said.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ProgressReport {
    pub frames: u64,
    /// Output timeline position (`out_time_us`), microseconds.
    pub out_time_us: u64,
    /// ffmpeg's own `speed=` (media seconds per wall second).
    pub speed: Option<f64>,
    /// `progress=end` seen, so the numbers are final.
    pub ended: bool,
}

/// Parse the body of a `-progress` report and return its LAST values. A
/// report is `key=value` lines in blocks closed by `progress=continue|end`;
/// a sidecar read mid-write can end inside a block, so the keys are taken
/// as they come and later ones win.
pub fn parse_progress(text: &str) -> ProgressReport {
    let mut r = ProgressReport::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "frame" => {
                if let Ok(n) = v.parse() {
                    r.frames = n;
                }
            }
            "out_time_us" => {
                if let Ok(n) = v.parse::<i64>() {
                    r.out_time_us = u64::try_from(n).unwrap_or(0);
                }
            }
            "speed" => {
                r.speed = v.trim_end_matches('x').trim().parse().ok();
            }
            "progress" => r.ended = v == "end",
            _ => {}
        }
    }
    r
}

/// Per-stage sums from `-benchmark_all` plus the closing `-benchmark` totals.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BenchReport {
    /// Seconds of wall spent inside each benchmarked call, summed.
    pub decode_video: f64,
    pub decode_audio: f64,
    pub encode_video: f64,
    pub encode_audio: f64,
    pub flush: f64,
    /// Process-level CPU and wall from the closing `bench: utime=` line.
    pub utime: Option<f64>,
    pub stime: Option<f64>,
    pub rtime: Option<f64>,
    pub maxrss_kib: Option<u64>,
}

impl BenchReport {
    fn benchmarked(&self) -> f64 {
        self.decode_video + self.decode_audio + self.encode_video + self.encode_audio + self.flush
    }
}

fn micros_to_secs(us: u64) -> f64 {
    us as f64 / 1_000_000.0
}

fn parse_secs(v: &str) -> Option<f64> {
    v.trim().trim_end_matches('s').parse().ok()
}

/// Parse ffmpeg's `-benchmark` / `-benchmark_all` stderr lines. Anything that
/// is not a `bench:` line is ignored, so the whole stderr can be passed in.
///
/// Per-call lines are `bench: <u> user <s> sys <r> real <label> <idx>` in
/// microseconds; the closing lines are `bench: utime=…s stime=…s rtime=…s`
/// and `bench: maxrss=…KiB`.
pub fn parse_benchmark_all(stderr: &str) -> BenchReport {
    let mut b = BenchReport::default();
    for line in stderr.lines() {
        let Some(rest) = line.strip_prefix("bench:") else {
            continue;
        };
        let rest = rest.trim();
        if rest.starts_with("utime=") {
            for (k, v) in rest.split_whitespace().filter_map(|p| p.split_once('=')) {
                match k {
                    "utime" => b.utime = parse_secs(v),
                    "stime" => b.stime = parse_secs(v),
                    "rtime" => b.rtime = parse_secs(v),
                    _ => {}
                }
            }
            continue;
        }
        if let Some(kib) = rest.strip_prefix("maxrss=") {
            b.maxrss_kib = kib.trim_end_matches("KiB").trim().parse().ok();
            continue;
        }
        let f: Vec<&str> = rest.split_whitespace().collect();
        // <u> user <s> sys <r> real <label> [idx]
        if let (Some(real), Some(label), Some(&"real")) = (f.get(4), f.get(6), f.get(5)) {
            let Ok(real) = real.parse::<u64>() else {
                continue;
            };
            // ffmpeg prints an unsigned diff of two timestamps, so a clock
            // step backwards arrives as a number near u64::MAX. It is a
            // negative duration, not 5 hours: count it as zero.
            let secs = if i64::try_from(real).is_ok() {
                micros_to_secs(real)
            } else {
                0.0
            };
            match *label {
                "decode_video" => b.decode_video += secs,
                "decode_audio" => b.decode_audio += secs,
                "encode_video" => b.encode_video += secs,
                "encode_audio" => b.encode_audio += secs,
                "flush_video" | "flush_audio" => b.flush += secs,
                _ => {}
            }
        }
    }
    b
}

/// `stderr` without `bench:` lines, so failure classification and the echoed
/// stderr tail are not buried under thousands of per-call lines.
pub fn strip_bench_lines(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| !l.starts_with("bench:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Global ffmpeg options a mode adds in front of the job's argv.
///
/// `-stats_period` only matters for a file sink, where the argv already
/// writes a `-progress` sidecar; it makes the first-frame poll resolve to
/// ~100 ms instead of ffmpeg's default 500 ms. `-benchmark_all` writes about
/// 1.7k stderr lines per 6 s segment, which is why it is diagnostic only.
///
/// `-benchmark` lines are logged at info level and the segment argv runs at
/// `-loglevel error`, so detailed mode lifts that to `info` as well. The extra
/// chatter is stderr only; [`strip_bench_lines`] keeps the bench lines out of
/// failure classification.
pub fn apply_mode_args(args: Vec<String>, mode: StageTimingMode, file_sink: bool) -> Vec<String> {
    if mode == StageTimingMode::Off {
        return args;
    }
    let mut a: Vec<String> = Vec::with_capacity(args.len() + 4);
    if file_sink {
        a.extend(["-stats_period".into(), "0.1".into()]);
    }
    if mode == StageTimingMode::Detailed {
        a.extend(["-benchmark".into(), "-benchmark_all".into()]);
    }
    let mut after_loglevel = false;
    for arg in args {
        if after_loglevel && mode == StageTimingMode::Detailed && arg == "error" {
            a.push("info".into());
        } else {
            a.push(arg.clone());
        }
        after_loglevel = arg == "-loglevel";
    }
    a
}

/// One job's collected timing, sent worker to scheduler.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StageTiming {
    pub spawn_secs: f64,
    /// `None` when no frame was observed (sink without a sidecar, or a job
    /// that finished inside the polling resolution).
    pub first_frame_secs: Option<f64>,
    pub total_secs: f64,
    pub progress: Option<ProgressReport>,
    pub bench: Option<BenchReport>,
}

impl StageTiming {
    /// Frames per wall second over the whole job, from the final report.
    /// ffmpeg's own `fps=` reads 0.00 on fast jobs, so it is recomputed.
    pub fn effective_fps(&self) -> Option<f64> {
        let p = self.progress?;
        (p.frames > 0 && self.total_secs > 0.0).then(|| p.frames as f64 / self.total_secs)
    }

    /// Media seconds produced per wall second.
    pub fn speed(&self) -> Option<f64> {
        let p = self.progress?;
        if p.out_time_us > 0 && self.total_secs > 0.0 {
            Some(micros_to_secs(p.out_time_us) / self.total_secs)
        } else {
            p.speed
        }
    }

    /// The (stage, seconds) pairs this timing carries. Detailed stages appear
    /// only when a benchmark report was collected.
    pub fn stages(&self) -> Vec<(Stage, f64)> {
        let mut v = vec![(Stage::Spawn, self.spawn_secs)];
        if let Some(ff) = self.first_frame_secs {
            v.push((Stage::FirstFrame, ff));
        }
        v.push((Stage::Total, self.total_secs));
        if let Some(b) = &self.bench {
            v.push((Stage::DecodeVideo, b.decode_video));
            v.push((Stage::DecodeAudio, b.decode_audio));
            v.push((Stage::EncodeVideo, b.encode_video));
            v.push((Stage::EncodeAudio, b.encode_audio));
            v.push((Stage::Flush, b.flush));
            let wall = b.rtime.unwrap_or(self.total_secs);
            v.push((Stage::Other, (wall - b.benchmarked()).max(0.0)));
        }
        v
    }

    /// Export as histograms and as one event on the current span (the
    /// scheduler calls this inside the job's `transcode_job` span). Labels
    /// are bounded: device kind, codec, stage; never an item or job id.
    pub fn record(&self, device_kind: &'static str, codec: &'static str) {
        for (stage, secs) in self.stages() {
            metrics::histogram!(
                "pharos_transcode_stage_seconds",
                "device_kind" => device_kind,
                "codec" => codec,
                "stage" => stage.label(),
            )
            .record(secs);
        }
        if let Some(fps) = self.effective_fps() {
            metrics::histogram!(
                "pharos_transcode_effective_fps",
                "device_kind" => device_kind,
                "codec" => codec,
            )
            .record(fps);
        }
        if let Some(speed) = self.speed() {
            metrics::histogram!(
                "pharos_transcode_speed_ratio",
                "device_kind" => device_kind,
                "codec" => codec,
            )
            .record(speed);
        }
        if let Some(p) = self.progress {
            metrics::counter!(
                "pharos_transcode_frames_total",
                "device_kind" => device_kind,
                "codec" => codec,
            )
            .increment(p.frames);
        }
        let bench = self.bench.as_ref();
        tracing::info!(
            device_kind,
            codec,
            spawn_ms = ms(self.spawn_secs),
            first_frame_ms = self.first_frame_secs.map(ms),
            total_ms = ms(self.total_secs),
            frames = self.progress.map(|p| p.frames),
            effective_fps = self.effective_fps(),
            speed = self.speed(),
            decode_video_ms = bench.map(|b| ms(b.decode_video)),
            decode_audio_ms = bench.map(|b| ms(b.decode_audio)),
            encode_video_ms = bench.map(|b| ms(b.encode_video)),
            encode_audio_ms = bench.map(|b| ms(b.encode_audio)),
            flush_ms = bench.map(|b| ms(b.flush)),
            ffmpeg_cpu_user_ms = bench.and_then(|b| b.utime).map(ms),
            ffmpeg_cpu_sys_ms = bench.and_then(|b| b.stime).map(ms),
            ffmpeg_maxrss_kib = bench.and_then(|b| b.maxrss_kib),
            "transcode stage timing"
        );
    }
}

fn ms(secs: f64) -> u64 {
    (secs * 1000.0).round().max(0.0) as u64
}

/// Bounded `device_kind` label.
pub fn device_kind(device: DeviceId) -> &'static str {
    match device {
        DeviceId::Cpu => "cpu",
        DeviceId::Hw { accel, .. } => match accel {
            HwAccel::Vaapi => "vaapi",
            HwAccel::Nvenc => "nvenc",
            HwAccel::Qsv => "qsv",
            HwAccel::VideoToolbox => "videotoolbox",
            HwAccel::Off | HwAccel::Auto => "cpu",
        },
    }
}

/// Bounded `codec` label for the job's video target.
pub fn codec_label(video: Option<VideoCodec>) -> &'static str {
    match video {
        Some(VideoCodec::H264) => "h264",
        Some(VideoCodec::H265) => "hevc",
        Some(VideoCodec::Vp9) => "vp9",
        Some(VideoCodec::Av1) => "av1",
        Some(VideoCodec::Copy) => "copy",
        None => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BENCH: &str = include_str!("../tests/fixtures/ffmpeg_stats/bench_all_h264_cpu.txt");
    const PROGRESS: &str = include_str!("../tests/fixtures/ffmpeg_stats/progress_end.txt");

    #[test]
    fn progress_takes_the_last_block() {
        let p = parse_progress(PROGRESS);
        assert_eq!(p.frames, 144);
        assert_eq!(p.out_time_us, 6_000_000);
        assert_eq!(p.speed, Some(7.32));
        assert!(p.ended);
    }

    #[test]
    fn a_progress_block_cut_mid_write_still_reports_what_it_has() {
        let p = parse_progress("frame=10\nfps=0.00\nout_time_us=139320\nspe");
        assert_eq!(p.frames, 10);
        assert_eq!(p.out_time_us, 139_320);
        assert!(!p.ended);
        assert_eq!(p.speed, None);
    }

    #[test]
    fn progress_na_values_do_not_panic_or_go_negative() {
        let p = parse_progress("frame=N/A\nout_time_us=-9223372036854775807\nspeed=N/A\n");
        assert_eq!(p.frames, 0);
        assert_eq!(p.out_time_us, 0);
        assert_eq!(p.speed, None);
    }

    #[test]
    fn benchmark_all_sums_real_time_per_stage() {
        let b = parse_benchmark_all(BENCH);
        // 4 + 13 us of audio decode, 5791 + 3 of video decode, etc.
        assert!((b.decode_audio - 0.000_017).abs() < 1e-9, "{b:?}");
        assert!((b.decode_video - 0.005_794).abs() < 1e-9, "{b:?}");
        assert!((b.encode_audio - 0.000_111).abs() < 1e-9, "{b:?}");
        assert!((b.encode_video - 0.003_032).abs() < 1e-9, "{b:?}");
        assert!((b.flush - 0.000_398).abs() < 1e-9, "{b:?}");
        assert_eq!(b.utime, Some(2.016));
        assert_eq!(b.stime, Some(1.099));
        assert_eq!(b.rtime, Some(0.820));
        assert_eq!(b.maxrss_kib, Some(438_580));
    }

    #[test]
    fn a_wrapped_negative_duration_counts_as_zero() {
        // Seen in a real run: one encode_audio call summed to ~1.8e13 s, i.e.
        // a value a few microseconds below u64::MAX.
        let b = parse_benchmark_all(
            "bench:        3 user        1 sys 18446744073709551600 real encode_audio 0.1 \n\
             bench:        3 user        1 sys             2000 real encode_audio 0.1 \n",
        );
        assert!((b.encode_audio - 0.002).abs() < 1e-9, "{b:?}");
    }

    #[test]
    fn benchmark_parser_ignores_everything_that_is_not_a_bench_line() {
        let b = parse_benchmark_all("[libx264 @ 0x1] kb/s:1300\nframe=  144 fps=0.0\n");
        assert_eq!(b, BenchReport::default());
    }

    #[test]
    fn stripping_bench_lines_keeps_the_error_text() {
        let s = strip_bench_lines(
            "bench:  1 user 2 sys 3 real decode_video 0:0 \n[h264_vaapi @ 0x1] Cannot load libva\nbench: utime=1.0s stime=0.1s rtime=0.5s",
        );
        assert_eq!(s, "[h264_vaapi @ 0x1] Cannot load libva");
    }

    #[test]
    fn fps_is_recomputed_because_ffmpegs_own_reads_zero() {
        let t = StageTiming {
            total_secs: 0.82,
            progress: Some(parse_progress(PROGRESS)),
            ..Default::default()
        };
        let fps = t.effective_fps().unwrap_or(0.0);
        assert!((fps - 144.0 / 0.82).abs() < 1e-9, "{fps}");
        let speed = t.speed().unwrap_or(0.0);
        assert!((speed - 6.0 / 0.82).abs() < 1e-9, "{speed}");
    }

    #[test]
    fn basic_mode_carries_no_benchmark_stages() {
        let t = StageTiming {
            spawn_secs: 0.002,
            first_frame_secs: Some(0.3),
            total_secs: 2.0,
            ..Default::default()
        };
        let labels: Vec<_> = t.stages().into_iter().map(|(s, _)| s.label()).collect();
        assert_eq!(labels, ["spawn", "first_frame", "total"]);
    }

    #[test]
    fn detailed_mode_attributes_the_remainder_to_other() {
        let t = StageTiming {
            total_secs: 0.9,
            bench: Some(parse_benchmark_all(BENCH)),
            ..Default::default()
        };
        let other = t
            .stages()
            .into_iter()
            .find(|(s, _)| *s == Stage::Other)
            .map(|(_, v)| v)
            .unwrap_or(-1.0);
        // rtime 0.820 minus the ~0.0094 s of benchmarked calls in the fixture.
        assert!((other - (0.820 - 0.009_352)).abs() < 1e-6, "{other}");
    }

    #[test]
    fn each_mode_adds_exactly_its_own_ffmpeg_flags() {
        let base = || -> Vec<String> {
            [
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                "in.mkv",
                "out.ts",
            ]
            .map(String::from)
            .to_vec()
        };
        let with = |mode, file| apply_mode_args(base(), mode, file);
        assert_eq!(with(StageTimingMode::Off, true), base());
        assert_eq!(
            with(StageTimingMode::Basic, true),
            [
                "-stats_period",
                "0.1",
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                "in.mkv",
                "out.ts"
            ]
        );
        assert_eq!(with(StageTimingMode::Basic, false), base());
        assert_eq!(
            with(StageTimingMode::Detailed, false),
            [
                "-benchmark",
                "-benchmark_all",
                "-hide_banner",
                "-loglevel",
                "info",
                "-i",
                "in.mkv",
                "out.ts"
            ]
        );
        assert_eq!(
            with(StageTimingMode::Detailed, true)
                .iter()
                .filter(|a| a.as_str() == "info")
                .count(),
            1
        );
    }

    #[test]
    fn stage_labels_are_distinct_and_stable() {
        let all = [
            (Stage::Spawn, "spawn"),
            (Stage::FirstFrame, "first_frame"),
            (Stage::Total, "total"),
            (Stage::DecodeVideo, "decode_video"),
            (Stage::DecodeAudio, "decode_audio"),
            (Stage::EncodeVideo, "encode_video"),
            (Stage::EncodeAudio, "encode_audio"),
            (Stage::Flush, "flush"),
            (Stage::Other, "other"),
        ];
        for (s, l) in all {
            assert_eq!(s.label(), l);
        }
    }

    #[test]
    fn mode_parses_its_own_spelling_and_rejects_the_rest() {
        for m in [
            StageTimingMode::Off,
            StageTimingMode::Basic,
            StageTimingMode::Detailed,
        ] {
            assert_eq!(StageTimingMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(StageTimingMode::parse("verbose"), None);
        assert_eq!(StageTimingMode::default(), StageTimingMode::Basic);
    }

    #[test]
    fn device_and_codec_labels_are_bounded() {
        assert_eq!(device_kind(DeviceId::Cpu), "cpu");
        assert_eq!(device_kind(DeviceId::hw(HwAccel::Vaapi, 1)), "vaapi");
        assert_eq!(device_kind(DeviceId::hw(HwAccel::Nvenc, 0)), "nvenc");
        assert_eq!(codec_label(Some(VideoCodec::H265)), "hevc");
        assert_eq!(codec_label(None), "none");
    }

    #[test]
    fn recording_exports_one_histogram_series_per_stage() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let t = StageTiming {
            spawn_secs: 0.002,
            first_frame_secs: Some(0.3),
            total_secs: 0.82,
            progress: Some(parse_progress(PROGRESS)),
            bench: Some(parse_benchmark_all(BENCH)),
        };
        metrics::with_local_recorder(&recorder, || t.record("vaapi", "h264"));
        let rows = snapshotter.snapshot().into_vec();
        let mut stages: Vec<String> = rows
            .iter()
            .filter(|(k, ..)| k.key().name() == "pharos_transcode_stage_seconds")
            .filter_map(|(k, ..)| {
                k.key()
                    .labels()
                    .find(|l| l.key() == "stage")
                    .map(|l| l.value().to_string())
            })
            .collect();
        stages.sort();
        assert_eq!(
            stages,
            [
                "decode_audio",
                "decode_video",
                "encode_audio",
                "encode_video",
                "first_frame",
                "flush",
                "other",
                "spawn",
                "total"
            ]
        );
        assert!(rows
            .iter()
            .any(|(k, ..)| k.key().name() == "pharos_transcode_effective_fps"));
    }
}
