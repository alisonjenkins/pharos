//! Transcode option types. Independent of ffmpeg specifics so callers
//! reason in terms of containers/codecs the wire protocol exposes.

use pharos_core::time::Ticks;
use serde::{Deserialize, Serialize};

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
    /// Whether this container is used to carry ONE HLS segment of a timeline
    /// tiled by independent per-segment transcodes.
    ///
    /// This is the property that decides whether a transcode needs
    /// frame-exact boundary timestamps: a segment's first frame has to land
    /// exactly where the previous segment's last frame ended, or the encoder
    /// resolves the sub-frame residue by duplicating or dropping the boundary
    /// frame — a hitch at every boundary, on every client. A progressive
    /// output has no boundaries and needs none of it.
    ///
    /// Derived from the container rather than re-tested per call site so a new
    /// segment container cannot be added without inheriting the treatment (the
    /// mpegts path was previously missed, so h264/TS segments never received
    /// `-enc_time_base` at all).
    pub fn is_hls_segment(self) -> bool {
        matches!(self, Container::Mpegts | Container::Fmp4)
    }

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

/// The title's one continuous audio encode, and where in the SOURCE its
/// first sample sits.
///
/// The start is not decoration. `-ss` on an input is relative to that
/// input's own start time, so seeking this file by an absolute source
/// position lands at `start + position` — measured on ffmpeg 8.1 with a
/// tone-marked fixture, a file spanning source 30..40 s served silence for
/// `-ss 32` and the correct content for `-ss 2`. A segment therefore has to
/// seek it by `position - start`, and cannot do that without knowing the
/// start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MuxedAudio {
    pub path: std::path::PathBuf,
    /// Source position of this file's first sample. 0 for a whole-title
    /// encode.
    pub start_seconds: f64,
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
    /// `true` when `burn_subtitle_stream_index` refers to a TEXT/ASS track
    /// (picks ffmpeg's `subtitles=` filter instead of the image `overlay`
    /// graph). `false` for image-subtitle burn or when no burn is set.
    pub burn_subtitle_is_text: bool,
    /// Local `.ass` sidecar to burn from for a TEXT/ASS burn. `Some` points
    /// the `subtitles=` filter at this small pre-extracted, disk-cached file
    /// (`filename=<p>`, no `si=` — a sidecar is single-track) instead of the
    /// whole source container. ffmpeg's `subtitles` filter opens a SECOND
    /// demuxer on `filename=` at init — ONCE PER SEGMENT — and reads the WHOLE
    /// container to collect subtitle packets + embedded fonts; pointing it at a
    /// multi-GB NFS source re-demuxes the entire file every 6 s segment (the
    /// documented whole-file-demux stutter). The sidecar preserves the source's
    /// ABSOLUTE event times, so the `setpts` alignment is unchanged. `None`
    /// falls back to `filename=<source>:si=N` (correct, just slower).
    pub burn_subtitle_ass_path: Option<std::path::PathBuf>,
    /// Directory of extracted embedded font attachments handed to libass as
    /// `:fontsdir=<dir>` alongside `burn_subtitle_ass_path`. libass scans it by
    /// font CONTENT, so index-named files are fine. `None` renders with
    /// system/default fonts. Only consulted on the TEXT/ASS-sidecar path.
    pub burn_fonts_dir: Option<std::path::PathBuf>,
    /// Overrides [`crate::DECODE_PREROLL_SECONDS`] — how far before
    /// `start_position_ticks` the decoder is seeded before the surplus is
    /// trimmed back off. `None` takes the default.
    ///
    /// Raised on a re-attempt when a produced segment came back short of
    /// frames: the container index claimed a random-access point the decoder
    /// could not actually start from, and the default preroll did not reach
    /// far enough back to find a real one.
    #[serde(default)]
    pub decode_preroll_seconds: Option<f64>,
    /// Take this segment's audio by COPY from the title's one continuous
    /// audio encode at this path, instead of encoding audio here.
    ///
    /// Encoding audio per segment re-primes the codec at every boundary: each
    /// segment emits its own priming frame and starts a fresh frame grid at
    /// its own seek point, so consecutive segments carry overlapping,
    /// phase-misaligned audio. Copying from one encode leaves a single global
    /// grid, so every audio frame belongs to exactly one segment at a
    /// deterministic timestamp and there is nothing to drift against.
    ///
    /// The file is added as a second input seeked to the SAME position as the
    /// source, because two inputs seeked to different positions are re-based
    /// by different amounts and the audio would land offset from the video by
    /// the difference.
    #[serde(default)]
    pub muxed_audio_source: Option<MuxedAudio>,
}

impl TranscodeOptions {
    pub fn start_position_seconds(&self) -> Option<f64> {
        if self.start_position_ticks == 0 {
            None
        } else {
            Some(Ticks(self.start_position_ticks).seconds())
        }
    }

    pub fn duration_seconds(&self) -> Option<f64> {
        self.duration_ticks.map(|d| Ticks(d).seconds())
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
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
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
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
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
