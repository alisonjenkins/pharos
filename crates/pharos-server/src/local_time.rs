//! Server-local wall clock for access schedules (T68, V578). Jellyfin
//! evaluates `AccessSchedule` against `DateTime.UtcNow.ToLocalTime()`, so the
//! host's timezone (`TZ`) decides when a user's window opens.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Set once the UTC fallback has been logged: a host without local time
/// fails on every request, and one warning says it as well as thousands.
static FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);

/// True only for the first caller.
fn first_fallback() -> bool {
    !FALLBACK_LOGGED.swap(true, Ordering::Relaxed)
}

/// Seconds in a day.
const DAY_SECS: u64 = 86_400;
/// 1970-01-01 was a Thursday (weekday 4, Sunday = 0).
const EPOCH_WEEKDAY: u64 = 4;

/// Weekday (0 = Sunday) and fractional hour of the UTC instant `secs` after
/// the epoch. The fallback when the host cannot convert to local time.
pub fn utc_weekday_hour(secs: u64) -> (u8, f64) {
    let weekday = (secs / DAY_SECS + EPOCH_WEEKDAY) % 7;
    let into_day = secs % DAY_SECS;
    (weekday as u8, into_day as f64 / 3600.0)
}

/// Weekday (0 = Sunday) and fractional hour now, in server local time.
pub fn now_local() -> (u8, f64) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    local_weekday_hour(secs).unwrap_or_else(|| {
        if first_fallback() {
            tracing::warn!("local time unavailable; evaluating access schedules in UTC");
        }
        utc_weekday_hour(secs)
    })
}

#[cfg(unix)]
fn local_weekday_hour(secs: u64) -> Option<(u8, f64)> {
    let t = libc::time_t::try_from(secs).ok()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `localtime_r` reads `t` and fills `tm`, both valid for the
    // call; it keeps no pointer to either and is thread-safe.
    let filled = unsafe { libc::localtime_r(&t, tm.as_mut_ptr()) };
    if filled.is_null() {
        return None;
    }
    // SAFETY: a non-null return means `localtime_r` initialised `tm`.
    let tm = unsafe { tm.assume_init() };
    let hour = f64::from(tm.tm_hour) + f64::from(tm.tm_min) / 60.0 + f64::from(tm.tm_sec) / 3600.0;
    Some((u8::try_from(tm.tm_wday).ok()?, hour))
}

#[cfg(not(unix))]
fn local_weekday_hour(_secs: u64) -> Option<(u8, f64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_utc_fallback_is_reported_once() {
        first_fallback();
        assert!(!first_fallback());
        assert!(!first_fallback());
    }

    #[test]
    fn utc_weekday_hour_matches_known_instants() {
        // 1970-01-01T00:00:00Z, a Thursday.
        assert_eq!(utc_weekday_hour(0), (4, 0.0));
        // 2026-11-07T14:30:00Z, a Saturday.
        assert_eq!(utc_weekday_hour(1_794_061_800), (6, 14.5));
    }
}
