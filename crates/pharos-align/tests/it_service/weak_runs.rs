//! Weak runs (B1402, V1372): lines the per-line pass matched badly, between
//! lines it matched well, are counted per song and on the metric, and
//! re-searched together over the audio between their anchors.

use std::path::Path;

use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_align::line::LineSpans;
use pharos_align::plan::Line;
use pharos_align::research::{RunCounts, WEAK_RUNS_TOTAL};
use pharos_align::service::{Aligned, Decoder, Engine, Model, ModelError, Progress};
use pharos_align::tokens::CLASSES;

/// 100 ms per token; a line holding the word "lost" scores 0.05, others 0.5.
/// With `letters` set, a window's emission gives every letter that
/// probability on every frame, so a re-searched line scores about it.
struct Scored {
    letters: Option<f32>,
}

impl Model for Scored {
    fn align(&mut self, _: &[f32], tokens: &[String], _: bool) -> Result<LineSpans, ModelError> {
        let lost = tokens.iter().any(|t| t == "lost");
        Ok(LineSpans {
            times: (0..tokens.len() as u64)
                .map(|i| (i * 100, (i + 1) * 100))
                .collect(),
            score: Some(if lost { 0.05 } else { 0.5 }),
        })
    }

    fn emission(&mut self, pcm: &[f32]) -> Result<Option<Vec<f32>>, ModelError> {
        Ok(self.letters.map(|p| {
            let mut row = [p.ln(); CLASSES];
            row[0] = 0.5f32.ln();
            row[CLASSES - 1] = 0.0;
            row.repeat(pcm.len() / 320)
        }))
    }
}

fn line(start_ms: i64, text: &str) -> Line {
    Line {
        start_ms,
        end_ms: start_ms + 3_000,
        text: text.into(),
    }
}

fn engine() -> Engine {
    engine_with(None)
}

/// 16 kHz silence as long as the window asked for.
fn engine_with(letters: Option<f32>) -> Engine {
    let decoder: Decoder = Box::new(|_: &Path, start, end| {
        Ok(vec![0.0; usize::try_from((end - start) * 16).unwrap_or(0)])
    });
    Engine::new(
        Box::new(move || Ok(Box::new(Scored { letters }) as Box<dyn Model>)),
        decoder,
        None,
    )
}

/// The song's answer, and each reported line as (index, its words) in the
/// order reported.
fn streamed(engine: &Engine, lines: Vec<Line>) -> (Aligned, Vec<(usize, Vec<i64>)>) {
    let mut heard = Vec::new();
    let (got, ..) = engine.align_song_on("a.mp3", Path::new("/a"), lines, &mut |p| {
        if let Progress::Line { index, words } = p {
            heard.push((index, firsts(&words.words)));
        }
    });
    (got.unwrap(), heard)
}

fn firsts(words: &[pharos_align::plan::Stamp]) -> Vec<i64> {
    words.iter().map(|w| w.start_ms).collect()
}

/// Two runs: lines 1-3 (the empty line 2 inside it) between two anchors,
/// and line 5 running to the song's end.
fn song() -> Vec<Line> {
    vec![
        line(1_000, "found here"),
        line(4_000, "lost here"),
        line(7_000, ""),
        line(10_000, "lost again"),
        line(13_000, "found there"),
        line(16_000, "lost last"),
    ]
}

#[test]
fn each_weak_run_is_counted_on_the_song_and_the_metric() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let got = engine()
        .align_song("a.mp3", Path::new("/a"), song())
        .unwrap();
    let total: u64 = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| k.key().name() == WEAK_RUNS_TOTAL)
        .map(|(.., v)| match v {
            DebugValue::Counter(c) => c,
            _ => 0,
        })
        .sum();
    assert_eq!(got.runs.total(), 2, "{:?}", got.runs);
    assert_eq!(total, 2);
}

#[test]
fn a_song_with_no_weak_line_has_no_runs() {
    let lines = vec![line(1_000, "found here"), line(4_000, "found there")];
    let got = engine()
        .align_song("a.mp3", Path::new("/a"), lines)
        .unwrap();
    assert_eq!(got.runs, RunCounts::default());
}

#[test]
fn a_re_search_that_scores_better_replaces_the_run_in_order() {
    let before = engine()
        .align_song("a.mp3", Path::new("/a"), song())
        .unwrap();
    let (got, heard) = streamed(&engine_with(Some(0.3)), song());
    assert_eq!(got.runs.accepted, 2, "{:?}", got.runs);
    let lines = &got.answer.lines;
    for i in [1, 3, 5] {
        assert_eq!(
            lines[i].score,
            Some(0.3),
            "line {i} scored by the re-search"
        );
        assert_ne!(
            lines[i].words, before.answer.lines[i].words,
            "line {i} moved"
        );
    }
    for i in [0, 4] {
        assert_eq!(lines[i], before.answer.lines[i], "anchor {i} unchanged");
    }
    let starts: Vec<i64> = [0, 1, 3, 4, 5]
        .iter()
        .map(|&i| lines[i].words[0].start_ms)
        .collect();
    assert!(
        starts.windows(2).all(|p| p[0] <= p[1]),
        "V1372 order: {starts:?}"
    );
    assert_eq!(got.words_aligned, 10);
    let order: Vec<usize> = heard.iter().map(|(i, _)| *i).collect();
    assert_eq!(
        order,
        (0..6).collect::<Vec<_>>(),
        "each line once, in order"
    );
    for (i, words) in &heard {
        assert_eq!(
            *words,
            firsts(&lines[*i].words),
            "line {i} reported as answered"
        );
    }
}

#[test]
fn a_re_search_no_better_than_the_margin_keeps_pass_one() {
    let before = engine()
        .align_song("a.mp3", Path::new("/a"), song())
        .unwrap();
    // 0.06 beats pass 1's 0.05, but not by the margin.
    let (got, heard) = streamed(&engine_with(Some(0.06)), song());
    assert_eq!(got.runs.rejected, 2, "{:?}", got.runs);
    assert_eq!(got.answer.lines, before.answer.lines);
    assert_eq!(heard.len(), 6);
}

#[test]
fn a_re_search_still_under_the_weak_bar_keeps_pass_one() {
    let before = engine()
        .align_song("a.mp3", Path::new("/a"), song())
        .unwrap();
    // 0.10 beats pass 1's 0.05 by more than the margin, but is still weak.
    let (got, heard) = streamed(&engine_with(Some(0.10)), song());
    assert_eq!(got.runs.rejected, 2, "{:?}", got.runs);
    assert_eq!(got.answer.lines, before.answer.lines);
    assert_eq!(heard.len(), 6);
}

#[test]
fn a_run_longer_than_the_cap_is_skipped() {
    let lines = vec![
        line(1_000, "found here"),
        line(100_000, "lost here"),
        line(300_000, "found there"),
    ];
    let (got, heard) = streamed(&engine_with(Some(0.3)), lines);
    assert_eq!(got.runs.skipped, 1, "{:?}", got.runs);
    assert_eq!(got.answer.lines[1].score, Some(0.05));
    assert_eq!(heard.len(), 3);
}
