//! How long until a queued song's word times are ready.
//!
//! The aligner's cost is learned from its own jobs: a fixed part per song
//! (decoding, the offset search, the model's warm-up) plus a part per second
//! of audio, fitted to recent jobs with the newest weighted most. A wall time
//! per audio second alone ran 2 to 3 times long on short songs and short on
//! long ones (2026-11-11, 16 songs). Nothing is stored; after a restart there is no
//! history and no estimate. Every call takes the time as `now_ms` (any
//! monotonic origin, as long as it is the same one throughout) so the maths
//! needs no clock.

use std::collections::VecDeque;

/// Weight of the newest job in the fit: high enough to follow a slower
/// or faster aligner (GPU contention included) within a few songs, low enough
/// that one odd file does not swing the estimate.
const ALPHA: f64 = 0.3;

/// The least spread of song lengths, as a variance in seconds², that can
/// tell a song's fixed cost from its cost per second; below it every song
/// is priced per second alone.
const MIN_SPREAD_S2: f64 = 100.0;

/// Recent jobs as decayed sums, newest weighted 1 and each older one
/// `1 - ALPHA` times the next: `x` audio seconds, `y` wall milliseconds.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Fit {
    w: f64,
    x: f64,
    y: f64,
    xx: f64,
    xy: f64,
}

impl Fit {
    fn add(&mut self, x: f64, y: f64) {
        let keep = 1.0 - ALPHA;
        self.w = self.w * keep + 1.0;
        self.x = self.x * keep + x;
        self.y = self.y * keep + y;
        self.xx = self.xx * keep + x * x;
        self.xy = self.xy * keep + x * y;
    }

    /// `(fixed_ms, ms_per_s)`, or `None` before any job. A line through the
    /// jobs when their lengths spread enough to place one; through the
    /// origin otherwise, or when the line would give a song negative cost.
    fn line(&self) -> Option<(f64, f64)> {
        if self.w <= 0.0 || self.x <= 0.0 {
            return None;
        }
        let (mx, my) = (self.x / self.w, self.y / self.w);
        let var = self.xx / self.w - mx * mx;
        let through_origin = (0.0, self.y / self.x);
        if var < MIN_SPREAD_S2 {
            return Some(through_origin);
        }
        let per_s = (self.xy / self.w - mx * my) / var;
        let fixed = my - per_s * mx;
        if per_s < 0.0 || fixed < 0.0 {
            return Some(through_origin);
        }
        Some((fixed, per_s))
    }
}

#[derive(Debug)]
struct Running {
    id: u64,
    started_ms: u64,
    /// The song's length; 0 when unknown.
    duration_ms: u64,
}

/// The queue, the running job and the learned cost of alignment.
#[derive(Debug, Default)]
pub struct Eta {
    /// What recent jobs cost.
    fit: Fit,
    running: Option<Running>,
    /// Waiting jobs, next to run first, with each song's length.
    queue: VecDeque<(u64, u64)>,
}

impl Eta {
    fn cost_ms((fixed, per_s): (f64, f64), duration_ms: u64) -> f64 {
        fixed + per_s * duration_ms as f64 / 1000.0
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
        self.running = Some(Running {
            id,
            started_ms: now_ms,
            duration_ms,
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
        self.fit.add(duration_ms as f64 / 1000.0, elapsed);
    }

    /// The running job failed. A failure's duration says nothing about the
    /// cost of a success, so the fit stays as it was.
    pub fn abort(&mut self, id: u64) {
        self.running.take_if(|r| r.id == id);
    }

    /// Milliseconds until `id` is done, or `None` with no history yet, an
    /// unknown length for the song or for any job ahead of it, or an `id`
    /// neither queued nor running.
    ///
    /// The running job's cost is priced at the current fit, so a job begun
    /// before the first was learned is still estimated once one is.
    pub fn eta_ms(&self, id: u64, now_ms: u64) -> Option<u64> {
        let rate = self.fit.line()?;
        let mut total = 0.0;
        if let Some(run) = &self.running {
            if run.duration_ms == 0 {
                return None;
            }
            let expected = Self::cost_ms(rate, run.duration_ms);
            let left = (expected - now_ms.saturating_sub(run.started_ms) as f64).max(0.0);
            if run.id == id {
                return Some(left as u64);
            }
            total += left;
        }
        for (queued, duration_ms) in &self.queue {
            if *duration_ms == 0 {
                return None;
            }
            total += Self::cost_ms(rate, *duration_ms);
            if *queued == id {
                return Some(total as u64);
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

    fn run(eta: &mut Eta, id: u64, duration_ms: u64, took_ms: u64) {
        eta.enqueue(id, duration_ms);
        eta.start(id, 0);
        eta.finish(id, took_ms, duration_ms);
    }

    #[test]
    fn the_first_job_sets_the_rate_and_later_ones_weigh_in_newest_first() {
        let mut eta = learned();
        assert_eq!(eta.fit.line(), Some((0.0, 500.0)));
        run(&mut eta, 2, 10_000, 10_000);
        // Songs of one length: per second alone, (0.7 * 5000 + 10000) /
        // (0.7 * 10 + 10) ms per second.
        let (fixed, per_s) = eta.fit.line().unwrap();
        assert!(fixed.abs() < 1e-9);
        assert!((per_s - 13_500.0 / 17.0).abs() < 1e-9, "{per_s}");
    }

    /// B1328, V1299: a fixed cost per song is learned once songs of different
    /// lengths have run, so a short song is not priced as a sliver of a long
    /// one. Disarm check: with `line` always through the origin, this fails.
    #[test]
    fn a_fixed_cost_per_song_is_learned_from_songs_of_different_lengths() {
        let mut eta = Eta::default();
        // 2 s per song plus 20 ms per audio second, exactly.
        for (id, secs) in [(1, 60), (2, 240), (3, 120), (4, 300)] {
            run(&mut eta, id, secs * 1000, 2_000 + secs * 20);
        }
        let (fixed, per_s) = eta.fit.line().unwrap();
        assert!((fixed - 2_000.0).abs() < 1e-6, "{fixed}");
        assert!((per_s - 20.0).abs() < 1e-9, "{per_s}");
        eta.enqueue(5, 30_000);
        // 2000 + 30 * 20, to within the float sums' rounding.
        let left = eta.eta_ms(5, 0).unwrap();
        assert!((2_599..=2_600).contains(&left), "{left}");
    }

    #[test]
    fn a_fit_that_would_price_a_song_below_zero_is_per_second_alone() {
        let mut eta = Eta::default();
        // Longer songs that took less time: no line fits, so the ratio.
        run(&mut eta, 1, 60_000, 9_000);
        run(&mut eta, 2, 300_000, 3_000);
        let (fixed, per_s) = eta.fit.line().unwrap();
        assert!(fixed.abs() < 1e-9);
        assert!((per_s - (0.7 * 9_000.0 + 3_000.0) / (0.7 * 60.0 + 300.0)).abs() < 1e-9);
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
        assert_eq!(eta.fit.line(), Some((0.0, 500.0)));
        assert!(eta.running.is_none());
    }

    #[test]
    fn a_song_of_unknown_length_has_no_estimate() {
        let mut eta = learned();
        eta.enqueue(2, 0);
        assert_eq!(eta.eta_ms(2, 6_000), None);
    }

    #[test]
    fn a_running_song_of_unknown_length_has_no_estimate() {
        let mut eta = learned();
        eta.enqueue(2, 0);
        eta.start(2, 6_000);
        assert_eq!(eta.eta_ms(2, 7_000), None);
    }

    #[test]
    fn a_song_behind_one_of_unknown_length_has_no_estimate() {
        let mut eta = learned();
        eta.enqueue(2, 0);
        eta.enqueue(3, 10_000);
        assert_eq!(eta.eta_ms(3, 6_000), None);
        eta.start(2, 6_000);
        assert_eq!(eta.eta_ms(3, 7_000), None);
    }

    #[test]
    fn a_job_begun_before_any_rate_is_estimated_once_one_is_learned() {
        let mut eta = Eta::default();
        eta.enqueue(1, 10_000);
        eta.enqueue(2, 20_000);
        eta.start(1, 0);
        // Pretend another worker's answer taught the rate; the running
        // job is then priced at it, not stuck at "almost done".
        eta.fit.add(10.0, 5_000.0);
        assert_eq!(eta.eta_ms(1, 1_000), Some(4_000));
        assert_eq!(eta.eta_ms(2, 1_000), Some(14_000));
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
