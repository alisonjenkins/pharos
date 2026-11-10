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
        assert_eq!(CpuModel::frames(case.samples), case.frames);
        assert_matches_torch(&ours, case);
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
    }
}

/// The checks the CPU pass meets, for any forward pass: torch's emission
/// within `MAX_ABS`, and the same CTC paths.
fn assert_matches_torch(ours: &[f32], case: &Case) {
    assert_eq!(
        ours.len(),
        case.frames * CLASSES,
        "frames for {} samples",
        case.samples
    );
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
        let mine = Emission::new(ours, case.frames, CLASSES).unwrap();
        for lead in [false, true] {
            assert_eq!(
                align_line(&mine, case.samples, tokens, lead),
                align_line(&theirs, case.samples, tokens, lead),
                "{} samples, {tokens:?}, lead {lead}",
                case.samples
            );
        }
    }
}

/// V1261: the Vulkan pass meets the CPU pass's bar on the same fixtures.
/// Skipped, saying why, on a machine with no usable Vulkan adapter.
#[cfg(feature = "vulkan")]
#[test]
#[ignore = "needs PHAROS_ALIGNER_WEIGHTS and a Vulkan GPU; run with --release --features vulkan"]
fn vulkan_emissions_match_torch_and_give_the_same_ctc_paths() {
    use burn::backend::{WgpuBackend, WgpuDevice, WgpuDeviceKind};
    use pharos_align::forward::vulkan::{self, VulkanModel};
    let fixture: Fixture = serde_json::from_str(include_str!("../fixtures/emission.json")).unwrap();
    let dir = std::env::var("PHAROS_ALIGNER_WEIGHTS").expect("PHAROS_ALIGNER_WEIGHTS");
    let weights = Weights::open(std::path::Path::new(&dir)).unwrap();
    let loaded = std::panic::catch_unwind(|| {
        let device = WgpuDevice::new(WgpuDeviceKind::DiscreteGpu(0)).on(WgpuBackend::Vulkan);
        VulkanModel::load(&weights, device.into())
    });
    let m = match loaded {
        Ok(m) => m.unwrap(),
        Err(_) => {
            eprintln!("SKIPPED: no discrete Vulkan adapter (wgpu panicked creating it)");
            return;
        }
    };
    for case in &fixture.cases {
        let w = normalise(&wave(case.samples, case.seed));
        let logits = m.logits(&w).unwrap();
        assert_matches_torch(&emission(&logits), case);
        // Bucketing pads the window: unpadded, it must give the same paths
        // and logits within float noise.
        let exact = m.logits_padded(&w, w.len()).unwrap();
        assert_matches_torch(&emission(&exact), case);
        let drift = exact
            .iter()
            .zip(&logits)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        eprintln!(
            "samples={} padded to {} drift={drift:e}",
            case.samples,
            vulkan::bucket(case.samples)
        );
        assert!(
            drift < 1e-3,
            "{} samples: padded vs exact {drift}",
            case.samples
        );
    }
    assert!(m
        .logits(&normalise(&wave(MIN_SAMPLES - 1, 9)))
        .unwrap()
        .is_empty());
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

/// Peak RSS a song may reach with the default batch cap (spec 039, V1260).
const MAX_RSS_KIB: u64 = 2_600 * 1024 * 1024 / 1000;
/// Set in the child process `peak_rss_stays_within_budget` starts.
const RSS_CHILD: &str = "PHAROS_ALIGN_RSS_CHILD";

/// A synthetic song's worth of work: seven ~22 s probe windows modelled
/// together under the default cap, then line windows one at a time, the
/// longest a full minute.
fn synthetic_song(aligner: &mut pharos_align::forward::CpuAligner) -> Vec<Vec<(u64, u64)>> {
    use pharos_align::service::Model;
    let tokens: Vec<String> = ["la", "da", "na"].iter().map(|s| (*s).to_owned()).collect();
    let probes: Vec<Vec<f32>> = (0..7)
        .map(|i| wave(352_000 + i * 1_000, i as u64))
        .collect();
    let windows: Vec<(&[f32], &[String])> = probes
        .iter()
        .map(|p| (p.as_slice(), tokens.as_slice()))
        .collect();
    let mut out: Vec<Vec<(u64, u64)>> = aligner
        .align_many(&windows, true)
        .into_iter()
        .map(Result::unwrap)
        .collect();
    for secs in [4, 12, 30, 60] {
        let line = wave(secs * 16_000, secs as u64);
        out.push(aligner.align(&line, &tokens, false).unwrap());
    }
    out
}

fn vm_hwm_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
#[ignore = "needs PHAROS_ALIGNER_WEIGHTS; run with --release"]
fn batched_probes_align_as_they_do_alone() {
    use pharos_align::forward::CpuAligner;
    let mut batched = CpuAligner {
        model: model(),
        batch_samples: pharos_align::config::DEFAULT_BATCH_SAMPLES,
        reason: "forced_cpu".into(),
    };
    let together = synthetic_song(&mut batched);
    batched.batch_samples = 1;
    let alone = synthetic_song(&mut batched);
    assert_eq!(together, alone);
}

#[test]
#[ignore = "needs PHAROS_ALIGNER_WEIGHTS; run with --release"]
fn peak_rss_stays_within_budget() {
    if std::env::var_os(RSS_CHILD).is_some() {
        let mut aligner = pharos_align::forward::CpuAligner {
            model: model(),
            batch_samples: pharos_align::config::DEFAULT_BATCH_SAMPLES,
            reason: "forced_cpu".into(),
        };
        synthetic_song(&mut aligner);
        let peak = vm_hwm_kib();
        println!("peak RSS {peak} KiB");
        assert!(
            peak <= MAX_RSS_KIB,
            "peak RSS {peak} KiB over {MAX_RSS_KIB} KiB"
        );
        return;
    }
    // Its own process, so no other test's memory counts toward the peak.
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "peak_rss_stays_within_budget",
            "--ignored",
            "--nocapture",
        ])
        .env(RSS_CHILD, "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("peak RSS"), "the child ran no song: {text}");
}
