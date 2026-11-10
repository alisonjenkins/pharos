//! The MMS_FA forward pass on a GPU through Burn (wgpu on Vulkan), f32
//! throughout, built op for op from the same `weights.bin` as the CPU pass
//! (spec 039, T039-16). Layout follows `cpu.rs`: activations are
//! `[1, frames, channels]`, turned to `[1, channels, frames]` for the convs.

use burn::tensor::activation::gelu;
use burn::tensor::module::{attention, conv1d, layer_norm};
use burn::tensor::ops::{AttentionModuleOptions, ConvOptions};
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

struct FeConv {
    w: Tensor<3>,
    b: Tensor<1>,
    ln: Norm,
    stride: usize,
}

struct Layer {
    ln1: Norm,
    q: Linear,
    k: Linear,
    v: Linear,
    o: Linear,
    ln2: Norm,
    f1: Linear,
    f2: Linear,
}

pub struct VulkanModel {
    device: Device,
    fe: Vec<FeConv>,
    fp_ln: Norm,
    fp: Linear,
    pos_w: Tensor<3>,
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
        let linear = |p: &str, dout: usize, din: usize| -> Result<Linear, WeightsError> {
            let w: Tensor<2> = tensor(
                weights.get(&format!("{p}.weight"), &[dout, din])?,
                [dout, din],
                dev,
            );
            Ok(Linear {
                wt: w.transpose().unsqueeze::<3>(),
                b: t1(&format!("{p}.bias"), dout)?,
            })
        };
        let fe = FE
            .iter()
            .enumerate()
            .map(|(i, &(cin, k, stride))| {
                let p = format!("feature_extractor.conv_layers.{i}");
                Ok(FeConv {
                    w: tensor(
                        weights.get(&format!("{p}.conv.weight"), &[FE_C, cin, k])?,
                        [FE_C, cin, k],
                        dev,
                    ),
                    b: t1(&format!("{p}.conv.bias"), FE_C)?,
                    ln: ln(&format!("{p}.layer_norm"), FE_C)?,
                    stride,
                })
            })
            .collect::<Result<Vec<_>, WeightsError>>()?;
        let cg = D / POS_G;
        let pos_name = "encoder.transformer.pos_conv_embed.conv.weight";
        let pos_w = tensor(weights.get(pos_name, &[D, cg, POS_K])?, [D, cg, POS_K], dev);
        let layers = (0..LAYERS)
            .map(|l| {
                let p = format!("encoder.transformer.layers.{l}");
                Ok(Layer {
                    ln1: ln(&format!("{p}.layer_norm"), D)?,
                    q: linear(&format!("{p}.attention.q_proj"), D, D)?,
                    k: linear(&format!("{p}.attention.k_proj"), D, D)?,
                    v: linear(&format!("{p}.attention.v_proj"), D, D)?,
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
            pos_w,
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
        if super::cpu::CpuModel::frames(wave.len()) == 0 {
            return Ok(Vec::new());
        }
        let mut x: Tensor<3> = tensor(wave, [1, 1, wave.len()], &self.device);
        for c in &self.fe {
            x = conv1d(
                x,
                c.w.clone(),
                Some(c.b.clone()),
                ConvOptions::new([c.stride], [0], [1], 1),
            );
            x = gelu(c.ln.apply(x.swap_dims(1, 2))).swap_dims(1, 2);
        }
        let mut x = self.fp.apply(self.fp_ln.apply(x.swap_dims(1, 2)));
        let t = x.dims()[1];

        // Grouped positional conv, pad 64 each side; the even kernel makes one
        // frame too many, and the last is dropped.
        let pos = conv1d(
            x.clone().swap_dims(1, 2),
            self.pos_w.clone(),
            Some(self.pos_b.clone()),
            ConvOptions::new([1], [POS_K / 2], [1], POS_G),
        )
        .narrow(2, 0, t);
        x = x + gelu(pos).swap_dims(1, 2);

        let heads = |y: Tensor<3>| y.reshape([1, t, HEADS, HD]).swap_dims(1, 2);
        for l in &self.layers {
            let h = l.ln1.apply(x.clone());
            let a = attention(
                heads(l.q.apply(h.clone())),
                heads(l.k.apply(h.clone())),
                heads(l.v.apply(h)),
                None,
                None,
                AttentionModuleOptions::default(),
            );
            x = x + l.o.apply(a.swap_dims(1, 2).reshape([1, t, D]));
            let h = l.ln2.apply(x.clone());
            x = x.clone() + l.f2.apply(gelu(l.f1.apply(h)));
        }
        let logits = self.aux.apply(self.final_ln.apply(x));
        logits
            .into_data()
            .try_to_vec::<f32>()
            .map_err(|e| format!("reading the logits back from the GPU: {e:?}"))
    }
}
