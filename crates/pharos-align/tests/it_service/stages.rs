//! The recogniser's stages after today's pass (spec 042, ADR-0124), with
//! scripted vocals, words heard and emission: a song the recogniser places
//! is corrected line by line, and one whose stage fails keeps today's answer.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pharos_align::crash::{CrashGuard, IN_FLIGHT};
use pharos_align::line::LineSpans;
use pharos_align::placement::{Fallback, Method};
use pharos_align::plan::{Line, SongOptions};
use pharos_align::service::{
    Aligned, Decoder, DeviceUsed, Engine, LineWords, Loader, Model, ModelError, Progress,
    SongError, StageError, Stages, StreamDecoder, ALIGNER_VERSION,
};
use pharos_align::tokens::{label, CLASSES, STAR};
use pharos_align::transcript::{Transcript, Word};

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
    /// Falls to the CPU at its first emission, as `Fallback` does on a
    /// GPU device error.
    falls_back: bool,
}

impl Model for Singer {
    fn align(&mut self, _: &[f32], tokens: &[String], _: bool) -> Result<LineSpans, ModelError> {
        Ok((0..tokens.len() as u64)
            .map(|i| (i * 100, (i + 1) * 100))
            .collect::<Vec<_>>()
            .into())
    }

    fn emission(&mut self, pcm: &[f32]) -> Result<Option<Vec<f32>>, ModelError> {
        if self.falls_back {
            self.device = "cpu";
        }
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
            silent_lead: false,
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
    heard: Transcript,
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

    fn recognise(&self, vocals: &[f32], _: Instant) -> Result<Transcript, StageError> {
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

fn transcript(words: Vec<Word>) -> Transcript {
    Transcript {
        words,
        first_words_clipped: 0,
    }
}

fn silent() -> Decoder {
    Box::new(|_: &Path, _, _| Ok(vec![0.0; 16]))
}

fn loader(sung: Option<Vec<(char, i64)>>) -> Loader {
    loader_on(sung, "vulkan")
}

fn loader_on(sung: Option<Vec<(char, i64)>>, device: &'static str) -> Loader {
    singers(sung, device, false)
}

fn singers(sung: Option<Vec<(char, i64)>>, device: &'static str, falls_back: bool) -> Loader {
    let sung = Arc::new(sung);
    Box::new(move || {
        Ok(Box::new(Singer {
            sung: (*sung).clone(),
            device,
            falls_back,
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
        heard: transcript(heard),
        slow: Duration::ZERO,
    };
    run(&Engine::new(loader(sung), silent(), None).with_stages(Box::new(stages)))
}

/// The offset and line events, without the text check's.
fn timed(events: &[serde_json::Value]) -> usize {
    events.iter().filter(|e| e["event"] != "text_clear").count()
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
fn the_song_says_how_many_first_words_were_clipped_and_anchors_started_late() {
    let (sung, mut heard) = song();
    // "low" heard from just after the line before it to where it was sung.
    heard[3].start_s = 3.2;
    heard[3].silent_lead = true;
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: Transcript {
            words: heard,
            first_words_clipped: 1,
        },
        slow: Duration::ZERO,
    };
    let (got, _) =
        run(&Engine::new(loader(Some(sung)), silent(), None).with_stages(Box::new(stages)));
    assert_eq!(got.placement.fallback, None);
    assert_eq!(got.placement.first_words_clipped, 1);
    assert_eq!(got.placement.anchors_from_second_word, 1);
}

#[test]
fn a_stage_failing_after_the_recogniser_still_says_how_many_first_words_it_clipped() {
    let (sung, heard) = song();
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: Transcript {
            words: heard,
            first_words_clipped: 2,
        },
        slow: Duration::ZERO,
    };
    let (got, _) = run(
        &Engine::new(singers(Some(sung), "vulkan", true), silent(), None)
            .with_stages(Box::new(stages)),
    );
    assert_eq!(got.placement.fallback, Some(Fallback::EmissionFailed));
    assert_eq!(got.placement.first_words_clipped, 2);

    let (_, heard) = song();
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Fails,
        heard: Transcript {
            words: heard,
            first_words_clipped: 2,
        },
        slow: Duration::ZERO,
    };
    let (got, _) = run(&Engine::new(loader(None), silent(), None).with_stages(Box::new(stages)));
    assert_eq!(got.placement.fallback, Some(Fallback::RecogniseFailed));
    assert_eq!(got.placement.first_words_clipped, 0);
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
    assert_eq!(
        kinds,
        ["offset", "line", "line", "text_clear", "line", "line"]
    );
    // Today's lines stream without a placement; the corrections carry one.
    assert!(events[1].get("placement").is_none());
    assert!(events[2].get("placement").is_none());
    for (i, e) in events[4..].iter().enumerate() {
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
        assert_eq!(timed(&events), 3, "{why:?}: no corrections");
    }
}

#[test]
fn a_song_the_recogniser_barely_heard_keeps_todays_answer() {
    let (sung, _) = song();
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: Transcript::default(),
        slow: Duration::ZERO,
    };
    let (got, events) =
        run(&Engine::new(loader(Some(sung)), silent(), None).with_stages(Box::new(stages)));
    let (before, _) = today();
    assert_eq!(got.placement.fallback, Some(Fallback::LowAnchorShare));
    assert!(got.fallback_reason.starts_with("low_anchor_share"));
    assert_eq!(words(&got), words(&before));
    assert!(kept(&got));
    assert_eq!(timed(&events), 3);
}

/// V1468: every song with a transcript says how much of its lyric text was
/// heard, on the placed path, the fallback path and a failed later stage;
/// a song with no transcript says nothing.
#[test]
fn every_song_with_a_transcript_carries_its_text_check() {
    let (sung, _) = song();
    let (placed, _) = with(Step::Works, Step::Works, Some(sung));
    let check = placed.check.expect("a placed song carries its check");
    assert_eq!(check.text_recall, Some(1.0));
    assert_eq!(check.heard_words, 6);

    let (no_emission, _) = with(Step::Works, Step::Works, None);
    assert_eq!(
        no_emission.placement.fallback,
        Some(Fallback::EmissionFailed)
    );
    assert_eq!(no_emission.check.map(|c| c.heard_words), Some(6));

    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: transcript(heard("the lanterns swing in the orchard", 1.0)),
        slow: Duration::ZERO,
    };
    let (got, _) =
        run(&Engine::new(loader(Some(song().0)), silent(), None).with_stages(Box::new(stages)));
    assert_eq!(got.placement.fallback, Some(Fallback::LowAnchorShare));
    let check = got.check.expect("a fallen-back song carries its check");
    assert_eq!(check.text_recall, Some(0.0));
    assert_eq!(check.heard_words, 6);

    let (failed, _) = with(Step::Works, Step::Fails, None);
    assert_eq!(failed.check, None);
    let (before, _) = today();
    assert_eq!(before.check, None);
}

/// B1479/V1468: a song heard at length to other words falls back as a
/// lyrics mismatch, keeps today's answer and says what was heard; a placed
/// song's answer carries no check.
#[test]
fn a_song_sung_to_other_words_is_a_lyrics_mismatch() {
    let other: Vec<Word> = (0..120)
        .map(|k| Word {
            text: format!(" lantern{}", k % 40),
            start_s: 0.08 * k as f64,
            end_s: 0.08 * k as f64 + 0.05,
            prob: 0.9,
            silent_lead: false,
        })
        .collect();
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: transcript(other),
        slow: Duration::ZERO,
    };
    let (got, events) =
        run(&Engine::new(loader(Some(song().0)), silent(), None).with_stages(Box::new(stages)));
    let (before, _) = today();
    assert_eq!(got.placement.fallback, Some(Fallback::LyricsMismatch));
    assert!(
        got.fallback_reason
            .starts_with("lyrics_mismatch: anchor share 0.000, text recall 0.000, heard 120 words"),
        "{}",
        got.fallback_reason
    );
    assert_eq!(words(&got), words(&before));
    assert!(kept(&got));
    assert_eq!(events.len(), 3);
    let answer = serde_json::to_value(&got.answer).unwrap();
    assert_eq!(answer["check"]["fallback"], "lyrics_mismatch");
    assert_eq!(answer["check"]["text_recall"], 0.0);
    assert_eq!(answer["check"]["heard_words"], 120);
    assert!(answer["check"]["heard"]
        .as_str()
        .unwrap()
        .starts_with(" lantern0 lantern1"));

    let (placed, _) = with(Step::Works, Step::Works, Some(song().0));
    assert_eq!(placed.placement.fallback, None);
    assert!(serde_json::to_value(&placed.answer)
        .unwrap()
        .get("check")
        .is_none());

    // A failed emission after the recogniser heard the song falls back as
    // EmissionFailed, still with the check for the server.
    let (barely, _) = with(Step::Works, Step::Works, None);
    assert_eq!(barely.placement.fallback, Some(Fallback::EmissionFailed));
    let answer = serde_json::to_value(&barely.answer).unwrap();
    assert_eq!(answer["check"]["fallback"], "emission_failed");
}

/// B1479/V1468: a song heard at length to other words is a lyrics mismatch
/// even when the letter model then fails, so the server still drops the
/// lyric's line times.
#[test]
fn a_song_sung_to_other_words_is_a_mismatch_though_its_emission_failed() {
    let other: Vec<Word> = (0..120)
        .map(|k| Word {
            text: format!(" lantern{}", k % 40),
            start_s: 0.08 * k as f64,
            end_s: 0.08 * k as f64 + 0.05,
            prob: 0.9,
            silent_lead: false,
        })
        .collect();
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: transcript(other),
        slow: Duration::ZERO,
    };
    let (got, _) = run(&Engine::new(loader(None), silent(), None).with_stages(Box::new(stages)));
    assert_eq!(got.placement.fallback, Some(Fallback::LyricsMismatch));
    assert!(
        got.fallback_reason
            .starts_with("lyrics_mismatch: text recall 0.000, heard 120 words; emission_failed: "),
        "{}",
        got.fallback_reason
    );
    assert!(kept(&got));
    let answer = serde_json::to_value(&got.answer).unwrap();
    assert_eq!(answer["check"]["fallback"], "lyrics_mismatch");
    assert_eq!(answer["check"]["heard_words"], 120);
}

/// V1468: once the recogniser heard the lyric's text, a song that cannot
/// end a mismatch says so before it is done, after the lines it has timed;
/// a song heard to other words, or one the recogniser failed, never does.
#[test]
fn a_song_heard_to_its_lyric_is_cleared_before_it_is_done() {
    let clear = serde_json::json!({"event": "text_clear"});
    let (_, events) = with(Step::Works, Step::Works, Some(song().0));
    let at = events.iter().position(|e| *e == clear);
    assert_eq!(at, Some(3), "{events:?}");

    let other: Vec<Word> = (0..120)
        .map(|k| Word {
            text: format!(" lantern{}", k % 40),
            start_s: 0.08 * k as f64,
            end_s: 0.08 * k as f64 + 0.05,
            prob: 0.9,
            silent_lead: false,
        })
        .collect();
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: transcript(other),
        slow: Duration::ZERO,
    };
    let (_, wrong) =
        run(&Engine::new(loader(Some(song().0)), silent(), None).with_stages(Box::new(stages)));
    assert!(!wrong.contains(&clear), "{wrong:?}");
    let (_, failed) = with(Step::Works, Step::Fails, None);
    assert!(!failed.contains(&clear), "{failed:?}");
}

#[test]
fn a_song_on_the_cpu_skips_the_stages_and_says_where_it_ran() {
    let (sung, _) = song();
    // Both stages would panic: running either is a failure of the gate.
    let stages = Scripted {
        vocals: Step::Panics,
        recognise: Step::Panics,
        heard: Transcript::default(),
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

/// Both stages panic: running either is a failure of the request's options.
fn panicking() -> Scripted {
    Scripted {
        vocals: Step::Panics,
        recognise: Step::Panics,
        heard: Transcript::default(),
        slow: Duration::ZERO,
    }
}

fn run_with(engine: &Engine, opts: SongOptions) -> Result<Aligned, SongError> {
    let (r, ..) = engine.align_song_with("a", Path::new("/a"), lines(), opts, &mut |_| {});
    r
}

#[test]
fn a_request_without_stages_keeps_todays_answer_and_says_it_was_not_asked() {
    let (sung, _) = song();
    let engine = Engine::new(loader(Some(sung)), silent(), None).with_stages(Box::new(panicking()));
    let opts = SongOptions {
        audio_stream: None,
        stages: false,
    };
    let got = run_with(&engine, opts).unwrap();
    let (before, _) = today();
    assert_eq!(got.placement.fallback, Some(Fallback::NotAsked));
    assert_eq!(got.stages.vocals_ms, 0);
    assert_eq!(words(&got), words(&before));
    assert!(kept(&got));
}

#[test]
fn a_named_audio_stream_is_decoded_from_that_stream_only() {
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let stream: StreamDecoder = Box::new(move |_: &Path, i, _, _| {
        seen.lock().unwrap().push(i);
        Ok(vec![0.0; 16])
    });
    let default: Decoder = Box::new(|_: &Path, _, _| panic!("the default audio was decoded"));
    let engine = Engine::new(loader(None), default, None).with_stream_decoder(stream);
    let opts = SongOptions {
        audio_stream: Some(3),
        stages: false,
    };
    run_with(&engine, opts).unwrap();
    let asked = asked.lock().unwrap();
    assert!(!asked.is_empty());
    assert!(asked.iter().all(|&i| i == 3), "{asked:?}");
}

#[test]
fn a_named_audio_stream_with_the_stages_on_is_refused() {
    let stream: StreamDecoder = Box::new(|_: &Path, _, _, _| Ok(vec![0.0; 16]));
    let engine = Engine::new(loader(None), silent(), None)
        .with_stream_decoder(stream)
        .with_stages(Box::new(panicking()));
    let opts = SongOptions {
        audio_stream: Some(2),
        stages: true,
    };
    match run_with(&engine, opts) {
        Err(SongError::Internal(m)) => assert!(m.contains("default audio"), "{m}"),
        other => panic!("{:?}", other.map(|a| a.placement)),
    }
}

#[test]
fn a_named_audio_stream_without_a_stream_decoder_is_refused() {
    let engine = Engine::new(loader(None), silent(), None);
    let opts = SongOptions {
        audio_stream: Some(1),
        stages: true,
    };
    match run_with(&engine, opts) {
        Err(SongError::Internal(m)) => assert!(m.contains("audio_stream 1"), "{m}"),
        other => panic!("{:?}", other.map(|a| a.placement)),
    }
}

#[test]
fn an_emission_that_falls_back_to_the_cpu_fails_the_stage() {
    let (sung, heard) = song();
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: transcript(heard),
        slow: Duration::ZERO,
    };
    let (got, events) = run(
        &Engine::new(singers(Some(sung), "vulkan", true), silent(), None)
            .with_stages(Box::new(stages)),
    );
    let (before, _) = today();
    assert_eq!(got.placement.fallback, Some(Fallback::EmissionFailed));
    assert!(
        got.fallback_reason.contains("fell back to the cpu"),
        "{}",
        got.fallback_reason
    );
    assert_eq!(words(&got), words(&before));
    assert!(kept(&got));
    assert_eq!(timed(&events), 3);
}

/// Today's windows work on the GPU; its emission meets a lost device.
struct LostOnEmission;

impl Model for LostOnEmission {
    fn align(&mut self, _: &[f32], tokens: &[String], _: bool) -> Result<LineSpans, ModelError> {
        Ok((0..tokens.len() as u64)
            .map(|i| (i * 100, (i + 1) * 100))
            .collect::<Vec<_>>()
            .into())
    }

    fn emission(&mut self, _: &[f32]) -> Result<Option<Vec<f32>>, ModelError> {
        Err(ModelError::Device("device lost".into()))
    }

    fn device(&self) -> DeviceUsed {
        DeviceUsed {
            device: "vulkan",
            reason: "scripted".into(),
        }
    }
}

#[test]
fn a_gpu_emission_that_fails_fails_the_stage_without_the_cpu_pass() {
    use pharos_align::forward::fallback::Fallback as GpuOrCpu;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (sung, heard) = song();
    let stages = Scripted {
        vocals: Step::Works,
        recognise: Step::Works,
        heard: transcript(heard),
        slow: Duration::ZERO,
    };
    let cpu_loads = Arc::new(AtomicUsize::new(0));
    let loads = Arc::clone(&cpu_loads);
    let sung = Arc::new(sung);
    let loader: Loader = Box::new(move || {
        let loads = Arc::clone(&loads);
        let sung = Arc::clone(&sung);
        Ok(Box::new(GpuOrCpu::new(
            Box::new(LostOnEmission),
            Box::new(move || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Singer {
                    sung: Some((*sung).clone()),
                    device: "cpu",
                    falls_back: false,
                }) as Box<dyn Model>)
            }),
        )) as Box<dyn Model>)
    });
    let (got, _) = run(&Engine::new(loader, silent(), None).with_stages(Box::new(stages)));
    let (before, _) = today();
    assert_eq!(got.placement.fallback, Some(Fallback::EmissionFailed));
    assert!(
        got.fallback_reason.contains("device lost"),
        "{}",
        got.fallback_reason
    );
    assert_eq!(words(&got), words(&before));
    assert_eq!(
        cpu_loads.load(Ordering::SeqCst),
        0,
        "the CPU pass was built"
    );
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
        heard: transcript(heard),
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
        heard: transcript(heard),
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

    fn recognise(&self, _: &[f32], _: Instant) -> Result<Transcript, StageError> {
        Ok(transcript(song().1))
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
        heard: Transcript::default(),
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
