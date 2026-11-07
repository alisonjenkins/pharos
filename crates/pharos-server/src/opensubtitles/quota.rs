//! The "no downloads until" gate (V567).
//!
//! A 406 or 429 on `/download` means the day's quota is gone. Retrying only
//! burns request budget, so the gate records when the provider said the limit
//! lifts and every caller checks it before asking.

use std::sync::atomic::{AtomicI64, Ordering};

/// Fallback pause when a quota response carries no usable reset time.
pub const DEFAULT_PAUSE_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Default)]
pub struct QuotaGate {
    blocked_until_unix: AtomicI64,
}

impl QuotaGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seconds from `now_unix` until downloads may resume; `None` when open.
    pub fn blocked_for(&self, now_unix: i64) -> Option<i64> {
        let until = self.blocked_until_unix.load(Ordering::Relaxed);
        (until > now_unix).then(|| until.saturating_sub(now_unix))
    }

    /// Close the gate until `until_unix`. It never reopens earlier than a
    /// previous, later close.
    pub fn block_until(&self, until_unix: i64) {
        self.blocked_until_unix
            .fetch_max(until_unix, Ordering::Relaxed);
    }

    pub fn blocked_until(&self) -> i64 {
        self.blocked_until_unix.load(Ordering::Relaxed)
    }
}

/// Unix seconds from an RFC 3339 UTC timestamp such as
/// `2026-11-08T12:00:00.000Z` (the shape of `reset_time_utc`). `None` for
/// anything else.
pub fn parse_utc_timestamp(s: &str) -> Option<i64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let time = time.split('.').next()?;
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn gate_is_open_until_blocked_and_reopens_at_the_instant() {
        let g = QuotaGate::new();
        assert_eq!(g.blocked_for(100), None);
        g.block_until(160);
        assert_eq!(g.blocked_for(100), Some(60));
        assert_eq!(g.blocked_for(160), None);
    }

    #[test]
    fn a_later_close_is_not_shortened_by_an_earlier_one() {
        let g = QuotaGate::new();
        g.block_until(500);
        g.block_until(200);
        assert_eq!(g.blocked_until(), 500);
    }

    #[test]
    fn parses_reset_timestamps() {
        assert_eq!(parse_utc_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_utc_timestamp("2026-11-08T12:00:00.000Z"),
            Some(1_794_139_200)
        );
        assert_eq!(
            parse_utc_timestamp("2024-02-29T00:00:01Z"),
            Some(1_709_164_801)
        );
        assert_eq!(parse_utc_timestamp("tomorrow"), None);
        assert_eq!(parse_utc_timestamp("2026-13-01T00:00:00Z"), None);
    }
}
