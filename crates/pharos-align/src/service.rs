//! One song's alignment, as `server.py`'s `_align` runs it once the request
//! is parsed: the offset estimate, then each line in turn, one song at a time.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::anchor::{match_lines, Options};
use crate::audio::DecodeError;
use crate::ctc::Emission;
use crate::line::LineSpans;
use crate::offset::{estimate, probe_windows, shift};
use crate::placement::{Fallback, Method, Placement};
use crate::plan::{stamps, window, Line, Stamp};
use crate::refine::{place, Song};
use crate::stitch::stitched;
use crate::text::words;
use crate::tokens::CLASSES;
use crate::transcript;

/// Sent with every answer. The server keeps only answers at or above its
/// minimum, so an aligner deployed after the server cannot pin old times.
/// 2: a line's window opens `LEAD_MS` before its stamp (B1165).
/// 3: the Rust service (spec 039).
/// 5: each line carries its `score`. Python's aligner went to 4 with the
///    same field, so the shadow comparison can tell the two apart.
/// 6: lines are placed by what the recogniser heard (spec 042, ADR-0124);
///    each line with words says how, in `placement`.
pub const ALIGNER_VERSION: u32 = 6;

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

/// Why a stage after today's pass failed: the reason the song keeps
/// today's answer, and the error that caused it.
#[derive(Debug, Clone, PartialEq)]
pub struct StageError {
    pub fallback: Fallback,
    pub reason: String,
}

/// The stages after today's per-line pass (spec 042, ADR-0124/0125). Each
/// loads its model, runs it and drops it before returning, so the letter
/// model is the only one still on the GPU when the next stage loads.
pub trait Stages: Send + Sync {
    /// The whole song's vocals at `audio::RATE` mono: decoded, separated
    /// (the mix itself without a GPU), resampled.
    fn vocals(&self, path: &Path) -> Result<Vec<f32>, StageError>;

    /// The words the recogniser heard in `vocals`.
    fn recognise(&self, vocals: &[f32]) -> Result<Vec<transcript::Word>, StageError>;
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LineWords {
    pub words: Vec<Stamp>,
    /// How well the words matched the singing, 0 to 1 to three places as
    /// server.py sends it; `null` for a line with no words or no times.
    pub score: Option<f64>,
    /// How the line was placed, in the final answer and a corrected line
    /// only: a line streamed from today's pass leaves it out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<Method>,
}

impl LineWords {
    /// A line with no words, or none the model could time.
    pub fn empty() -> Self {
        Self {
            words: Vec::new(),
            score: None,
            placement: None,
        }
    }

    /// The same words and score: a corrected line is one that differs.
    fn same_times(&self, other: &Self) -> bool {
        self.words == other.words && self.score == other.score
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
/// happens: its offset once found, then each line as today's pass times
/// it, then each line the recogniser placed differently, again (ADR-0124).
/// A line's last report is what the final answer holds.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress<'a> {
    /// The song's lyric stamps are this far off the singing; every line moves by it.
    Offset {
        offset_ms: i64,
        aligner_version: u32,
        lines: usize,
    },
    /// Line `index` is timed; a later report of the same line replaces it.
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

/// Wall time of each stage after today's pass, for the song's log line;
/// zero for a stage that did not run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StageTimes {
    pub vocals_ms: u64,
    pub recognise_ms: u64,
    pub emission_ms: u64,
    pub place_ms: u64,
}

/// A finished song: the answer plus what the log line and span report.
#[derive(Debug)]
pub struct Aligned {
    pub answer: Answer,
    pub words_aligned: usize,
    pub probes: Vec<Option<i64>>,
    pub reason: String,
    pub phases: Phases,
    /// How the lines with words were placed.
    pub placement: Placement,
    /// Why the song fell back, naming the value; empty when it did not.
    pub fallback_reason: String,
    pub stages: StageTimes,
    /// Lines reported again because the recogniser moved them.
    pub corrected: usize,
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
    stages: Option<Box<dyn Stages>>,
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
            stages: None,
        }
    }

    /// Runs `stages` after today's pass; without them every song keeps
    /// today's answer with [`Fallback::Off`].
    #[must_use]
    pub fn with_stages(mut self, stages: Box<dyn Stages>) -> Self {
        self.stages = Some(stages);
        self
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
        // The offset probes' windows are decoded on CPU threads while the
        // model, if it was unloaded, loads on the GPU: neither waits on the
        // other, and the first line is out by the slower of the two (B1414).
        let windows = probe_windows(&lines);
        let parent = tracing::Span::current();
        let (loaded, probes) = std::thread::scope(|s| {
            let load = slot.model.is_none().then(|| {
                s.spawn(|| {
                    let started = Instant::now();
                    (self.loader)().map(|m| (m, started.elapsed()))
                })
            });
            let probes = decode_probes(&self.decoder, full_path, &windows, &parent, s);
            (load.map(std::thread::ScopedJoinHandle::join), probes)
        });
        if let Some(loaded) = loaded {
            let model = match loaded {
                Ok(Ok((m, took))) => {
                    tracing::info!(
                        load_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
                        probes = windows.len(),
                        "model loaded"
                    );
                    m
                }
                Ok(Err(e)) => {
                    let e = SongError::Internal(format!("model load failed: {e}"));
                    return (Err(e), None, None);
                }
                Err(_) => {
                    let e = SongError::Internal("model load panicked".into());
                    return (Err(e), None, None);
                }
            };
            slot.model = Some(model);
        }
        let result = match slot.model.as_deref_mut() {
            Some(model) => {
                model.begin_song();
                let r = run_song(model, &self.decoder, rel_path, full_path, lines, probes, on).map(
                    |today| match self.stages.as_deref() {
                        Some(stages) => improve(model, stages, rel_path, full_path, today, on),
                        None => today.kept(Fallback::Off, String::new()),
                    },
                );
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

/// One probe window decoded ahead of the offset estimate: its bounds, what
/// the decoder gave, and how long that took.
type Probe = ((i64, i64), Result<Vec<f32>, DecodeError>, u64);

/// Every probe window decoded at once, one thread each: ffmpeg runs on CPU
/// and the seven are independent, so together they take about as long as
/// the slowest rather than the sum. A window that times out costs only that
/// probe, and since all of them run together the wait is one timeout at most.
fn decode_probes<'scope>(
    decoder: &'scope Decoder,
    path: &'scope Path,
    windows: &'scope [(i64, i64)],
    parent: &'scope tracing::Span,
    s: &'scope std::thread::Scope<'scope, '_>,
) -> Vec<Probe> {
    let handles: Vec<_> = windows
        .iter()
        .map(|&(start, end)| {
            s.spawn(move || {
                let span = tracing::info_span!(
                    parent: parent,
                    "aligner.decode",
                    stage = "probe",
                    window_ms = end - start
                );
                let started = Instant::now();
                let got = span.in_scope(|| decoder(path, start, end));
                let took = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                ((start, end), got, took)
            })
        })
        .collect();
    handles
        .into_iter()
        .zip(windows)
        .map(|(h, &bounds)| {
            h.join().unwrap_or_else(|_| {
                let e = DecodeError::Io(std::io::Error::other("probe decode panicked"));
                (bounds, Err(e), 0)
            })
        })
        .collect()
}

/// Today's per-line answer, before the recogniser's stages.
struct Today {
    /// The request's lines, before the offset moved them: the recogniser's
    /// placement reads their stamps as sent.
    request: Vec<Line>,
    answer: Answer,
    words_aligned: usize,
    probes: Vec<Option<i64>>,
    reason: String,
    phases: Phases,
    with_words: usize,
}

impl Today {
    /// The song with today's answer and every line with words kept, because of `why`.
    fn kept(self, why: Fallback, reason: String) -> Aligned {
        let placement = Placement::kept(self.with_words, why);
        self.into_aligned(placement, reason, StageTimes::default(), 0)
    }

    fn into_aligned(
        mut self,
        placement: Placement,
        fallback_reason: String,
        stages: StageTimes,
        corrected: usize,
    ) -> Aligned {
        for (line, req) in self.answer.lines.iter_mut().zip(&self.request) {
            if line.placement.is_none() && !words(&req.text).is_empty() {
                line.placement = Some(Method::Kept);
            }
        }
        Aligned {
            answer: self.answer,
            words_aligned: self.words_aligned,
            probes: self.probes,
            reason: self.reason,
            phases: self.phases,
            placement,
            fallback_reason,
            stages,
            corrected,
        }
    }
}

/// `f`'s answer, or its panic as an error.
fn caught<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "a panic without a message".into());
        Err(format!("panicked: {message}"))
    })
}

fn ms_since(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Stages 2 to 7 of plan 042 after today's pass: the vocals, what the
/// recogniser heard in them, the letter model's emission over them, then
/// every line placed. A line placed differently from today is reported
/// again. Any stage that fails, or panics, leaves today's answer, with the
/// reason.
fn improve(
    model: &mut dyn Model,
    stages: &dyn Stages,
    rel_path: &str,
    full_path: &Path,
    today: Today,
    on: &mut dyn FnMut(Progress<'_>),
) -> Aligned {
    let mut times = StageTimes::default();
    let fail = |today: Today, times: StageTimes, e: StageError| {
        tracing::warn!(
            path = %rel_path,
            fallback = e.fallback.label(),
            reason = %e.reason,
            "recogniser stages fell back to today's answer"
        );
        let placement = Placement::kept(today.with_words, e.fallback);
        today.into_aligned(placement, e.reason, times, 0)
    };
    let stage = |fallback: Fallback| move |reason: String| StageError { fallback, reason };

    let t = Instant::now();
    let vocals = tracing::info_span!("aligner.stage", stage = "vocals").in_scope(|| {
        catch_unwind(AssertUnwindSafe(|| stages.vocals(full_path)))
            .unwrap_or_else(|_| Err(stage(Fallback::SeparateFailed)("vocals panicked".into())))
    });
    times.vocals_ms = ms_since(t);
    let vocals = match vocals {
        Ok(v) => v,
        Err(e) => return fail(today, times, e),
    };

    let t = Instant::now();
    let heard = tracing::info_span!("aligner.stage", stage = "recognise").in_scope(|| {
        catch_unwind(AssertUnwindSafe(|| stages.recognise(&vocals))).unwrap_or_else(|_| {
            Err(stage(Fallback::RecogniseFailed)(
                "recogniser panicked".into(),
            ))
        })
    });
    times.recognise_ms = ms_since(t);
    let heard = match heard {
        Ok(h) => h,
        Err(e) => return fail(today, times, e),
    };

    let t = Instant::now();
    let emission = tracing::info_span!("aligner.stage", stage = "emission").in_scope(|| {
        caught(|| {
            stitched(&vocals, |chunk| model.emission(chunk))
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "the letter model gives no emission".to_owned())
        })
        .map_err(stage(Fallback::EmissionFailed))
    });
    times.emission_ms = ms_since(t);
    let data = match emission {
        Ok(d) => d,
        Err(e) => return fail(today, times, e),
    };

    let t = Instant::now();
    let placed = tracing::info_span!("aligner.stage", stage = "place").in_scope(|| {
        let frames = data.len() / CLASSES;
        let emission = Emission::new(&data, frames, CLASSES)
            .map_err(|e| stage(Fallback::EmissionFailed)(e.to_string()))?;
        let matched = match_lines(&today.request, &heard, Options { slot_cap: true });
        let song = Song {
            emission,
            n_samples: vocals.len(),
        };
        caught(|| Ok(place(&today.request, &today.answer.lines, &matched, &song)))
            .map_err(stage(Fallback::EmissionFailed))
    });
    times.place_ms = ms_since(t);
    let refined = match placed {
        Ok(r) => r,
        Err(e) => return fail(today, times, e),
    };
    if let Some(why) = refined.placement.fallback {
        let reason = format!(
            "{}: anchor share {:.3}",
            why.label(),
            refined.placement.anchor_share.unwrap_or(0.0)
        );
        return today.into_aligned(refined.placement, reason, times, 0);
    }

    let mut today = today;
    let mut corrected = 0;
    for (index, ((line, was), method)) in refined
        .lines
        .into_iter()
        .zip(today.answer.lines.iter_mut())
        .zip(refined.methods)
        .enumerate()
    {
        let line = LineWords {
            placement: method,
            ..line
        };
        if !line.same_times(was) {
            corrected += 1;
            on(Progress::Line {
                index,
                words: &line,
            });
        }
        *was = line;
    }
    today.into_aligned(refined.placement, String::new(), times, corrected)
}

fn run_song(
    model: &mut dyn Model,
    decoder: &Decoder,
    rel_path: &str,
    full_path: &Path,
    lines: Vec<Line>,
    probes: Vec<Probe>,
    on: &mut dyn FnMut(Progress<'_>),
) -> Result<Today, SongError> {
    let request = lines.clone();
    let mut phases = Phases::default();
    let path: PathBuf = full_path.to_owned();
    // Summed over the probes, as the line decodes are, though they ran at once.
    phases.decode_ms = probes.iter().map(|(_, _, took)| took).sum();

    // Probes are optional: a window ffmpeg cannot decode costs the song its
    // offset, not its word times. They were decoded before the model was
    // ready (see `decode_probes`); here each is handed over in the order the
    // estimate asks for it.
    let mut probes = probes.into_iter();
    let probe_phases = std::cell::RefCell::new(&mut phases);
    let decode_probe = |start: i64, end: i64| -> Result<Vec<f32>, SongError> {
        let got = match probes.next() {
            Some((bounds, got, _)) if bounds == (start, end) => got,
            // Not the window decoded for this probe: decode it now.
            _ => {
                let span =
                    tracing::info_span!("aligner.decode", stage = "probe", window_ms = end - start);
                span.in_scope(|| {
                    let mut p = probe_phases.borrow_mut();
                    Phases::time(&mut p.decode_ms, || decoder(&path, start, end))
                })
            }
        };
        match got {
            Ok(pcm) => Ok(pcm),
            Err(e @ DecodeError::TimedOut { .. }) => {
                tracing::warn!(path = %rel_path, error = %e, "offset probe decode timed out");
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
    let mut with_words = 0;
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
        with_words += 1;
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
        let done = LineWords {
            words: line_stamps,
            score: spans.score.and_then(round_score),
            placement: None,
        };
        on(Progress::Line {
            index,
            words: &done,
        });
        out.push(done);
    }
    Ok(Today {
        request,
        answer: Answer {
            lines: out,
            offset_ms: found.offset_ms,
            aligner_version: ALIGNER_VERSION,
        },
        words_aligned,
        probes: found.probes,
        reason: found.reason,
        phases,
        with_words,
    })
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
