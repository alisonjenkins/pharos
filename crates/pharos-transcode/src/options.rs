//! Transcode option types. Independent of ffmpeg specifics so callers
//! reason in terms of containers/codecs the wire protocol exposes.

use serde::{Deserialize, Serialize};

const JELLYFIN_TICKS_PER_SECOND: f64 = 10_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    Mp4,
    Mkv,
    WebM,
    Mpegts,
    Mp3,
    Flac,
    Ogg,
    /// P11 — raw ADTS (AAC) stream. Used by `/Audio/{id}/universal`
    /// when remuxing FLAC / lossless sources to AAC for clients
    /// without FLAC decode.
    Adts,
    /// P38 — fragmented MP4 segment for HLSv6. Same mp4 muxer as
    /// `Container::Mp4` but the HLS handler picks a different
    /// `-hls_segment_type` and the master playlist bumps to
    /// `EXT-X-VERSION:6`. Safari + iOS native HLS prefer this; the
    /// MPEG-TS path stays default for everyone else.
    Fmp4,
}

impl Container {
    /// ffmpeg `-f` muxer name.
    pub fn ffmpeg_muxer(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "matroska",
            Self::WebM => "webm",
            Self::Mpegts => "mpegts",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::Adts => "adts",
            // fMP4 segments use the mp4 muxer with movflags tuned for
            // fragmentation; the HLS handler appends the flags.
            Self::Fmp4 => "mp4",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::Mkv => "video/x-matroska",
            Self::WebM => "video/webm",
            Self::Mpegts => "video/mp2t",
            Self::Mp3 => "audio/mpeg",
            Self::Flac => "audio/flac",
            Self::Ogg => "audio/ogg",
            Self::Adts => "audio/aac",
            // RFC 6381 / Apple HLS Tech Note 281 — fMP4 segments are
            // `video/iso.segment`; the matching init segment is
            // `video/mp4` but the segment endpoint returns this.
            Self::Fmp4 => "video/iso.segment",
        }
    }

    /// Map a Jellyfin / ffprobe container token (lowercase) to the
    /// enum. Returns `None` for unknown / unsupported targets so the
    /// caller can fall back rather than 500.
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mp4" | "m4v" => Some(Self::Mp4),
            // P38 — explicit fMP4 token; only handler code currently
            // surfaces this. Device profiles continue to emit "mp4".
            "fmp4" | "iso-segment" | "iso.segment" => Some(Self::Fmp4),
            "mkv" | "matroska" => Some(Self::Mkv),
            "webm" => Some(Self::WebM),
            "ts" | "mpegts" => Some(Self::Mpegts),
            "mp3" => Some(Self::Mp3),
            "flac" => Some(Self::Flac),
            "ogg" => Some(Self::Ogg),
            "aac" | "adts" => Some(Self::Adts),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
    Vp9,
    Av1,
    /// Pass-through; ffmpeg `-c:v copy`.
    Copy,
}

impl VideoCodec {
    pub fn ffmpeg_codec(self) -> &'static str {
        match self {
            Self::H264 => "libx264",
            Self::H265 => "libx265",
            Self::Vp9 => "libvpx-vp9",
            Self::Av1 => "libaom-av1",
            Self::Copy => "copy",
        }
    }

    /// Resolve a Jellyfin / probe codec name to the enum. Falls back
    /// to `None` for codecs ffmpeg in our build can't encode (e.g.
    /// proprietary HEVC variants without -enable-libx265).
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h264" | "avc" | "avc1" => Some(Self::H264),
            "h265" | "hevc" => Some(Self::H265),
            "vp9" => Some(Self::Vp9),
            "av1" => Some(Self::Av1),
            "copy" => Some(Self::Copy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Aac,
    Mp3,
    Opus,
    Flac,
    Vorbis,
    /// Pass-through.
    Copy,
}

impl AudioCodec {
    pub fn ffmpeg_codec(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Mp3 => "libmp3lame",
            Self::Opus => "libopus",
            Self::Flac => "flac",
            Self::Vorbis => "libvorbis",
            Self::Copy => "copy",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "aac" | "mp4a" => Some(Self::Aac),
            "mp3" => Some(Self::Mp3),
            "opus" => Some(Self::Opus),
            "flac" => Some(Self::Flac),
            "vorbis" => Some(Self::Vorbis),
            "copy" => Some(Self::Copy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodeOptions {
    pub container: Container,
    pub video: Option<VideoCodec>,
    pub audio: Option<AudioCodec>,
    pub video_bitrate_bps: Option<u64>,
    pub audio_bitrate_bps: Option<u64>,
    /// Jellyfin-style ticks (10,000,000 per second). 0 = start of stream.
    pub start_position_ticks: u64,
    /// Optional clip duration in Jellyfin ticks.
    pub duration_ticks: Option<u64>,
    /// Source-relative audio-stream index (`AudioStreamIndex` query
    /// param). When set, ffmpeg gets `-map 0:a:{N}` so multi-track
    /// sources transcode the chosen track instead of the default.
    /// None defers to ffmpeg's default selection.
    pub audio_source_stream_index: Option<u32>,
    /// Subtitle-relative stream index for IMAGE-subtitle burn-in
    /// (PGS/VOBSUB/DVB — the only kind callers request burn for; text subs
    /// are delivered out-of-band, ADR-0006). When set, ffmpeg gets a
    /// `-filter_complex "[0:v:0][0:s:N]overlay=…"` graph rendering the
    /// bitmap subtitle into the video frames (B40 — the text-only
    /// `subtitles=` filter cannot render image subs). None leaves subtitles
    /// out of the encode entirely.
    pub burn_subtitle_stream_index: Option<u32>,
}

impl TranscodeOptions {
    pub fn start_position_seconds(&self) -> Option<f64> {
        if self.start_position_ticks == 0 {
            None
        } else {
            Some(self.start_position_ticks as f64 / JELLYFIN_TICKS_PER_SECOND)
        }
    }

    pub fn duration_seconds(&self) -> Option<f64> {
        self.duration_ticks
            .map(|d| d as f64 / JELLYFIN_TICKS_PER_SECOND)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn ticks_to_seconds_roundtrip() {
        let o = TranscodeOptions {
            container: Container::Mp4,
            video: None,
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 30_000_000,
            duration_ticks: Some(50_000_000),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        assert_eq!(o.start_position_seconds(), Some(3.0));
        assert_eq!(o.duration_seconds(), Some(5.0));
    }

    #[test]
    fn zero_start_returns_none() {
        let o = TranscodeOptions {
            container: Container::Mp4,
            video: None,
            audio: None,
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        assert_eq!(o.start_position_seconds(), None);
    }

    #[test]
    fn container_content_types_match_jellyfin_expectations() {
        assert_eq!(Container::Mp4.content_type(), "video/mp4");
        assert_eq!(Container::Mpegts.content_type(), "video/mp2t");
        assert_eq!(Container::Mp3.content_type(), "audio/mpeg");
    }

    #[test]
    fn fmp4_container_muxes_as_mp4_with_segment_type() {
        // P38 — the muxer name has to stay "mp4" so ffmpeg pipes the
        // bytes through the same demuxer the HLS handler initialises
        // its `-movflags` for. The wire-shape content-type swap to
        // `video/iso.segment` is what tells Safari it's HLSv6.
        assert_eq!(Container::Fmp4.ffmpeg_muxer(), "mp4");
        assert_eq!(Container::Fmp4.content_type(), "video/iso.segment");
        assert_eq!(Container::from_name("fmp4"), Some(Container::Fmp4));
        assert_eq!(Container::from_name("iso-segment"), Some(Container::Fmp4));
        // "mp4" itself stays the regular mp4 progressive container so
        // device-profile parsers don't accidentally upgrade clients
        // that asked for plain mp4.
        assert_eq!(Container::from_name("mp4"), Some(Container::Mp4));
    }

    #[test]
    fn video_codec_maps_to_known_ffmpeg_lib() {
        assert_eq!(VideoCodec::H264.ffmpeg_codec(), "libx264");
        assert_eq!(VideoCodec::Av1.ffmpeg_codec(), "libaom-av1");
        assert_eq!(VideoCodec::Copy.ffmpeg_codec(), "copy");
    }

    #[test]
    fn audio_codec_maps_to_known_ffmpeg_lib() {
        assert_eq!(AudioCodec::Aac.ffmpeg_codec(), "aac");
        assert_eq!(AudioCodec::Opus.ffmpeg_codec(), "libopus");
    }
}
