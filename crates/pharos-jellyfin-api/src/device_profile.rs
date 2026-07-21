//! Jellyfin `DeviceProfile` parsing + direct-play / transcode
//! negotiation (T41 phase 2).
//!
//! Clients POST a `DeviceProfile` body to `/Items/{id}/PlaybackInfo`
//! enumerating which containers + codecs they can natively decode and
//! the bitrate ceiling. pharos walks the profile against the source
//! media's probed streams and chooses one of:
//!
//! - `Decision::DirectPlay`: client streams the file as-is.
//! - `Decision::Transcode { … }`: pharos must transcode container /
//!   codec / bitrate.
//!
//! Phase-2 scope is intentionally narrow: container + per-stream codec
//! match, audio remux on codec-only mismatch, bitrate guardrail. Full
//! CodecProfile expression evaluation (`AudioChannels<=2`, etc.) lands
//! later — we approximate "respect bitrate cap" only.

use serde::Deserialize;

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct DeviceProfile {
    pub direct_play_profiles: Vec<DirectPlayProfile>,
    pub transcoding_profiles: Vec<TranscodingProfile>,
    pub max_streaming_bitrate: Option<u64>,
    pub max_static_bitrate: Option<u64>,
    /// P27 — clause-based codec restrictions, e.g.
    /// `{Codec:"h264", Conditions:[{Condition:"LessThanEqual",
    /// Property:"VideoLevel", Value:"41", IsRequired:true}]}`.
    /// Evaluated after the DirectPlay codec/container match — failed
    /// required conditions fall through to Transcode.
    #[serde(default)]
    pub codec_profiles: Vec<CodecProfileDto>,
    #[serde(default)]
    pub subtitle_profiles: Vec<SubtitleProfileDto>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct CodecProfileDto {
    /// Jellyfin spells `Video` / `Audio` / `VideoAudio`. Empty / unset
    /// = match any kind.
    #[serde(rename = "Type", default)]
    pub kind: String,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub conditions: Vec<ProfileCondition>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct SubtitleProfileDto {
    #[serde(default)]
    pub format: String,
    /// Jellyfin: "Encode" (burn) | "Embed" | "External" | "Hls".
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub language: String,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct ProfileCondition {
    /// Jellyfin op names: `LessThanEqual`, `GreaterThanEqual`,
    /// `Equals`, `NotEquals`, `EqualsAny`.
    #[serde(default)]
    pub condition: String,
    /// `VideoLevel`, `VideoProfile`, `VideoBitDepth`, `AudioChannels`,
    /// `Width`, `Height`, `AudioBitRate`.
    #[serde(default)]
    pub property: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub is_required: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct DirectPlayProfile {
    /// Comma-separated container list, e.g. `"mp4,m4v"`.
    #[serde(default)]
    pub container: String,
    #[serde(default)]
    pub video_codec: String,
    #[serde(default)]
    pub audio_codec: String,
    /// Jellyfin spells the discriminator `Video` / `Audio` / `Photo`.
    #[serde(rename = "Type", default)]
    pub kind: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct TranscodingProfile {
    #[serde(default)]
    pub container: String,
    #[serde(default)]
    pub video_codec: String,
    #[serde(default)]
    pub audio_codec: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(rename = "Type", default)]
    pub kind: String,
}

/// What pharos probed about the source file. Concise — only the fields
/// the negotiation actually inspects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceMedia {
    pub container: String,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub is_video: bool,
    /// P27 — extended source descriptors used by CodecProfile.Conditions
    /// evaluation. All optional; when missing, the comparator treats
    /// the condition permissively (no-op).
    pub video_level: Option<u32>,
    pub video_profile: Option<String>,
    pub audio_channels: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Luma bit depth (8 / 10 / 12), derived from the source `pix_fmt`.
    /// Drives the `VideoBitDepth` CodecProfile condition — an 8-bit-only
    /// decoder must NOT be handed a 10-bit HEVC/AV1 source for direct
    /// play (it decodes to garbage or hard-fails). `None` = unknown →
    /// the condition evaluates permissively.
    pub video_bit_depth: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Decision {
    DirectPlay,
    /// Audio remux: container + video codec match a DirectPlayProfile
    /// but the audio codec doesn't. The video stream copies; audio
    /// re-encodes to the negotiated codec.
    AudioRemux {
        target_audio_codec: String,
    },
    /// P9 — Video remux: video + audio codecs match a DirectPlayProfile
    /// but the container doesn't (e.g. MKV source against an MP4
    /// profile). Video bitstream copies; audio also copies when its
    /// codec matches AND the client accepts it, otherwise re-encodes
    /// to `target_audio_codec`. Container always swaps to the profile
    /// the client asked for.
    VideoRemux {
        target_container: String,
        /// `None` = audio codec matches profile and copies as well.
        target_audio_codec: Option<String>,
    },
    /// Full transcode: container, video, or video bitrate exceeds the
    /// profile cap. All three fields populated from the matched
    /// TranscodingProfile (or sensible defaults if the client supplied
    /// none).
    Transcode {
        target_container: String,
        target_video_codec: Option<String>,
        target_audio_codec: Option<String>,
        max_video_bitrate_bps: Option<u64>,
    },
}

impl Decision {
    pub fn is_direct(&self) -> bool {
        matches!(self, Decision::DirectPlay)
    }

    /// Lower a transcode's video-bitrate ceiling to `ceiling_bps`, taking the
    /// min with any cap already negotiated. A `None` existing cap (the client
    /// sent no MaxStreamingBitrate, or "Auto") becomes `ceiling_bps`.
    ///
    /// Used to apply a connection-aware default (see the server's
    /// `remote_default_bitrate_bps`): a remote client on "Auto" quality
    /// advertises an effectively-unlimited MaxStreamingBitrate, so an uncapped
    /// transcode targets the source/encoder ceiling — unplayable over a home
    /// uplink. This only ever LOWERS the ceiling and is a no-op for non-
    /// transcode decisions (DirectPlay / remux carry no encoder bitrate).
    #[must_use]
    pub fn clamp_video_bitrate(self, ceiling_bps: u64) -> Self {
        match self {
            Decision::Transcode {
                target_container,
                target_video_codec,
                target_audio_codec,
                max_video_bitrate_bps,
            } => Decision::Transcode {
                target_container,
                target_video_codec,
                target_audio_codec,
                max_video_bitrate_bps: Some(
                    max_video_bitrate_bps.map_or(ceiling_bps, |cap| cap.min(ceiling_bps)),
                ),
            },
            other => other,
        }
    }
}

/// P27 — evaluate a CodecProfile's required conditions against the
/// source. Returns `true` when every required condition passes; an
/// empty or non-matching profile is permissive.
pub fn codec_profile_passes(
    profiles: &[CodecProfileDto],
    source: &SourceMedia,
    source_video_level: Option<u32>,
    source_video_profile: Option<&str>,
    source_audio_channels: Option<u32>,
) -> bool {
    for cp in profiles {
        // Scope the profile by kind. "Audio" profiles never apply to a
        // video source's video stream, and "Video" profiles never apply
        // to an audio source. "VideoAudio"/empty apply to both.
        let is_audio_profile = cp.kind.eq_ignore_ascii_case("Audio");
        let is_video_profile = cp.kind.eq_ignore_ascii_case("Video");
        if source.is_video && is_audio_profile {
            continue;
        }
        if !source.is_video && is_video_profile {
            continue;
        }
        if !cp.codec.is_empty() {
            let want = cp.codec.to_ascii_lowercase();
            // Compare against the stream the profile is about: audio
            // codec for an Audio profile (or an audio source), else the
            // video codec. The previous code always compared the video
            // codec, so Audio CodecProfile restrictions (e.g. a 2-channel
            // AudioChannels cap) were silently dropped — too-permissive.
            let compare_audio = is_audio_profile || !source.is_video;
            let have = if compare_audio {
                source.audio_codec.as_deref().unwrap_or("")
            } else {
                source.video_codec.as_deref().unwrap_or("")
            }
            .to_ascii_lowercase();
            // Codec may be CSV.
            let mut codec_matches = false;
            for token in want.split(',') {
                if token.trim() == have {
                    codec_matches = true;
                    break;
                }
            }
            if !codec_matches {
                continue;
            }
        }
        for cond in &cp.conditions {
            if !cond.is_required {
                continue;
            }
            let ok = match cond.property.as_str() {
                "VideoLevel" => compare_numeric(
                    &cond.condition,
                    source_video_level.map(|n| n as i64),
                    &cond.value,
                ),
                "AudioChannels" => compare_numeric(
                    &cond.condition,
                    source_audio_channels.map(|n| n as i64),
                    &cond.value,
                ),
                "Width" => {
                    compare_numeric(&cond.condition, source.width.map(|n| n as i64), &cond.value)
                }
                "Height" => compare_numeric(
                    &cond.condition,
                    source.height.map(|n| n as i64),
                    &cond.value,
                ),
                "VideoProfile" => compare_string(
                    &cond.condition,
                    source_video_profile,
                    Some(cond.value.as_str()),
                ),
                "VideoBitDepth" => compare_numeric(
                    &cond.condition,
                    source.video_bit_depth.map(|n| n as i64),
                    &cond.value,
                ),
                _ => true, // unknown property — permissive
            };
            if !ok {
                return false;
            }
        }
    }
    true
}

/// Derive luma bit depth from an ffprobe `pix_fmt` token. ffmpeg encodes
/// depth as a `NN` suffix before the endianness marker — `yuv420p10le` → 10,
/// `yuv444p12le` → 12, `p010le` → 10 — while plain 8-bit formats (`yuv420p`,
/// `nv12`, `rgb24`) carry no suffix → 8. Unknown/empty → `None` (permissive).
pub fn bit_depth_from_pix_fmt(pix_fmt: Option<&str>) -> Option<u32> {
    let f = pix_fmt?.to_ascii_lowercase();
    if f.is_empty() {
        return None;
    }
    // Every >8-bit ffmpeg pix_fmt carries an explicit endianness suffix
    // (`yuv420p10le`, `p010le`, `yuv444p12be`) — 8-bit formats never do. So a
    // depth token is real ONLY when immediately followed by `le`/`be`. This
    // deliberately does NOT match trailing digits in a format NAME
    // (`nv12`, `nv21`, `nv16` are all 8-bit).
    for depth in ["16", "14", "12", "10", "9"] {
        if f.contains(&format!("{depth}le")) || f.contains(&format!("{depth}be")) {
            return depth.parse().ok();
        }
    }
    Some(8)
}

fn compare_numeric(op: &str, source: Option<i64>, raw_target: &str) -> bool {
    let Some(s) = source else {
        return true; // missing source value — permissive
    };
    // Jellyfin delimits `EqualsAny` lists with the pipe `|`, e.g.
    // Value="2|6". Membership over the pipe-separated tokens.
    if op == "EqualsAny" {
        return raw_target
            .split('|')
            .filter_map(|t| t.trim().parse::<i64>().ok())
            .any(|t| s == t);
    }
    let Some(t) = raw_target.trim().parse::<i64>().ok() else {
        return true; // unparseable single target — permissive
    };
    match op {
        "LessThanEqual" => s <= t,
        "GreaterThanEqual" => s >= t,
        "Equals" => s == t,
        "NotEquals" => s != t,
        _ => true,
    }
}

fn compare_string(op: &str, source: Option<&str>, target: Option<&str>) -> bool {
    let (Some(s), Some(t)) = (source, target) else {
        return true;
    };
    match op {
        "Equals" => s.eq_ignore_ascii_case(t),
        "NotEquals" => !s.eq_ignore_ascii_case(t),
        // Jellyfin delimits EqualsAny with '|' (e.g. "high|main|baseline").
        "EqualsAny" => t.split('|').any(|opt| opt.trim().eq_ignore_ascii_case(s)),
        _ => true,
    }
}

/// Pick the right action given the source + a client profile. Caller
/// is expected to use `DeviceProfile::default()` when the client
/// didn't send a body (matches Jellyfin's permissive default).
pub fn negotiate(
    profile: &DeviceProfile,
    source: &SourceMedia,
    server: &ServerCodecSupport,
) -> Decision {
    let want_kind = if source.is_video { "Video" } else { "Audio" };

    let bitrate_cap = profile.max_streaming_bitrate.or(profile.max_static_bitrate);
    let over_bitrate = matches!(
        (bitrate_cap, source.bitrate_bps),
        (Some(cap), Some(have)) if have > cap
    );

    // Look for an exact direct-play match first.
    let mut audio_remux_candidate: Option<&DirectPlayProfile> = None;
    for p in &profile.direct_play_profiles {
        if !p.kind.is_empty() && !p.kind.eq_ignore_ascii_case(want_kind) {
            continue;
        }
        if !matches_csv(&p.container, &source.container) {
            continue;
        }
        let video_ok = matches_codec(&p.video_codec, source.video_codec.as_deref());
        let audio_ok = matches_codec(&p.audio_codec, source.audio_codec.as_deref());
        if video_ok && audio_ok && !over_bitrate {
            // P27 — clause-based codec restrictions. When the profile
            // pins e.g. VideoLevel ≤ 41 and the source is Level 51,
            // fall through to Transcode even though container + codec
            // match. Conditions that pharos doesn't probe (e.g. AV1
            // tier) are permissive.
            if codec_profile_passes(
                &profile.codec_profiles,
                source,
                source.video_level,
                source.video_profile.as_deref(),
                source.audio_channels,
            ) {
                return Decision::DirectPlay;
            }
        }
        // Video matches but audio doesn't → audio-remux is viable
        // (container + video codec stay).
        if video_ok && !audio_ok && audio_remux_candidate.is_none() && !over_bitrate {
            audio_remux_candidate = Some(p);
        }
    }
    if let Some(_p) = audio_remux_candidate {
        // Pick the first sensible target codec the client *can* play.
        // For now AAC is the de-facto Jellyfin lowest-common-denominator.
        return Decision::AudioRemux {
            target_audio_codec: "aac".into(),
        };
    }

    // P9 — Video remux: relax the container check. When the source's
    // video codec matches a DirectPlayProfile AND the profile's
    // container differs from the source's, remux container only.
    // Skip when video bitrate exceeds the cap — full transcode is
    // forced in that case.
    if !over_bitrate && source.is_video {
        for p in &profile.direct_play_profiles {
            if !p.kind.is_empty() && !p.kind.eq_ignore_ascii_case(want_kind) {
                continue;
            }
            // Container MUST differ here, otherwise the earlier loop
            // already returned DirectPlay / AudioRemux.
            if matches_csv(&p.container, &source.container) {
                continue;
            }
            let video_ok = matches_codec(&p.video_codec, source.video_codec.as_deref());
            if !video_ok {
                continue;
            }
            let target_container = pick_first_csv(&p.container)
                .unwrap_or_else(|| default_container(source.is_video).into());
            let audio_ok = matches_codec(&p.audio_codec, source.audio_codec.as_deref());
            return Decision::VideoRemux {
                target_container,
                target_audio_codec: if audio_ok { None } else { Some("aac".into()) },
            };
        }
    }

    // Fall through to TranscodingProfile.
    if let Some(tp) = profile
        .transcoding_profiles
        .iter()
        .find(|t| t.kind.is_empty() || t.kind.eq_ignore_ascii_case(want_kind))
    {
        return Decision::Transcode {
            target_container: pick_first_csv(&tp.container)
                .unwrap_or_else(|| default_container(source.is_video).into()),
            // Capability-aware: best codec in (client-decodable ∩
            // server-encodable), hardware-preferred, h264 tiebreak for B59
            // SyncPlay convergence. Audio keeps first-listed.
            target_video_codec: if source.is_video {
                pick_transcode_video_codec(&tp.video_codec, server)
            } else {
                pick_first_csv(&tp.video_codec)
            },
            target_audio_codec: pick_first_csv(&tp.audio_codec),
            max_video_bitrate_bps: bitrate_cap,
        };
    }

    // No profile supplied → permissive default: HLS + H264 + AAC for
    // video, mp3 for audio. Matches what jellyfin-web requests when its
    // built-in browser profile applies.
    Decision::Transcode {
        target_container: default_container(source.is_video).into(),
        // No client profile → pick the server's safe target (h264 if encodable).
        target_video_codec: source.is_video.then(|| server.fallback_target()),
        target_audio_codec: Some(if source.is_video {
            "aac".into()
        } else {
            "mp3".into()
        }),
        max_video_bitrate_bps: bitrate_cap,
    }
}

fn default_container(is_video: bool) -> &'static str {
    if is_video {
        "ts"
    } else {
        "mp3"
    }
}

fn matches_csv(csv: &str, candidate: &str) -> bool {
    if csv.is_empty() {
        return true;
    }
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|s| s.eq_ignore_ascii_case(candidate))
}

/// `candidate = None` means the source file has no stream of this kind
/// (e.g. a silent video). Treat that as a match — direct play still
/// works, the client just doesn't play any audio. Previously this
/// returned `false`, which sent silent WebM video down the transcode
/// path; with no TranscodingUrl populated the client surfaced "Playback
/// Error" instead of just playing the video silently.
fn matches_codec(csv: &str, candidate: Option<&str>) -> bool {
    if csv.is_empty() {
        return true;
    }
    let Some(c) = candidate else {
        return true;
    };
    matches_csv(csv, c)
}

fn pick_first_csv(csv: &str) -> Option<String> {
    csv.split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// What the SERVER can encode, as the negotiator sees it. A plain DTO in this
/// leaf crate (no dependency on `pharos-transcode`); the server converts its
/// `ServerEncodeCapabilities` into this. Codec names are canonical lowercase
/// (`h264`, `h265`, `vp9`, `av1`).
#[derive(Debug, Clone)]
pub struct CodecCap {
    pub name: String,
    /// Hardware-accelerated on this server.
    pub hw: bool,
    /// Rough encode cost, lower = cheaper (0 hw … 3 glacial software AV1).
    pub cost: u8,
}

/// The set of video codecs THIS server can encode. See [`negotiate`].
#[derive(Debug, Clone)]
pub struct ServerCodecSupport {
    pub encodable: Vec<CodecCap>,
}

impl Default for ServerCodecSupport {
    /// Permissive: assume software h264/h265/vp9/av1 (what a full ffmpeg build
    /// provides). Used by tests and as a safe fallback; production builds the
    /// real value from the server's probed capabilities.
    fn default() -> Self {
        Self {
            encodable: vec![
                CodecCap {
                    name: "h264".into(),
                    hw: false,
                    cost: 1,
                },
                CodecCap {
                    name: "h265".into(),
                    hw: false,
                    cost: 1,
                },
                CodecCap {
                    name: "vp9".into(),
                    hw: false,
                    cost: 2,
                },
                CodecCap {
                    name: "av1".into(),
                    hw: false,
                    cost: 3,
                },
            ],
        }
    }
}

impl ServerCodecSupport {
    fn find(&self, canon: &str) -> Option<&CodecCap> {
        self.encodable.iter().find(|c| c.name == canon)
    }

    /// The safe target when the client offers nothing this server can encode:
    /// h264 if available, else the cheapest encodable codec, else `h264`.
    fn fallback_target(&self) -> String {
        if self.find("h264").is_some() {
            return "h264".into();
        }
        self.encodable
            .iter()
            .min_by(|a, b| a.cost.cmp(&b.cost).then(a.name.cmp(&b.name)))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "h264".into())
    }
}

/// Canonicalise a client codec token to the negotiator's codec name, or `None`
/// for a codec pharos never targets.
fn canon_video_codec(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "h264" | "avc" | "avc1" => Some("h264"),
        "h265" | "hevc" | "hvc1" => Some("h265"),
        "vp9" | "vp09" => Some("vp9"),
        "av1" | "av01" => Some("av1"),
        _ => None,
    }
}

/// Pick the transcode VIDEO codec as the best codec in
/// *(client-decodable ∩ server-encodable)*, preferring HARDWARE, then lower
/// cost, with an H.264 tiebreak.
///
/// The h264 tiebreak keeps B59 SyncPlay convergence: a group whose browsers
/// list different first codecs (`vp9,h264` vs `h264,vp9`) still lands on ONE
/// shared cached encode, keyed by `(media, seg, audio, sub, bitrate, codec)`.
/// Replaces the old h264-hardcode: on a box with a hardware VP9 encoder and a
/// vp9-only client this now targets VP9 hardware instead of silently forcing a
/// software encode, and on a plain NVENC box a `vp9,h264` client targets the
/// hardware h264 rung instead of software VP9. Falls back to the server's safe
/// target when the client offers nothing this server can encode.
fn pick_transcode_video_codec(client_csv: &str, server: &ServerCodecSupport) -> Option<String> {
    let mut cands: Vec<(usize, &CodecCap)> = Vec::new();
    for (i, tok) in client_csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        if let Some(canon) = canon_video_codec(tok) {
            if let Some(cap) = server.find(canon) {
                if !cands.iter().any(|(_, c)| c.name == cap.name) {
                    cands.push((i, cap));
                }
            }
        }
    }
    if cands.is_empty() {
        return Some(server.fallback_target());
    }
    cands.sort_by(|(ai, a), (bi, b)| {
        // Hardware first, then cheaper, then h264 wins ties, then client order.
        b.hw.cmp(&a.hw)
            .then(a.cost.cmp(&b.cost))
            .then((b.name == "h264").cmp(&(a.name == "h264")))
            .then(ai.cmp(bi))
    });
    Some(cands[0].1.name.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn clamp_video_bitrate_lowers_and_fills_transcode_cap() {
        let tc = |cap: Option<u64>| Decision::Transcode {
            target_container: "ts".into(),
            target_video_codec: Some("h264".into()),
            target_audio_codec: Some("aac".into()),
            max_video_bitrate_bps: cap,
        };
        let cap_of = |d: Decision| match d {
            Decision::Transcode {
                max_video_bitrate_bps,
                ..
            } => max_video_bitrate_bps,
            _ => panic!("expected transcode"),
        };
        // No prior cap ("Auto") → ceiling fills it in.
        assert_eq!(
            cap_of(tc(None).clamp_video_bitrate(6_000_000)),
            Some(6_000_000)
        );
        // A higher client cap is lowered to the ceiling.
        assert_eq!(
            cap_of(tc(Some(140_000_000)).clamp_video_bitrate(6_000_000)),
            Some(6_000_000)
        );
        // An explicit LOWER client pick is honoured (never raised).
        assert_eq!(
            cap_of(tc(Some(2_000_000)).clamp_video_bitrate(6_000_000)),
            Some(2_000_000)
        );
        // Non-transcode decisions are untouched.
        assert_eq!(
            Decision::DirectPlay.clamp_video_bitrate(6_000_000),
            Decision::DirectPlay
        );
    }

    #[test]
    fn equals_any_string_splits_on_pipe() {
        // Jellyfin sends "high|main|baseline"; a "high" source must match.
        assert!(compare_string(
            "EqualsAny",
            Some("high"),
            Some("high|main|baseline")
        ));
        assert!(compare_string("EqualsAny", Some("MAIN"), Some("high|main")));
        assert!(!compare_string(
            "EqualsAny",
            Some("high10"),
            Some("high|main")
        ));
        // A comma value is a single token (not a delimiter) — no false match.
        assert!(!compare_string(
            "EqualsAny",
            Some("high"),
            Some("high,main")
        ));
    }

    #[test]
    fn equals_any_numeric_splits_on_pipe() {
        assert!(compare_numeric("EqualsAny", Some(6), "2|6"));
        assert!(!compare_numeric("EqualsAny", Some(8), "2|6"));
        // Single-value numeric ops still work via the raw string.
        assert!(compare_numeric("LessThanEqual", Some(2), "2"));
        assert!(!compare_numeric("GreaterThanEqual", Some(1), "2"));
        // Missing source → permissive.
        assert!(compare_numeric("EqualsAny", None, "2|6"));
    }

    fn dp(container: &str, video: &str, audio: &str, kind: &str) -> DirectPlayProfile {
        DirectPlayProfile {
            container: container.into(),
            video_codec: video.into(),
            audio_codec: audio.into(),
            kind: kind.into(),
        }
    }

    fn tp(container: &str, video: &str, audio: &str, kind: &str) -> TranscodingProfile {
        TranscodingProfile {
            container: container.into(),
            video_codec: video.into(),
            audio_codec: audio.into(),
            protocol: "hls".into(),
            kind: kind.into(),
        }
    }

    fn webm_vp9_opus_source() -> SourceMedia {
        SourceMedia {
            container: "webm".into(),
            video_codec: Some("vp9".into()),
            audio_codec: Some("opus".into()),
            bitrate_bps: Some(2_000_000),
            is_video: true,
            ..Default::default()
        }
    }

    #[test]
    fn empty_profile_falls_through_to_default_transcode() {
        let d = negotiate(
            &DeviceProfile::default(),
            &webm_vp9_opus_source(),
            &ServerCodecSupport::default(),
        );
        match d {
            Decision::Transcode {
                target_container,
                target_video_codec,
                target_audio_codec,
                ..
            } => {
                assert_eq!(target_container, "ts");
                assert_eq!(target_video_codec.as_deref(), Some("h264"));
                assert_eq!(target_audio_codec.as_deref(), Some("aac"));
            }
            other => panic!("expected Transcode, got {other:?}"),
        }
    }

    #[test]
    fn transcode_codec_hardware_then_cost_then_h264_tiebreak() {
        // Software-only box (permissive default): h264/h265 cheap, vp9 costlier,
        // av1 costliest.
        let sw = ServerCodecSupport::default();
        // h264 wins over vp9 (cheaper + B59 tiebreak), whatever the slot.
        assert_eq!(
            pick_transcode_video_codec("vp9,h264", &sw).as_deref(),
            Some("h264")
        );
        assert_eq!(
            pick_transcode_video_codec("h264,vp9", &sw).as_deref(),
            Some("h264")
        );
        // Canonical lowercase output, case-insensitive input.
        assert_eq!(
            pick_transcode_video_codec("VP9, H264", &sw).as_deref(),
            Some("h264")
        );
        // No h264: the CHEAPER software codec wins (vp9 < av1), not client-order
        // — software AV1 is glacial and would stall live playback.
        assert_eq!(
            pick_transcode_video_codec("av1,vp9", &sw).as_deref(),
            Some("vp9")
        );
        assert_eq!(
            pick_transcode_video_codec("vp9", &sw).as_deref(),
            Some("vp9")
        );
        // Client offers only a codec this server can't encode → safe h264 fallback.
        assert_eq!(
            pick_transcode_video_codec("theora", &sw).as_deref(),
            Some("h264")
        );
    }

    #[test]
    fn transcode_codec_is_capability_aware() {
        // A box with hardware VP9 but only software h264 targets VP9 for a
        // vp9,h264 client (hardware beats the h264 cost/tiebreak).
        let hw_vp9 = ServerCodecSupport {
            encodable: vec![
                CodecCap {
                    name: "h264".into(),
                    hw: false,
                    cost: 1,
                },
                CodecCap {
                    name: "vp9".into(),
                    hw: true,
                    cost: 0,
                },
            ],
        };
        assert_eq!(
            pick_transcode_video_codec("vp9,h264", &hw_vp9).as_deref(),
            Some("vp9")
        );
        // The NVENC + Firefox case: hardware h264 + software vp9 → h264, so
        // Firefox gets the fast hardware rung instead of a software VP9 encode.
        let hw_h264 = ServerCodecSupport {
            encodable: vec![
                CodecCap {
                    name: "h264".into(),
                    hw: true,
                    cost: 0,
                },
                CodecCap {
                    name: "vp9".into(),
                    hw: false,
                    cost: 2,
                },
            ],
        };
        assert_eq!(
            pick_transcode_video_codec("vp9,h264", &hw_h264).as_deref(),
            Some("h264")
        );
        // A vp9-only server can't give an h264-only client h264 → fallback to
        // the one thing it CAN encode.
        let vp9_only = ServerCodecSupport {
            encodable: vec![CodecCap {
                name: "vp9".into(),
                hw: true,
                cost: 0,
            }],
        };
        assert_eq!(
            pick_transcode_video_codec("h264", &vp9_only).as_deref(),
            Some("vp9")
        );
    }

    #[test]
    fn transcode_target_prefers_h264_when_client_lists_vp9_first() {
        // A browser that lists `vp9,h264` (jana's Windows Firefox) must still
        // transcode to h264 so it shares the group's warm h264 encode instead
        // of cold-transcoding its own VP9. (B59.)
        let source = SourceMedia {
            container: "mkv".into(),
            video_codec: Some("h264".into()),
            audio_codec: Some("aac".into()),
            bitrate_bps: Some(8_000_000),
            is_video: true,
            ..Default::default()
        };
        let profile = DeviceProfile {
            // No direct-play match (container mkv absent) → must transcode.
            transcoding_profiles: vec![tp("ts", "vp9,h264", "aac", "Video")],
            ..Default::default()
        };
        match negotiate(&profile, &source, &ServerCodecSupport::default()) {
            Decision::Transcode {
                target_video_codec, ..
            } => assert_eq!(target_video_codec.as_deref(), Some("h264")),
            other => panic!("expected Transcode, got {other:?}"),
        }
    }

    #[test]
    fn exact_codec_match_is_direct_play() {
        let profile = DeviceProfile {
            direct_play_profiles: vec![dp("webm", "vp9", "opus", "Video")],
            ..Default::default()
        };
        assert_eq!(
            negotiate(
                &profile,
                &webm_vp9_opus_source(),
                &ServerCodecSupport::default()
            ),
            Decision::DirectPlay,
        );
    }

    #[test]
    fn case_insensitive_container_match() {
        let profile = DeviceProfile {
            direct_play_profiles: vec![dp("WEBM,MKV", "vp9", "opus", "Video")],
            ..Default::default()
        };
        assert_eq!(
            negotiate(
                &profile,
                &webm_vp9_opus_source(),
                &ServerCodecSupport::default()
            ),
            Decision::DirectPlay,
        );
    }

    #[test]
    fn audio_codec_mismatch_yields_audio_remux() {
        let profile = DeviceProfile {
            direct_play_profiles: vec![dp("webm", "vp9", "aac", "Video")],
            transcoding_profiles: vec![tp("ts", "h264", "aac", "Video")],
            ..Default::default()
        };
        match negotiate(
            &profile,
            &webm_vp9_opus_source(),
            &ServerCodecSupport::default(),
        ) {
            Decision::AudioRemux { target_audio_codec } => assert_eq!(target_audio_codec, "aac"),
            other => panic!("expected AudioRemux, got {other:?}"),
        }
    }

    #[test]
    fn video_codec_mismatch_falls_through_to_transcode() {
        let profile = DeviceProfile {
            direct_play_profiles: vec![dp("webm", "h264", "opus", "Video")],
            transcoding_profiles: vec![tp("mp4", "h264", "aac", "Video")],
            ..Default::default()
        };
        match negotiate(
            &profile,
            &webm_vp9_opus_source(),
            &ServerCodecSupport::default(),
        ) {
            Decision::Transcode {
                target_container,
                target_video_codec,
                target_audio_codec,
                ..
            } => {
                assert_eq!(target_container, "mp4");
                assert_eq!(target_video_codec.as_deref(), Some("h264"));
                assert_eq!(target_audio_codec.as_deref(), Some("aac"));
            }
            other => panic!("expected Transcode, got {other:?}"),
        }
    }

    #[test]
    fn over_bitrate_disables_direct_play_even_on_exact_codec() {
        let profile = DeviceProfile {
            direct_play_profiles: vec![dp("webm", "vp9", "opus", "Video")],
            transcoding_profiles: vec![tp("ts", "h264", "aac", "Video")],
            max_streaming_bitrate: Some(500_000),
            ..Default::default()
        };
        let source = webm_vp9_opus_source();
        let d = negotiate(&profile, &source, &ServerCodecSupport::default());
        assert!(!d.is_direct(), "expected non-direct, got {d:?}");
        if let Decision::Transcode {
            max_video_bitrate_bps,
            ..
        } = d
        {
            assert_eq!(max_video_bitrate_bps, Some(500_000));
        }
    }

    #[test]
    fn audio_source_matches_audio_profile_only() {
        let source = SourceMedia {
            container: "mp3".into(),
            video_codec: None,
            audio_codec: Some("mp3".into()),
            bitrate_bps: Some(192_000),
            is_video: false,
            ..Default::default()
        };
        // Video profile present but disregarded; Audio profile is the
        // one that gets consulted.
        let profile = DeviceProfile {
            direct_play_profiles: vec![
                dp("webm", "vp9", "opus", "Video"),
                dp("mp3", "", "mp3", "Audio"),
            ],
            ..Default::default()
        };
        assert_eq!(
            negotiate(&profile, &source, &ServerCodecSupport::default()),
            Decision::DirectPlay
        );
    }

    #[test]
    fn silent_video_still_direct_plays() {
        // Some test fixtures (BBB WebM corpus, no audio track). Profile
        // demands an audio codec, but the file has none — direct play
        // should still succeed (browser will play silently) rather than
        // forcing a transcode whose TranscodingUrl is never wired up.
        let profile = DeviceProfile {
            direct_play_profiles: vec![dp("webm", "vp9", "vorbis,opus", "Video")],
            ..Default::default()
        };
        let source = SourceMedia {
            container: "webm".into(),
            video_codec: Some("vp9".into()),
            audio_codec: None,
            bitrate_bps: Some(2_000_000),
            is_video: true,
            ..Default::default()
        };
        assert_eq!(
            negotiate(&profile, &source, &ServerCodecSupport::default()),
            Decision::DirectPlay
        );
    }

    #[test]
    fn deserializes_typical_jellyfin_web_profile_subset() {
        let raw = r#"{
            "DirectPlayProfiles": [
                {"Container":"webm","VideoCodec":"vp9","AudioCodec":"opus","Type":"Video"}
            ],
            "TranscodingProfiles": [
                {"Container":"ts","VideoCodec":"h264","AudioCodec":"aac","Protocol":"hls","Type":"Video"}
            ],
            "MaxStreamingBitrate": 4000000
        }"#;
        let p: DeviceProfile = serde_json::from_str(raw).unwrap();
        assert_eq!(p.direct_play_profiles.len(), 1);
        assert_eq!(p.direct_play_profiles[0].video_codec, "vp9");
        assert_eq!(p.max_streaming_bitrate, Some(4_000_000));
    }

    #[test]
    fn parses_subtitle_profiles_from_pascalcase() {
        let json = r#"{"SubtitleProfiles":[
            {"Format":"ass","Method":"Encode"},
            {"Format":"subrip","Method":"External"}]}"#;
        let p: super::DeviceProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.subtitle_profiles.len(), 2);
        assert_eq!(p.subtitle_profiles[0].format, "ass");
        assert_eq!(p.subtitle_profiles[0].method, "Encode");
        assert_eq!(p.subtitle_profiles[1].method, "External");
    }

    // ---- bit-depth derivation (B75) ----

    #[test]
    fn bit_depth_from_pix_fmt_covers_common_formats() {
        let cases = [
            (Some("yuv420p"), Some(8)),
            (Some("yuvj420p"), Some(8)),
            (Some("nv12"), Some(8)),
            (Some("rgb24"), Some(8)),
            (Some("yuv420p10le"), Some(10)),
            (Some("yuv422p10le"), Some(10)),
            (Some("yuv444p10le"), Some(10)),
            (Some("p010le"), Some(10)),
            (Some("yuv420p12le"), Some(12)),
            (Some("yuv444p12be"), Some(12)),
            (Some(""), None),
            (None, None),
        ];
        for (pix, want) in cases {
            assert_eq!(bit_depth_from_pix_fmt(pix), want, "pix_fmt {pix:?}");
        }
    }

    // ---- device-category negotiation matrix (B75) ----
    //
    // The user's mandate: negotiate the CORRECT decision per DeviceProfile so
    // we never hand a device a stream it can't decode (→ crash). One case per
    // device class the deployment actually serves.

    /// A 10-bit HEVC 1080p source (the "Alien"-style remux that OOM-killed the
    /// TCL TV under the old force-transcode).
    fn hevc_10bit_source() -> SourceMedia {
        SourceMedia {
            container: "mkv".into(),
            video_codec: Some("hevc".into()),
            audio_codec: Some("aac".into()),
            bitrate_bps: Some(16_000_000),
            is_video: true,
            width: Some(1920),
            height: Some(800),
            video_bit_depth: Some(10),
            ..Default::default()
        }
    }

    fn hevc_8bit_source() -> SourceMedia {
        SourceMedia {
            video_bit_depth: Some(8),
            ..hevc_10bit_source()
        }
    }

    /// A modern TV that advertises HEVC (mkv) direct play, capped at 8-bit via
    /// a CodecProfile VideoBitDepth<=8 condition (an 8-bit-only decoder).
    fn hevc_8bit_only_tv_profile() -> DeviceProfile {
        serde_json::from_str(
            r#"{
              "DirectPlayProfiles":[
                {"Container":"mkv","Type":"Video","VideoCodec":"hevc","AudioCodec":"aac"}
              ],
              "CodecProfiles":[
                {"Type":"Video","Codec":"hevc","Conditions":[
                  {"Condition":"LessThanEqual","Property":"VideoBitDepth","Value":"8","IsRequired":true}
                ]}
              ],
              "TranscodingProfiles":[
                {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn hevc_capable_tv_direct_plays_8bit_source() {
        // TCL-style TV that CAN decode HEVC → direct play, no transcode.
        let profile = hevc_8bit_only_tv_profile();
        assert_eq!(
            negotiate(
                &profile,
                &hevc_8bit_source(),
                &ServerCodecSupport::default()
            ),
            Decision::DirectPlay,
            "8-bit HEVC on an HEVC-capable TV must direct-play"
        );
    }

    #[test]
    fn eight_bit_only_device_transcodes_10bit_hevc() {
        // THE crash-prevention case: a 10-bit source on an 8-bit-only decoder
        // must NOT direct-play (it would decode garbage / hard-fail). The
        // VideoBitDepth<=8 CodecProfile condition forces a transcode.
        let profile = hevc_8bit_only_tv_profile();
        match negotiate(
            &profile,
            &hevc_10bit_source(),
            &ServerCodecSupport::default(),
        ) {
            Decision::Transcode { .. } => {}
            other => panic!("10-bit on 8-bit device must transcode, got {other:?}"),
        }
    }

    #[test]
    fn h264_only_device_transcodes_hevc() {
        // A device that only lists h264 direct play (no HEVC) must transcode an
        // HEVC source, never direct-play it.
        let profile: DeviceProfile = serde_json::from_str(
            r#"{
              "DirectPlayProfiles":[
                {"Container":"mp4","Type":"Video","VideoCodec":"h264","AudioCodec":"aac"}
              ],
              "TranscodingProfiles":[
                {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
              ]
            }"#,
        )
        .unwrap();
        match negotiate(
            &profile,
            &hevc_8bit_source(),
            &ServerCodecSupport::default(),
        ) {
            Decision::Transcode {
                target_video_codec, ..
            } => assert_eq!(target_video_codec.as_deref(), Some("h264")),
            other => panic!("HEVC on an h264-only device must transcode, got {other:?}"),
        }
    }

    #[test]
    fn over_bitrate_source_transcodes_even_when_codecs_match() {
        // A bandwidth-limited client (low MaxStreamingBitrate) must transcode a
        // high-bitrate source it could otherwise direct-play.
        let profile: DeviceProfile = serde_json::from_str(
            r#"{
              "DirectPlayProfiles":[
                {"Container":"mkv","Type":"Video","VideoCodec":"hevc","AudioCodec":"aac"}
              ],
              "TranscodingProfiles":[
                {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264","AudioCodec":"aac"}
              ],
              "MaxStreamingBitrate": 4000000
            }"#,
        )
        .unwrap();
        match negotiate(
            &profile,
            &hevc_8bit_source(),
            &ServerCodecSupport::default(),
        ) {
            Decision::Transcode {
                max_video_bitrate_bps,
                ..
            } => assert_eq!(max_video_bitrate_bps, Some(4_000_000)),
            other => panic!("over-bitrate source must transcode, got {other:?}"),
        }
    }
}
