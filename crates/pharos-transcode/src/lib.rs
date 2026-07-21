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
pub mod capability;
pub mod device;
pub mod fingerprint;
pub mod hwaccel;
#[cfg(unix)]
pub mod libav;
pub mod options;
#[cfg(unix)]
pub mod probe;
pub mod protocol;
pub mod scheduler;
pub mod segment;
pub mod subwin;
#[cfg(unix)]
pub mod worker;

#[cfg(feature = "backend-lib")]
pub use backend::LibBackend;
#[cfg(feature = "backend-spawn")]
pub use backend::SpawnBackend;
pub use backend::{BackendError, FfmpegBackend, ProbeJson, SubtitleFormat, WaveformPoint};
pub use capability::{EncodeAccel, RelCost, ServerEncodeCapabilities, VideoEncodeCap};
pub use hwaccel::HwAccel;
pub use options::{AudioCodec, Container, TranscodeOptions, VideoCodec};
pub use segment::{SegmentAudio, SegmentContainer, SegmentOpts, SegmentVideo};

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
        let args =
            build_args_for_device(input_str, opts, hwaccel_to_device(self.hwaccel), "pipe:1");
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

/// Escape a filesystem path for use as an ffmpeg filtergraph option value —
/// the `subtitles=filename=…` argument of the text-burn chain.
///
/// ffmpeg parses a filter option value through TWO independent levels: the
/// filtergraph tokenizer (splits filters on `,`/`;`, options on `:`, pads on
/// `[`/`]`) AND the per-option value parser (which itself treats `'` as a quote
/// and `\` as an escape). A SINGLE level of escaping is therefore not enough:
/// an apostrophe in a real path (`Ocean's.mkv`) survives level 1, then the
/// option parser reads it as an opening quote and swallows the remainder of the
/// argument — `:si=N` included — while dropping the quote character itself
/// (verified live: the file `it's a.mkv` opened as `its a.mkv:si=0`). So each
/// special is escaped twice: once for the option-value layer, then again for
/// the filtergraph layer. Verified empirically in `tests/subtitle_burn_ass.rs`
/// / the Task-7 spike against a path holding a space, `[`, `]`, `,`, and `'`
/// (which produces e.g. `it\\\'s` and `\[x\]\,1`, both of which ffmpeg 8.1
/// accepts and opens correctly).
fn ffmpeg_filter_escape(path: &str) -> String {
    fn escape_layer(s: &str, specials: &[char]) -> String {
        let mut out = String::with_capacity(s.len() + 8);
        for c in s.chars() {
            if specials.contains(&c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }
    // Level 2 — the option-value parser: backslash, quote, and the `:` that
    // ends the option value.
    let l2 = escape_layer(path, &['\\', '\'', ':']);
    // Level 1 — the filtergraph tokenizer: backslash, quote, and the graph
    // delimiters. This re-escapes the backslashes level 2 introduced so they
    // survive to reach the option parser.
    escape_layer(&l2, &['\\', '\'', '[', ']', ',', ';'])
}

/// Emit the video filter arguments. Three shapes:
///
/// - No burn — a plain `-vf` chain (or nothing when the chain is empty).
/// - IMAGE subtitle burn (B40) — a `-filter_complex` overlaying the bitmap
///   subtitle stream onto the video before the rest of the chain:
///   `[0:v:0][0:s:N]overlay=eof_action=pass[,rest][vout]`. `overlay` is the
///   only ffmpeg path that renders bitmap subs (PGS/VOBSUB/DVB); libass'
///   `subtitles=` is text-only and aborts a PGS burn with "Only text based
///   subtitles are currently supported" (Avatar's PGS track 500'd every
///   segment). A `-filter_complex` disables default stream selection, so the
///   video (`[vout]`) and audio maps are emitted here.
/// - TEXT/ASS subtitle burn (Task 7) — a plain `-vf subtitles=filename=…:si=N`
///   chain. libass rasterizes the text/ASS events and auto-loads the source's
///   embedded attachment fonts. Because the `subtitles` filter opens a SECOND
///   demuxer at t=0 and renders by frame PTS, a mid-file segment (input-seek
///   `-ss START` before `-i`) needs its frame PTS lifted to ABSOLUTE time or
///   the wrong (from-zero) cue renders. `setpts=PTS+START/TB … setpts=PTS-…`
///   brackets the filter so it sees absolute PTS (correct cue) while the OUTPUT
///   stays zero-based — leaving `-t DUR` and the muxer's `-output_ts_offset`
///   timing untouched (`-copyts` instead would make output PTS absolute and
///   collapse `-t` to ~0 frames — verified in `tests/subtitle_burn_ass.rs`). A
///   plain `-vf` (single input) keeps ffmpeg default stream selection, so NO
///   maps are emitted here; the caller's normal audio-map path handles an
///   explicit audio track.
///
/// The `si=N` and `[0:s:N]` indices share the same subtitle-relative meaning.
// A private arg-marshalling helper for the burn filter chain; the parameters
// are all distinct scalars threaded straight from `TranscodeOptions`, so a
// wrapper struct would only add indirection.
#[allow(clippy::too_many_arguments)]
fn push_video_filters(
    a: &mut Vec<String>,
    vf_parts: &[String],
    burn_subtitle_stream_index: Option<u32>,
    burn_is_text: bool,
    input: &str,
    start_seconds: Option<f64>,
    audio_source_stream_index: Option<u32>,
    burn_ass_path: Option<&Path>,
    burn_fonts_dir: Option<&Path>,
) {
    match (burn_subtitle_stream_index, burn_is_text) {
        (Some(si), true) => {
            // Prefer the small pre-extracted `.ass` sidecar: ffmpeg's
            // `subtitles` filter opens a SECOND demuxer on `filename=` at init —
            // ONCE PER SEGMENT — and reads the WHOLE container to gather subtitle
            // packets + embedded fonts, so pointing it at the multi-GB NFS source
            // re-demuxes the entire file every 6 s segment (the documented
            // whole-file-demux stutter). The sidecar is a single-track `.ass`
            // (NO `si=`) whose events keep the source's ABSOLUTE times, so the
            // `setpts` sandwich below is unchanged. `:fontsdir=` hands libass the
            // extracted embedded fonts. When no sidecar was produced, fall back
            // to `filename=<source>:si=N` so a burn degrades (slower) not breaks.
            let subtitles = match burn_ass_path {
                Some(ass) => {
                    let esc = ffmpeg_filter_escape(&ass.to_string_lossy());
                    let mut s = format!("subtitles=filename={esc}");
                    if let Some(dir) = burn_fonts_dir {
                        let fesc = ffmpeg_filter_escape(&dir.to_string_lossy());
                        s.push_str(&format!(":fontsdir={fesc}"));
                    }
                    s
                }
                None => {
                    let esc = ffmpeg_filter_escape(input);
                    format!("subtitles=filename={esc}:si={si}")
                }
            };
            let mut chain: Vec<String> = Vec::new();
            match start_seconds {
                Some(start) if start > 0.0 => {
                    chain.push(format!("setpts=PTS+{start:.3}/TB"));
                    chain.push(subtitles);
                    chain.push(format!("setpts=PTS-{start:.3}/TB"));
                }
                _ => chain.push(subtitles),
            }
            // Any hardware-upload / format filters (VAAPI: format=nv12,hwupload)
            // chain AFTER the software rasterization.
            chain.extend(vf_parts.iter().cloned());
            a.push("-vf".into());
            a.push(chain.join(","));
        }
        (Some(si), false) => {
            let mut graph = format!("[0:v:0][0:s:{si}]overlay=eof_action=pass");
            if !vf_parts.is_empty() {
                graph.push(',');
                graph.push_str(&vf_parts.join(","));
            }
            graph.push_str("[vout]");
            a.push("-filter_complex".into());
            a.push(graph);
            a.push("-map".into());
            a.push("[vout]".into());
            a.push("-map".into());
            a.push(match audio_source_stream_index {
                Some(i) => format!("0:a:{i}"),
                None => "0:a:0?".into(),
            });
        }
        (None, _) => {
            if !vf_parts.is_empty() {
                a.push("-vf".into());
                a.push(vf_parts.join(","));
            }
        }
    }
}

#[cfg(test)]
fn build_args(input: &str, opts: &TranscodeOptions) -> Vec<String> {
    build_args_for_device(input, opts, crate::protocol::DeviceId::Cpu, "pipe:1")
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
    // B40 — image-subtitle burn-in rides `overlay` on the SAME single input
    // (`[0:v:0][0:s:N]overlay`), so input seeking (`-ss` before `-i`) moves
    // video and subtitle streams together and stays fast. The old text
    // `subtitles=` filter needed an output-side seek (it demuxed the file
    // from 0 — tens of seconds per segment deep into a movie) AND could not
    // render image subs at all: every PGS burn 500'd with "Only text based
    // subtitles are currently supported" while the guards upstream only ever
    // request burn for image subs.
    // Only the IMAGE burn rides an `overlay` filter_complex that owns the
    // video + audio maps. A TEXT/ASS burn (`burn_subtitle_is_text`) uses a
    // plain `-vf subtitles=` chain that keeps ffmpeg's default stream
    // selection, so it must NOT take the map-skip path below — it falls through
    // to the normal audio-map handling like any other re-encode.
    let burning_image_subs = opts.video.is_some()
        && !matches!(opts.video, Some(VideoCodec::Copy))
        && opts.burn_subtitle_stream_index.is_some()
        && !opts.burn_subtitle_is_text;
    // Bound DECODER threads for a real video transcode (input-side
    // `-threads`, before `-i`). ffmpeg otherwise frame-threads the decoder
    // across every logical core PER JOB, so N concurrent segment encodes
    // spawn N×cores decode threads and thrash each other — the same
    // oversubscription the encoder-side cap below prevents. A 4-thread
    // decode of 1080p 10-bit HEVC still runs well above realtime, and the
    // scheduler's permit budget (cores / threads-per-encode) only holds if
    // each job's total footprint actually stays near that many cores.
    // Copy/remux does no decode → skip; audio-only decode is trivial.
    if opts.video.is_some() && !matches!(opts.video, Some(VideoCodec::Copy)) {
        a.push("-threads".into());
        a.push(sw_encode_threads().to_string());
    }
    let start = opts.start_position_seconds();
    if let Some(pos) = start {
        a.push("-ss".into());
        a.push(format!("{pos:.3}"));
    }
    a.push("-i".into());
    a.push(input.to_string());
    if let Some(dur) = opts.duration_seconds() {
        a.push("-t".into());
        a.push(format!("{dur:.3}"));
    }
    // W1 — when the caller specifies an audio stream, route the video + the
    // chosen audio track explicitly. Map only the PRIMARY video (`0:v:0`), not
    // ALL video (`0:v?`): a source with a second video stream — an embedded
    // cover-art / attached-picture poster, common in anime releases — would
    // otherwise have BOTH mapped, and libvpx/x264 fails encoding the poster
    // ("[vf#0:1] Error sending frames to consumers: Invalid argument", -22),
    // so "at least one stream received no packets" and the whole segment 500s.
    // ffmpeg's default stream selection (taken when no audio index is given)
    // already picks a single primary video and excludes attached pictures,
    // which is why default playback works; this branch must match it. The `?`
    // keeps the map optional so an audio-only source still transcodes. `0:v:0`
    // also aligns with the probe, which advertises the first video as THE
    // video track. (B6 — surfaced once B4 made the audio index actually reach
    // the segment; before that the index was dropped and this path never ran.)
    if burning_image_subs {
        // B40 — the overlay filter_complex below owns the video map
        // (`-map [vout]`); adding `0:v:0?` here would map the source video a
        // second time. (Text-sub burn uses `-vf` + default selection, so it is
        // deliberately excluded from `burning_image_subs` and handled here.)
    } else if let Some(audio_idx) = opts.audio_source_stream_index {
        a.push("-map".into());
        a.push("0:v:0?".into());
        a.push("-map".into());
        a.push(format!("0:a:{audio_idx}"));
    }
    match opts.video {
        Some(VideoCodec::Copy) => {
            a.push("-c:v".into());
            a.push("copy".into());
        }
        Some(c) => {
            // Software filter chain that must run before any hardware
            // upload. Burn-in is NOT part of this chain: it needs a second
            // filter input (the subtitle stream) and therefore a
            // `-filter_complex`, emitted by `push_video_filters` below.
            let mut vf_parts: Vec<String> = Vec::new();
            let is_vaapi = matches!(
                device,
                DeviceId::Hw {
                    accel: HwAccel::Vaapi,
                    ..
                }
            );
            if is_vaapi && matches!(c, VideoCodec::H264 | VideoCodec::H265) {
                // VAAPI: upload frames to the GPU, then encode. The
                // upload filter chains after any software filters.
                vf_parts.push("format=nv12".into());
                vf_parts.push("hwupload".into());
                push_video_filters(
                    &mut a,
                    &vf_parts,
                    opts.burn_subtitle_stream_index,
                    opts.burn_subtitle_is_text,
                    input,
                    start,
                    opts.audio_source_stream_index,
                    opts.burn_subtitle_ass_path.as_deref(),
                    opts.burn_fonts_dir.as_deref(),
                );
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
                push_video_filters(
                    &mut a,
                    &vf_parts,
                    opts.burn_subtitle_stream_index,
                    opts.burn_subtitle_is_text,
                    input,
                    start,
                    opts.audio_source_stream_index,
                    opts.burn_subtitle_ass_path.as_deref(),
                    opts.burn_fonts_dir.as_deref(),
                );
                a.push("-c:v".into());
                a.push(encoder.into());
                // Force broadly-decodable 8-bit 4:2:0 output. A 10-bit
                // (yuv420p10le) or 4:4:4 source would otherwise carry its
                // pixel format into the H.264/HEVC stream, which most
                // clients — and the headless test chromium — can't decode.
                // ffmpeg inserts an auto-convert before the software /
                // NVENC / QSV / VideoToolbox encoder. VAAPI handles this
                // via the `format=nv12` filter above, so it's scoped to
                // the non-VAAPI encoders here.
                a.push("-pix_fmt".into());
                a.push("yuv420p".into());
                // NVENC GPU ordinal selection on a multi-GPU box. Only the
                // NVENC h264/hevc encoders accept `-gpu`; appending it for a
                // software codec (libvpx-vp9, which never runs on NVENC)
                // aborts ffmpeg with "Option gpu not found" → a 0-byte stream.
                if matches!(c, VideoCodec::H264 | VideoCodec::H265)
                    && matches!(
                        device,
                        DeviceId::Hw {
                            accel: HwAccel::Nvenc,
                            ..
                        }
                    )
                {
                    if let Some(idx) = device.index() {
                        a.push("-gpu".into());
                        a.push(idx.to_string());
                    }
                }
                if let Some(b) = opts.video_bitrate_bps {
                    a.push("-b:v".into());
                    a.push(format!("{b}"));
                }
                // Software x264/x265: default preset is `medium` — a
                // quality-first offline setting ~3-5× slower than realtime
                // needs, and it auto-threads across EVERY logical core per
                // job, so concurrent segment encodes (live + prefetch +
                // other viewers) all fight for all cores and each slows
                // below realtime (measured: 3.5-12 s per 6 s segment on the
                // 16-core box → player starves → "video hangs"). Match the
                // VP9 path's discipline — and Jellyfin's own default
                // (`veryfast`): speed-first preset + a bounded thread
                // footprint so `cores / threads` encodes genuinely run in
                // parallel, each at several× realtime. Scoped to the
                // software encoders ONLY: NVENC/QSV/VAAPI/VideoToolbox use
                // different preset vocabularies (`veryfast` aborts NVENC)
                // and manage their own parallelism on-device.
                if encoder == "libx264" || encoder == "libx265" {
                    a.push("-preset".into());
                    a.push("veryfast".into());
                    a.push("-threads".into());
                    a.push(sw_encode_threads().to_string());
                }
                // libvpx (VP9) defaults to a glacial good-quality multi-pass
                // encode — unusable for a live progressive transcode. Force
                // realtime + multithreaded row encoding so it keeps pace with
                // playback. Only the software libvpx encoder needs this (the
                // H.264/HEVC hw + x264 paths pace fine).
                if matches!(c, VideoCodec::Vp9) && encoder == "libvpx-vp9" {
                    // Benchmarked (scratchpad/vp9bench) for pharos's per-segment
                    // realtime transcode on a CPU-only box wanting several
                    // concurrent streams. A 6 s segment encodes in ~0.4-0.7 s, so
                    // the goal is not single-stream speed (already 10×+ realtime)
                    // but keeping the CPU footprint small so N concurrent streams
                    // don't oversubscribe the cores.
                    a.push("-deadline".into());
                    a.push("realtime".into());
                    // cpu-used 8 = fastest realtime. At streaming bitrates its
                    // SSIM matches cpu-used 7 within noise (0.9501 vs 0.9488),
                    // for less CPU — so max speed, no quality cost.
                    a.push("-cpu-used".into());
                    a.push("8".into());
                    a.push("-row-mt".into());
                    a.push("1".into());
                    // tile-columns 2 (up to 4 cols) parallelises enough for
                    // realtime while keeping FEWER tile boundaries than the old
                    // `4` — measured SSIM 0.9501 vs 0.9499 AND ~10% smaller, so
                    // it's actually higher quality-per-bit. Pairs with ~4 threads.
                    a.push("-tile-columns".into());
                    a.push("2".into());
                    // No `-frame-parallel`: deprecated in modern libvpx, measured
                    // zero speed benefit here, and it weakens inter-frame
                    // prediction (bigger files) + risks decoder-compat quirks.
                    a.push("-lag-in-frames".into());
                    a.push("0".into());
                    a.push("-threads".into());
                    a.push(sw_encode_threads().to_string());
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
            // libopus rejects a 5.1(side) source under the default mapping
            // family (-1) — "Invalid channel layout … for specified mapping
            // family -1" aborts the whole encode. Downmix to stereo: it's the
            // browser-safe layout for a progressive VP9/WebM stream anyway, and
            // avoids opus's finicky multichannel mapping entirely.
            // B45 — AAC gets the same stereo downmix: re-encoding a 5.1
            // source otherwise keeps 6 channels, and multichannel AAC is
            // undecodable in Firefox's MSE (fatal bufferAppendError on the
            // FIRST segment — playback never starts; Chrome tolerates it).
            if matches!(c, AudioCodec::Opus | AudioCodec::Aac) {
                a.push("-ac".into());
                a.push("2".into());
            }
        }
        None => {
            a.push("-an".into());
        }
    }
    // Never mux a subtitle into the transcoded AV container. pharos delivers
    // subtitles out-of-band (Stream.js / WebVTT / MediaAttachments), but
    // ffmpeg's default stream selection otherwise grabs a source subtitle and
    // writes it as a `mov_text` track — a spurious third track in every fMP4
    // segment that bloats the stream and can confuse hls.js's timeline. Burn-in
    // is unaffected: it reads the file directly via the `subtitles` filter, not
    // a mapped output stream.
    a.push("-sn".into());
    // fMP4 HLS segments are independent per-segment encodes that must tile on
    // ONE shared timeline (hls.js concatenates them under a single init
    // segment). Anchor each segment's timestamps to the SOURCE clock instead
    // of letting the muxer zero-base them — see the fMP4 muxer block below
    // for the mux-side half. `-enc_time_base 1:90000` stops libvpx/libx264
    // from quantizing timestamps to whole frame durations (the default
    // 1/framerate timebase rounds the zero-based first-frame pts to frame
    // index 0 or 1 semi-randomly per segment, gapping/duping the boundary
    // frame by ±1 frame). Encoder option → only when actually encoding video.
    let fmp4_segment = matches!(opts.container, Container::Fmp4);
    if fmp4_segment && opts.video.is_some() && !matches!(opts.video, Some(VideoCodec::Copy)) {
        a.push("-enc_time_base".into());
        a.push("1:90000".into());
    }
    a.push("-f".into());
    a.push(opts.container.ffmpeg_muxer().into());
    // `-movflags` is an mp4/mov-muxer option — fragmented MP4 for progressive
    // streaming. The webm/mpegts muxers reject it ("Unrecognized option"), so
    // scope it to the mp4-family muxers. WebM is inherently streamable (live
    // cluster writing). `Fmp4` needs the SAME fragmentation flags: the VP9-in-
    // HLS path (see api::jellyfin::fmp4) generates each segment as a self-
    // contained fragmented mp4 (`ftyp moov moof mdat`), then splits off the
    // init so per-segment output concatenates in hls.js.
    if matches!(opts.container, Container::Mp4 | Container::Fmp4) {
        a.push("-movflags".into());
        if fmp4_segment {
            // Source-anchored HLS segment (mux side): re-apply the seek
            // offset at the muxer so tfdt = the frame's TRUE source
            // timestamp. Consecutive segments then butt-join exactly and
            // audio/video share one clock — forcing a nominal 6.0 s tfdt
            // grid instead accumulates the per-segment video-vs-audio
            // content differential (~6 ms/segment) into audible A/V drift
            // over a full episode. `frag_discont` marks the runs as
            // intentionally discontinuous; `-avoid_negative_ts disabled`
            // stops the muxer from re-shifting the anchored timestamps
            // (opus preskip puts segment 0's first audio packet slightly
            // below zero — fmp4::process_segment clamps it).
            a.push("+empty_moov+frag_keyframe+default_base_moof+frag_discont".into());
            a.push("-avoid_negative_ts".into());
            a.push("disabled".into());
            a.push("-output_ts_offset".into());
            a.push(format!("{:.3}", start.unwrap_or(0.0)));
        } else {
            a.push("+empty_moov+frag_keyframe+default_base_moof".into());
        }
    }
    // B41 — mpegts HLS segments must ALSO carry their true timeline position.
    // Without the offset every `.ts` segment starts at PTS≈0, which works for
    // linear playback from 0 (hls.js re-anchors fragment-by-fragment) but
    // breaks a MID-TIMELINE start (subtitle/audio switch resumes at the
    // current position): hls.js buffers the fragment at its raw PTS near 0
    // while the playhead sits at the resume position → permanent stall, and
    // the eventual user retry restarts the movie from 0:00.
    // B45 — `-muxdelay 0` (+ preload) is REQUIRED for the offset to land on
    // the grid: the mpegts muxer otherwise adds its default 1.4 s initial
    // cue delay to every segment, so consecutive independently-transcoded
    // segments each carry a +1.4 s skew relative to their EXTINF position.
    // NOTE `-output_ts_offset` is inert under `-c:v copy` (ffmpeg 8.1) —
    // which is one of the reasons the segmented path never stream-copies
    // (see `build_segment_opts`); this block assumes a re-encode.
    if matches!(opts.container, Container::Mpegts) {
        a.push("-muxdelay".into());
        a.push("0".into());
        a.push("-muxpreload".into());
        a.push("0".into());
        if let Some(pos) = start {
            a.push("-output_ts_offset".into());
            a.push(format!("{pos:.3}"));
        }
    }
    // File-direct outputs are written by the worker; ffmpeg refuses to
    // overwrite an existing file without `-y`, and the scheduler hands
    // us a fresh `.tmp` path so clobbering is intended.
    if output != "pipe:1" {
        a.push("-y".into());
    }
    a.push(output.to_string());
    a
}

/// Thread budget for ONE software video encode/decode job (libvpx-vp9,
/// libx264, libx265, and the paired decoder). Benchmarked on VP9: a 1080p
/// realtime segment encodes in ~0.6 s at 4 threads (well above the 6 s
/// realtime budget), and MORE threads barely help single-stream latency
/// while their footprint sums across concurrent jobs. Cap at 4 so several
/// segments genuinely encode in parallel on a CPU-only box — the whole
/// "many small parallel encodes" design only works when
/// `threads-per-job × concurrent-jobs ≈ cores`; uncapped jobs each grab
/// every core and all of them slow below realtime together. Pairs with
/// [`device::default_cpu_permits`], which admits `cores / this` jobs.
/// Lower-core boxes scale down (min 2).
pub fn sw_encode_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
        .clamp(2, 4)
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
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
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
        // Software encode forces broad-compat 8-bit 4:2:0.
        assert!(joined.contains("-pix_fmt yuv420p"), "{joined}");
        assert!(joined.contains("-c:a aac"), "{joined}");
        assert!(joined.contains("-f mp4"), "{joined}");
        assert!(joined.contains("pipe:1"));
    }

    #[test]
    fn software_h264_gets_speed_preset_and_bounded_threads() {
        // The parallel-small-encodes design: each software job must be
        // speed-first (x264 default `medium` runs below realtime under
        // load) and thread-bounded (an uncapped encoder auto-threads over
        // every core, so `cores` admitted jobs thrash each other and ALL
        // slow below realtime — the measured 3.5-12 s per 6 s segment
        // phone-playback hang).
        let a = build_args("/m/foo.mkv", &opts());
        let joined = a.join(" ");
        assert!(joined.contains("-c:v libx264"), "{joined}");
        assert!(joined.contains("-preset veryfast"), "{joined}");
        let cap = sw_encode_threads().to_string();
        // Bounded on BOTH sides: decoder (input-side, before `-i`) and
        // encoder (output-side).
        let i_pos = a.iter().position(|s| s == "-i").expect("has -i");
        let thread_positions: Vec<usize> = a
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_str() == "-threads")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            thread_positions.len(),
            2,
            "decoder + encoder thread caps: {joined}"
        );
        assert!(
            thread_positions[0] < i_pos,
            "decoder cap must precede -i: {joined}"
        );
        assert!(
            thread_positions[1] > i_pos,
            "encoder cap follows -i: {joined}"
        );
        for p in thread_positions {
            assert_eq!(a[p + 1], cap, "{joined}");
        }
    }

    #[test]
    fn nvenc_h264_keeps_hw_preset_vocabulary() {
        // `-preset veryfast` is x264/x265 vocabulary; NVENC's presets are
        // p1-p7/hq/ll — passing veryfast aborts the encode. The speed
        // preset must stay scoped to the software encoders.
        let a = build_args_for_device(
            "/m/foo.mkv",
            &opts(),
            crate::protocol::DeviceId::hw(HwAccel::Nvenc, 0),
            "pipe:1",
        );
        let joined = a.join(" ");
        assert!(joined.contains("h264_nvenc"), "{joined}");
        assert!(!joined.contains("-preset veryfast"), "{joined}");
        // Software DECODE still feeds the GPU encoder here (no -hwaccel
        // input wiring), so the input-side decoder cap stays.
        let i_pos = a.iter().position(|s| s == "-i").expect("has -i");
        let dec_cap = a.iter().position(|s| s == "-threads").expect("dec cap");
        assert!(dec_cap < i_pos, "{joined}");
    }

    #[test]
    fn video_copy_skips_thread_caps() {
        // Remux does no video decode/encode — no thread caps to apply.
        let mut o = opts();
        o.video = Some(VideoCodec::Copy);
        let joined = build_args("/m/foo.mkv", &o).join(" ");
        assert!(joined.contains("-c:v copy"), "{joined}");
        assert!(!joined.contains("-threads"), "{joined}");
        assert!(!joined.contains("-preset"), "{joined}");
    }

    #[test]
    fn args_suppress_muxed_subtitles() {
        // pharos delivers subtitles out-of-band (Stream.js / WebVTT /
        // MediaAttachments), never muxed into the transcoded AV container.
        // Without `-sn`, ffmpeg's default stream selection picks up a source
        // subtitle and muxes it as a `mov_text` track — producing a 3-track
        // fMP4 (video + audio + text) that bloats every VP9 segment and can
        // confuse hls.js's timeline mapping. Proven against a live segment:
        // the deployed VP9 stream carried a spurious `text` track.
        let joined = build_args("/m/x.mkv", &opts()).join(" ");
        assert!(
            joined.contains(" -sn "),
            "must suppress muxed subtitles: {joined}"
        );
    }

    #[test]
    fn vp9_webm_args_are_realtime_and_skip_movflags() {
        let o = TranscodeOptions {
            container: Container::WebM,
            video: Some(VideoCodec::Vp9),
            audio: Some(AudioCodec::Opus),
            video_bitrate_bps: Some(2_000_000),
            audio_bitrate_bps: Some(128_000),
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        };
        let joined = build_args("/m/x.mkv", &o).join(" ");
        assert!(joined.contains("-c:v libvpx-vp9"), "{joined}");
        assert!(joined.contains("-c:a libopus"), "{joined}");
        // Opus downmixes to stereo — a 5.1(side) source otherwise aborts the
        // encode ("Invalid channel layout for mapping family -1").
        assert!(joined.contains("-ac 2"), "{joined}");
        assert!(joined.contains("-f webm"), "{joined}");
        // Realtime pacing is mandatory for a live libvpx encode.
        assert!(joined.contains("-deadline realtime"), "{joined}");
        assert!(joined.contains("-cpu-used 8"), "{joined}");
        assert!(joined.contains("-row-mt 1"), "{joined}");
        // Benchmark-tuned (scratchpad/vp9bench): tile-columns 2 (not 4) —
        // enough parallelism for realtime, fewer tile boundaries → equal/better
        // SSIM + smaller output; no `-frame-parallel` (deprecated, no benefit);
        // `-lag-in-frames 0` (realtime, no lookahead); a small thread cap so
        // concurrent streams don't oversubscribe the cores.
        assert!(joined.contains("-tile-columns 2"), "{joined}");
        assert!(!joined.contains("-tile-columns 4"), "{joined}");
        assert!(!joined.contains("-frame-parallel"), "{joined}");
        assert!(joined.contains("-lag-in-frames 0"), "{joined}");
        assert!(joined.contains("-threads "), "{joined}");
        // `-movflags` is mp4-only; the webm muxer rejects it.
        assert!(!joined.contains("-movflags"), "{joined}");
    }

    #[test]
    fn fmp4_segments_are_source_anchored() {
        // The VP9-in-HLS path generates each segment as an independent
        // `ffmpeg -ss N*6 -t 6` run. Left to its defaults the mp4 muxer
        // zero-bases every run, so segments only concatenate if tfdt is
        // rewritten onto a nominal 6.0 s grid afterwards — and forcing that
        // grid desyncs A/V progressively (video/audio real content ≈ 6.012 /
        // 6.006 s per segment). Instead, anchor every segment to the SOURCE
        // timeline at the muxer (`-output_ts_offset`), so consecutive
        // segments tile exactly and both tracks share one clock:
        // - `-enc_time_base 1:90000`: without it libvpx quantizes timestamps
        //   to whole frame durations, randomly dropping/gapping the boundary
        //   frame (±1 frame per segment).
        // - `-avoid_negative_ts disabled`: the mp4 muxer must not re-shift
        //   the anchored timestamps (opus preskip makes seg 0 slightly
        //   negative; fmp4.rs clamps that).
        // - `+frag_discont`: per-segment runs are discontinuous by design.
        let o = TranscodeOptions {
            container: Container::Fmp4,
            video: Some(VideoCodec::Vp9),
            audio: Some(AudioCodec::Opus),
            video_bitrate_bps: Some(2_000_000),
            audio_bitrate_bps: Some(128_000),
            start_position_ticks: 30 * 10_000_000, // segment 5 → 30 s
            duration_ticks: Some(6 * 10_000_000),
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
        };
        let joined = build_args("/m/x.mkv", &o).join(" ");
        assert!(joined.contains("-enc_time_base 1:90000"), "{joined}");
        assert!(
            joined.contains("-movflags +empty_moov+frag_keyframe+default_base_moof+frag_discont"),
            "{joined}"
        );
        assert!(joined.contains("-avoid_negative_ts disabled"), "{joined}");
        assert!(joined.contains("-output_ts_offset 30.000"), "{joined}");
    }

    #[test]
    fn progressive_mp4_stays_zero_based() {
        // Progressive (non-HLS) MP4 keeps ffmpeg's default zero-based
        // timestamps: the client plays a single stream from the requested
        // position and expects it to start at t≈0. Source-anchoring is a
        // per-segment HLS concern only.
        let mut o = opts(); // Container::Mp4
        o.start_position_ticks = 30 * 10_000_000;
        let joined = build_args("/m/x.mkv", &o).join(" ");
        assert!(!joined.contains("-output_ts_offset"), "{joined}");
        assert!(!joined.contains("-avoid_negative_ts"), "{joined}");
        assert!(!joined.contains("frag_discont"), "{joined}");
        assert!(!joined.contains("-enc_time_base"), "{joined}");
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
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
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
            burn_subtitle_is_text: false,
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
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
        // Only the PRIMARY video (`0:v:0`), never all video (`0:v?`): a source
        // with an embedded cover-art / attached-picture second video stream
        // (common in anime releases) would otherwise have BOTH mapped, and the
        // encoder chokes on the poster ("Error sending frames to consumers:
        // Invalid argument" → the whole segment 500s). ffmpeg's default
        // stream-selection (used when no audio track is chosen) already picks a
        // single primary video, which is why default playback works; the
        // explicit-map branch must match it. See B6.
        assert!(joined.contains("-map 0:v:0?"), "{joined}");
        assert!(!joined.contains("-map 0:v?"), "{joined}");
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
    fn burn_subtitle_uses_overlay_filter_complex() {
        // B40 — burn-in is only ever requested for IMAGE subs (PGS/VOBSUB),
        // which the text-only `subtitles=` filter cannot render ("Only text
        // based subtitles are currently supported" — every Avatar segment
        // 500'd). The burn must ride an overlay filter_complex with explicit
        // maps (a filter_complex disables default stream selection).
        let mut o = opts();
        o.burn_subtitle_stream_index = Some(3);
        let a = build_args("/m/x.mkv", &o);
        let joined = a.join(" ");
        assert!(
            joined.contains("-filter_complex [0:v:0][0:s:3]overlay=eof_action=pass[vout]"),
            "{joined}"
        );
        assert!(joined.contains("-map [vout]"), "{joined}");
        assert!(joined.contains("-map 0:a:0?"), "{joined}");
        assert!(!joined.contains("subtitles="), "{joined}");
        // Video must not ALSO be mapped from the source.
        assert!(!joined.contains("-map 0:v:0?"), "{joined}");
    }

    #[test]
    fn ffmpeg_filter_escape_double_escapes_filtergraph_specials() {
        // ffmpeg parses a filter option value through two levels (filtergraph
        // tokenizer + option-value parser), so specials need escaping twice.
        // A plain path is untouched.
        assert_eq!(ffmpeg_filter_escape("/m/plain.mkv"), "/m/plain.mkv");
        // An apostrophe (the common breakage — `Ocean's.mkv`) must reach libass
        // as a literal, so it emerges triple-backslashed + quote.
        assert_eq!(ffmpeg_filter_escape("a'b"), "a\\\\\\'b");
        // Filtergraph delimiters `[ ] ,` (release-tag paths like `[x],1`) get a
        // single backslash — they are inert at the option-value level.
        assert_eq!(ffmpeg_filter_escape("a[b],c"), "a\\[b\\]\\,c");
        // A literal `:` (rare on-disk, but legal) is escaped for the option
        // parser then its backslash survives level 1.
        assert_eq!(ffmpeg_filter_escape("a:b"), "a\\\\:b");
    }

    #[test]
    fn burn_text_subtitle_uses_subtitles_vf_with_setpts_alignment() {
        // Task 7 — a TEXT/ASS burn rasterizes via libass' `subtitles=` filter
        // (NOT overlay, which is bitmap-only). It is a plain `-vf` (single
        // input), so it keeps ffmpeg's default stream selection: no
        // `-filter_complex`, no `-map [vout]`. Because the filter renders by
        // frame PTS off a from-zero demuxer, a mid-file segment brackets it with
        // `setpts` so it sees ABSOLUTE time (right cue) while output stays
        // zero-based (verified in tests/subtitle_burn_ass.rs).
        let mut o = opts();
        o.start_position_ticks = 27 * 10_000_000; // 27 s segment start
        o.burn_subtitle_stream_index = Some(2);
        o.burn_subtitle_is_text = true;
        let a = build_args("/m/x.mkv", &o);
        let joined = a.join(" ");
        assert!(
            joined.contains(
                "-vf setpts=PTS+27.000/TB,subtitles=filename=/m/x.mkv:si=2,setpts=PTS-27.000/TB"
            ),
            "{joined}"
        );
        // Never the image overlay path for text.
        assert!(!joined.contains("-filter_complex"), "{joined}");
        assert!(!joined.contains("overlay="), "{joined}");
        assert!(!joined.contains("-map [vout]"), "{joined}");
        // Default stream selection handles audio here (no explicit index set).
        assert!(!joined.contains("-map "), "{joined}");
        // Still a real re-encode (filtering requires it).
        assert!(joined.contains("-c:v libx264"), "{joined}");
    }

    #[test]
    fn burn_text_subtitle_from_ass_sidecar_reads_sidecar_not_source() {
        // The fix — a TEXT/ASS burn with a pre-extracted `.ass` sidecar points
        // the `subtitles` filter at the SMALL local file (`filename=<ass>`, NO
        // `si=` — single-track sidecar) + the extracted `fontsdir`, NOT the
        // whole source (which ffmpeg would re-demux end-to-end once per HLS
        // segment). The `setpts` sandwich stays (sidecar keeps absolute times).
        let mut o = opts();
        o.start_position_ticks = 27 * 10_000_000;
        o.burn_subtitle_stream_index = Some(2);
        o.burn_subtitle_is_text = true;
        o.burn_subtitle_ass_path = Some(std::path::PathBuf::from("/cache/subs/x.ass"));
        o.burn_fonts_dir = Some(std::path::PathBuf::from("/cache/fonts/9"));
        let joined = build_args("/m/whole-source.mkv", &o).join(" ");
        assert!(
            joined.contains(
                "-vf setpts=PTS+27.000/TB,\
                 subtitles=filename=/cache/subs/x.ass:fontsdir=/cache/fonts/9,\
                 setpts=PTS-27.000/TB"
            ),
            "{joined}"
        );
        // Must NOT re-demux the source: no `filename=<source>` and no `si=`.
        assert!(!joined.contains("whole-source.mkv:si="), "{joined}");
        assert!(!joined.contains("si=2"), "{joined}");
    }

    #[test]
    fn burn_text_subtitle_sidecar_without_fonts_omits_fontsdir() {
        // No embedded fonts → no `:fontsdir=`; libass falls back to defaults.
        let mut o = opts();
        o.burn_subtitle_stream_index = Some(0);
        o.burn_subtitle_is_text = true;
        o.burn_subtitle_ass_path = Some(std::path::PathBuf::from("/cache/subs/y.ass"));
        let joined = build_args("/m/x.mkv", &o).join(" ");
        assert!(
            joined.contains("-vf subtitles=filename=/cache/subs/y.ass"),
            "{joined}"
        );
        assert!(!joined.contains("fontsdir"), "{joined}");
        assert!(!joined.contains(":si="), "{joined}");
    }

    #[test]
    fn burn_text_subtitle_without_sidecar_falls_back_to_source_si() {
        // No sidecar produced (e.g. memory-only cache / extraction failed) →
        // the burn degrades to the source-file form so it still works, just
        // slower. This is the safety net the resolver relies on.
        let mut o = opts();
        o.burn_subtitle_stream_index = Some(2);
        o.burn_subtitle_is_text = true;
        o.burn_subtitle_ass_path = None;
        let joined = build_args("/m/x.mkv", &o).join(" ");
        assert!(
            joined.contains("-vf subtitles=filename=/m/x.mkv:si=2"),
            "{joined}"
        );
    }

    #[test]
    fn burn_text_subtitle_at_zero_skips_setpts() {
        // A segment starting at 0 (or a progressive stream from the top) has no
        // absolute-time offset, so the setpts bracket is omitted — plain
        // `subtitles=`.
        let mut o = opts();
        o.burn_subtitle_stream_index = Some(0);
        o.burn_subtitle_is_text = true;
        let joined = build_args("/m/x.mkv", &o).join(" ");
        assert!(
            joined.contains("-vf subtitles=filename=/m/x.mkv:si=0"),
            "{joined}"
        );
        assert!(!joined.contains("setpts"), "{joined}");
    }

    #[test]
    fn burn_text_subtitle_with_audio_index_maps_that_track() {
        // Text burn keeps default selection UNLESS an explicit audio track is
        // requested, in which case the normal map path (not the overlay path)
        // routes video + the chosen audio.
        let mut o = opts();
        o.burn_subtitle_stream_index = Some(0);
        o.burn_subtitle_is_text = true;
        o.audio_source_stream_index = Some(3);
        let joined = build_args("/m/x.mkv", &o).join(" ");
        assert!(joined.contains("-map 0:v:0?"), "{joined}");
        assert!(joined.contains("-map 0:a:3"), "{joined}");
        assert!(joined.contains("subtitles=filename="), "{joined}");
        assert!(!joined.contains("-filter_complex"), "{joined}");
    }

    #[test]
    fn burn_subtitle_with_explicit_audio_maps_that_track() {
        let mut o = opts();
        o.burn_subtitle_stream_index = Some(1);
        o.audio_source_stream_index = Some(2);
        let a = build_args("/m/x.mkv", &o);
        let joined = a.join(" ");
        assert!(joined.contains("-map [vout]"), "{joined}");
        assert!(joined.contains("-map 0:a:2"), "{joined}");
        assert!(!joined.contains("-map 0:v:0?"), "{joined}");
    }

    #[test]
    fn burn_subtitle_with_seek_uses_input_seek() {
        // B40 — overlay reads the subtitle stream from the SAME input, so an
        // input-side seek moves video + subs together AND stays fast (the old
        // text-filter burn forced output seeking = decode-from-0, tens of
        // seconds per segment deep into a movie).
        let mut o = opts();
        o.start_position_ticks = 50_000_000; // 5s
        o.burn_subtitle_stream_index = Some(0);
        let a = build_args("/m/x.mkv", &o);
        let i_pos = a.iter().position(|x| x == "-i").unwrap();
        let ss_pos = a.iter().position(|x| x == "-ss").unwrap();
        assert!(ss_pos < i_pos, "expected input seek (-ss before -i): {a:?}");
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
        assert!(
            joined.contains("-vaapi_device /dev/dri/renderD129"),
            "{joined}"
        );
        assert!(joined.contains("format=nv12,hwupload"), "{joined}");
        assert!(joined.contains("-c:v h264_vaapi"), "{joined}");
        // VAAPI sets the format via the filter (nv12 in GPU memory), not a
        // software `-pix_fmt` (which would clash with the hw frames).
        assert!(!joined.contains("-pix_fmt"), "{joined}");
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

    #[test]
    fn mpegts_segment_carries_output_ts_offset() {
        // B41 — every `.ts` segment must carry its true timeline position:
        // PTS≈0 segments stall any mid-timeline start (subtitle/audio switch
        // resume), because hls.js buffers them near 0 while the playhead sits
        // at the resume position.
        let mut o = opts();
        o.container = Container::Mpegts;
        o.start_position_ticks = 300_000_000; // 30s
        let a = build_args("/m/x.mkv", &o);
        let joined = a.join(" ");
        assert!(joined.contains("-output_ts_offset 30.000"), "{joined}");
    }

    #[test]
    fn mpegts_segment_at_zero_has_no_offset() {
        let mut o = opts();
        o.container = Container::Mpegts;
        let a = build_args("/m/x.mkv", &o);
        assert!(!a.iter().any(|x| x == "-output_ts_offset"), "{a:?}");
    }

    #[test]
    fn mpegts_zeroes_the_mux_delay() {
        // B45 — without `-muxdelay 0` the mpegts muxer adds its default
        // 1.4 s initial cue delay, skewing every independently-transcoded
        // segment +1.4 s off its EXTINF grid position (the offset then
        // anchors to the wrong base).
        let mut o = opts();
        o.container = Container::Mpegts;
        let a = build_args("/m/x.mkv", &o);
        let joined = a.join(" ");
        assert!(joined.contains("-muxdelay 0"), "{joined}");
        assert!(joined.contains("-muxpreload 0"), "{joined}");
    }

    #[test]
    fn aac_downmixes_to_stereo() {
        // B45 — a 5.1 source re-encoded to AAC otherwise keeps 6 channels;
        // multichannel AAC is undecodable in Firefox's MSE (fatal append
        // error on the first segment — playback never starts).
        let a = build_args("/m/x.mkv", &opts());
        let joined = a.join(" ");
        assert!(joined.contains("-c:a aac"), "{joined}");
        assert!(joined.contains("-ac 2"), "{joined}");
    }
}
