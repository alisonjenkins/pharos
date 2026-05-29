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

pub mod hwaccel;
pub mod options;

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
        let args = build_args_with_hwaccel(input_str, opts, self.hwaccel);
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

/// Escape a path for the ffmpeg `subtitles=` filter graph. Filter args
/// are colon-separated key/value pairs, so the path must escape `\`,
/// `'`, `:` and `,` to keep ffmpeg's parser from misreading the rest
/// of the filter chain.
fn escape_subtitles_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 8);
    for c in p.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            ':' => out.push_str("\\:"),
            ',' => out.push_str("\\,"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
fn build_args(input: &str, opts: &TranscodeOptions) -> Vec<String> {
    build_args_with_hwaccel(input, opts, HwAccel::Off)
}

fn build_args_with_hwaccel(input: &str, opts: &TranscodeOptions, hwaccel: HwAccel) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    if let Some(pos) = opts.start_position_seconds() {
        a.push("-ss".into());
        a.push(format!("{pos:.3}"));
    }
    a.push("-i".into());
    a.push(input.to_string());
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
            a.push("-c:v".into());
            // P14 — swap libx264/libx265 for the platform hw encoder
            // when the transcoder was wired with one. Other codecs
            // (no hw mapping) fall through to the software default.
            let encoder = match c {
                VideoCodec::H264 => hwaccel.h264_encoder().unwrap_or(c.ffmpeg_codec()),
                VideoCodec::H265 => hwaccel.hevc_encoder().unwrap_or(c.ffmpeg_codec()),
                _ => c.ffmpeg_codec(),
            };
            a.push(encoder.into());
            if let Some(b) = opts.video_bitrate_bps {
                a.push("-b:v".into());
                a.push(format!("{b}"));
            }
            // W2 — burn the chosen subtitle stream into the video
            // frames via the `subtitles` filter. Skipped on `Copy`
            // (filtering requires re-encode) and on `None` (no video).
            if let Some(sub_idx) = opts.burn_subtitle_stream_index {
                a.push("-vf".into());
                a.push(format!(
                    "subtitles={}:si={sub_idx}",
                    escape_subtitles_path(input)
                ));
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
    a.push("pipe:1".into());
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
        assert!(joined.contains("-vf subtitles=/m/x.mkv:si=3"), "{joined}");
    }

    #[test]
    fn escape_subtitles_path_protects_filter_metachars() {
        assert_eq!(escape_subtitles_path("/m/a.mkv"), "/m/a.mkv");
        assert_eq!(escape_subtitles_path("/m/it's.mkv"), "/m/it\\'s.mkv");
        assert_eq!(escape_subtitles_path("/m/a:b.mkv"), "/m/a\\:b.mkv");
        assert_eq!(escape_subtitles_path("a\\b"), "a\\\\b");
        assert_eq!(escape_subtitles_path("a,b"), "a\\,b");
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
