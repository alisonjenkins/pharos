//! P48 — `SpawnBackend`. Default backend that spawns a real `ffmpeg`
//! / `ffprobe` binary on PATH. Implementations mirror the existing
//! shell-out code paths scattered across pharos-server +
//! pharos-scanner; the trait centralises them so the lib backend
//! (P49-P51) only has to implement the same surface once.

use crate::backend::{BackendError, FfmpegBackend, ProbeJson, SubtitleFormat, WaveformPoint};
use crate::{FfmpegTranscoder, HwAccel, TranscodeOptions, TranscodeStream};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::process::Command;

/// Default backend wrapping the real `ffmpeg` + `ffprobe` binaries.
/// Holds the binary paths so test deployments can swap in a stub
/// `ffmpeg` script when the real one isn't installed.
#[derive(Debug, Clone)]
pub struct SpawnBackend {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub hwaccel: HwAccel,
}

impl Default for SpawnBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SpawnBackend {
    pub fn new() -> Self {
        Self {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            hwaccel: HwAccel::Off,
        }
    }

    pub fn with_hwaccel(mut self, accel: HwAccel) -> Self {
        self.hwaccel = accel;
        self
    }
}

impl FfmpegBackend for SpawnBackend {
    fn probe<'a>(
        &'a self,
        path: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<ProbeJson, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let input = path.to_str().ok_or(BackendError::NonUtf8Path)?;
            let out = Command::new(&self.ffprobe)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-print_format",
                    "json",
                    "-show_streams",
                    "-show_format",
                    "-show_chapters",
                    input,
                ])
                .output()
                .await?;
            if !out.status.success() {
                return Err(BackendError::Ffmpeg(
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ));
            }
            Ok(ProbeJson(out.stdout))
        })
    }

    fn extract_image<'a>(
        &'a self,
        src: &'a Path,
        seek_ms: u64,
        width: u32,
        out: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let input = src.to_str().ok_or(BackendError::NonUtf8Path)?;
            let out_str = out.to_str().ok_or(BackendError::NonUtf8Path)?;
            let seek_s = format!("{}.{:03}", seek_ms / 1000, seek_ms % 1000);
            let filter = format!("scale={width}:-2");
            let status = Command::new(&self.ffmpeg)
                .args([
                    "-y",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-ss",
                    &seek_s,
                    "-i",
                    input,
                    "-frames:v",
                    "1",
                    "-vf",
                    &filter,
                    "-q:v",
                    "3",
                    out_str,
                ])
                .status()
                .await?;
            if !status.success() {
                return Err(BackendError::Ffmpeg("image extract".into()));
            }
            Ok(())
        })
    }

    fn extract_subtitle<'a>(
        &'a self,
        src: &'a Path,
        stream_idx: u32,
        format: SubtitleFormat,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let input = src.to_str().ok_or(BackendError::NonUtf8Path)?;
            let (codec, mux) = match format {
                SubtitleFormat::WebVtt => ("webvtt", "webvtt"),
                SubtitleFormat::Srt => ("subrip", "srt"),
            };
            let map = format!("0:{stream_idx}");
            let out = Command::new(&self.ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-i",
                    input,
                    "-map",
                    &map,
                    "-c:s",
                    codec,
                    "-f",
                    mux,
                    "pipe:1",
                ])
                .output()
                .await?;
            if !out.status.success() {
                return Err(BackendError::Ffmpeg(
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ));
            }
            Ok(out.stdout)
        })
    }

    fn convert_srt_to_webvtt<'a>(
        &'a self,
        src: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let input = src.to_str().ok_or(BackendError::NonUtf8Path)?;
            let out = Command::new(&self.ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-i",
                    input,
                    "-c:s",
                    "webvtt",
                    "-f",
                    "webvtt",
                    "pipe:1",
                ])
                .output()
                .await?;
            if !out.status.success() {
                return Err(BackendError::Ffmpeg(
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ));
            }
            Ok(out.stdout)
        })
    }

    fn transcode_image<'a>(
        &'a self,
        src: &'a Path,
        target_ext: &'a str,
        out: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let input = src.to_str().ok_or(BackendError::NonUtf8Path)?;
            let out_str = out.to_str().ok_or(BackendError::NonUtf8Path)?;
            let codec = match target_ext {
                "webp" => "libwebp",
                "avif" => "libaom-av1",
                _ => return Err(BackendError::UnsupportedCodec(target_ext.to_string())),
            };
            let mut cmd = Command::new(&self.ffmpeg);
            cmd.args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-i",
                input,
                "-c:v",
                codec,
            ]);
            if target_ext == "avif" {
                cmd.args(["-still-picture", "1", "-cpu-used", "8"]);
            } else {
                cmd.args(["-quality", "80"]);
            }
            cmd.arg(out_str);
            let status = cmd.status().await?;
            if !status.success() {
                return Err(BackendError::Ffmpeg(format!("{target_ext} transcode")));
            }
            Ok(())
        })
    }

    fn waveform_rms<'a>(
        &'a self,
        src: &'a Path,
        samples_per_bin: u64,
        target_bins: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<WaveformPoint>, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let input = src.to_str().ok_or(BackendError::NonUtf8Path)?;
            let filter = format!(
                "aresample=async=1,aformat=channel_layouts=mono,asetnsamples={samples_per_bin},astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level"
            );
            let out = Command::new(&self.ffmpeg)
                .args([
                    "-hide_banner",
                    "-nostdin",
                    "-loglevel",
                    "info",
                    "-i",
                    input,
                    "-vn",
                    "-af",
                    &filter,
                    "-f",
                    "null",
                    "-",
                ])
                .output()
                .await?;
            if !out.status.success() {
                return Err(BackendError::Ffmpeg(
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ));
            }
            let stderr = String::from_utf8_lossy(&out.stderr);
            let mut peaks: Vec<WaveformPoint> = Vec::with_capacity(target_bins as usize);
            for line in stderr.lines() {
                if let Some(idx) = line.find("RMS_level=") {
                    let tail = &line[idx + "RMS_level=".len()..];
                    let val = tail.split_whitespace().next().unwrap_or("");
                    if val == "-inf" || val.is_empty() {
                        peaks.push(0.0);
                        continue;
                    }
                    if let Ok(db) = val.parse::<f32>() {
                        peaks.push(db);
                    } else {
                        peaks.push(0.0);
                    }
                }
            }
            while peaks.len() < target_bins as usize {
                peaks.push(0.0);
            }
            peaks.truncate(target_bins as usize);
            Ok(peaks)
        })
    }

    fn transcode_stream<'a>(
        &'a self,
        src: &'a Path,
        opts: &'a TranscodeOptions,
    ) -> Pin<Box<dyn Future<Output = Result<TranscodeStream, BackendError>> + Send + 'a>> {
        Box::pin(async move {
            let transcoder =
                FfmpegTranscoder::with_binary(self.ffmpeg.clone()).with_hwaccel(self.hwaccel);
            transcoder
                .transcode(src, opts)
                .await
                .map_err(|e| BackendError::Ffmpeg(e.to_string()))
        })
    }
}
