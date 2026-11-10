//! The hand-written CPU forward pass: channels-last activations, OpenBLAS
//! sgemm for the convolutions and linears (fused QKV, bias and residual
//! folded into the GEMM), and vectorised GELU, softmax and LayerNorm.

use std::ops::Range;

use rayon::prelude::*;

use super::{math, LOGITS};
use crate::weights::{Weights, WeightsError};

unsafe extern "C" {
    fn cblas_sgemm(
        order: i32,
        ta: i32,
        tb: i32,
        m: i32,
        n: i32,
        k: i32,
        alpha: f32,
        a: *const f32,
        lda: i32,
        b: *const f32,
        ldb: i32,
        beta: f32,
        c: *mut f32,
        ldc: i32,
    );
    fn openblas_set_num_threads(n: i32);
}

const ROW_MAJOR: i32 = 101;
const NO_TRANS: i32 = 111;
const TRANS: i32 = 112;

/// Model width, heads and feed-forward width of the 24-layer wav2vec2 large.
const D: usize = 1024;
const HEADS: usize = 16;
const HD: usize = D / HEADS;
const FF: usize = 4096;
const LAYERS: usize = 24;
/// Feature extractor channels, and each conv's (input channels, kernel, stride).
const FE_C: usize = 512;
const FE: [(usize, usize, usize); 7] = [
    (1, 10, 5),
    (FE_C, 3, 2),
    (FE_C, 3, 2),
    (FE_C, 3, 2),
    (FE_C, 3, 2),
    (FE_C, 2, 2),
    (FE_C, 2, 2),
];
/// Samples the feature extractor needs for one frame.
pub const MIN_SAMPLES: usize = 400;
const POS_K: usize = 128;
const POS_G: usize = 16;
const EPS: f32 = 1e-5;
/// Elementwise work per rayon task.
const CHUNK: usize = 1 << 14;

fn as_i32(v: usize) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// C[m,n] = A[m,k] * op(B) + beta * C, row-major; `bt`: B is stored [n,k].
#[allow(clippy::too_many_arguments)]
fn sgemm(
    m: usize,
    n: usize,
    k: usize,
    a: &[f32],
    lda: usize,
    b: &[f32],
    ldb: usize,
    bt: bool,
    c: &mut [f32],
    ldc: usize,
    beta: f32,
) {
    if m == 0 || n == 0 {
        return;
    }
    let b_rows = if bt { n } else { k };
    let b_cols = if bt { k } else { n };
    assert!(a.len() >= (m - 1) * lda + k, "sgemm A too short");
    assert!(b.len() >= (b_rows - 1) * ldb + b_cols, "sgemm B too short");
    assert!(c.len() >= (m - 1) * ldc + n, "sgemm C too short");
    // SAFETY: the asserts above keep every row BLAS reads or writes inside its slice.
    unsafe {
        cblas_sgemm(
            ROW_MAJOR,
            NO_TRANS,
            if bt { TRANS } else { NO_TRANS },
            as_i32(m),
            as_i32(n),
            as_i32(k),
            1.0,
            a.as_ptr(),
            as_i32(lda),
            b.as_ptr(),
            as_i32(ldb),
            beta,
            c.as_mut_ptr(),
            as_i32(ldc),
        );
    }
}

/// Conv weight [cout, cin, k] reordered to [cout, k, cin] for channels-last input.
fn conv_kc(w: &[f32], cout: usize, cin: usize, k: usize) -> Vec<f32> {
    let mut out = vec![0f32; w.len()];
    for o in 0..cout {
        for c in 0..cin {
            for t in 0..k {
                out[(o * k + t) * cin + c] = w[(o * cin + c) * k + t];
            }
        }
    }
    out
}

type Norm = (Vec<f32>, Vec<f32>);

struct FeConv {
    /// [cout, k, cin]
    w: Vec<f32>,
    b: Vec<f32>,
    ln: Norm,
    cin: usize,
    k: usize,
    s: usize,
}

struct Layer {
    ln1: Norm,
    /// q, k and v stacked: [3D, D].
    qkv_w: Vec<f32>,
    qkv_b: Vec<f32>,
    o_w: Range<usize>,
    o_b: Vec<f32>,
    ln2: Norm,
    f1_w: Range<usize>,
    f1_b: Vec<f32>,
    f2_w: Range<usize>,
    f2_b: Vec<f32>,
}

/// The model, its large matrices left in the mapped file; only fused QKV
/// and the reordered conv weights are copied.
pub struct CpuModel {
    weights: Weights,
    fe: Vec<FeConv>,
    fp_ln: Norm,
    fp_w: Range<usize>,
    fp_b: Vec<f32>,
    /// Per group [64 out, k, 64 in].
    pos_w: Vec<f32>,
    pos_b: Vec<f32>,
    layers: Vec<Layer>,
    final_ln: Norm,
    aux_w: Range<usize>,
    aux_b: Vec<f32>,
    pool: rayon::ThreadPool,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Weights(#[from] WeightsError),
    #[error("building the {threads}-thread pool: {source}")]
    Pool {
        threads: usize,
        source: rayon::ThreadPoolBuildError,
    },
}

#[derive(Default)]
struct Bufs {
    a: Vec<f32>,
    b: Vec<f32>,
    x: Vec<f32>,
    h: Vec<f32>,
    qkv: Vec<f32>,
    o: Vec<f32>,
    f: Vec<f32>,
    col: Vec<f32>,
}

impl CpuModel {
    /// Checks every tensor's shape and builds the derived weights; `threads`
    /// sizes both the rayon pool and OpenBLAS.
    pub fn load(weights: Weights, threads: usize) -> Result<Self, LoadError> {
        let w = &weights;
        let v = |n: &str, shape: &[usize]| w.get(n, shape).map(<[f32]>::to_vec);
        let ln = |p: &str, n: usize| -> Result<Norm, WeightsError> {
            Ok((
                v(&format!("{p}.weight"), &[n])?,
                v(&format!("{p}.bias"), &[n])?,
            ))
        };
        let fe = FE
            .iter()
            .enumerate()
            .map(|(i, &(cin, k, s))| {
                let p = format!("feature_extractor.conv_layers.{i}");
                Ok(FeConv {
                    w: conv_kc(
                        w.get(&format!("{p}.conv.weight"), &[FE_C, cin, k])?,
                        FE_C,
                        cin,
                        k,
                    ),
                    b: v(&format!("{p}.conv.bias"), &[FE_C])?,
                    ln: ln(&format!("{p}.layer_norm"), FE_C)?,
                    cin,
                    k,
                    s,
                })
            })
            .collect::<Result<Vec<_>, WeightsError>>()?;
        let cg = D / POS_G;
        let pw = w.get(
            "encoder.transformer.pos_conv_embed.conv.weight",
            &[D, cg, POS_K],
        )?;
        let mut pos_w = vec![0f32; pw.len()];
        for o in 0..D {
            for c in 0..cg {
                for t in 0..POS_K {
                    pos_w[(o * POS_K + t) * cg + c] = pw[(o * cg + c) * POS_K + t];
                }
            }
        }
        let layers = (0..LAYERS)
            .map(|l| {
                let p = format!("encoder.transformer.layers.{l}");
                let mut qkv_w = Vec::with_capacity(3 * D * D);
                let mut qkv_b = Vec::with_capacity(3 * D);
                for n in ["q_proj", "k_proj", "v_proj"] {
                    qkv_w.extend_from_slice(w.get(&format!("{p}.attention.{n}.weight"), &[D, D])?);
                    qkv_b.extend_from_slice(w.get(&format!("{p}.attention.{n}.bias"), &[D])?);
                }
                Ok(Layer {
                    ln1: ln(&format!("{p}.layer_norm"), D)?,
                    qkv_w,
                    qkv_b,
                    o_w: w.range(&format!("{p}.attention.out_proj.weight"), &[D, D])?,
                    o_b: v(&format!("{p}.attention.out_proj.bias"), &[D])?,
                    ln2: ln(&format!("{p}.final_layer_norm"), D)?,
                    f1_w: w.range(
                        &format!("{p}.feed_forward.intermediate_dense.weight"),
                        &[FF, D],
                    )?,
                    f1_b: v(&format!("{p}.feed_forward.intermediate_dense.bias"), &[FF])?,
                    f2_w: w.range(&format!("{p}.feed_forward.output_dense.weight"), &[D, FF])?,
                    f2_b: v(&format!("{p}.feed_forward.output_dense.bias"), &[D])?,
                })
            })
            .collect::<Result<Vec<_>, WeightsError>>()?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("forward-{i}"))
            .build()
            .map_err(|source| LoadError::Pool { threads, source })?;
        // SAFETY: a plain setter on OpenBLAS's global thread count.
        unsafe { openblas_set_num_threads(as_i32(threads)) };
        Ok(Self {
            fe,
            fp_ln: ln("encoder.feature_projection.layer_norm", FE_C)?,
            fp_w: w.range("encoder.feature_projection.projection.weight", &[D, FE_C])?,
            fp_b: v("encoder.feature_projection.projection.bias", &[D])?,
            pos_w,
            pos_b: v("encoder.transformer.pos_conv_embed.conv.bias", &[D])?,
            layers,
            final_ln: ln("encoder.transformer.layer_norm", D)?,
            aux_w: w.range("aux.weight", &[LOGITS, D])?,
            aux_b: v("aux.bias", &[LOGITS])?,
            pool,
            weights,
        })
    }

    /// Frames the feature extractor makes from `samples` samples.
    pub fn frames(samples: usize) -> usize {
        FE.iter().fold(
            samples,
            |t, &(_, k, s)| if t < k { 0 } else { (t - k) / s + 1 },
        )
    }

    /// Raw `[frames x 28]` logits per waveform, each already normalised
    /// (`forward::normalise`). Windows run together: per-frame ops see all
    /// their rows stacked, so the GEMMs get a larger M, while attention and
    /// the positional conv stay per window. A window shorter than
    /// `MIN_SAMPLES` gives no frames.
    pub fn logits(&self, waves: &[&[f32]]) -> Vec<Vec<f32>> {
        self.pool.install(|| self.forward(waves))
    }

    fn forward(&self, waves: &[&[f32]]) -> Vec<Vec<f32>> {
        let mut bf = Bufs::default();
        let mut x = Vec::new();
        let ts: Vec<usize> = waves
            .iter()
            .map(|w| {
                if w.len() < MIN_SAMPLES {
                    0
                } else {
                    self.features(w, &mut bf, &mut x)
                }
            })
            .collect();
        bf.x = x;
        let segs: Vec<(usize, usize)> = ts
            .iter()
            .scan(0, |o, &t| {
                let s = *o;
                *o += t;
                Some((s, t))
            })
            .collect();
        let n: usize = ts.iter().sum();
        if n == 0 {
            return vec![Vec::new(); waves.len()];
        }
        for &(o, t) in segs.iter().filter(|(_, t)| *t > 0) {
            self.pos_conv(&mut bf.x[o * D..(o + t) * D], t, &mut bf.col, &mut bf.b);
        }
        bf.h.resize(n * D, 0.0);
        bf.qkv.resize(n * 3 * D, 0.0);
        bf.o.resize(n * D, 0.0);
        bf.f.resize(n * FF, 0.0);
        let w = &self.weights;
        for l in &self.layers {
            layernorm(&bf.x, &mut bf.h, D, &l.ln1, false);
            linear(&bf.h, n, D, &l.qkv_w, &l.qkv_b, &mut bf.qkv, false);
            attention(&bf.qkv, &segs, &mut bf.o);
            linear(&bf.o, n, D, w.at(&l.o_w), &l.o_b, &mut bf.x, true);
            layernorm(&bf.x, &mut bf.h, D, &l.ln2, false);
            linear(&bf.h, n, D, w.at(&l.f1_w), &l.f1_b, &mut bf.f, false);
            gelu_inplace(&mut bf.f);
            linear(&bf.f, n, FF, w.at(&l.f2_w), &l.f2_b, &mut bf.x, true);
        }
        layernorm(&bf.x, &mut bf.h, D, &self.final_ln, false);
        let mut logits = vec![0f32; n * LOGITS];
        linear(
            &bf.h,
            n,
            D,
            w.at(&self.aux_w),
            &self.aux_b,
            &mut logits,
            false,
        );
        segs.iter()
            .map(|&(o, t)| logits[o * LOGITS..(o + t) * LOGITS].to_vec())
            .collect()
    }

    /// Conv feature extractor and feature projection for one window, appended
    /// to `x` as [t, D]; returns t.
    fn features(&self, wave: &[f32], bf: &mut Bufs, x: &mut Vec<f32>) -> usize {
        let mut tin = wave.len();
        bf.a.clear();
        bf.a.extend_from_slice(wave);
        for c in &self.fe {
            tin = fe_conv(&bf.a, tin, c, &mut bf.b);
            std::mem::swap(&mut bf.a, &mut bf.b);
            let n = tin * FE_C;
            bf.b.resize(n, 0.0);
            layernorm(&bf.a[..n], &mut bf.b[..n], FE_C, &c.ln, true);
            std::mem::swap(&mut bf.a, &mut bf.b);
        }
        let t = tin;
        bf.h.resize(t * FE_C, 0.0);
        layernorm(
            &bf.a[..t * FE_C],
            &mut bf.h[..t * FE_C],
            FE_C,
            &self.fp_ln,
            false,
        );
        let off = x.len();
        x.resize(off + t * D, 0.0);
        linear(
            &bf.h,
            t,
            FE_C,
            self.weights.at(&self.fp_w),
            &self.fp_b,
            &mut x[off..],
            false,
        );
        t
    }

    /// Grouped positional conv (pad 64, last frame dropped) and GELU, added into x.
    fn pos_conv(&self, x: &mut [f32], t: usize, col: &mut Vec<f32>, tmp: &mut Vec<f32>) {
        let cg = D / POS_G;
        let pad = POS_K / 2;
        let tp = t + 2 * pad;
        let rows = POS_K * cg;
        tmp.clear();
        tmp.resize(t * D, 0.0);
        fill_bias(tmp, &self.pos_b);
        col.resize(t * rows, 0.0);
        for g in 0..POS_G {
            // col[t, k*cg + c] = xpad[t + k, g*cg + c]
            col.par_chunks_mut(rows).enumerate().for_each(|(tt, r)| {
                for k in 0..POS_K {
                    let p = tt + k;
                    let dst = &mut r[k * cg..(k + 1) * cg];
                    if p < pad || p >= tp - pad {
                        dst.fill(0.0);
                    } else {
                        let src = (p - pad) * D + g * cg;
                        dst.copy_from_slice(&x[src..src + cg]);
                    }
                }
            });
            sgemm(
                t,
                cg,
                rows,
                col,
                rows,
                &self.pos_w[g * cg * rows..],
                rows,
                true,
                &mut tmp[g * cg..],
                D,
                1.0,
            );
        }
        x.par_chunks_mut(CHUNK)
            .zip(tmp.par_chunks(CHUNK))
            .for_each(|(a, p)| {
                a.iter_mut().zip(p).for_each(|(e, v)| *e += math::gelu(*v));
            });
    }
}

fn layernorm(x: &[f32], out: &mut [f32], n: usize, (g, b): &Norm, act: bool) {
    out.par_chunks_mut(n * 8)
        .zip(x.par_chunks(n * 8))
        .for_each(|(o, xi)| {
            for (orow, row) in o.chunks_mut(n).zip(xi.chunks(n)) {
                let mean = math::sum(row) / n as f32;
                let var = math::sq_dev(row, mean) / n as f32;
                let inv = 1.0 / (var + EPS).sqrt();
                for i in 0..n {
                    let y = (row[i] - mean) * inv * g[i] + b[i];
                    orow[i] = if act { math::gelu(y) } else { y };
                }
            }
        });
}

fn fill_bias(c: &mut [f32], b: &[f32]) {
    c.par_chunks_mut(b.len()).for_each(|r| r.copy_from_slice(b));
}

fn add_bias(c: &mut [f32], b: &[f32]) {
    c.par_chunks_mut(b.len())
        .for_each(|r| r.iter_mut().zip(b).for_each(|(e, bb)| *e += bb));
}

fn gelu_inplace(x: &mut [f32]) {
    x.par_chunks_mut(CHUNK)
        .for_each(|c| c.iter_mut().for_each(|e| *e = math::gelu(*e)));
}

/// Channels-last strided conv as sgemms over tap groups of at most `s` taps,
/// so each A operand has lda >= k (BLAS rejects overlapping rows); no im2col copy.
fn fe_conv(x: &[f32], tin: usize, c: &FeConv, out: &mut Vec<f32>) -> usize {
    let tout = (tin - c.k) / c.s + 1;
    out.clear();
    out.resize(tout * FE_C, 0.0);
    fill_bias(out, &c.b);
    let kc = c.k * c.cin;
    let mut j = 0;
    while j < c.k {
        let l = c.s.min(c.k - j);
        // B = taps j..j+l of every output channel: rows of length l*cin at stride k*cin.
        sgemm(
            tout,
            FE_C,
            l * c.cin,
            &x[j * c.cin..],
            c.s * c.cin,
            &c.w[j * c.cin..],
            kc,
            true,
            out,
            FE_C,
            1.0,
        );
        j += l;
    }
    tout
}

/// out[t, dout] = x[t, din] * w[dout, din]^T + b, or += it with `accumulate`
/// (the residual folded into the GEMM).
fn linear(
    x: &[f32],
    t: usize,
    din: usize,
    w: &[f32],
    b: &[f32],
    out: &mut [f32],
    accumulate: bool,
) {
    let dout = b.len();
    if accumulate {
        add_bias(&mut out[..t * dout], b);
    } else {
        fill_bias(&mut out[..t * dout], b);
    }
    sgemm(t, dout, din, x, din, w, din, true, out, dout, 1.0);
}

/// Unmasked softmax attention over fused qkv [n, 3D], each window segment
/// attending only to itself; (segment, head) pairs in parallel, each a
/// single-threaded gemm.
fn attention(qkv: &[f32], segs: &[(usize, usize)], out: &mut [f32]) {
    let scale = 1.0 / (HD as f32).sqrt();
    assert!(qkv.len() >= segs.iter().map(|(o, t)| (o + t) * 3 * D).max().unwrap_or(0));
    assert!(out.len() >= segs.iter().map(|(o, t)| (o + t) * D).max().unwrap_or(0));
    let outp = out.as_mut_ptr() as usize;
    let jobs: Vec<(usize, usize, usize)> = segs
        .iter()
        .filter(|(_, t)| *t > 0)
        .flat_map(|&(o, t)| (0..HEADS).map(move |h| (o, t, h)))
        .collect();
    jobs.into_par_iter()
        .for_each_init(Vec::new, |s: &mut Vec<f32>, (o, t, h)| {
            let qkv = &qkv[o * 3 * D..];
            s.resize(t * t, 0.0);
            let q = qkv[h * HD..].as_ptr();
            let k = qkv[D + h * HD..].as_ptr();
            let v = qkv[2 * D + h * HD..].as_ptr();
            let (ti, three_d, d) = (t as isize, (3 * D) as isize, D as isize);
            // SAFETY: q, k and v rows lie in this segment's [t, 3D] block of qkv
            // (asserted above); s holds t*t.
            unsafe {
                gemm::gemm(
                    t,
                    t,
                    HD,
                    s.as_mut_ptr(),
                    1,
                    ti,
                    false,
                    q,
                    1,
                    three_d,
                    k,
                    three_d,
                    1,
                    0.0,
                    scale,
                    false,
                    false,
                    false,
                    gemm::Parallelism::None,
                );
            }
            for r in s.chunks_mut(t) {
                let mx = math::max(r);
                r.iter_mut().for_each(|e| *e = math::exp_neg(*e - mx));
                let inv = 1.0 / math::sum(r);
                r.iter_mut().for_each(|e| *e *= inv);
            }
            let dst = (outp as *mut f32).wrapping_add(o * D + h * HD);
            // SAFETY: each (segment, head) job writes only its own t rows and HD
            // columns of out, so no two jobs write the same element.
            unsafe {
                gemm::gemm(
                    t,
                    HD,
                    t,
                    dst,
                    1,
                    d,
                    false,
                    s.as_ptr(),
                    1,
                    ti,
                    v,
                    1,
                    three_d,
                    0.0,
                    1.0,
                    false,
                    false,
                    false,
                    gemm::Parallelism::None,
                );
            }
        });
}

#[cfg(test)]
mod tests {
    use super::CpuModel;

    #[test]
    fn frames_follow_the_conv_strides() {
        // 20 ms a frame at 16 kHz once the receptive field is covered.
        assert_eq!(CpuModel::frames(399), 0);
        assert_eq!(CpuModel::frames(400), 1);
        assert_eq!(CpuModel::frames(16_000), 49);
        assert_eq!(CpuModel::frames(160_000), 499);
    }
}
