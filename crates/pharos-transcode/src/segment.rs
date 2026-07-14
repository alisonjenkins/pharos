//! Per-segment HLS transcode spec — the type-level half of V30/B45.
//!
//! Segmented HLS (independent per-segment transcodes tiling one shared
//! timeline) has invariants the general [`TranscodeOptions`] cannot express:
//!
//! - video is NEVER stream-copied (`-output_ts_offset` is inert under
//!   `-c:v copy`, copy cuts on source keyframes so durations drift off the
//!   EXTINF grid — B45);
//! - audio is NEVER stream-copied (multichannel AAC passthrough is
//!   undecodable in Firefox's MSE);
//! - the container is a segment container (mpegts / fMP4), never a
//!   progressive one (mp4/webm/mkv).
//!
//! [`SegmentOpts`] makes those states unrepresentable: [`SegmentVideo`] and
//! [`SegmentAudio`] have no `Copy` variant, [`SegmentContainer`] has no
//! progressive variant. The segment cache accepts ONLY this type, so a
//! copy-shaped segment can no longer be minted by any code path — the
//! compiler enforces what B45 previously guarded by comment. Conversion to
//! the transcoder's wire options happens in one place
//! ([`SegmentOpts::to_transcode_options`]).
//!
//! Copy remux remains legal on the progressive `/stream` path, which keeps
//! using [`TranscodeOptions`] directly (one continuous output, no
//! per-segment cuts).

use crate::options::{AudioCodec, Container, TranscodeOptions, VideoCodec};
use serde::{Deserialize, Serialize};

/// Containers a per-segment HLS transcode may target. No progressive
/// containers here by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentContainer {
    /// `.ts` — the h264 HLS surface.
    Mpegts,
    /// `.m4s`/`init.mp4` — the VP9 fMP4 HLS surface.
    Fmp4,
}

impl SegmentContainer {
    pub fn content_type(self) -> &'static str {
        Container::from(self).content_type()
    }
}

impl From<SegmentContainer> for Container {
    fn from(c: SegmentContainer) -> Self {
        match c {
            SegmentContainer::Mpegts => Container::Mpegts,
            SegmentContainer::Fmp4 => Container::Fmp4,
        }
    }
}

/// Video codecs a segment may carry. NO `Copy` variant — stream-copied
/// segments are structurally broken (B45/V30).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentVideo {
    H264,
    Vp9,
}

impl SegmentVideo {
    pub fn ffmpeg_codec(self) -> &'static str {
        VideoCodec::from(self).ffmpeg_codec()
    }
}

impl From<SegmentVideo> for VideoCodec {
    fn from(v: SegmentVideo) -> Self {
        match v {
            SegmentVideo::H264 => VideoCodec::H264,
            SegmentVideo::Vp9 => VideoCodec::Vp9,
        }
    }
}

/// Audio codecs a segment may carry. NO `Copy` variant — passthrough
/// multichannel audio kills Firefox MSE (B45); both encoders downmix to
/// stereo in the arg builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentAudio {
    Aac,
    Opus,
}

impl From<SegmentAudio> for AudioCodec {
    fn from(a: SegmentAudio) -> Self {
        match a {
            SegmentAudio::Aac => AudioCodec::Aac,
            SegmentAudio::Opus => AudioCodec::Opus,
        }
    }
}

/// Options for ONE independent per-segment HLS transcode. Same field names
/// as [`TranscodeOptions`] (call sites read identically), but the codec /
/// container types exclude every segment-illegal state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentOpts {
    pub container: SegmentContainer,
    /// `None` = audio-only rendition segment (`-vn`).
    pub video: Option<SegmentVideo>,
    /// `None` = audio-free video segment (`-an`; the VP9 surface serves
    /// audio as a separate continuous rendition).
    pub audio: Option<SegmentAudio>,
    pub video_bitrate_bps: Option<u64>,
    pub audio_bitrate_bps: Option<u64>,
    /// Jellyfin-style ticks (10,000,000 per second). 0 = start of stream.
    pub start_position_ticks: u64,
    pub duration_ticks: Option<u64>,
    /// Source-relative audio-stream index (`-map 0:a:{N}`).
    pub audio_source_stream_index: Option<u32>,
    /// Subtitle-relative stream index for IMAGE-subtitle burn-in.
    pub burn_subtitle_stream_index: Option<u32>,
}

impl SegmentOpts {
    /// Lower to the transcoder's wire options — the ONLY bridge from the
    /// segment-legal subset into the general option space.
    pub fn to_transcode_options(&self) -> TranscodeOptions {
        TranscodeOptions {
            container: self.container.into(),
            video: self.video.map(VideoCodec::from),
            audio: self.audio.map(AudioCodec::from),
            video_bitrate_bps: self.video_bitrate_bps,
            audio_bitrate_bps: self.audio_bitrate_bps,
            start_position_ticks: self.start_position_ticks,
            duration_ticks: self.duration_ticks,
            audio_source_stream_index: self.audio_source_stream_index,
            burn_subtitle_stream_index: self.burn_subtitle_stream_index,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_preserves_every_field() {
        let s = SegmentOpts {
            container: SegmentContainer::Mpegts,
            video: Some(SegmentVideo::H264),
            audio: Some(SegmentAudio::Aac),
            video_bitrate_bps: Some(3_000_000),
            audio_bitrate_bps: Some(128_000),
            start_position_ticks: 60_060_000,
            duration_ticks: Some(60_060_000),
            audio_source_stream_index: Some(1),
            burn_subtitle_stream_index: Some(0),
        };
        let t = s.to_transcode_options();
        assert_eq!(t.container, Container::Mpegts);
        assert_eq!(t.video, Some(VideoCodec::H264));
        assert_eq!(t.audio, Some(AudioCodec::Aac));
        assert_eq!(t.video_bitrate_bps, Some(3_000_000));
        assert_eq!(t.start_position_ticks, 60_060_000);
        assert_eq!(t.duration_ticks, Some(60_060_000));
        assert_eq!(t.audio_source_stream_index, Some(1));
        assert_eq!(t.burn_subtitle_stream_index, Some(0));
    }

    #[test]
    fn segment_types_have_no_copy_or_progressive_variants() {
        // Compile-time property spelled out for the reader: the match arms
        // below are EXHAUSTIVE. Adding a Copy/progressive variant to any of
        // these enums fails this match (and the segment surface's V30
        // invariant) at compile time, forcing the author to confront it.
        for v in [SegmentVideo::H264, SegmentVideo::Vp9] {
            match v {
                SegmentVideo::H264 | SegmentVideo::Vp9 => {}
            }
        }
        for a in [SegmentAudio::Aac, SegmentAudio::Opus] {
            match a {
                SegmentAudio::Aac | SegmentAudio::Opus => {}
            }
        }
        for c in [SegmentContainer::Mpegts, SegmentContainer::Fmp4] {
            match c {
                SegmentContainer::Mpegts | SegmentContainer::Fmp4 => {}
            }
        }
    }
}
