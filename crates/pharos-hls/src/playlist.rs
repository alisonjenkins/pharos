//! HLS master and media playlist parser for the subset emitted by Pharos.

use std::fmt;

use crate::uri;

/// Parsed HLS master playlist (`master.m3u8`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Master {
    pub version: Option<u8>,
    pub independent_segments: bool,
    pub variants: Vec<Variant>,
    pub audio: Vec<Rendition>,
    pub unsupported: Vec<Unsupported>,
}

/// One `#EXT-X-STREAM-INF` variant stream entry in a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub codecs: Vec<String>,
    pub resolution: Option<(u32, u32)>,
    pub audio_group: Option<String>,
}

/// One `#EXT-X-MEDIA:TYPE=AUDIO` rendition entry in a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    pub group_id: String,
    pub name: String,
    pub uri: Option<String>,
    pub default: bool,
    pub autoselect: bool,
    pub language: Option<String>,
}

/// Parsed HLS media playlist (video variant or audio rendition).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPlaylist {
    pub version: Option<u8>,
    pub target_duration_s: u32,
    pub media_sequence: u64,
    pub vod: bool,
    pub ended: bool,
    pub init_uri: Option<String>,
    pub start_offset_ms: Option<u64>,
    pub segments: Vec<Segment>,
    pub unsupported: Vec<Unsupported>,
}

/// One media segment with millisecond timing accumulated from `#EXTINF`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub uri: String,
    pub start_ms: u64,
    pub duration_ms: u64,
}

/// HLS features outside the fMP4 VOD subset supported by the Pharos engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    Encrypted,
    ByteRange,
    Discontinuity,
    Live,
    IFramesOnly,
    MediaTypeSubtitles,
    ClosedCaptions,
}

impl Unsupported {
    /// Stable telemetry and log token for this unsupported feature.
    pub fn label(self) -> &'static str {
        match self {
            Self::Encrypted => "encrypted",
            Self::ByteRange => "byte_range",
            Self::Discontinuity => "discontinuity",
            Self::Live => "live",
            Self::IFramesOnly => "iframes_only",
            Self::MediaTypeSubtitles => "subtitles",
            Self::ClosedCaptions => "closed_captions",
        }
    }
}

/// Syntax or structural failure when parsing an HLS master or media playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    NotM3u8,
    MissingTargetDuration,
    BadAttribute {
        tag: &'static str,
        line: usize,
        value: String,
    },
    BadDuration {
        line: usize,
        value: String,
    },
    UriWithoutTag {
        line: usize,
    },
    TagWithoutUri {
        tag: &'static str,
        line: usize,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotM3u8 => f.write_str("playlist does not start with #EXTM3U"),
            Self::MissingTargetDuration => {
                f.write_str("media playlist missing #EXT-X-TARGETDURATION")
            }
            Self::BadAttribute { tag, line, value } => {
                write!(f, "invalid #{tag} attribute at line {line}: {value}")
            }
            Self::BadDuration { line, value } => {
                write!(f, "invalid #EXTINF duration at line {line}: {value}")
            }
            Self::UriWithoutTag { line } => {
                write!(f, "unexpected URI without preceding tag at line {line}")
            }
            Self::TagWithoutUri { tag, line } => {
                write!(f, "#{tag} at line {line} has no following URI")
            }
        }
    }
}

impl std::error::Error for ParseError {}

struct PendingVariant {
    bandwidth: u64,
    codecs: Vec<String>,
    resolution: Option<(u32, u32)>,
    audio_group: Option<String>,
}

/// Parses an HLS master playlist and resolves variant and rendition URIs against `base_url`.
pub fn parse_master(text: &str, base_url: &str) -> Result<Master, ParseError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut saw_header = false;
    let mut master = Master {
        version: None,
        independent_segments: false,
        variants: Vec::new(),
        audio: Vec::new(),
        unsupported: Vec::new(),
    };
    let mut pending_stream_inf: Option<(PendingVariant, usize)> = None;

    for (idx, raw_line) in text.lines().enumerate() {
        let line_num = idx.saturating_add(1);
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if !saw_header {
            if line != "#EXTM3U" {
                return Err(ParseError::NotM3u8);
            }
            saw_header = true;
            continue;
        }
        if !line.starts_with('#') {
            let Some((pending, _)) = pending_stream_inf.take() else {
                return Err(ParseError::UriWithoutTag { line: line_num });
            };
            master.variants.push(Variant {
                uri: uri::resolve(base_url, line),
                bandwidth: pending.bandwidth,
                codecs: pending.codecs,
                resolution: pending.resolution,
                audio_group: pending.audio_group,
            });
            continue;
        }
        if !line.starts_with("#EXT") {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-VERSION:") {
            let v = rest
                .trim()
                .parse::<u8>()
                .map_err(|_| ParseError::BadAttribute {
                    tag: "EXT-X-VERSION",
                    line: line_num,
                    value: rest.to_string(),
                })?;
            master.version = Some(v);
        } else if line == "#EXT-X-INDEPENDENT-SEGMENTS" {
            master.independent_segments = true;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            parse_ext_x_media(rest, line_num, base_url, &mut master)?;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            if let Some((_, prev_line)) = pending_stream_inf {
                return Err(ParseError::TagWithoutUri {
                    tag: "EXT-X-STREAM-INF",
                    line: prev_line,
                });
            }
            let pending = parse_stream_inf(rest, line_num)?;
            pending_stream_inf = Some((pending, line_num));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            parse_ext_x_key(rest, line_num, &mut master.unsupported)?;
        }
    }

    if !saw_header {
        return Err(ParseError::NotM3u8);
    }
    if let Some((_, tag_line)) = pending_stream_inf {
        return Err(ParseError::TagWithoutUri {
            tag: "EXT-X-STREAM-INF",
            line: tag_line,
        });
    }

    Ok(master)
}

/// Parses an HLS media playlist and resolves init and segment URIs against `base_url`.
pub fn parse_media(text: &str, base_url: &str) -> Result<MediaPlaylist, ParseError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut saw_header = false;
    let mut version: Option<u8> = None;
    let mut target_duration_s: Option<u32> = None;
    let mut media_sequence: u64 = 0;
    let mut vod = false;
    let mut ended = false;
    let mut init_uri: Option<String> = None;
    let mut start_offset_ms: Option<u64> = None;
    let mut segments: Vec<Segment> = Vec::new();
    let mut unsupported: Vec<Unsupported> = Vec::new();
    let mut pending_extinf: Option<(f64, usize)> = None;
    let mut accum_secs: f64 = 0.0;

    for (idx, raw_line) in text.lines().enumerate() {
        let line_num = idx.saturating_add(1);
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if !saw_header {
            if line != "#EXTM3U" {
                return Err(ParseError::NotM3u8);
            }
            saw_header = true;
            continue;
        }
        if !line.starts_with('#') {
            let Some((dur_secs, _)) = pending_extinf.take() else {
                return Err(ParseError::UriWithoutTag { line: line_num });
            };
            let start_ms = (accum_secs * 1000.0).round() as u64;
            accum_secs += dur_secs;
            let end_ms = (accum_secs * 1000.0).round() as u64;
            let duration_ms = end_ms.saturating_sub(start_ms);
            segments.push(Segment {
                uri: uri::resolve(base_url, line),
                start_ms,
                duration_ms,
            });
            continue;
        }
        if !line.starts_with("#EXT") {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXT-X-VERSION:") {
            let v = rest
                .trim()
                .parse::<u8>()
                .map_err(|_| ParseError::BadAttribute {
                    tag: "EXT-X-VERSION",
                    line: line_num,
                    value: rest.to_string(),
                })?;
            version = Some(v);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            let d = rest
                .trim()
                .parse::<u32>()
                .map_err(|_| ParseError::BadAttribute {
                    tag: "EXT-X-TARGETDURATION",
                    line: line_num,
                    value: rest.to_string(),
                })?;
            target_duration_s = Some(d);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = rest
                .trim()
                .parse::<u64>()
                .map_err(|_| ParseError::BadAttribute {
                    tag: "EXT-X-MEDIA-SEQUENCE",
                    line: line_num,
                    value: rest.to_string(),
                })?;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            match rest.trim() {
                "VOD" => vod = true,
                "EVENT" => vod = false,
                other => {
                    return Err(ParseError::BadAttribute {
                        tag: "EXT-X-PLAYLIST-TYPE",
                        line: line_num,
                        value: other.to_string(),
                    });
                }
            }
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = parse_attributes(rest, "EXT-X-MAP", line_num)?;
            let mut map_uri: Option<String> = None;
            for (k, v) in attrs {
                match k {
                    "URI" if !v.is_empty() => map_uri = Some(uri::resolve(base_url, v)),
                    "BYTERANGE" => push_unsupported(&mut unsupported, Unsupported::ByteRange),
                    _ => {}
                }
            }
            let Some(resolved) = map_uri else {
                return Err(ParseError::BadAttribute {
                    tag: "EXT-X-MAP",
                    line: line_num,
                    value: rest.to_string(),
                });
            };
            init_uri = Some(resolved);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-START:") {
            start_offset_ms = parse_start(rest, line_num)?;
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            if let Some((_, prev_line)) = pending_extinf {
                return Err(ParseError::TagWithoutUri {
                    tag: "EXTINF",
                    line: prev_line,
                });
            }
            let dur_str = rest.split_once(',').map_or(rest, |(d, _)| d).trim();
            let secs = dur_str
                .parse::<f64>()
                .map_err(|_| ParseError::BadDuration {
                    line: line_num,
                    value: dur_str.to_string(),
                })?;
            if !secs.is_finite() || secs < 0.0 {
                return Err(ParseError::BadDuration {
                    line: line_num,
                    value: dur_str.to_string(),
                });
            }
            pending_extinf = Some((secs, line_num));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            parse_ext_x_key(rest, line_num, &mut unsupported)?;
        } else if line == "#EXT-X-BYTERANGE" || line.starts_with("#EXT-X-BYTERANGE:") {
            push_unsupported(&mut unsupported, Unsupported::ByteRange);
        } else if line == "#EXT-X-DISCONTINUITY" {
            push_unsupported(&mut unsupported, Unsupported::Discontinuity);
        } else if line == "#EXT-X-I-FRAMES-ONLY" {
            push_unsupported(&mut unsupported, Unsupported::IFramesOnly);
        } else if line == "#EXT-X-STREAM-INF" || line.starts_with("#EXT-X-STREAM-INF:") {
            return Err(ParseError::TagWithoutUri {
                tag: "EXT-X-STREAM-INF",
                line: line_num,
            });
        }
    }

    if !saw_header {
        return Err(ParseError::NotM3u8);
    }
    if let Some((_, tag_line)) = pending_extinf {
        return Err(ParseError::TagWithoutUri {
            tag: "EXTINF",
            line: tag_line,
        });
    }
    let Some(target_duration_s) = target_duration_s else {
        return Err(ParseError::MissingTargetDuration);
    };
    if !ended && !vod {
        push_unsupported(&mut unsupported, Unsupported::Live);
    }

    Ok(MediaPlaylist {
        version,
        target_duration_s,
        media_sequence,
        vod,
        ended,
        init_uri,
        start_offset_ms,
        segments,
        unsupported,
    })
}

/// The `EXT-X-START` offset in ms; `None` for a negative (end-relative)
/// offset, which Pharos never writes.
fn parse_start(rest: &str, line: usize) -> Result<Option<u64>, ParseError> {
    let bad = |value: &str| ParseError::BadAttribute {
        tag: "EXT-X-START",
        line,
        value: value.to_string(),
    };
    let attrs = parse_attributes(rest, "EXT-X-START", line)?;
    let Some((_, v)) = attrs.into_iter().find(|(k, _)| *k == "TIME-OFFSET") else {
        return Err(bad(rest));
    };
    let secs = v.parse::<f64>().map_err(|_| bad(v))?;
    if !secs.is_finite() {
        return Err(bad(v));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = (secs * 1000.0).round() as u64;
    Ok((secs >= 0.0).then_some(ms))
}

fn parse_ext_x_media(
    rest: &str,
    line: usize,
    base_url: &str,
    master: &mut Master,
) -> Result<(), ParseError> {
    let attrs = parse_attributes(rest, "EXT-X-MEDIA", line)?;
    let mut media_type: Option<&str> = None;
    let mut group_id: Option<String> = None;
    let mut name: Option<String> = None;
    let mut uri_attr: Option<String> = None;
    let mut default = false;
    let mut autoselect = false;
    let mut language: Option<String> = None;

    for (k, v) in attrs {
        match k {
            "TYPE" => media_type = Some(v),
            "GROUP-ID" => group_id = Some(v.to_string()),
            "NAME" => name = Some(v.to_string()),
            "URI" => uri_attr = Some(uri::resolve(base_url, v)),
            "DEFAULT" => default = parse_yes_no(v, "EXT-X-MEDIA", line)?,
            "AUTOSELECT" => autoselect = parse_yes_no(v, "EXT-X-MEDIA", line)?,
            "LANGUAGE" => language = Some(v.to_string()),
            _ => {}
        }
    }

    match media_type {
        Some("AUDIO") => {
            let (Some(group_id), Some(name)) = (group_id, name) else {
                return Err(ParseError::BadAttribute {
                    tag: "EXT-X-MEDIA",
                    line,
                    value: rest.to_string(),
                });
            };
            master.audio.push(Rendition {
                group_id,
                name,
                uri: uri_attr,
                default,
                autoselect,
                language,
            });
            Ok(())
        }
        Some("SUBTITLES") => {
            push_unsupported(&mut master.unsupported, Unsupported::MediaTypeSubtitles);
            Ok(())
        }
        Some("CLOSED-CAPTIONS") => {
            push_unsupported(&mut master.unsupported, Unsupported::ClosedCaptions);
            Ok(())
        }
        Some(_) => Ok(()),
        None => Err(ParseError::BadAttribute {
            tag: "EXT-X-MEDIA",
            line,
            value: rest.to_string(),
        }),
    }
}

fn parse_stream_inf(rest: &str, line: usize) -> Result<PendingVariant, ParseError> {
    let attrs = parse_attributes(rest, "EXT-X-STREAM-INF", line)?;
    let mut bandwidth: Option<u64> = None;
    let mut codecs: Vec<String> = Vec::new();
    let mut resolution: Option<(u32, u32)> = None;
    let mut audio_group: Option<String> = None;

    for (k, v) in attrs {
        match k {
            "BANDWIDTH" => {
                let bw = v.parse::<u64>().map_err(|_| ParseError::BadAttribute {
                    tag: "EXT-X-STREAM-INF",
                    line,
                    value: v.to_string(),
                })?;
                bandwidth = Some(bw);
            }
            "CODECS" => {
                codecs = v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
                    .collect();
            }
            "RESOLUTION" => {
                let Some((w_str, h_str)) = v.split_once(['x', 'X']) else {
                    return Err(ParseError::BadAttribute {
                        tag: "EXT-X-STREAM-INF",
                        line,
                        value: v.to_string(),
                    });
                };
                let w = w_str.parse::<u32>().map_err(|_| ParseError::BadAttribute {
                    tag: "EXT-X-STREAM-INF",
                    line,
                    value: v.to_string(),
                })?;
                let h = h_str.parse::<u32>().map_err(|_| ParseError::BadAttribute {
                    tag: "EXT-X-STREAM-INF",
                    line,
                    value: v.to_string(),
                })?;
                resolution = Some((w, h));
            }
            "AUDIO" => audio_group = Some(v.to_string()),
            _ => {}
        }
    }

    let Some(bandwidth) = bandwidth else {
        return Err(ParseError::BadAttribute {
            tag: "EXT-X-STREAM-INF",
            line,
            value: rest.to_string(),
        });
    };

    Ok(PendingVariant {
        bandwidth,
        codecs,
        resolution,
        audio_group,
    })
}

fn parse_ext_x_key(
    rest: &str,
    line: usize,
    unsupported: &mut Vec<Unsupported>,
) -> Result<(), ParseError> {
    let attrs = parse_attributes(rest, "EXT-X-KEY", line)?;
    let mut method: Option<&str> = None;
    for (k, v) in attrs {
        if k == "METHOD" {
            method = Some(v);
        }
    }
    let Some(method) = method else {
        return Err(ParseError::BadAttribute {
            tag: "EXT-X-KEY",
            line,
            value: rest.to_string(),
        });
    };
    if method != "NONE" {
        push_unsupported(unsupported, Unsupported::Encrypted);
    }
    Ok(())
}

fn parse_yes_no(val: &str, tag: &'static str, line: usize) -> Result<bool, ParseError> {
    match val {
        "YES" => Ok(true),
        "NO" => Ok(false),
        _ => Err(ParseError::BadAttribute {
            tag,
            line,
            value: val.to_string(),
        }),
    }
}

fn parse_attributes<'a>(
    input: &'a str,
    tag: &'static str,
    line: usize,
) -> Result<Vec<(&'a str, &'a str)>, ParseError> {
    let mut rest = input.trim();
    if rest.is_empty() {
        return Err(ParseError::BadAttribute {
            tag,
            line,
            value: input.to_string(),
        });
    }
    let mut out = Vec::new();
    while !rest.is_empty() {
        let Some((raw_key, after_eq)) = rest.split_once('=') else {
            return Err(ParseError::BadAttribute {
                tag,
                line,
                value: rest.to_string(),
            });
        };
        let key = raw_key.trim();
        if key.is_empty() {
            return Err(ParseError::BadAttribute {
                tag,
                line,
                value: rest.to_string(),
            });
        }
        let after_eq = after_eq.trim_start();
        if let Some(after_open) = after_eq.strip_prefix('"') {
            let Some((val, after_close)) = after_open.split_once('"') else {
                return Err(ParseError::BadAttribute {
                    tag,
                    line,
                    value: rest.to_string(),
                });
            };
            let after_close = after_close.trim_start();
            rest = match after_close.strip_prefix(',') {
                Some(r) => r.trim_start(),
                None if after_close.is_empty() => "",
                None => {
                    return Err(ParseError::BadAttribute {
                        tag,
                        line,
                        value: after_close.to_string(),
                    });
                }
            };
            out.push((key, val));
        } else {
            let (val, next_rest) = match after_eq.split_once(',') {
                Some((v, r)) => (v.trim(), r.trim_start()),
                None => (after_eq.trim(), ""),
            };
            if val.is_empty() {
                return Err(ParseError::BadAttribute {
                    tag,
                    line,
                    value: rest.to_string(),
                });
            }
            rest = next_rest;
            out.push((key, val));
        }
    }
    Ok(out)
}

fn push_unsupported(list: &mut Vec<Unsupported>, item: Unsupported) {
    if !list.contains(&item) {
        list.push(item);
    }
}

#[cfg(test)]
mod tests {
    // Panicking on unexpected parse results is the intended failure mode in unit tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeSet;

    use super::{
        parse_master, parse_media, Master, MediaPlaylist, ParseError, Rendition, Segment,
        Unsupported, Variant,
    };

    #[test]
    fn unsupported_labels_are_distinct() {
        let all = [
            Unsupported::Encrypted,
            Unsupported::ByteRange,
            Unsupported::Discontinuity,
            Unsupported::Live,
            Unsupported::IFramesOnly,
            Unsupported::MediaTypeSubtitles,
            Unsupported::ClosedCaptions,
        ];
        let labels: BTreeSet<&'static str> = all.into_iter().map(Unsupported::label).collect();
        assert_eq!(labels.len(), all.len());
        assert_eq!(Unsupported::Encrypted.label(), "encrypted");
        assert_eq!(Unsupported::ByteRange.label(), "byte_range");
        assert_eq!(Unsupported::Discontinuity.label(), "discontinuity");
        assert_eq!(Unsupported::Live.label(), "live");
        assert_eq!(Unsupported::IFramesOnly.label(), "iframes_only");
        assert_eq!(Unsupported::MediaTypeSubtitles.label(), "subtitles");
        assert_eq!(Unsupported::ClosedCaptions.label(), "closed_captions");
    }

    #[test]
    fn parses_pharos_h264_cmaf_browser_master() {
        let base = "http://pharos.local/videos/42/master.m3u8?PlaySessionId=p1&api_key=k";
        let qs = "PlaySessionId=p1&api_key=k";
        let body = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio\",DEFAULT=YES,\
             AUTOSELECT=YES,URI=\"/videos/42/vp9/audio.m3u8?{qs}\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=8128000,CODECS=\"avc1.640028,opus\",AUDIO=\"aud\",RESOLUTION=1920x1080\n\
             /videos/42/h264cmaf/main.m3u8?{qs}\n"
        );
        let parsed = parse_master(&body, base).unwrap();
        assert_eq!(
            parsed,
            Master {
                version: Some(7),
                independent_segments: true,
                variants: vec![Variant {
                    uri: format!("http://pharos.local/videos/42/h264cmaf/main.m3u8?{qs}"),
                    bandwidth: 8_128_000,
                    codecs: vec!["avc1.640028".to_string(), "opus".to_string()],
                    resolution: Some((1920, 1080)),
                    audio_group: Some("aud".to_string()),
                }],
                audio: vec![Rendition {
                    group_id: "aud".to_string(),
                    name: "Audio".to_string(),
                    uri: Some(format!("http://pharos.local/videos/42/vp9/audio.m3u8?{qs}")),
                    default: true,
                    autoselect: true,
                    language: None,
                }],
                unsupported: Vec::new(),
            }
        );
    }

    #[test]
    fn parses_pharos_vp9_vod_media_playlist() {
        let base = "http://pharos.local/videos/42/vp9/main.m3u8?PlaySessionId=p1&api_key=k";
        let qs = "PlaySessionId=p1&api_key=k";
        let body = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXT-X-TARGETDURATION:6\n\
             #EXT-X-MAP:URI=\"/videos/42/vp9/init.mp4?{qs}&e=3\"\n\
             #EXT-X-START:TIME-OFFSET=12.012,PRECISE=YES\n\
             #EXT-X-MEDIA-SEQUENCE:0\n\
             #EXTINF:6.006,\n\
             /videos/42/vp9/0.m4s?{qs}&e=3\n\
             #EXTINF:6.006,\n\
             /videos/42/vp9/1.m4s?{qs}&e=3\n\
             #EXTINF:2.503,\n\
             /videos/42/vp9/2.m4s?{qs}&e=3\n\
             #EXT-X-ENDLIST\n"
        );
        let parsed = parse_media(&body, base).unwrap();
        assert_eq!(
            parsed,
            MediaPlaylist {
                version: Some(7),
                target_duration_s: 6,
                media_sequence: 0,
                vod: true,
                ended: true,
                init_uri: Some(format!(
                    "http://pharos.local/videos/42/vp9/init.mp4?{qs}&e=3"
                )),
                start_offset_ms: Some(12_012),
                segments: vec![
                    Segment {
                        uri: format!("http://pharos.local/videos/42/vp9/0.m4s?{qs}&e=3"),
                        start_ms: 0,
                        duration_ms: 6_006,
                    },
                    Segment {
                        uri: format!("http://pharos.local/videos/42/vp9/1.m4s?{qs}&e=3"),
                        start_ms: 6_006,
                        duration_ms: 6_006,
                    },
                    Segment {
                        uri: format!("http://pharos.local/videos/42/vp9/2.m4s?{qs}&e=3"),
                        start_ms: 12_012,
                        duration_ms: 2_503,
                    },
                ],
                unsupported: Vec::new(),
            }
        );
    }

    #[test]
    fn parses_muxed_mpegts_audio_only_master() {
        let base = "http://pharos.local/Videos/42/master.m3u8?PlaySessionId=p1&api_key=k";
        let qs = "PlaySessionId=p1&api_key=k";
        let body = format!(
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXT-X-STREAM-INF:BANDWIDTH=628000,CODECS=\"mp4a.40.2\"\n\
             /Videos/42/main.m3u8?{qs}\n\
             #EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\n\
             /Videos/42/variants/128k.m3u8?{qs}\n"
        );
        let parsed = parse_master(&body, base).unwrap();
        assert_eq!(
            parsed,
            Master {
                version: Some(3),
                independent_segments: true,
                variants: vec![
                    Variant {
                        uri: format!("http://pharos.local/Videos/42/main.m3u8?{qs}"),
                        bandwidth: 628_000,
                        codecs: vec!["mp4a.40.2".to_string()],
                        resolution: None,
                        audio_group: None,
                    },
                    Variant {
                        uri: format!("http://pharos.local/Videos/42/variants/128k.m3u8?{qs}"),
                        bandwidth: 128_000,
                        codecs: vec!["mp4a.40.2".to_string()],
                        resolution: None,
                        audio_group: None,
                    },
                ],
                audio: Vec::new(),
                unsupported: Vec::new(),
            }
        );
    }

    #[test]
    fn detects_each_unsupported_feature_at_most_once() {
        let master_body = "\
            #EXTM3U\n\
            #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=86000,URI=\"iframes.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",URI=\"en.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"French\",URI=\"fr.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",NAME=\"CC1\",INSTREAM-ID=\"CC1\"\n";
        let master = parse_master(master_body, "http://pharos.local/master.m3u8").unwrap();
        assert_eq!(
            master.unsupported,
            vec![Unsupported::MediaTypeSubtitles, Unsupported::ClosedCaptions,]
        );

        let media_body = "\
            #EXTM3U\n\
            #EXT-X-TARGETDURATION:6\n\
            #EXT-X-PLAYLIST-TYPE:EVENT\n\
            #EXT-X-I-FRAMES-ONLY\n\
            #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
            #EXT-X-KEY:METHOD=NONE\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"https://example.com/key\"\n\
            #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"https://example.com/key2\"\n\
            #EXT-X-BYTERANGE:1024@720\n\
            #EXTINF:6.000,\n\
            seg0.m4s\n\
            #EXT-X-DISCONTINUITY\n\
            #EXT-X-DISCONTINUITY\n\
            #EXTINF:6.000,\n\
            seg1.m4s\n";
        let media = parse_media(media_body, "http://pharos.local/videos/1/main.m3u8").unwrap();
        assert!(!media.vod);
        assert!(!media.ended);
        assert_eq!(
            media.unsupported,
            vec![
                Unsupported::IFramesOnly,
                Unsupported::ByteRange,
                Unsupported::Encrypted,
                Unsupported::Discontinuity,
                Unsupported::Live,
            ]
        );
    }

    #[test]
    fn handles_bom_crlf_comments_unknown_tags_and_commas_in_quotes() {
        let body = "\u{feff}\r\n\
            #EXTM3U\r\n\
            # plain comment\r\n\
            #EXT-X-UNKNOWN-CUSTOM:FOO=1\r\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Japanese, 5.1 Surround\",\
            LANGUAGE=\"jpn\",DEFAULT=NO,AUTOSELECT=YES,URI=\"audio.m3u8\"\r\n\
            #EXT-X-STREAM-INF:BANDWIDTH=4000000,CODECS=\"vp09.00.41.08, opus\",AUDIO=\"aud\"\r\n\
            main.m3u8\r\n";
        let master = parse_master(body, "http://pharos.local/videos/9/master.m3u8").unwrap();
        assert_eq!(master.audio.len(), 1);
        assert_eq!(master.audio[0].name, "Japanese, 5.1 Surround");
        assert_eq!(master.audio[0].language.as_deref(), Some("jpn"));
        assert!(!master.audio[0].default);
        assert!(master.audio[0].autoselect);
        assert_eq!(
            master.variants[0].codecs,
            vec!["vp09.00.41.08".to_string(), "opus".to_string()]
        );
    }

    #[test]
    fn rejects_invalid_playlists_with_descriptive_errors() {
        assert_eq!(
            parse_master("#EXT-X-VERSION:7\n", "http://pharos.local/m.m3u8"),
            Err(ParseError::NotM3u8)
        );
        assert_eq!(
            parse_media(
                "#EXTM3U\n#EXTINF:6.0,\n0.m4s\n#EXT-X-ENDLIST\n",
                "http://pharos.local/v.m3u8"
            ),
            Err(ParseError::MissingTargetDuration)
        );
        assert_eq!(
            parse_media(
                "#EXTM3U\n#EXT-X-TARGETDURATION:6\n0.m4s\n",
                "http://pharos.local/v.m3u8"
            ),
            Err(ParseError::UriWithoutTag { line: 3 })
        );
        assert_eq!(
            parse_master(
                "#EXTM3U\n/videos/1/main.m3u8\n",
                "http://pharos.local/m.m3u8"
            ),
            Err(ParseError::UriWithoutTag { line: 2 })
        );
        assert_eq!(
            parse_master(
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\n",
                "http://pharos.local/m.m3u8"
            ),
            Err(ParseError::TagWithoutUri {
                tag: "EXT-X-STREAM-INF",
                line: 2
            })
        );
        assert_eq!(
            parse_media(
                "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n#EXT-X-ENDLIST\n",
                "http://pharos.local/v.m3u8"
            ),
            Err(ParseError::TagWithoutUri {
                tag: "EXTINF",
                line: 3
            })
        );

        let bad_extinf = parse_media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:-2.5,\n0.m4s\n#EXT-X-ENDLIST\n",
            "http://pharos.local/v.m3u8",
        )
        .unwrap_err();
        assert_eq!(
            bad_extinf,
            ParseError::BadDuration {
                line: 3,
                value: "-2.5".to_string()
            }
        );
        let msg = bad_extinf.to_string();
        assert!(msg.contains('3'), "message did not name line: {msg}");
        assert!(msg.contains("-2.5"), "message did not name value: {msg}");
    }

    #[test]
    fn video_renditions_are_ignored_and_negative_start_is_dropped() {
        let master = parse_master(
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID=\"v\",NAME=\"x\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n",
            "http://h/m.m3u8",
        )
        .unwrap();
        assert_eq!(master.variants.len(), 1);
        assert!(master.unsupported.is_empty());
        let media = parse_media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-START:TIME-OFFSET=-10\n#EXTINF:6,\na.m4s\n#EXT-X-ENDLIST\n",
            "http://h/m.m3u8",
        )
        .unwrap();
        assert_eq!(media.start_offset_ms, None);
    }

    #[test]
    fn a_vod_playlist_without_endlist_is_not_live() {
        let media = parse_media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:6,\na.m4s\n",
            "http://h/m.m3u8",
        )
        .unwrap();
        assert!(!media.unsupported.contains(&Unsupported::Live));
    }
}
