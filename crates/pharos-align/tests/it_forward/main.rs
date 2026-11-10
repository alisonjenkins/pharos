//! The CPU forward pass against torchaudio's MMS_FA on the same synthetic
//! waveforms (`tests/fixtures/gen_emissions.py`).
//!
//! Needs the converted weights, so it is ignored by default:
//! `PHAROS_ALIGNER_WEIGHTS=$(nix build .#alignerWeights --print-out-paths --no-link)
//! cargo test --release -p pharos-align --test it_forward -- --ignored`
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_align::ctc::Emission;
use pharos_align::forward::cpu::{CpuModel, MIN_SAMPLES};
use pharos_align::forward::{emission, normalise};
use pharos_align::line::align_line;
use pharos_align::tokens::CLASSES;
use pharos_align::weights::Weights;
use serde::Deserialize;

/// Largest difference allowed from torch's log-probabilities (spec 039, V1257).
const MAX_ABS: f32 = 2e-2;

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    samples: usize,
    seed: u64,
    frames: usize,
    emission: Vec<f32>,
}

/// gen_emissions.py's `wave`, step for step.
fn wave(n: usize, seed: u64) -> Vec<f32> {
    let mut state = seed;
    let tau = 2.0 * std::f64::consts::PI;
    (0..n)
        .map(|i| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let noise = (state >> 40) as f64 / f64::from(1u32 << 24) - 0.5;
            let t = i as f64 / 16_000.0;
            let s = seed as f64;
            (0.3 * (tau * (220.0 + 30.0 * s) * t + 2.0 * (tau * 5.0 * t).sin()).sin()
                + 0.2 * (tau * 660.0 * t).sin()
                + 0.1 * (tau * (1500.0 + 100.0 * s) * t).sin()
                + 0.05 * noise) as f32
        })
        .collect()
}

fn model() -> CpuModel {
    let dir = std::env::var("PHAROS_ALIGNER_WEIGHTS")
        .expect("PHAROS_ALIGNER_WEIGHTS: the alignerWeights output");
    let weights = Weights::open(std::path::Path::new(&dir)).unwrap();
    assert_eq!(weights.len(), 422);
    CpuModel::load(weights, 8).unwrap()
}

#[test]
#[ignore = "needs PHAROS_ALIGNER_WEIGHTS"]
fn emissions_match_torch_and_give_the_same_ctc_paths() {
    let fixture: Fixture = serde_json::from_str(include_str!("../fixtures/emission.json")).unwrap();
    let m = model();
    let waves: Vec<Vec<f32>> = fixture
        .cases
        .iter()
        .map(|c| normalise(&wave(c.samples, c.seed)))
        .collect();
    let refs: Vec<&[f32]> = waves.iter().map(Vec::as_slice).collect();
    // All windows in one batch, then each alone: stacking must not change a window.
    let batched = m.logits(&refs);
    for (i, case) in fixture.cases.iter().enumerate() {
        let alone = m.logits(&refs[i..=i]).remove(0);
        let ours = emission(&batched[i]);
        assert_eq!(
            ours.len(),
            case.frames * CLASSES,
            "frames for {} samples",
            case.samples
        );
        assert_eq!(CpuModel::frames(case.samples), case.frames);
        let worst = ours
            .iter()
            .zip(&case.emission)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(
            worst <= MAX_ABS,
            "{} samples: max abs {worst}",
            case.samples
        );
        let drift = emission(&alone)
            .iter()
            .zip(&ours)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(
            drift < 1e-3,
            "{} samples: batched vs alone {drift}",
            case.samples
        );
        eprintln!(
            "samples={} frames={} max_abs={worst:e}",
            case.samples, case.frames
        );

        for tokens in [
            &["la"][..],
            &["hello", "world"],
            &["oh", "the", "night", "is", "long", "and", "cold"],
        ] {
            let theirs = Emission::new(&case.emission, case.frames, CLASSES).unwrap();
            let mine = Emission::new(&ours, case.frames, CLASSES).unwrap();
            for lead in [false, true] {
                let want = align_line(&theirs, case.samples, tokens, lead);
                let got = align_line(&mine, case.samples, tokens, lead);
                assert_eq!(
                    got, want,
                    "{} samples, {tokens:?}, lead {lead}",
                    case.samples
                );
            }
        }
    }
}

#[test]
#[ignore = "needs PHAROS_ALIGNER_WEIGHTS"]
fn a_window_under_one_frame_gives_no_frames() {
    let m = model();
    let short = normalise(&wave(MIN_SAMPLES - 1, 9));
    let long = normalise(&wave(16_000, 2));
    let out = m.logits(&[&short, &long, &[]]);
    assert!(out[0].is_empty() && out[2].is_empty());
    assert_eq!(out[1].len(), 49 * 28);
}
