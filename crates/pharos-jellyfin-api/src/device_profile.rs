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
pub fn negotiate(profile: &DeviceProfile, source: &SourceMedia) -> Decision {
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
            // B59 — prefer h264 (shareable + universally decodable) over the
            // client's first-listed codec for VIDEO, so a SyncPlay group whose
            // browsers each list a different first codec still converges on one
            // shared encode. Audio keeps first-listed.
            target_video_codec: if source.is_video {
                pick_preferred_video_codec(&tp.video_codec)
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
        target_video_codec: source.is_video.then(|| "h264".to_string()),
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

/// Pick the transcode VIDEO codec from a client's CSV list, PREFERRING H.264
/// when the client offers it (B59). H.264 is the universally-decodable,
/// hardware-friendly target AND — crucially for SyncPlay — the SHAREABLE one:
/// every group member transcoding to h264 hits ONE cached encode, keyed by
/// `(media, seg, audio, sub, bitrate, codec)`. Honouring the client's
/// first-listed codec instead (e.g. a browser that lists `vp9,h264`) split a
/// group across two encodes — one member cold-transcoding VP9 (slow, software)
/// while the rest shared a warm h264 stream, so the VP9 member stalled and
/// never joined the shared playback. A client that offers NO h264 (a VP9/AV1-
/// only profile) still gets its first codec. The Linux-Firefox-can't-decode-
/// h264 case is forced to VP9 downstream (playback_info `force_webm`), which
/// overrides this — so a browser that genuinely needs VP9 still gets it.
fn pick_preferred_video_codec(csv: &str) -> Option<String> {
    let codecs: Vec<&str> = csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    codecs
        .iter()
        .find(|c| c.eq_ignore_ascii_case("h264"))
        .or_else(|| codecs.first())
        .map(|s| (*s).to_string())
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
        let d = negotiate(&DeviceProfile::default(), &webm_vp9_opus_source());
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
    fn prefers_h264_over_first_listed_video_codec() {
        // B59 — the pure picker: h264 wins whenever offered, whatever its slot.
        assert_eq!(
            pick_preferred_video_codec("vp9,h264").as_deref(),
            Some("h264")
        );
        assert_eq!(
            pick_preferred_video_codec("h264,vp9").as_deref(),
            Some("h264")
        );
        // Case-insensitive match.
        assert_eq!(
            pick_preferred_video_codec("VP9, H264").as_deref(),
            Some("H264")
        );
        // No h264 offered → first listed stands (a VP9/AV1-only browser).
        assert_eq!(
            pick_preferred_video_codec("av1,vp9").as_deref(),
            Some("av1")
        );
        assert_eq!(pick_preferred_video_codec("vp9").as_deref(), Some("vp9"));
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
        match negotiate(&profile, &source) {
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
            negotiate(&profile, &webm_vp9_opus_source()),
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
            negotiate(&profile, &webm_vp9_opus_source()),
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
        match negotiate(&profile, &webm_vp9_opus_source()) {
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
        match negotiate(&profile, &webm_vp9_opus_source()) {
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
        let d = negotiate(&profile, &source);
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
        assert_eq!(negotiate(&profile, &source), Decision::DirectPlay);
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
        assert_eq!(negotiate(&profile, &source), Decision::DirectPlay);
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
            negotiate(&profile, &hevc_8bit_source()),
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
        match negotiate(&profile, &hevc_10bit_source()) {
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
        match negotiate(&profile, &hevc_8bit_source()) {
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
        match negotiate(&profile, &hevc_8bit_source()) {
            Decision::Transcode {
                max_video_bitrate_bps,
                ..
            } => assert_eq!(max_video_bitrate_bps, Some(4_000_000)),
            other => panic!("over-bitrate source must transcode, got {other:?}"),
        }
    }
}
