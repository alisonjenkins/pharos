//! T106 (B1060, V1020) — the overlap consensus replayed on REAL fingerprints.
//!
//! Series A is the season the bug was measured on: its opening is ~109 s, and
//! four of its nine fingerprinted episodes emitted before T106. Series B (a
//! ~3 s title card) and series C (no theme, a shared ending) are controls that
//! must stay silent on the intro side. Dumped read-only from
//! `episode_fingerprints` on 2026-11-10 with invented item ids; see
//! `tests/fixtures/README.md`.
//!
//! Measured on this replay (item: reading, span in seconds):
//!
//! ```text
//! 1003 1004 1005 1006  endpoints  0..108-109   (unchanged; matches production)
//! 1002                 overlap    9..103
//! 1007                 overlap    9..95
//! 1008                 overlap    0..63
//! 1001                 overlap    0..46
//! 1009                 low_confidence (2 of 5 agree)
//! series B             no_span on all 10, both windows
//! series C             no intro emitted; all 24 endings by endpoints
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::{load_fixture, of_kind};
use pharos_transcode::fingerprint::season::{
    detect_season_verbose, Consensus, EpisodeFingerprint, EpisodeVerdict, SeasonConfig, Verdict,
};

fn verdicts(fixture: &str, kind: &str) -> Vec<EpisodeVerdict> {
    let rows = load_fixture(fixture);
    let eps: Vec<EpisodeFingerprint> = of_kind(&rows, kind)
        .iter()
        .map(|r| EpisodeFingerprint {
            id: r.item_id,
            points: r.points.clone(),
            window_offset_secs: 0.0,
        })
        .collect();
    detect_season_verbose(&eps, &SeasonConfig::default())
}

/// The episodes that emitted before T106.
const SERIES_A_ENDPOINTS: [u64; 4] = [1003, 1004, 1005, 1006];
/// The episodes T106 adds, all by overlap.
const SERIES_A_OVERLAP: [u64; 4] = [1001, 1002, 1007, 1008];

/// The episodes production already emits keep exactly the endpoint reading,
/// and the fragmented ones now emit too, each starting where the season's
/// opening starts and never running past its end.
#[test]
fn series_a_gains_its_fragmented_openings() {
    let season = verdicts("series_a_fingerprints.txt", "intro");
    for id in SERIES_A_ENDPOINTS {
        let v = season.iter().find(|v| v.id == id).expect("a verdict");
        assert_eq!(v.consensus, Some(Consensus::Endpoints), "{v:?}");
        let s = v.span.expect("a span");
        assert!(s.start == 0.0 && (107.0..=110.0).contains(&s.end), "{v:?}");
    }
    let mut by_overlap: Vec<u64> = season
        .iter()
        .filter(|v| v.consensus == Some(Consensus::Overlap))
        .map(|v| v.id)
        .collect();
    by_overlap.sort_unstable();
    assert_eq!(by_overlap, SERIES_A_OVERLAP, "{season:?}");
    for v in season
        .iter()
        .filter(|v| v.consensus == Some(Consensus::Overlap))
    {
        let s = v.span.expect("a span");
        assert!(
            s.start <= 10.0 && s.end <= 110.0,
            "an overlap span must sit inside the season's ~0..109 s opening: {v:?}"
        );
    }
}

#[test]
fn series_b_stays_silent() {
    for kind in ["intro", "credits"] {
        let v = verdicts("series_b_fingerprints.txt", kind);
        assert_eq!(v.len(), 10);
        assert!(
            v.iter().all(|v| v.verdict == Verdict::NoSpan),
            "{kind}: {v:?}"
        );
    }
}

#[test]
fn series_c_has_no_intro_and_keeps_every_ending() {
    let intro = verdicts("series_c_fingerprints.txt", "intro");
    assert!(
        intro.iter().all(|v| v.verdict != Verdict::Emitted),
        "series C has no shared theme: {intro:?}"
    );
    let credits = verdicts("series_c_fingerprints.txt", "credits");
    assert_eq!(credits.len(), 24);
    assert!(
        credits
            .iter()
            .all(|v| v.consensus == Some(Consensus::Endpoints)),
        "{credits:?}"
    );
}
