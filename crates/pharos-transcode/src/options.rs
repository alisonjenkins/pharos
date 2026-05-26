//! Transcode option types. Independent of ffmpeg specifics so callers
//! reason in terms of containers/codecs the wire protocol exposes.

const JELLYFIN_TICKS_PER_SECOND: f64 = 10_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Mkv,
    WebM,
    Mpegts,
    Mp3,
    Flac,
    Ogg,
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Debug, Clone)]
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
