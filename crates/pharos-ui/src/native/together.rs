//! Watch together (014): the client half of a SyncPlay group that does not
//! need the server. Pure functions and models here; the screens and the
//! network seam come in later commits.

/// How many clock samples the estimator keeps; older ones fall off.
pub const CLOCK_WINDOW: usize = 9;

/// One round trip to `Group.clock` (NTP style, all milliseconds): the client
/// sent at `t1`, the server received at `t2` and replied at `t3`, the client
/// got the reply at `t4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    pub t1: i64,
    pub t2: i64,
    pub t3: i64,
    pub t4: i64,
}

impl ClockSample {
    /// `((t2 - t1) + (t3 - t4)) / 2`: add it to a client clock reading to get
    /// server time.
    fn offset_ms(&self) -> i64 {
        ((self.t2 - self.t1) + (self.t3 - self.t4)).div_euclid(2)
    }

    /// `(t4 - t1) - (t3 - t2)`: the time on the wire, without the server's
    /// own handling time.
    fn rtt_ms(&self) -> i64 {
        (self.t4 - self.t1) - (self.t3 - self.t2)
    }
}

/// The offset between the client and server clocks, from the last
/// [`CLOCK_WINDOW`] samples. The median ignores the odd slow round trip that
/// a mean would follow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClockEstimator {
    samples: Vec<ClockSample>,
}

impl ClockEstimator {
    pub fn push(&mut self, sample: ClockSample) {
        self.samples.push(sample);
        if self.samples.len() > CLOCK_WINDOW {
            self.samples.remove(0);
        }
    }

    /// Median offset; `None` before the first sample.
    pub fn offset_ms(&self) -> Option<i64> {
        median(self.samples.iter().map(ClockSample::offset_ms).collect())
    }

    /// Median round-trip time; `None` before the first sample.
    pub fn rtt_ms(&self) -> Option<i64> {
        median(self.samples.iter().map(ClockSample::rtt_ms).collect())
    }

    /// Server time for a client clock reading; `None` before the first sample.
    pub fn server_now(&self, client_ms: i64) -> Option<i64> {
        self.offset_ms().map(|o| client_ms + o)
    }
}

/// The middle value; for an even count, the mean of the two middle values.
fn median(mut values: Vec<i64>) -> Option<i64> {
    values.sort_unstable();
    let n = values.len();
    if n == 0 {
        return None;
    }
    let mid = n / 2;
    if n % 2 == 1 {
        values.get(mid).copied()
    } else {
        let (a, b) = (values.get(mid - 1)?, values.get(mid)?);
        Some((a + b).div_euclid(2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample whose server clock is `offset` ahead and whose trip took
    /// `rtt` split evenly, sent at `at`; the server answers at once.
    fn sample(at: i64, offset: i64, rtt: i64) -> ClockSample {
        let server = at + rtt / 2 + offset;
        ClockSample {
            t1: at,
            t2: server,
            t3: server,
            t4: at + rtt,
        }
    }

    fn estimator(samples: &[ClockSample]) -> ClockEstimator {
        let mut e = ClockEstimator::default();
        for s in samples {
            e.push(*s);
        }
        e
    }

    #[test]
    fn no_samples_gives_nothing() {
        let e = ClockEstimator::default();
        assert_eq!(e.offset_ms(), None);
        assert_eq!(e.rtt_ms(), None);
        assert_eq!(e.server_now(1_000), None);
    }

    #[test]
    fn one_sample_gives_its_offset_and_rtt() {
        let e = estimator(&[sample(1_000, 250, 40)]);
        assert_eq!(e.offset_ms(), Some(250));
        assert_eq!(e.rtt_ms(), Some(40));
        assert_eq!(e.server_now(2_000), Some(2_250));
    }

    #[test]
    fn the_servers_handling_time_is_not_round_trip() {
        // 20 ms out, 30 ms handling, 20 ms back; server 100 ms ahead.
        let s = ClockSample {
            t1: 1_000,
            t2: 1_120,
            t3: 1_150,
            t4: 1_070,
        };
        let e = estimator(&[s]);
        assert_eq!(e.rtt_ms(), Some(40));
        assert_eq!(e.offset_ms(), Some(100));
    }

    #[test]
    fn an_odd_count_takes_the_middle_and_an_even_count_the_mean_of_two() {
        let odd = estimator(&[
            sample(0, 100, 20),
            sample(1_000, 300, 40),
            sample(2_000, 200, 30),
        ]);
        assert_eq!(odd.offset_ms(), Some(200));
        assert_eq!(odd.rtt_ms(), Some(30));
        let even = estimator(&[sample(0, 100, 20), sample(1_000, 200, 40)]);
        assert_eq!(even.offset_ms(), Some(150));
        assert_eq!(even.rtt_ms(), Some(30));
    }

    #[test]
    fn a_slow_outlier_does_not_move_the_median() {
        let mut samples = vec![sample(0, 100, 20); 4];
        samples.push(sample(5_000, 900, 2_000));
        let e = estimator(&samples);
        assert_eq!(e.offset_ms(), Some(100));
        assert_eq!(e.rtt_ms(), Some(20));
    }

    #[test]
    fn only_the_last_nine_samples_count() {
        let mut e = ClockEstimator::default();
        for i in 0..CLOCK_WINDOW as i64 {
            e.push(sample(i * 1_000, 10_000, 20));
        }
        assert_eq!(e.offset_ms(), Some(10_000));
        for i in 0..CLOCK_WINDOW as i64 {
            e.push(sample(100_000 + i * 1_000, 50, 20));
        }
        assert_eq!(e.offset_ms(), Some(50));
        assert_eq!(e.samples.len(), CLOCK_WINDOW);
    }

    #[test]
    fn a_negative_offset_is_kept() {
        let e = estimator(&[sample(10_000, -400, 60)]);
        assert_eq!(e.offset_ms(), Some(-400));
    }
}
