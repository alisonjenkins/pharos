//! Pure subtitle event-window math — feature-free so the HLS burn-gating
//! layer can test overlap without linking libav (the SCAN that produces
//! windows lives in `libav::subtitle_windows`, `backend-lib` builds only).

/// One on-screen interval, milliseconds from stream start.
pub type WindowMs = (u64, u64);

/// Merge overlapping/adjacent windows so consumers hold a minimal sorted
/// interval list.
pub fn merge_windows(mut windows: Vec<WindowMs>) -> Vec<WindowMs> {
    windows.sort_unstable();
    let mut merged: Vec<WindowMs> = Vec::with_capacity(windows.len());
    for (s, e) in windows {
        match merged.last_mut() {
            Some((_, le)) if s <= *le => *le = (*le).max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// Does any window overlap `[start_ms, end_ms)`? `windows` must be merged/
/// sorted (the scan output). Binary-search on the sorted starts keeps the
/// per-segment check O(log n).
pub fn any_window_overlaps(windows: &[WindowMs], start_ms: u64, end_ms: u64) -> bool {
    // First window starting at-or-after start_ms…
    let idx = windows.partition_point(|(s, _)| *s < start_ms);
    // …either it begins inside the range,
    if windows.get(idx).is_some_and(|(s, _)| *s < end_ms) {
        return true;
    }
    // …or the previous window is still open at start_ms.
    idx > 0 && windows[idx - 1].1 > start_ms
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn merge_collapses_overlaps_and_keeps_gaps() {
        let m = merge_windows(vec![(10, 20), (15, 30), (40, 50), (50, 60), (100, 110)]);
        assert_eq!(m, vec![(10, 30), (40, 60), (100, 110)]);
    }

    #[test]
    fn overlap_test_covers_edges() {
        let w = vec![(10_000, 20_000), (60_000, 70_000)];
        // Fully inside a window.
        assert!(any_window_overlaps(&w, 12_000, 13_000));
        // Segment straddles a window start.
        assert!(any_window_overlaps(&w, 5_000, 10_001));
        // Segment straddles a window end.
        assert!(any_window_overlaps(&w, 19_999, 25_000));
        // Gap between windows → no burn.
        assert!(!any_window_overlaps(&w, 20_000, 60_000));
        // Before all / after all.
        assert!(!any_window_overlaps(&w, 0, 10_000));
        assert!(!any_window_overlaps(&w, 70_000, 80_000));
        // Empty list.
        assert!(!any_window_overlaps(&[], 0, u64::MAX));
    }
}
