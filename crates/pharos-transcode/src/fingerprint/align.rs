//! Pure audio-fingerprint alignment — the algorithmic heart of intro/outro
//! detection (ADR-0018), ported from the Jellyfin intro-skipper plugin's
//! `ChromaprintAnalyzer.CompareEpisodes` (branch 10.11).
//!
//! A fingerprint is a `&[u32]`, one point per [`sample_duration_secs`] of
//! audio (rusty-chromaprint preset_test2 hop ≈ 0.248 s). Two episodes
//! of a series share their intro (and, at the tail, their credits) as a run of
//! near-identical audio — this module finds that run.
//!
//! Deliberately dependency-free and deterministic so the whole detector can be
//! unit-tested on synthetic fingerprint vectors, no ffmpeg in the hot path.

/// Seconds of audio covered by one fingerprint point for the preset our
/// fingerprinter uses. Measured empirically (`fingerprint_detect` probe):
/// `rusty-chromaprint`'s `.fingerprint()` emits ONE point per **two** config
/// "items", so the real steady-state hop is `2 × item_duration_in_seconds()`
/// (≈ 0.248 s), not the item duration itself and not the plugin's ffmpeg-muxer
/// 0.124 s. Sourced from the crate config so it tracks any preset change.
/// (A ~5 s warmup shifts absolute positions by a small constant, absorbed by
/// the ≤5 s snap-to-zero for intros and human tolerance on the skip button.)
pub fn sample_duration_secs() -> f64 {
    2.0 * rusty_chromaprint::Configuration::preset_test2().item_duration_in_seconds() as f64
}

/// Tunable constants for [`compare`], defaulted to the intro-skipper values.
#[derive(Debug, Clone, Copy)]
pub struct AlignConfig {
    /// Max Hamming distance (differing bits, of 32) for two points to match.
    /// intro-skipper `MaximumFingerprintPointDifferences` = 6.
    pub max_bit_diff: u32,
    /// Max gap (seconds) between consecutive matches inside one contiguous
    /// span. intro-skipper `MaximumTimeSkip` = 3.5.
    pub max_time_skip: f64,
    /// Reject spans shorter than this (seconds). `MinimumIntroDuration` = 15.
    pub min_duration: f64,
    /// Reject spans longer than this (seconds). `MaximumIntroDuration` = 120.
    pub max_duration: f64,
    /// A span starting at or before this (seconds) snaps to 0 — an intro that
    /// begins "almost immediately" starts at the top. `= 5`.
    pub snap_start_secs: f64,
    /// Seconds per fingerprint point — see [`sample_duration_secs`]. On
    /// `AlignConfig::default()` it's read from the crate; keep it consistent
    /// with the fingerprinter that produced the points.
    pub secs_per_point: f64,
}

impl Default for AlignConfig {
    fn default() -> Self {
        Self {
            max_bit_diff: 6,
            max_time_skip: 3.5,
            min_duration: 15.0,
            max_duration: 120.0,
            snap_start_secs: 5.0,
            secs_per_point: sample_duration_secs(),
        }
    }
}

/// A `[start, end]` span in seconds, relative to the fingerprinted window's
/// zero (the caller adds any window offset, e.g. a credits tail start).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

impl Span {
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }
}

/// The intro/credits span located in EACH of the two episodes. They can sit at
/// different offsets (episode B's intro may start later than A's), so both are
/// returned — each is saved against its own episode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchResult {
    pub lhs: Span,
    pub rhs: Span,
}

#[inline]
fn point_time(index: usize, secs_per_point: f64) -> f64 {
    index as f64 * secs_per_point
}

/// Longest contiguous run in an ascending time list where consecutive entries
/// are ≤ `max_skip` apart. Ported from `TimeRangeHelpers.FindContiguous`.
/// `secs_per_point` extends the last matched point by its own hop so a run's
/// end covers the point's sample window (the plugin's times are point-centres).
fn find_contiguous(times: &[f64], max_skip: f64, secs_per_point: f64) -> Option<Span> {
    if times.is_empty() {
        return None;
    }
    let (mut best_start, mut best_end) = (times[0], times[0]);
    let (mut cur_start, mut cur_end) = (times[0], times[0]);
    for &t in &times[1..] {
        if t - cur_end <= max_skip {
            cur_end = t;
        } else {
            if cur_end - cur_start > best_end - best_start {
                best_start = cur_start;
                best_end = cur_end;
            }
            cur_start = t;
            cur_end = t;
        }
    }
    if cur_end - cur_start > best_end - best_start {
        best_start = cur_start;
        best_end = cur_end;
    }
    // A single-point run has zero duration; extend by one hop so the last
    // matched point contributes its own sample window (matches the plugin,
    // whose times are point-centres).
    Some(Span {
        start: best_start,
        end: best_end + secs_per_point,
    })
}

/// Every alignment shift (`rhs_index - lhs_index`) that could still yield an
/// in-bounds span — i.e. every shift whose overlap is at least `min_duration`
/// long. Nothing shorter can clear `bound_and_snap`, so nothing shorter is worth
/// testing.
///
/// This replaces the intro-skipper's inverted-index seeding, which probed for
/// points equal within `index_shift` as an INTEGER addend. That made discovery
/// exact while acceptance ([`matches_at_shift`]) is fuzzy to `max_bit_diff`
/// differing bits — so two episodes could share an entire opening, every point
/// of it inside the 6-bit tolerance, and still seed ZERO shifts because no pair
/// of points happened to be numerically adjacent. Measured on a real season
/// (`tests/fixtures/mushoku_s03_fingerprints.txt`): 9 of 10 intro pairs produced
/// no candidate shift at all, while the credits window of the same five files
/// matched 6 of 10. The opening was never compared, not compared and rejected.
///
/// Cost is bounded and predictable: `|lhs| + |rhs|` shifts, each scanning at most
/// the overlap, so a pair is `O(n²)` popcounts — for the ~1400-point windows this
/// detector uses, single-digit milliseconds.
fn feasible_shifts(lhs: &[u32], rhs: &[u32], cfg: &AlignConfig) -> std::ops::Range<i64> {
    // A span needs this many consecutive points to reach `min_duration`.
    let min_points = (cfg.min_duration / cfg.secs_per_point).ceil() as i64;
    let (l, r) = (lhs.len() as i64, rhs.len() as i64);
    // Overlap at shift s is min(l, r - s) - max(0, -s); require >= min_points.
    let lo = -(l - min_points).max(0);
    let hi = (r - min_points).max(0) + 1;
    lo..hi.max(lo)
}

/// Matched point times in each episode at a fixed `shift` (rhs index = lhs
/// index + shift). Two points match when their popcount-XOR ≤ `max_bit_diff`.
fn matches_at_shift(
    lhs: &[u32],
    rhs: &[u32],
    shift: i64,
    cfg: &AlignConfig,
) -> (Vec<f64>, Vec<f64>) {
    let mut lhs_times = Vec::new();
    let mut rhs_times = Vec::new();
    // Overlap of lhs indices `i` such that `0 <= i+shift < rhs.len()`.
    let lo = if shift < 0 { (-shift) as usize } else { 0 };
    // Index arithmetic (i and its shifted partner j) is the point — not an
    // iterator walk over one slice.
    #[allow(clippy::needless_range_loop)]
    for i in lo..lhs.len() {
        let j = i as i64 + shift;
        if j < 0 || j as usize >= rhs.len() {
            continue;
        }
        let j = j as usize;
        if (lhs[i] ^ rhs[j]).count_ones() <= cfg.max_bit_diff {
            lhs_times.push(point_time(i, cfg.secs_per_point));
            rhs_times.push(point_time(j, cfg.secs_per_point));
        }
    }
    (lhs_times, rhs_times)
}

/// Apply the shared bounds + snap to a raw contiguous span. Returns `None` when
/// the span is out of the [min, max] duration window.
fn bound_and_snap(mut span: Span, cfg: &AlignConfig) -> Option<Span> {
    if span.start <= cfg.snap_start_secs {
        span.start = 0.0;
    }
    let dur = span.duration();
    if dur < cfg.min_duration || dur > cfg.max_duration {
        return None;
    }
    Some(span)
}

/// Find the intro/credits span shared by two episodes' fingerprints. Returns
/// the (possibly differently-offset) span located in each. `None` when no
/// span in the [min,max] duration window is shared.
///
/// The best candidate shift is the one whose lhs contiguous span is longest
/// (and in-bounds) — the plugin keeps the longest match per episode.
pub fn compare(lhs: &[u32], rhs: &[u32], cfg: &AlignConfig) -> Option<MatchResult> {
    if lhs.is_empty() || rhs.is_empty() {
        return None;
    }
    let mut best: Option<MatchResult> = None;
    for shift in feasible_shifts(lhs, rhs, cfg) {
        let (lhs_times, rhs_times) = matches_at_shift(lhs, rhs, shift, cfg);
        let (Some(lhs_span), Some(rhs_span)) = (
            find_contiguous(&lhs_times, cfg.max_time_skip, cfg.secs_per_point),
            find_contiguous(&rhs_times, cfg.max_time_skip, cfg.secs_per_point),
        ) else {
            continue;
        };
        let (Some(lhs_span), Some(rhs_span)) =
            (bound_and_snap(lhs_span, cfg), bound_and_snap(rhs_span, cfg))
        else {
            continue;
        };
        let better = best
            .as_ref()
            .map(|b| lhs_span.duration() > b.lhs.duration())
            .unwrap_or(true);
        if better {
            best = Some(MatchResult {
                lhs: lhs_span,
                rhs: rhs_span,
            });
        }
    }
    best
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Build a fingerprint: `head` filler + a shared `intro` block + `tail`
    /// filler, so two episodes share the intro at (possibly) different offsets.
    fn fp(head: &[u32], intro: &[u32], tail: &[u32]) -> Vec<u32> {
        let mut v = Vec::new();
        v.extend_from_slice(head);
        v.extend_from_slice(intro);
        v.extend_from_slice(tail);
        v
    }

    /// A distinctive, non-repeating filler so candidate shifts aren't polluted.
    fn filler(seed: u32, n: usize) -> Vec<u32> {
        (0..n)
            .map(|i| {
                seed.wrapping_mul(2_654_435_761)
                    .wrapping_add(i as u32 * 40_503)
            })
            .collect()
    }

    /// A shared intro block long enough to clear the 15 s min (~121 points).
    fn intro_block(n: usize) -> Vec<u32> {
        (0..n)
            .map(|i| 0xA53C_0000 ^ (i as u32).wrapping_mul(2_246_822_519))
            .collect()
    }

    #[test]
    fn finds_shared_intro_at_different_offsets() {
        // ~150-point intro ≈ 18.6 s, clears the 15 s minimum.
        let intro = intro_block(150);
        // Episode A: intro starts at point 10; B: at point 60 (later).
        let a = fp(&filler(1, 10), &intro, &filler(2, 200));
        let b = fp(&filler(3, 60), &intro, &filler(4, 200));
        let cfg = AlignConfig::default();
        let m = compare(&a, &b, &cfg).expect("intro found");
        // A's intro is near the top (start 10 pts ≈ 1.2 s ≤ 5 s snap → 0).
        assert!(m.lhs.start < 1.0, "lhs start {}", m.lhs.start);
        // B's intro starts at point 60 ≈ 7.4 s (past the snap window).
        assert!(
            (m.rhs.start - 60.0 * AlignConfig::default().secs_per_point).abs() < 1.5,
            "rhs {}",
            m.rhs.start
        );
        // The 150-point shared block is in-bounds (≥15 s, ≤120 s).
        assert!(
            m.lhs.duration() > 15.0 && m.lhs.duration() < 120.0,
            "dur {}",
            m.lhs.duration()
        );
    }

    #[test]
    fn tolerates_bit_noise_within_threshold() {
        let intro = intro_block(150);
        let a = fp(&filler(1, 5), &intro, &filler(2, 50));
        // Real intros are near-identical: most points match exactly (seeding
        // the shift via the ±2 index probe) while a minority carry ≤6-bit
        // noise (caught by the popcount threshold). Noise every 4th point.
        let mut noisy = intro.clone();
        for (i, p) in noisy.iter_mut().enumerate() {
            if i % 4 == 0 {
                *p ^= 0b10_1101; // 4 bits, ≤ the 6-bit threshold
            }
        }
        let b = fp(&filler(3, 5), &noisy, &filler(4, 50));
        let m = compare(&a, &b, &AlignConfig::default()).expect("noisy intro still matches");
        assert!(m.lhs.duration() > 15.0);
    }

    #[test]
    fn rejects_when_no_shared_span() {
        let a = fp(&filler(1, 10), &intro_block(150), &filler(2, 50));
        let b = fp(&filler(9, 10), &filler(8, 150), &filler(7, 50));
        assert!(compare(&a, &b, &AlignConfig::default()).is_none());
    }

    #[test]
    fn rejects_too_short_a_match() {
        // A 40-point shared block ≈ 5 s < the 15 s minimum.
        let short = intro_block(40);
        let a = fp(&filler(1, 10), &short, &filler(2, 50));
        let b = fp(&filler(3, 30), &short, &filler(4, 50));
        assert!(compare(&a, &b, &AlignConfig::default()).is_none());
    }

    #[test]
    fn find_contiguous_breaks_on_large_gap() {
        // Two runs separated by a 10 s gap → the longer run wins.
        let times = vec![0.0, 0.1, 0.2, 0.3, 10.3, 10.4];
        let span = find_contiguous(&times, 3.5, AlignConfig::default().secs_per_point).unwrap();
        assert!((span.start - 0.0).abs() < 1e-9);
        assert!(
            span.end < 1.0,
            "should stop before the gap, got {}",
            span.end
        );
    }

    #[test]
    fn snap_pulls_near_zero_start_to_top() {
        let s = bound_and_snap(
            Span {
                start: 3.0,
                end: 25.0,
            },
            &AlignConfig::default(),
        )
        .unwrap();
        assert_eq!(s.start, 0.0);
    }

    /// Spec 002 — the mechanism, in miniature.
    ///
    /// Acceptance is FUZZY: two points match when `(a ^ b).count_ones() <= 6`.
    /// Shift discovery used to be EXACT: it seeded only where two points were
    /// numerically equal within `index_shift` (±2 as an integer addend, which on
    /// a bit-packed chromaprint word means the two low bits and nothing else).
    ///
    /// So two episodes could share an entire opening — every point within the
    /// 6-bit tolerance — and yield ZERO candidate shifts, because not one pair
    /// of points was numerically near-identical. Nine of ten real intro pairs in
    /// `tests/fixtures/mushoku_s03_fingerprints.txt` were in exactly this state.
    ///
    /// Here every shared point differs by two HIGH bits: Hamming distance 2 (well
    /// inside the threshold) but numerically 0x30000000 apart (hopelessly outside
    /// the ±2 probe).
    #[test]
    fn a_shared_span_no_point_of_which_is_numerically_near_still_aligns() {
        const HIGH_BITS: u32 = 0x3000_0000; // 2 bits set, distance 2, value far
        /// The intro-skipper's `InvertedIndexShift`, the ± integer addend the
        /// deleted seeding step probed with. Kept as a literal so the test still
        /// states what the old discovery could reach.
        const OLD_INDEX_SHIFT: u32 = 2;
        assert_eq!(HIGH_BITS.count_ones(), 2, "inside max_bit_diff");

        let intro = intro_block(150);
        let shifted: Vec<u32> = intro.iter().map(|p| p ^ HIGH_BITS).collect();
        let a = fp(&filler(1, 10), &intro, &filler(2, 200));
        let b = fp(&filler(3, 60), &shifted, &filler(4, 200));

        let cfg = AlignConfig::default();
        for (i, (x, y)) in intro.iter().zip(shifted.iter()).enumerate() {
            assert!(
                (x ^ y).count_ones() <= cfg.max_bit_diff,
                "point {i} must be acceptable to the matcher"
            );
            assert!(
                x.abs_diff(*y) > OLD_INDEX_SHIFT,
                "point {i} must be OUT of reach of an exact-value probe"
            );
        }

        let m = compare(&a, &b, &cfg).expect(
            "a span every point of which is within the matcher's tolerance must be \
             found; if this is None, shift discovery is using a different notion of \
             similarity than point acceptance",
        );
        assert!(
            m.lhs.duration() > 15.0,
            "located {:.1}s of the ~18.6s shared block",
            m.lhs.duration()
        );
    }

    /// Spec 002 — stage attribution, pinned against the real failing input.
    ///
    /// The plan's first hypothesis was that a spurious over-long run was being
    /// chosen as the longest at a shift and then discarded by `bound_and_snap`,
    /// hiding a valid opening. The fixture refutes it: the failing pairs never
    /// reached the bounds check, because no shift was ever proposed. This test
    /// keeps that refutation from being quietly re-adopted — if intro recall
    /// regresses, it must not be blamed on the duration bounds again.
    #[test]
    fn the_bounds_check_is_not_what_discarded_the_real_openings() {
        let raw = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/mushoku_s03_fingerprints.txt"),
        )
        .expect("fixture readable");
        let intros: Vec<Vec<u32>> = raw
            .lines()
            .filter(|l| l.contains(" intro "))
            .filter_map(|l| l.split(' ').nth(2))
            .map(|hex| {
                hex.as_bytes()
                    .chunks(8)
                    .filter_map(|c| {
                        let s = std::str::from_utf8(c).ok()?;
                        u32::from_str_radix(s, 16).ok().map(u32::swap_bytes)
                    })
                    .collect()
            })
            .collect();
        assert_eq!(intros.len(), 5, "five episodes of intro fingerprints");

        let cfg = AlignConfig::default();
        // For every pair, the longest run at every shift the discovery step is
        // willing to test. If a pair fails, it must NOT be because every such run
        // fell outside [min_duration, max_duration].
        for i in 0..intros.len() {
            for j in (i + 1)..intros.len() {
                if compare(&intros[i], &intros[j], &cfg).is_some() {
                    continue;
                }
                let mut in_bounds_run_existed = false;
                for shift in feasible_shifts(&intros[i], &intros[j], &cfg) {
                    let (lhs_times, _) = matches_at_shift(&intros[i], &intros[j], shift, &cfg);
                    let Some(span) =
                        find_contiguous(&lhs_times, cfg.max_time_skip, cfg.secs_per_point)
                    else {
                        continue;
                    };
                    if bound_and_snap(span, &cfg).is_some() {
                        in_bounds_run_existed = true;
                    }
                }
                assert!(
                    !in_bounds_run_existed,
                    "pair {i}x{j} failed while an in-bounds run existed — the \
                     discard moved to a stage this test does not cover"
                );
            }
        }
    }
}
