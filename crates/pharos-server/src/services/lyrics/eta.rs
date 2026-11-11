//! How long until a queued song's word times are ready.
//!
//! The aligner's cost is learned from its own jobs: wall time per second of
//! audio, as a moving average. Nothing is stored; after a restart there is no
//! history and no estimate. Every call takes the time as `now_ms` (any
//! monotonic origin, as long as it is the same one throughout) so the maths
//! needs no clock.

use std::collections::VecDeque;

/// Weight of the newest job in the average: high enough to follow a slower
/// or faster aligner (GPU contention included) within a few songs, low enough
/// that one odd file does not swing the estimate.
const ALPHA: f64 = 0.3;

#[derive(Debug)]
struct Running {
    id: u64,
    started_ms: u64,
    expected_ms: f64,
}

/// The queue, the running job and the learned cost of alignment.
#[derive(Debug, Default)]
pub struct Eta {
    /// Wall milliseconds per second of audio.
    rate: Option<f64>,
    running: Option<Running>,
    /// Waiting jobs, next to run first, with each song's length.
    queue: VecDeque<(u64, u64)>,
}

impl Eta {
    fn cost_ms(rate: f64, duration_ms: u64) -> f64 {
        rate * duration_ms as f64 / 1000.0
    }

    /// A job joins the back of the queue.
    pub fn enqueue(&mut self, id: u64, duration_ms: u64) {
        self.queue.push_back((id, duration_ms));
    }

    /// A job that could not be queued, or is no longer wanted.
    pub fn forget(&mut self, id: u64) {
        self.queue.retain(|(queued, _)| *queued != id);
    }

    /// The worker takes `id` off the queue and begins it.
    pub fn start(&mut self, id: u64, now_ms: u64) {
        let duration_ms = self
            .queue
            .iter()
            .position(|(queued, _)| *queued == id)
            .and_then(|at| self.queue.remove(at))
            .map_or(0, |(_, d)| d);
        let expected_ms = self.rate.map_or(0.0, |r| Self::cost_ms(r, duration_ms));
        self.running = Some(Running {
            id,
            started_ms: now_ms,
            expected_ms,
        });
    }

    /// The running job succeeded: learn from how long it took.
    pub fn finish(&mut self, id: u64, now_ms: u64, duration_ms: u64) {
        let Some(run) = self.running.take_if(|r| r.id == id) else {
            return;
        };
        if duration_ms == 0 {
            return;
        }
        let elapsed = now_ms.saturating_sub(run.started_ms) as f64;
        let sample = elapsed * 1000.0 / duration_ms as f64;
        self.rate = Some(match self.rate {
            Some(rate) => rate + ALPHA * (sample - rate),
            None => sample,
        });
    }

    /// The running job failed. A failure's duration says nothing about the
    /// cost of a success, so the rate stays as it was.
    pub fn abort(&mut self, id: u64) {
        self.running.take_if(|r| r.id == id);
    }

    /// Milliseconds until `id` is done, or `None` with no history yet, an
    /// unknown song length, or an `id` neither queued nor running.
    pub fn eta_ms(&self, id: u64, now_ms: u64) -> Option<u64> {
        let rate = self.rate?;
        let mut total = 0.0;
        if let Some(run) = &self.running {
            let left = (run.expected_ms - now_ms.saturating_sub(run.started_ms) as f64).max(0.0);
            if run.id == id {
                return Some(left as u64);
            }
            total += left;
        }
        for (queued, duration_ms) in &self.queue {
            total += Self::cost_ms(rate, *duration_ms);
            if *queued == id {
                return (*duration_ms > 0).then_some(total as u64);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// An estimator that has learned 500 ms of wall time per audio second.
    fn learned() -> Eta {
        let mut eta = Eta::default();
        eta.enqueue(1, 10_000);
        eta.start(1, 0);
        eta.finish(1, 5_000, 10_000);
        eta
    }

    #[test]
    fn there_is_no_estimate_before_any_job_has_finished() {
        let mut eta = Eta::default();
        eta.enqueue(1, 200_000);
        assert_eq!(eta.eta_ms(1, 0), None);
        eta.start(1, 0);
        assert_eq!(eta.eta_ms(1, 1_000), None);
    }

    #[test]
    fn the_first_job_sets_the_rate_and_later_ones_are_averaged_in() {
        let mut eta = learned();
        assert_eq!(eta.rate, Some(500.0));
        eta.enqueue(2, 10_000);
        eta.start(2, 6_000);
        eta.finish(2, 16_000, 10_000);
        // 500 + 0.3 * (1000 - 500)
        assert!((eta.rate.unwrap() - 650.0).abs() < 1e-9);
    }

    #[test]
    fn a_queued_song_waits_for_every_song_ahead_of_it_and_itself() {
        let mut eta = learned();
        eta.enqueue(2, 20_000);
        eta.enqueue(3, 40_000);
        eta.enqueue(4, 10_000);
        assert_eq!(eta.eta_ms(2, 1_000), Some(10_000));
        assert_eq!(eta.eta_ms(3, 1_000), Some(30_000));
        assert_eq!(eta.eta_ms(4, 1_000), Some(35_000));
        assert_eq!(eta.eta_ms(9, 1_000), None);
    }

    #[test]
    fn a_running_job_counts_only_what_is_left_of_it() {
        let mut eta = learned();
        eta.enqueue(2, 20_000);
        eta.enqueue(3, 10_000);
        eta.start(2, 6_000);
        // 2 expects 10 s and began at 6 s; at 9 s 7 s remain.
        assert_eq!(eta.eta_ms(2, 9_000), Some(7_000));
        assert_eq!(eta.eta_ms(3, 9_000), Some(12_000));
    }

    #[test]
    fn a_job_past_its_expected_time_has_zero_left_never_less() {
        let mut eta = learned();
        eta.enqueue(2, 10_000);
        eta.enqueue(3, 10_000);
        eta.start(2, 6_000);
        assert_eq!(eta.eta_ms(2, 60_000), Some(0));
        assert_eq!(eta.eta_ms(3, 60_000), Some(5_000));
    }

    #[test]
    fn a_failed_job_leaves_the_rate_alone() {
        let mut eta = learned();
        eta.enqueue(2, 10_000);
        eta.start(2, 6_000);
        eta.abort(2);
        assert_eq!(eta.rate, Some(500.0));
        assert!(eta.running.is_none());
    }

    #[test]
    fn a_song_of_unknown_length_has_no_estimate() {
        let mut eta = learned();
        eta.enqueue(2, 0);
        assert_eq!(eta.eta_ms(2, 6_000), None);
    }

    #[test]
    fn a_forgotten_job_is_no_longer_waited_for() {
        let mut eta = learned();
        eta.enqueue(2, 10_000);
        eta.enqueue(3, 10_000);
        eta.forget(2);
        assert_eq!(eta.eta_ms(3, 6_000), Some(5_000));
    }
}
