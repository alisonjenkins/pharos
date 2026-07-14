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

use std::collections::HashMap;

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
    /// Fuzzy point-value probe range (±) when discovering candidate shifts.
    /// intro-skipper `InvertedIndexShift` = 2.
    pub index_shift: i64,
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
            index_shift: 2,
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

/// Build `point value → last index` (the inverted index). Last-wins matches
/// the plugin; repeated values are rare and either index works as a shift seed.
fn inverted_index(fp: &[u32]) -> HashMap<u32, usize> {
    let mut m = HashMap::with_capacity(fp.len());
    for (i, &p) in fp.iter().enumerate() {
        m.insert(p, i);
    }
    m
}

/// Candidate alignment shifts (`rhs_index - lhs_index`) worth testing, found by
/// fuzzy-probing the inverted indexes (`point ± index_shift`). Far cheaper than
/// brute-forcing every shift.
fn candidate_shifts(lhs: &[u32], rhs: &[u32], cfg: &AlignConfig) -> Vec<i64> {
    let lhs_idx = inverted_index(lhs);
    let rhs_idx = inverted_index(rhs);
    let mut shifts = std::collections::HashSet::new();
    for (&point, &li) in &lhs_idx {
        for d in -cfg.index_shift..=cfg.index_shift {
            let modified = point.wrapping_add(d as u32);
            if let Some(&ri) = rhs_idx.get(&modified) {
                shifts.insert(ri as i64 - li as i64);
            }
        }
    }
    shifts.into_iter().collect()
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
    for shift in candidate_shifts(lhs, rhs, cfg) {
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
}
