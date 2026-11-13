//! Weak runs (B1402, V1372): lines the per-line pass matched badly, between
//! lines it matched well, are counted per song and on the metric.

use std::path::Path;

use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_align::line::LineSpans;
use pharos_align::plan::Line;
use pharos_align::research::{RunCounts, WEAK_RUNS_TOTAL};
use pharos_align::service::{Decoder, Engine, Model, ModelError};

/// 100 ms per token; a line holding the word "lost" scores 0.05, others 0.5.
struct Scored;

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
}

fn line(start_ms: i64, text: &str) -> Line {
    Line {
        start_ms,
        end_ms: start_ms + 3_000,
        text: text.into(),
    }
}

fn engine() -> Engine {
    let decoder: Decoder = Box::new(|_: &Path, _, _| Ok(vec![0.0; 16]));
    Engine::new(
        Box::new(|| Ok(Box::new(Scored) as Box<dyn Model>)),
        decoder,
        None,
    )
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
