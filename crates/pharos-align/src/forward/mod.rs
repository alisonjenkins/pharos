//! The MMS_FA acoustic model: waveform in, per-frame log-probabilities over
//! the 29 labels out, as torchaudio's bundle model computes them.

pub mod cpu;
mod math;

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

#[cfg(test)]
mod tests {
    use super::*;

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
