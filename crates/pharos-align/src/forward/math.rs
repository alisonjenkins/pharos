//! Branch-free f32 kernels the compiler can vectorise.

/// erf via Eigen's rational approximation on [-4, 4] (a few ulp in f32).
#[inline(always)]
pub fn erf(x: f32) -> f32 {
    let x = x.clamp(-4.0, 4.0);
    let x2 = x * x;
    let mut p = x2 * -2.726_142_3e-10 + 2.770_681_4e-08;
    p = x2 * p + -2.101_024e-06;
    p = x2 * p + -5.692_506_4e-05;
    p = x2 * p + -7.349_906_3e-04;
    p = x2 * p + -2.954_600_1e-03;
    p = x2 * p + -1.609_603_3e-02;
    p *= x;
    let mut q = x2 * -1.456_607_2e-05 + -2.133_740_6e-04;
    q = x2 * q + -1.682_827e-03;
    q = x2 * q + -7.373_329_2e-03;
    q = x2 * q + -1.426_474e-02;
    p / q
}

/// Exact-form GELU: x * 0.5 * (1 + erf(x / sqrt 2)).
#[inline(always)]
pub fn gelu(x: f32) -> f32 {
    x * 0.5 * (1.0 + erf(x * std::f32::consts::FRAC_1_SQRT_2))
}

/// exp for x <= 0 (softmax after max subtraction): 2^n * p(r), relative error ~2e-7.
#[inline(always)]
pub fn exp_neg(x: f32) -> f32 {
    let x = x.max(-87.0);
    // Adding 1.5*2^23 rounds to nearest and leaves n in the low mantissa bits;
    // integer-only 2^n keeps the loop vectorisable (a float->int `as` cast does not).
    const MAGIC: f32 = 12_582_912.0;
    let t = x * std::f32::consts::LOG2_E + MAGIC;
    let n = t - MAGIC;
    let r = x - n * 0.693_145_75 - n * 1.428_606_8e-6;
    let mut p = 1.987_569_1e-4_f32;
    p = p * r + 1.398_199_9e-3;
    p = p * r + 8.333_452e-3;
    p = p * r + 4.166_579_6e-2;
    p = p * r + 1.666_666_5e-1;
    p = p * r + 5.0e-1;
    p = p * r * r + r + 1.0;
    f32::from_bits(t.to_bits().wrapping_sub(MAGIC.to_bits()).wrapping_add(127) << 23) * p
}

/// Sum with 8 independent lanes so the reduction vectorises.
#[inline(always)]
pub fn sum(x: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let mut it = x.chunks_exact(8);
    for c in &mut it {
        for i in 0..8 {
            acc[i] += c[i];
        }
    }
    acc.iter().sum::<f32>() + it.remainder().iter().sum::<f32>()
}

#[inline(always)]
pub fn max(x: &[f32]) -> f32 {
    let mut acc = [f32::NEG_INFINITY; 8];
    let mut it = x.chunks_exact(8);
    for c in &mut it {
        for i in 0..8 {
            acc[i] = acc[i].max(c[i]);
        }
    }
    it.remainder()
        .iter()
        .chain(acc.iter())
        .copied()
        .fold(f32::NEG_INFINITY, f32::max)
}

/// Sum of squared deviations from `mean`, 8 lanes.
#[inline(always)]
pub fn sq_dev(x: &[f32], mean: f32) -> f32 {
    let mut acc = [0f32; 8];
    let mut it = x.chunks_exact(8);
    for c in &mut it {
        for i in 0..8 {
            let d = c[i] - mean;
            acc[i] += d * d;
        }
    }
    acc.iter().sum::<f32>()
        + it.remainder()
            .iter()
            .map(|e| (e - mean) * (e - mean))
            .sum::<f32>()
}

#[cfg(test)]
mod tests {
    #[test]
    fn erf_matches_libm() {
        let mut worst = 0f32;
        for i in -6000..=6000 {
            let x = i as f32 / 1000.0;
            let want = libm_erf(x);
            worst = worst.max((super::erf(x) - want).abs());
        }
        assert!(worst < 5e-7, "erf max abs error {worst}");
    }

    #[test]
    fn exp_matches_std() {
        let mut worst = 0f32;
        for i in 0..=87000 {
            let x = -(i as f32) / 1000.0;
            let want = x.exp();
            worst = worst.max(((super::exp_neg(x) - want) / want.max(f32::MIN_POSITIVE)).abs());
        }
        assert!(worst < 1e-6, "exp max rel error {worst}");
    }

    fn libm_erf(x: f32) -> f32 {
        // f64 series reference: erf via std is unavailable, use a high-precision continued sum.
        let x = x as f64;
        let mut sum = 0f64;
        let mut term = x;
        let mut n = 0f64;
        if x.abs() > 4.0 {
            return x.signum() as f32 * 1.0;
        }
        loop {
            let t = term / (2.0 * n + 1.0);
            sum += t;
            if t.abs() < 1e-17 {
                break;
            }
            n += 1.0;
            term *= -x * x / n;
        }
        (sum * 2.0 / std::f64::consts::PI.sqrt()) as f32
    }
}
