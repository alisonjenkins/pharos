//! FR-007 / FR-010: from aligner scores to a verdict per track, never per cue.

/// FR-007 thresholds. Defaults are the starting values from the 2026-11-12 spike;
/// FR-014 calibrates them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    /// `T_median`: the median probe score that fits on its own.
    pub median: f32,
    /// `T_line`: a probe at or above this is a hit; half the probes hitting fits.
    pub line: f32,
    /// `T_margin`: how far the probe median must beat the decoy median.
    pub margin: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            median: 0.45,
            line: 0.5,
            margin: 0.2,
        }
    }
}

/// FR-010: an offset under this is the aligner's own noise (its `NEGLIGIBLE_MS`).
pub const NEGLIGIBLE_MS: i64 = 400;
/// FR-010: `segmented` needs probe groups at offsets further apart than this.
pub const SEGMENT_GAP_MS: i64 = 2_000;
/// FR-010: each `segmented` group has at least this many probes.
pub const SEGMENT_MIN_PROBES: usize = 2;

/// Median of `scores`, with a non-finite score counted as 0. 0 when empty.
pub fn median(scores: &[f32]) -> f32 {
    let mut v: Vec<f32> = scores
        .iter()
        .map(|s| if s.is_finite() { *s } else { 0.0 })
        .collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    let mid = v.len() / 2;
    let at = |i: usize| v.get(i).copied().unwrap_or(0.0);
    if v.len() % 2 == 0 {
        (at(mid - 1) + at(mid)) / 2.0
    } else {
        at(mid)
    }
}

/// The scores of one candidate timing: the probes at that timing and their decoys
/// in the same windows.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Total shift tried, aligner estimate included.
    pub offset_ms: i64,
    /// Speed ratio tried (1.0 in stage 1).
    pub ratio: f64,
    /// One score per probe, in probe order.
    pub probes: Vec<f32>,
    /// One score per decoy, in probe order.
    pub decoys: Vec<f32>,
}

/// FR-007's numbers for one candidate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub median: f32,
    pub decoy: f32,
    pub hits: usize,
    pub n: usize,
}

impl Candidate {
    pub fn fit(&self, t: &Thresholds) -> Fit {
        Fit {
            median: median(&self.probes),
            decoy: median(&self.decoys),
            hits: self.hits(t).count(),
            n: self.probes.len(),
        }
    }

    fn hits<'a>(&'a self, t: &'a Thresholds) -> impl Iterator<Item = usize> + 'a {
        self.probes
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_finite() && **s >= t.line)
            .map(|(i, _)| i)
    }
}

impl Fit {
    /// FR-007: (median or half the probes) and a margin over the decoys.
    pub fn fits(&self, t: &Thresholds) -> bool {
        let strong = self.median >= t.median || (self.n > 0 && 2 * self.hits >= self.n);
        strong && self.median - self.decoy >= t.margin
    }

    /// Decoy-level: the probes do no better than text from elsewhere.
    pub fn decoy_level(&self, t: &Thresholds) -> bool {
        self.median - self.decoy < t.margin
    }
}

impl std::fmt::Display for Fit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "median={:.2} decoy={:.2} hits={}/{}",
            self.median, self.decoy, self.hits, self.n
        )
    }
}

/// A track's verdict (FR-010). [`Verdict::label`] is the metric label;
/// `Display` is the stored reason.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Fits {
        fit: Fit,
    },
    Retimed {
        fit: Fit,
        offset_ms: i64,
        ratio: f64,
    },
    WrongVideo {
        fit: Fit,
    },
    Segmented {
        offsets_ms: Vec<i64>,
    },
    Unknown {
        class: UnknownClass,
        reason: String,
    },
}

/// Bounded reason classes of `unknown`, for the metric's `reason_class` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnknownClass {
    /// FR-002: track and audio languages differ.
    LanguageMismatch,
    /// FR-002: not in the Latin alphabet.
    Script,
    /// FR-003.
    LanguageUntagged,
    /// FR-004.
    TooFewLines,
    /// FR-015a: retries used up.
    AlignerError,
    /// Stage 2 has not run: no `wrong_video` without the wide search (FR-008).
    NotSearched,
    /// FR-010: the best candidate is neither fitting nor decoy-level.
    Inconclusive,
}

impl UnknownClass {
    pub const ALL: [Self; 7] = [
        Self::LanguageMismatch,
        Self::Script,
        Self::LanguageUntagged,
        Self::TooFewLines,
        Self::AlignerError,
        Self::NotSearched,
        Self::Inconclusive,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::LanguageMismatch => "language_mismatch",
            Self::Script => "script",
            Self::LanguageUntagged => "language_untagged",
            Self::TooFewLines => "too_few_lines",
            Self::AlignerError => "aligner_error",
            Self::NotSearched => "not_searched",
            Self::Inconclusive => "inconclusive",
        }
    }
}

impl Verdict {
    pub const LABELS: [&'static str; 5] =
        ["fits", "retimed", "wrong_video", "segmented", "unknown"];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Fits { .. } => "fits",
            Self::Retimed { .. } => "retimed",
            Self::WrongVideo { .. } => "wrong_video",
            Self::Segmented { .. } => "segmented",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// FR-019 classes for verdicts other than `unknown`.
    pub const REASON_CLASSES: [&'static str; 5] =
        ["fits", "offset", "ratio", "decoy_level", "segmented"];

    /// Metric `reason_class` (FR-019): why the verdict, in a bounded word.
    pub fn reason_class(&self) -> &'static str {
        match self {
            Self::Fits { .. } => "fits",
            Self::Retimed { ratio, .. } if (ratio - 1.0).abs() >= 1e-9 => "ratio",
            Self::Retimed { .. } => "offset",
            Self::WrongVideo { .. } => "decoy_level",
            Self::Segmented { .. } => "segmented",
            Self::Unknown { class, .. } => class.label(),
        }
    }

    pub fn unknown(class: UnknownClass, reason: impl std::fmt::Display) -> Self {
        Self::Unknown {
            class,
            reason: reason.to_string(),
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fits { fit } | Self::WrongVideo { fit } => write!(f, "{fit}"),
            Self::Retimed {
                fit,
                offset_ms,
                ratio,
            } => write!(f, "{fit} offset_ms={offset_ms} ratio={ratio:.6}"),
            Self::Segmented { offsets_ms } => {
                let list: Vec<String> = offsets_ms.iter().map(i64::to_string).collect();
                write!(f, "offsets_ms={}", list.join(","))
            }
            Self::Unknown { reason, .. } => f.write_str(reason),
        }
    }
}

fn is_identity(c: &Candidate) -> bool {
    c.offset_ms.abs() < NEGLIGIBLE_MS && (c.ratio - 1.0).abs() < 1e-9
}

/// FR-010 `segmented`: two candidates more than [`SEGMENT_GAP_MS`] apart, each
/// with a group of at least [`SEGMENT_MIN_PROBES`] probes that hit there and
/// not at the other.
fn segmented(cands: &[&Candidate], t: &Thresholds) -> Option<Vec<i64>> {
    for (i, a) in cands.iter().enumerate() {
        for b in cands.iter().skip(i + 1) {
            if (a.offset_ms - b.offset_ms).abs() <= SEGMENT_GAP_MS {
                continue;
            }
            let ha: Vec<usize> = a.hits(t).collect();
            let hb: Vec<usize> = b.hits(t).collect();
            let only_a = ha.iter().filter(|p| !hb.contains(p)).count();
            let only_b = hb.iter().filter(|p| !ha.contains(p)).count();
            if only_a >= SEGMENT_MIN_PROBES && only_b >= SEGMENT_MIN_PROBES {
                let mut offs = vec![a.offset_ms, b.offset_ms];
                offs.sort_unstable();
                return Some(offs);
            }
        }
    }
    None
}

/// FR-007 / FR-008 / FR-010 over the candidates tried for one track.
///
/// `stage2` is `None` when the wide search has not run (slice 2 of the plan):
/// then a track that does not fit is `unknown` (`not_searched`), never
/// `wrong_video`. Candidates in `stage2` are in the order tried; the first that
/// fits wins.
pub fn decide(stage1: &Candidate, stage2: Option<&[Candidate]>, t: &Thresholds) -> Verdict {
    let fit1 = stage1.fit(t);
    if fit1.fits(t) {
        return if is_identity(stage1) {
            Verdict::Fits { fit: fit1 }
        } else {
            Verdict::Retimed {
                fit: fit1,
                offset_ms: stage1.offset_ms,
                ratio: stage1.ratio,
            }
        };
    }
    let wide = stage2.unwrap_or(&[]);
    if let Some(c) = wide.iter().find(|c| c.fit(t).fits(t)) {
        return Verdict::Retimed {
            fit: c.fit(t),
            offset_ms: c.offset_ms,
            ratio: c.ratio,
        };
    }
    let all: Vec<&Candidate> = std::iter::once(stage1).chain(wide).collect();
    if let Some(offsets_ms) = segmented(&all, t) {
        return Verdict::Segmented { offsets_ms };
    }
    let Some(wide) = stage2 else {
        return Verdict::unknown(
            UnknownClass::NotSearched,
            format_args!("not_searched {fit1}"),
        );
    };
    // Only the wide search can call a track wrong: with no stage-2
    // candidate there is nothing to call it on.
    let Some(best) = wide
        .iter()
        .map(|c| c.fit(t))
        .max_by(|a, b| a.median.total_cmp(&b.median))
    else {
        return Verdict::unknown(
            UnknownClass::Inconclusive,
            format_args!("inconclusive no_candidates {fit1}"),
        );
    };
    if best.decoy_level(t) {
        Verdict::WrongVideo { fit: best }
    } else {
        Verdict::unknown(
            UnknownClass::Inconclusive,
            format_args!("inconclusive {best}"),
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    // Score vectors shaped like the 2026-11-12 spike's: right text around a
    // median of 0.90, decoy and wrong-video text around 0.25.
    const RIGHT: [f32; 12] = [
        0.93, 0.88, 0.91, 0.95, 0.62, 0.90, 0.87, 0.97, 0.41, 0.92, 0.89, 0.90,
    ];
    const DECOY: [f32; 12] = [
        0.22, 0.31, 0.18, 0.27, 0.25, 0.40, 0.12, 0.26, 0.24, 0.30, 0.19, 0.28,
    ];
    const WRONG: [f32; 12] = [
        0.30, 0.21, 0.27, 0.35, 0.19, 0.28, 0.24, 0.52, 0.26, 0.29, 0.22, 0.33,
    ];

    fn cand(offset_ms: i64, ratio: f64, probes: &[f32], decoys: &[f32]) -> Candidate {
        Candidate {
            offset_ms,
            ratio,
            probes: probes.to_vec(),
            decoys: decoys.to_vec(),
        }
    }

    fn t() -> Thresholds {
        Thresholds::default()
    }

    #[test]
    fn median_handles_even_odd_empty_and_nan() {
        assert_eq!(median(&[]), 0.0);
        assert!((median(&[0.1, 0.9, 0.5]) - 0.5).abs() < 1e-6);
        assert!((median(&[0.2, 0.4, 0.6, 0.8]) - 0.5).abs() < 1e-6);
        assert!((median(&[f32::NAN, 0.2, 0.4]) - 0.2).abs() < 1e-6);
    }

    #[test]
    fn right_text_at_its_own_time_fits() {
        let v = decide(&cand(120, 1.0, &RIGHT, &DECOY), None, &t());
        assert_eq!(v.label(), "fits");
        assert_eq!(v.to_string(), "median=0.90 decoy=0.25 hits=11/12");
    }

    #[test]
    fn aligner_offset_past_noise_is_retimed() {
        let v = decide(&cand(3_000, 1.0, &RIGHT, &DECOY), None, &t());
        assert_eq!(v.label(), "retimed");
        assert!(
            v.to_string().ends_with("offset_ms=3000 ratio=1.000000"),
            "{v}"
        );
    }

    #[test]
    fn half_the_probes_hitting_fits_even_with_a_low_median() {
        let probes = [
            0.55, 0.55, 0.55, 0.55, 0.55, 0.55, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1,
        ];
        let decoys = [0.05; 12];
        let fit = cand(0, 1.0, &probes, &decoys).fit(&t());
        assert!(fit.median < t().median);
        assert!(fit.fits(&t()));
    }

    #[test]
    fn high_scores_without_a_margin_over_decoys_do_not_fit() {
        // Text the aligner scores well whatever the audio: no evidence either way.
        let fit = cand(0, 1.0, &RIGHT, &RIGHT).fit(&t());
        assert!(!fit.fits(&t()));
    }

    #[test]
    fn without_the_wide_search_a_misfit_is_never_wrong_video() {
        let v = decide(&cand(0, 1.0, &WRONG, &DECOY), None, &t());
        assert_eq!(v.reason_class(), "not_searched");
        assert!(v.to_string().starts_with("not_searched median=0.28"), "{v}");
    }

    #[test]
    fn wide_search_fit_is_retimed_with_its_ratio() {
        let wide = [
            cand(20_000, 1.0, &WRONG, &DECOY),
            cand(-1_500, 25.0 / 23.976, &RIGHT, &DECOY),
        ];
        let v = decide(&cand(0, 1.0, &WRONG, &DECOY), Some(&wide), &t());
        assert_eq!(v.label(), "retimed");
        assert!(
            v.to_string().ends_with("offset_ms=-1500 ratio=1.042709"),
            "{v}"
        );
    }

    #[test]
    fn decoy_level_everywhere_is_wrong_video() {
        let wide = [
            cand(20_000, 1.0, &WRONG, &DECOY),
            cand(-40_000, 1.0, &DECOY, &DECOY),
        ];
        let v = decide(&cand(0, 1.0, &WRONG, &DECOY), Some(&wide), &t());
        assert_eq!(v.label(), "wrong_video");
        assert_eq!(v.to_string(), "median=0.28 decoy=0.25 hits=1/12");
    }

    #[test]
    fn a_wide_search_with_no_candidates_is_inconclusive_not_wrong() {
        let v = decide(&cand(0, 1.0, &WRONG, &DECOY), Some(&[]), &t());
        assert_eq!(v.reason_class(), "inconclusive");
        assert!(
            v.to_string().starts_with("inconclusive no_candidates"),
            "{v}"
        );
    }

    #[test]
    fn non_finite_scores_are_not_hits() {
        let probes = [f32::INFINITY, f32::INFINITY, 0.1, 0.1];
        assert_eq!(cand(0, 1.0, &probes, &[0.0; 4]).fit(&t()).hits, 0);
    }

    #[test]
    fn neither_fitting_nor_decoy_level_is_inconclusive() {
        let mid = [
            0.50, 0.40, 0.48, 0.45, 0.42, 0.47, 0.44, 0.41, 0.46, 0.43, 0.49, 0.30,
        ];
        let v = decide(&cand(0, 1.0, &mid, &[0.1; 12]), Some(&[]), &t());
        assert_eq!(v.reason_class(), "inconclusive");
    }

    #[test]
    fn two_groups_fitting_at_offsets_far_apart_are_segmented() {
        let first_half = [0.9, 0.9, 0.9, 0.9, 0.9, 0.9, 0.2, 0.2, 0.2, 0.2, 0.2, 0.2];
        let mut second_half = first_half;
        second_half.reverse();
        // Each alone has half hits, but a low median and a short margin.
        let decoys = [0.45; 12];
        let wide = [cand(30_000, 1.0, &second_half, &decoys)];
        let v = decide(&cand(0, 1.0, &first_half, &decoys), Some(&wide), &t());
        assert_eq!(v.label(), "segmented");
        assert_eq!(v.to_string(), "offsets_ms=0,30000");
    }

    #[test]
    fn labels_are_distinct_and_stable() {
        let mut labels: Vec<&str> = Verdict::LABELS.to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Verdict::LABELS.len());
        let mut classes: Vec<&str> = Verdict::REASON_CLASSES.to_vec();
        classes.extend(UnknownClass::ALL.iter().map(|c| c.label()));
        let n = classes.len();
        classes.sort_unstable();
        classes.dedup();
        assert_eq!(classes.len(), n);
    }

    #[test]
    fn reason_class_says_offset_or_ratio_for_a_retimed_track() {
        let off = decide(&cand(3_000, 1.0, &RIGHT, &DECOY), None, &t());
        assert_eq!(off.reason_class(), "offset");
        let wide = [cand(0, 25.0 / 23.976, &RIGHT, &DECOY)];
        let ratio = decide(&cand(0, 1.0, &WRONG, &DECOY), Some(&wide), &t());
        assert_eq!(ratio.reason_class(), "ratio");
        assert_eq!(
            decide(&cand(0, 1.0, &RIGHT, &DECOY), None, &t()).reason_class(),
            "fits"
        );
    }
}
