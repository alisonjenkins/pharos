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
    /// What the SOURCE file contains, when it could be named — see
    /// [`TranscodeOptions::source_video_codec`]. Lowered unchanged; the
    /// scheduler reads it to decide whether the device it places this segment
    /// on can decode the source at all.
    pub source_video_codec: Option<crate::options::SourceCodec>,
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
    ///
    /// This is the SEGMENT's value and may be cleared for an individual
    /// segment by the burn gate when no subtitle event falls in its window.
    /// Anything that must hold across a whole rendition — device placement
    /// above all — has to read [`Self::burn_intent`] instead (B200/V153).
    pub burn_subtitle_stream_index: Option<u32>,
    /// Whether this RENDITION burns subtitles at all, independent of whether
    /// this particular segment happens to contain one.
    ///
    /// Exists because the two differ, and the difference was undecodable:
    /// `maybe_gate_burn` clears the index for a quiet stretch, and placement
    /// keyed on that index sent the quiet segments to a different encoder from
    /// their neighbours — two H.264 profiles under one `avcC` (B200, #114).
    /// The gate is still worth having; it just may not move the encoder.
    pub burn_intent: bool,
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
}

/// A segment's audio once it is SETTLED.
///
/// [`Muxed`](Self::Muxed) cannot be constructed without the slice it copies
/// from, so "a muxed segment with no audio source" is not a state this program
/// can hold. It used to be: `SegmentOpts` carried an
/// `Option<MuxedAudio>` alongside the delivery intent, the two could disagree,
/// and when they did the argv silently fell through to `-an` and shipped a
/// video-only segment under a playlist advertising audio (the Google TV
/// outage). A runtime guard caught that; this makes the compiler catch it.
#[derive(Debug, Clone)]
pub enum ResolvedAudio {
    /// The segment carries no audio — [`AudioDelivery::Separate`], where audio
    /// is served as its own rendition.
    Silent,
    /// Copied from this slice of the title's one continuous encode.
    Muxed(ContinuousAudio, crate::options::MuxedAudio),
}

/// A segment whose audio is settled, and therefore the ONLY thing that can be
/// lowered to [`TranscodeOptions`].
///
/// Construct via [`SegmentOpts::resolve`]. There is no other constructor, and
/// no way to reach `to_transcode_options` without going through it.
#[derive(Debug, Clone)]
pub struct ResolvedSegment {
    opts: SegmentOpts,
    audio: ResolvedAudio,
}

impl ResolvedSegment {
    /// The request this was resolved from — for cache keying and logging.
    pub fn opts(&self) -> &SegmentOpts {
        &self.opts
    }

    /// The settled audio.
    pub fn audio(&self) -> &ResolvedAudio {
        &self.audio
    }
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

    /// Settle this segment's audio, producing the only value that can be
    /// lowered for transcode.
    ///
    /// `slice` is called exactly when the delivery is [`AudioDelivery::Muxed`]
    /// — i.e. when a continuous-audio slice is genuinely required — and its
    /// result is moved INTO the resolved value. A caller cannot skip it,
    /// forget it, or pass `None`: [`ResolvedAudio::Muxed`] has nowhere to put
    /// an absent source. That is what makes the video-only-segment bug a
    /// compile error rather than a silent `-an`.
    pub async fn resolve<F, Fut, E>(self, slice: F) -> Result<ResolvedSegment, E>
    where
        F: FnOnce(ContinuousAudio) -> Fut,
        Fut: std::future::Future<Output = Result<crate::options::MuxedAudio, E>>,
    {
        let audio = match self.audio {
            AudioDelivery::Separate => ResolvedAudio::Silent,
            AudioDelivery::Muxed(c) => ResolvedAudio::Muxed(c, slice(c).await?),
        };
        Ok(ResolvedSegment { opts: self, audio })
    }

    /// [`resolve`](Self::resolve) for a caller that already holds the slice, or
    /// can produce it without awaiting. Same totality: the `Muxed` arm has
    /// nowhere to put an absent source.
    pub fn resolve_with<F, E>(self, slice: F) -> Result<ResolvedSegment, E>
    where
        F: FnOnce(ContinuousAudio) -> Result<crate::options::MuxedAudio, E>,
    {
        let audio = match self.audio {
            AudioDelivery::Separate => ResolvedAudio::Silent,
            AudioDelivery::Muxed(c) => ResolvedAudio::Muxed(c, slice(c)?),
        };
        Ok(ResolvedSegment { opts: self, audio })
    }
}

impl ResolvedSegment {
    /// Lower to the transcoder's wire options — the ONLY bridge from the
    /// segment-legal subset into the general option space.
    pub fn to_transcode_options(&self) -> TranscodeOptions {
        let s = &self.opts;
        TranscodeOptions {
            container: s.container.into(),
            source_frame_rate: s.window.rate(),
            video: s.video.map(VideoCodec::from),
            source_video_codec: s.source_video_codec,
            // ALWAYS `None`: a segment never runs an audio encoder. When it
            // carries audio at all, the bytes are COPIED from the continuous
            // encode — which `muxed_audio_source` below now always names,
            // because `ResolvedAudio::Muxed` cannot exist without it.
            audio: None,
            video_bitrate_bps: s.video_bitrate_bps,
            audio_bitrate_bps: s.audio_bitrate_bps(),
            start_position_ticks: s.window.start_ticks(),
            duration_ticks: Some(s.window.duration_ticks()),
            audio_source_stream_index: s.audio_source_stream_index,
            burn_subtitle_stream_index: s.burn_subtitle_stream_index,
            burn_intent: s.burn_intent,
            burn_subtitle_is_text: s.burn_subtitle_is_text,
            burn_subtitle_ass_path: s.burn_subtitle_ass_path.clone(),
            burn_fonts_dir: s.burn_fonts_dir.clone(),
            decode_preroll_seconds: None,
            muxed_audio_source: match &self.audio {
                ResolvedAudio::Silent => None,
                ResolvedAudio::Muxed(_, m) => Some(m.clone()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn lowering_preserves_every_field() {
        let s = SegmentOpts {
            source_video_codec: None,
            container: SegmentContainer::Mpegts,
            video: Some(SegmentVideo::H264),
            audio: AudioDelivery::Muxed(ContinuousAudio {
                codec: SegmentAudio::Aac,

                bitrate_bps: Some(128_000),
            }),

            video_bitrate_bps: Some(3_000_000),
            window: pharos_core::SegmentWindow::for_segment(10, None, Some(600.0)),
            audio_source_stream_index: Some(1),
            burn_subtitle_stream_index: Some(0),
            burn_intent: true,
            burn_subtitle_is_text: true,
            burn_subtitle_ass_path: Some(std::path::PathBuf::from("/cache/sub.ass")),
            burn_fonts_dir: Some(std::path::PathBuf::from("/cache/fonts")),
        };
        let slice = crate::options::MuxedAudio {
            path: std::path::PathBuf::from("/cache/continuous.m4a"),
            start_seconds: 0.0,
        };
        let r = s
            .clone()
            .resolve_with(|_| Ok::<_, ()>(slice.clone()))
            .expect("slice supplied");
        let t = r.to_transcode_options();
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

    /// The Google TV outage, at the argv level.
    ///
    /// Lowering sets `audio: None`, and `build_args` turns that into `-an`. A
    /// MUXED segment lowered without its continuous-audio slice therefore
    /// produced a VIDEO-ONLY file — silently, exit 0 — under a playlist
    /// advertising audio. ExoPlayer fetched one such segment and stopped; the
    /// bytes pulled back from prod probed `nb_streams: 1`.
    ///
    /// That state no longer has a representation: `ResolvedSegment` is the only
    /// thing that lowers, and `ResolvedAudio::Muxed` cannot be built without a
    /// slice. The test that used to assert the broken argv could not be written
    /// today — it would not compile — so what is asserted instead is the
    /// property that replaced it: a resolved muxed segment COPIES, and never
    /// emits `-an`.
    #[test]
    fn a_resolved_muxed_segment_copies_its_audio_and_never_drops_it() {
        let base = SegmentOpts {
            source_video_codec: None,
            container: SegmentContainer::Mpegts,
            video: Some(SegmentVideo::H264),
            audio: AudioDelivery::Muxed(ContinuousAudio {
                codec: SegmentAudio::Aac,
                bitrate_bps: Some(128_000),
            }),
            video_bitrate_bps: Some(3_000_000),
            window: pharos_core::SegmentWindow::for_segment(1, None, Some(600.0)),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_intent: false,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        };
        let argv = |r: &ResolvedSegment| {
            crate::ffmpeg_transcode_args(
                "/src.mkv",
                &r.to_transcode_options(),
                crate::protocol::DeviceId::Cpu,
                "/out.ts",
                crate::DecodeOffload::Allowed,
            )
        };

        let resolved = base
            .clone()
            .resolve_with(|c| {
                assert_eq!(
                    c.codec,
                    SegmentAudio::Aac,
                    "the slice is asked for BY codec"
                );
                Ok::<_, ()>(crate::options::MuxedAudio {
                    path: std::path::PathBuf::from("/cache/continuous.m4a"),
                    start_seconds: 0.0,
                })
            })
            .expect("slice supplied");
        let a = argv(&resolved);
        assert!(
            !a.iter().any(|x| x == "-an"),
            "a muxed rung must never drop its audio: {a:?}"
        );
        let ca = a.iter().position(|x| x == "-c:a").expect("audio codec set");
        assert_eq!(
            a[ca + 1],
            "copy",
            "audio is copied, never re-encoded: {a:?}"
        );

        // The converse still holds: an audio-free rung resolves without a slice
        // (its closure is never run) and legitimately carries `-an`.
        let mut separate = base;
        separate.audio = AudioDelivery::Separate;
        let silent = separate
            .resolve_with(|_| -> Result<crate::options::MuxedAudio, ()> {
                unreachable!("an audio-free segment must not ask for a slice")
            })
            .expect("resolves with no slice");
        assert!(matches!(silent.audio(), ResolvedAudio::Silent));
        assert!(argv(&silent).iter().any(|x| x == "-an"));
    }
}
