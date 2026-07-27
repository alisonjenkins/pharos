//! Shared helpers for the fingerprint integration tests.
//!
//! Lives under `tests/common/` rather than `tests/<name>.rs` because cargo
//! compiles every top-level file in `tests/` as its own test binary — a helper
//! module there would build as a target with no tests in it.
#![allow(dead_code)]

use std::path::Path;

/// One stored fingerprint row: `(item_id, kind, points)`.
///
/// `kind` is the *fingerprint* vocabulary (`intro` / `credits`), which is not the
/// `media_segments` vocabulary (`Intro` / `Outro`).
pub struct FixtureRow {
    pub item_id: u64,
    pub kind: String,
    pub points: Vec<u32>,
}

/// Parse a fingerprint fixture: one `\<item_id\> \<kind\> \<hex\>` row per line.
///
/// `hex` is `episode_fingerprints.points` verbatim — little-endian `u32` per
/// fingerprint point — so each 8-hex-char group is byte-swapped back on read.
/// See `tests/fixtures/README.md` for provenance.
pub fn parse_fixture(raw: &str) -> Vec<FixtureRow> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let mut parts = line.split(' ');
            let item_id = parts.next()?.parse().ok()?;
            let kind = parts.next()?.to_string();
            let hex = parts.next()?;
            let points = decode_points(hex)?;
            Some(FixtureRow {
                item_id,
                kind,
                points,
            })
        })
        .collect()
}

/// Decode a hex-encoded little-endian `u32` sequence. `None` when the input is
/// not a whole number of 4-byte points or contains a non-hex digit.
fn decode_points(hex: &str) -> Option<Vec<u32>> {
    if hex.len() % 8 != 0 {
        return None;
    }
    hex.as_bytes()
        .chunks(8)
        .map(|chunk| {
            let s = std::str::from_utf8(chunk).ok()?;
            u32::from_str_radix(s, 16).ok().map(u32::swap_bytes)
        })
        .collect()
}

/// Load a fixture from `tests/fixtures/<name>`.
pub fn load_fixture(name: &str) -> Vec<FixtureRow> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    match std::fs::read_to_string(&path) {
        Ok(raw) => parse_fixture(&raw),
        Err(e) => panic!("fixture {} unreadable: {e}", path.display()),
    }
}

/// The rows of one `kind`, in file order.
pub fn of_kind<'a>(rows: &'a [FixtureRow], kind: &str) -> Vec<&'a FixtureRow> {
    rows.iter().filter(|r| r.kind == kind).collect()
}
