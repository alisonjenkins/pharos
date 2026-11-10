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
    // coincidences (T102: Show AD S1 at 3/7 = 0.43, whole season
    // dropped). By duration alone, a SHORT true ending loses to a few long
    // spurious matches — measured on the real fingerprints after T102 shipped:
    // Show X S5 credits went 6 emitted -> 0, Show AF S1 9 -> 1,
    // library-wide outro rows 7025 -> 6947 while intro rows rose 6186 -> 6252.
    //
    // Neither ratio is "the" definition of agreement; each is evidence. Taking
    // the max on the same real fixtures dominates both: GoT S1 0/4/4,
    // Show X S5 6/0/6, Show AF S1 9/3/12, Show AG S1 22/16/23
    // (count/duration/max). The controls stay silent — Show AE S1 and Show AH S2
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

/// T106 (B1060) — the region of an episode that at least `min_agreeing`
/// located spans cover at once, for when a match FRAGMENTS. At the strict
/// `max_bit_diff` one opening is often located as several sub-ranges of
/// itself (one measured season: pair 2x3 locates 8.91..103.26, pair 2x5 only
/// 66.36..94.84 inside it), and no two fragments share an endpoint, so
/// [`best_cluster`] sees only singletons.
///
/// The region is where coverage reaches `min_agreeing`, never the union of
/// the spans: a union of 8.17..63 and 8.91..103 is a Skip button that runs
/// 40 s too long, while their common cover 8.91..63 is attested by both.
/// Adjacent covered runs merge only when ONE located span bridges the gap, so
/// two fragments of one opening join but unrelated runs do not chain.
///
/// A span votes for the region only when at least half of it lies inside, so
/// one long spurious match sweeping across the region adds no weight. The
/// returned seconds are the voters' overlap with the region.
///
/// Returns (region, voters, agreed seconds) for the region with the most
/// agreed seconds that is at least `min_secs` and at most `max_secs` long.
/// The cap matters because several bridging spans can chain runs into a
/// region longer than any one comparison located.
fn overlap_consensus(
    spans: &[Located],
    min_agreeing: u32,
    min_secs: f64,
    max_secs: f64,
) -> Option<(Span, u32, f64)> {
    let need = min_agreeing.max(1) as i64;
    let mut edges: Vec<(f64, i64)> = spans
        .iter()
        .filter(|s| s.end > s.start)
        .flat_map(|s| [(s.start, 1), (s.end, -1)])
        .collect();
    // Ends before starts at one instant: spans that only touch do not overlap.
    edges.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut runs: Vec<Span> = Vec::new();
    let mut depth = 0i64;
    let mut open: Option<f64> = None;
    for (t, step) in edges {
        depth += step;
        match open {
            None if depth >= need => open = Some(t),
            Some(start) if depth < need => {
                if t > start {
                    runs.push(Span { start, end: t });
                }
                open = None;
            }
            _ => {}
        }
    }

    let mut merged: Vec<Span> = Vec::new();
    for run in runs {
        let bridged = merged.last().is_some_and(|prev| {
            spans
                .iter()
                .any(|s| s.start <= prev.end && s.end >= run.start)
        });
        match merged.last_mut() {
            Some(prev) if bridged => prev.end = run.end,
            _ => merged.push(run),
        }
    }

    let mut best: Option<(Span, u32, f64)> = None;
    for region in merged
        .into_iter()
        .filter(|r| (min_secs..=max_secs).contains(&(r.end - r.start)))
    {
        let mut voters = 0u32;
        let mut agreed = 0.0;
        for s in spans {
            let len = s.end - s.start;
            let inside = s.end.min(region.end) - s.start.max(region.start);
            if len > 0.0 && inside >= len / 2.0 {
                voters += 1;
                agreed += inside;
            }
        }
        if best.as_ref().map_or(true, |(_, _, b)| agreed > *b) {
            best = Some((region, voters, agreed));
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

/// Which reading of the located spans produced an episode's consensus. A
/// bounded set — it is a metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consensus {
    /// Enough spans agreed on BOTH endpoints within `cluster_tolerance_secs`.
    Endpoints,
    /// The endpoint reading failed, but enough spans covered one region of
    /// the episode: a fragmented match of a single opening (T106, B1060).
    Overlap,
}

impl Consensus {
    /// Stable label for metrics. Renaming one breaks a dashboard silently.
    pub fn label(self) -> &'static str {
        match self {
            Consensus::Endpoints => "endpoints",
            Consensus::Overlap => "overlap",
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
    /// The reading the emitted span came from; `None` unless `Emitted`.
    pub consensus: Option<Consensus>,
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
                consensus: None,
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
        // stored fingerprints for Show AD S1 (which opens with its full
        // ~111 s title sequence at t=0 — the span S2 emits at confidence
        // 0.63-0.88): episode 5 had THREE peers agree on 0.00..109 and four
        // locate scattered 17-28 s fragments elsewhere. By count that is
        // 3/7 = 0.43 and the season emits nothing; by duration it is
        // 327/415 = 0.79 and the true intro survives. Every GoT S1 episode
        // failed this way, at 0.38-0.43, while adjacent seasons emitted.
        //
        // `min_agreeing` still guards the other direction: one long spurious
        // match cannot carry an episode on its own.
        let gate = |agreeing: u32, confidence: f64| {
            if agreeing < cfg.min_agreeing {
                Verdict::FewAgreeing
            } else if confidence < cfg.min_confidence {
                Verdict::LowConfidence
            } else {
                Verdict::Emitted
            }
        };
        let confidence = agreement_confidence(spans, agreed_secs, agreeing, matched);
        let mut reading = (consensus, agreeing, confidence, gate(agreeing, confidence));
        let mut method = Consensus::Endpoints;
        // T106 — the overlap reading is a fallback only. An episode the
        // endpoint reading emits keeps exactly its T102/T107 span, so this can
        // add Skip buttons but never move or remove one. A dropped episode
        // keeps the endpoint reading's reason, which is what explains it.
        if reading.3 != Verdict::Emitted {
            if let Some((region, voters, secs)) = overlap_consensus(
                spans,
                cfg.min_agreeing,
                cfg.align.min_duration,
                cfg.align.max_duration,
            ) {
                let conf = agreement_confidence(spans, secs, voters, matched);
                if gate(voters, conf) == Verdict::Emitted {
                    reading = (region, voters, conf, Verdict::Emitted);
                    method = Consensus::Overlap;
                }
            }
        }
        let (consensus, agreeing, confidence, verdict) = reading;
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
            consensus: (verdict == Verdict::Emitted).then_some(method),
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
        let verdicts = detect_season_verbose(&eps, &SeasonConfig::default());
        assert!(
            verdicts
                .iter()
                .all(|v| v.consensus == Some(Consensus::Endpoints)),
            "an agreeing season keeps the endpoint reading: {verdicts:?}"
        );
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

    /// T102 — the Show AD S1 shape, taken from production. Three peers
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
    /// Show X S5 credits went 6 -> 0 and library outro rows fell
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

    fn located(spans: &[(f64, f64)]) -> Vec<Located> {
        spans
            .iter()
            .map(|&(start, end)| Located { start, end })
            .collect()
    }

    /// T106 (B1060) — the shape of the measured season. Two episodes carry the
    /// whole opening; three carry only a different third of it, so every
    /// comparison against those three locates a FRAGMENT of the opening and
    /// no two fragments share an endpoint. By endpoints the full-opening
    /// episodes get one agreeing comparison each and are dropped.
    ///
    /// Disarm by deleting the overlap fallback in `detect_season_verbose` and
    /// this goes red with episodes 1 and 2 missing.
    #[test]
    fn a_fragmented_opening_still_gets_a_segment() {
        let intro = intro_block(450); // ~111 s, a full title sequence
        let slice = |from: usize, to: usize| intro[from..to].to_vec();
        let eps = vec![
            ep(1, 10, &intro, 0.0),
            ep(2, 36, &intro, 0.0),
            ep(3, 20, &slice(0, 140), 0.0),
            ep(4, 28, &slice(150, 290), 0.0),
            ep(5, 14, &slice(300, 450), 0.0),
        ];
        let verdicts = detect_season_verbose(&eps, &SeasonConfig::default());
        let hop = sample_duration_secs();
        for id in [1u64, 2] {
            let v = verdicts.iter().find(|v| v.id == id).expect("a verdict");
            assert_eq!(v.verdict, Verdict::Emitted, "episode {id}: {v:?}");
            assert_eq!(v.consensus, Some(Consensus::Overlap), "episode {id}");
            let span = v.span.expect("a span");
            let head = if id == 1 { 10.0 } else { 36.0 } * hop;
            assert!(
                (span.start - head).abs() < 3.0 && (span.end - (head + 450.0 * hop)).abs() < 3.0,
                "episode {id} must get the whole opening, not one fragment: {span:?}"
            );
        }
    }

    /// The hazard T106 names: a naive overlap-merge unions 8.17..63 with
    /// 8.91..103 into a span neither comparison located. The region two spans
    /// agree on is their common cover.
    #[test]
    fn overlap_consensus_is_the_common_cover_not_the_union() {
        let spans = located(&[(8.17, 63.0), (8.91, 103.0)]);
        let (region, voters, _) = overlap_consensus(&spans, 2, 15.0, 120.0).expect("a region");
        assert_eq!(voters, 2);
        assert!(
            (region.start - 8.91).abs() < 1e-9 && (region.end - 63.0).abs() < 1e-9,
            "got {region:?}"
        );
    }

    /// Fragments that cover nothing in common are coincidences, not one
    /// opening: no second is covered twice, so no region forms.
    #[test]
    fn scattered_fragments_form_no_overlap_region() {
        let spans = located(&[(0.0, 17.3), (29.5, 48.0), (70.6, 94.1), (200.0, 230.0)]);
        assert!(overlap_consensus(&spans, 2, 15.0, 120.0).is_none());
    }

    /// One long spurious span sweeping across a short shared region covers
    /// it, but mostly lies outside it, so it adds no vote and the region stays
    /// short of `min_agreeing`.
    #[test]
    fn a_long_span_sweeping_across_a_region_does_not_vote_for_it() {
        let spans = located(&[(0.0, 120.0), (40.0, 60.0)]);
        let (_, voters, _) = overlap_consensus(&spans, 2, 15.0, 120.0).expect("a covered region");
        assert_eq!(voters, 1, "only the 20 s span lies inside 40..60");
    }

    /// Two covered runs join only through ONE span that bridges the gap.
    #[test]
    fn covered_runs_join_only_through_a_bridging_span() {
        // Runs 10..30 and 50..80, both covered twice; no span spans 30..50.
        let apart = located(&[(10.0, 30.0), (5.0, 32.0), (50.0, 80.0), (48.0, 85.0)]);
        let (region, _, _) = overlap_consensus(&apart, 2, 15.0, 120.0).expect("a region");
        assert!(region.start >= 48.0, "the runs must not chain: {region:?}");

        let mut bridged = apart.clone();
        bridged.push(Located {
            start: 8.0,
            end: 82.0,
        });
        let (region, _, _) = overlap_consensus(&bridged, 2, 15.0, 120.0).expect("a region");
        assert!(
            region.start <= 10.0 && region.end >= 80.0,
            "one span covering the gap joins them: {region:?}"
        );
    }

    /// Bridging spans can chain runs into a region longer than anything a
    /// comparison located; past `max_secs` it is not an intro.
    #[test]
    fn a_chained_region_longer_than_the_cap_is_not_offered() {
        // Three runs each covered twice (0..40, 60..100, 120..160), chained by
        // two bridging spans into one 0..160 region.
        let spans = located(&[
            (0.0, 40.0),
            (0.0, 40.0),
            (60.0, 100.0),
            (60.0, 100.0),
            (120.0, 160.0),
            (120.0, 160.0),
            (30.0, 70.0),
            (90.0, 130.0),
        ]);
        let (region, _, _) = overlap_consensus(&spans, 2, 15.0, 200.0).expect("a region");
        assert!(
            region.end - region.start > 120.0,
            "precondition: chained {region:?}"
        );
        assert!(
            overlap_consensus(&spans, 2, 15.0, 120.0).is_none(),
            "a 160 s chain must not pass a 120 s cap"
        );
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
