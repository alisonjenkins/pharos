//! `ffprobe` subprocess + SIMD JSON parsing via `sonic-rs`.

use pharos_core::{DomainError, DomainResult, MediaKind, ProbeInfo, Prober};
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
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: String,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeFormat {
    #[serde(default)]
    format_name: Option<String>,
    #[serde(default)]
    duration: Option<String>,
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
    Ok(ProbeInfo {
        kind,
        duration_ms,
        container: parsed.format.format_name,
    })
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
            {"codec_type": "video"},
            {"codec_type": "audio"}
        ],
        "format": {"format_name": "matroska,webm", "duration": "3600.5"}
    }"#;

    const AUDIO_JSON: &[u8] = br#"{
        "streams": [{"codec_type": "audio"}],
        "format": {"format_name": "flac", "duration": "245.123"}
    }"#;

    #[test]
    fn parse_video_returns_movie() {
        let info = parse_ffprobe_output(VIDEO_JSON).unwrap();
        assert_eq!(info.kind, MediaKind::Movie);
        assert_eq!(info.duration_ms, Some(3_600_500));
        assert_eq!(info.container.as_deref(), Some("matroska,webm"));
    }

    #[test]
    fn parse_audio_only_returns_audio() {
        let info = parse_ffprobe_output(AUDIO_JSON).unwrap();
        assert_eq!(info.kind, MediaKind::Audio);
        assert_eq!(info.duration_ms, Some(245_123));
    }

    #[test]
    fn parse_missing_duration_is_none() {
        let json = br#"{"streams":[{"codec_type":"audio"}],"format":{}}"#;
        let info = parse_ffprobe_output(json).unwrap();
        assert!(info.duration_ms.is_none());
    }

    #[test]
    fn parse_garbage_returns_err() {
        let res = parse_ffprobe_output(b"not json");
        assert!(res.is_err());
    }
}
