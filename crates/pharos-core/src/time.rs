//! T88(b) — canonical time units.
//!
//! Jellyfin measures media positions in **ticks** (100-nanosecond units, so
//! `10_000_000` per second). Before this module every crate that touched a
//! position redefined `const TICKS_PER_SECOND` and open-coded the
//! `ticks as f64 / 10_000_000.0` / `(secs * 10_000_000.0) as u64`
//! conversions — six copies, easy to drift (one was even `f64`). This is the
//! single source of truth: one constant, one conversion impl.
//!
//! The newtypes make the unit explicit at conversion boundaries
//! (`Ticks::from_seconds(x)` reads unambiguously) without forcing every
//! `u64` field to change type — call sites unwrap via `.0` where a raw tick
//! count is still what the wire / options struct wants.

/// Jellyfin ticks per second (100 ns units).
pub const TICKS_PER_SECOND: u64 = 10_000_000;

/// Jellyfin ticks per millisecond.
pub const TICKS_PER_MS: u64 = TICKS_PER_SECOND / 1_000;

/// A position/duration in Jellyfin ticks (100 ns units).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Ticks(pub u64);

/// A position/duration in seconds (fractional).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct Seconds(pub f64);

/// A position/duration in whole milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Millis(pub u64);

impl Ticks {
    /// Ticks → fractional seconds. Byte-identical to the old
    /// `ticks as f64 / TICKS_PER_SECOND as f64`.
    pub fn seconds(self) -> f64 {
        self.0 as f64 / TICKS_PER_SECOND as f64
    }

    /// Fractional seconds → ticks (truncating, like the old
    /// `(secs * TICKS_PER_SECOND as f64) as u64`). Negative / NaN clamps to 0.
    pub fn from_seconds(secs: f64) -> Self {
        if secs.is_finite() && secs > 0.0 {
            Ticks((secs * TICKS_PER_SECOND as f64) as u64)
        } else {
            Ticks(0)
        }
    }

    /// Whole milliseconds → ticks.
    pub fn from_millis(ms: u64) -> Self {
        Ticks(ms.saturating_mul(TICKS_PER_MS))
    }

    /// Ticks → whole milliseconds (truncating).
    pub fn millis(self) -> u64 {
        self.0 / TICKS_PER_MS
    }
}

impl Seconds {
    /// Seconds → ticks.
    pub fn to_ticks(self) -> Ticks {
        Ticks::from_seconds(self.0)
    }
}

impl Millis {
    /// Milliseconds → ticks.
    pub fn to_ticks(self) -> Ticks {
        Ticks::from_millis(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_round_trip_matches_legacy_math() {
        // 90.5s → ticks → back. Exact for values representable in f64.
        let t = Ticks::from_seconds(90.5);
        assert_eq!(t.0, 905_000_000);
        assert_eq!(t.seconds(), 90.5);
    }

    #[test]
    fn from_seconds_truncates_like_the_old_cast() {
        // (1.99999 * 10_000_000) as u64 == 19_999_900 (trunc, not round).
        assert_eq!(Ticks::from_seconds(1.99999).0, 19_999_900);
    }

    #[test]
    fn from_seconds_clamps_non_positive_and_nan() {
        assert_eq!(Ticks::from_seconds(-5.0).0, 0);
        assert_eq!(Ticks::from_seconds(f64::NAN).0, 0);
        assert_eq!(Ticks::from_seconds(0.0).0, 0);
    }

    #[test]
    fn millis_round_trip() {
        assert_eq!(Ticks::from_millis(1500).0, 15_000_000);
        assert_eq!(Ticks(15_000_000).millis(), 1500);
    }
}
