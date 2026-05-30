//! In-process libav probe — the highest-frequency tiny op (one call per
//! file during a library scan). Replaces the `ffprobe` fork in
//! `pharos-scanner::ffmpeg::FfmpegProber`, producing the same
//! `pharos_core::ProbeInfo` shape.
//!
//! We read the libav structs via raw pointers (`as_ptr()`) and map
//! integer fields to strings through libav's own name functions
//! (`avcodec_get_name`, `av_color_*_name`, `avcodec_profile_name`,
//! `av_get_pix_fmt_name`) so the strings match ffprobe's output exactly
//! (codec negotiation + HLS CODECS tokens depend on this parity).

use ffmpeg_the_third as ffmpeg;
use ffmpeg::ffi;
use pharos_core::{
    AudioTrack, MediaChapter, MediaKind, MediaProbe, ProbeInfo, SubtitleTrack,
};
use std::ffi::CStr;
use std::path::Path;

/// Error kind mirrored from the worker contract: `BadInput` for a file
/// libav can't open/parse, `Other` for anything else.
#[derive(Debug)]
pub enum ProbeError {
    BadInput(String),
    Other(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::BadInput(s) => write!(f, "bad input: {s}"),
            ProbeError::Other(s) => write!(f, "{s}"),
        }
    }
}

/// Probe `path` entirely in-process. Blocking.
pub fn probe(path: &Path) -> Result<ProbeInfo, ProbeError> {
    ffmpeg::init().map_err(|e| ProbeError::Other(format!("libav init: {e}")))?;
    let ictx = ffmpeg::format::input(path)
        .map_err(|e| ProbeError::BadInput(format!("open: {e}")))?;

    // SAFETY: `ictx` owns a valid AVFormatContext for the call's lifetime.
    let fmt = unsafe { &*ictx.as_ptr() };

    // Container name (ffprobe's format.format_name, e.g. "matroska,webm").
    let container = unsafe { cstr_opt((*fmt.iformat).name) };

    // Duration: AVFormatContext.duration is in AV_TIME_BASE (1e6) units.
    let duration_ms = if fmt.duration > 0 {
        Some((fmt.duration as i128 * 1000 / ffi::AV_TIME_BASE as i128) as u64)
    } else {
        None
    };
    let bitrate_bps = if fmt.bit_rate > 0 {
        Some(fmt.bit_rate as u64)
    } else {
        None
    };
    // Size from the filesystem (ffprobe's format.size).
    let size_bytes = std::fs::metadata(path).ok().map(|m| m.len());

    let mut video: Option<VideoFields> = None;
    let mut audio_tracks: Vec<AudioTrack> = Vec::new();
    let mut subtitle_tracks: Vec<SubtitleTrack> = Vec::new();

    for stream in ictx.streams() {
        // SAFETY: stream + its codecpar are valid for the iteration.
        let st = unsafe { &*stream.as_ptr() };
        let par = unsafe { &*st.codecpar };
        let disp = st.disposition;
        let index = stream.index() as u32;
        match par.codec_type {
            ffi::AVMediaType::VIDEO if video.is_none() => {
                video = Some(extract_video(par, st));
            }
            ffi::AVMediaType::AUDIO => {
                let meta = stream_tags(st);
                audio_tracks.push(AudioTrack {
                    stream_index: index,
                    codec: codec_name(par.codec_id),
                    channels: ch_count(par),
                    sample_rate: if par.sample_rate > 0 {
                        Some(par.sample_rate as u32)
                    } else {
                        None
                    },
                    language: meta.language,
                    title: meta.title,
                    is_default: disp & ffi::AV_DISPOSITION_DEFAULT != 0,
                    replaygain_track_centidb: meta.rg_track,
                    replaygain_album_centidb: meta.rg_album,
                });
            }
            ffi::AVMediaType::SUBTITLE => {
                let meta = stream_tags(st);
                subtitle_tracks.push(SubtitleTrack {
                    stream_index: index,
                    language: meta.language,
                    codec: codec_name(par.codec_id),
                    title: meta.title,
                    is_default: disp & ffi::AV_DISPOSITION_DEFAULT != 0,
                    is_forced: disp & ffi::AV_DISPOSITION_FORCED != 0,
                    is_hearing_impaired: disp & ffi::AV_DISPOSITION_HEARING_IMPAIRED != 0,
                });
            }
            _ => {}
        }
    }

    let chapters = extract_chapters(fmt);
    let ftags = format_tags(fmt);
    let kind = if video.is_some() {
        MediaKind::Movie
    } else {
        MediaKind::Audio
    };
    let v = video.unwrap_or_default();

    Ok(ProbeInfo {
        kind,
        probe: MediaProbe {
            size_bytes,
            duration_ms,
            container,
            bitrate_bps,
            video_codec: v.codec,
            video_profile: v.profile,
            video_level: v.level,
            pixel_format: v.pix_fmt,
            color_primaries: v.color_primaries,
            color_transfer: v.color_transfer,
            color_space: v.color_space,
            audio_codec: audio_tracks.first().and_then(|a| a.codec.clone()),
            width: v.width,
            height: v.height,
            frame_rate_mille: v.frame_rate_mille,
            audio_channels: audio_tracks.first().and_then(|a| a.channels),
            sample_rate: audio_tracks.first().and_then(|a| a.sample_rate),
            subtitle_tracks,
            audio_tracks,
            artist: ftags.artist,
            album: ftags.album,
            album_artist: ftags.album_artist,
            genre: ftags.genre,
            chapters,
            alternate_sources: Vec::new(),
        },
    })
}

#[derive(Default)]
struct VideoFields {
    codec: Option<String>,
    profile: Option<String>,
    level: Option<u32>,
    pix_fmt: Option<String>,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate_mille: Option<u32>,
}

fn extract_video(par: &ffi::AVCodecParameters, st: &ffi::AVStream) -> VideoFields {
    // avg_frame_rate preferred (VFR-correct), fall back to r_frame_rate.
    let frame_rate_mille =
        rational_mille(st.avg_frame_rate).or_else(|| rational_mille(st.r_frame_rate));
    VideoFields {
        codec: codec_name(par.codec_id),
        profile: profile_name(par.codec_id, par.profile),
        // ffprobe drops the -99 "unknown level" sentinel.
        level: if par.level > 0 {
            Some(par.level as u32)
        } else {
            None
        },
        pix_fmt: unsafe { cstr_owned(ffi::av_get_pix_fmt_name(ffi::AVPixelFormat(par.format))) },
        color_primaries: unsafe { cstr_owned(ffi::av_color_primaries_name(par.color_primaries)) },
        color_transfer: unsafe { cstr_owned(ffi::av_color_transfer_name(par.color_trc)) },
        color_space: unsafe { cstr_owned(ffi::av_color_space_name(par.color_space)) },
        width: if par.width > 0 {
            Some(par.width as u32)
        } else {
            None
        },
        height: if par.height > 0 {
            Some(par.height as u32)
        } else {
            None
        },
        frame_rate_mille,
    }
}

fn ch_count(par: &ffi::AVCodecParameters) -> Option<u32> {
    // ffmpeg 7+ uses AVChannelLayout.nb_channels.
    let n = par.ch_layout.nb_channels;
    if n > 0 {
        Some(n as u32)
    } else {
        None
    }
}

fn codec_name(id: ffi::AVCodecID) -> Option<String> {
    unsafe { cstr_owned(ffi::avcodec_get_name(id)) }
}

fn profile_name(id: ffi::AVCodecID, profile: i32) -> Option<String> {
    if profile == ffi::AV_PROFILE_UNKNOWN {
        return None;
    }
    unsafe { cstr_owned(ffi::avcodec_profile_name(id, profile)) }
}

fn rational_mille(r: ffi::AVRational) -> Option<u32> {
    if r.den == 0 || r.num <= 0 {
        return None;
    }
    let fps = r.num as f64 / r.den as f64;
    if !fps.is_finite() || fps <= 0.0 {
        return None;
    }
    Some((fps * 1000.0).round() as u32)
}

struct StreamMeta {
    language: Option<String>,
    title: Option<String>,
    rg_track: Option<i16>,
    rg_album: Option<i16>,
}

fn stream_tags(st: &ffi::AVStream) -> StreamMeta {
    StreamMeta {
        language: dict_get(st.metadata, "language"),
        title: dict_get(st.metadata, "title"),
        rg_track: dict_get(st.metadata, "replaygain_track_gain")
            .or_else(|| dict_get(st.metadata, "REPLAYGAIN_TRACK_GAIN"))
            .and_then(|s| parse_replaygain_centidb(&s)),
        rg_album: dict_get(st.metadata, "replaygain_album_gain")
            .or_else(|| dict_get(st.metadata, "REPLAYGAIN_ALBUM_GAIN"))
            .and_then(|s| parse_replaygain_centidb(&s)),
    }
}

#[derive(Default)]
struct FormatTags {
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
}

fn format_tags(fmt: &ffi::AVFormatContext) -> FormatTags {
    // libav normalises tag keys to lowercase canonical names in most
    // containers; `av_dict_get` with IGNORE_SUFFIX/case is handled by
    // trying the common variants.
    let get = |k: &str, alts: &[&str]| {
        dict_get(fmt.metadata, k).or_else(|| alts.iter().find_map(|a| dict_get(fmt.metadata, a)))
    };
    FormatTags {
        artist: get("artist", &["ARTIST", "Artist"]),
        album: get("album", &["ALBUM", "Album"]),
        album_artist: get(
            "album_artist",
            &["ALBUM_ARTIST", "ALBUMARTIST", "AlbumArtist"],
        ),
        genre: get("genre", &["GENRE", "Genre"]),
    }
}

fn extract_chapters(fmt: &ffi::AVFormatContext) -> Vec<MediaChapter> {
    let mut out = Vec::new();
    let n = fmt.nb_chapters as isize;
    for i in 0..n {
        // SAFETY: chapters is a valid array of `nb_chapters` pointers.
        let ch = unsafe { &**fmt.chapters.offset(i) };
        let tb = ch.time_base;
        let to_ms = |ts: i64| -> u64 {
            if tb.den == 0 {
                0
            } else {
                (ts as i128 * 1000 * tb.num as i128 / tb.den as i128).max(0) as u64
            }
        };
        let start_ms = to_ms(ch.start);
        let end_ms = if ch.end > ch.start {
            to_ms(ch.end)
        } else {
            start_ms
        };
        let title = dict_get(ch.metadata, "title")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("Chapter {}", i + 1));
        out.push(MediaChapter {
            start_ms,
            end_ms,
            title,
        });
    }
    out
}

// --- raw FFI helpers ---

/// Read a `*const c_char` (may be null) into an owned String.
unsafe fn cstr_opt(p: *const std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

/// Same, for name-function returns (null = unknown/none).
unsafe fn cstr_owned(p: *const std::os::raw::c_char) -> Option<String> {
    cstr_opt(p)
}

/// `av_dict_get(dict, key, NULL, 0)` → owned value if present.
fn dict_get(dict: *mut ffi::AVDictionary, key: &str) -> Option<String> {
    let ckey = std::ffi::CString::new(key).ok()?;
    // SAFETY: dict may be null (av_dict_get handles it); ckey is valid.
    unsafe {
        let entry = ffi::av_dict_get(dict, ckey.as_ptr(), std::ptr::null(), 0);
        if entry.is_null() {
            None
        } else {
            cstr_opt((*entry).value)
        }
    }
}

/// Parse a ReplayGain string ("-7.34 dB") into centidecibels (×100).
fn parse_replaygain_centidb(s: &str) -> Option<i16> {
    let t = s.trim().trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let v: f32 = t.trim().parse().ok()?;
    let scaled = (v * 100.0).round();
    if scaled.is_finite() && scaled >= i16::MIN as f32 && scaled <= i16::MAX as f32 {
        Some(scaled as i16)
    } else {
        None
    }
}
