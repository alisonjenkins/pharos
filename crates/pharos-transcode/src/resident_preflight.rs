//! Source checks the GPU-resident VAAPI pipeline needs and the probe cannot
//! answer.
//!
//! The scanner's probe records the pixel format, transfer and size, but not
//! the display-matrix rotation or the field order. The CPU-filter pipeline
//! handles both implicitly (ffmpeg autorotates software frames; an interlaced
//! source passes through). The resident pipeline does not: measured on
//! ffmpeg 9.0.1 with a 90 degree display matrix, it exits 0 and writes
//! 1920x1080 where the hybrid path writes 1080x1920, so a rotated source would
//! be served sideways without any error to retry on.
//!
//! So a job headed for the resident pipeline first gets one `ffmpeg -i` of its
//! source, parsed here. The answer is memoised per path; a failed inspection is
//! not, because it says nothing about the file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

/// Why a source must take the CPU-filter pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentBlock {
    /// A non-zero display-matrix rotation.
    Rotated,
    /// A field order other than progressive.
    Interlaced,
    /// The inspection itself failed or timed out.
    Unreadable,
}

impl ResidentBlock {
    /// Bounded label for `pharos_transcode_vaapi_resident_fallback_total`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Rotated => "rotated",
            Self::Interlaced => "interlaced",
            Self::Unreadable => "inspect_failed",
        }
    }
}

/// Read ffmpeg's `-i` banner and say whether the first video stream is rotated
/// or interlaced. Pure; the banner is passed in.
///
/// Interlaced shows as `yuv420p(tv, top first)` (also `bottom first`, `top
/// coded first`, `bottom coded first`) on the stream line; rotation as
/// `Display Matrix: rotation of 90.00 degrees` in the `Side data:` lines that
/// follow it.
pub fn parse_stream_info(banner: &str) -> Option<ResidentBlock> {
    let mut in_video = false;
    for line in banner.lines() {
        let t = line.trim_start();
        if t.starts_with("Stream #") {
            if in_video {
                break;
            }
            in_video = t.contains(": Video:");
            if in_video
                && [
                    "top first",
                    "bottom first",
                    "top coded first",
                    "bottom coded first",
                ]
                .iter()
                .any(|f| t.contains(f))
            {
                return Some(ResidentBlock::Interlaced);
            }
        } else if in_video {
            if let Some(rest) = t.split("rotation of ").nth(1) {
                let degrees: f64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|d| d.parse().ok())
                    .unwrap_or(0.0);
                if degrees != 0.0 {
                    return Some(ResidentBlock::Rotated);
                }
            }
        }
    }
    None
}

/// Longest an inspection may take. Opening a header is milliseconds; this only
/// bounds a stalled network mount, after which the job takes the safe pipeline.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Memo size cap. Past it the memo is cleared rather than evicted entry by
/// entry: it is a cache of a cheap question, not state.
const MEMO_CAP: usize = 4096;

/// Runs and memoises the per-source inspection.
pub struct SourceInspector {
    ffmpeg: PathBuf,
    memo: Mutex<HashMap<PathBuf, Option<ResidentBlock>>>,
}

impl SourceInspector {
    pub fn new(ffmpeg: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            memo: Mutex::new(HashMap::new()),
        }
    }

    /// `None`: fine for the resident pipeline. `Some(why)`: use the other one.
    pub async fn inspect(&self, input: &Path) -> Option<ResidentBlock> {
        if let Some(hit) = self.memo.lock().ok().and_then(|m| m.get(input).copied()) {
            return hit;
        }
        // No `Stream #` line means ffmpeg could not open the file at all, so
        // "no rotation seen" would be an absence of evidence, not a clean bill.
        let verdict = match self.run(input).await {
            Some(banner) if banner.contains("Stream #") => parse_stream_info(&banner),
            _ => return Some(ResidentBlock::Unreadable),
        };
        if let Ok(mut m) = self.memo.lock() {
            if m.len() >= MEMO_CAP {
                m.clear();
            }
            m.insert(input.to_path_buf(), verdict);
        }
        verdict
    }

    /// ffmpeg's stderr for `-i input`, or `None` if it could not be run. ffmpeg
    /// exits 1 here (no output file); the banner is what matters.
    async fn run(&self, input: &Path) -> Option<String> {
        let child = tokio::process::Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-nostdin", "-i"])
            .arg(input)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let out = tokio::time::timeout(INSPECT_TIMEOUT, child.wait_with_output())
            .await
            .ok()?
            .ok()?;
        Some(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const PLAIN: &str = include_str!("../tests/fixtures/ffmpeg_info/progressive.txt");
    const ROTATED: &str = include_str!("../tests/fixtures/ffmpeg_info/rotated_90.txt");
    const INTERLACED: &str = include_str!("../tests/fixtures/ffmpeg_info/interlaced_tff.txt");

    #[test]
    fn a_progressive_unrotated_source_is_fine() {
        assert_eq!(parse_stream_info(PLAIN), None);
    }

    #[test]
    fn a_display_matrix_rotation_blocks_the_resident_pipeline() {
        assert_eq!(parse_stream_info(ROTATED), Some(ResidentBlock::Rotated));
    }

    #[test]
    fn a_field_order_blocks_the_resident_pipeline() {
        assert_eq!(
            parse_stream_info(INTERLACED),
            Some(ResidentBlock::Interlaced)
        );
    }

    #[test]
    fn a_zero_degree_matrix_is_not_a_rotation() {
        let banner = "  Stream #0:0: Video: h264, yuv420p(progressive), 1920x1080\n    Side data:\n      Display Matrix: rotation of 0.00 degrees\n";
        assert_eq!(parse_stream_info(banner), None);
    }

    #[test]
    fn only_the_first_video_stream_counts() {
        // A cover-art stream after the real video must not be read as it.
        let banner = "  Stream #0:0: Video: h264, yuv420p(progressive), 1920x1080\n  Stream #0:1: Video: mjpeg, yuvj420p(pc, bottom first), 600x600\n";
        assert_eq!(parse_stream_info(banner), None);
    }

    #[tokio::test]
    async fn an_unrunnable_ffmpeg_reads_as_unreadable_and_is_not_memoised() {
        let i = SourceInspector::new("/nonexistent/ffmpeg");
        let p = Path::new("/m/x.mkv");
        assert_eq!(i.inspect(p).await, Some(ResidentBlock::Unreadable));
        assert!(i.memo.lock().unwrap().is_empty());
    }

    #[test]
    fn labels_are_distinct_and_stable() {
        assert_eq!(ResidentBlock::Rotated.label(), "rotated");
        assert_eq!(ResidentBlock::Interlaced.label(), "interlaced");
        assert_eq!(ResidentBlock::Unreadable.label(), "inspect_failed");
    }
}
