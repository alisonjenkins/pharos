//! Bandwidth estimation and adaptive bitrate (ABR) selection (spec 031 slice 4, ADR-0071).

/// Fast EWMA half-life in milliseconds so a link drop shows within roughly one segment.
const DEFAULT_FAST_HALF_LIFE_MS: u64 = 3_000;

/// Slow EWMA half-life in milliseconds so brief throughput spikes do not trigger up-switches.
const DEFAULT_SLOW_HALF_LIFE_MS: u64 = 9_000;

/// Minimum segment downloads before emitting a bandwidth estimate.
const DEFAULT_MIN_SAMPLES: u32 = 2;

/// Minimum segment size in bytes; tiny responses time the RTT, not the link.
const DEFAULT_MIN_BYTES: u64 = 16_000;

/// Loads shorter than this were served from a cache or below the timer's
/// resolution, and would read as a link far faster than the real one.
const MIN_LOAD_MS: u64 = 5;

/// Spend at most 80% of the estimated throughput to leave headroom for estimation error.
const DEFAULT_SAFETY_PERMILLE: u64 = 800;

/// Minimum milliseconds between up-switches or after start so quality does not oscillate (FR-011).
const DEFAULT_UP_INTERVAL_MS: u64 = 10_000;

/// Forward buffer threshold in milliseconds below which segment download time governs down-switching.
const DEFAULT_LOW_BUFFER_MS: u64 = 8_000;

/// Initial target bandwidth in bits per second before the estimator has enough samples.
const DEFAULT_START_BPS: u64 = 2_500_000;

/// Denominator for permille scaling factors.
const PERMILLE_SCALE: u128 = 1_000;

/// Conversion factor from `(bytes / ms)` to bits per second (`8 bits/byte * 1000 ms/s`).
const BITS_PER_BYTE_MS_PER_S: u128 = 8_000;

/// Tuning parameters for [`Estimator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EstimatorConfig {
    pub fast_half_life_ms: u64,
    pub slow_half_life_ms: u64,
    pub min_samples: u32,
    pub min_bytes: u64,
}

impl Default for EstimatorConfig {
    fn default() -> Self {
        Self {
            fast_half_life_ms: DEFAULT_FAST_HALF_LIFE_MS,
            slow_half_life_ms: DEFAULT_SLOW_HALF_LIFE_MS,
            min_samples: DEFAULT_MIN_SAMPLES,
            min_bytes: DEFAULT_MIN_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Ewma {
    half_life_ms: u64,
    estimate: f64,
    total_weight: f64,
}

impl Ewma {
    fn new(half_life_ms: u64) -> Self {
        Self {
            half_life_ms,
            estimate: 0.0,
            total_weight: 1.0,
        }
    }

    fn sample(&mut self, sample_bps: u64, media_ms: u64) {
        // f64 has 53 bits of significand, which is plenty for EWMA smoothing over bps and ms.
        #[allow(clippy::cast_precision_loss)]
        let sample = sample_bps as f64;
        let alpha = if self.half_life_ms == 0 {
            0.0
        } else {
            // Duration ratios in milliseconds only need standard floating-point precision for 0.5^x.
            #[allow(clippy::cast_precision_loss)]
            let exponent = (media_ms as f64) / (self.half_life_ms as f64);
            0.5_f64.powf(exponent)
        };
        self.estimate = alpha * self.estimate + (1.0 - alpha) * sample;
        self.total_weight *= alpha;
    }

    fn estimate_bps(&self) -> Option<u64> {
        let denom = 1.0 - self.total_weight;
        if denom <= 0.0 || !self.estimate.is_finite() {
            return None;
        }
        let corrected = (self.estimate / denom).round();
        if !corrected.is_finite() || corrected <= 0.0 {
            return Some(0);
        }
        // Clamped non-negative finite float; Rust saturates values >= 2^64 to u64::MAX.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let bps = corrected.min(u64::MAX as f64) as u64;
        Some(bps)
    }
}

/// Dual-EWMA throughput estimator over segment downloads (T031-20).
#[derive(Debug, Clone)]
pub struct Estimator {
    config: EstimatorConfig,
    fast: Ewma,
    slow: Ewma,
    samples: u32,
}

impl Estimator {
    pub fn new(config: EstimatorConfig) -> Self {
        Self {
            fast: Ewma::new(config.fast_half_life_ms),
            slow: Ewma::new(config.slow_half_life_ms),
            config,
            samples: 0,
        }
    }

    /// One downloaded segment: `bytes` over `load_ms` (first to last byte), holding `media_ms` of media.
    pub fn sample(&mut self, bytes: u64, load_ms: u64, media_ms: u64) {
        if bytes < self.config.min_bytes || media_ms == 0 || load_ms < MIN_LOAD_MS {
            return;
        }
        let clamped_load_ms = u128::from(load_ms);
        let bits_ms = u128::from(bytes).saturating_mul(BITS_PER_BYTE_MS_PER_S);
        let sample_bps = u64::try_from(bits_ms / clamped_load_ms).unwrap_or(u64::MAX);

        self.fast.sample(sample_bps, media_ms);
        self.slow.sample(sample_bps, media_ms);
        self.samples = self.samples.saturating_add(1);
    }

    /// The lower of the fast and slow averages, once `min_samples` samples have been taken.
    pub fn estimate_bps(&self) -> Option<u64> {
        if self.samples < self.config.min_samples {
            return None;
        }
        Some(self.fast.estimate_bps()?.min(self.slow.estimate_bps()?))
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }
}

/// One quality rung in an HLS variant ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rung {
    pub bandwidth_bps: u64,
    pub height: Option<u32>,
}

/// Policy thresholds for [`Abr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbrConfig {
    pub safety_permille: u64,
    pub up_interval_ms: u64,
    pub low_buffer_ms: u64,
    pub default_start_bps: u64,
}

impl Default for AbrConfig {
    fn default() -> Self {
        Self {
            safety_permille: DEFAULT_SAFETY_PERMILLE,
            up_interval_ms: DEFAULT_UP_INTERVAL_MS,
            low_buffer_ms: DEFAULT_LOW_BUFFER_MS,
            default_start_bps: DEFAULT_START_BPS,
        }
    }
}

/// Cause of an [`Abr`] rung selection or switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    Start,
    Bandwidth,
    Buffer,
    Ceiling,
}

impl Reason {
    /// Stable telemetry and log token for this switch reason (FR-013).
    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Bandwidth => "bandwidth",
            Self::Buffer => "buffer",
            Self::Ceiling => "ceiling",
        }
    }
}

/// Rung transition emitted by [`Abr::start`] or [`Abr::decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switch {
    pub from: Option<usize>,
    pub to: usize,
    pub reason: Reason,
}

/// Per-segment observation passed to [`Abr::decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    pub now_ms: u64,
    pub estimate_bps: Option<u64>,
    pub buffer_ms: u64,
    pub segment_ms: u64,
    pub cold_ms: u64,
    pub ceiling_bps: Option<u64>,
}

/// Pure adaptive-bitrate controller over a sorted [`Rung`] ladder (T031-21).
#[derive(Debug, Clone)]
pub struct Abr {
    ladder: Vec<Rung>,
    config: AbrConfig,
    current: Option<usize>,
    last_up_ms: Option<u64>,
}

impl Abr {
    /// Builds an ABR controller, sorting `ladder` ascending by `bandwidth_bps`.
    pub fn new(mut ladder: Vec<Rung>, config: AbrConfig) -> Self {
        ladder.sort_by_key(|r| r.bandwidth_bps);
        Self {
            ladder,
            config,
            current: None,
            last_up_ms: None,
        }
    }

    pub fn ladder(&self) -> &[Rung] {
        &self.ladder
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// Picks the initial rung: the highest allowed rung whose bandwidth fits
    /// `estimate * safety`, or `default_start_bps` when there is no estimate,
    /// falling back to the lowest rung (`0`) when none fits.
    ///
    /// `now_ms` starts the up-switch interval. `None` on an empty ladder,
    /// which has no rung to start on.
    pub fn start(
        &mut self,
        now_ms: u64,
        estimate_bps: Option<u64>,
        ceiling_bps: Option<u64>,
    ) -> Option<Switch> {
        let from = self.current;
        if self.ladder.is_empty() {
            self.current = None;
            return None;
        }

        let max_allowed = self.highest_allowed(ceiling_bps);
        let budget_bps = match estimate_bps {
            Some(est) => usable_bps(est, self.config.safety_permille),
            None => self.config.default_start_bps,
        };
        let to = self.highest_fitting(budget_bps, max_allowed);
        self.current = Some(to);
        self.last_up_ms = Some(now_ms);
        Some(Switch {
            from,
            to,
            reason: Reason::Start,
        })
    }

    /// Evaluates whether to switch rungs before the next video segment fetch.
    /// Returns `None` when staying on the current rung or when the ladder is empty.
    pub fn decide(&mut self, inputs: &Inputs) -> Option<Switch> {
        if self.ladder.is_empty() {
            return None;
        }
        let current = self.current?;
        let max_allowed = self.highest_allowed(inputs.ceiling_bps);

        if current > max_allowed {
            self.current = Some(max_allowed);
            return Some(Switch {
                from: Some(current),
                to: max_allowed,
                reason: Reason::Ceiling,
            });
        }

        let estimate_bps = inputs.estimate_bps?;
        let usable = usable_bps(estimate_bps, self.config.safety_permille);
        let fit = self.highest_fitting(usable, max_allowed);

        if fit < current {
            self.current = Some(fit);
            return Some(Switch {
                from: Some(current),
                to: fit,
                reason: Reason::Bandwidth,
            });
        }

        if current > 0 && inputs.buffer_ms < self.config.low_buffer_ms {
            let cur_bw = self.ladder.get(current)?.bandwidth_bps;
            if download_ms(cur_bw, inputs.segment_ms, estimate_bps) > inputs.buffer_ms {
                let to = self
                    .ladder
                    .get(..current)
                    .and_then(|below| {
                        below.iter().enumerate().rev().find_map(|(idx, r)| {
                            (download_ms(r.bandwidth_bps, inputs.segment_ms, estimate_bps)
                                < inputs.buffer_ms)
                                .then_some(idx)
                        })
                    })
                    .unwrap_or(0);
                self.current = Some(to);
                return Some(Switch {
                    from: Some(current),
                    to,
                    reason: Reason::Buffer,
                });
            }
        }

        if fit > current {
            let interval_ok = self.last_up_ms.map_or(true, |last_ms| {
                inputs.now_ms.saturating_sub(last_ms) >= self.config.up_interval_ms
            });
            let fit_bw = self.ladder.get(fit)?.bandwidth_bps;
            let needed_buffer_ms =
                inputs
                    .cold_ms
                    .saturating_add(download_ms(fit_bw, inputs.segment_ms, estimate_bps));
            if interval_ok && inputs.buffer_ms >= needed_buffer_ms {
                self.current = Some(fit);
                self.last_up_ms = Some(inputs.now_ms);
                return Some(Switch {
                    from: Some(current),
                    to: fit,
                    reason: Reason::Bandwidth,
                });
            }
        }

        None
    }

    fn highest_allowed(&self, ceiling_bps: Option<u64>) -> usize {
        let Some(cap) = ceiling_bps else {
            return self.ladder.len().saturating_sub(1);
        };
        self.ladder
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, r)| (r.bandwidth_bps <= cap).then_some(idx))
            .unwrap_or(0)
    }

    fn highest_fitting(&self, budget_bps: u64, max_allowed: usize) -> usize {
        let end = max_allowed.saturating_add(1).min(self.ladder.len());
        self.ladder
            .get(..end)
            .and_then(|allowed| {
                allowed
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(idx, r)| (r.bandwidth_bps <= budget_bps).then_some(idx))
            })
            .unwrap_or(0)
    }
}

fn usable_bps(estimate_bps: u64, safety_permille: u64) -> u64 {
    let scaled =
        u128::from(estimate_bps).saturating_mul(u128::from(safety_permille)) / PERMILLE_SCALE;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

fn download_ms(bandwidth_bps: u64, segment_ms: u64, estimate_bps: u64) -> u64 {
    let bits = u128::from(bandwidth_bps).saturating_mul(u128::from(segment_ms));
    if bits == 0 {
        return 0;
    }
    let Some(ms) = bits.checked_div(u128::from(estimate_bps)) else {
        return u64::MAX;
    };
    u64::try_from(ms).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    // Panicking on unexpected ABR states is the intended failure mode in tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::{
        download_ms, Abr, AbrConfig, Estimator, EstimatorConfig, Inputs, Reason, Rung, Switch,
    };

    fn sample_ladder() -> Vec<Rung> {
        vec![
            Rung {
                bandwidth_bps: 1_000_000,
                height: Some(480),
            },
            Rung {
                bandwidth_bps: 2_500_000,
                height: Some(720),
            },
            Rung {
                bandwidth_bps: 5_000_000,
                height: Some(1080),
            },
            Rung {
                bandwidth_bps: 10_000_000,
                height: Some(1440),
            },
        ]
    }

    #[test]
    fn reason_labels_are_distinct_and_stable() {
        let all = [
            Reason::Start,
            Reason::Bandwidth,
            Reason::Buffer,
            Reason::Ceiling,
        ];
        let labels: BTreeSet<&'static str> = all.into_iter().map(Reason::label).collect();
        assert_eq!(labels.len(), all.len());
        assert_eq!(Reason::Start.label(), "start");
        assert_eq!(Reason::Bandwidth.label(), "bandwidth");
        assert_eq!(Reason::Buffer.label(), "buffer");
        assert_eq!(Reason::Ceiling.label(), "ceiling");
    }

    #[test]
    fn estimator_ignores_tiny_and_zero_duration_samples_and_waits_for_min_samples() {
        let mut est = Estimator::new(EstimatorConfig::default());
        assert_eq!(est.samples(), 0);
        assert_eq!(est.estimate_bps(), None);

        // Under min_bytes (16_000) is ignored.
        est.sample(15_999, 100, 6_000);
        assert_eq!(est.samples(), 0);
        assert_eq!(est.estimate_bps(), None);

        // Zero media_ms is ignored.
        est.sample(64_000, 100, 0);
        assert_eq!(est.samples(), 0);
        assert_eq!(est.estimate_bps(), None);

        // A load under 5 ms is a cache hit, not the link: 64 kB in 1 ms would read as 512 Mbps.
        est.sample(64_000, 1, 6_000);
        est.sample(64_000, 0, 6_000);
        assert_eq!(est.samples(), 0);

        // First valid sample (500_000 bytes in 1_000 ms = 4_000_000 bps) is below min_samples (2).
        est.sample(500_000, 1_000, 6_000);
        assert_eq!(est.samples(), 1);
        assert_eq!(est.estimate_bps(), None);

        // Second valid sample at the same rate yields 4_000_000 bps after zero-bias correction.
        est.sample(500_000, 1_000, 6_000);
        assert_eq!(est.samples(), 2);
        assert_eq!(est.estimate_bps(), Some(4_000_000));

        // A cached zero-ms load leaves the estimate alone.
        est.sample(20_000, 0, 2_000);
        assert_eq!((est.samples(), est.estimate_bps()), (2, Some(4_000_000)));
    }

    #[test]
    fn estimator_takes_lower_ewma_after_drop_and_recovers_on_slow_ewma_after_rise() {
        // Steady high link (10 Mbps = 1_250_000 bytes / 1_000 ms), then a sudden drop to 1 Mbps.
        let mut drop_est = Estimator::new(EstimatorConfig::default());
        drop_est.sample(1_250_000, 1_000, 3_000);
        drop_est.sample(1_250_000, 1_000, 3_000);
        assert_eq!(drop_est.estimate_bps(), Some(10_000_000));

        drop_est.sample(125_000, 1_000, 3_000);
        let after_drop = drop_est.estimate_bps().unwrap();
        let fast_only = drop_est.fast.estimate_bps().unwrap();
        let slow_only = drop_est.slow.estimate_bps().unwrap();
        assert!(fast_only < slow_only);
        assert_eq!(after_drop, fast_only);

        // Steady low link (1 Mbps), then a sudden jump to 10 Mbps: slow EWMA is lower.
        let mut rise_est = Estimator::new(EstimatorConfig::default());
        rise_est.sample(125_000, 1_000, 3_000);
        rise_est.sample(125_000, 1_000, 3_000);
        assert_eq!(rise_est.estimate_bps(), Some(1_000_000));

        rise_est.sample(1_250_000, 1_000, 3_000);
        let after_rise = rise_est.estimate_bps().unwrap();
        let rise_fast = rise_est.fast.estimate_bps().unwrap();
        let rise_slow = rise_est.slow.estimate_bps().unwrap();
        assert!(rise_slow < rise_fast);
        assert_eq!(after_rise, rise_slow);
    }

    #[test]
    fn start_sorts_ladder_and_picks_by_estimate_default_and_ceiling() {
        let unsorted = vec![
            Rung {
                bandwidth_bps: 5_000_000,
                height: Some(1080),
            },
            Rung {
                bandwidth_bps: 1_000_000,
                height: Some(480),
            },
            Rung {
                bandwidth_bps: 2_500_000,
                height: Some(720),
            },
        ];
        let mut abr = Abr::new(unsorted, AbrConfig::default());
        assert_eq!(
            abr.ladder()
                .iter()
                .map(|r| r.bandwidth_bps)
                .collect::<Vec<_>>(),
            vec![1_000_000, 2_500_000, 5_000_000]
        );

        // No estimate uses default_start_bps (2_500_000) -> rung 1.
        assert_eq!(
            abr.start(0, None, None),
            Some(Switch {
                from: None,
                to: 1,
                reason: Reason::Start,
            })
        );
        assert_eq!(abr.current(), Some(1));

        // Estimate 7_000_000 -> usable 5_600_000 -> rung 2 (5_000_000).
        assert_eq!(
            abr.start(0, Some(7_000_000), None),
            Some(Switch {
                from: Some(1),
                to: 2,
                reason: Reason::Start,
            })
        );

        // Ceiling 2_500_000 caps start at rung 1 despite high estimate.
        assert_eq!(
            abr.start(0, Some(20_000_000), Some(2_500_000)),
            Some(Switch {
                from: Some(2),
                to: 1,
                reason: Reason::Start,
            })
        );

        // Ceiling below lowest rung still allows rung 0.
        assert_eq!(
            abr.start(0, Some(20_000_000), Some(500_000)),
            Some(Switch {
                from: Some(1),
                to: 0,
                reason: Reason::Start,
            })
        );

        // Estimate whose usable is below lowest rung picks rung 0.
        assert_eq!(
            abr.start(0, Some(500_000), None),
            Some(Switch {
                from: Some(0),
                to: 0,
                reason: Reason::Start,
            })
        );

        // An empty ladder has no rung to start on or switch to.
        let mut empty = Abr::new(Vec::new(), AbrConfig::default());
        assert_eq!(empty.start(0, Some(5_000_000), None), None);
        assert_eq!(empty.current(), None);
        assert_eq!(
            empty.decide(&Inputs {
                now_ms: 10_000,
                estimate_bps: Some(5_000_000),
                buffer_ms: 20_000,
                segment_ms: 6_000,
                cold_ms: 2_000,
                ceiling_bps: None,
            }),
            None
        );
    }

    #[test]
    fn decide_rules_and_guards_fire_in_order() {
        let mut abr = Abr::new(sample_ladder(), AbrConfig::default());
        abr.start(0, Some(15_000_000), None);
        assert_eq!(abr.current(), Some(3));

        // Rule 1: Ceiling applies even without an estimate.
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 1_000,
                estimate_bps: None,
                buffer_ms: 30_000,
                segment_ms: 6_000,
                cold_ms: 2_000,
                ceiling_bps: Some(5_000_000),
            }),
            Some(Switch {
                from: Some(3),
                to: 2,
                reason: Reason::Ceiling,
            })
        );

        // Rule 2: No estimate and within ceiling -> stay.
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 2_000,
                estimate_bps: None,
                buffer_ms: 30_000,
                segment_ms: 6_000,
                cold_ms: 2_000,
                ceiling_bps: Some(5_000_000),
            }),
            None
        );

        // Rule 4: Down by bandwidth (usable = 3_200_000 -> fit = 1 < current = 2), not rate-limited.
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 3_000,
                estimate_bps: Some(4_000_000),
                buffer_ms: 30_000,
                segment_ms: 6_000,
                cold_ms: 2_000,
                ceiling_bps: None,
            }),
            Some(Switch {
                from: Some(2),
                to: 1,
                reason: Reason::Bandwidth,
            })
        );

        // Rule 5: Down by buffer when buffer_ms < 8_000 and current (2.5 Mbps) download takes
        // 2_500_000 * 6_000 / 3_200_000 = 4_687 ms > buffer_ms (3_000 ms), while usable is
        // 3_200_000 * 0.8 = 2_560_000 (so fit == 1 == current, Rule 4 does not fire).
        // Rung 0 (1 Mbps) takes 1_000_000 * 6_000 / 3_200_000 = 1_875 ms < 3_000 ms.
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 4_000,
                estimate_bps: Some(3_200_000),
                buffer_ms: 3_000,
                segment_ms: 6_000,
                cold_ms: 2_000,
                ceiling_bps: None,
            }),
            Some(Switch {
                from: Some(1),
                to: 0,
                reason: Reason::Buffer,
            })
        );

        // Rule 6 guard 1: Up-switch blocked before up_interval_ms (10_000 ms since start at 0).
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 9_999,
                estimate_bps: Some(8_000_000),
                buffer_ms: 30_000,
                segment_ms: 6_000,
                cold_ms: 2_000,
                ceiling_bps: None,
            }),
            None
        );

        // Rule 6 guard 2: At now_ms = 10_000, estimate = 8_000_000 (usable 6_400_000 -> fit = 2,
        // 5 Mbps). Download of fit is 5_000_000 * 6_000 / 8_000_000 = 3_750 ms; plus cold_ms
        // 3_000 = 6_750 ms. buffer_ms = 6_749 ms blocks the up-switch.
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 10_000,
                estimate_bps: Some(8_000_000),
                buffer_ms: 6_749,
                segment_ms: 6_000,
                cold_ms: 3_000,
                ceiling_bps: None,
            }),
            None
        );

        // Rule 6 fires once buffer_ms >= 6_750 ms and records now_ms = 10_000.
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 10_000,
                estimate_bps: Some(8_000_000),
                buffer_ms: 6_750,
                segment_ms: 6_000,
                cold_ms: 3_000,
                ceiling_bps: None,
            }),
            Some(Switch {
                from: Some(0),
                to: 2,
                reason: Reason::Bandwidth,
            })
        );

        // Subsequent up-switch to rung 3 is blocked at now_ms = 19_999 (< 10_000 + 10_000),
        // and blocked by ceiling when ceiling_bps = 5_000_000.
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 19_999,
                estimate_bps: Some(20_000_000),
                buffer_ms: 30_000,
                segment_ms: 6_000,
                cold_ms: 2_000,
                ceiling_bps: None,
            }),
            None
        );
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 20_000,
                estimate_bps: Some(20_000_000),
                buffer_ms: 30_000,
                segment_ms: 6_000,
                cold_ms: 2_000,
                ceiling_bps: Some(5_000_000),
            }),
            None
        );
        assert_eq!(
            abr.decide(&Inputs {
                now_ms: 20_000,
                estimate_bps: Some(20_000_000),
                buffer_ms: 30_000,
                segment_ms: 6_000,
                cold_ms: 2_000,
                ceiling_bps: None,
            }),
            Some(Switch {
                from: Some(2),
                to: 3,
                reason: Reason::Bandwidth,
            })
        );
    }

    fn ladder_strategy() -> impl Strategy<Value = Vec<Rung>> {
        prop::collection::vec(100_000u64..=25_000_000u64, 1..=6).prop_map(|bws| {
            bws.into_iter()
                .map(|bandwidth_bps| Rung {
                    bandwidth_bps,
                    height: None,
                })
                .collect()
        })
    }

    #[derive(Debug, Clone)]
    struct StepInput {
        dt_ms: u64,
        estimate_bps: Option<u64>,
        buffer_ms: u64,
        segment_ms: u64,
        cold_ms: u64,
        ceiling_bps: Option<u64>,
    }

    fn step_strategy() -> impl Strategy<Value = StepInput> {
        (
            0u64..=15_000u64,
            prop::option::of(100_000u64..=30_000_000u64),
            0u64..=40_000u64,
            1_000u64..=10_000u64,
            0u64..=8_000u64,
            prop::option::of(50_000u64..=30_000_000u64),
        )
            .prop_map(
                |(dt_ms, estimate_bps, buffer_ms, segment_ms, cold_ms, ceiling_bps)| StepInput {
                    dt_ms,
                    estimate_bps,
                    buffer_ms,
                    segment_ms,
                    cold_ms,
                    ceiling_bps,
                },
            )
    }

    proptest! {
        #[test]
        fn v645_never_picks_a_rung_above_ceiling(
            ladder in ladder_strategy(),
            start_est in prop::option::of(100_000u64..=30_000_000u64),
            start_ceil in prop::option::of(50_000u64..=30_000_000u64),
            steps in prop::collection::vec(step_strategy(), 1..=40),
        ) {
            let mut abr = Abr::new(ladder, AbrConfig::default());
            let start_sw = abr.start(0, start_est, start_ceil).unwrap();
            prop_assert_eq!(abr.current(), Some(start_sw.to));
            if let Some(cap) = start_ceil {
                prop_assert!(
                    start_sw.to == 0 || abr.ladder()[start_sw.to].bandwidth_bps <= cap
                );
            }

            let mut now_ms = 0u64;
            for step in steps {
                now_ms = now_ms.saturating_add(step.dt_ms);
                let _ = abr.decide(&Inputs {
                    now_ms,
                    estimate_bps: step.estimate_bps,
                    buffer_ms: step.buffer_ms,
                    segment_ms: step.segment_ms,
                    cold_ms: step.cold_ms,
                    ceiling_bps: step.ceiling_bps,
                });
                let cur = abr.current().unwrap();
                prop_assert!(cur < abr.ladder().len());
                if let Some(cap) = step.ceiling_bps {
                    prop_assert!(cur == 0 || abr.ladder()[cur].bandwidth_bps <= cap);
                }
            }
        }

        #[test]
        fn v646_up_switch_requires_cold_encode_buffer_margin_and_interval(
            ladder in ladder_strategy(),
            start_est in prop::option::of(100_000u64..=30_000_000u64),
            start_ceil in prop::option::of(50_000u64..=30_000_000u64),
            steps in prop::collection::vec(step_strategy(), 1..=40),
        ) {
            let config = AbrConfig::default();
            let mut abr = Abr::new(ladder, config);
            let _ = abr.start(0, start_est, start_ceil);
            let mut last_up_or_start_ms = 0u64;
            let mut now_ms = 0u64;

            for step in steps {
                now_ms = now_ms.saturating_add(step.dt_ms);
                let before = abr.current().unwrap();
                let inputs = Inputs {
                    now_ms,
                    estimate_bps: step.estimate_bps,
                    buffer_ms: step.buffer_ms,
                    segment_ms: step.segment_ms,
                    cold_ms: step.cold_ms,
                    ceiling_bps: step.ceiling_bps,
                };
                if let Some(sw) = abr.decide(&inputs) {
                    prop_assert_eq!(sw.from, Some(before));
                    if sw.to > before {
                        let est = step.estimate_bps.unwrap();
                        let dl_ms = download_ms(
                            abr.ladder()[sw.to].bandwidth_bps,
                            step.segment_ms,
                            est,
                        );
                        prop_assert!(step.buffer_ms >= step.cold_ms.saturating_add(dl_ms));
                        prop_assert!(now_ms >= last_up_or_start_ms.saturating_add(config.up_interval_ms));
                        last_up_or_start_ms = now_ms;
                    }
                }
            }
        }

        #[test]
        fn v647_buffer_down_switch_fits_in_forward_buffer_and_never_switches_up(
            ladder in ladder_strategy(),
            start_est in prop::option::of(100_000u64..=30_000_000u64),
            start_ceil in prop::option::of(50_000u64..=30_000_000u64),
            steps in prop::collection::vec(step_strategy(), 1..=40),
        ) {
            let mut abr = Abr::new(ladder, AbrConfig::default());
            let _ = abr.start(0, start_est, start_ceil);
            let mut now_ms = 0u64;

            for step in steps {
                now_ms = now_ms.saturating_add(step.dt_ms);
                let before = abr.current().unwrap();
                let inputs = Inputs {
                    now_ms,
                    estimate_bps: step.estimate_bps,
                    buffer_ms: step.buffer_ms,
                    segment_ms: step.segment_ms,
                    cold_ms: step.cold_ms,
                    ceiling_bps: step.ceiling_bps,
                };
                if let Some(sw) = abr.decide(&inputs) {
                    match sw.reason {
                        Reason::Buffer => {
                            prop_assert!(sw.to < before);
                            let est = step.estimate_bps.unwrap();
                            let dl_ms = download_ms(
                                abr.ladder()[sw.to].bandwidth_bps,
                                step.segment_ms,
                                est,
                            );
                            prop_assert!(sw.to == 0 || dl_ms < step.buffer_ms);
                        }
                        Reason::Ceiling => {
                            prop_assert!(sw.to < before);
                        }
                        Reason::Bandwidth | Reason::Start => {}
                    }
                }
            }
        }
    }
}
