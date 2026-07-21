//! P14 — hardware video encoder selection.
//!
//! `HwAccel` enumerates the platform encoder families pharos can opt
//! into. Detection runs `ffmpeg -hide_banner -hwaccels` once at boot,
//! parses the output, and reports the set of *available* encoders.
//! Mapping into ffmpeg `-c:v` strings happens at the build_args call
//! site so a transcoder configured with `HwAccel::VideoToolbox`
//! emits `h264_videotoolbox` instead of `libx264` for h264 targets.
//!
//! `HwAccel::Auto` resolves to the first detected encoder in the
//! priority order (VideoToolbox on macOS, Nvenc on NVIDIA-bearing
//! Linux, Qsv on Intel, Vaapi as a generic fallback). When none are
//! detected `Auto` falls back to `Off` (software).

use std::process::Stdio;
use tokio::process::Command;
use tokio::sync::OnceCell;

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum HwAccel {
    #[default]
    Auto,
    Off,
    VideoToolbox,
    Nvenc,
    Vaapi,
    Qsv,
}

impl HwAccel {
    /// ffmpeg encoder name for h264 targets. Returns `None` for the
    /// software-only variants (`Off`, `Auto` pre-resolution) — the
    /// transcoder uses `libx264` then.
    pub fn h264_encoder(self) -> Option<&'static str> {
        match self {
            Self::VideoToolbox => Some("h264_videotoolbox"),
            Self::Nvenc => Some("h264_nvenc"),
            Self::Vaapi => Some("h264_vaapi"),
            Self::Qsv => Some("h264_qsv"),
            _ => None,
        }
    }

    /// ffmpeg encoder name for hevc targets.
    pub fn hevc_encoder(self) -> Option<&'static str> {
        match self {
            Self::VideoToolbox => Some("hevc_videotoolbox"),
            Self::Nvenc => Some("hevc_nvenc"),
            Self::Vaapi => Some("hevc_vaapi"),
            Self::Qsv => Some("hevc_qsv"),
            _ => None,
        }
    }

    /// ffmpeg encoder name for vp9 targets. Only VAAPI has a hardware VP9
    /// encoder in ffmpeg (`vp9_vaapi`); NVENC/QSV/VideoToolbox have none, so
    /// VP9 stays software (`libvpx-vp9`) on those. Whether a given VAAPI device
    /// ACTUALLY has a VP9 encode block is confirmed by a trial encode at boot —
    /// this method only names the encoder to try.
    pub fn vp9_encoder(self) -> Option<&'static str> {
        match self {
            Self::Vaapi => Some("vp9_vaapi"),
            _ => None,
        }
    }

    /// ffmpeg encoder name for av1 targets. Hardware AV1 encode exists on newer
    /// VAAPI (Intel Arc / AMD RDNA3+), NVENC (Ada / RTX 40+) and QSV (Arc); a
    /// trial encode at boot confirms whether THIS device really has it (e.g. a
    /// Pascal NVENC names `av1_nvenc` but has no AV1 block — the trial fails and
    /// it is not advertised).
    pub fn av1_encoder(self) -> Option<&'static str> {
        match self {
            Self::Vaapi => Some("av1_vaapi"),
            Self::Nvenc => Some("av1_nvenc"),
            Self::Qsv => Some("av1_qsv"),
            _ => None,
        }
    }

    /// The ffmpeg encoder name for `codec` on this family, or `None` when the
    /// family has no hardware encoder for it.
    pub fn video_encoder(self, codec: crate::VideoCodec) -> Option<&'static str> {
        use crate::VideoCodec::*;
        match codec {
            H264 => self.h264_encoder(),
            H265 => self.hevc_encoder(),
            Vp9 => self.vp9_encoder(),
            Av1 => self.av1_encoder(),
            Copy => None,
        }
    }

    /// Resolve `Auto` against the detected-encoder set. Pure function
    /// on a snapshot — the snapshot itself comes from
    /// [`detect_available`].
    pub fn resolve_auto(self, detected: &[HwAccel]) -> HwAccel {
        if !matches!(self, Self::Auto) {
            return self;
        }
        // Priority order: prefer VideoToolbox on macOS, then GPU
        // encoders by efficiency.
        for candidate in [Self::VideoToolbox, Self::Nvenc, Self::Qsv, Self::Vaapi] {
            if detected.contains(&candidate) {
                return candidate;
            }
        }
        Self::Off
    }
}

/// Run `ffmpeg -hide_banner -hwaccels` once and cache the parsed
/// result. Repeated callers reuse the cache so boot-time detection
/// doesn't fan out into per-request subprocess spawns.
static DETECTED: OnceCell<Vec<HwAccel>> = OnceCell::const_new();

pub async fn detect_available(ffmpeg_bin: &str) -> Vec<HwAccel> {
    DETECTED
        .get_or_init(|| async move {
            let output = Command::new(ffmpeg_bin)
                .arg("-hide_banner")
                .arg("-hwaccels")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .await;
            match output {
                Ok(o) if o.status.success() => parse_hwaccels_output(&o.stdout),
                _ => Vec::new(),
            }
        })
        .await
        .clone()
}

/// Parse the body of `ffmpeg -hwaccels` (one accel name per line
/// after a header line). Pure function — exported for tests.
pub fn parse_hwaccels_output(out: &[u8]) -> Vec<HwAccel> {
    let s = String::from_utf8_lossy(out);
    let mut found = Vec::new();
    for line in s.lines() {
        let line = line.trim().to_ascii_lowercase();
        let mapped = match line.as_str() {
            "videotoolbox" => Some(HwAccel::VideoToolbox),
            "cuda" | "nvdec" | "nvenc" => Some(HwAccel::Nvenc),
            "vaapi" => Some(HwAccel::Vaapi),
            "qsv" => Some(HwAccel::Qsv),
            _ => None,
        };
        if let Some(a) = mapped {
            if !found.contains(&a) {
                found.push(a);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_macos_ffmpeg_hwaccels() {
        let out = b"Hardware acceleration methods:\nvideotoolbox\n";
        assert_eq!(parse_hwaccels_output(out), vec![HwAccel::VideoToolbox]);
    }

    #[test]
    fn parse_linux_with_nvenc_and_vaapi() {
        let out = b"Hardware acceleration methods:\ncuda\nnvdec\nvaapi\n";
        let got = parse_hwaccels_output(out);
        assert!(got.contains(&HwAccel::Nvenc));
        assert!(got.contains(&HwAccel::Vaapi));
        // No duplicates even when both `cuda` and `nvdec` appear.
        let n = got.iter().filter(|x| **x == HwAccel::Nvenc).count();
        assert_eq!(n, 1);
    }

    #[test]
    fn parse_unknown_accel_silently_dropped() {
        let out = b"Hardware acceleration methods:\nopencl\nvulkan\n";
        assert!(parse_hwaccels_output(out).is_empty());
    }

    #[test]
    fn h264_encoder_mapping() {
        assert_eq!(
            HwAccel::VideoToolbox.h264_encoder(),
            Some("h264_videotoolbox")
        );
        assert_eq!(HwAccel::Nvenc.h264_encoder(), Some("h264_nvenc"));
        assert_eq!(HwAccel::Off.h264_encoder(), None);
        assert_eq!(HwAccel::Auto.h264_encoder(), None);
    }

    #[test]
    fn resolve_auto_picks_videotoolbox_when_available() {
        let detected = vec![HwAccel::VideoToolbox, HwAccel::Nvenc];
        assert_eq!(HwAccel::Auto.resolve_auto(&detected), HwAccel::VideoToolbox);
    }

    #[test]
    fn resolve_auto_falls_back_to_off_when_nothing_detected() {
        assert_eq!(HwAccel::Auto.resolve_auto(&[]), HwAccel::Off);
    }

    #[test]
    fn resolve_auto_passes_through_explicit_selection() {
        let detected = vec![HwAccel::VideoToolbox];
        assert_eq!(HwAccel::Nvenc.resolve_auto(&detected), HwAccel::Nvenc);
    }
}
