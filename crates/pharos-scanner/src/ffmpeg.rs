//! `ffprobe` subprocess + SIMD JSON parsing via `sonic-rs`. Yields a full
//! `ProbeInfo { kind, probe: MediaProbe }` so the API surface can render
//! real Size / Bitrate / RunTimeTicks / Width / Height / FrameRate /
//! codec / channels / sample rate without re-shelling on every request.

use pharos_core::{
    DomainError, DomainResult, MediaChapter, MediaKind, MediaProbe, ProbeInfo, Prober,
    SubtitleTrack,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FfmpegProber {
    ffprobe_bin: PathBuf,
}

impl Default for FfmpegProber {
    fn default() -> Self {
        Self::new()
    }
}

impl FfmpegProber {
    pub fn new() -> Self {
        Self {
            ffprobe_bin: PathBuf::from("ffprobe"),
        }
    }

    pub fn with_binary(p: impl Into<PathBuf>) -> Self {
        Self {
            ffprobe_bin: p.into(),
        }
    }
}

impl Prober for FfmpegProber {
    async fn probe(&self, path: &Path) -> DomainResult<ProbeInfo> {
        let out = tokio::process::Command::new(&self.ffprobe_bin)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                "-show_chapters",
            ])
            .arg(path)
            .output()
            .await
            .map_err(|e| DomainError::Backend(format!("ffprobe spawn: {e}")))?;

        if !out.status.success() {
            return Err(DomainError::Backend(format!(
                "ffprobe exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        parse_ffprobe_output(&out.stdout)
    }
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    #[serde(default)]
    format: FfprobeFormat,
    #[serde(default)]
    chapters: Vec<FfprobeChapter>,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeChapter {
    #[serde(default)]
    id: i64,
    /// ffprobe reports seconds as a string (`"12.345"`).
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    end_time: Option<String>,
    #[serde(default)]
    tags: FfprobeChapterTags,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeChapterTags {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: String,
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    codec_name: Option<String>,
    /// Codec profile string ("High", "Main 10", "Profile 0"). Used to
    /// emit RFC 6381 codec tokens for HLS CODECS attribute.
    #[serde(default)]
    profile: Option<String>,
    /// Codec level × 10 reported by ffprobe for AVC/HEVC. None for
    /// VP9/AV1/Opus etc.
    #[serde(default)]
    level: Option<i32>,
    /// P13 — HDR / color metadata.
    #[serde(default)]
    pix_fmt: Option<String>,
    #[serde(default)]
    color_primaries: Option<String>,
    #[serde(default)]
    color_transfer: Option<String>,
    #[serde(default)]
    color_space: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    channels: Option<u32>,
    /// ffprobe reports `sample_rate` as a string ("48000").
    #[serde(default)]
    sample_rate: Option<String>,
    /// Rational frame rate, e.g. `"24000/1001"`. `avg_frame_rate`
    /// preferred over `r_frame_rate` for VFR sources.
    #[serde(default)]
    avg_frame_rate: Option<String>,
    #[serde(default)]
    r_frame_rate: Option<String>,
    #[serde(default)]
    tags: FfprobeStreamTags,
    #[serde(default)]
    disposition: FfprobeDisposition,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeStreamTags {
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeDisposition {
    #[serde(default)]
    default: i32,
    #[serde(default)]
    forced: i32,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeFormat {
    #[serde(default)]
    format_name: Option<String>,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    bit_rate: Option<String>,
    #[serde(default)]
    tags: FfprobeFormatTags,
}

/// `format.tags` from ffprobe. ID3v2 + Vorbis + MP4 metadata all
/// normalise into the same JSON keys; we accept the lowercase
/// canonical form (artist / album / album_artist / genre / title).
#[derive(Debug, Default, Deserialize)]
struct FfprobeFormatTags {
    #[serde(default, alias = "ARTIST", alias = "Artist")]
    artist: Option<String>,
    #[serde(default, alias = "ALBUM", alias = "Album")]
    album: Option<String>,
    #[serde(
        default,
        alias = "ALBUM_ARTIST",
        alias = "ALBUMARTIST",
        alias = "AlbumArtist",
        alias = "album_artist"
    )]
    album_artist: Option<String>,
    #[serde(default, alias = "GENRE", alias = "Genre")]
    genre: Option<String>,
}

/// Public so the criterion bench in `benches/parse.rs` can call directly
/// without spawning a real `ffprobe`.
pub fn parse_ffprobe_output(stdout: &[u8]) -> DomainResult<ProbeInfo> {
    let parsed: FfprobeOutput = sonic_rs::from_slice(stdout)
        .map_err(|e| DomainError::Backend(format!("ffprobe parse: {e}")))?;
    let kind = infer_kind(&parsed);
    let duration_ms = parsed
        .format
        .duration
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|s| (s * 1000.0) as u64);
    let size_bytes = parsed
        .format
        .size
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok());
    let bitrate_bps = parsed
        .format
        .bit_rate
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok());

    let video_stream = parsed.streams.iter().find(|s| s.codec_type == "video");
    let audio_stream = parsed.streams.iter().find(|s| s.codec_type == "audio");

    let video_codec = video_stream.and_then(|s| s.codec_name.clone());
    let video_profile = video_stream.and_then(|s| s.profile.clone());
    // ffprobe sometimes reports level=-99 for unknown / inapplicable
    // streams (VP9 / AV1 / Opus). Drop negative values.
    let video_level = video_stream
        .and_then(|s| s.level)
        .filter(|&l| l > 0)
        .map(|l| l as u32);
    let pixel_format = video_stream.and_then(|s| s.pix_fmt.clone());
    let color_primaries = video_stream.and_then(|s| s.color_primaries.clone());
    let color_transfer = video_stream.and_then(|s| s.color_transfer.clone());
    let color_space = video_stream.and_then(|s| s.color_space.clone());
    let audio_codec = audio_stream.and_then(|s| s.codec_name.clone());
    let width = video_stream.and_then(|s| s.width);
    let height = video_stream.and_then(|s| s.height);
    let frame_rate_mille = video_stream.and_then(|s| {
        s.avg_frame_rate
            .as_deref()
            .and_then(parse_rational_mille)
            .or_else(|| s.r_frame_rate.as_deref().and_then(parse_rational_mille))
    });
    let audio_channels = audio_stream.and_then(|s| s.channels);
    let sample_rate = audio_stream
        .and_then(|s| s.sample_rate.as_deref())
        .and_then(|s| s.parse::<u32>().ok());

    let subtitle_tracks: Vec<SubtitleTrack> = parsed
        .streams
        .iter()
        .filter(|s| s.codec_type == "subtitle")
        .filter_map(|s| {
            Some(SubtitleTrack {
                stream_index: s.index?,
                language: s.tags.language.clone(),
                codec: s.codec_name.clone(),
                title: s.tags.title.clone(),
                is_default: s.disposition.default != 0,
                is_forced: s.disposition.forced != 0,
            })
        })
        .collect();

    let chapters: Vec<MediaChapter> = parsed
        .chapters
        .iter()
        .enumerate()
        .filter_map(|(idx, c)| {
            let start = c
                .start_time
                .as_deref()
                .and_then(|s| s.parse::<f64>().ok())
                .map(|s| (s * 1000.0) as u64)?;
            let end = c
                .end_time
                .as_deref()
                .and_then(|s| s.parse::<f64>().ok())
                .map(|s| (s * 1000.0) as u64)
                .unwrap_or(start);
            let title = c
                .tags
                .title
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("Chapter {}", idx + 1));
            let _ = c.id;
            Some(MediaChapter {
                start_ms: start,
                end_ms: end,
                title,
            })
        })
        .collect();

    Ok(ProbeInfo {
        kind,
        probe: MediaProbe {
            size_bytes,
            duration_ms,
            container: parsed.format.format_name,
            bitrate_bps,
            video_codec,
            video_profile,
            video_level,
            pixel_format,
            color_primaries,
            color_transfer,
            color_space,
            audio_codec,
            width,
            height,
            frame_rate_mille,
            audio_channels,
            sample_rate,
            subtitle_tracks,
            artist: parsed.format.tags.artist,
            album: parsed.format.tags.album,
            album_artist: parsed.format.tags.album_artist,
            genre: parsed.format.tags.genre,
            chapters,
        },
    })
}

/// ffprobe rationals look like `"24000/1001"`. `0/0` (no frames seen)
/// and a 0 denominator both yield `None`.
fn parse_rational_mille(s: &str) -> Option<u32> {
    let (num, den) = s.split_once('/')?;
    let num: f64 = num.parse().ok()?;
    let den: f64 = den.parse().ok()?;
    if den == 0.0 {
        return None;
    }
    let fps = num / den;
    if !fps.is_finite() || fps <= 0.0 {
        return None;
    }
    Some((fps * 1000.0).round() as u32)
}

fn infer_kind(out: &FfprobeOutput) -> MediaKind {
    let has_video = out.streams.iter().any(|s| s.codec_type == "video");
    if has_video {
        MediaKind::Movie
    } else {
        MediaKind::Audio
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const VIDEO_JSON: &[u8] = br#"{
        "streams": [
            {"codec_type": "video", "codec_name": "vp9", "width": 1920, "height": 1080,
             "avg_frame_rate": "24000/1001", "r_frame_rate": "24/1"},
            {"codec_type": "audio", "codec_name": "opus", "channels": 2,
             "sample_rate": "48000"}
        ],
        "format": {"format_name": "matroska,webm", "duration": "3600.5",
                   "size": "5243523", "bit_rate": "4000000"}
    }"#;

    const AUDIO_JSON: &[u8] = br#"{
        "streams": [
            {"codec_type": "audio", "codec_name": "mp3", "channels": 2,
             "sample_rate": "44100"}
        ],
        "format": {"format_name": "mp3", "duration": "245.123",
                   "size": "9876543", "bit_rate": "320000"}
    }"#;

    #[test]
    fn parse_video_extracts_full_metadata() {
        let info = parse_ffprobe_output(VIDEO_JSON).unwrap();
        assert_eq!(info.kind, MediaKind::Movie);
        let p = &info.probe;
        assert_eq!(p.duration_ms, Some(3_600_500));
        assert_eq!(p.container.as_deref(), Some("matroska,webm"));
        assert_eq!(p.size_bytes, Some(5_243_523));
        assert_eq!(p.bitrate_bps, Some(4_000_000));
        assert_eq!(p.video_codec.as_deref(), Some("vp9"));
        assert_eq!(p.audio_codec.as_deref(), Some("opus"));
        assert_eq!(p.width, Some(1920));
        assert_eq!(p.height, Some(1080));
        assert_eq!(p.audio_channels, Some(2));
        assert_eq!(p.sample_rate, Some(48000));
        // 24000/1001 ≈ 23.976 fps → 23_976 in mille.
        assert_eq!(p.frame_rate_mille, Some(23_976));
    }

    #[test]
    fn parse_hdr10_extracts_color_transfer() {
        let json = br#"{
            "streams": [
                {"codec_type": "video", "codec_name": "hevc",
                 "profile": "Main 10", "level": 153,
                 "pix_fmt": "yuv420p10le",
                 "color_primaries": "bt2020",
                 "color_transfer": "smpte2084",
                 "color_space": "bt2020nc",
                 "width": 3840, "height": 2160}
            ],
            "format": {"format_name": "matroska", "duration": "100"}
        }"#;
        let info = parse_ffprobe_output(json).unwrap();
        let p = &info.probe;
        assert_eq!(p.pixel_format.as_deref(), Some("yuv420p10le"));
        assert_eq!(p.color_primaries.as_deref(), Some("bt2020"));
        assert_eq!(p.color_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(p.color_space.as_deref(), Some("bt2020nc"));
        // HDR derivation lands via the MediaProbe helper.
        assert!(p.is_hdr());
        assert_eq!(p.video_range(), "HDR");
    }

    #[test]
    fn parse_sdr_video_reports_sdr_video_range() {
        let json = br#"{
            "streams": [
                {"codec_type": "video", "codec_name": "h264",
                 "pix_fmt": "yuv420p",
                 "color_primaries": "bt709",
                 "color_transfer": "bt709",
                 "color_space": "bt709",
                 "width": 1920, "height": 1080}
            ],
            "format": {"format_name": "mp4"}
        }"#;
        let info = parse_ffprobe_output(json).unwrap();
        assert_eq!(info.probe.video_range(), "SDR");
        assert!(!info.probe.is_hdr());
    }

    #[test]
    fn parse_h264_extracts_profile_and_level() {
        let json = br#"{
            "streams": [
                {"codec_type": "video", "codec_name": "h264",
                 "profile": "High", "level": 40,
                 "width": 1920, "height": 1080},
                {"codec_type": "audio", "codec_name": "aac", "channels": 2,
                 "sample_rate": "48000"}
            ],
            "format": {"format_name": "mov,mp4", "duration": "120"}
        }"#;
        let info = parse_ffprobe_output(json).unwrap();
        let p = &info.probe;
        assert_eq!(p.video_codec.as_deref(), Some("h264"));
        assert_eq!(p.video_profile.as_deref(), Some("High"));
        assert_eq!(p.video_level, Some(40));
    }

    #[test]
    fn parse_drops_negative_level_sentinel() {
        // ffprobe reports level=-99 for VP9 / AV1 / formats without
        // discrete levels. Must come through as None, not Some(-99 as u32).
        let json = br#"{
            "streams": [
                {"codec_type": "video", "codec_name": "vp9",
                 "profile": "Profile 0", "level": -99,
                 "width": 1280, "height": 720}
            ],
            "format": {"format_name": "matroska"}
        }"#;
        let info = parse_ffprobe_output(json).unwrap();
        let p = &info.probe;
        assert_eq!(p.video_codec.as_deref(), Some("vp9"));
        assert_eq!(p.video_profile.as_deref(), Some("Profile 0"));
        assert_eq!(p.video_level, None);
    }

    #[test]
    fn parse_audio_only_skips_video_fields() {
        let info = parse_ffprobe_output(AUDIO_JSON).unwrap();
        assert_eq!(info.kind, MediaKind::Audio);
        let p = &info.probe;
        assert_eq!(p.duration_ms, Some(245_123));
        assert_eq!(p.video_codec, None);
        assert_eq!(p.width, None);
        assert_eq!(p.height, None);
        assert_eq!(p.frame_rate_mille, None);
        assert_eq!(p.audio_codec.as_deref(), Some("mp3"));
        assert_eq!(p.audio_channels, Some(2));
        assert_eq!(p.sample_rate, Some(44100));
    }

    #[test]
    fn parse_missing_duration_is_none() {
        let json = br#"{"streams":[{"codec_type":"audio"}],"format":{}}"#;
        let info = parse_ffprobe_output(json).unwrap();
        assert!(info.probe.duration_ms.is_none());
        assert!(info.probe.size_bytes.is_none());
    }

    #[test]
    fn parse_zero_over_zero_frame_rate_is_none() {
        let json = br#"{
            "streams": [{"codec_type": "video", "avg_frame_rate": "0/0",
                         "r_frame_rate": "0/0"}],
            "format": {}
        }"#;
        let info = parse_ffprobe_output(json).unwrap();
        assert_eq!(info.probe.frame_rate_mille, None);
    }

    #[test]
    fn parse_garbage_returns_err() {
        let res = parse_ffprobe_output(b"not json");
        assert!(res.is_err());
    }

    const VIDEO_WITH_CHAPTERS: &[u8] = br#"{
        "streams": [{"codec_type":"video","codec_name":"h264"}],
        "format": {"duration": "1800.0"},
        "chapters": [
            {"id": 0, "start_time": "0.000", "end_time": "300.000",
             "tags": {"title": "Opening"}},
            {"id": 1, "start_time": "300.000", "end_time": "900.000",
             "tags": {"title": ""}},
            {"id": 2, "start_time": "900.000", "end_time": "1800.000"}
        ]
    }"#;

    #[test]
    fn parse_chapters_extracts_with_fallback_titles() {
        let info = parse_ffprobe_output(VIDEO_WITH_CHAPTERS).unwrap();
        let chs = &info.probe.chapters;
        assert_eq!(chs.len(), 3);
        assert_eq!(chs[0].title, "Opening");
        assert_eq!(chs[0].start_ms, 0);
        assert_eq!(chs[0].end_ms, 300_000);
        // Empty title falls back to "Chapter {idx+1}".
        assert_eq!(chs[1].title, "Chapter 2");
        // Missing title also falls back.
        assert_eq!(chs[2].title, "Chapter 3");
        assert_eq!(chs[2].start_ms, 900_000);
    }

    #[test]
    fn parse_no_chapters_section_returns_empty_vec() {
        let info = parse_ffprobe_output(VIDEO_JSON).unwrap();
        assert!(info.probe.chapters.is_empty());
    }
}
