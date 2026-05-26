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

pub mod options;

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
        }
    }

    pub fn with_binary(p: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg_bin: p.into(),
        }
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
        let args = build_args(input_str, opts);
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

fn build_args(input: &str, opts: &TranscodeOptions) -> Vec<String> {
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
    match opts.video {
        Some(VideoCodec::Copy) => {
            a.push("-c:v".into());
            a.push("copy".into());
        }
        Some(c) => {
            a.push("-c:v".into());
            a.push(c.ffmpeg_codec().into());
            if let Some(b) = opts.video_bitrate_bps {
                a.push("-b:v".into());
                a.push(format!("{b}"));
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
        };
        let a = build_args("/m/x.flac", &o);
        let joined = a.join(" ");
        assert!(joined.contains("-vn"));
        assert!(joined.contains("-c:a libmp3lame"));
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
            let res =
                tokio::runtime::Runtime::new().unwrap().block_on(t.transcode(p, &opts()));
            assert!(matches!(res, Err(TranscodeError::NonUtf8Path)));
        }
    }
}
