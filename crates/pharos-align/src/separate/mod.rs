//! The vocal separator (spec 042 slice 3, ADR-0125): htdemucs pulls the
//! vocals out of the mix before the recogniser listens. Everything around
//! the network is here, as demucs's `apply_model` and `separate` do it, so
//! it is tested with a stand-in network.

pub mod stft;

use stft::{Stft, StftError, BINS};

pub const RATE: u32 = 44_100;
pub const CHANNELS: usize = 2;
/// htdemucs's sources: drums, bass, other, vocals.
pub const SOURCES: usize = 4;
pub const VOCALS: usize = 3;
/// The network's input: its 7.8 s training segment.
pub const SEGMENT: usize = 343_980;
pub const FRAMES: usize = stft::frames(SEGMENT);
/// `apply_model`'s default: each segment shares a quarter with the next.
const STRIDE: usize = SEGMENT * 3 / 4;

/// Left and right channels.
pub type Stereo = [Vec<f32>; CHANNELS];

#[derive(Debug, thiserror::Error)]
pub enum SeparateError {
    #[error(transparent)]
    Stft(#[from] StftError),
    #[error("channels differ in length: {left} and {right} samples")]
    Channels { left: usize, right: usize },
    #[error("the network gave {what} of {len} values, not {want}")]
    Shape {
        what: &'static str,
        len: usize,
        want: usize,
    },
    #[error("running the network on the segment at sample {offset}: {message}")]
    Network { offset: usize, message: String },
    #[error("separator model {path}: {message}")]
    Model {
        path: std::path::PathBuf,
        message: String,
    },
}

/// The network over one segment. `mix` is `[channel][SEGMENT]` and `mag`
/// `[4][BINS][FRAMES]` ([`stft::pack`]); it returns `x`,
/// `[SOURCES][4][BINS][FRAMES]`, and `xt`, `[SOURCES][CHANNELS][SEGMENT]`.
pub type Output = (Vec<f32>, Vec<f32>);

/// The vocals of one segment: the time branch plus the inverse of the
/// spectrogram branch, as `HTDemucs.forward` adds them.
fn segment_vocals(
    stft: &Stft,
    seg: &Stereo,
    net: &mut impl FnMut(&[f32], &[f32]) -> Result<Output, String>,
    offset: usize,
) -> Result<Stereo, SeparateError> {
    let z = [stft.spec(&seg[0])?, stft.spec(&seg[1])?];
    let mag = stft::pack(&z);
    let mix = seg.concat();
    let (x, xt) = net(&mix, &mag).map_err(|message| SeparateError::Network { offset, message })?;
    let plane = BINS * FRAMES;
    let shape = |what, len, want| {
        if len == want {
            Ok(())
        } else {
            Err(SeparateError::Shape { what, len, want })
        }
    };
    shape("x", x.len(), SOURCES * 2 * CHANNELS * plane)?;
    shape("xt", xt.len(), SOURCES * CHANNELS * SEGMENT)?;
    let spec = &x[VOCALS * 2 * CHANNELS * plane..(VOCALS + 1) * 2 * CHANNELS * plane];
    let mut out: Stereo = [Vec::new(), Vec::new()];
    for (c, o) in out.iter_mut().enumerate() {
        let z = stft::unpack(spec, c, plane).ok_or(SeparateError::Shape {
            what: "x's vocals",
            len: spec.len(),
            want: 2 * CHANNELS * plane,
        })?;
        let time = &xt[(VOCALS * CHANNELS + c) * SEGMENT..(VOCALS * CHANNELS + c + 1) * SEGMENT];
        *o = stft
            .ispec(&z, SEGMENT)?
            .iter()
            .zip(time)
            .map(|(s, t)| s + t)
            .collect();
    }
    Ok(out)
}

/// `apply_model`'s weight over a segment: a triangle peaking at 1 mid-way.
fn weight() -> Vec<f32> {
    let half = SEGMENT / 2;
    (1..=half)
        .chain((1..=SEGMENT - half).rev())
        .map(|i| i as f32 / half as f32)
        .collect()
}

/// The mix's mean and standard deviation over both channels' average, as
/// demucs normalises a song before separating it. A silent song keeps a
/// scale of one.
fn stats(mix: &Stereo) -> (f32, f32) {
    let n = mix[0].len();
    let mono = || {
        mix[0]
            .iter()
            .zip(&mix[1])
            .map(|(l, r)| (f64::from(*l) + f64::from(*r)) / 2.0)
    };
    let mean = mono().sum::<f64>() / n as f64;
    let var = mono().map(|v| (v - mean).powi(2)).sum::<f64>() / n.saturating_sub(1).max(1) as f64;
    let std = var.sqrt();
    (mean as f32, if std > 0.0 { std as f32 } else { 1.0 })
}

/// The vocals of a whole song at [`RATE`]: normalised, cut into segments a
/// quarter apart, each through `net`, then blended back with [`weight`].
/// A segment past either end of the song is filled with its neighbouring
/// audio, then silence, as demucs's `TensorChunk.padded` does.
pub fn vocals(
    mix: &Stereo,
    mut net: impl FnMut(&[f32], &[f32]) -> Result<Output, String>,
) -> Result<Stereo, SeparateError> {
    let n = mix[0].len();
    if mix[1].len() != n {
        return Err(SeparateError::Channels {
            left: n,
            right: mix[1].len(),
        });
    }
    if n == 0 {
        return Ok([Vec::new(), Vec::new()]);
    }
    let (mean, std) = stats(mix);
    let normed: Vec<Vec<f32>> = mix
        .iter()
        .map(|c| c.iter().map(|v| (v - mean) / std).collect())
        .collect();
    let stft = Stft::new();
    let weight = weight();
    let mut out: Stereo = [vec![0.0; n], vec![0.0; n]];
    let mut total = vec![0.0f32; n];
    for offset in (0..n).step_by(STRIDE) {
        let len = (n - offset).min(SEGMENT);
        let lead = (SEGMENT - len) / 2;
        let seg: Stereo = [0, 1].map(|c| {
            (0..SEGMENT)
                .map(|i| {
                    (offset + i)
                        .checked_sub(lead)
                        .and_then(|j| normed[c].get(j))
                        .copied()
                        .unwrap_or(0.0)
                })
                .collect()
        });
        let v = segment_vocals(&stft, &seg, &mut net, offset)?;
        for (o, v) in out.iter_mut().zip(&v) {
            for ((o, v), w) in o[offset..offset + len]
                .iter_mut()
                .zip(&v[lead..lead + len])
                .zip(&weight)
            {
                *o += w * v;
            }
        }
        for (t, w) in total[offset..offset + len].iter_mut().zip(&weight) {
            *t += w;
        }
    }
    for o in &mut out {
        for (o, t) in o.iter_mut().zip(&total) {
            *o = *o / t * std + mean;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
