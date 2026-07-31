//! Closed-loop admission control for speculative transcode work.
//!
//! `background_alongside_client` — how many speculative encodes may run beside
//! the segment a client is blocked on — was a constant calibrated by hand on one
//! GTX 1070 transcoding one 23 Mbps HEVC source. On a card with more NVENC
//! engines it under-uses the hardware; on a weaker box it is still too many.
//!
//! This replaces it with a closed loop over a hardware-INDEPENDENT invariant: a
//! segment covering N seconds of playback must be produced within
//! `margin_ratio × N`. The same rule admits many concurrent jobs on a fast card
//! and none on a weak one, with nothing to calibrate per machine.
//!
//! Why a loop and not a model: the measured cost of a segment is dominated by
//! decode and I/O, not encode (the same segment costs the same at 360p as at
//! 1080p for a sixteenth of the output), so a model fitted on peer count alone
//! predicts well for its training mix and mispredicts when the mix changes. A
//! loop measures the thing that matters — did the client's segment make its
//! deadline — so a slow NFS morning, a heavier codec and another tenant on the
//! GPU all arrive as the same signal without being modelled separately.
//!
//! Additive increase, multiplicative decrease: recovery is slow and backoff is
//! fast, so the loop errs conservative. State is per device and in memory; it is
//! relearned each boot (see spec 006, "Restart behaviour").

use crate::protocol::DeviceId;
use std::collections::HashMap;

/// How the loop reacts. Every value here is **dimensionless** — it describes the
/// shape of the response, not what the machine can do — which is the whole
/// trade: constants that had to know the hardware are replaced by constants that
/// must not.
#[derive(Debug, Clone)]
pub struct AdmissionConfig {
    /// Fraction of a segment's own playback duration within which it must be
    /// produced. 0.5 ⇒ a 6 s segment must be encoded in ≤ 3 s.
    pub margin_ratio: f64,
    /// Additive increase, applied once per exercised success.
    pub increase_step: f64,
    /// Multiplicative decrease, applied once per contended miss.
    pub decrease_factor: f64,
    /// Never starve prefetch to zero: shed-not-queue means what is refused is
    /// not retried later, so an allowance of 0 would make every segment a cold
    /// miss. This is the value `SchedConfig::background_alongside_client` ships.
    pub floor: f64,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            margin_ratio: 0.5,
            increase_step: 1.0,
            decrease_factor: 0.5,
            floor: 1.0,
        }
    }
}

/// What one finished interactive job said about the device it ran on.
///
/// `ignored` is a first-class outcome, not an absence: a frozen allowance with
/// all-ignored observations means *no signal*, which is a completely different
/// problem from a device that genuinely cannot go faster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Met,
    Missed,
    Ignored,
}

impl Verdict {
    /// Bounded, stable metric label. Renaming one of these breaks alerts
    /// silently.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Met => "met",
            Verdict::Missed => "missed",
            Verdict::Ignored => "ignored",
        }
    }
}

/// One completed job, reduced to what the control law needs.
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    /// Playback seconds this job's output covers. `None` for live/progressive
    /// transcodes, which have no deadline to measure against.
    pub segment_seconds: Option<f64>,
    /// Wall-clock spent encoding — the last dispatch to completion, excluding
    /// queue wait, which is a different problem with a different fix.
    pub encode_seconds: f64,
    /// Speculative jobs already running on the same device when this one was
    /// dispatched. Both guards below are expressed in terms of this.
    pub background_peers: usize,
    /// `false` for a job that failed or was retried: its encode time is the
    /// duration of a bounce, not of an encode.
    pub usable: bool,
}

/// Per-device AIMD state. Pure: no clock, no I/O, no tokio.
#[derive(Debug)]
pub struct AdmissionController {
    cfg: AdmissionConfig,
    /// Absent ⇒ the floor. A device is only entered once it has produced a
    /// usable observation, so a cold controller behaves exactly like the
    /// constant it replaces.
    per_device: HashMap<DeviceId, f64>,
}

impl AdmissionController {
    pub fn new(cfg: AdmissionConfig) -> Self {
        Self {
            cfg,
            per_device: HashMap::new(),
        }
    }

    /// Never fill a device with speculation. `.max(floor)` matters on a
    /// one-permit device, where `capacity - 1` is 0: dropping below the floor
    /// there would silently change today's behaviour on the weakest hardware,
    /// which is the last place a regression should land.
    fn ceiling(&self, capacity: usize) -> f64 {
        (capacity.saturating_sub(1) as f64).max(self.cfg.floor)
    }

    /// The learned value, unrounded — for the gauge. Fractional detail is the
    /// evidence that a multiplicative decrease happened at all (2 → 1.5 reads as
    /// 2 → 1 once floored).
    pub fn raw_allowance(&self, dev: DeviceId, capacity: usize) -> f64 {
        self.per_device
            .get(&dev)
            .copied()
            .unwrap_or(self.cfg.floor)
            .clamp(self.cfg.floor, self.ceiling(capacity))
    }

    /// How many speculative jobs may run beside a client's segment on `dev`.
    ///
    /// **Floored**: an allowance of 2.5 permits two concurrent background jobs,
    /// not three. So a decrease takes effect immediately while the recovery from
    /// 1.5 back to 2 costs one more successful observation — asymmetric in the
    /// conservative direction, which is intended.
    pub fn allowance(&self, dev: DeviceId, capacity: usize) -> usize {
        self.raw_allowance(dev, capacity).floor() as usize
    }

    /// Fold one finished job into `dev`'s allowance and report what it said.
    pub fn observe(&mut self, dev: DeviceId, capacity: usize, obs: Observation) -> Verdict {
        let Some(seg_secs) = obs.segment_seconds else {
            return Verdict::Ignored;
        };
        if !obs.usable || seg_secs <= 0.0 || !obs.encode_seconds.is_finite() {
            return Verdict::Ignored;
        }
        let deadline = seg_secs * self.cfg.margin_ratio;
        let met = obs.encode_seconds <= deadline;

        let floor = self.cfg.floor;
        let ceiling = self.ceiling(capacity);
        let current = self.per_device.get(&dev).copied().unwrap_or(floor);
        let next = if met {
            // Increase only on an EXERCISED observation. A segment that met its
            // deadline while running alone never tested the allowance, so it is
            // no evidence the allowance can safely rise.
            if obs.background_peers >= current.floor() as usize {
                current + self.cfg.increase_step
            } else {
                current
            }
        } else if obs.background_peers > 0 {
            // Decrease only when speculation was actually present. A source that
            // blows the deadline alone is not prefetch's fault and backing off
            // cannot help.
            current * self.cfg.decrease_factor
        } else {
            current
        };
        self.per_device.insert(dev, next.clamp(floor, ceiling));

        if met {
            Verdict::Met
        } else {
            Verdict::Missed
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;

    fn gpu() -> DeviceId {
        DeviceId::hw(HwAccel::Nvenc, 0)
    }

    /// 6 s segments, `margin_ratio` 0.5 ⇒ a 3 s deadline.
    fn met(peers: usize) -> Observation {
        Observation {
            segment_seconds: Some(6.0),
            encode_seconds: 1.0,
            background_peers: peers,
            usable: true,
        }
    }

    fn missed(peers: usize) -> Observation {
        Observation {
            segment_seconds: Some(6.0),
            encode_seconds: 5.0,
            background_peers: peers,
            usable: true,
        }
    }

    #[test]
    fn a_fast_device_climbs_to_the_ceiling_and_stops_there() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        // Capacity 8 ⇒ ceiling 7. Feed exercised successes forever.
        for _ in 0..50 {
            let peers = c.allowance(gpu(), 8);
            assert_eq!(c.observe(gpu(), 8, met(peers)), Verdict::Met);
        }
        assert_eq!(c.allowance(gpu(), 8), 7);
    }

    /// The pathological direction: a device that cannot make the deadline under
    /// any contention must return to the floor and stay there, because the floor
    /// is exactly today's shipped behaviour.
    #[test]
    fn a_slow_device_collapses_to_the_floor_and_stays() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        for _ in 0..20 {
            assert_eq!(c.observe(gpu(), 8, missed(3)), Verdict::Missed);
        }
        assert_eq!(c.allowance(gpu(), 8), 1);
    }

    /// A segment that met its deadline while running ALONE never tested the
    /// allowance, so it is not evidence the allowance can rise. Without this
    /// guard the value inflates on irrelevant successes and then discovers the
    /// truth by hurting a viewer.
    #[test]
    fn an_unexercised_success_does_not_increase_the_allowance() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        for _ in 0..10 {
            assert_eq!(c.observe(gpu(), 8, met(0)), Verdict::Met);
        }
        assert_eq!(c.allowance(gpu(), 8), 1);
    }

    /// A 4K HEVC source that blows the deadline with nothing beside it is not
    /// prefetch's fault and backing off cannot help. Without this guard one
    /// pathological title ratchets every device to the floor permanently.
    #[test]
    fn a_solo_miss_does_not_decrease_the_allowance() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        c.observe(gpu(), 8, met(1));
        c.observe(gpu(), 8, met(2));
        assert_eq!(c.allowance(gpu(), 8), 3);
        assert_eq!(c.observe(gpu(), 8, missed(0)), Verdict::Missed);
        assert_eq!(c.allowance(gpu(), 8), 3);
    }

    /// Clamping at both ends, including the degenerate single-permit device
    /// where `capacity - 1` would otherwise sit BELOW the floor and silently
    /// change today's behaviour.
    #[test]
    fn the_allowance_is_clamped_at_both_ends() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        for _ in 0..50 {
            let peers = c.allowance(gpu(), 3);
            c.observe(gpu(), 3, met(peers));
        }
        assert_eq!(c.allowance(gpu(), 3), 2, "ceiling is capacity - 1");

        let mut single = AdmissionController::new(AdmissionConfig::default());
        for _ in 0..50 {
            single.observe(gpu(), 1, met(5));
        }
        assert_eq!(
            single.allowance(gpu(), 1),
            1,
            "a one-permit device never drops below today's constant"
        );
    }

    /// Live/progressive jobs carry no segment duration and failed or retried
    /// jobs carry a meaningless encode time. Both must leave the state
    /// untouched AND be distinguishable in the metric, because an all-ignored
    /// stream is "no signal", not "a device that cannot go faster".
    #[test]
    fn observations_that_carry_no_signal_change_nothing() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        c.observe(gpu(), 8, met(1));
        assert_eq!(c.allowance(gpu(), 8), 2);

        let live = Observation {
            segment_seconds: None,
            ..met(4)
        };
        assert_eq!(c.observe(gpu(), 8, live), Verdict::Ignored);

        let retried = Observation {
            usable: false,
            ..met(4)
        };
        assert_eq!(c.observe(gpu(), 8, retried), Verdict::Ignored);

        assert_eq!(c.allowance(gpu(), 8), 2);
    }

    /// State is per device: a CPU spill that misses every deadline must not
    /// drag down what the GPU has learned.
    #[test]
    fn each_device_learns_independently() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        c.observe(gpu(), 8, met(1));
        c.observe(gpu(), 8, met(2));
        for _ in 0..5 {
            c.observe(DeviceId::Cpu, 4, missed(2));
        }
        assert_eq!(c.allowance(gpu(), 8), 3);
        assert_eq!(c.allowance(DeviceId::Cpu, 4), 1);
    }

    /// The verdict strings are a dashboard contract.
    #[test]
    fn verdict_labels_are_distinct_and_stable() {
        let all = [Verdict::Met, Verdict::Missed, Verdict::Ignored];
        let labels: Vec<&str> = all.iter().map(|v| v.label()).collect();
        assert_eq!(labels, vec!["met", "missed", "ignored"]);
        let uniq: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(uniq.len(), labels.len());
    }
}
