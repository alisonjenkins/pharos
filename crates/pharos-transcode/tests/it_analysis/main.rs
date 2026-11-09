//! Consolidated integration-test binary: one link instead of 4; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../common/dvd_subtitle.rs"]
mod dvd_subtitle;

#[path = "../common/mod.rs"]
mod common;

mod fingerprint_detect;
mod intro_alignment_recall;
mod picture_area_scan;
mod subtitle_burn_ass;
mod subtitle_burn_per_eye;
mod subtitle_images_extract;
mod subtitle_windows_scan;
