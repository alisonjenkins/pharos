//! Consolidated integration-test binary: one link instead of 9; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod attachment_fonts;
mod audio_init_edit_list;
mod jellyfin_hls_multivariant;
mod jellyfin_hls_session_lifecycle;
mod jellyfin_trickplay;
mod subtitle_delivery;
mod vp9_audio_rendition;
mod vp9_audio_switch;
mod vp9_fmp4_hls;
