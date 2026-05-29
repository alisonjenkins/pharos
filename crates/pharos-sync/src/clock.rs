//! Per-member rolling clock-offset estimator. NTP-style:
//!
//! ```text
//! offset = ((T2 - T1) + (T3 - T4)) / 2
//! rtt    = (T4 - T1) - (T3 - T2)
//! ```
//!
//! We keep the last N samples and publish the **median** so a single
//! jitter spike does not pollute scheduling. Median > mean for the
//! same reason NTP discards stragglers.

use std::collections::VecDeque;

pub const DEFAULT_WINDOW: usize = 9;

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub offset_ms: i64,
    pub rtt_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ClockOffset {
    window: usize,
    samples: VecDeque<Sample>,
}

impl Default for ClockOffset {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW)
    }
}

impl ClockOffset {
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            samples: VecDeque::with_capacity(window.max(1)),
        }
    }

    pub fn observe(&mut self, t1: u64, t2: u64, t3: u64, t4: u64) {
        let offset_ms = ((t2 as i64 - t1 as i64) + (t3 as i64 - t4 as i64)) / 2;
        let rtt_ms = (t4.saturating_sub(t1)).saturating_sub(t3.saturating_sub(t2));
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample { offset_ms, rtt_ms });
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Median observed offset over the window. `None` if no samples yet.
    pub fn median_offset_ms(&self) -> Option<i64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut xs: Vec<i64> = self.samples.iter().map(|s| s.offset_ms).collect();
        xs.sort_unstable();
        Some(xs[xs.len() / 2])
    }

    /// Maximum RTT observed in the window. Used by the group actor to set
    /// the server lead time when broadcasting a Play (V3 enforcement).
    pub fn max_rtt_ms(&self) -> u64 {
        self.samples.iter().map(|s| s.rtt_ms).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn empty_offset_is_none() {
        let c = ClockOffset::default();
        assert_eq!(c.median_offset_ms(), None);
        assert!(c.is_empty());
    }

    #[test]
    fn single_sample_returns_its_offset() {
        let mut c = ClockOffset::default();
        // T1=0 (client send), T2=110, T3=120 (server send pong),
        // T4=20 (client recv). Offset = ((110-0)+(120-20))/2 = 105
        c.observe(0, 110, 120, 20);
        assert_eq!(c.median_offset_ms(), Some(105));
    }

    #[test]
    fn median_kills_outlier() {
        let mut c = ClockOffset::new(5);
        for _ in 0..4 {
            c.observe(0, 100, 110, 10); // offset 100
        }
        c.observe(0, 100000, 100010, 10); // huge outlier
                                          // 4 samples at 100, 1 at ~100000. Sorted: 100,100,100,100,100000.
                                          // Median index = 2 -> 100.
        assert_eq!(c.median_offset_ms(), Some(100));
    }

    #[test]
    fn window_evicts_oldest() {
        let mut c = ClockOffset::new(3);
        c.observe(0, 100, 110, 10); // 100
        c.observe(0, 200, 210, 10); // 200
        c.observe(0, 300, 310, 10); // 300
        c.observe(0, 400, 410, 10); // 400 — evicts the 100
        assert_eq!(c.len(), 3);
        // Window: 200, 300, 400. Median = 300.
        assert_eq!(c.median_offset_ms(), Some(300));
    }

    #[test]
    fn max_rtt_picks_largest() {
        let mut c = ClockOffset::default();
        c.observe(0, 50, 55, 30); // rtt = 30 - 5 = 25
        c.observe(0, 50, 55, 100); // rtt = 100 - 5 = 95
        c.observe(0, 50, 55, 60); // rtt = 60 - 5 = 55
        assert_eq!(c.max_rtt_ms(), 95);
    }
}
