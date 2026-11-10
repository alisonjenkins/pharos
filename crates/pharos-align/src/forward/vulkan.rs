//! The MMS_FA forward pass on a GPU through Burn (wgpu on Vulkan), f32
//! throughout, built op for op from the same `weights.bin` as the CPU pass
//! (spec 039, T039-16). Layout follows `cpu.rs`: activations are
//! `[1, frames, channels]`, turned to `[1, channels, frames]` for the convs.

use burn::tensor::activation::gelu;
use burn::tensor::module::{attention, layer_norm};
use burn::tensor::ops::AttentionModuleOptions;
use burn::tensor::{Device, Tensor, TensorData};

use super::LOGITS;
use crate::weights::{Weights, WeightsError};

const D: usize = 1024;
const HEADS: usize = 16;
const HD: usize = D / HEADS;
const FF: usize = 4096;
const LAYERS: usize = 24;
const FE_C: usize = 512;
/// (in channels, kernel, stride) of each feature-extractor conv.
const FE: [(usize, usize, usize); 7] = [
    (1, 10, 5),
    (FE_C, 3, 2),
    (FE_C, 3, 2),
    (FE_C, 3, 2),
    (FE_C, 3, 2),
    (FE_C, 2, 2),
    (FE_C, 2, 2),
];
const POS_K: usize = 128;
const POS_G: usize = 16;
const EPS: f64 = 1e-5;

struct Norm {
    gamma: Tensor<1>,
    beta: Tensor<1>,
}

impl Norm {
    fn apply<const N: usize>(&self, x: Tensor<N>) -> Tensor<N> {
        layer_norm(x, self.gamma.clone(), Some(self.beta.clone()), EPS)
    }
}

/// `x [1, t, din]` times the transposed weight, plus bias.
struct Linear {
    /// [1, din, dout]
    wt: Tensor<3>,
    b: Tensor<1>,
}

impl Linear {
    fn apply(&self, x: Tensor<3>) -> Tensor<3> {
        x.matmul(self.wt.clone()) + self.b.clone().unsqueeze::<3>()
    }
}

/// A feature-extractor conv as matmuls on channels-last `[1, t, cin]`:
/// frames are grouped `stride` to a row, and block `j` of the kernel (taps
/// `j*stride..`) multiplies the rows shifted by `j`. Burn's strided conv1d
/// ran at about 0.1 TFLOPS here and was 95% of a window's time.
struct FeConv {
    /// Per block, `[1, taps*cin, 512]`, rows ordered tap-major then channel.
    blocks: Vec<Tensor<3>>,
    b: Tensor<1>,
    ln: Norm,
    cin: usize,
    k: usize,
    stride: usize,
}

impl FeConv {
    fn apply(&self, x: Tensor<3>) -> Tensor<3> {
        let [_, t, _] = x.dims();
        let (s, cin) = (self.stride, self.cin);
        let tout = (t - self.k) / s + 1;
        let rows = tout + self.blocks.len() - 1;
        let need = rows * s;
        let x = if t >= need {
            x.narrow(1, 0, need)
        } else {
            let pad = Tensor::zeros([1, need - t, cin], &x.device());
            Tensor::cat(vec![x, pad], 1)
        };
        let grouped = x.reshape([1, rows, s * cin]);
        let mut out = self.b.clone().unsqueeze::<3>();
        for (j, w) in self.blocks.iter().enumerate() {
            let taps = w.dims()[1] / cin;
            let xb = grouped.clone().narrow(1, j, tout).narrow(2, 0, taps * cin);
            out = out + xb.matmul(w.clone());
        }
        out
    }
}

struct Layer {
    ln1: Norm,
    /// q, k and v stacked on the output axis.
    qkv: Linear,
    o: Linear,
    ln2: Norm,
    f1: Linear,
    f2: Linear,
}

/// One second of 16 kHz audio.
const SECOND: usize = 16_000;
/// Padded window lengths in seconds: each at most half again the last past
/// 4 s, so padding costs under 50% of a window's work there.
const BUCKETS_S: [usize; 12] = [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64];
/// Past the last bucket, windows round up to a multiple of this.
const LONG_STEP_S: usize = 32;

/// The padded length a window of `samples` runs at.
pub fn bucket(samples: usize) -> usize {
    BUCKETS_S
        .iter()
        .map(|s| s * SECOND)
        .find(|&b| b >= samples)
        .unwrap_or_else(|| samples.div_ceil(LONG_STEP_S * SECOND) * LONG_STEP_S * SECOND)
}

/// Every bucketed length a window of at most `max_samples` can run at, for
/// warm-up to compile ahead of the first song.
pub fn buckets_up_to(max_samples: usize) -> Vec<usize> {
    let mut out: Vec<usize> = BUCKETS_S.iter().map(|s| s * SECOND).collect();
    let mut b = (BUCKETS_S[BUCKETS_S.len() - 1] / LONG_STEP_S + 1) * LONG_STEP_S * SECOND;
    while b <= bucket(max_samples) {
        out.push(b);
        b += LONG_STEP_S * SECOND;
    }
    out.retain(|&b| b <= bucket(max_samples));
    out
}

pub struct VulkanModel {
    device: Device,
    fe: Vec<FeConv>,
    fp_ln: Norm,
    fp: Linear,
    /// Per tap, `[1, groups, 64 in, 64 out]`.
    pos_taps: Vec<Tensor<4>>,
    pos_b: Tensor<1>,
    layers: Vec<Layer>,
    final_ln: Norm,
    aux: Linear,
}

fn tensor<const N: usize>(data: &[f32], shape: [usize; N], device: &Device) -> Tensor<N> {
    Tensor::from_data(TensorData::new(data.to_vec(), shape), device)
}

impl VulkanModel {
    /// Uploads every weight to `device`, checking each shape as the CPU pass does.
    pub fn load(weights: &Weights, device: Device) -> Result<Self, WeightsError> {
        let dev = &device;
        let t1 = |n: &str, len: usize| -> Result<Tensor<1>, WeightsError> {
            Ok(tensor(weights.get(n, &[len])?, [len], dev))
        };
        let ln = |p: &str, n: usize| -> Result<Norm, WeightsError> {
            Ok(Norm {
                gamma: t1(&format!("{p}.weight"), n)?,
                beta: t1(&format!("{p}.bias"), n)?,
            })
        };
        // Transposed on the host, so the GPU holds a contiguous [din, dout]
        // and each matmul reads it in order rather than through a strided view.
        let host_t = |w: &[f32], dout: usize, din: usize| -> Vec<f32> {
            let mut t = vec![0f32; w.len()];
            for o in 0..dout {
                for i in 0..din {
                    t[i * dout + o] = w[o * din + i];
                }
            }
            t
        };
        let linear = |p: &str, dout: usize, din: usize| -> Result<Linear, WeightsError> {
            let w = weights.get(&format!("{p}.weight"), &[dout, din])?;
            Ok(Linear {
                wt: tensor(&host_t(w, dout, din), [1, din, dout], dev),
                b: t1(&format!("{p}.bias"), dout)?,
            })
        };
        // q, k and v as one [D, 3D] matmul, as the CPU pass fuses them.
        let qkv = |p: &str| -> Result<Linear, WeightsError> {
            let mut wt = vec![0f32; D * 3 * D];
            let mut b = Vec::with_capacity(3 * D);
            for (j, n) in ["q_proj", "k_proj", "v_proj"].iter().enumerate() {
                let w = weights.get(&format!("{p}.attention.{n}.weight"), &[D, D])?;
                for o in 0..D {
                    for i in 0..D {
                        wt[i * 3 * D + j * D + o] = w[o * D + i];
                    }
                }
                b.extend_from_slice(weights.get(&format!("{p}.attention.{n}.bias"), &[D])?);
            }
            Ok(Linear {
                wt: tensor(&wt, [1, D, 3 * D], dev),
                b: tensor(&b, [3 * D], dev),
            })
        };
        let fe = FE
            .iter()
            .enumerate()
            .map(|(i, &(cin, k, stride))| {
                let p = format!("feature_extractor.conv_layers.{i}");
                let w = weights.get(&format!("{p}.conv.weight"), &[FE_C, cin, k])?;
                let blocks = (0..k.div_ceil(stride))
                    .map(|j| {
                        let taps = stride.min(k - j * stride);
                        let mut m = vec![0f32; taps * cin * FE_C];
                        for tap in 0..taps {
                            for c in 0..cin {
                                for o in 0..FE_C {
                                    m[(tap * cin + c) * FE_C + o] =
                                        w[(o * cin + c) * k + j * stride + tap];
                                }
                            }
                        }
                        tensor(&m, [1, taps * cin, FE_C], dev)
                    })
                    .collect();
                Ok(FeConv {
                    blocks,
                    b: t1(&format!("{p}.conv.bias"), FE_C)?,
                    ln: ln(&format!("{p}.layer_norm"), FE_C)?,
                    cin,
                    k,
                    stride,
                })
            })
            .collect::<Result<Vec<_>, WeightsError>>()?;
        let cg = D / POS_G;
        let pos_name = "encoder.transformer.pos_conv_embed.conv.weight";
        let pw = weights.get(pos_name, &[D, cg, POS_K])?;
        let pos_taps = (0..POS_K)
            .map(|k| {
                let mut m = vec![0f32; POS_G * cg * cg];
                for g in 0..POS_G {
                    for c in 0..cg {
                        for o in 0..cg {
                            m[(g * cg + c) * cg + o] = pw[((g * cg + o) * cg + c) * POS_K + k];
                        }
                    }
                }
                tensor(&m, [1, POS_G, cg, cg], dev)
            })
            .collect();
        let layers = (0..LAYERS)
            .map(|l| {
                let p = format!("encoder.transformer.layers.{l}");
                Ok(Layer {
                    ln1: ln(&format!("{p}.layer_norm"), D)?,
                    qkv: qkv(&p)?,
                    o: linear(&format!("{p}.attention.out_proj"), D, D)?,
                    ln2: ln(&format!("{p}.final_layer_norm"), D)?,
                    f1: linear(&format!("{p}.feed_forward.intermediate_dense"), FF, D)?,
                    f2: linear(&format!("{p}.feed_forward.output_dense"), D, FF)?,
                })
            })
            .collect::<Result<Vec<_>, WeightsError>>()?;
        Ok(Self {
            fe,
            fp_ln: ln("encoder.feature_projection.layer_norm", FE_C)?,
            fp: linear("encoder.feature_projection.projection", D, FE_C)?,
            pos_taps,
            pos_b: t1("encoder.transformer.pos_conv_embed.conv.bias", D)?,
            layers,
            final_ln: ln("encoder.transformer.layer_norm", D)?,
            aux: linear("aux", LOGITS, D)?,
            device,
        })
    }

    /// Raw `[frames x 28]` logits for one normalised waveform; empty when it
    /// is shorter than one frame. An error is the device failing to hand the
    /// result back.
    pub fn logits(&self, wave: &[f32]) -> Result<Vec<f32>, String> {
        self.logits_padded(wave, bucket(wave.len()))
    }

    /// [`VulkanModel::logits`] with the window zero-padded to `padded`
    /// samples. Each new length costs seconds of kernel compile and
    /// autotune, so windows run at a few fixed lengths; the padding cannot
    /// reach a real frame (see the comments below), and is cut off the result.
    pub fn logits_padded(&self, wave: &[f32], padded: usize) -> Result<Vec<f32>, String> {
        let real = super::cpu::CpuModel::frames(wave.len());
        if real == 0 {
            return Ok(Vec::new());
        }
        let padded = padded.max(wave.len());
        let mut samples = wave.to_vec();
        samples.resize(padded, 0.0);
        // A frame sees only its own 400 samples, so the extractor and the
        // projection give real frames exactly as without the padding.
        let mut x: Tensor<3> = tensor(&samples, [1, padded, 1], &self.device);
        for c in &self.fe {
            x = gelu(c.ln.apply(c.apply(x)));
        }
        let mut x = self.fp.apply(self.fp_ln.apply(x));
        let t = x.dims()[1];
        // Padded frames become zeros, which is what the positional conv's own
        // zero padding would have shown the real frames next to them.
        let keep: Vec<f32> = (0..t).map(|i| if i < real { 1.0 } else { 0.0 }).collect();
        let keep: Tensor<3> = tensor(&keep, [1, t, 1], &self.device);
        x = x * keep;
        // And no real frame attends to a padded one.
        let masked: Vec<bool> = (0..t).map(|i| i >= real).collect();
        let mask = Tensor::<4, burn::tensor::Bool>::from_data(
            TensorData::new(masked, [1, 1, 1, t]),
            &self.device,
        )
        .expand([1, HEADS, t, t]);

        // Grouped positional conv, pad 64 each side, as one batched matmul
        // over the 16 groups per tap; the even kernel's extra last frame is
        // never computed. Burn's grouped conv1d was most of a window's time.
        let cg = D / POS_G;
        let pad = Tensor::zeros([1, POS_K / 2, D], &self.device);
        let xp = Tensor::cat(vec![pad.clone(), x.clone(), pad], 1)
            .reshape([1, t + POS_K, POS_G, cg])
            .swap_dims(1, 2);
        let mut pos: Tensor<4> = Tensor::zeros([1, POS_G, t, cg], &self.device);
        for (k, w) in self.pos_taps.iter().enumerate() {
            pos = pos + xp.clone().narrow(2, k, t).matmul(w.clone());
        }
        let pos = pos.swap_dims(1, 2).reshape([1, t, D]) + self.pos_b.clone().unsqueeze::<3>();
        x = x + gelu(pos);

        let heads = |y: Tensor<3>| y.reshape([1, t, HEADS, HD]).swap_dims(1, 2);
        for l in &self.layers {
            let h = l.ln1.apply(x.clone());
            let qkv = l.qkv.apply(h);
            let a = attention(
                heads(qkv.clone().narrow(2, 0, D)),
                heads(qkv.clone().narrow(2, D, D)),
                heads(qkv.narrow(2, 2 * D, D)),
                (real < t).then(|| mask.clone()),
                None,
                AttentionModuleOptions::default(),
            );
            x = x + l.o.apply(a.swap_dims(1, 2).reshape([1, t, D]));
            let h = l.ln2.apply(x.clone());
            x = x.clone() + l.f2.apply(gelu(l.f1.apply(h)));
        }
        let logits = self.aux.apply(self.final_ln.apply(x.narrow(1, 0, real)));
        logits
            .into_data()
            .try_to_vec::<f32>()
            .map_err(|e| format!("reading the logits back from the GPU: {e:?}"))
    }
}

/// Keeps compiled kernels and autotune results under `dir` across restarts,
/// so each bucket compiles once per cache rather than once per process.
/// Call before the first GPU device starts; false when something read the
/// kernel settings first, which leaves them per process.
pub fn use_kernel_cache(dir: &std::path::Path) -> bool {
    use cubecl_runtime::config::{cache::CacheConfig, CubeClRuntimeConfig, RuntimeConfig};
    let mut config = CubeClRuntimeConfig::default();
    config.compilation.cache = true;
    config.environment.path = CacheConfig::Directory(dir.to_path_buf());
    CubeClRuntimeConfig::try_set(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_round_up_to_a_few_lengths() {
        assert_eq!(bucket(1), SECOND);
        assert_eq!(bucket(SECOND), SECOND);
        assert_eq!(bucket(SECOND + 1), 2 * SECOND);
        assert_eq!(bucket(5 * SECOND), 6 * SECOND);
        assert_eq!(bucket(64 * SECOND), 64 * SECOND);
        assert_eq!(bucket(64 * SECOND + 1), 96 * SECOND);
        assert_eq!(bucket(100 * SECOND), 128 * SECOND);
        for n in (1..200).map(|s| s * 8_000) {
            assert!(bucket(n) >= n);
            if n > 4 * SECOND && n <= 64 * SECOND {
                assert!(bucket(n) * 2 <= n * 3, "{n} pads to {}", bucket(n));
            }
        }
        assert_eq!(buckets_up_to(30 * SECOND).len(), 10);
        assert_eq!(buckets_up_to(100 * SECOND).last(), Some(&(128 * SECOND)));
    }
}
