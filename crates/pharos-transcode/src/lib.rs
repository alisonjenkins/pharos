//! pharos-transcode — ffmpeg subprocess wrapper producing a byte stream
//! the HTTP layer can pipe to the client.
//!
//! V6: a crashed ffmpeg child must never bring the server down. The
//! `TranscodeStream` returned here propagates failures as `Err` items
//! on the stream; the calling handler decides whether to close the
//! response or retry. `Drop` kills the child so abandoned transcodes
//! don't leak processes.
//!
//! V12: the public API takes a plain `Path` + `TranscodeOptions` —
//! no IO traits leak into core. Transcoder swap (e.g. a future GPU-
//! accelerated impl) happens at the wiring layer.

pub mod backend;
pub mod device;
pub mod hwaccel;
pub mod options;
pub mod protocol;
pub mod scheduler;
#[cfg(unix)]
pub mod worker;

#[cfg(feature = "backend-lib")]
pub use backend::LibBackend;
#[cfg(feature = "backend-spawn")]
pub use backend::SpawnBackend;
pub use backend::{BackendError, FfmpegBackend, ProbeJson, SubtitleFormat, WaveformPoint};
pub use hwaccel::HwAccel;
pub use options::{AudioCodec, Container, TranscodeOptions, VideoCodec};

use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncRead;
use tokio::process::{Child, ChildStdout, Command};
use tokio_util::io::ReaderStream;
use tracing::instrument;

#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    #[error("spawn ffmpeg: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("ffmpeg stdout not captured")]
    NoStdout,
    #[error("invalid path: not utf-8")]
    NonUtf8Path,
}

#[derive(Debug, Clone)]
pub struct FfmpegTranscoder {
    ffmpeg_bin: PathBuf,
    hwaccel: HwAccel,
}

impl Default for FfmpegTranscoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FfmpegTranscoder {
    pub fn new() -> Self {
        Self {
            ffmpeg_bin: PathBuf::from("ffmpeg"),
            hwaccel: HwAccel::Off,
        }
    }

    pub fn with_binary(p: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg_bin: p.into(),
            hwaccel: HwAccel::Off,
        }
    }

    /// P14 — attach a hardware encoder. `HwAccel::Off` keeps the
    /// software libx264/libx265 path; anything else swaps the `-c:v`
    /// to the matching platform encoder for h264 / hevc targets.
    pub fn with_hwaccel(mut self, accel: HwAccel) -> Self {
        self.hwaccel = accel;
        self
    }

    pub fn hwaccel(&self) -> HwAccel {
        self.hwaccel
    }

    pub fn binary(&self) -> &Path {
        &self.ffmpeg_bin
    }

    /// Spawn an ffmpeg transcode and return a stream of its stdout bytes.
    /// The returned `TranscodeStream` owns the child; dropping it sends
    /// `SIGKILL` to the subprocess.
    #[instrument(skip(self), fields(ffmpeg = %self.ffmpeg_bin.display(), input = %input.display()))]
    pub async fn transcode(
        &self,
        input: &Path,
        opts: &TranscodeOptions,
    ) -> Result<TranscodeStream, TranscodeError> {
        let input_str = input.to_str().ok_or(TranscodeError::NonUtf8Path)?;
        let args = build_args_for_device(input_str, opts, hwaccel_to_device(self.hwaccel), "pipe:1");
        let mut cmd = Command::new(&self.ffmpeg_bin);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(TranscodeError::Spawn)?;
        let stdout = child.stdout.take().ok_or(TranscodeError::NoStdout)?;
        Ok(TranscodeStream::new(child, stdout))
    }
}

/// Owned byte stream over a running ffmpeg child. Implements `AsyncRead`
/// (forwarding to the child's stdout). The owned `KillGuard` ensures
/// dropping the stream kills the subprocess (V6).
pub struct TranscodeStream {
    _kill_guard: KillGuard,
    stdout: ChildStdout,
}

impl TranscodeStream {
    fn new(child: Child, stdout: ChildStdout) -> Self {
        Self {
            _kill_guard: KillGuard(Some(child)),
            stdout,
        }
    }

    /// Wrap stdout in a `ReaderStream` to feed HTTP frameworks that
    /// expect `Stream<Item = io::Result<Bytes>>`. The kill guard is
    /// moved into the stream so the subprocess outlives the consumer
    /// for exactly as long as the stream itself lives.
    pub fn into_stream(self) -> impl futures_core::Stream<Item = std::io::Result<Bytes>> {
        let Self {
            _kill_guard: guard,
            stdout,
        } = self;
        OwningStream {
            _child: guard,
            inner: ReaderStream::new(stdout),
        }
    }
}

impl AsyncRead for TranscodeStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let stdout = unsafe { &mut self.get_unchecked_mut().stdout };
        Pin::new(stdout).poll_read(cx, buf)
    }
}

struct OwningStream<S> {
    _child: KillGuard,
    inner: S,
}

struct KillGuard(Option<Child>);

impl Drop for KillGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.start_kill();
        }
    }
}

impl<S> futures_core::Stream for OwningStream<S>
where
    S: futures_core::Stream + Unpin,
{
    type Item = S::Item;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Quote + escape a path for the ffmpeg `subtitles=filename=…` filter
/// argument. We wrap the whole filename in single quotes so the
/// filtergraph metacharacters `: , [ ] ;` inside the path are taken
/// literally (the previous char-by-char escaping missed `[ ] ;` and
/// emitted the value unquoted — a real bug: any library path like
/// `/m/[Group] Title [1080p].mkv` aborted the encode). Inside the
/// quotes only `\` and `'` need handling; ffmpeg's quote syntax escapes
/// an embedded single quote as `'\''`.
fn escape_subtitles_filename(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 8);
    out.push('\'');
    for c in p.chars() {
        match c {
            '\'' => out.push_str("'\\''"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
fn build_args(input: &str, opts: &TranscodeOptions) -> Vec<String> {
    build_args_for_device(input, opts, crate::protocol::DeviceId::Cpu, "pipe:1")
}

#[cfg(test)]
fn build_args_with_hwaccel(
    input: &str,
    opts: &TranscodeOptions,
    hwaccel: HwAccel,
    output: &str,
) -> Vec<String> {
    build_args_for_device(input, opts, hwaccel_to_device(hwaccel), output)
}

/// Map a legacy `HwAccel` selection to a concrete `DeviceId` (GPU
/// ordinal 0). `Off`/`Auto` → CPU (software).
fn hwaccel_to_device(h: HwAccel) -> crate::protocol::DeviceId {
    match h {
        HwAccel::Off | HwAccel::Auto => crate::protocol::DeviceId::Cpu,
        a => crate::protocol::DeviceId::hw(a, 0),
    }
}

/// Build the ffmpeg argv for a transcode whose output goes to `output`
/// (`"pipe:1"` for streaming, or a file path for the worker's
/// `FileDirect` sink) on a concrete `device`. Exposed so the
/// out-of-process worker reuses the exact same negotiated argument logic
/// as the in-process transcoder.
pub fn ffmpeg_transcode_args(
    input: &str,
    opts: &TranscodeOptions,
    device: crate::protocol::DeviceId,
    output: &str,
) -> Vec<String> {
    build_args_for_device(input, opts, device, output)
}

fn build_args_for_device(
    input: &str,
    opts: &TranscodeOptions,
    device: crate::protocol::DeviceId,
    output: &str,
) -> Vec<String> {
    use crate::protocol::DeviceId;
    let hwaccel = device.hwaccel();
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    // VAAPI: select the render node BEFORE `-i` so the `hwupload` filter
    // resolves the default device. Each GPU is a distinct render node.
    if let Some(node) = device.vaapi_render_node() {
        a.push("-vaapi_device".into());
        a.push(node);
    }
    // Seek placement: input seeking (`-ss` before `-i`) is fast but
    // desyncs a burned-in subtitles filter (which demuxes the file from
    // 0 with original PTS). When burning subtitles at a non-zero start,
    // use output seeking (`-ss` after `-i`) to keep video + subs aligned.
    let burning_subs = opts.video.is_some()
        && !matches!(opts.video, Some(VideoCodec::Copy))
        && opts.burn_subtitle_stream_index.is_some();
    let start = opts.start_position_seconds();
    if let Some(pos) = start {
        if !burning_subs {
            a.push("-ss".into());
            a.push(format!("{pos:.3}"));
        }
    }
    a.push("-i".into());
    a.push(input.to_string());
    if let Some(pos) = start {
        if burning_subs {
            // Output-side seek keeps the subtitles filter in sync.
            a.push("-ss".into());
            a.push(format!("{pos:.3}"));
        }
    }
    if let Some(dur) = opts.duration_seconds() {
        a.push("-t".into());
        a.push(format!("{dur:.3}"));
    }
    // W1 — when the caller specifies an audio stream, route the
    // video + the chosen audio track explicitly. `0:v?` keeps video
    // optional (no error on audio-only sources). Default selection
    // falls through to ffmpeg's "pick the most appropriate stream"
    // heuristic which mirrors the prior behaviour.
    if let Some(audio_idx) = opts.audio_source_stream_index {
        a.push("-map".into());
        a.push("0:v?".into());
        a.push("-map".into());
        a.push(format!("0:a:{audio_idx}"));
    }
    match opts.video {
        Some(VideoCodec::Copy) => {
            a.push("-c:v".into());
            a.push("copy".into());
        }
        Some(c) => {
            // Compose the software filter prefix (subtitle burn-in) that
            // must run before any hardware upload.
            let mut vf_parts: Vec<String> = Vec::new();
            if let Some(sub_idx) = opts.burn_subtitle_stream_index {
                vf_parts.push(format!(
                    "subtitles=filename={}:si={sub_idx}",
                    escape_subtitles_filename(input)
                ));
            }
            let is_vaapi = matches!(device, DeviceId::Hw { accel: HwAccel::Vaapi, .. });
            if is_vaapi && matches!(c, VideoCodec::H264 | VideoCodec::H265) {
                // VAAPI: upload frames to the GPU, then encode. The
                // upload filter chains after any software filters.
                vf_parts.push("format=nv12".into());
                vf_parts.push("hwupload".into());
                a.push("-vf".into());
                a.push(vf_parts.join(","));
                a.push("-c:v".into());
                a.push(
                    if matches!(c, VideoCodec::H265) {
                        "hevc_vaapi"
                    } else {
                        "h264_vaapi"
                    }
                    .into(),
                );
                if let Some(b) = opts.video_bitrate_bps {
                    a.push("-b:v".into());
                    a.push(format!("{b}"));
                }
            } else {
                // NVENC / QSV / VideoToolbox / software: pick the encoder
                // name (hw-mapped or software default).
                let encoder = match c {
                    VideoCodec::H264 => hwaccel.h264_encoder().unwrap_or(c.ffmpeg_codec()),
                    VideoCodec::H265 => hwaccel.hevc_encoder().unwrap_or(c.ffmpeg_codec()),
                    _ => c.ffmpeg_codec(),
                };
                if !vf_parts.is_empty() {
                    a.push("-vf".into());
                    a.push(vf_parts.join(","));
                }
                a.push("-c:v".into());
                a.push(encoder.into());
                // NVENC GPU ordinal selection on a multi-GPU box.
                if matches!(device, DeviceId::Hw { accel: HwAccel::Nvenc, .. }) {
                    if let Some(idx) = device.index() {
                        a.push("-gpu".into());
                        a.push(idx.to_string());
                    }
                }
                if let Some(b) = opts.video_bitrate_bps {
                    a.push("-b:v".into());
                    a.push(format!("{b}"));
                }
            }
        }
        None => {
            a.push("-vn".into());
        }
    }
    match opts.audio {
        Some(AudioCodec::Copy) => {
            a.push("-c:a".into());
            a.push("copy".into());
        }
        Some(c) => {
            a.push("-c:a".into());
            a.push(c.ffmpeg_codec().into());
            if let Some(b) = opts.audio_bitrate_bps {
                a.push("-b:a".into());
                a.push(format!("{b}"));
            }
        }
        None => {
            a.push("-an".into());
        }
    }
    a.push("-f".into());
    a.push(opts.container.ffmpeg_muxer().into());
    a.push("-movflags".into());
    a.push("+empty_moov+frag_keyframe+default_base_moof".into());
    // File-direct outputs are written by the worker; ffmpeg refuses to
    // overwrite an existing file without `-y`, and the scheduler hands
    // us a fresh `.tmp` path so clobbering is intended.
    if output != "pipe:1" {
        a.push("-y".into());
    }
    a.push(output.to_string());
    a
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn opts() -> TranscodeOptions {
        TranscodeOptions {
            container: Container::Mp4,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: Some(2_000_000),
            audio_bitrate_bps: Some(128_000),
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        }
    }

    #[test]
    fn args_include_seek_and_duration_when_set() {
        let mut o = opts();
        o.start_position_ticks = 50_000_000; // 5 seconds in Jellyfin ticks
        o.duration_ticks = Some(100_000_000); // 10 seconds
        let a = build_args("/m/foo.mkv", &o);
        let joined = a.join(" ");
        assert!(joined.contains("-ss 5.000"), "{joined}");
        assert!(joined.contains("-t 10.000"), "{joined}");
        assert!(joined.contains("-i /m/foo.mkv"), "{joined}");
        assert!(joined.contains("-c:v libx264"), "{joined}");
        assert!(joined.contains("-c:a aac"), "{joined}");
        assert!(joined.contains("-f mp4"), "{joined}");
        assert!(joined.contains("pipe:1"));
    }

    #[test]
    fn copy_codecs_skip_bitrate() {
        let o = TranscodeOptions {
            container: Container::Mp4,
            video: Some(VideoCodec::Copy),
            audio: Some(AudioCodec::Copy),
            video_bitrate_bps: Some(2_000_000), // ignored when copy
            audio_bitrate_bps: Some(128_000),
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        let a = build_args("/m/x.mp4", &o);
        let joined = a.join(" ");
        assert!(joined.contains("-c:v copy"));
        assert!(joined.contains("-c:a copy"));
        assert!(!joined.contains("-b:v"));
        assert!(!joined.contains("-b:a"));
    }

    #[test]
    fn no_video_emits_vn() {
        let o = TranscodeOptions {
            container: Container::Mp3,
            video: None,
            audio: Some(AudioCodec::Mp3),
            video_bitrate_bps: None,
            audio_bitrate_bps: Some(192_000),
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
        };
        let a = build_args("/m/x.flac", &o);
        let joined = a.join(" ");
        assert!(joined.contains("-vn"));
        assert!(joined.contains("-c:a libmp3lame"));
    }

    #[test]
    fn audio_stream_index_emits_explicit_map() {
        let mut o = opts();
        o.audio_source_stream_index = Some(2);
        let a = build_args("/m/x.mkv", &o);
        let joined = a.join(" ");
        assert!(joined.contains("-map 0:v?"), "{joined}");
        assert!(joined.contains("-map 0:a:2"), "{joined}");
    }

    #[test]
    fn audio_stream_index_default_skips_map_clause() {
        let o = opts();
        let a = build_args("/m/x.mkv", &o);
        let joined = a.join(" ");
        assert!(!joined.contains("-map"), "{joined}");
    }

    #[test]
    fn burn_subtitle_appends_subtitles_filter() {
        let mut o = opts();
        o.burn_subtitle_stream_index = Some(3);
        let a = build_args("/m/x.mkv", &o);
        let joined = a.join(" ");
        assert!(
            joined.contains("-vf subtitles=filename='/m/x.mkv':si=3"),
            "{joined}"
        );
    }

    #[test]
    fn escape_subtitles_filename_quotes_and_protects_metachars() {
        // Plain path → wrapped in single quotes.
        assert_eq!(escape_subtitles_filename("/m/a.mkv"), "'/m/a.mkv'");
        // Filtergraph metachars `[ ] ; : ,` are literal inside quotes —
        // the old escaper missed `[ ] ;` entirely.
        assert_eq!(
            escape_subtitles_filename("/m/[Grp] T [1080p].mkv"),
            "'/m/[Grp] T [1080p].mkv'"
        );
        assert_eq!(escape_subtitles_filename("/m/a;b,c:d.mkv"), "'/m/a;b,c:d.mkv'");
        // Embedded single quote → ffmpeg `'\''` sequence.
        assert_eq!(escape_subtitles_filename("/m/it's.mkv"), "'/m/it'\\''s.mkv'");
        // Backslash doubled.
        assert_eq!(escape_subtitles_filename("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn burn_subtitle_with_seek_uses_output_seek_for_sync() {
        // Audit fix: burned subs must not desync under -ss. With a seek +
        // subtitle burn, -ss must appear AFTER -i (output seeking).
        let mut o = opts();
        o.start_position_ticks = 50_000_000; // 5s
        o.burn_subtitle_stream_index = Some(0);
        let a = build_args("/m/x.mkv", &o);
        let i_pos = a.iter().position(|x| x == "-i").unwrap();
        let ss_pos = a.iter().position(|x| x == "-ss").unwrap();
        assert!(ss_pos > i_pos, "expected output seek (-ss after -i): {a:?}");
    }

    #[test]
    fn seek_without_subs_uses_input_seek() {
        let mut o = opts();
        o.start_position_ticks = 50_000_000;
        let a = build_args("/m/x.mkv", &o);
        let i_pos = a.iter().position(|x| x == "-i").unwrap();
        let ss_pos = a.iter().position(|x| x == "-ss").unwrap();
        assert!(ss_pos < i_pos, "expected input seek (-ss before -i): {a:?}");
    }

    #[test]
    fn vaapi_device_emits_render_node_and_hwupload() {
        use crate::protocol::DeviceId;
        let o = opts(); // H264
        let a = build_args_for_device("/m/x.mkv", &o, DeviceId::hw(HwAccel::Vaapi, 1), "out.ts");
        let joined = a.join(" ");
        assert!(joined.contains("-vaapi_device /dev/dri/renderD129"), "{joined}");
        assert!(joined.contains("format=nv12,hwupload"), "{joined}");
        assert!(joined.contains("-c:v h264_vaapi"), "{joined}");
    }

    #[test]
    fn burn_subtitle_skipped_when_video_is_copy() {
        // Filtering needs re-encode; -vf with -c:v copy is a no-op
        // ffmpeg would error on. We deliberately don't add the filter
        // when video is in copy mode.
        let mut o = opts();
        o.video = Some(VideoCodec::Copy);
        o.burn_subtitle_stream_index = Some(0);
        let a = build_args("/m/x.mkv", &o);
        let joined = a.join(" ");
        assert!(!joined.contains("-vf"), "{joined}");
    }

    #[test]
    fn non_utf8_path_errors_cleanly() {
        // Synthesize a non-utf8 PathBuf only on Unix (windows uses
        // WTF-16 internally); skip on others.
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let raw = OsStr::from_bytes(b"/m/\xff\xfe.mkv");
            let p = std::path::Path::new(raw);
            let t = FfmpegTranscoder::new();
            let res = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(t.transcode(p, &opts()));
            assert!(matches!(res, Err(TranscodeError::NonUtf8Path)));
        }
    }
}
