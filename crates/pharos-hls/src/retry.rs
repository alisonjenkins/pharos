//! Segment request deadlines and retry backoff (spec 031 slice 3, V637, FR-005).
//!
//! Pure: callers pass the clock in, so the policy is tested with a fake one.

/// Retries after the first failed attempt, as hls.js `fragLoadPolicy.errorRetry`.
const DEFAULT_MAX_RETRIES: u32 = 6;

/// First backoff delay; it doubles per retry (hls.js `errorRetry.retryDelayMs`).
const DEFAULT_BASE_DELAY_MS: u64 = 1_000;

/// Backoff ceiling (hls.js `errorRetry.maxRetryDelayMs`).
const DEFAULT_MAX_DELAY_MS: u64 = 8_000;

/// How long a segment request may wait for its response headers, and then for
/// its body. Provisional, not measured: the slice 0 baseline (T031-03) retunes
/// them per device class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    /// From sending the request to its response headers. Covers an on-demand
    /// encode, which the server does before answering.
    pub first_byte_ms: u64,
    /// From the response headers to the last body byte.
    pub load_ms: u64,
}

/// Backoff and give-up limits for failed segment requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    pub deadlines: Deadlines,
}

impl RetryPolicy {
    /// The default backoff with the given deadlines.
    pub fn with_deadlines(deadlines: Deadlines) -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            base_delay_ms: DEFAULT_BASE_DELAY_MS,
            max_delay_ms: DEFAULT_MAX_DELAY_MS,
            deadlines,
        }
    }

    /// Delay before retry number `retry` (1-based): base, doubling, capped.
    pub fn backoff_ms(&self, retry: u32) -> u64 {
        let doublings = retry.saturating_sub(1).min(63);
        self.base_delay_ms
            .saturating_mul(1_u64 << doublings)
            .min(self.max_delay_ms)
    }
}

/// Why a segment request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The server answered with a non-success status.
    Http(u16),
    /// The request failed with no response (offline, reset, CORS).
    Network,
    /// No response headers within [`Deadlines::first_byte_ms`].
    TimeoutFirstByte,
    /// No complete body within [`Deadlines::load_ms`] of the headers.
    TimeoutLoad,
}

impl Cause {
    /// Stable log and telemetry token.
    pub fn label(self) -> &'static str {
        match self {
            Self::Http(_) => "http",
            Self::Network => "network",
            Self::TimeoutFirstByte => "timeout_first_byte",
            Self::TimeoutLoad => "timeout_load",
        }
    }

    /// A client error will fail the same way again, except a request timeout
    /// (408) or a rate limit (429). Everything else may be transient.
    pub fn retryable(self) -> bool {
        match self {
            Self::Http(status) => !(400..500).contains(&status) || status == 408 || status == 429,
            Self::Network | Self::TimeoutFirstByte | Self::TimeoutLoad => true,
        }
    }
}

/// Why [`Retries::on_failure`] gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUp {
    /// The cause cannot succeed on a retry.
    NotRetryable,
    /// [`RetryPolicy::max_retries`] retries already failed.
    Exhausted,
}

impl GiveUp {
    /// Stable log and telemetry token.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotRetryable => "not_retryable",
            Self::Exhausted => "exhausted",
        }
    }
}

/// What to do after a failed attempt. `attempt` counts this request's failed
/// attempts so far, starting at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Retry { attempt: u32, after_ms: u64 },
    Fail { attempt: u32, why: GiveUp },
}

/// Consecutive failures of one request key (a track's init or segment index).
/// A failure of a different key starts the count again.
#[derive(Debug, Clone)]
pub struct Retries<K> {
    policy: RetryPolicy,
    failing: Option<(K, u32)>,
}

impl<K: Copy + PartialEq> Retries<K> {
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            policy,
            failing: None,
        }
    }

    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    /// Records a failed attempt of `key` and decides whether to try again.
    pub fn on_failure(&mut self, key: K, cause: Cause) -> Verdict {
        let attempt = match self.failing {
            Some((prev, n)) if prev == key => n.saturating_add(1),
            _ => 1,
        };
        self.failing = Some((key, attempt));
        if !cause.retryable() {
            return Verdict::Fail {
                attempt,
                why: GiveUp::NotRetryable,
            };
        }
        if attempt > self.policy.max_retries {
            return Verdict::Fail {
                attempt,
                why: GiveUp::Exhausted,
            };
        }
        Verdict::Retry {
            attempt,
            after_ms: self.policy.backoff_ms(attempt),
        }
    }

    /// A request succeeded: the next failure starts a fresh count.
    pub fn on_success(&mut self) {
        self.failing = None;
    }
}

/// Deadline clock of one in-flight request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    deadlines: Deadlines,
    sent_ms: u64,
    headers_ms: Option<u64>,
}

impl Watch {
    pub fn new(deadlines: Deadlines, sent_ms: u64) -> Self {
        Self {
            deadlines,
            sent_ms,
            headers_ms: None,
        }
    }

    /// The response headers arrived at `now_ms`; the load deadline starts.
    pub fn headers(&mut self, now_ms: u64) {
        self.headers_ms = Some(now_ms);
    }

    /// The deadline the request is now running against, as its cause and
    /// the absolute time it passes.
    pub fn due(&self) -> (Cause, u64) {
        match self.headers_ms {
            None => (
                Cause::TimeoutFirstByte,
                self.sent_ms.saturating_add(self.deadlines.first_byte_ms),
            ),
            Some(at) => (
                Cause::TimeoutLoad,
                at.saturating_add(self.deadlines.load_ms),
            ),
        }
    }

    /// Milliseconds left before [`Watch::due`] passes, zero once it has.
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.due().1.saturating_sub(now_ms)
    }

    /// The timeout cause once the current deadline has passed at `now_ms`.
    pub fn expired(&self, now_ms: u64) -> Option<Cause> {
        let (cause, at) = self.due();
        (now_ms >= at).then_some(cause)
    }

    /// The deadline value in force, for the log line.
    pub fn deadline_ms(&self) -> u64 {
        match self.headers_ms {
            None => self.deadlines.first_byte_ms,
            Some(_) => self.deadlines.load_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cause, Deadlines, GiveUp, Retries, RetryPolicy, Verdict, Watch};

    const DEADLINES: Deadlines = Deadlines {
        first_byte_ms: 10_000,
        load_ms: 60_000,
    };

    fn retries() -> Retries<usize> {
        Retries::new(RetryPolicy::with_deadlines(DEADLINES))
    }

    #[test]
    fn backoff_doubles_from_base_and_caps() {
        let p = RetryPolicy::with_deadlines(DEADLINES);
        let delays: Vec<u64> = (1..=7).map(|r| p.backoff_ms(r)).collect();
        assert_eq!(delays, [1_000, 2_000, 4_000, 8_000, 8_000, 8_000, 8_000]);
        assert_eq!(p.backoff_ms(u32::MAX), 8_000);
    }

    #[test]
    fn retries_with_backoff_up_to_the_limit_then_fails_exhausted() {
        let mut r = retries();
        for attempt in 1..=6 {
            assert_eq!(
                r.on_failure(3, Cause::Network),
                Verdict::Retry {
                    attempt,
                    after_ms: r.policy().backoff_ms(attempt),
                },
            );
        }
        assert_eq!(
            r.on_failure(3, Cause::Http(503)),
            Verdict::Fail {
                attempt: 7,
                why: GiveUp::Exhausted,
            },
        );
    }

    #[test]
    fn another_request_or_a_success_starts_the_count_again() {
        let mut r = retries();
        r.on_failure(3, Cause::Network);
        r.on_failure(3, Cause::Network);
        assert!(matches!(
            r.on_failure(4, Cause::Network),
            Verdict::Retry { attempt: 1, .. }
        ));
        r.on_success();
        assert!(matches!(
            r.on_failure(4, Cause::Network),
            Verdict::Retry { attempt: 1, .. }
        ));
    }

    #[test]
    fn client_errors_fail_at_once_except_timeout_and_rate_limit() {
        for status in [400, 401, 403, 404, 410] {
            assert_eq!(
                retries().on_failure(0, Cause::Http(status)),
                Verdict::Fail {
                    attempt: 1,
                    why: GiveUp::NotRetryable,
                },
                "{status}",
            );
        }
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(
                matches!(
                    retries().on_failure(0, Cause::Http(status)),
                    Verdict::Retry { attempt: 1, .. }
                ),
                "{status}",
            );
        }
    }

    #[test]
    fn watch_times_the_first_byte_then_the_body_on_a_fake_clock() {
        let mut w = Watch::new(DEADLINES, 1_000);
        assert_eq!(w.expired(10_999), None);
        assert_eq!(w.remaining_ms(10_999), 1);
        assert_eq!(w.expired(11_000), Some(Cause::TimeoutFirstByte));
        assert_eq!(w.deadline_ms(), 10_000);

        w.headers(5_000);
        assert_eq!(w.expired(11_000), None);
        assert_eq!(w.due(), (Cause::TimeoutLoad, 65_000));
        assert_eq!(w.remaining_ms(64_000), 1_000);
        assert_eq!(w.expired(65_000), Some(Cause::TimeoutLoad));
        assert_eq!(w.remaining_ms(70_000), 0);
        assert_eq!(w.deadline_ms(), 60_000);
    }

    #[test]
    fn cause_and_give_up_labels_are_distinct() {
        let causes = [
            Cause::Http(500),
            Cause::Network,
            Cause::TimeoutFirstByte,
            Cause::TimeoutLoad,
        ]
        .map(Cause::label);
        let mut sorted = causes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), causes.len());
        assert_ne!(GiveUp::NotRetryable.label(), GiveUp::Exhausted.label());
    }
}
