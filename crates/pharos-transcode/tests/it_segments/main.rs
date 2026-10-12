//! Consolidated integration-test binary: one link instead of 6; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod filler_segment;
mod mpegts_segment_timeline;
mod muxed_audio_content;
mod segment_audio_continuity;
mod segment_frame_completeness;
mod segment_track_shape;
