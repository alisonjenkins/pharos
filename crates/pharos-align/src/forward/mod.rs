//! The MMS_FA acoustic model: waveform in, per-frame log-probabilities over
//! the 29 labels out, as torchaudio's bundle model computes them.

pub mod cpu;
pub mod fallback;
mod math;
#[cfg(feature = "vulkan")]
pub mod vulkan;

use crate::ctc::Emission;
use crate::line::align_line;
use crate::service::{DeviceUsed, Model, ModelError};
use crate::tokens::CLASSES;

/// The model's 28 output labels; the star (`tokens::STAR`) is appended after.
pub const LOGITS: usize = 28;

/// The waveform layer-normalised over the window, as the bundle's model does
/// before the convolutions: f64 mean and variance, eps 1e-5, no affine.
pub fn normalise(wave: &[f32]) -> Vec<f32> {
    let n = wave.len() as f64;
    let mean = wave.iter().map(|&x| f64::from(x)).sum::<f64>() / n;
    let var = wave
        .iter()
        .map(|&x| (f64::from(x) - mean).powi(2))
        .sum::<f64>()
        / n;
    let inv = 1.0 / (var + 1e-5).sqrt();
    wave.iter()
        .map(|&x| ((f64::from(x) - mean) * inv) as f32)
        .collect()
}

/// `[frames x 28]` logits to `[frames x 29]` log-probabilities: log_softmax
/// over the 28, then a zero column for the star, as the bundle appends it.
pub fn emission(logits: &[f32]) -> Vec<f32> {
    let frames = logits.len() / LOGITS;
    let mut out = Vec::with_capacity(frames * CLASSES);
    for row in logits.chunks_exact(LOGITS) {
        let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let sum: f32 = row.iter().map(|&x| (x - max).exp()).sum();
        let log_sum = max + sum.ln();
        out.extend(row.iter().map(|&x| x - log_sum));
        out.push(0.0);
    }
    out
}

/// Consecutive runs of `lens` whose total stays within `cap`, as index
/// ranges; a length over the cap gets a run of its own.
pub fn batches(lens: &[usize], cap: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut total) = (0, 0);
    for (i, &len) in lens.iter().enumerate() {
        if i > start && total + len > cap {
            out.push(start..i);
            (start, total) = (i, 0);
        }
        total += len;
    }
    if start < lens.len() {
        out.push(start..lens.len());
    }
    out
}

/// `service::Model` on the CPU pass: the window normalised, forwarded,
/// turned into log-probabilities and handed to CTC, as `Aligner.align` does.
pub struct CpuAligner {
    pub model: cpu::CpuModel,
    /// Most samples one forward pass holds; activations grow with it.
    pub batch_samples: usize,
    /// Why the CPU runs the pass: R11's start reason, or a GPU fallback's.
    pub reason: String,
}

impl CpuAligner {
    /// One forward pass over `windows`, all non-empty.
    fn forward(&self, windows: &[&[f32]]) -> Vec<Vec<f32>> {
        let waves: Vec<Vec<f32>> = windows.iter().map(|w| normalise(w)).collect();
        let refs: Vec<&[f32]> = waves.iter().map(Vec::as_slice).collect();
        let samples: usize = windows.iter().map(|w| w.len()).sum();
        tracing::info_span!(
            "aligner.forward",
            samples,
            windows = windows.len(),
            aligner.device = "cpu",
            aligner.device_reason = %self.reason
        )
        .in_scope(|| self.model.logits(&refs))
    }
}

/// CTC over one window's logits.
pub(crate) fn spans(
    logits: &[f32],
    samples: usize,
    tokens: &[String],
    lead_star: bool,
) -> Result<Vec<(u64, u64)>, ModelError> {
    let probs = emission(logits);
    let frames = probs.len() / CLASSES;
    tracing::info_span!("aligner.ctc", frames).in_scope(|| {
        let em =
            Emission::new(&probs, frames, CLASSES).map_err(|e| ModelError::Fatal(e.to_string()))?;
        // torchaudio raises for text its window cannot hold: only that line fails.
        align_line(&em, samples, tokens, lead_star).map_err(|e| ModelError::Line(e.to_string()))
    })
}

impl Model for CpuAligner {
    fn device(&self) -> DeviceUsed {
        DeviceUsed {
            device: "cpu",
            reason: self.reason.clone(),
        }
    }

    fn align(
        &mut self,
        pcm: &[f32],
        tokens: &[String],
        lead_star: bool,
    ) -> Result<Vec<(u64, u64)>, ModelError> {
        if pcm.is_empty() || tokens.is_empty() {
            return Ok(Vec::new());
        }
        let logits = self.forward(&[pcm]).pop().unwrap_or_default();
        spans(&logits, pcm.len(), tokens, lead_star)
    }

    fn align_many(
        &mut self,
        windows: &[(&[f32], &[String])],
        lead_star: bool,
    ) -> Vec<Result<Vec<(u64, u64)>, ModelError>> {
        let mut out: Vec<Result<Vec<(u64, u64)>, ModelError>> =
            windows.iter().map(|_| Ok(Vec::new())).collect();
        let live: Vec<usize> = (0..windows.len())
            .filter(|&i| !windows[i].0.is_empty() && !windows[i].1.is_empty())
            .collect();
        let lens: Vec<usize> = live.iter().map(|&i| windows[i].0.len()).collect();
        for run in batches(&lens, self.batch_samples) {
            let ids = &live[run];
            let pcm: Vec<&[f32]> = ids.iter().map(|&i| windows[i].0).collect();
            for (&i, logits) in ids.iter().zip(self.forward(&pcm)) {
                let (pcm, tokens) = windows[i];
                out[i] = spans(&logits, pcm.len(), tokens, lead_star);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_fill_up_to_the_cap_and_a_long_window_runs_alone() {
        assert_eq!(batches(&[], 10), Vec::<std::ops::Range<usize>>::new());
        assert_eq!(batches(&[4, 4, 2, 3], 10), vec![0..3, 3..4]);
        assert_eq!(batches(&[3, 25, 3, 3], 10), vec![0..1, 1..2, 2..4]);
        assert_eq!(batches(&[25], 10), vec![0..1]);
        assert_eq!(batches(&[5, 5, 5], 1_000), vec![0..3]);
    }

    #[test]
    fn normalise_gives_zero_mean_unit_variance() {
        let wave: Vec<f32> = (0..1000)
            .map(|i| (i as f32 * 0.01).sin() * 0.3 + 0.1)
            .collect();
        let out = normalise(&wave);
        let mean: f64 = out.iter().map(|&x| f64::from(x)).sum::<f64>() / 1000.0;
        let var: f64 = out
            .iter()
            .map(|&x| (f64::from(x) - mean).powi(2))
            .sum::<f64>()
            / 1000.0;
        assert!(
            mean.abs() < 1e-6 && (var - 1.0).abs() < 1e-3,
            "{mean} {var}"
        );
    }

    #[test]
    fn emission_rows_are_log_probabilities_plus_a_zero_star() {
        let logits: Vec<f32> = (0..2 * LOGITS).map(|i| (i % 7) as f32 - 3.0).collect();
        let out = emission(&logits);
        assert_eq!(out.len(), 2 * CLASSES);
        for row in out.chunks_exact(CLASSES) {
            let total: f32 = row[..LOGITS].iter().map(|x| x.exp()).sum();
            assert!((total - 1.0).abs() < 1e-5, "{total}");
            assert_eq!(row[LOGITS], 0.0);
        }
    }
}
