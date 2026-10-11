//! Sort rank for free-text official ratings (`SortBy=ParentalRating`).
//!
//! `official_rating` is whatever the metadata source wrote, so it cannot be
//! ordered as text. The scores below follow Jellyfin's bundled rating tables
//! (`Emby.Server.Implementations/Localization/Ratings/{us,gb,au,de}.json`):
//! a `(score, subScore)` pair per rating string, compared lexicographically.
//! A string two regions both define takes the first region listed (US, then
//! GB, AU, DE) so every string has exactly one rank.
//!
//! This table only orders results. It is deliberately independent of the
//! config-overridable gating table in `pharos-server`'s `parental.rs`
//! (`MaxParentalRating`), whose scores are compared against a user's numeric
//! ceiling and must not shift. The two agree on relative order (`G` <= `PG` <
//! `PG-13` < `R` <= `TV-MA` < `NC-17`); this one is finer-grained and wider.

use std::sync::LazyLock;

/// `(lowercased rating, score, subScore)` — one row per distinct string.
const BASE: &[(&str, i32, i32)] = &[
    // US
    ("approved", 0, 0),
    ("g", 0, 0),
    ("tv-g", 0, 0),
    ("tv-y", 0, 0),
    ("tv-y7", 7, 0),
    ("tv-y7-fv", 7, 1),
    ("pg", 10, 0),
    ("tv-pg", 10, 0),
    ("pg-13", 13, 0),
    ("tv-14", 14, 0),
    ("r", 17, 0),
    // Jellyfin scores NC-17 with TV-MA; the gating table puts it above, so
    // sort it above too (sub-score 2) rather than disagree on order.
    ("nc-17", 17, 2),
    ("ec", 0, 0),
    ("tv-ma", 17, 1),
    ("tv-x", 18, 0),
    ("tv-ao", 18, 0),
    // GB
    ("0+", 0, 0),
    ("all", 0, 0),
    ("e", 0, 0),
    ("u", 0, 0),
    ("6+", 6, 0),
    ("7+", 7, 0),
    ("9", 9, 0),
    ("12a", 12, 0),
    ("12pg", 12, 0),
    ("12", 12, 1),
    ("12+", 12, 1),
    ("13+", 13, 0),
    ("teen", 13, 0),
    ("14+", 14, 0),
    ("15", 15, 3),
    ("16", 16, 0),
    ("18", 18, 1),
    ("caution", 18, 1),
    ("mature", 1000, 0),
    ("adult", 1000, 0),
    ("r18", 1000, 0),
    // AU
    ("exempt", 0, 0),
    ("m", 15, 2),
    ("ma", 15, 3),
    ("ma15+", 15, 3),
    ("16+", 16, 0),
    ("18+", 18, 1),
    ("r18+", 18, 1),
    ("x", 1000, 0),
    ("x18", 1000, 0),
    ("x18+", 1000, 0),
    ("rc", 1001, 0),
    // DE
    ("0", 0, 0),
    ("fsk0", 0, 0),
    ("fsk-0", 0, 0),
    ("ab0", 0, 0),
    ("educational", 0, 0),
    ("infoprogramm", 0, 0),
    ("6", 6, 0),
    ("fsk6", 6, 0),
    ("fsk-6", 6, 0),
    ("ab6", 6, 0),
    ("fsk12", 12, 0),
    ("fsk-12", 12, 0),
    ("ab12", 12, 0),
    ("fsk16", 16, 0),
    ("fsk-16", 16, 0),
    ("ab16", 16, 0),
    ("fsk18", 18, 0),
    ("fsk-18", 18, 0),
    ("ab18", 18, 0),
];

const ALL_DESCRIPTORS: &[&str] = &[
    "d", "l", "s", "v", "dl", "ds", "dv", "ls", "lv", "sv", "dls", "dlv", "dsv", "lsv", "dlsv",
];
const MA_DESCRIPTORS: &[&str] = &["l", "s", "v", "ls", "lv", "sv", "lsv"];

/// US content-descriptor variants (`TV-14-DLS`, `TV-PG-V`): `(base, score,
/// subScore, descriptor suffixes the base may carry)`.
const DESCRIPTOR_BASES: &[(&str, i32, i32, &[&str])] = &[
    ("tv-pg", 10, 1, ALL_DESCRIPTORS),
    ("tv-14", 14, 1, ALL_DESCRIPTORS),
    ("tv-ma", 17, 1, MA_DESCRIPTORS),
];

/// Multiplier that keeps `subScore` (at most 3 in the tables) from spilling
/// into the next score.
const SUB_SCORE_SPAN: i32 = 10;

/// Rank 0 is reserved for unknown / empty ratings, so the lowest known rating
/// (`G`, score 0) still ranks above them.
const fn rank(score: i32, sub: i32) -> i32 {
    score * SUB_SCORE_SPAN + sub + 1
}

static RANKS: LazyLock<Vec<(String, i32)>> = LazyLock::new(|| {
    let mut out: Vec<(String, i32)> = BASE
        .iter()
        .map(|(name, score, sub)| ((*name).to_string(), rank(*score, *sub)))
        .collect();
    for (base, score, sub, suffixes) in DESCRIPTOR_BASES {
        for suffix in *suffixes {
            out.push((format!("{base}-{suffix}"), rank(*score, *sub)));
        }
    }
    out
});

/// Every known `(lowercased rating, rank)` pair; ranks are `>= 1`.
pub fn parental_rating_ranks() -> &'static [(String, i32)] {
    RANKS.as_slice()
}

/// Rank of an official rating: `0` for an empty or unrecognised one, otherwise
/// a value that grows with maturity. Case and surrounding spaces ignored —
/// exactly what the SQL side's `LOWER(TRIM(x))` does (both strip only U+0020
/// and fold ASCII), so a tab or newline leaves the rating unknown on both.
pub fn parental_rating_rank(rating: &str) -> i32 {
    let key = rating.trim_matches(' ').to_ascii_lowercase();
    RANKS
        .iter()
        .find(|(name, _)| *name == key)
        .map_or(0, |(_, r)| *r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systems_order_by_maturity() {
        let r = parental_rating_rank;
        assert!(r("") < r("G"));
        assert!(r("G") < r("PG") && r("PG") < r("PG-13") && r("PG-13") < r("R"));
        assert!(r("R") < r("TV-MA") && r("TV-MA") < r("NC-17"));
        assert!(r("NC-17") < r("TV-X"));
        assert_eq!(r("EC"), r("G"));
        assert!(r("TV-Y") < r("TV-Y7") && r("TV-Y7") < r("TV-PG"));
        assert!(r("TV-PG") < r("TV-14") && r("TV-14") < r("TV-MA"));
        assert!(r("U") < r("12A") && r("12A") < r("15") && r("15") < r("18"));
        assert!(r("FSK12") < r("FSK16") && r("FSK16") < r("FSK18"));
        assert!(r("TV-14") < r("TV-14-DLSV") && r("TV-14-DLSV") < r("R"));
    }

    #[test]
    fn unknown_and_blank_rank_lowest() {
        assert_eq!(parental_rating_rank("Not Rated"), 0);
        assert_eq!(parental_rating_rank("   "), 0);
        // SQL TRIM strips spaces only; the Rust side must agree.
        assert_eq!(parental_rating_rank("PG-13\n"), 0);
        assert_eq!(parental_rating_rank("\tR"), 0);
        assert_eq!(
            parental_rating_rank(" pg-13 "),
            parental_rating_rank("PG-13")
        );
    }

    #[test]
    fn every_string_has_one_rank() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in parental_rating_ranks() {
            assert!(seen.insert(name.clone()), "duplicate rating {name}");
        }
    }
}
