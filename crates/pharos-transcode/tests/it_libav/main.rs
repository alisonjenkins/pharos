//! Consolidated integration-test binary: one link instead of 5; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../common/dvd_subtitle.rs"]
mod dvd_subtitle;

mod libav_frames;
mod libav_integrity;
mod libav_pool;
mod libav_probe;
mod worker_ipc;
