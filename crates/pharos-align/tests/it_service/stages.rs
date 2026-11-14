//! The recogniser's stages after today's pass (spec 042, ADR-0124), with
//! scripted vocals, words heard and emission: a song the recogniser places
//! is corrected line by line, and one whose stage fails keeps today's answer.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pharos_align::crash::{CrashGuard, IN_FLIGHT};
use pharos_align::line::LineSpans;
use pharos_align::placement::{Fallback, Method};
use pharos_align::plan::Line;
use pharos_align::service::{
    Aligned, Decoder, DeviceUsed, Engine, LineWords, Loader, Model, ModelError, Progress,
    StageError, Stages, ALIGNER_VERSION,
};
use pharos_align::tokens::{label, CLASSES, STAR};
use pharos_align::transcript::Word;

const RATE: usize = 16_000;
/// 20 ms frames, as MMS_FA gives at 16 kHz.
const HOP: usize = 320;
const FRAME_MS: i64 = 20;
const SONG_S: usize = 10;

/// Today's model, as `engine.rs`'s `Even`, that can also give an emission
/// in which `sung` letters stand out of the blank, on `device`.
struct Singer {
    sung: Option<Vec<(char, i64)>>,
    device: &'static str,
}

impl Model for Singer {
    fn align(&mut self, _: &[f32], tokens: &[String], _: bool) -> Result<LineSpans, ModelError> {
        Ok((0..tokens.len() as u64)
            .map(|i| (i * 100, (i + 1) * 100))
            .collect::<Vec<_>>()
            .into())
    }

    fn emission(&mut self, pcm: &[f32]) -> Result<Option<Vec<f32>>, ModelError> {
        let Some(sung) = &self.sung else {
            return Ok(None);
        };
        let frames = pcm.len() / HOP;
        let mut e = vec![-12.0f32; frames * CLASSES];
        for t in 0..frames {
            e[t * CLASSES] = -0.05;
            e[t * CLASSES + STAR as usize] = 0.0;
        }
        for &(c, ms) in sung {
            let c = label(c).unwrap() as usize;
            let f = usize::try_from(ms / FRAME_MS).unwrap();
            for t in f..(f + 3).min(frames) {
                e[t * CLASSES] = -6.0;
                e[t * CLASSES + c] = -0.01;
            }
        }
        Ok(Some(e))
    }

    fn device(&self) -> DeviceUsed {
        DeviceUsed {
            device: self.device,
            reason: "scripted".into(),
        }
    }
}

/// The letters of `text` sung from `start_ms`, 100 ms apart, words 200 ms apart.
fn sing(text: &str, start_ms: i64) -> Vec<(char, i64)> {
    let mut out = Vec::new();
    let mut t = start_ms;
    for w in text.split_whitespace() {
        for c in w.chars() {
            out.push((c, t));
            t += 100;
        }
        t += 100;
    }
    out
}

fn heard(text: &str, start_s: f64) -> Vec<Word> {
    text.split_whitespace()
        .enumerate()
        .map(|(k, w)| Word {
            text: format!(" {w}"),
            start_s: start_s + 0.5 * k as f64,
            end_s: start_s + 0.5 * k as f64 + 0.4,
            prob: 0.9,
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Step {
    Works,
    Fails,
    Panics,
}

struct Scripted {
    vocals: Step,
    recognise: Step,
    heard: Vec<Word>,
    /// How long separating takes.
    slow: Duration,
}

impl Stages for Scripted {
    fn vocals(&self, _: &Path, _: Instant) -> Result<Vec<f32>, StageError> {
        std::thread::sleep(self.slow);
        match self.vocals {
            Step::Works => Ok(vec![0.0; SONG_S * RATE]),
            Step::Fails => Err(StageError {
                fallback: Fallback::SeparateFailed,
                reason: "out of device memory".into(),
            }),
            Step::Panics => panic!("separator blew up"),
        }
    }

    fn recognise(&self, vocals: &[f32], _: Instant) -> Result<Vec<Word>, StageError> {
        assert_eq!(vocals.len(), SONG_S * RATE);
        match self.recognise {
            Step::Works => Ok(self.heard.clone()),
            Step::Fails => Err(StageError {
                fallback: Fallback::RecogniseFailed,
                reason: "whisper failed".into(),
            }),
            Step::Panics => panic!("recogniser blew up"),
        }
    }
}

fn lines() -> Vec<Line> {
    vec![
        Line {
            start_ms: 1_000,
            end_ms: 5_000,
            text: "grey gulls wheel".into(),
        },
        Line {
            start_ms: 5_000,
            end_ms: 9_000,
            text: "low tide hums".into(),
        },
    ]
}

/// Both lines heard a little late, and sung later than their stamps.
fn song() -> (Vec<(char, i64)>, Vec<Word>) {
    let mut sung = sing("grey gulls wheel", 1_500);
    sung.extend(sing("low tide hums", 5_600));
    let mut h = heard("grey gulls wheel", 1.7);
    h.extend(heard("low tide hums", 5.9));
    (sung, h)
}

fn silent() -> Decoder {
    Box::new(|_: &Path, _, _| Ok(vec![0.0; 16]))
}

fn loader(sung: Option<Vec<(char, i64)>>) -> Loader {
    loader_on(sung, "vulkan")
}

fn loader_on(sung: Option<Vec<(char, i64)>>, device: &'static str) -> Loader {
    let sung = Arc::new(sung);
    Box::new(move || {
        Ok(Box::new(Singer {
            sung: (*sung).clone(),
            device,
        }) as Box<dyn Model>)
    })
}

/// Runs the song, keeping each event as JSON so a later report can be told
/// from an earlier one.
fn run(engine: &Engine) -> (Aligned, Vec<serde_json::Value>) {
    let mut events = Vec::new();
    let (r, ..) = engine.align_song_on("a", Path::new("/a"), lines(), &mut |p: Progress<'_>| {
        events.push(serde_json::to_value(p).unwrap());
    });
    (r.unwrap(), events)
}

fn today() -> (Aligned, Vec<serde_json::Value>) {
    run(&Engine::new(loader(None), silent(), None))
}

fn with(
    vocals: Step,
    recognise: Step,
    sung: Option<Vec<(char, i64)>>,
) -> (Aligned, Vec<serde_json::Value>) {
    let (_, heard) = song();
    let stages = Scripted {
        vocals,
        recognise,
        heard,
        slow: Duration::ZERO,
    };
    run(&Engine::new(loader(sung), silent(), None).with_stages(Box::new(stages)))
}

fn words(a: &Aligned) -> Vec<Vec<i64>> {
    a.answer
        .lines
        .iter()
        .map(|l| l.words.iter().map(|s| s.start_ms).collect())
        .collect()
}

fn kept(a: &Aligned) -> bool {
    a.answer
        .lines
        .iter()
        .all(|l: &LineWords| l.placement == Some(Method::Kept))
}

#[test]
fn a_placed_song_reports_each_moved_line_again_and_says_how_it_was_placed() {
    let (sung, _) = song();
    let (got, events) = with(Step::Works, Step::Works, Some(sung));
    let (before, _) = today();

    assert_eq!(got.answer.aligner_version, ALIGNER_VERSION);
    assert_eq!(ALIGNER_VERSION, 6);
    assert_eq!(got.placement.fallback, None);
    assert_eq!(got.fallback_reason, "");
    assert_eq!(got.placement.count(Method::Refined), 2);
    assert_eq!(got.corrected, 2);
    assert_ne!(words(&got), words(&before));

    let kinds: Vec<_> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["offset", "line", "line", "line", "line"]);
    // Today's lines stream without a placement; the corrections carry one.
    assert!(events[1].get("placement").is_none());
    assert!(events[2].get("placement").is_none());
    for (i, e) in events[3..].iter().enumerate() {
        assert_eq!(e["index"], i);
        assert_eq!(e["placement"], "refined");
        let streamed: Vec<i64> = e["words"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["start_ms"].as_i64().unwrap())
            .collect();
        assert_eq!(streamed, words(&got)[i], "the last report is the answer");
    }
    for l in &got.answer.lines {
        assert_eq!(l.placement, Some(Method::Refined));
    }
}

#[test]
fn without_stages_the_answer_is_todays_with_every_line_kept() {
    let (got, events) = today();
    assert_eq!(got.placement.fallback, Some(Fallback::Off));
    assert!(kept(&got));
    assert_eq!(got.corrected, 0);
    assert_eq!(events.len(), 3);
}

#[test]
fn each_failing_stage_keeps_todays_answer_and_names_why() {
    let (sung, _) = song();
    let (before, _) = today();
    let cases = [
        (
            Step::Fails,
            Step::Works,
            Some(sung.clone()),
            Fallback::SeparateFailed,
            "out of device memory",
        ),
        (
            Step::Panics,
            Step::Works,
            Some(sung.clone()),
            Fallback::SeparateFailed,
            "panicked",
        ),
        (
            Step::Works,
            Step::Fails,
            Some(sung.clone()),
            Fallback::RecogniseFailed,
            "whisper failed",
        ),
        (
            Step::Works,
            Step::Panics,
            Some(sung),
            Fallback::RecogniseFailed,
            "panicked",
        ),
        (
            Step::Works,
            Step::Works,
            None,
            Fallback::EmissionFailed,
            "no emission",
        ),
    ];
    for (vocals, recognise, sung, why, reason) in cases {
        let (got, events) = with(vocals, recognise, sung);
        assert_eq!(got.placement.fallback, Some(why));
        assert!(
            got.fallback_reason.contains(reason),
            "{why:?}: {}",
            got.fallback_reason
        );
        assert_eq!(words(&got), words(&before), "{why:?}");
        assert!(kept(&got), "{why:?}");
        assert_eq!(got.corrected, 0);
        assert_eq!(events.len(), 3, "{why:?}: no corrections");
    }
}

#[test]
fn a_song_the_recogniser_barely_heard_keeps_todays_answer() {
    let (sung, _) = song();
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: Vec::new(),
        slow: Duration::ZERO,
    };
    let (got, events) =
        run(&Engine::new(loader(Some(sung)), silent(), None).with_stages(Box::new(stages)));
    let (before, _) = today();
    assert_eq!(got.placement.fallback, Some(Fallback::LowAnchorShare));
    assert!(got.fallback_reason.starts_with("low_anchor_share"));
    assert_eq!(words(&got), words(&before));
    assert!(kept(&got));
    assert_eq!(events.len(), 3);
}

#[test]
fn a_song_on_the_cpu_skips_the_stages_and_says_where_it_ran() {
    let (sung, _) = song();
    // Both stages would panic: running either is a failure of the gate.
    let stages = Scripted {
        vocals: Step::Panics,
        recognise: Step::Panics,
        heard: Vec::new(),
        slow: Duration::ZERO,
    };
    let (got, events) =
        run(&Engine::new(loader_on(Some(sung), "cpu"), silent(), None)
            .with_stages(Box::new(stages)));
    let (before, _) = today();
    assert_eq!(got.placement.fallback, Some(Fallback::NoGpuStages));
    assert_eq!(got.fallback_reason, "the song ran on cpu: scripted");
    assert_eq!(got.stages.vocals_ms, 0);
    assert_eq!(words(&got), words(&before));
    assert!(kept(&got));
    assert_eq!(events.len(), 3);
}

#[test]
fn a_song_past_its_stage_budget_keeps_todays_answer_and_says_where_it_stopped() {
    let (sung, _) = song();
    let (before, _) = today();
    let (_, heard) = song();
    // Separating outlasts the budget, so the recogniser, which would
    // panic, never runs.
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Panics,
        heard,
        slow: Duration::from_millis(50),
    };
    let engine = Engine::new(loader(Some(sung)), silent(), None)
        .with_stages(Box::new(stages))
        .with_stage_budget(Duration::from_millis(20));
    let (got, events) = run(&engine);
    assert_eq!(got.placement.fallback, Some(Fallback::StageTimeout));
    assert!(
        got.fallback_reason
            .starts_with("stage budget 0.02 s passed at "),
        "{}",
        got.fallback_reason
    );
    assert!(
        got.fallback_reason.ends_with(", in recognise"),
        "{}",
        got.fallback_reason
    );
    assert_eq!(words(&got), words(&before));
    assert!(kept(&got));
    assert_eq!(events.len(), 3);
}

#[test]
fn a_stage_that_fails_past_the_budget_is_a_timeout_naming_its_error() {
    let (sung, _) = song();
    let (_, heard) = song();
    let stages = Scripted {
        vocals: Step::Fails,
        recognise: Step::Works,
        heard,
        slow: Duration::from_millis(50),
    };
    let engine = Engine::new(loader(Some(sung)), silent(), None)
        .with_stages(Box::new(stages))
        .with_stage_budget(Duration::from_millis(20));
    let (got, _) = run(&engine);
    assert_eq!(got.placement.fallback, Some(Fallback::StageTimeout));
    assert!(
        got.fallback_reason
            .ends_with(", in vocals: out of device memory"),
        "{}",
        got.fallback_reason
    );
}

fn guard_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pharos-align-guard-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Separates, and says whether the song was marked in flight meanwhile.
struct Watched {
    marker: std::path::PathBuf,
    saw: Arc<std::sync::Mutex<Option<String>>>,
}

impl Stages for Watched {
    fn vocals(&self, _: &Path, _: Instant) -> Result<Vec<f32>, StageError> {
        *self.saw.lock().unwrap() = std::fs::read_to_string(&self.marker).ok();
        Ok(vec![0.0; SONG_S * RATE])
    }

    fn recognise(&self, _: &[f32], _: Instant) -> Result<Vec<Word>, StageError> {
        Ok(song().1)
    }
}

#[test]
fn a_song_is_marked_in_flight_only_while_its_stages_run() {
    let (sung, _) = song();
    let d = guard_dir("flight");
    let saw = Arc::default();
    let stages = Watched {
        marker: d.join(IN_FLIGHT),
        saw: Arc::clone(&saw),
    };
    let engine = Engine::new(loader(Some(sung)), silent(), None)
        .with_stages(Box::new(stages))
        .with_crash_guard(CrashGuard::open(&d).unwrap());
    let (got, _) = run(&engine);
    assert_eq!(got.placement.fallback, None);
    assert_eq!(saw.lock().unwrap().as_deref(), Some("a"));
    assert!(!d.join(IN_FLIGHT).exists());
}

#[test]
fn a_song_whose_stages_ended_the_last_process_skips_them() {
    let (sung, _) = song();
    let (before, _) = today();
    let d = guard_dir("crashed");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(IN_FLIGHT), "a").unwrap();
    let stages = Scripted {
        vocals: Step::Panics,
        recognise: Step::Panics,
        heard: Vec::new(),
        slow: Duration::ZERO,
    };
    let engine = Engine::new(loader(Some(sung)), silent(), None)
        .with_stages(Box::new(stages))
        .with_crash_guard(CrashGuard::open(&d).unwrap());
    let (got, events) = run(&engine);
    assert_eq!(got.placement.fallback, Some(Fallback::StageCrashed));
    assert_eq!(
        got.fallback_reason,
        "the stages ended an earlier process on this song"
    );
    assert_eq!(words(&got), words(&before));
    assert!(kept(&got));
    assert_eq!(events.len(), 3);
}
