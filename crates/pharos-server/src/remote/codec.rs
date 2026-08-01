//! Turning a resolver's codec string back into the fields a playlist needs.
//!
//! `yt-dlp -j` reports a video codec the way a browser does — `"avc1.640028"` —
//! because that is what `MediaSource.isTypeSupported` takes. [`MediaProbe`]
//! wants the same information taken apart: `video_codec` (`"h264"`),
//! `video_profile` (`"High"`) and `video_level` (`40`), because those three are
//! what `codecs_attr` reassembles into the RFC 6381 CODECS attribute on the HLS
//! master playlist.
//!
//! Skipping this and storing `"avc1.640028"` in `video_codec` is not a cosmetic
//! loss. The master playlist would advertise a CODECS attribute built from a
//! codec name that is already a CODECS attribute, Safari would fail to match any
//! variant, and the failure looks like "Safari won't play it" rather than
//! anything to do with metadata.
//!
//! [`MediaProbe`]: pharos_core::MediaProbe

/// What a resolver's codec string says, taken apart.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodecParts {
    /// Codec family in pharos's own vocabulary (`h264`, `hevc`, `vp9`, `av1`),
    /// matching what the local prober writes so downstream comparisons work.
    pub codec: Option<String>,
    /// Human profile name (`High`, `Main`, `Baseline`, `Main 10`).
    pub profile: Option<String>,
    /// Level × 10, the same scale ffprobe reports and `MediaProbe` stores —
    /// level 4.0 is `40`.
    pub level: Option<u32>,
}

/// Parse a browser-style codec string.
///
/// Unrecognised input yields the string as the codec family with no profile or
/// level, rather than an error. A codec pharos does not know how to take apart
/// is still a codec, and refusing the item over it would be a worse outcome than
/// a slightly thinner CODECS attribute.
pub fn parse_video_codec(s: &str) -> CodecParts {
    let s = s.trim();
    if s.is_empty() || s == "none" {
        return CodecParts::default();
    }
    let mut it = s.split('.');
    let family = it.next().unwrap_or("");
    match family {
        // avc1.PPCCLL — profile_idc, constraint flags, level_idc, each one
        // hex byte. This is the only form YouTube emits for H.264.
        "avc1" | "avc3" => {
            let rest = it.next().unwrap_or("");
            let (profile, level) = if rest.len() >= 6 {
                let p = u8::from_str_radix(&rest[0..2], 16).ok();
                let l = u8::from_str_radix(&rest[4..6], 16).ok();
                (p.and_then(h264_profile), l.map(u32::from))
            } else {
                (None, None)
            };
            CodecParts {
                codec: Some("h264".into()),
                profile,
                level,
            }
        }
        // hev1/hvc1.P.C.TLL.CC — the level sits in the fourth field prefixed
        // by tier ("L120" / "H120") and is already level × 30.
        "hev1" | "hvc1" => {
            let profile = it.next().and_then(hevc_profile);
            let _compat = it.next();
            let level = it
                .next()
                .and_then(|t| t.trim_start_matches(['L', 'H']).parse::<u32>().ok())
                // HEVC general_level_idc is level × 30; MediaProbe stores × 10.
                .map(|v| v / 3);
            CodecParts {
                codec: Some("hevc".into()),
                profile,
                level,
            }
        }
        "vp09" | "vp9" => CodecParts {
            codec: Some("vp9".into()),
            ..Default::default()
        },
        "av01" | "av1" => CodecParts {
            codec: Some("av1".into()),
            ..Default::default()
        },
        other => CodecParts {
            codec: Some(other.to_ascii_lowercase()),
            ..Default::default()
        },
    }
}

/// H.264 `profile_idc` → the name ffprobe would have reported.
fn h264_profile(idc: u8) -> Option<String> {
    Some(
        match idc {
            66 => "Baseline",
            77 => "Main",
            88 => "Extended",
            100 => "High",
            110 => "High 10",
            122 => "High 4:2:2",
            244 => "High 4:4:4 Predictive",
            _ => return None,
        }
        .to_string(),
    )
}

/// HEVC profile field (`1` / `2` / `4`, sometimes with a leading `A`/`B`/`C`).
fn hevc_profile(f: &str) -> Option<String> {
    let n: u32 = f.trim_start_matches(['A', 'B', 'C']).parse().ok()?;
    Some(
        match n {
            1 => "Main",
            2 => "Main 10",
            3 => "Main Still Picture",
            4 => "Rext",
            _ => return None,
        }
        .to_string(),
    )
}

/// Normalise a resolver's audio codec to pharos's vocabulary.
pub fn parse_audio_codec(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s == "none" {
        return None;
    }
    let family = s.split('.').next().unwrap_or(s);
    Some(
        match family {
            "mp4a" => "aac",
            "ec-3" => "eac3",
            "ac-3" => "ac3",
            other => other,
        }
        .to_ascii_lowercase(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The exact string YouTube reports for its most common H.264 rendition
    /// must come apart into the three fields the CODECS attribute is rebuilt
    /// from. Storing it whole would make the master playlist advertise a
    /// CODECS attribute built from a CODECS attribute, and Safari would match
    /// no variant at all.
    #[test]
    fn a_youtube_h264_codec_string_comes_apart_into_probe_fields() {
        let p = parse_video_codec("avc1.640028");
        assert_eq!(p.codec.as_deref(), Some("h264"));
        assert_eq!(p.profile.as_deref(), Some("High"));
        // 0x28 = 40 = level 4.0, on the same x10 scale MediaProbe stores.
        assert_eq!(p.level, Some(40));

        // Main profile at level 3.1, the other common rung.
        let p = parse_video_codec("avc1.4d401f");
        assert_eq!(p.profile.as_deref(), Some("Main"));
        assert_eq!(p.level, Some(31));
    }

    /// HEVC's level is on a DIFFERENT scale from H.264's — general_level_idc
    /// is level x 30, not x 10 — so passing it through unconverted would
    /// advertise level 12.0 for a level 4.0 stream.
    #[test]
    fn hevc_level_is_converted_off_its_own_scale() {
        let p = parse_video_codec("hvc1.1.6.L120.90");
        assert_eq!(p.codec.as_deref(), Some("hevc"));
        assert_eq!(p.profile.as_deref(), Some("Main"));
        assert_eq!(p.level, Some(40), "L120 is level 4.0, not level 12");

        let p = parse_video_codec("hev1.2.4.L153.b0");
        assert_eq!(p.profile.as_deref(), Some("Main 10"));
        assert_eq!(p.level, Some(51));
    }

    /// An unknown or truncated codec still yields a usable family rather than
    /// failing the item. A thinner CODECS attribute is a far better outcome
    /// than refusing to catalogue the video.
    #[test]
    fn an_unparseable_codec_degrades_instead_of_failing() {
        assert_eq!(
            parse_video_codec("vp09.00.10.08").codec.as_deref(),
            Some("vp9")
        );
        assert_eq!(
            parse_video_codec("av01.0.08M.08").codec.as_deref(),
            Some("av1")
        );
        // Truncated avc1: family survives, profile/level do not.
        let p = parse_video_codec("avc1.64");
        assert_eq!(p.codec.as_deref(), Some("h264"));
        assert_eq!(p.profile, None);
        assert_eq!(p.level, None);
        // Genuinely unknown.
        assert_eq!(parse_video_codec("weird").codec.as_deref(), Some("weird"));
        // Absent is absent, not a codec called "none".
        assert_eq!(parse_video_codec("none"), CodecParts::default());
        assert_eq!(parse_video_codec(""), CodecParts::default());
    }

    #[test]
    fn audio_codecs_normalise_to_the_probers_vocabulary() {
        assert_eq!(parse_audio_codec("mp4a.40.2").as_deref(), Some("aac"));
        assert_eq!(parse_audio_codec("opus").as_deref(), Some("opus"));
        assert_eq!(parse_audio_codec("ec-3").as_deref(), Some("eac3"));
        assert_eq!(parse_audio_codec("none"), None);
        assert_eq!(parse_audio_codec(""), None);
    }
}
