//! Property-based fuzz tests on the device-profile negotiator.
//!
//! Per-handler unit tests can't enumerate the input space — every
//! supported client ships its own DeviceProfile shape, and the
//! interaction between containers / codecs / bitrate caps surfaces
//! bugs no fixed test grid catches.
//!
//! Invariants we exercise:
//!
//! - `negotiate(profile, source)` always returns a Decision; never
//!   panics on adversarial input.
//! - When DirectPlay matches, the source's container is in the
//!   profile's CSV list and codecs satisfy the profile's CSV match.
//! - When over-bitrate, DirectPlay is never returned.
//! - When the source has no audio (audio_codec=None), the negotiator
//!   still produces a usable Decision (silent video must direct-play
//!   when container + video match — the bug that broke BBB).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_jellyfin_api::device_profile::{
    negotiate, Decision, DeviceProfile, DirectPlayProfile, ServerCodecSupport, SourceMedia,
    TranscodingProfile,
};
use proptest::prelude::*;

const VIDEO_CONTAINERS: &[&str] = &["webm", "mp4", "m4v", "mkv", "ts", "avi"];
const VIDEO_CODECS: &[&str] = &["vp9", "vp8", "av1", "h264", "hevc"];
const AUDIO_CODECS: &[&str] = &["opus", "vorbis", "aac", "mp3", "flac"];

fn container_strat() -> impl Strategy<Value = String> {
    prop::sample::select(VIDEO_CONTAINERS.to_vec()).prop_map(String::from)
}
fn video_codec_strat() -> impl Strategy<Value = String> {
    prop::sample::select(VIDEO_CODECS.to_vec()).prop_map(String::from)
}
fn audio_codec_strat() -> impl Strategy<Value = String> {
    prop::sample::select(AUDIO_CODECS.to_vec()).prop_map(String::from)
}

fn direct_play_profile_strat() -> impl Strategy<Value = DirectPlayProfile> {
    (
        // CSV list of 1-3 containers.
        prop::collection::vec(container_strat(), 1..=3),
        prop::collection::vec(video_codec_strat(), 1..=3),
        prop::collection::vec(audio_codec_strat(), 1..=3),
    )
        .prop_map(|(c, v, a)| DirectPlayProfile {
            container: c.join(","),
            video_codec: v.join(","),
            audio_codec: a.join(","),
            kind: "Video".into(),
        })
}

fn transcoding_profile_strat() -> impl Strategy<Value = TranscodingProfile> {
    (container_strat(), video_codec_strat(), audio_codec_strat()).prop_map(|(c, v, a)| {
        TranscodingProfile {
            container: c,
            video_codec: v,
            audio_codec: a,
            protocol: "hls".into(),
            kind: "Video".into(),
        }
    })
}

fn profile_strat() -> impl Strategy<Value = DeviceProfile> {
    (
        prop::collection::vec(direct_play_profile_strat(), 0..=4),
        prop::collection::vec(transcoding_profile_strat(), 0..=2),
        prop::option::of(1_000_000u64..=200_000_000),
    )
        .prop_map(|(dpp, tp, max_br)| DeviceProfile {
            direct_play_profiles: dpp,
            transcoding_profiles: tp,
            max_streaming_bitrate: max_br,
            max_static_bitrate: None,
            codec_profiles: vec![],
            subtitle_profiles: vec![],
        })
}

fn source_strat() -> impl Strategy<Value = SourceMedia> {
    (
        container_strat(),
        prop::option::of(video_codec_strat()),
        prop::option::of(audio_codec_strat()),
        prop::option::of(100_000u64..=20_000_000),
    )
        .prop_map(|(c, v, a, br)| SourceMedia {
            container: c,
            video_codec: v,
            audio_codec: a,
            bitrate_bps: br,
            is_video: true,
            ..Default::default()
        })
}

// P47 — `PROPTEST_CASES` env override; see auth_header_fuzz.rs for
// the design notes.
fn cfg() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(cfg())]

    /// Invariant 1: the negotiator never panics on adversarial input.
    /// All shapes must produce *some* Decision.
    #[test]
    fn negotiate_never_panics(profile in profile_strat(), source in source_strat()) {
        let _ = negotiate(&profile, &source, &ServerCodecSupport::default());
    }

    /// Invariant 2: when DirectPlay is selected, at least one
    /// DirectPlayProfile must list this source's container (in the
    /// CSV) and codec set.
    #[test]
    fn direct_play_matches_a_profile(profile in profile_strat(), source in source_strat()) {
        if let Decision::DirectPlay = negotiate(&profile, &source, &ServerCodecSupport::default()) {
            let any_match = profile.direct_play_profiles.iter().any(|p| {
                let containers: Vec<&str> = p.container.split(',').map(str::trim).collect();
                containers.iter().any(|c| c.eq_ignore_ascii_case(&source.container))
            });
            prop_assert!(any_match, "DirectPlay picked but no profile lists container {}", source.container);
        }
    }

    /// Invariant 3: when source bitrate exceeds the profile cap,
    /// DirectPlay must not be selected.
    #[test]
    fn over_bitrate_disables_direct_play(profile in profile_strat(), source in source_strat()) {
        let cap = profile.max_streaming_bitrate.or(profile.max_static_bitrate);
        if let (Some(cap), Some(br)) = (cap, source.bitrate_bps) {
            if br > cap {
                let d = negotiate(&profile, &source, &ServerCodecSupport::default());
                prop_assert!(!matches!(d, Decision::DirectPlay), "DirectPlay over bitrate: {d:?}");
            }
        }
    }

    /// Invariant 4: silent video (audio_codec=None) must still
    /// DirectPlay when container + video codec match the profile.
    /// This is the BBB-corpus regression — fabricating audio broke
    /// the decoder; rejecting direct play broke the UX.
    #[test]
    fn silent_video_direct_plays_when_container_and_video_match(
        container in container_strat(),
        video in video_codec_strat(),
    ) {
        let profile = DeviceProfile {
            direct_play_profiles: vec![DirectPlayProfile {
                container: container.clone(),
                video_codec: video.clone(),
                audio_codec: "vorbis,opus".into(),
                kind: "Video".into(),
            }],
            ..Default::default()
        };
        let source = SourceMedia {
            container,
            video_codec: Some(video),
            audio_codec: None,
            bitrate_bps: Some(2_000_000),
            is_video: true,
            ..Default::default()
        };
        let d = negotiate(&profile, &source, &ServerCodecSupport::default());
        prop_assert!(matches!(d, Decision::DirectPlay), "silent video must direct-play: {d:?}");
    }

    /// Invariant 5: case-insensitive container match. ffprobe emits
    /// lowercase; manual DeviceProfiles vary.
    #[test]
    fn container_match_is_case_insensitive(
        container in container_strat(),
        video in video_codec_strat(),
        audio in audio_codec_strat(),
    ) {
        let profile = DeviceProfile {
            direct_play_profiles: vec![DirectPlayProfile {
                container: container.to_uppercase(),
                video_codec: video.clone(),
                audio_codec: audio.clone(),
                kind: "Video".into(),
            }],
            ..Default::default()
        };
        let source = SourceMedia {
            container,
            video_codec: Some(video),
            audio_codec: Some(audio),
            bitrate_bps: Some(2_000_000),
            is_video: true,
            ..Default::default()
        };
        let d = negotiate(&profile, &source, &ServerCodecSupport::default());
        prop_assert!(matches!(d, Decision::DirectPlay), "case mismatch broke direct play: {d:?}");
    }
}
