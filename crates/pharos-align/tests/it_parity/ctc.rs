use pharos_align::ctc::{align_words, forced_align, merge_tokens, Emission};
use pharos_align::tokens::{label, CLASSES};
use pharos_align::AlignError;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Fixture {
    torchaudio: String,
    dictionary: BTreeMap<String, u32>,
    grid: f32,
    cases: Vec<Case>,
}

/// torchaudio TokenSpan: token, start frame, end frame (exclusive), mean probability.
type Span = (u32, usize, usize, f32);

#[derive(Deserialize)]
struct Case {
    name: String,
    frames: usize,
    classes: usize,
    words: Vec<Vec<u32>>,
    grid: Option<Vec<i32>>,
    emission: Option<Vec<f32>>,
    error: Option<String>,
    path: Option<Vec<u32>>,
    log_scores: Option<Vec<f32>>,
    spans: Option<Vec<Vec<Span>>>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("../fixtures/ctc.json")).unwrap()
}

impl Case {
    fn emission(&self, grid: f32) -> Vec<f32> {
        match (&self.grid, &self.emission) {
            // -k / 64 is exact in f32. Negate the integer, as Python's
            // `-randint() / 64` did: zero stays +0.0, not -0.0.
            (Some(g), _) => g.iter().map(|&k| (-k) as f32 / grid).collect(),
            (None, Some(e)) => e.clone(),
            (None, None) => panic!("{}: no emission", self.name),
        }
    }
}

#[test]
fn dictionary_matches_torchaudio() {
    let f = fixture();
    assert_eq!(f.dictionary.len(), CLASSES);
    for (c, &i) in &f.dictionary {
        let c = c.chars().next().unwrap();
        let want = if c == '-' { None } else { Some(i) };
        assert_eq!(label(c), want, "{c:?}");
    }
}

#[test]
fn paths_match_torchaudio_bit_for_bit() {
    let f = fixture();
    assert!(f.cases.len() > 40);
    for case in &f.cases {
        let data = case.emission(f.grid);
        let em = Emission::new(&data, case.frames, case.classes).unwrap();
        let flat: Vec<u32> = case.words.iter().flatten().copied().collect();
        let got = forced_align(&em, &flat);
        match &case.error {
            Some(msg) => {
                let err = got.unwrap_err();
                assert!(
                    matches!(err, AlignError::TooFewFrames { .. }),
                    "{}: {err} vs torchaudio {msg}",
                    case.name
                );
            }
            None => {
                let got = got.unwrap_or_else(|e| panic!("{}: {e}", case.name));
                assert_eq!(
                    &got.path,
                    case.path.as_ref().unwrap(),
                    "{} (torchaudio {})",
                    case.name,
                    f.torchaudio
                );
                let want: Vec<u32> = case
                    .log_scores
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|s| s.to_bits())
                    .collect();
                let got_bits: Vec<u32> = got.log_scores.iter().map(|s| s.to_bits()).collect();
                assert_eq!(got_bits, want, "{}: log scores", case.name);
            }
        }
    }
}

#[test]
fn spans_match_merge_tokens() {
    let f = fixture();
    for case in f.cases.iter().filter(|c| c.error.is_none()) {
        let data = case.emission(f.grid);
        let em = Emission::new(&data, case.frames, case.classes).unwrap();
        let got = align_words(&em, &case.words).unwrap();
        let want = case.spans.as_ref().unwrap();
        assert_eq!(got.len(), want.len(), "{}", case.name);
        for (gw, ww) in got.iter().zip(want) {
            assert_eq!(gw.len(), ww.len(), "{}", case.name);
            for (g, &(token, start, end, score)) in gw.iter().zip(ww) {
                assert_eq!(
                    (g.token, g.start, g.end),
                    (token, start, end),
                    "{}",
                    case.name
                );
                // torch's exp and mean reduce in another order: equal to a few ulp.
                assert!(
                    (g.score - score).abs() <= 1e-6 * score.abs().max(1e-30),
                    "{}: score {} vs {score}",
                    case.name,
                    g.score
                );
            }
        }
    }
}

#[test]
fn merge_collapses_runs_and_drops_blanks() {
    let a = pharos_align::ctc::Alignment {
        path: vec![0, 1, 1, 0, 1, 2, 2],
        log_scores: vec![0.0; 7],
    };
    let spans: Vec<(u32, usize, usize)> = merge_tokens(&a)
        .iter()
        .map(|s| (s.token, s.start, s.end))
        .collect();
    assert_eq!(spans, vec![(1, 1, 3), (1, 4, 5), (2, 5, 7)]);
}
