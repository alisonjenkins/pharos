//! Consolidated integration-test binary: one link instead of 19; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../common/store_calls.rs"]
mod store_calls;

mod browser_codec_matrix;
mod jellyfin_hls_softsubs;
mod jellyfin_playbackinfo_client_switches;
mod jellyfin_playbackinfo_kotlin_strict;
mod jellyfin_playbackinfo_native_directplay;
mod jellyfin_playbackinfo_stream_selection;
mod jellyfin_playbackinfo_subtitle_burn;
mod jellyfin_playbackinfo_subtitle_downgrade;
mod jellyfin_playbackinfo_subtitle_forward;
mod jellyfin_resume_position;
mod jellyfin_start_time_ticks;
mod jellyfin_stream;
mod jellyfin_subtitle_defaults;
mod jellyfin_track_selection;
mod jellyfin_video_remux;
mod media_source_selection;
mod playback_stopped_audit;
mod playing_progress_coalescing;
mod playing_progress_db_calls;
mod session_start_position;
