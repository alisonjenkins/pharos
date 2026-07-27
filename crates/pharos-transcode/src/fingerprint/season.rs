//! Season-level intro/outro consensus (ADR-0018, improvement #4).
//!
//! The intro-skipper plugin keeps the single LONGEST pairwise match per
//! episode — one coincidental musical match then sets a bogus intro. We
//! instead pairwise-compare every episode, CLUSTER each episode's located
//! spans, and emit the consensus span when the agreement clears a threshold by
//! EITHER reading: the share of comparisons that agreed, or the share of
//! located AUDIO SECONDS that agreed (T102 + T107). Outliers are dropped, not
//! enshrined.
//!
//! Both readings are needed because they fail in opposite directions. By count,
//! a long true opening loses to a crowd of short coincidences. By duration, a
//! short true ending loses to a few long spurious matches. Requiring both loses
//! coverage twice over; `min_agreeing` remains the floor under either.
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
    /// Strength of agreement, 0..=1: the greater of the agreeing COMPARISON
    /// share and the agreeing SECONDS share (T102 + T107). A peer that located
    /// nothing offered no evidence either way and is not counted against it
    /// (B123).
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
    /// Minimum confidence to emit, met by EITHER the agreeing-comparison share
    /// or the agreeing-seconds share — a purity gate on the evidence obtained,
    /// not a quorum of the season.
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

/// Strength of agreement: the GREATER of the agreeing-comparison share and the
/// agreeing-seconds share (T102 + T107). See the body for why both are needed.
fn agreement_confidence(spans: &[Located], agreed_secs: f64, agreeing: u32, matched: u32) -> f64 {
    let by_count = agreeing as f64 / matched.max(1) as f64;
    let total_secs: f64 = spans.iter().map(|s| (s.end - s.start).max(0.0)).sum();
    if total_secs <= 0.0 {
        return by_count;
    }
    let by_duration = agreed_secs / total_secs;
    // T107 — agreement counts if EITHER reading finds it, because the two fail
    // in opposite directions and requiring both loses coverage twice over.
    //
    // By count alone, a long true opening loses to a crowd of short
    // coincidences (T102: Game of Thrones S1 at 3/7 = 0.43, whole season
    // dropped). By duration alone, a SHORT true ending loses to a few long
    // spurious matches — measured on the real fingerprints after T102 shipped:
    // Breaking Bad S5 credits went 6 emitted -> 0, Sabrina S1 9 -> 1,
    // library-wide outro rows 7025 -> 6947 while intro rows rose 6186 -> 6252.
    //
    // Neither ratio is "the" definition of agreement; each is evidence. Taking
    // the max on the same real fixtures dominates both: GoT S1 0/4/4,
    // Breaking Bad S5 6/0/6, Sabrina S1 9/3/12, Frieren S1 22/16/23
    // (count/duration/max). The controls stay silent — Cobra Kai S1 and 24 S2
    // emit 0 under all three — so this widens recall without inventing spans.
    //
    // `min_agreeing` remains the floor that stops a lone coincidence either way.
    by_count.max(by_duration)
}

/// Greedy 1-pass clustering: the largest group of spans whose endpoints are
/// all within `tol` of the group seed. Returns (consensus span, group size).
/// The consensus is the arithmetic mean of the winning group (robust-ish;
/// endpoints already agree within `tol`).
fn best_cluster(spans: &[Located], tol: f64) -> Option<(Span, u32, f64)> {
    let mut best: Option<(Span, u32, f64)> = None;
    for (i, seed) in spans.iter().enumerate() {
        let group: Vec<&Located> = spans
            .iter()
            .skip(i)
            .filter(|s| (s.start - seed.start).abs() <= tol && (s.end - seed.end).abs() <= tol)
            .collect();
        let n = group.len() as u32;
        let take = best.as_ref().map(|(_, bn, _)| n > *bn).unwrap_or(true);
        if take {
            let mean_start = group.iter().map(|s| s.start).sum::<f64>() / n as f64;
            let mean_end = group.iter().map(|s| s.end).sum::<f64>() / n as f64;
            let agreed_secs = group
                .iter()
                .map(|s| (s.end - s.start).max(0.0))
                .sum::<f64>();
            best = Some((
                Span {
                    start: mean_start,
                    end: mean_end,
                },
                n,
                agreed_secs,
            ));
        }
    }
    best
}

/// Why an episode did or did not receive a segment. A bounded set — it is a
/// metric label, so a new variant is a dashboard change, not a free addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Cleared both gates; a segment was emitted.
    Emitted,
    /// No comparison located a span for this episode at all.
    NoSpan,
    /// A cluster formed but fewer than `min_agreeing` comparisons agreed.
    FewAgreeing,
    /// Enough agreed, but comparisons locating a DIFFERENT span both
    /// outnumbered AND outweighed them — neither agreement share reached
    /// `min_confidence`.
    LowConfidence,
}

impl Verdict {
    /// Stable label for metrics. Renaming one breaks a dashboard silently.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Emitted => "emitted",
            Verdict::NoSpan => "no_span",
            Verdict::FewAgreeing => "few_agreeing",
            Verdict::LowConfidence => "low_confidence",
        }
    }
}

/// One episode's full detection record — the inputs to the gate as well as its
/// outcome, so a dropped episode can be explained rather than merely counted.
#[derive(Debug, Clone, Copy)]
pub struct EpisodeVerdict {
    pub id: u64,
    /// Comparisons that located a span for this episode (0..=n-1).
    pub matched: u32,
    /// Size of the winning cluster.
    pub agreeing: u32,
    pub confidence: f64,
    /// The consensus span, on the episode's own timeline. `None` when nothing
    /// clustered.
    pub span: Option<Span>,
    pub verdict: Verdict,
}

/// Detect the shared span (intro OR credits — the caller picks the window)
/// across a season's episodes, returning a consensus segment per episode that
/// cleared the confidence gate. Episodes with too few agreeing comparisons are
/// omitted (no bogus segment).
///
/// `O(n²)` pairwise `compare`. For incremental single-episode adds, prefer a
/// stored reference fingerprint (ADR-0018 improvement #2) rather than this.
pub fn detect_season(eps: &[EpisodeFingerprint], cfg: &SeasonConfig) -> Vec<SeasonSegment> {
    detect_season_verbose(eps, cfg)
        .into_iter()
        .filter(|v| v.verdict == Verdict::Emitted)
        .filter_map(|v| {
            v.span.map(|s| SeasonSegment {
                id: v.id,
                start_secs: s.start,
                end_secs: s.end,
                confidence: v.confidence,
                agreeing: v.agreeing,
            })
        })
        .collect()
}

/// [`detect_season`] with every episode's verdict retained, including the ones
/// that were dropped and why. The caller instruments from this: a detector that
/// silently emits nothing for two thirds of a library is the shape that hides
/// a recall problem, and the counts that explain it exist only here.
pub fn detect_season_verbose(
    eps: &[EpisodeFingerprint],
    cfg: &SeasonConfig,
) -> Vec<EpisodeVerdict> {
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
    let mut out = Vec::with_capacity(n);
    for (idx, spans) in located.iter().enumerate() {
        let matched = spans.len() as u32;
        let Some((consensus, agreeing, agreed_secs)) =
            best_cluster(spans, cfg.cluster_tolerance_secs)
        else {
            out.push(EpisodeVerdict {
                id: eps[idx].id,
                matched,
                agreeing: 0,
                confidence: 0.0,
                span: None,
                verdict: Verdict::NoSpan,
            });
            continue;
        };
        // B123 — the denominator is the comparisons that LOCATED a span for
        // this episode, not every peer in the season. A peer that produced no
        // span is not a peer that disagreed; it is a peer that offered no
        // evidence, and counting it as dissent made recall fall as a season
        // got longer. Measured on a real 20-episode season: episode 12 had
        // NINE independent episodes agree on the same 22.0 s span — the same
        // intro length as every emitted one — and was discarded because
        // 9 < half of 19. Against matched comparisons it scores 1.00.
        //
        // T102 — weighted by matched DURATION, not by comparison COUNT. Counting
        // votes treats a 109 s agreement and an 18 s fragment as equal evidence,
        // so a correct answer can be outvoted by noise. Measured on the real
        // stored fingerprints for Game of Thrones S1 (which opens with its full
        // ~111 s title sequence at t=0 — the span S2 emits at confidence
        // 0.63-0.88): episode 5 had THREE peers agree on 0.00..109 and four
        // locate scattered 17-28 s fragments elsewhere. By count that is
        // 3/7 = 0.43 and the season emits nothing; by duration it is
        // 327/415 = 0.79 and the true intro survives. Every GoT S1 episode
        // failed this way, at 0.38-0.43, while adjacent seasons emitted.
        //
        // `min_agreeing` still guards the other direction: one long spurious
        // match cannot carry an episode on its own.
        let confidence = agreement_confidence(spans, agreed_secs, agreeing, matched);
        let verdict = if agreeing < cfg.min_agreeing {
            Verdict::FewAgreeing
        } else if confidence < cfg.min_confidence {
            Verdict::LowConfidence
        } else {
            Verdict::Emitted
        };
        let off = eps[idx].window_offset_secs;
        out.push(EpisodeVerdict {
            id: eps[idx].id,
            matched,
            agreeing,
            confidence,
            span: Some(Span {
                start: consensus.start + off,
                end: consensus.end + off,
            }),
            verdict,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::super::align::sample_duration_secs;
    use super::*;

    /// Non-shared filler audio. Each point is AVALANCHED, not stepped: an
    /// arithmetic ramp (`seed*C + i*step`) makes two fillers differ by a
    /// constant at every index, so under some shift a long run of them lands
    /// inside the 6-bit acceptance and a pair of unrelated episodes appears to
    /// share a span. Real chromaprint points do not march in step like that, so
    /// the ramp was modelling an alignment that cannot occur — it read as
    /// "unrelated audio" only because shift discovery used to be too narrow to
    /// find it.
    fn filler(seed: u32, n: usize) -> Vec<u32> {
        (0..n)
            .map(|i| {
                let mut x = seed
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add((i as u32).wrapping_mul(2_246_822_519));
                x ^= x >> 15;
                x = x.wrapping_mul(2_246_822_519);
                x ^= x >> 13;
                x
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

    /// T102 — the Game of Thrones S1 shape, taken from production. Three peers
    /// agree on the full ~109 s title sequence; four locate short coincidental
    /// fragments elsewhere. By COUNT that is 3/7 = 0.43 and the true intro is
    /// discarded; by DURATION it is 327/415 = 0.79 and it survives.
    ///
    /// Disarm by reverting `confidence` to `agreeing / matched` and this goes
    /// red — which is the whole point: every GoT S1 episode failed exactly here
    /// at 0.38-0.43 while adjacent seasons emitted the same span at 0.63-0.88.
    #[test]
    fn a_long_agreement_outweighs_a_crowd_of_short_coincidences() {
        let spans = [
            Located {
                start: 0.0,
                end: 109.45,
            },
            Located {
                start: 0.0,
                end: 107.96,
            },
            Located {
                start: 0.0,
                end: 109.70,
            },
            Located {
                start: 0.0,
                end: 17.33,
            },
            Located {
                start: 29.47,
                end: 48.04,
            },
            Located {
                start: 70.57,
                end: 94.10,
            },
            Located {
                start: 70.82,
                end: 99.30,
            },
        ];
        let cfg = SeasonConfig::default();
        let (consensus, agreeing, agreed_secs) =
            best_cluster(&spans, cfg.cluster_tolerance_secs).expect("a cluster");
        assert_eq!(agreeing, 3, "the three full-length spans are the cluster");
        assert!(
            consensus.end > 100.0,
            "consensus should be the title sequence, got {consensus:?}"
        );

        // Through the SAME function the detector uses, so disarming the rule
        // fails this rather than leaving the test computing its own answer.
        let matched = spans.len() as u32;
        let confidence = agreement_confidence(&spans, agreed_secs, agreeing, matched);
        assert!(
            confidence >= cfg.min_confidence,
            "duration-weighted confidence {confidence:.2} must clear {:.2}; \
             by comparison count it would be {:.2} and the real intro would be lost",
            cfg.min_confidence,
            3.0 / matched as f64
        );
    }

    /// T107 — the mirror of the T102 case, and the regression it caused. A
    /// SHORT true ending (6 peers agreeing on 30 s) outweighed by a few LONG
    /// spurious matches (4 peers at 90 s): by duration that is 180/540 = 0.33
    /// and the season emits nothing; by count it is 6/10 = 0.60 and it stands.
    ///
    /// Measured, not hypothetical: after duration-only weighting shipped,
    /// Breaking Bad S5 credits went 6 -> 0 and library outro rows fell
    /// 7025 -> 6947.
    ///
    /// Disarm by returning `by_duration` alone and this goes red.
    #[test]
    fn a_short_agreement_survives_a_few_long_coincidences() {
        let mut spans: Vec<Located> = (0..6)
            .map(|i| Located {
                start: 100.0 + i as f64 * 0.1,
                end: 130.0 + i as f64 * 0.1,
            })
            .collect();
        spans.extend((0..4).map(|i| Located {
            start: 200.0 + i as f64 * 20.0,
            end: 290.0 + i as f64 * 20.0,
        }));

        let cfg = SeasonConfig::default();
        let (_, agreeing, agreed_secs) =
            best_cluster(&spans, cfg.cluster_tolerance_secs).expect("a cluster");
        assert_eq!(agreeing, 6, "the six short spans are the cluster");

        let matched = spans.len() as u32;
        let conf = agreement_confidence(&spans, agreed_secs, agreeing, matched);
        let by_duration = agreed_secs / spans.iter().map(|s| s.end - s.start).sum::<f64>();
        assert!(
            by_duration < cfg.min_confidence,
            "precondition: by duration alone this must FAIL ({by_duration:.2})"
        );
        assert!(
            conf >= cfg.min_confidence,
            "a 6-of-10 majority must still emit; got {conf:.2}"
        );
    }

    /// The other direction: `min_agreeing` must still stop ONE long
    /// coincidental match from carrying an episode on its own, which is the
    /// risk duration weighting introduces.
    #[test]
    fn one_long_coincidence_still_cannot_carry_an_episode() {
        let eps = vec![
            EpisodeFingerprint {
                id: 1,
                points: filler(1, 400),
                window_offset_secs: 0.0,
            },
            EpisodeFingerprint {
                id: 2,
                points: filler(2, 400),
                window_offset_secs: 0.0,
            },
        ];
        let segs = detect_season(&eps, &SeasonConfig::default());
        assert!(
            segs.is_empty(),
            "unrelated filler must not emit under duration weighting, got {segs:?}"
        );
    }

    /// B123 — the recall bug, in miniature. A season where only some pairs
    /// align must still emit for the episodes whose alignments all agreed:
    /// a peer that located nothing offered no evidence, and counting it as
    /// dissent is what dropped an episode nine peers agreed with on a real
    /// 20-episode season.
    #[test]
    fn agreement_is_measured_against_the_evidence_obtained() {
        let intro = intro_block(160);
        // Six episodes share the intro; four more share nothing with anyone,
        // so every sharer matches 5 of its 9 peers — a minority of the season.
        let mut eps: Vec<EpisodeFingerprint> = (1..=6)
            .map(|i| ep(i, 8 + (i as usize * 5), &intro, 0.0))
            .collect();
        for i in 7..=10u64 {
            eps.push(EpisodeFingerprint {
                id: i,
                points: filler(i as u32 * 31, 300),
                window_offset_secs: 0.0,
            });
        }
        let segs = detect_season(&eps, &SeasonConfig::default());
        let ids: Vec<u64> = segs.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec![1, 2, 3, 4, 5, 6],
            "every episode carrying the shared intro must be emitted; \
             against ALL peers each scores 5/9 = 0.56 and a season with more \
             non-sharers would fall under the gate purely for being longer"
        );
        for s in &segs {
            assert!(
                s.confidence > 0.99,
                "all five located spans agreed, so the evidence is unanimous: {}",
                s.confidence
            );
        }
    }

    #[test]
    fn single_episode_season_is_empty() {
        let eps = vec![ep(1, 10, &intro_block(160), 0.0)];
        assert!(detect_season(&eps, &SeasonConfig::default()).is_empty());
    }

    #[test]
    fn sample_duration_is_positive_and_sane() {
        // The whole timeline math depends on this hop; guard it's a plausible
        // fingerprint item duration (well under a second, above zero).
        let d = sample_duration_secs();
        assert!(d > 0.0 && d < 1.0, "implausible hop {d}");
    }
}
