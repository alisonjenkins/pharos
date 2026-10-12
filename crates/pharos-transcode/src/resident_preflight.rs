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
//! source, parsed here. Concurrent first-play segments of one file share a
//! single run; the answer is memoised against the file's size and mtime, and an
//! unreadable file is only remembered for a short while.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;

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
            // Newer ffmpeg: `Display Matrix: rotation of 90.00 degrees`.
            let matrix = t
                .split("rotation of ")
                .nth(1)
                .and_then(|r| r.split_whitespace().next());
            // ffmpeg 4.x and earlier: a `rotate : 90` stream metadata tag.
            let tag = t
                .split_once(':')
                .filter(|(k, _)| k.trim() == "rotate")
                .map(|(_, v)| v.trim());
            let degrees: f64 = matrix.or(tag).and_then(|d| d.parse().ok()).unwrap_or(0.0);
            if degrees % 360.0 != 0.0 {
                return Some(ResidentBlock::Rotated);
            }
        }
    }
    None
}

/// Longest an inspection may take. Opening a header is milliseconds; this only
/// bounds a stalled network mount, after which the job takes the safe pipeline.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Memo size cap. Past it the oldest entry is evicted: it is a cache of a
/// cheap question, not state.
const MEMO_CAP: usize = 4096;

/// How long an `Unreadable` answer is believed. A stalled mount costs up to
/// `INSPECT_TIMEOUT` per attempt, and N parallel first-play segments would each
/// pay it; a good answer is kept until the file changes, a bad one only briefly.
const NEGATIVE_TTL: Duration = Duration::from_secs(45);

/// A file is the same file while its path, size and mtime are: a source
/// replaced in place must not keep the old verdict.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SourceKey {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

struct Entry {
    /// Shared by every concurrent caller for the same file: the first to arrive
    /// runs ffmpeg, the rest wait for its answer.
    cell: Arc<OnceCell<Option<ResidentBlock>>>,
    created: Instant,
    seq: u64,
}

#[derive(Default)]
struct Memo {
    entries: HashMap<SourceKey, Entry>,
    next_seq: u64,
}

/// Runs and memoises the per-source inspection.
pub struct SourceInspector {
    ffmpeg: PathBuf,
    memo: Mutex<Memo>,
    negative_ttl: Duration,
    timeout: Duration,
    prefix: Vec<std::ffi::OsString>,
    cap: usize,
}

impl SourceInspector {
    pub fn new(ffmpeg: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            memo: Mutex::new(Memo::default()),
            negative_ttl: NEGATIVE_TTL,
            timeout: INSPECT_TIMEOUT,
            prefix: ["-hide_banner", "-nostdin", "-i"].map(Into::into).to_vec(),
            cap: MEMO_CAP,
        }
    }

    /// Run `program prefix.. <path>` instead of ffmpeg, so a test can count and
    /// delay spawns without writing an executable.
    #[cfg(test)]
    fn with_command(
        mut self,
        program: impl Into<PathBuf>,
        prefix: Vec<std::ffi::OsString>,
    ) -> Self {
        self.ffmpeg = program.into();
        self.prefix = prefix;
        self
    }

    /// Override the unreadable-answer TTL and the ffmpeg timeout (tests).
    pub fn with_limits(mut self, negative_ttl: Duration, timeout: Duration) -> Self {
        self.negative_ttl = negative_ttl;
        self.timeout = timeout;
        self
    }

    /// `None`: fine for the resident pipeline. `Some(why)`: use the other one.
    ///
    /// Concurrent calls for one file share a single ffmpeg run; a clean or
    /// blocked answer lasts until the file's size or mtime changes; an
    /// unreadable one lasts `negative_ttl`.
    pub async fn inspect(&self, input: &Path) -> Option<ResidentBlock> {
        let key = match tokio::fs::metadata(input).await {
            Ok(m) => SourceKey {
                path: input.to_path_buf(),
                len: m.len(),
                modified: m.modified().ok(),
            },
            // Cannot even stat it: ffmpeg will not do better, and a key built
            // from nothing would alias every unreadable file.
            Err(_) => return Some(ResidentBlock::Unreadable),
        };
        let cell = self.cell_for(key);
        *cell
            .get_or_init(|| async {
                // No `Stream #` line means ffmpeg could not open the file at
                // all, so "no rotation seen" would be an absence of evidence,
                // not a clean bill.
                match self.run(input).await {
                    Some(banner) if banner.contains("Stream #") => parse_stream_info(&banner),
                    _ => Some(ResidentBlock::Unreadable),
                }
            })
            .await
    }

    fn cell_for(&self, key: SourceKey) -> Arc<OnceCell<Option<ResidentBlock>>> {
        let Ok(mut memo) = self.memo.lock() else {
            return Arc::new(OnceCell::new());
        };
        if let Some(e) = memo.entries.get(&key) {
            let stale = e.cell.get() == Some(&Some(ResidentBlock::Unreadable))
                && e.created.elapsed() >= self.negative_ttl;
            if !stale {
                return e.cell.clone();
            }
        }
        if memo.entries.len() >= self.cap && !memo.entries.contains_key(&key) {
            let oldest = memo
                .entries
                .iter()
                .min_by_key(|(_, e)| e.seq)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                memo.entries.remove(&k);
            }
        }
        let seq = memo.next_seq;
        memo.next_seq = seq.wrapping_add(1);
        let cell = Arc::new(OnceCell::new());
        memo.entries.insert(
            key,
            Entry {
                cell: cell.clone(),
                created: Instant::now(),
                seq,
            },
        );
        cell
    }

    /// ffmpeg's stderr for `-i input`, or `None` if it could not be run. ffmpeg
    /// exits 1 here (no output file); the banner is what matters.
    async fn run(&self, input: &Path) -> Option<String> {
        let child = tokio::process::Command::new(&self.ffmpeg)
            .args(&self.prefix)
            .arg(input)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
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

    /// ffmpeg 4.x prints rotation as a `rotate` metadata tag, not side data. The
    /// fixture is that banner shape (written from the old format, not captured:
    /// this tree ships ffmpeg 9).
    #[test]
    fn an_older_ffmpegs_rotate_tag_blocks_the_resident_pipeline() {
        const OLD: &str = include_str!("../tests/fixtures/ffmpeg_info/rotated_metadata_tag.txt");
        assert_eq!(parse_stream_info(OLD), Some(ResidentBlock::Rotated));
        let zero = OLD.replace("rotate          : 90", "rotate          : 0");
        assert_eq!(parse_stream_info(&zero), None);
        let upside_down = OLD.replace("rotate          : 90", "rotate          : 180");
        assert_eq!(
            parse_stream_info(&upside_down),
            Some(ResidentBlock::Rotated)
        );
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

    /// An inspector whose "ffmpeg" is `sh -c <script>`: it appends a line to a
    /// counter file per spawn, waits `delay`, then prints `banner`.
    fn counting(dir: &Path, delay: &str, banner: &str) -> (SourceInspector, PathBuf) {
        let counter = dir.join("spawns");
        let script = format!(
            "echo x >> '{}'; sleep {delay}; printf '%s\\n' '{banner}' >&2",
            counter.display()
        );
        let i = SourceInspector::new("unused")
            .with_command("/bin/sh", vec!["-c".into(), script.into(), "sh".into()]);
        (i, counter)
    }

    fn spawns(counter: &Path) -> usize {
        std::fs::read_to_string(counter).map_or(0, |s| s.lines().count())
    }

    const BANNER: &str = "  Stream #0:0: Video: h264, yuv420p(progressive), 1920x1080";

    fn source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_statted_reads_as_unreadable() {
        let i = SourceInspector::new("/nonexistent/ffmpeg");
        let p = Path::new("/m/does-not-exist.mkv");
        assert_eq!(i.inspect(p).await, Some(ResidentBlock::Unreadable));
    }

    #[tokio::test]
    async fn an_unrunnable_ffmpeg_reads_as_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let p = source(dir.path(), "a.mkv", b"x");
        let i = SourceInspector::new("/nonexistent/ffmpeg");
        assert_eq!(i.inspect(&p).await, Some(ResidentBlock::Unreadable));
    }

    /// N segments of one file arriving together are one `ffmpeg -i`, not N.
    #[tokio::test]
    async fn concurrent_callers_for_one_file_share_a_single_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let p = source(dir.path(), "a.mkv", b"x");
        let (i, counter) = counting(dir.path(), "0.3", BANNER);
        let i = Arc::new(i);
        let calls = (0..8).map(|_| {
            let i = i.clone();
            let p = p.clone();
            tokio::spawn(async move { i.inspect(&p).await })
        });
        for c in calls {
            assert_eq!(c.await.unwrap(), None);
        }
        assert_eq!(spawns(&counter), 1);
        // And the answer is remembered.
        assert_eq!(i.inspect(&p).await, None);
        assert_eq!(spawns(&counter), 1);
    }

    /// A file replaced in place (new size or mtime) is inspected again.
    #[tokio::test]
    async fn a_changed_file_is_inspected_again() {
        let dir = tempfile::tempdir().unwrap();
        let p = source(dir.path(), "a.mkv", b"x");
        let (i, counter) = counting(dir.path(), "0", BANNER);
        i.inspect(&p).await;
        i.inspect(&p).await;
        assert_eq!(spawns(&counter), 1);
        std::fs::write(&p, b"a different, longer file").unwrap();
        i.inspect(&p).await;
        assert_eq!(spawns(&counter), 2);
    }

    /// A stalled file costs the timeout once, not once per segment; the answer
    /// is believed only for the TTL.
    #[tokio::test]
    async fn an_unreadable_file_is_remembered_briefly() {
        let dir = tempfile::tempdir().unwrap();
        let p = source(dir.path(), "a.mkv", b"x");
        let (i, counter) = counting(dir.path(), "5", BANNER);
        let i = i.with_limits(Duration::from_millis(300), Duration::from_millis(100));
        let t = Instant::now();
        assert_eq!(i.inspect(&p).await, Some(ResidentBlock::Unreadable));
        assert!(t.elapsed() < Duration::from_secs(2), "timeout not applied");
        let t = Instant::now();
        assert_eq!(i.inspect(&p).await, Some(ResidentBlock::Unreadable));
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "second call paid again"
        );
        assert_eq!(spawns(&counter), 1);
        tokio::time::sleep(Duration::from_millis(350)).await;
        i.inspect(&p).await;
        assert_eq!(spawns(&counter), 2, "TTL expired, ask again");
    }

    #[tokio::test]
    async fn the_memo_evicts_the_oldest_entry_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let (mut i, _) = counting(dir.path(), "0", BANNER);
        i.cap = 4;
        for n in 0..9 {
            let p = source(dir.path(), &format!("{n}.mkv"), b"x");
            i.inspect(&p).await;
        }
        let memo = i.memo.lock().unwrap();
        assert_eq!(memo.entries.len(), 4);
        assert!(
            !memo.entries.keys().any(|k| k.path.ends_with("0.mkv")),
            "oldest entry should be gone"
        );
    }

    #[test]
    fn labels_are_distinct_and_stable() {
        assert_eq!(ResidentBlock::Rotated.label(), "rotated");
        assert_eq!(ResidentBlock::Interlaced.label(), "interlaced");
        assert_eq!(ResidentBlock::Unreadable.label(), "inspect_failed");
    }
}
