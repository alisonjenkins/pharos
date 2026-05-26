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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMedia {
    pub container: String,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub is_video: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    DirectPlay,
    /// Audio remux: container + video codec match a DirectPlayProfile
    /// but the audio codec doesn't. The video stream copies; audio
    /// re-encodes to the negotiated codec.
    AudioRemux {
        target_audio_codec: String,
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
            return Decision::DirectPlay;
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

    // Fall through to TranscodingProfile.
    if let Some(tp) = profile
        .transcoding_profiles
        .iter()
        .find(|t| t.kind.is_empty() || t.kind.eq_ignore_ascii_case(want_kind))
    {
        return Decision::Transcode {
            target_container: pick_first_csv(&tp.container)
                .unwrap_or_else(|| default_container(source.is_video).into()),
            target_video_codec: pick_first_csv(&tp.video_codec),
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

fn matches_codec(csv: &str, candidate: Option<&str>) -> bool {
    if csv.is_empty() {
        return true;
    }
    let Some(c) = candidate else {
        return false;
    };
    matches_csv(csv, c)
}

fn pick_first_csv(csv: &str) -> Option<String> {
    csv.split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(|s| s.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
}
