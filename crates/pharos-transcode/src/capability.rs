//! Server encode capability (T91) — what codecs THIS server can actually
//! produce, and how expensively.
//!
//! The negotiation layer used to pick a transcode target codec purely from the
//! *client* profile (and in practice hardcoded h264), completely blind to what
//! the server hardware can encode. That coupled two facts that never met: which
//! codec to produce (client-side) and what the box can encode (server-side).
//! [`ServerEncodeCapabilities`] is the first API-facing answer to "what can THIS
//! server encode", built once at boot from the trial-confirmed hardware devices
//! plus a parse of `ffmpeg -encoders` for the software encoders. The negotiator
//! then targets the best codec in *(client-decodable ∩ server-encodable)*,
//! hardware-preferred — see `pharos_jellyfin_api::device_profile`.
//!
//! Hardware VP9/AV1 (VAAPI, newer NVENC/Arc) is added in a follow-up; today the
//! hardware families expose only h264/hevc encoders, so VP9/AV1 resolve to the
//! software cost tier here.

use std::collections::BTreeSet;

use tokio::sync::OnceCell;

use crate::hwaccel::HwAccel;
use crate::{AudioCodec, VideoCodec};

/// Run `ffmpeg -hide_banner -encoders` once and cache the parsed name set, so
/// boot-time capability detection doesn't fan out into per-request subprocess
/// spawns (mirrors [`crate::hwaccel::detect_available`]).
static ENCODERS: OnceCell<BTreeSet<String>> = OnceCell::const_new();

/// Detect the ffmpeg encoder names available in this build (cached).
pub async fn detect_encoders(ffmpeg_bin: &str) -> BTreeSet<String> {
    ENCODERS
        .get_or_init(|| async move {
            let output = tokio::process::Command::new(ffmpeg_bin)
                .arg("-hide_banner")
                .arg("-encoders")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .output()
                .await;
            match output {
                Ok(o) if o.status.success() => parse_encoders_output(&o.stdout),
                _ => BTreeSet::new(),
            }
        })
        .await
        .clone()
}

/// How a codec is encoded on this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EncodeAccel {
    /// A trial-confirmed hardware encoder family.
    Hardware(HwAccel),
    /// A software (libx264/libvpx/…) encoder.
    Software,
}

impl EncodeAccel {
    pub fn is_hardware(self) -> bool {
        matches!(self, EncodeAccel::Hardware(_))
    }
}

/// Rough relative encode cost — the negotiator prefers cheaper targets among
/// otherwise-equal choices. Ordered Cheap < Moderate < Expensive < Glacial.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum RelCost {
    /// Hardware-accelerated — realtime with negligible CPU.
    Cheap,
    /// Software H.264/H.265 — realtime on a modern box.
    Moderate,
    /// Software VP9 (libvpx) — near-realtime only with tuning.
    Expensive,
    /// Software AV1 (libaom) — well below realtime for live use.
    Glacial,
}

/// One encodable video codec + how this server encodes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VideoEncodeCap {
    pub codec: VideoCodec,
    pub accel: EncodeAccel,
    pub cost: RelCost,
}

/// What this server can encode. Built once at boot; queried by the negotiation
/// layer. `video` holds one entry per codec, already collapsed to its BEST
/// available acceleration (hardware beats software).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ServerEncodeCapabilities {
    pub video: Vec<VideoEncodeCap>,
    pub audio: Vec<AudioCodec>,
}

/// The concrete video codecs the negotiator may target (never `Copy`, which is
/// a remux decision, not a transcode target).
const TARGET_VIDEO_CODECS: [VideoCodec; 4] = [
    VideoCodec::H264,
    VideoCodec::H265,
    VideoCodec::Vp9,
    VideoCodec::Av1,
];

/// The ffmpeg *software* encoder name pharos invokes for each target codec
/// (mirrors `VideoCodec::ffmpeg_codec` for the software path). Presence of this
/// name in `ffmpeg -encoders` means the software fallback is genuinely usable.
fn software_video_encoder(codec: VideoCodec) -> Option<&'static str> {
    match codec {
        VideoCodec::H264 => Some("libx264"),
        VideoCodec::H265 => Some("libx265"),
        VideoCodec::Vp9 => Some("libvpx-vp9"),
        VideoCodec::Av1 => Some("libaom-av1"),
        VideoCodec::Copy => None,
    }
}

/// Whether a hardware family exposes a real encoder for `codec`. VP9/AV1 return
/// `None` for every family today (no `vp9_encoder`/`av1_encoder` yet), so they
/// fall through to the software tier.
fn hardware_video_encoder(accel: HwAccel, codec: VideoCodec) -> Option<&'static str> {
    match codec {
        VideoCodec::H264 => accel.h264_encoder(),
        VideoCodec::H265 => accel.hevc_encoder(),
        _ => None,
    }
}

/// Software encode cost per codec.
fn software_cost(codec: VideoCodec) -> RelCost {
    match codec {
        VideoCodec::H264 | VideoCodec::H265 => RelCost::Moderate,
        VideoCodec::Vp9 => RelCost::Expensive,
        VideoCodec::Av1 => RelCost::Glacial,
        VideoCodec::Copy => RelCost::Cheap,
    }
}

/// The ffmpeg software audio encoder name pharos invokes for each audio codec.
fn software_audio_encoder(codec: AudioCodec) -> Option<&'static str> {
    match codec {
        // The native aac encoder is always present in modern ffmpeg.
        AudioCodec::Aac => Some("aac"),
        AudioCodec::Mp3 => Some("libmp3lame"),
        AudioCodec::Opus => Some("libopus"),
        AudioCodec::Flac => Some("flac"),
        AudioCodec::Vorbis => Some("libvorbis"),
        AudioCodec::Copy => None,
    }
}

impl ServerEncodeCapabilities {
    /// Build from the set of trial-confirmed hardware families (pass an empty
    /// slice for a software-only / GPU-less box) and the parsed
    /// `ffmpeg -encoders` name set. Each codec resolves to its best available
    /// acceleration: a hardware encoder (Cheap) when any confirmed family
    /// provides it, else the software encoder if present.
    pub fn from_parts(confirmed_hw: &[HwAccel], encoders: &BTreeSet<String>) -> Self {
        let mut video = Vec::new();
        for codec in TARGET_VIDEO_CODECS {
            let hw = confirmed_hw
                .iter()
                .copied()
                .find(|&a| hardware_video_encoder(a, codec).is_some());
            if let Some(accel) = hw {
                video.push(VideoEncodeCap {
                    codec,
                    accel: EncodeAccel::Hardware(accel),
                    cost: RelCost::Cheap,
                });
            } else if software_video_encoder(codec).is_some_and(|e| encoders.contains(e)) {
                video.push(VideoEncodeCap {
                    codec,
                    accel: EncodeAccel::Software,
                    cost: software_cost(codec),
                });
            }
        }
        let audio = [
            AudioCodec::Aac,
            AudioCodec::Mp3,
            AudioCodec::Opus,
            AudioCodec::Flac,
            AudioCodec::Vorbis,
        ]
        .into_iter()
        .filter(|&c| software_audio_encoder(c).is_some_and(|e| encoders.contains(e)))
        .collect();
        Self { video, audio }
    }

    /// Can this server encode `codec` at all (hardware or software)?
    pub fn can_encode(&self, codec: VideoCodec) -> bool {
        self.video.iter().any(|c| c.codec == codec)
    }

    /// How this server best encodes `codec`, if it can.
    pub fn best_for(&self, codec: VideoCodec) -> Option<VideoEncodeCap> {
        self.video.iter().copied().find(|c| c.codec == codec)
    }

    /// The video codecs this server can hardware-encode.
    pub fn hw_codecs(&self) -> impl Iterator<Item = VideoCodec> + '_ {
        self.video
            .iter()
            .filter(|c| c.accel.is_hardware())
            .map(|c| c.codec)
    }

    /// Every video codec this server can encode (hardware or software).
    pub fn encodable_video(&self) -> impl Iterator<Item = VideoCodec> + '_ {
        self.video.iter().map(|c| c.codec)
    }
}

/// Parse `ffmpeg -encoders` into the set of encoder names present. Each body
/// line is `<6 flag chars> <name> <description>`; we take the second
/// whitespace token. Pure — exported for tests (mirrors
/// [`crate::hwaccel::parse_hwaccels_output`]).
pub fn parse_encoders_output(out: &[u8]) -> BTreeSet<String> {
    let s = String::from_utf8_lossy(out);
    let mut names = BTreeSet::new();
    for line in s.lines() {
        let trimmed = line.trim_start();
        // Body lines start with the 6-char capability flag block (e.g. "V....D"
        // / "A....."). The header ("Encoders:", " ------") and the flag legend
        // have no such token in the name position, so require the first token to
        // look like a flag block: exactly 6 chars, first is one of V/A/S.
        let mut it = trimmed.split_whitespace();
        let (Some(flags), Some(name)) = (it.next(), it.next()) else {
            continue;
        };
        // The legend lines (`V..... = Video`) also match the 6-char flag shape,
        // so require the name token to actually look like an encoder id
        // (alphanumeric + `_`/`-`), which excludes the `=` legend separator.
        let name_ok = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if flags.len() == 6 && matches!(flags.as_bytes()[0], b'V' | b'A' | b'S') && name_ok {
            names.insert(name.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn encoders(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_encoders_extracts_names_skips_header_and_legend() {
        let out = b"Encoders:\n \
             V..... = Video\n \
             ------\n \
             V....D libx264              libx264 H.264 / AVC\n \
             V....D libx265              libx265 H.265 / HEVC\n \
             V....D h264_nvenc           NVIDIA NVENC H.264\n \
             A....D aac                  AAC (Advanced Audio Coding)\n \
             A....D libopus              libopus Opus\n";
        let got = parse_encoders_output(out);
        assert!(got.contains("libx264"));
        assert!(got.contains("libx265"));
        assert!(got.contains("h264_nvenc"));
        assert!(got.contains("aac"));
        assert!(got.contains("libopus"));
        // The "= Video" legend token must not be captured as an encoder name.
        assert!(!got.contains("="));
        assert!(!got.contains("Video"));
    }

    #[test]
    fn software_only_box_has_no_hw_and_cost_reflects_codec() {
        let enc = encoders(&[
            "libx264",
            "libx265",
            "libvpx-vp9",
            "libaom-av1",
            "aac",
            "libopus",
        ]);
        let caps = ServerEncodeCapabilities::from_parts(&[], &enc);
        assert!(caps.can_encode(VideoCodec::H264));
        assert!(caps.can_encode(VideoCodec::Vp9));
        assert!(caps.can_encode(VideoCodec::Av1));
        assert_eq!(caps.hw_codecs().count(), 0, "no hardware on a sw-only box");
        assert_eq!(
            caps.best_for(VideoCodec::H264).unwrap().cost,
            RelCost::Moderate
        );
        assert_eq!(
            caps.best_for(VideoCodec::Vp9).unwrap().cost,
            RelCost::Expensive
        );
        assert_eq!(
            caps.best_for(VideoCodec::Av1).unwrap().cost,
            RelCost::Glacial
        );
        assert!(caps.best_for(VideoCodec::H264).unwrap().accel == EncodeAccel::Software);
    }

    #[test]
    fn nvenc_box_hardware_encodes_h264_hevc_but_vp9_stays_software() {
        // NVENC exposes h264/hevc encoders; VP9 has no hw encoder yet → software.
        let enc = encoders(&["libx264", "libx265", "libvpx-vp9", "aac"]);
        let caps = ServerEncodeCapabilities::from_parts(&[HwAccel::Nvenc], &enc);
        let h264 = caps.best_for(VideoCodec::H264).unwrap();
        assert_eq!(h264.accel, EncodeAccel::Hardware(HwAccel::Nvenc));
        assert_eq!(h264.cost, RelCost::Cheap);
        let hevc = caps.best_for(VideoCodec::H265).unwrap();
        assert_eq!(hevc.accel, EncodeAccel::Hardware(HwAccel::Nvenc));
        // VP9: still software (no hw vp9 encoder yet).
        let vp9 = caps.best_for(VideoCodec::Vp9).unwrap();
        assert_eq!(vp9.accel, EncodeAccel::Software);
        assert_eq!(vp9.cost, RelCost::Expensive);
        let hw: Vec<_> = caps.hw_codecs().collect();
        assert!(hw.contains(&VideoCodec::H264) && hw.contains(&VideoCodec::H265));
        assert!(!hw.contains(&VideoCodec::Vp9));
    }

    #[test]
    fn missing_libx265_means_no_h265() {
        let enc = encoders(&["libx264", "aac"]);
        let caps = ServerEncodeCapabilities::from_parts(&[], &enc);
        assert!(caps.can_encode(VideoCodec::H264));
        assert!(
            !caps.can_encode(VideoCodec::H265),
            "build without libx265 can't encode HEVC"
        );
    }

    #[test]
    fn audio_reflects_present_encoders() {
        let enc = encoders(&["libx264", "aac", "libopus"]);
        let caps = ServerEncodeCapabilities::from_parts(&[], &enc);
        assert!(caps.audio.contains(&AudioCodec::Aac));
        assert!(caps.audio.contains(&AudioCodec::Opus));
        assert!(
            !caps.audio.contains(&AudioCodec::Mp3),
            "no libmp3lame → no mp3"
        );
    }
}
