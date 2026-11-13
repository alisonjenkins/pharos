//! One song's alignment, as `server.py`'s `_align` runs it once the request
//! is parsed: the offset estimate, then each line in turn, one song at a time.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::audio::DecodeError;
use crate::line::LineSpans;
use crate::offset::{estimate, shift};
use crate::plan::{stamps, window, Line, Stamp};
use crate::research::{self, Open, Outcome, RunCounts, RunReport};
use crate::text::words;

/// Sent with every answer. The server keeps only answers at or above its
/// minimum, so an aligner deployed after the server cannot pin old times.
/// 2: a line's window opens `LEAD_MS` before its stamp (B1165).
/// 3: the Rust service (spec 039).
/// 5: each line carries its `score`. Python's aligner went to 4 with the
///    same field, so the shadow comparison can tell the two apart.
pub const ALIGNER_VERSION: u32 = 5;

/// Why the model gave a window no spans.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// This window only, as torchaudio's RuntimeError for text that outruns
    /// its window: the line goes without word times and the song goes on.
    #[error("{0}")]
    Line(String),
    /// The model itself is broken; the song answers 500.
    #[error("{0}")]
    Fatal(String),
    /// The device under the model failed (lost, out of memory); a model
    /// with a fallback runs the window elsewhere, otherwise it is fatal.
    #[error("{0}")]
    Device(String),
}

/// Where a song's forward passes ran and why, for its span and log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceUsed {
    pub device: &'static str,
    pub reason: String,
}

/// A GPU memory pool's bytes at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuPool {
    pub in_use: u64,
    pub reserved: u64,
}

impl GpuPool {
    /// Each count's larger value of two readings: a high-water mark.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self {
            in_use: self.in_use.max(other.in_use),
            reserved: self.reserved.max(other.reserved),
        }
    }
}

/// The GPU pool at the end of a song (B1352, V1323): `peak`, the most the
/// song held as read after each window and before the cleanup, and `after`
/// what it keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuMemory {
    pub peak: GpuPool,
    pub after: GpuPool,
}

/// Waveform to word spans: the acoustic model plus CTC, `model.py`'s `Aligner.align`.
pub trait Model: Send {
    /// `(start_ms, end_ms)` per token within the window, and the line's score.
    fn align(
        &mut self,
        pcm: &[f32],
        tokens: &[String],
        lead_star: bool,
    ) -> Result<LineSpans, ModelError>;

    /// One window's log-probabilities, `[frames x tokens::CLASSES]` with
    /// the star column, for a pass that runs its own CTC over several
    /// lines (B1402); `None` from a model that cannot give them.
    fn emission(&mut self, _pcm: &[f32]) -> Result<Option<Vec<f32>>, ModelError> {
        Ok(None)
    }

    /// Called before each song's first window.
    fn begin_song(&mut self) {}

    /// Called after each song's last window, whatever its outcome: a GPU
    /// model hands back its pool's empty pages and says what it held.
    fn end_song(&mut self) -> Option<GpuMemory> {
        None
    }

    /// Where the song so far ran.
    fn device(&self) -> DeviceUsed {
        DeviceUsed {
            device: "cpu",
            reason: "forced_cpu".into(),
        }
    }

    /// [`Model::align`] over independent windows, one answer each in order;
    /// a model may run them through shared forward passes.
    fn align_many(
        &mut self,
        windows: &[(&[f32], &[String])],
        lead_star: bool,
    ) -> Vec<Result<LineSpans, ModelError>> {
        windows
            .iter()
            .map(|(pcm, tokens)| self.align(pcm, tokens, lead_star))
            .collect()
    }
}

/// Loads the model on first use and after an idle unload.
pub type Loader = Box<dyn Fn() -> Result<Box<dyn Model>, String> + Send + Sync>;

/// One window of a file to samples; `audio::decode` outside tests.
pub type Decoder = Box<dyn Fn(&Path, i64, i64) -> Result<Vec<f32>, DecodeError> + Send + Sync>;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LineWords {
    pub words: Vec<Stamp>,
    /// How well the words matched the singing, 0 to 1 to three places as
    /// server.py sends it; `null` for a line with no words or no times.
    pub score: Option<f64>,
}

impl LineWords {
    /// A line with no words, or none the model could time.
    pub fn empty() -> Self {
        Self {
            words: Vec::new(),
            score: None,
        }
    }
}

/// `round(score, 3)`, as server.py rounds it. Python rounds the nearest
/// double half to even, this rounds it away from zero; they can differ by
/// 0.001 only on an exact tie, within the shadow's tolerance. A score that
/// is not finite answers `null`, and one outside 0..=1 is clamped to it, as
/// the server would read it anyway.
pub fn round_score(score: f64) -> Option<f64> {
    score
        .is_finite()
        .then(|| (score.clamp(0.0, 1.0) * 1000.0).round() / 1000.0)
}

/// What a song can show before it is done (ADR-0121), in the order it
/// happens: its offset once found, then each line as it is timed. A line's
/// words never change after it is reported; only the next line reads where
/// it ended.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress<'a> {
    /// The song's lyric stamps are this far off the singing; every line moves by it.
    Offset {
        offset_ms: i64,
        aligner_version: u32,
        lines: usize,
    },
    /// Line `index` is timed, as it will be in the final answer.
    Line {
        index: usize,
        #[serde(flatten)]
        words: &'a LineWords,
    },
}

/// The `/align` answer, fields in the Python service's order.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Answer {
    pub lines: Vec<LineWords>,
    pub offset_ms: i64,
    pub aligner_version: u32,
}

/// Why a song failed as a whole.
#[derive(Debug, thiserror::Error)]
pub enum SongError {
    /// A line window ffmpeg could not decode: 422.
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(DecodeError),
    /// Anything else, ffmpeg missing or timing out on a line included: 500.
    #[error("{0}")]
    Internal(String),
}

/// Wall time per phase of one song, summed over its windows, for the log line
/// and the request span.
#[derive(Debug, Default, Clone)]
pub struct Phases {
    decode_ms: u64,
    model_ms: u64,
}

impl Phases {
    fn time<T>(slot: &mut u64, f: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let out = f();
        *slot += u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        out
    }

    pub fn decode_ms(&self) -> u64 {
        self.decode_ms
    }

    pub fn model_ms(&self) -> u64 {
        self.model_ms
    }

    /// `decode_ms=… model_ms=…`, as `Phases.summary` prints it.
    pub fn summary(&self) -> String {
        format!("decode_ms={} model_ms={}", self.decode_ms, self.model_ms)
    }
}

/// A finished song: the answer plus what the log line and span report.
#[derive(Debug)]
pub struct Aligned {
    pub answer: Answer,
    pub words_aligned: usize,
    pub probes: Vec<Option<i64>>,
    pub reason: String,
    pub phases: Phases,
    /// Weak runs found, by what became of them (B1402).
    pub runs: RunCounts,
}

struct Slot {
    model: Option<Box<dyn Model>>,
    last_used: Instant,
}

/// The loaded model behind the one-song-at-a-time lock.
pub struct Engine {
    slot: Mutex<Slot>,
    loader: Loader,
    decoder: Decoder,
    idle_unload: Option<Duration>,
}

impl Engine {
    pub fn new(loader: Loader, decoder: Decoder, idle_unload: Option<Duration>) -> Self {
        Self {
            slot: Mutex::new(Slot {
                model: None,
                last_used: Instant::now(),
            }),
            loader,
            decoder,
            idle_unload,
        }
    }

    /// Drops the model when it has sat unused for the idle timeout; true if
    /// it did. A song in progress holds the lock, so it is never unloaded mid-song.
    pub fn unload_if_idle(&self) -> bool {
        let Some(idle) = self.idle_unload else {
            return false;
        };
        // A song that panicked holding the lock leaves it poisoned; its model
        // must still be freed, as `align_song_on` still uses it.
        let mut slot = match self.slot.try_lock() {
            Ok(slot) => slot,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        if slot.model.is_none() || slot.last_used.elapsed() < idle {
            return false;
        }
        slot.model = None;
        tracing::info!(
            idle_s = slot.last_used.elapsed().as_secs(),
            "model unloaded after idle"
        );
        true
    }

    /// Aligns every line of the song at `full_path` (already resolved and
    /// checked to exist); `rel_path` names it in logs.
    pub fn align_song(
        &self,
        rel_path: &str,
        full_path: &Path,
        lines: Vec<Line>,
    ) -> Result<Aligned, SongError> {
        self.align_song_on(rel_path, full_path, lines, &mut |_| {})
            .0
    }

    /// [`Engine::align_song`], plus where it ran: known on failure too, so
    /// the failure's log line names the device as the success's does.
    /// `on` hears the song's [`Progress`] as it happens, on this thread.
    pub fn align_song_on(
        &self,
        rel_path: &str,
        full_path: &Path,
        lines: Vec<Line>,
        on: &mut dyn FnMut(Progress<'_>),
    ) -> (
        Result<Aligned, SongError>,
        Option<DeviceUsed>,
        Option<GpuMemory>,
    ) {
        let mut slot = tracing::info_span!("aligner.wait").in_scope(|| {
            // A song that panicked mid-way leaves nothing half-written that the
            // next one reads: the model is reloaded below if it was dropped.
            self.slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if slot.model.is_none() {
            let started = Instant::now();
            let model = match (self.loader)() {
                Ok(m) => m,
                Err(e) => {
                    let e = SongError::Internal(format!("model load failed: {e}"));
                    return (Err(e), None, None);
                }
            };
            tracing::info!(
                load_ms = started.elapsed().as_millis() as u64,
                "model loaded"
            );
            slot.model = Some(model);
        }
        let result = match slot.model.as_deref_mut() {
            Some(model) => {
                model.begin_song();
                let r = run_song(model, &self.decoder, rel_path, full_path, lines, on);
                let used = model.device();
                (r, Some(used), model.end_song())
            }
            None => (
                Err(SongError::Internal("model missing after load".into())),
                None,
                None,
            ),
        };
        slot.last_used = Instant::now();
        result
    }
}

/// `[-33, None, 120]`, as Python prints a probe list, for the offset log line.
fn probe_list(probes: &[Option<i64>]) -> String {
    let items: Vec<String> = probes
        .iter()
        .map(|p| p.map_or_else(|| "None".to_owned(), |v| v.to_string()))
        .collect();
    format!("[{}]", items.join(", "))
}

fn run_song(
    model: &mut dyn Model,
    decoder: &Decoder,
    rel_path: &str,
    full_path: &Path,
    lines: Vec<Line>,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Aligned, SongError> {
    let mut phases = Phases::default();
    let path: PathBuf = full_path.to_owned();

    // Probes are optional: a window ffmpeg cannot decode costs the song its
    // offset, not its word times. After one timeout the rest are skipped, so
    // seven of them cannot outlast the server's whole-song budget.
    let mut probe_timed_out = false;
    let probe_phases = std::cell::RefCell::new(&mut phases);
    let decode_probe = |start: i64, end: i64| -> Result<Vec<f32>, SongError> {
        if probe_timed_out {
            return Ok(Vec::new());
        }
        let span = tracing::info_span!("aligner.decode", stage = "probe", window_ms = end - start);
        let got = span.in_scope(|| {
            let mut p = probe_phases.borrow_mut();
            Phases::time(&mut p.decode_ms, || decoder(&path, start, end))
        });
        match got {
            Ok(pcm) => Ok(pcm),
            Err(e @ DecodeError::TimedOut { .. }) => {
                probe_timed_out = true;
                tracing::warn!(path = %rel_path, error = %e, "offset probe decode timed out; skipping the rest");
                Ok(Vec::new())
            }
            Err(e @ DecodeError::Failed { .. }) => {
                tracing::warn!(path = %rel_path, error = %e, "offset probe decode failed");
                Ok(Vec::new())
            }
            Err(e) => Err(SongError::Internal(e.to_string())),
        }
    };
    let align_wide = |windows: &[(&[f32], &[String])]| -> Result<Vec<Vec<(u64, u64)>>, SongError> {
        let words: usize = windows.iter().map(|(_, t)| t.len()).sum();
        let span = tracing::info_span!(
            "aligner.model",
            stage = "probe",
            windows = windows.len(),
            words
        );
        let got = span.in_scope(|| {
            let mut p = probe_phases.borrow_mut();
            Phases::time(&mut p.model_ms, || model.align_many(windows, true))
        });
        got.into_iter()
            .map(|r| match r {
                // The offset needs only the times.
                Ok(spans) => Ok(spans.times),
                Err(ModelError::Line(e)) => {
                    tracing::warn!(path = %rel_path, error = %e, "offset probe failed");
                    Ok(Vec::new())
                }
                Err(e @ (ModelError::Fatal(_) | ModelError::Device(_))) => {
                    Err(SongError::Internal(e.to_string()))
                }
            })
            .collect()
    };
    let found = estimate(&lines, decode_probe, align_wide)?;
    tracing::info!(
        path = %rel_path,
        offset_ms = found.offset_ms,
        probes = %probe_list(&found.probes),
        reason = %found.reason,
        "offset estimate"
    );

    let lines = shift(lines, found.offset_ms);
    on(Progress::Offset {
        offset_ms: found.offset_ms,
        aligner_version: ALIGNER_VERSION,
        lines: lines.len(),
    });
    let mut out = Vec::with_capacity(lines.len());
    let mut words_aligned = 0;
    let mut prev_end_ms = 0;
    let mut runs = RunCounts::default();
    let mut open: Option<Open> = None;
    let mut anchor_end_ms = 0;
    for (index, line) in lines.iter().enumerate() {
        let w_list = words(&line.text);
        if w_list.is_empty() {
            let done = LineWords::empty();
            on(Progress::Line {
                index,
                words: &done,
            });
            out.push(done);
            continue;
        }
        let (w_start, w_end) = window(line, w_list.len(), prev_end_ms);
        let span = tracing::info_span!(
            "aligner.decode",
            stage = "line",
            line = index,
            window_ms = w_end - w_start
        );
        let pcm = span
            .in_scope(|| Phases::time(&mut phases.decode_ms, || decoder(&path, w_start, w_end)))
            .map_err(|e| match e {
                DecodeError::Failed { .. } => SongError::Ffmpeg(e),
                other => SongError::Internal(other.to_string()),
            })?;
        let tokens: Vec<String> = w_list.iter().map(|w| w.token.clone()).collect();
        let span = tracing::info_span!(
            "aligner.model",
            stage = "line",
            line = index,
            words = tokens.len()
        );
        let spans = match span
            .in_scope(|| Phases::time(&mut phases.model_ms, || model.align(&pcm, &tokens, false)))
        {
            Ok(spans) => spans,
            // A window too short for its text: only that line goes without word times.
            Err(ModelError::Line(e)) => {
                tracing::warn!(path = %rel_path, line = index, error = %e, "alignment failed");
                open.get_or_insert(Open {
                    first: index,
                    start_ms: anchor_end_ms,
                    scores: Vec::new(),
                })
                .scores
                .push(None);
                let done = LineWords::empty();
                on(Progress::Line {
                    index,
                    words: &done,
                });
                out.push(done);
                continue;
            }
            Err(e @ (ModelError::Fatal(_) | ModelError::Device(_))) => {
                return Err(SongError::Internal(e.to_string()))
            }
        };
        let line_stamps = stamps(&line.text, &w_list, &spans.times, w_start);
        if let Some(last) = line_stamps.last() {
            words_aligned += w_list.len();
            prev_end_ms = last.start_ms;
        }
        if research::is_weak(!line_stamps.is_empty(), spans.score) {
            open.get_or_insert(Open {
                first: index,
                start_ms: anchor_end_ms,
                scores: Vec::new(),
            })
            .scores
            .push(spans.score);
        } else {
            if let (Some(run), Some(first)) = (open.take(), line_stamps.first()) {
                close_run(rel_path, run, index, first.start_ms, &mut runs);
            }
            let last_end = spans.times.last().map_or(0, |&(_, end)| end);
            anchor_end_ms = w_start + i64::try_from(last_end).unwrap_or(i64::MAX - w_start);
        }
        let done = LineWords {
            words: line_stamps,
            score: spans.score.and_then(round_score),
        };
        on(Progress::Line {
            index,
            words: &done,
        });
        out.push(done);
    }
    if let Some(run) = open.take() {
        let end_ms = lines.last().map_or(run.start_ms, |l| l.end_ms);
        close_run(rel_path, run, lines.len(), end_ms, &mut runs);
    }
    Ok(Aligned {
        answer: Answer {
            lines: out,
            offset_ms: found.offset_ms,
            aligner_version: ALIGNER_VERSION,
        },
        words_aligned,
        probes: found.probes,
        reason: found.reason,
        phases,
        runs,
    })
}

/// Ends the weak run `run` before line `next` (or the song's end), whose
/// audio stops at `end_ms`, and reports it.
fn close_run(rel_path: &str, run: Open, next: usize, end_ms: i64, runs: &mut RunCounts) {
    let report = RunReport {
        lines: run.first..next,
        window_ms: (run.start_ms, end_ms),
        before: research::mean_score(&run.scores),
        after: None,
        outcome: Outcome::Skipped,
        reason: "re-search off".into(),
    };
    research::record(rel_path, &report, runs);
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pool_high_water_mark_keeps_each_counts_largest_reading() {
        use super::GpuPool;
        let p = |in_use, reserved| GpuPool { in_use, reserved };
        // A later reading after cubecl freed pages does not lower the peak.
        assert_eq!(p(4, 9).max(p(2, 6)), p(4, 9));
        assert_eq!(p(4, 6).max(p(2, 9)), p(4, 9));
        assert_eq!(p(1, 5).max(p(1, 5)), p(1, 5));
    }

    #[test]
    fn a_score_is_rounded_to_three_places_within_0_to_1_and_null_when_not_finite() {
        use super::round_score;
        assert_eq!(round_score(0.81264), Some(0.813));
        assert_eq!(round_score(0.0), Some(0.0));
        assert_eq!(round_score(1.0), Some(1.0));
        assert_eq!(round_score(1.2), Some(1.0));
        assert_eq!(round_score(-0.3), Some(0.0));
        assert_eq!(round_score(f64::NAN), None);
        assert_eq!(round_score(f64::INFINITY), None);
        assert_eq!(round_score(f64::NEG_INFINITY), None);
    }

    #[test]
    fn probe_list_prints_as_python_does() {
        assert_eq!(
            super::probe_list(&[Some(-33), None, Some(120)]),
            "[-33, None, 120]"
        );
        assert_eq!(super::probe_list(&[]), "[]");
    }
}
