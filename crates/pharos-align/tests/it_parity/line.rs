use pharos_align::ctc::Emission;
use pharos_align::line::{align_line, frame_ms};
use pharos_align::tokens::CLASSES;
use pharos_align::AlignError;
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    grid: f32,
    cases: Vec<Case>,
    /// n_samples, frames, start frame, end frame, start ms, end ms.
    ms: Vec<(usize, usize, usize, usize, u64, u64)>,
}

#[derive(Deserialize)]
struct Case {
    tokens: Vec<String>,
    lead_star: bool,
    n_samples: usize,
    frames: usize,
    grid: Vec<i32>,
    ms: Option<Vec<(u64, u64)>>,
    /// model.py's `mean_score`; absent with `error`, `null` when nothing aligned.
    #[serde(default)]
    score: Option<f64>,
    error: Option<String>,
}

/// The shadow's tolerance on a line score (spec 039, R21).
const SCORE_TOLERANCE: f64 = 0.001;

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("../fixtures/line.json")).unwrap()
}

#[test]
fn frame_ms_matches_model_py() {
    for (n, frames, s, e, s_ms, e_ms) in fixture().ms {
        assert_eq!(
            (frame_ms(n, frames, s), frame_ms(n, frames, e)),
            (s_ms, e_ms),
            "n={n} frames={frames} s={s} e={e}"
        );
    }
}

#[test]
fn lines_match_model_py_end_to_end() {
    let f = fixture();
    assert!(
        f.cases.iter().any(|c| c.error.is_some()),
        "fixture lost its repeat-bound case"
    );
    for lead_star in [false, true] {
        assert!(
            f.cases
                .iter()
                .any(|c| c.lead_star == lead_star && c.score.is_some()),
            "fixture lost its scored lead_star={lead_star} case"
        );
        assert!(
            f.cases
                .iter()
                .any(|c| c.lead_star == lead_star && c.ms.as_ref().is_some_and(Vec::is_empty)),
            "fixture lost its empty lead_star={lead_star} case"
        );
    }
    for case in &f.cases {
        let data: Vec<f32> = case.grid.iter().map(|&k| (-k) as f32 / f.grid).collect();
        let em = Emission::new(&data, case.frames, CLASSES).unwrap();
        let got = align_line(&em, case.n_samples, &case.tokens, case.lead_star);
        match (&case.ms, &case.error) {
            (Some(want), _) => {
                let got = got.unwrap();
                let at = format!("{:?} lead_star={}", case.tokens, case.lead_star);
                assert_eq!(&got.times, want, "{at}");
                match (got.score, case.score) {
                    (Some(g), Some(w)) => assert!(
                        (g - w).abs() <= SCORE_TOLERANCE,
                        "{at}: score {g} vs python {w}"
                    ),
                    (g, w) => assert_eq!(g, w, "{at}: score"),
                }
            }
            (None, Some(msg)) => {
                let err = got.unwrap_err();
                assert!(
                    matches!(err, AlignError::TooFewFrames { .. }),
                    "{:?}: {err} vs python {msg}",
                    case.tokens
                );
            }
            (None, None) => panic!("case without ms or error"),
        }
    }
}
