//! Full decode check: decode every frame of a file and count what the decoders
//! complain about.
//!
//! The library integrity sweep only demuxes ([`crate::integrity`]). That finds
//! holes, missing chunks and truncation, but not a flipped bit inside a frame:
//! the packet is the right size in the right place, and only decoding it shows
//! the picture is broken. Decoding a whole film costs minutes of CPU, so this
//! never runs in the background; an admin asks for it, one file at a time.
//!
//! It spawns the `ffmpeg` binary (`ffmpeg -v error … -f null -`) rather than
//! decoding in process: the cost is all decode time, a decoder fault cannot
//! touch the server (V6), and it builds with either ffmpeg backend.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Decoder threads per check. Two keep a check from taking over the CPU that
/// transcodes for viewers need; it is on demand and nobody is waiting on it.
pub const DECODE_THREADS: &str = "2";

/// What decoding a whole file found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeepReport {
    /// `-v error` lines ffmpeg wrote while decoding.
    pub errors: u64,
    /// The first of them, verbatim.
    pub first_error: Option<String>,
    /// Roughly where the first error is, in ms: the last progress report
    /// before it. ffmpeg's error lines carry no timestamp of their own.
    pub first_error_ms: Option<u64>,
    /// How far decoding got, in ms.
    pub decoded_ms: Option<u64>,
}

impl DeepReport {
    pub fn is_damaged(&self) -> bool {
        self.errors > 0
    }
}

/// Why a check produced no verdict.
#[derive(Debug, thiserror::Error)]
pub enum DeepError {
    #[error("could not start {bin}: {source}")]
    Spawn { bin: String, source: std::io::Error },
    #[error("decoding took over {secs}s and was stopped")]
    TimedOut { secs: u64 },
    /// ffmpeg exited non-zero without a single decode error, so it never got
    /// to decoding (the file would not open, or no stream could be decoded).
    #[error("ffmpeg exited with {status} before decoding: {stderr}")]
    Failed { status: String, stderr: String },
}

/// Decode every audio and video stream of `path` with `ffmpeg`, bounded by
/// `timeout`. The child is killed if the future is dropped or times out.
pub async fn decode_check(
    ffmpeg: &str,
    path: &Path,
    timeout: Duration,
) -> Result<DeepReport, DeepError> {
    let mut child = tokio::process::Command::new(ffmpeg)
        .args([
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-v",
            "error",
            "-progress",
            "pipe:1",
            "-threads",
            DECODE_THREADS,
            "-i",
        ])
        .arg(path)
        .args(["-map", "0:v?", "-map", "0:a?", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| DeepError::Spawn {
            bin: ffmpeg.to_string(),
            source,
        })?;

    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(DeepError::Failed {
            status: "no pipes".into(),
            stderr: String::new(),
        });
    };
    let run = async {
        let mut progress = BufReader::new(stdout).lines();
        let mut errors = BufReader::new(stderr).lines();
        let mut report = DeepReport::default();
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            tokio::select! {
                line = progress.next_line(), if !out_done => match line {
                    Ok(Some(l)) => {
                        if let Some(ms) = progress_ms(&l) {
                            report.decoded_ms = Some(ms);
                        }
                    }
                    _ => out_done = true,
                },
                line = errors.next_line(), if !err_done => match line {
                    Ok(Some(l)) if !l.trim().is_empty() => {
                        report.errors = report.errors.saturating_add(1);
                        if report.first_error.is_none() {
                            report.first_error = Some(l);
                            report.first_error_ms = report.decoded_ms;
                        }
                    }
                    Ok(Some(_)) => {}
                    _ => err_done = true,
                },
            }
        }
        let status = child.wait().await;
        (report, status)
    };
    let (report, status) =
        tokio::time::timeout(timeout, run)
            .await
            .map_err(|_| DeepError::TimedOut {
                secs: timeout.as_secs(),
            })?;
    match status {
        Ok(s) if !s.success() && report.errors == 0 => Err(DeepError::Failed {
            status: s.to_string(),
            stderr: String::new(),
        }),
        Ok(s) if !s.success() && report.decoded_ms.is_none() => Err(DeepError::Failed {
            status: s.to_string(),
            stderr: report.first_error.unwrap_or_default(),
        }),
        Ok(_) => Ok(report),
        Err(e) => Err(DeepError::Failed {
            status: format!("wait failed: {e}"),
            stderr: String::new(),
        }),
    }
}

/// `out_time_us=12345678` (or the misnamed `out_time_ms`, also microseconds)
/// as milliseconds. `N/A` before the first frame is not a position.
fn progress_ms(line: &str) -> Option<u64> {
    let v = line
        .strip_prefix("out_time_us=")
        .or_else(|| line.strip_prefix("out_time_ms="))?;
    v.trim().parse::<u64>().ok().map(|us| us / 1000)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn have_ffmpeg() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Four seconds of MPEG-2 video in MPEG-TS: an encoder every ffmpeg build
    /// has, in a container that keeps going past damage instead of giving up.
    fn make_clip(path: &Path) {
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25",
                "-t",
                "4",
                "-c:v",
                "mpeg2video",
                "-q:v",
                "2",
                "-f",
                "mpegts",
            ])
            .arg(path)
            .status()
            .unwrap()
            .success();
        assert!(ok, "fixture encode failed");
    }

    #[test]
    fn progress_lines_read_as_milliseconds() {
        assert_eq!(progress_ms("out_time_us=2500000"), Some(2_500));
        assert_eq!(progress_ms("out_time_ms=2500000"), Some(2_500));
        assert_eq!(progress_ms("out_time_us=N/A"), None);
        assert_eq!(progress_ms("frame=10"), None);
    }

    #[tokio::test]
    async fn a_healthy_file_decodes_without_errors() {
        if !have_ffmpeg() {
            eprintln!("skipping: no ffmpeg on PATH");
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let clip = dir.path().join("ok.ts");
        make_clip(&clip);
        let r = decode_check("ffmpeg", &clip, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(r.errors, 0, "{r:?}");
        assert!(r.decoded_ms.unwrap_or(0) >= 3_000, "{r:?}");
    }

    /// The case the demux check passes: the TS packets are all there, but the
    /// picture data inside them is garbage, so only the decoder notices.
    #[tokio::test]
    async fn garbage_inside_the_frames_is_found() {
        if !have_ffmpeg() {
            eprintln!("skipping: no ffmpeg on PATH");
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let clip = dir.path().join("bad.ts");
        make_clip(&clip);
        let mut bytes = std::fs::read(&clip).unwrap();
        // Scribble over payload bytes of TS packets in the middle, leaving
        // each 4-byte packet header (sync byte 0x47) intact.
        let mid = bytes.len() / 2;
        let start = mid - mid % 188;
        for pkt in bytes[start..].chunks_mut(188).take(200) {
            for b in pkt.iter_mut().skip(16) {
                *b ^= 0x5a;
            }
        }
        std::fs::write(&clip, bytes).unwrap();
        let r = decode_check("ffmpeg", &clip, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(r.errors > 0, "{r:?}");
        assert!(r.first_error.is_some());
    }

    #[tokio::test]
    async fn a_missing_binary_is_an_error_not_a_verdict() {
        let err = decode_check(
            "/nonexistent/ffmpeg",
            Path::new("/x.mkv"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DeepError::Spawn { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_file_that_will_not_open_is_an_error_not_a_verdict() {
        if !have_ffmpeg() {
            eprintln!("skipping: no ffmpeg on PATH");
            return;
        }
        let err = decode_check(
            "ffmpeg",
            Path::new("/nonexistent/x.mkv"),
            Duration::from_secs(30),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DeepError::Failed { .. }), "{err}");
    }
}
