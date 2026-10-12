//! Consolidated integration-test binary: one link instead of 4; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../common/mod.rs"]
mod common;

mod fingerprint_detect;
mod intro_alignment_recall;
mod subtitle_burn_ass;
mod subtitle_windows_scan;
