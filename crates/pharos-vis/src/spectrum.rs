//! The FFT: `FFT_SIZE` samples to `BINS` levels in dBFS, as the web's
//! `AnalyserNode` reports them (Blackman window, magnitude over `N`).

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use crate::frame::{quantise_db, quantise_sample, Frame, BINS, FFT_SIZE};

/// The Blackman window's alpha, as the Web Audio specification fixes it.
const BLACKMAN_ALPHA: f32 = 0.16;

pub struct Analyser {
    plan: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl Default for Analyser {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyser {
    pub fn new() -> Self {
        let plan = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        let a0 = (1.0 - BLACKMAN_ALPHA) / 2.0;
        let a2 = BLACKMAN_ALPHA / 2.0;
        let n = FFT_SIZE as f32;
        let window = (0..FFT_SIZE)
            .map(|i| {
                let x = std::f32::consts::TAU * i as f32 / n;
                a0 - 0.5 * x.cos() + a2 * (2.0 * x).cos()
            })
            .collect();
        Self {
            input: plan.make_input_vec(),
            output: plan.make_output_vec(),
            scratch: plan.make_scratch_vec(),
            plan,
            window,
        }
    }

    /// The frame for `samples`, the latest `FFT_SIZE` mono samples (-1 to 1)
    /// with the newest last. A shorter slice is taken as the newest, with
    /// silence before it.
    pub fn frame(&mut self, samples: &[f32]) -> Frame {
        let lead = FFT_SIZE.saturating_sub(samples.len());
        let tail = &samples[samples.len().saturating_sub(FFT_SIZE)..];
        self.input.fill(0.0);
        self.input[lead..].copy_from_slice(tail);
        let wave: Vec<i8> = self.input.iter().map(|&s| quantise_sample(s)).collect();
        for (x, w) in self.input.iter_mut().zip(&self.window) {
            *x *= w;
        }
        // The lengths are the plan's own, so processing cannot fail; a
        // failure would leave a silent frame rather than stale bins.
        let ok = self
            .plan
            .process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch)
            .is_ok();
        let n = FFT_SIZE as f32;
        let bins = self
            .output
            .iter()
            .take(BINS)
            .map(|c| {
                if ok {
                    quantise_db(20.0 * (c.norm() / n).log10())
                } else {
                    crate::frame::SILENT_BIN
                }
            })
            .collect();
        Frame { bins, wave }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{dequantise_db, dequantise_sample};

    fn sine(bin: usize, amp: f32) -> Vec<f32> {
        (0..FFT_SIZE)
            .map(|i| amp * (std::f32::consts::TAU * bin as f32 * i as f32 / FFT_SIZE as f32).sin())
            .collect()
    }

    fn loudest(f: &Frame) -> usize {
        (0..f.bins.len()).min_by_key(|&i| f.bins[i]).unwrap_or(0)
    }

    /// The web analyser puts a full-scale sine at about -13.6 dBFS with a
    /// Blackman window (0.21 of the amplitude).
    #[test]
    fn a_sine_peaks_in_its_bin_at_the_blackman_level() {
        let f = Analyser::new().frame(&sine(100, 1.0));
        assert_eq!(f.bins.len(), BINS);
        assert_eq!(loudest(&f), 100);
        let db = dequantise_db(f.bins[100]);
        assert!((db - -13.56).abs() < 0.6, "peak {db} dBFS");
        assert!(dequantise_db(f.bins[400]) < -70.0, "far bins stay quiet");
    }

    #[test]
    fn half_the_amplitude_is_six_decibels_down() {
        let mut a = Analyser::new();
        let full = dequantise_db(a.frame(&sine(50, 1.0)).bins[50]);
        let half = dequantise_db(a.frame(&sine(50, 0.5)).bins[50]);
        assert!((full - half - 6.02).abs() < 0.6, "{full} vs {half}");
    }

    #[test]
    fn silence_has_no_bins_and_a_flat_waveform() {
        let f = Analyser::new().frame(&vec![0.0; FFT_SIZE]);
        assert!(f.bins.iter().all(|&b| b == crate::frame::SILENT_BIN));
        assert!(f.wave.iter().all(|&s| s == 0));
    }

    #[test]
    fn the_waveform_is_the_input_and_a_short_input_is_the_newest_samples() {
        let f = Analyser::new().frame(&[1.0, -1.0]);
        assert_eq!(f.wave.len(), FFT_SIZE);
        assert_eq!(dequantise_sample(f.wave[FFT_SIZE - 2]), 1.0);
        assert_eq!(dequantise_sample(f.wave[FFT_SIZE - 1]), -1.0);
        assert_eq!(f.wave[0], 0);
    }
}
