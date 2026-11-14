//! htdemucs's spectrogram and its inverse (`HTDemucs._spec`, `_ispec`), which
//! the exported network leaves out: a Hann window of 4,096 samples every
//! 1,024, scaled by 1/sqrt(4096) both ways, with demucs's reflect padding and
//! frame crop around torch's `stft(center=True)`.

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

pub const N_FFT: usize = 4_096;
pub const HOP: usize = 1_024;
/// Bins kept: the Nyquist bin is dropped, as demucs does.
pub const BINS: usize = N_FFT / 2;
/// demucs pads by 1.5 hops so its kept frames cover the signal evenly.
const PAD: usize = HOP / 2 * 3;
/// torch's `center=True` padding.
const CENTER: usize = N_FFT / 2;
/// Frames torch makes beyond the ones demucs keeps: two each side.
const EDGE: usize = 2;
/// The reflect pads need more samples than this.
pub const MIN_LEN: usize = PAD + HOP;

#[derive(Debug, thiserror::Error)]
pub enum StftError {
    #[error("{len} samples is too short for the spectrogram's padding; it needs over {MIN_LEN}")]
    TooShort { len: usize },
    #[error("spectrum of {len} values is not {BINS} bins x {frames} frames")]
    Shape { len: usize, frames: usize },
    #[error("{what}: {source}")]
    Fft {
        what: &'static str,
        source: realfft::FftError,
    },
}

/// Frames the spectrogram of `len` samples has.
pub fn frames(len: usize) -> usize {
    len.div_ceil(HOP)
}

/// torch's reflect padding: the edge sample is not repeated. Both pads must
/// be shorter than `x`.
fn reflect(x: &[f32], left: usize, right: usize) -> Vec<f32> {
    let n = x.len();
    let mut out = Vec::with_capacity(left + n + right);
    out.extend((1..=left).rev().map(|i| x[i]));
    out.extend_from_slice(x);
    out.extend((0..right).map(|i| x[n - 2 - i]));
    out
}

pub struct Stft {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    window: Vec<f32>,
    /// 1/sqrt(N_FFT), torch's `normalized=True` in both directions.
    scale: f32,
}

impl Default for Stft {
    fn default() -> Self {
        Self::new()
    }
}

impl Stft {
    pub fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        // torch.hann_window is periodic.
        let window = (0..N_FFT)
            .map(|n| {
                (0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / N_FFT as f64).cos()) as f32
            })
            .collect();
        Self {
            forward: planner.plan_fft_forward(N_FFT),
            inverse: planner.plan_fft_inverse(N_FFT),
            window,
            scale: (N_FFT as f32).sqrt().recip(),
        }
    }

    /// `_spec` of one channel: `[bin][frame]`, row-major, [`frames`]`(x.len())` frames.
    pub fn spec(&self, x: &[f32]) -> Result<Vec<Complex32>, StftError> {
        if x.len() <= MIN_LEN {
            return Err(StftError::TooShort { len: x.len() });
        }
        let le = frames(x.len());
        let padded = reflect(x, PAD, PAD + le * HOP - x.len());
        let padded = reflect(&padded, CENTER, CENTER);
        let mut buf = self.forward.make_input_vec();
        let mut bins = self.forward.make_output_vec();
        let mut scratch = self.forward.make_scratch_vec();
        let mut out = vec![Complex32::default(); BINS * le];
        for k in 0..le {
            let start = (k + EDGE) * HOP;
            for ((b, s), w) in buf
                .iter_mut()
                .zip(&padded[start..start + N_FFT])
                .zip(&self.window)
            {
                *b = s * w;
            }
            self.forward
                .process_with_scratch(&mut buf, &mut bins, &mut scratch)
                .map_err(|source| StftError::Fft {
                    what: "forward transform",
                    source,
                })?;
            for (f, c) in bins[..BINS].iter().enumerate() {
                out[f * le + k] = c * self.scale;
            }
        }
        Ok(out)
    }

    /// `_ispec` of one channel: `z` is `[bin][frame]` as [`Self::spec`] gives,
    /// `length` the samples wanted back.
    pub fn ispec(&self, z: &[Complex32], length: usize) -> Result<Vec<f32>, StftError> {
        let le = frames(length);
        if z.len() != BINS * le {
            return Err(StftError::Shape {
                len: z.len(),
                frames: le,
            });
        }
        // torch's frame count with demucs's two empty frames each side; the
        // empty frames add nothing but still count in the window envelope.
        let total = le + 2 * EDGE;
        let span = N_FFT + HOP * (total - 1);
        let mut y = vec![0.0f32; span];
        let mut envelope = vec![0.0f32; span];
        let mut bins = self.inverse.make_input_vec();
        let mut buf = self.inverse.make_output_vec();
        let mut scratch = self.inverse.make_scratch_vec();
        for t in 0..total {
            let start = t * HOP;
            for (e, w) in envelope[start..start + N_FFT].iter_mut().zip(&self.window) {
                *e += w * w;
            }
            if t < EDGE || t >= le + EDGE {
                continue;
            }
            let k = t - EDGE;
            for (f, b) in bins[..BINS].iter_mut().enumerate() {
                *b = z[f * le + k];
            }
            // torch's inverse ignores the imaginary parts of the DC and
            // Nyquist bins; realfft wants them zero.
            bins[0].im = 0.0;
            bins[BINS] = Complex32::default();
            self.inverse
                .process_with_scratch(&mut bins, &mut buf, &mut scratch)
                .map_err(|source| StftError::Fft {
                    what: "inverse transform",
                    source,
                })?;
            for ((o, s), w) in y[start..start + N_FFT]
                .iter_mut()
                .zip(&buf)
                .zip(&self.window)
            {
                *o += s * self.scale * w;
            }
        }
        let first = CENTER + PAD;
        Ok((first..first + length)
            .map(|i| match envelope.get(i) {
                Some(&e) if e > 1e-11 => y[i] / e,
                _ => 0.0,
            })
            .collect())
    }
}

/// htdemucs's `_magnitude` with `cac`: `[channel * 2 + (re, im)][bin][frame]`.
pub fn pack(channels: &[Vec<Complex32>]) -> Vec<f32> {
    let mut out = Vec::with_capacity(channels.iter().map(|c| c.len() * 2).sum());
    for c in channels {
        out.extend(c.iter().map(|z| z.re));
        out.extend(c.iter().map(|z| z.im));
    }
    out
}

/// One channel's spectrum back out of [`pack`]'s layout; `plane` is
/// `BINS * frames` values.
pub fn unpack(packed: &[f32], channel: usize, plane: usize) -> Option<Vec<Complex32>> {
    let re = packed.get(channel * 2 * plane..(channel * 2 + 1) * plane)?;
    let im = packed.get((channel * 2 + 1) * plane..(channel * 2 + 2) * plane)?;
    Some(
        re.iter()
            .zip(im)
            .map(|(&re, &im)| Complex32::new(re, im))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const RATE: f64 = 44_100.0;

    #[derive(serde::Deserialize)]
    struct Fixture {
        length: usize,
        bins: usize,
        frames: usize,
        spec: Vec<f32>,
        inverse: Vec<f32>,
    }

    fn fixture() -> Fixture {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stft.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    /// tests/fixtures/gen_stft.py's `lcg`.
    fn lcg(state: &mut u64) -> f64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (*state >> 40) as f64 / (1u64 << 24) as f64 - 0.5
    }

    /// gen_stft.py's `signal`.
    fn signal(n: usize) -> Vec<f32> {
        let mut state = 7;
        (0..n)
            .map(|i| {
                let noise = lcg(&mut state);
                let t = i as f64 / RATE;
                let tau = 2.0 * std::f64::consts::PI;
                (0.4 * (tau * 1_000.0 * t).sin()
                    + 0.2 * (tau * (200.0 + 4_000.0 * t) * t).sin()
                    + 0.05 * noise) as f32
            })
            .collect()
    }

    /// gen_stft.py's `lcg_spectrum`.
    fn lcg_spectrum(bins: usize, frames: usize) -> Vec<Complex32> {
        let mut state = 11;
        (0..bins * frames)
            .map(|_| {
                let re = lcg(&mut state) as f32;
                Complex32::new(re, lcg(&mut state) as f32)
            })
            .collect()
    }

    fn max_diff(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn the_spectrogram_matches_demucs() {
        let f = fixture();
        assert_eq!((f.bins, f.frames), (BINS, frames(f.length)));
        let z = Stft::new().spec(&signal(f.length)).unwrap();
        let ours: Vec<f32> = z.iter().flat_map(|c| [c.re, c.im]).collect();
        let peak = f.spec.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let d = max_diff(&ours, &f.spec);
        assert!(
            d < 1e-4 * peak,
            "max difference {d} against a peak of {peak}"
        );
    }

    #[test]
    fn the_inverse_matches_demucs() {
        let f = fixture();
        let y = Stft::new()
            .ispec(&lcg_spectrum(f.bins, f.frames), f.length)
            .unwrap();
        let peak = f.inverse.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let d = max_diff(&y, &f.inverse);
        assert!(
            d < 1e-4 * peak,
            "max difference {d} against a peak of {peak}"
        );
    }

    #[test]
    fn a_segment_comes_back_through_the_inverse() {
        let s = Stft::new();
        // The fixture's chirp passes Nyquist this long, so tones only.
        let x: Vec<f32> = (0..343_980)
            .map(|i| {
                let t = i as f64 / RATE;
                (0.4 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
                    + 0.2 * (2.0 * std::f64::consts::PI * 3_000.0 * t).cos()) as f32
            })
            .collect();
        let z = s.spec(&x).unwrap();
        assert_eq!(z.len(), BINS * 336);
        let y = s.ispec(&z, x.len()).unwrap();
        // demucs zeroes the two frames past each end, which still count in
        // the window envelope, so the first and last 1.5 hops come back
        // quieter; overlap-add between segments covers them.
        let edge = PAD;
        let n = x.len();
        let d = max_diff(&x[edge..n - edge], &y[edge..n - edge]);
        assert!(d < 1e-4, "interior differs by {d}");
        assert!(
            (y[0] / x[0]).abs() < 1.0,
            "the first sample came back whole"
        );
    }

    #[test]
    fn a_tone_lands_in_its_bin() {
        // Bin 100 of 4,096 at 44.1 kHz.
        let hz = 100.0 * RATE / N_FFT as f64;
        let x: Vec<f32> = (0..20_000)
            .map(|i| (2.0 * std::f64::consts::PI * hz * i as f64 / RATE).sin() as f32)
            .collect();
        let le = frames(x.len());
        let z = Stft::new().spec(&x).unwrap();
        let k = le / 2;
        let loudest = (0..BINS)
            .max_by(|&a, &b| z[a * le + k].norm().total_cmp(&z[b * le + k].norm()))
            .unwrap();
        assert_eq!(loudest, 100);
    }

    #[test]
    fn short_input_is_refused() {
        assert!(matches!(
            Stft::new().spec(&[0.0; MIN_LEN]),
            Err(StftError::TooShort { len: MIN_LEN })
        ));
        assert!(Stft::new().spec(&[0.0; MIN_LEN + 1]).is_ok());
    }

    #[test]
    fn channels_pack_as_real_then_imaginary_planes() {
        let a = vec![Complex32::new(1.0, 2.0), Complex32::new(3.0, 4.0)];
        let b = vec![Complex32::new(5.0, 6.0), Complex32::new(7.0, 8.0)];
        let p = pack(&[a.clone(), b.clone()]);
        assert_eq!(p, [1.0, 3.0, 2.0, 4.0, 5.0, 7.0, 6.0, 8.0]);
        assert_eq!(unpack(&p, 0, 2).unwrap(), a);
        assert_eq!(unpack(&p, 1, 2).unwrap(), b);
        assert_eq!(unpack(&p, 2, 2), None);
    }
}
