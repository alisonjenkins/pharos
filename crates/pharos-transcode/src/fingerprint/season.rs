//! Season-level intro/outro consensus (ADR-0018, improvement #4).
//!
//! The intro-skipper plugin keeps the single LONGEST pairwise match per
//! episode — one coincidental musical match then sets a bogus intro. We
//! instead pairwise-compare every episode, CLUSTER each episode's located
//! spans, and emit the consensus span with a **confidence** = fraction of
//! comparisons that agreed. A segment is only served when confidence clears a
//! threshold, so outliers are dropped, not enshrined.
//!
//! Pure (operates on already-computed fingerprints), so the whole
//! season-aggregation policy is unit-tested without ffmpeg.

use super::align::{compare, AlignConfig, Span};

/// One episode's fingerprint for a given window (intro head or credits tail).
pub struct EpisodeFingerprint {
    /// Opaque caller id (a media id) — echoed back on the result.
    pub id: u64,
    /// The fingerprint points for the window.
    pub points: Vec<u32>,
    /// Seconds the window's zero is offset into the episode (0 for the intro
    /// head window; the credits-window start for the tail). Added to the
    /// emitted span so it lands on the real episode timeline.
    pub window_offset_secs: f64,
}

/// The agreed span for one episode plus how strongly the season agreed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeasonSegment {
    pub id: u64,
    pub start_secs: f64,
    pub end_secs: f64,
    /// Fraction of this episode's comparisons that landed in the winning
    /// cluster — 0..=1. Higher = more episodes independently agreed.
    pub confidence: f64,
    /// How many comparisons agreed (the winning cluster size).
    pub agreeing: u32,
}

/// Consensus tuning.
#[derive(Debug, Clone, Copy)]
pub struct SeasonConfig {
    pub align: AlignConfig,
    /// Two spans are "the same" when both endpoints are within this many
    /// seconds — the clustering tolerance.
    pub cluster_tolerance_secs: f64,
    /// Minimum comparisons that must agree to emit a segment (≥2 stops a
    /// single coincidental pair from setting a segment).
    pub min_agreeing: u32,
    /// Minimum confidence (agreeing / comparisons) to emit.
    pub min_confidence: f64,
}

impl Default for SeasonConfig {
    fn default() -> Self {
        Self {
            align: AlignConfig::default(),
            cluster_tolerance_secs: 3.0,
            min_agreeing: 2,
            min_confidence: 0.5,
        }
    }
}

/// A raw located span for one episode, before clustering.
#[derive(Clone, Copy)]
struct Located {
    start: f64,
    end: f64,
}

/// Greedy 1-pass clustering: the largest group of spans whose endpoints are
/// all within `tol` of the group seed. Returns (consensus span, group size).
/// The consensus is the arithmetic mean of the winning group (robust-ish;
/// endpoints already agree within `tol`).
fn best_cluster(spans: &[Located], tol: f64) -> Option<(Span, u32)> {
    let mut best: Option<(Span, u32)> = None;
    for (i, seed) in spans.iter().enumerate() {
        let group: Vec<&Located> = spans
            .iter()
            .skip(i)
            .filter(|s| (s.start - seed.start).abs() <= tol && (s.end - seed.end).abs() <= tol)
            .collect();
        let n = group.len() as u32;
        let take = best.as_ref().map(|(_, bn)| n > *bn).unwrap_or(true);
        if take {
            let mean_start = group.iter().map(|s| s.start).sum::<f64>() / n as f64;
            let mean_end = group.iter().map(|s| s.end).sum::<f64>() / n as f64;
            best = Some((
                Span {
                    start: mean_start,
                    end: mean_end,
                },
                n,
            ));
        }
    }
    best
}

/// Detect the shared span (intro OR credits — the caller picks the window)
/// across a season's episodes, returning a consensus segment per episode that
/// cleared the confidence gate. Episodes with too few agreeing comparisons are
/// omitted (no bogus segment).
///
/// `O(n²)` pairwise `compare`. For incremental single-episode adds, prefer a
/// stored reference fingerprint (ADR-0018 improvement #2) rather than this.
pub fn detect_season(eps: &[EpisodeFingerprint], cfg: &SeasonConfig) -> Vec<SeasonSegment> {
    let n = eps.len();
    if n < 2 {
        return Vec::new();
    }
    // Per-episode list of spans located for it across all its comparisons.
    let mut located: Vec<Vec<Located>> = vec![Vec::new(); n];
    for i in 0..n {
        for j in (i + 1)..n {
            if let Some(m) = compare(&eps[i].points, &eps[j].points, &cfg.align) {
                located[i].push(Located {
                    start: m.lhs.start,
                    end: m.lhs.end,
                });
                located[j].push(Located {
                    start: m.rhs.start,
                    end: m.rhs.end,
                });
            }
        }
    }
    let comparisons_per_ep = (n - 1) as f64;
    let mut out = Vec::new();
    for (idx, spans) in located.iter().enumerate() {
        let Some((consensus, agreeing)) = best_cluster(spans, cfg.cluster_tolerance_secs) else {
            continue;
        };
        let confidence = agreeing as f64 / comparisons_per_ep;
        if agreeing < cfg.min_agreeing || confidence < cfg.min_confidence {
            continue;
        }
        let off = eps[idx].window_offset_secs;
        out.push(SeasonSegment {
            id: eps[idx].id,
            start_secs: consensus.start + off,
            end_secs: consensus.end + off,
            confidence,
            agreeing,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::align::SAMPLE_DURATION_SECS;
    use super::*;

    fn filler(seed: u32, n: usize) -> Vec<u32> {
        (0..n)
            .map(|i| {
                seed.wrapping_mul(2_654_435_761)
                    .wrapping_add(i as u32 * 40_503)
            })
            .collect()
    }
    fn intro_block(n: usize) -> Vec<u32> {
        (0..n)
            .map(|i| 0xA53C_0000 ^ (i as u32).wrapping_mul(2_246_822_519))
            .collect()
    }
    fn ep(id: u64, head_len: usize, intro: &[u32], off: f64) -> EpisodeFingerprint {
        let mut points = filler(id as u32 + 7, head_len);
        points.extend_from_slice(intro);
        points.extend_from_slice(&filler(id as u32 + 99, 120));
        EpisodeFingerprint {
            id,
            points,
            window_offset_secs: off,
        }
    }

    #[test]
    fn agreeing_season_yields_confident_segments() {
        let intro = intro_block(160); // ~19.8 s
                                      // 4 episodes sharing the intro at varied offsets.
        let eps = vec![
            ep(1, 8, &intro, 0.0),
            ep(2, 40, &intro, 0.0),
            ep(3, 12, &intro, 0.0),
            ep(4, 25, &intro, 0.0),
        ];
        let segs = detect_season(&eps, &SeasonConfig::default());
        assert_eq!(segs.len(), 4, "every episode gets a segment");
        for s in &segs {
            assert!(s.confidence >= 0.9, "high agreement, got {}", s.confidence);
            assert!((s.end_secs - s.start_secs) > 15.0);
        }
    }

    #[test]
    fn window_offset_lands_on_real_timeline() {
        // Credits window: fingerprints are zero-based but the window starts
        // 1200 s into the episode.
        let outro = intro_block(160);
        // 3 episodes so the consensus min_agreeing=2 can be met.
        let eps = vec![
            ep(1, 5, &outro, 1200.0),
            ep(2, 5, &outro, 1200.0),
            ep(3, 20, &outro, 1200.0),
        ];
        let segs = detect_season(&eps, &SeasonConfig::default());
        assert_eq!(segs.len(), 3);
        // start = intro offset (snapped to 0) + 1200 window offset.
        assert!(
            segs[0].start_secs >= 1200.0,
            "offset applied: {}",
            segs[0].start_secs
        );
    }

    #[test]
    fn lone_coincidental_pair_is_dropped() {
        // 3 episodes: eps 1&2 share an intro; ep 3 shares nothing. Ep 3 gets
        // at most 0 agreeing comparisons → no segment. Eps 1&2 have only ONE
        // agreeing comparison each (< min_agreeing=2) → also dropped, so a
        // 2-of-3 coincidence never sets a season-wide segment.
        let intro = intro_block(160);
        let eps = vec![
            ep(1, 10, &intro, 0.0),
            ep(2, 30, &intro, 0.0),
            EpisodeFingerprint {
                id: 3,
                points: filler(555, 300),
                window_offset_secs: 0.0,
            },
        ];
        let segs = detect_season(&eps, &SeasonConfig::default());
        assert!(
            segs.is_empty(),
            "1 agreeing comparison < min_agreeing, got {segs:?}"
        );
    }

    #[test]
    fn single_episode_season_is_empty() {
        let eps = vec![ep(1, 10, &intro_block(160), 0.0)];
        assert!(detect_season(&eps, &SeasonConfig::default()).is_empty());
    }

    #[test]
    fn sample_duration_is_chromaprint_hop() {
        // Guard the constant the whole timeline math depends on.
        assert!((SAMPLE_DURATION_SECS - 0.123_82).abs() < 1e-4);
    }
}
