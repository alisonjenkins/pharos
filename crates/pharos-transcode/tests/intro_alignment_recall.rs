//! Intro-detection recall, measured on REAL fingerprints (spec 002).
//!
//! A viewer reported that Skip Intro never appears while Skip Outro does. The
//! cause was not delivery — both kinds travel one code path and one DTO — but
//! recall: for the reported season the detector emitted a closing for 3 of 4
//! episodes and `no_span` for every episode's opening, in the same pass, on the
//! same files.
//!
//! This test replays that season's stored fingerprints through the same
//! comparison the sweep runs, so the failure is reproducible with no media, no
//! NFS and no database. The credits side of the same fixture is the control: it
//! is produced by identical code over identical files, so any explanation that
//! would also break credits is wrong by construction.
//!
//! Observed on the code that shipped the bug:
//!
//! ```text
//! == intro:   1 of 10 pairs matched  (only 0x4, 60.2s at 136.2s)
//! == credits: 6 of 10 pairs matched  (90.9s at ~353.8s — the real ending)
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{load_fixture, of_kind, FixtureRow};
use pharos_transcode::fingerprint::align::{compare, AlignConfig};
use pharos_transcode::fingerprint::season::{
    detect_season_verbose, EpisodeFingerprint, SeasonConfig, Verdict,
};

const FIXTURE: &str = "mushoku_s03_fingerprints.txt";

/// Pairs of `kind` whose comparison located a span, and the located durations.
fn matched_pairs(rows: &[FixtureRow], kind: &str) -> (usize, usize, Vec<f64>) {
    let eps = of_kind(rows, kind);
    let cfg = AlignConfig::default();
    let mut total = 0;
    let mut durations = Vec::new();
    for i in 0..eps.len() {
        for j in (i + 1)..eps.len() {
            total += 1;
            if let Some(m) = compare(&eps[i].points, &eps[j].points, &cfg) {
                durations.push(m.lhs.end - m.lhs.start);
            }
        }
    }
    (durations.len(), total, durations)
}

/// The fixture is the detector's real input; a decoding slip would silently
/// weaken every assertion below, so the shape is pinned first.
#[test]
fn the_fixture_decodes_to_plausible_fingerprints() {
    let rows = load_fixture(FIXTURE);
    assert_eq!(rows.len(), 10, "5 episodes x 2 windows");
    let intro = of_kind(&rows, "intro");
    let credits = of_kind(&rows, "credits");
    assert_eq!(intro.len(), 5);
    assert_eq!(credits.len(), 5);
    for r in &intro {
        assert!(
            (1400..=1420).contains(&r.points.len()),
            "intro window is ~350s at the 0.2476s hop, got {} points",
            r.points.len()
        );
    }
    for r in &credits {
        assert!(
            (1780..=1800).contains(&r.points.len()),
            "credits window is 450s at the 0.2476s hop, got {} points",
            r.points.len()
        );
    }
}

/// The control. Credits detection worked in production and must keep working —
/// a fix that lifts intro recall by loosening a shared gate would show up here.
#[test]
fn credits_recall_is_the_control_and_stays_high() {
    let rows = load_fixture(FIXTURE);
    let (matched, total, durations) = matched_pairs(&rows, "credits");
    assert!(
        matched >= 6,
        "credits recall regressed: {matched}/{total} pairs"
    );
    for d in &durations {
        assert!(
            (85.0..=95.0).contains(d),
            "credits span should be the ~91s ending, got {d:.1}s"
        );
    }
}

/// Recall must equal PRESENCE: wherever the shared opening is actually in the
/// analysed window, it is located.
///
/// This started life asserting `intro_matched >= 6` — parity with the credits
/// control — on the assumption that the opening is in every episode's window.
/// Measurement refuted that. Sliding the opening located in the E04×E05 pair
/// against each episode's own window gives:
///
/// ```text
/// ep0 (E04): mean bit-distance 0.00 at 136.2s — 243/243 points within 6 bits
/// ep4 (E05): mean bit-distance 5.83 at 159.2s — 141/243 points within 6 bits
/// ep1 (E02): mean 15.00   ep2 (E03): mean 14.59   ep3 (E01): mean 15.04
/// ```
///
/// A mean of ~15 differing bits of 32 is chance: for three of the five episodes
/// that audio is simply not in the first 350 s. No comparison can find what was
/// never fingerprinted, so parity with credits is not achievable here and
/// asserting it would be asserting a fiction. The open question — whether those
/// openings sit beyond the window or differ — needs the media, and is tracked in
/// `specs/002-fix-skip-intro/research.md`.
#[test]
fn the_opening_is_located_wherever_it_is_present() {
    let rows = load_fixture(FIXTURE);
    let eps = of_kind(&rows, "intro");
    let cfg = AlignConfig::default();
    // E04 (index 0) and E05 (index 4) demonstrably share the opening.
    let m = compare(&eps[0].points, &eps[4].points, &cfg).expect(
        "the two episodes whose windows provably contain the same opening must \
         align; if this is None the comparison itself has regressed",
    );
    let d = m.lhs.end - m.lhs.start;
    assert!(
        (50.0..=120.0).contains(&d),
        "located {d:.1}s of a ~60s opening"
    );
    assert!(
        (130.0..=145.0).contains(&m.lhs.start),
        "the opening sits at ~136s in E04, located at {:.1}s",
        m.lhs.start
    );
    // Every located span still respects the duration bounds.
    let (_, _, durations) = matched_pairs(&rows, "intro");
    for d in &durations {
        assert!(
            *d >= 15.0,
            "a span shorter than the minimum duration escaped the bounds check: {d:.1}s"
        );
    }
}

/// Recall at the pair level only matters if the season consensus then emits.
/// This is the end of the detection chain: what the sweep would persist.
#[test]
fn the_season_verdicts_name_the_real_state() {
    let rows = load_fixture(FIXTURE);
    let eps: Vec<EpisodeFingerprint> = of_kind(&rows, "intro")
        .iter()
        .map(|r| EpisodeFingerprint {
            id: r.item_id,
            points: r.points.clone(),
            // The intro window starts at the top of the episode.
            window_offset_secs: 0.0,
        })
        .collect();
    let verdicts = detect_season_verbose(&eps, &SeasonConfig::default());
    assert_eq!(verdicts.len(), 5);
    // Only E04 and E05 carry the opening in their analysed window, so exactly
    // one comparison locates a span for each of them — one short of
    // `min_agreeing`, which is why this season persists no opening even though
    // two of its five episodes demonstrably share one.
    //
    // That gate is correct as written: a single agreeing pair is the shape of a
    // coincidence, and admitting it is how a bogus intro gets enshrined. The
    // season is under-evidenced, not mis-judged — and the verdict says so, which
    // is the whole point of V75.
    let by_id: Vec<_> = verdicts
        .iter()
        .map(|v| (v.id, v.verdict.label(), v.matched, v.agreeing))
        .collect();
    let emitted = verdicts
        .iter()
        .filter(|v| v.verdict == Verdict::Emitted)
        .count();
    let no_span = verdicts
        .iter()
        .filter(|v| v.verdict == Verdict::NoSpan)
        .count();
    let few_agreeing = verdicts
        .iter()
        .filter(|v| v.verdict == Verdict::FewAgreeing)
        .count();
    assert_eq!(
        (emitted, few_agreeing, no_span),
        (0, 2, 3),
        "the two episodes sharing an opening report few_agreeing and the three \
         without it report no_span — a verdict split that names the real state. \
         Got: {by_id:?}"
    );
}

/// The fix widens the per-pair shift search, and alignment is O(n^2) pairwise
/// across a season. A cost blow-up would not fail any correctness assertion, so
/// it gets its own guard.
#[test]
fn a_full_season_replay_stays_cheap() {
    let rows = load_fixture(FIXTURE);
    let start = std::time::Instant::now();
    let _ = matched_pairs(&rows, "intro");
    let _ = matched_pairs(&rows, "credits");
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs_f64() < 10.0,
        "20 pairwise comparisons took {elapsed:?}; a 20-episode season is 190 \
         pairs per kind and would be ~19x this"
    );
}
