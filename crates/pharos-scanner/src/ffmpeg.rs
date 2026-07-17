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
    // `#[serde(default)]` so a single stream lacking `codec_type`
    // (attachment/data/unknown shapes, or a future ffprobe build) is
    // ignored rather than failing the whole document's deserialization —
    // which previously dropped the entire media file from the library.
    #[serde(default)]
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
    /// P37 — ReplayGain track gain string ffprobe reports as
    /// `"-7.34 dB"`. Parsed at AudioTrack construction time.
    #[serde(default, rename = "replaygain_track_gain")]
    replaygain_track_gain: Option<String>,
    #[serde(default, rename = "replaygain_album_gain")]
    replaygain_album_gain: Option<String>,
    /// Some ffprobe builds emit the uppercase variant. Same string
    /// shape; just match either casing.
    #[serde(default, rename = "REPLAYGAIN_TRACK_GAIN")]
    replaygain_track_gain_upper: Option<String>,
    #[serde(default, rename = "REPLAYGAIN_ALBUM_GAIN")]
    replaygain_album_gain_upper: Option<String>,
    /// Attachment streams (fonts) carry the file name + MIME type here.
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    mimetype: Option<String>,
}

/// P37 — parse a ReplayGain string of the form `"-7.34 dB"` /
/// `"+0.10 dB"` / `"-7.34"` into centidecibels (× 100). Returns
/// `None` on garbage input or values that overflow i16.
fn parse_replaygain_centidb(s: &str) -> Option<i16> {
    let trimmed = s.trim().trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let trimmed = trimmed.trim();
    let v: f32 = trimmed.parse().ok()?;
    let scaled = (v * 100.0).round();
    if scaled.is_finite() && scaled >= i16::MIN as f32 && scaled <= i16::MAX as f32 {
        Some(scaled as i16)
    } else {
        None
    }
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeDisposition {
    #[serde(default)]
    default: i32,
    #[serde(default)]
    forced: i32,
    /// P35 — ffprobe reports `disposition.hearing_impaired` for SDH
    /// / CC tracks. Promotes through to `SubtitleTrack` so the
    /// jellyfin-web picker labels the track and the audio-only deaf
    /// filter on `/Items` returns the right rows.
    #[serde(default)]
    hearing_impaired: i32,
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
    /// Embedded track title (`TIT2` in ID3, `TITLE` in Vorbis/FLAC, `©nam`
    /// in MP4). The authoritative song name — the scanner prefers it over
    /// the filename stem so tracks keep their real names.
    #[serde(default, alias = "TITLE", alias = "Title")]
    title: Option<String>,
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
    /// `track` is "3" or "3/12" depending on tagger; parse the leading int.
    #[serde(default, alias = "TRACK", alias = "Track", alias = "track_number")]
    track: Option<String>,
    #[serde(
        default,
        alias = "DISC",
        alias = "Disc",
        alias = "disc_number",
        alias = "DISCNUMBER"
    )]
    disc: Option<String>,
    /// Vorbis `date`, ID3 `TYER`/`TDRC` normalise to `date`; some taggers
    /// write `year`. Either way the leading 4 digits are the release year.
    /// For reissues this is the *reissue* year, not the original — see
    /// `original_date` below, which wins when present.
    #[serde(
        default,
        alias = "DATE",
        alias = "Date",
        alias = "year",
        alias = "YEAR"
    )]
    date: Option<String>,
    /// Original-release date — ID3v2.4 `TDOR` (`TORY` on v2.3), Vorbis
    /// `ORIGINALDATE`/`ORIGINALYEAR`. On a remaster/reissue the plain
    /// `date` is the reissue year (e.g. 2008) while this carries the real
    /// first-release year (e.g. 1991). Preferred as the item's year so
    /// albums sort + display by original release like Jellyfin does.
    #[serde(
        default,
        alias = "TDOR",
        alias = "TORY",
        alias = "originaldate",
        alias = "ORIGINALDATE",
        alias = "originalyear",
        alias = "ORIGINALYEAR"
    )]
    original_date: Option<String>,
    /// B90 — embedded long-form description → `Overview`.
    #[serde(
        default,
        alias = "SYNOPSIS",
        alias = "description",
        alias = "DESCRIPTION",
        alias = "comment",
        alias = "COMMENT",
        alias = "plot",
        alias = "PLOT",
        alias = "summary",
        alias = "SUMMARY"
    )]
    synopsis: Option<String>,
    /// B90 — embedded parental/content rating → `OfficialRating`.
    #[serde(
        default,
        alias = "CONTENT_RATING",
        alias = "rating",
        alias = "RATING",
        alias = "mpaa",
        alias = "MPAA",
        alias = "law_rating",
        alias = "LAW_RATING",
        alias = "icra"
    )]
    content_rating: Option<String>,
    /// B90 — embedded network / publisher / studio → `Studios`.
    #[serde(
        default,
        alias = "NETWORK",
        alias = "publisher",
        alias = "PUBLISHER",
        alias = "studio",
        alias = "STUDIO",
        alias = "TVNetworkName"
    )]
    network: Option<String>,
    /// B90 — MP4/Matroska container creation date, a `date` fallback.
    #[serde(default, alias = "creation_time")]
    creation_time: Option<String>,
}

/// Parse the leading unsigned integer of a `track`/`disc` tag ("3", "3/12").
fn leading_uint(s: &str) -> Option<u32> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok().filter(|n| *n > 0)
}

/// Leading 4-digit year of a `date` tag ("1999", "1999-06-22").
fn leading_year(s: &str) -> Option<u32> {
    let digits: String = s.trim().chars().take(4).collect();
    (digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
        .filter(|y| (1000..=2999).contains(y))
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

    // P16 — every audio stream surfaces as an AudioTrack. The scalar
    // `audio_codec` / `audio_channels` / `sample_rate` fields stay
    // populated from the first stream for back-compat.
    let audio_tracks: Vec<pharos_core::AudioTrack> = parsed
        .streams
        .iter()
        .filter(|s| s.codec_type == "audio")
        .map(|s| {
            let rg_track = s
                .tags
                .replaygain_track_gain
                .as_deref()
                .or(s.tags.replaygain_track_gain_upper.as_deref())
                .and_then(parse_replaygain_centidb);
            let rg_album = s
                .tags
                .replaygain_album_gain
                .as_deref()
                .or(s.tags.replaygain_album_gain_upper.as_deref())
                .and_then(parse_replaygain_centidb);
            pharos_core::AudioTrack {
                stream_index: s.index.unwrap_or(0),
                codec: s.codec_name.clone(),
                channels: s.channels,
                sample_rate: s.sample_rate.as_deref().and_then(|r| r.parse::<u32>().ok()),
                language: s.tags.language.clone(),
                title: s.tags.title.clone(),
                is_default: s.disposition.default != 0,
                replaygain_track_centidb: rg_track,
                replaygain_album_centidb: rg_album,
            }
        })
        .collect();

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
                is_hearing_impaired: s.disposition.hearing_impaired != 0,
            })
        })
        .collect();

    let attachments: Vec<pharos_core::MediaAttachment> = parsed
        .streams
        .iter()
        .filter(|s| s.codec_type == "attachment")
        .filter_map(|s| {
            Some(pharos_core::MediaAttachment {
                stream_index: s.index?,
                filename: s.tags.filename.clone(),
                mime_type: s.tags.mimetype.clone(),
                codec: s.codec_name.clone(),
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
            audio_tracks,
            attachments,
            title: parsed.format.tags.title.filter(|t| !t.trim().is_empty()),
            artist: parsed.format.tags.artist,
            album: parsed.format.tags.album,
            album_artist: parsed.format.tags.album_artist,
            genre: parsed.format.tags.genre,
            track_number: parsed.format.tags.track.as_deref().and_then(leading_uint),
            disc_number: parsed.format.tags.disc.as_deref().and_then(leading_uint),
            // Prefer the original-release year over the (possibly reissue)
            // `date` tag so a 2008 remaster of a 1991 album shows 1991.
            year: parsed
                .format
                .tags
                .original_date
                .as_deref()
                .and_then(leading_year)
                .or_else(|| parsed.format.tags.date.as_deref().and_then(leading_year)),
            synopsis: parsed.format.tags.synopsis.filter(|s| !s.trim().is_empty()),
            content_rating: parsed
                .format
                .tags
                .content_rating
                .filter(|s| !s.trim().is_empty()),
            network: parsed.format.tags.network.filter(|s| !s.trim().is_empty()),
            // Full raw date for PremiereDate; prefer original release, then the
            // plain date, then the container creation_time.
            release_date: parsed
                .format
                .tags
                .original_date
                .filter(|s| !s.trim().is_empty())
                .or_else(|| parsed.format.tags.date.filter(|s| !s.trim().is_empty()))
                .or_else(|| {
                    parsed
                        .format
                        .tags
                        .creation_time
                        .filter(|s| !s.trim().is_empty())
                }),
            chapters,
            // P34 — alternate editions enrichment lives in a
            // future scanner pass (sibling-file convention reader
            // or NFO metadata). FfmpegProber today only probes a
            // single file.
            alternate_sources: Vec::new(),
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
    fn parse_audio_reads_embedded_title_and_prefers_original_year() {
        // A 2008 reissue of a 1991 album: `date` is the reissue year, `TDOR`
        // the original. The item must surface the original (1991) and the
        // embedded track title, NOT the album folder name.
        let json = br#"{
            "streams": [{"codec_type": "audio", "codec_name": "mp3",
                         "channels": 2, "sample_rate": "44100"}],
            "format": {"format_name": "mp3", "duration": "248.0",
                       "tags": {"title": "Something Got Me Started",
                                "album": "Stars", "artist": "Simply Red",
                                "track": "1/10", "date": "2008",
                                "TDOR": "1991-10"}}
        }"#;
        let p = parse_ffprobe_output(json).unwrap().probe;
        assert_eq!(p.title.as_deref(), Some("Something Got Me Started"));
        assert_eq!(p.album.as_deref(), Some("Stars"));
        assert_eq!(p.track_number, Some(1));
        // Original release year wins over the 2008 reissue `date`.
        assert_eq!(p.year, Some(1991));
    }

    #[test]
    fn parse_audio_falls_back_to_date_when_no_original_year() {
        let json = br#"{
            "streams": [{"codec_type": "audio", "codec_name": "flac"}],
            "format": {"format_name": "flac",
                       "tags": {"DATE": "2015-06-01", "TITLE": ""}}
        }"#;
        let p = parse_ffprobe_output(json).unwrap().probe;
        // No original-date tag → the plain `date` year is used.
        assert_eq!(p.year, Some(2015));
        // A blank title tag is treated as absent (falls back to filename).
        assert_eq!(p.title, None);
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
