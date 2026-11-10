use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pharos_align::audio::DecodeError;
use pharos_align::plan::{Line, Stamp};
use pharos_align::service::{
    Decoder, Engine, Loader, Model, ModelError, SongError, ALIGNER_VERSION,
};

/// Evenly spaced 100 ms spans per token, as the Python tests' FakeAligner.
struct Even {
    /// Lines with more tokens than this fail as torchaudio does on long text.
    fail_over: usize,
    fatal: bool,
}

impl Model for Even {
    fn align(
        &mut self,
        _: &[f32],
        tokens: &[String],
        _: bool,
    ) -> Result<Vec<(u64, u64)>, ModelError> {
        if self.fatal {
            return Err(ModelError::Fatal("weights gone".into()));
        }
        if tokens.len() > self.fail_over {
            return Err(ModelError::Line(
                "targets length is too long for CTC".into(),
            ));
        }
        Ok((0..tokens.len() as u64)
            .map(|i| (i * 100, (i + 1) * 100))
            .collect())
    }
}

fn loader(fail_over: usize, fatal: bool, loads: Arc<AtomicUsize>) -> Loader {
    Box::new(move || {
        loads.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Even { fail_over, fatal }) as Box<dyn Model>)
    })
}

fn silent() -> Decoder {
    Box::new(|_: &Path, _, _| Ok(vec![0.0; 16]))
}

fn engine(decoder: Decoder) -> Engine {
    Engine::new(loader(usize::MAX, false, Arc::default()), decoder, None)
}

fn line(start_ms: i64, end_ms: i64, text: &str) -> Line {
    Line {
        start_ms,
        end_ms,
        text: text.into(),
    }
}

fn stamp(at: usize, start_ms: i64) -> Stamp {
    Stamp { at, start_ms }
}

fn failed() -> DecodeError {
    DecodeError::Failed {
        status: std::os::unix::process::ExitStatusExt::from_raw(256),
        start_ms: 0,
        end_ms: 0,
        stderr: "bad".into(),
    }
}

fn three_probe_lines(len_ms: i64) -> Vec<Line> {
    vec![
        line(20_000, 20_000 + len_ms, "one line sung here"),
        line(30_000, 30_000 + len_ms, "another line sung there"),
        line(40_000, 40_000 + len_ms, "the last line sung"),
    ]
}

#[test]
fn two_lines_one_without_words() {
    let lines = vec![
        line(1000, 2000, "Hello world"),
        line(2500, 3000, "\u{266a}\u{266a}"),
    ];
    let got = engine(silent())
        .align_song("a", Path::new("/a"), lines)
        .unwrap();
    assert_eq!(
        got.answer.lines[0].words,
        [stamp(0, 250), stamp(6, 350), stamp(11, 450)]
    );
    assert!(got.answer.lines[1].words.is_empty());
    assert_eq!(got.words_aligned, 2);
    assert_eq!(got.answer.aligner_version, ALIGNER_VERSION);
}

#[test]
fn a_line_sung_straight_after_another_cannot_take_its_last_word() {
    let lines = vec![
        line(1000, 1100, "Hello world"),
        line(1100, 3000, "next line"),
    ];
    let got = engine(silent())
        .align_song("a", Path::new("/a"), lines)
        .unwrap();
    assert_eq!(got.answer.lines[1].words[0], stamp(0, 450));
}

#[test]
fn a_failing_line_leaves_the_others() {
    let e = Engine::new(loader(2, false, Arc::default()), silent(), None);
    let lines = vec![
        line(0, 0, "far too many words here"),
        line(1000, 2000, "hello there"),
    ];
    let got = e.align_song("a", Path::new("/a"), lines).unwrap();
    assert!(got.answer.lines[0].words.is_empty());
    assert!(!got.answer.lines[1].words.is_empty());
}

#[test]
fn a_broken_model_fails_the_song() {
    let e = Engine::new(loader(usize::MAX, true, Arc::default()), silent(), None);
    let got = e.align_song("a", Path::new("/a"), vec![line(0, 1000, "hello")]);
    assert!(matches!(got, Err(SongError::Internal(_))), "{got:?}");
}

#[test]
fn missing_ffmpeg_is_internal_not_ffmpeg() {
    let missing: Decoder = Box::new(|_: &Path, _, _| {
        Err(DecodeError::Spawn {
            program: "ffmpeg".into(),
            source: std::io::ErrorKind::NotFound.into(),
        })
    });
    let got = engine(missing).align_song("a", Path::new("/a"), vec![line(0, 1000, "hello")]);
    assert!(matches!(got, Err(SongError::Internal(_))), "{got:?}");
    // Probes hit it first; a song with none still fails on its line window.
    let missing: Decoder = Box::new(|_: &Path, _, _| {
        Err(DecodeError::Spawn {
            program: "ffmpeg".into(),
            source: std::io::ErrorKind::NotFound.into(),
        })
    });
    let got = engine(missing).align_song("a", Path::new("/a"), three_probe_lines(1000));
    assert!(matches!(got, Err(SongError::Internal(_))), "{got:?}");
}

#[test]
fn a_line_ffmpeg_cannot_decode_fails_the_song_as_ffmpeg() {
    let bad: Decoder = Box::new(|_: &Path, _, _| Err(failed()));
    let got = engine(bad).align_song("a", Path::new("/a"), vec![line(0, 1000, "hello")]);
    assert!(matches!(got, Err(SongError::Ffmpeg(_))), "{got:?}");
    let slow: Decoder = Box::new(|_: &Path, s, e| {
        Err(DecodeError::TimedOut {
            after: Duration::from_secs(120),
            start_ms: s,
            end_ms: e,
        })
    });
    let got = engine(slow).align_song("a", Path::new("/a"), vec![line(0, 1000, "hello")]);
    assert!(matches!(got, Err(SongError::Internal(_))), "{got:?}");
}

#[test]
fn reports_the_whole_song_offset_and_aligns_the_shifted_lines() {
    // Every first word lands at the start of its window, so each wide probe
    // finds its line SEARCH_MS early.
    let got = engine(silent())
        .align_song("a", Path::new("/a"), three_probe_lines(10_000))
        .unwrap();
    assert_eq!(got.answer.offset_ms, -8_000);
    assert_eq!(got.answer.lines[0].words[0], stamp(0, 11_250));
}

#[test]
fn a_probe_that_cannot_be_decoded_costs_only_the_offset() {
    let wide_fail: Decoder = Box::new(|_: &Path, s, e| {
        if e - s > 10_000 {
            Err(failed())
        } else {
            Ok(vec![0.0; 16])
        }
    });
    let got = engine(wide_fail)
        .align_song("a", Path::new("/a"), three_probe_lines(1000))
        .unwrap();
    assert_eq!(got.answer.offset_ms, 0);
    assert!(got.answer.lines.iter().all(|l| !l.words.is_empty()));
}

#[test]
fn a_probe_that_times_out_skips_the_other_probes() {
    let wide = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&wide);
    let hang: Decoder = Box::new(move |_: &Path, s, e| {
        if e - s > 10_000 {
            calls.lock().unwrap().push(s);
            return Err(DecodeError::TimedOut {
                after: Duration::from_secs(120),
                start_ms: s,
                end_ms: e,
            });
        }
        Ok(vec![0.0; 16])
    });
    let got = engine(hang)
        .align_song("a", Path::new("/a"), three_probe_lines(1000))
        .unwrap();
    assert_eq!(got.answer.offset_ms, 0);
    assert!(got.answer.lines.iter().all(|l| !l.words.is_empty()));
    assert_eq!(wide.lock().unwrap().len(), 1);
}

#[test]
fn an_idle_model_is_unloaded_and_reloaded_on_the_next_song() {
    let loads = Arc::new(AtomicUsize::new(0));
    let e = Engine::new(
        loader(usize::MAX, false, Arc::clone(&loads)),
        silent(),
        Some(Duration::ZERO),
    );
    assert!(!e.unload_if_idle(), "nothing loaded yet");
    e.align_song("a", Path::new("/a"), vec![line(0, 1000, "hello")])
        .unwrap();
    e.align_song("a", Path::new("/a"), vec![line(0, 1000, "hello")])
        .unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert!(e.unload_if_idle());
    e.align_song("a", Path::new("/a"), vec![line(0, 1000, "hello")])
        .unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 2);

    let kept = Engine::new(loader(usize::MAX, false, Arc::default()), silent(), None);
    kept.align_song("a", Path::new("/a"), vec![line(0, 1000, "hello")])
        .unwrap();
    assert!(!kept.unload_if_idle(), "no idle timeout keeps the model");
}
