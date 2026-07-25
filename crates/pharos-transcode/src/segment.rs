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

/// The audio encode a segment's audio is COPIED from — one per
/// `(media, track, bitrate, codec)`, produced once for the whole title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuousAudio {
    pub codec: SegmentAudio,
    pub bitrate_bps: Option<u64>,
}

/// How a segment gets its audio.
///
/// There is deliberately **no variant meaning "encode audio for this
/// segment"**. Encoding audio per segment re-primes the codec at every
/// boundary — each segment emits its own priming frame and starts a fresh
/// frame grid at its own seek point — so consecutive segments carry
/// overlapping, phase-misaligned audio. That shipped on the muxed mpegts
/// surface while the browser surface had a continuous encode, because the
/// choice lived in a field any delivery path could set either way.
///
/// Now it cannot be set that way: the only shapes are "no audio here" and
/// "copied from the one encode".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioDelivery {
    /// This segment carries no audio; it is served as its own rendition
    /// (`EXT-X-MEDIA` group). The fMP4 surfaces.
    Separate,
    /// Muxed into this segment, copied from the title's continuous encode.
    Muxed(ContinuousAudio),
}

impl AudioDelivery {
    /// The codec of the audio this segment carries, if any. Used to key the
    /// cache: segments differing only in audio codec are different bytes.
    pub fn codec(self) -> Option<SegmentAudio> {
        match self {
            Self::Separate => None,
            Self::Muxed(c) => Some(c.codec),
        }
    }

    /// The bitrate of the continuous encode this segment copies from.
    pub fn bitrate_bps(self) -> Option<u64> {
        match self {
            Self::Separate => None,
            Self::Muxed(c) => c.bitrate_bps,
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
    /// How this segment gets its audio. See [`AudioDelivery`] — there is no
    /// value here meaning "encode audio for this segment".
    pub audio: AudioDelivery,
    pub video_bitrate_bps: Option<u64>,
    /// Where this segment sits on the shared timeline. Built only from a
    /// segment index and the source frame rate — see
    /// [`pharos_core::SegmentWindow`], whose fields are private precisely so
    /// a delivery path cannot state a position of its own.
    pub window: pharos_core::SegmentWindow,
    /// Source-relative audio-stream index (`-map 0:a:{N}`).
    pub audio_source_stream_index: Option<u32>,
    /// Subtitle-relative stream index for subtitle burn-in (image OR text).
    pub burn_subtitle_stream_index: Option<u32>,
    /// `true` when `burn_subtitle_stream_index` refers to a TEXT/ASS track
    /// (Task 7 picks the `subtitles=` filter instead of `overlay`). `false`
    /// for image-subtitle burn (PGS/VOBSUB/DVB) or when no burn is set.
    pub burn_subtitle_is_text: bool,
    /// Local `.ass` sidecar to burn a TEXT/ASS track from (see
    /// [`TranscodeOptions::burn_subtitle_ass_path`]). `Some` points libass at
    /// the small cached file instead of re-demuxing the whole source per
    /// segment; `None` falls back to the source-file form.
    pub burn_subtitle_ass_path: Option<std::path::PathBuf>,
    /// Directory of extracted embedded fonts for libass (`:fontsdir=`); see
    /// [`TranscodeOptions::burn_fonts_dir`].
    pub burn_fonts_dir: Option<std::path::PathBuf>,
    /// The RESOLVED continuous encode this segment copies its audio slice
    /// from. `None` until the cache has produced (or found) the encode that
    /// [`AudioDelivery::Muxed`] asks for — a handler declares the intent, the
    /// cache supplies the file.
    pub muxed_audio_source: Option<crate::options::MuxedAudio>,
}

impl SegmentOpts {
    /// The audio codec this segment carries, if any.
    pub fn audio_codec(&self) -> Option<SegmentAudio> {
        self.audio.codec()
    }

    /// The bitrate of the continuous encode this segment copies audio from.
    pub fn audio_bitrate_bps(&self) -> Option<u64> {
        self.audio.bitrate_bps()
    }

    /// Lower to the transcoder's wire options — the ONLY bridge from the
    /// segment-legal subset into the general option space.
    pub fn to_transcode_options(&self) -> TranscodeOptions {
        TranscodeOptions {
            container: self.container.into(),
            video: self.video.map(VideoCodec::from),
            // ALWAYS `None`: a segment never runs an audio encoder. When it
            // carries audio at all, the bytes are copied from the continuous
            // encode named by `muxed_audio_source`.
            audio: None,
            video_bitrate_bps: self.video_bitrate_bps,
            audio_bitrate_bps: self.audio_bitrate_bps(),
            start_position_ticks: self.window.start_ticks(),
            duration_ticks: Some(self.window.duration_ticks()),
            audio_source_stream_index: self.audio_source_stream_index,
            burn_subtitle_stream_index: self.burn_subtitle_stream_index,
            burn_subtitle_is_text: self.burn_subtitle_is_text,
            burn_subtitle_ass_path: self.burn_subtitle_ass_path.clone(),
            burn_fonts_dir: self.burn_fonts_dir.clone(),
            decode_preroll_seconds: None,
            muxed_audio_source: self.muxed_audio_source.clone(),
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
            audio: AudioDelivery::Muxed(ContinuousAudio {
                codec: SegmentAudio::Aac,

                bitrate_bps: Some(128_000),
            }),

            video_bitrate_bps: Some(3_000_000),
            window: pharos_core::SegmentWindow::for_segment(10, None),
            audio_source_stream_index: Some(1),
            burn_subtitle_stream_index: Some(0),
            burn_subtitle_is_text: true,
            burn_subtitle_ass_path: Some(std::path::PathBuf::from("/cache/sub.ass")),
            burn_fonts_dir: Some(std::path::PathBuf::from("/cache/fonts")),
            muxed_audio_source: None,
        };
        let t = s.to_transcode_options();
        assert_eq!(t.container, Container::Mpegts);
        assert_eq!(t.video, Some(VideoCodec::H264));
        // The old assertion here was `t.audio == Some(AudioCodec::Aac)` —
        // lowering used to hand the segment an audio ENCODER. That is exactly
        // the state `AudioDelivery` now makes unrepresentable, so the
        // assertion inverts: a lowered segment never names an audio encoder,
        // and the codec survives only as cache-keying information.
        assert_eq!(t.audio, None, "a segment never encodes its own audio");
        assert_eq!(s.audio_codec(), Some(SegmentAudio::Aac));
        assert_eq!(t.audio_bitrate_bps, Some(128_000));
        assert_eq!(t.video_bitrate_bps, Some(3_000_000));
        assert_eq!(t.start_position_ticks, s.window.start_ticks());
        assert_eq!(t.duration_ticks, Some(s.window.duration_ticks()));
        assert_eq!(t.audio_source_stream_index, Some(1));
        assert_eq!(t.burn_subtitle_stream_index, Some(0));
        assert!(t.burn_subtitle_is_text);
        assert_eq!(
            t.burn_subtitle_ass_path,
            Some(std::path::PathBuf::from("/cache/sub.ass"))
        );
        assert_eq!(
            t.burn_fonts_dir,
            Some(std::path::PathBuf::from("/cache/fonts"))
        );
    }

    #[test]
    fn no_audio_delivery_means_encode_this_segments_audio() {
        // THE property this type exists for. Encoding audio per segment
        // re-primes the codec at every boundary, so consecutive segments
        // carry overlapping, phase-misaligned audio — measured live as 6.037s
        // of audio against 6.006s of video, grids 0.53 of a frame apart. It
        // shipped because the choice was a field any delivery path could set
        // either way.
        //
        // The match below is EXHAUSTIVE: adding a variant that means "encode
        // audio here" fails to compile until someone deletes this test, which
        // is the point.
        for d in [
            AudioDelivery::Separate,
            AudioDelivery::Muxed(ContinuousAudio {
                codec: SegmentAudio::Aac,
                bitrate_bps: Some(128_000),
            }),
        ] {
            match d {
                // Carries no audio at all.
                AudioDelivery::Separate => assert_eq!(d.codec(), None),
                // Carries audio COPIED from one encode — never its own.
                AudioDelivery::Muxed(c) => assert_eq!(d.codec(), Some(c.codec)),
            }
        }
    }

    #[test]
    fn lowering_can_never_name_an_audio_encoder() {
        // Whatever a caller declares, the wire options reaching ffmpeg carry
        // no audio codec: the bytes come from the continuous encode.
        for audio in [
            AudioDelivery::Separate,
            AudioDelivery::Muxed(ContinuousAudio {
                codec: SegmentAudio::Aac,
                bitrate_bps: Some(128_000),
            }),
            AudioDelivery::Muxed(ContinuousAudio {
                codec: SegmentAudio::Opus,
                bitrate_bps: None,
            }),
        ] {
            let s = SegmentOpts {
                container: SegmentContainer::Mpegts,
                video: Some(SegmentVideo::H264),
                audio,
                video_bitrate_bps: Some(2_000_000),
                window: pharos_core::SegmentWindow::for_segment(0, None),
                audio_source_stream_index: None,
                burn_subtitle_stream_index: None,
                burn_subtitle_is_text: false,
                burn_subtitle_ass_path: None,
                burn_fonts_dir: None,
                muxed_audio_source: None,
            };
            assert_eq!(s.to_transcode_options().audio, None, "{audio:?}");
        }
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
