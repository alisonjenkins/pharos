//! Wire protocol between the in-process `TranscodeScheduler` and its
//! out-of-process FFI workers.
//!
//! Framing: a 4-byte little-endian length prefix followed by a
//! `bincode`-encoded body. Frames carry **control only** — job specs,
//! progress, completion. The encoded *media bytes* never travel over
//! this channel; they land in an `OutputSink` (a file the worker writes
//! directly, or a pipe whose write-end fd is handed to the worker via
//! `SCM_RIGHTS`). Keeping media off the control channel means a slow
//! consumer of media can never stall heartbeats and trigger a false
//! "worker dead" kill.
//!
//! The wire types live here (not in `device.rs`) because they cross the
//! process boundary; `DeviceId` is a wire identity. The runtime
//! `DeviceTable` (which owns `Semaphore`s and is not serialisable) lives
//! in `device.rs`.

use crate::hwaccel::HwAccel;
use crate::options::TranscodeOptions;
use pharos_core::ProbeInfo;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Control frames are tiny (a `JobSpec` is a path + options). Cap well
/// below anything a media payload would need so a desynchronised or
/// hostile peer can't make us allocate unbounded memory off a forged
/// length prefix.
pub const MAX_FRAME: u32 = 1 << 20; // 1 MiB

/// Monotonic per-scheduler job identifier. Wraps a u64 so it is `Copy`
/// and cheap to thread through messages + maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job-{}", self.0)
    }
}

/// Per-scheduler worker identifier. Stable across a worker's lifetime;
/// a respawned worker gets a fresh id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkerId(pub u64);

impl std::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "worker-{}", self.0)
    }
}

/// A transcode device the scheduler can dispatch to. `Cpu` is always
/// present as the terminal fallback (software encode); `Hw` names a
/// resolved hardware encoder family **plus a GPU ordinal** (`index`) so a
/// multi-GPU box load-balances across each card independently. `index`
/// maps to a DRM render node for VAAPI (`/dev/dri/renderD{128+index}`)
/// and to the encoder GPU ordinal for NVENC (`-gpu {index}`). `accel` is
/// never `HwAccel::Auto`/`Off` — those resolve away before a `DeviceId`
/// is formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceId {
    Cpu,
    Hw { accel: HwAccel, index: u8 },
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceId::Cpu => write!(f, "cpu"),
            DeviceId::Hw { accel, index } => write!(f, "{accel:?}:{index}"),
        }
    }
}

impl DeviceId {
    /// Convenience constructor for a hardware device.
    pub fn hw(accel: HwAccel, index: u8) -> Self {
        DeviceId::Hw { accel, index }
    }

    /// The `HwAccel` a worker should configure when encoding on this
    /// device. `Cpu` → software (`Off`); a hardware device → its family.
    pub fn hwaccel(self) -> HwAccel {
        match self {
            DeviceId::Cpu => HwAccel::Off,
            DeviceId::Hw { accel, .. } => accel,
        }
    }

    /// GPU ordinal for hardware devices (`None` for CPU).
    pub fn index(self) -> Option<u8> {
        match self {
            DeviceId::Cpu => None,
            DeviceId::Hw { index, .. } => Some(index),
        }
    }

    /// DRM render-node path for a VAAPI device (`/dev/dri/renderD{128+index}`).
    /// `None` for non-VAAPI devices.
    pub fn vaapi_render_node(self) -> Option<String> {
        match self {
            DeviceId::Hw {
                accel: HwAccel::Vaapi,
                index,
            } => Some(format!("/dev/dri/renderD{}", 128 + index as u32)),
            _ => None,
        }
    }
}

/// Where the worker writes the encoded output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OutputSink {
    /// Cached-segment path: the worker mux-writes straight to `path`
    /// (`.tmp`), the scheduler renames to the final name on `Done`. No
    /// cross-process byte copy.
    FileDirect { path: PathBuf },
    /// Live path: the worker writes the muxed stream to its own stdout,
    /// which the spawner connected to an OS pipe the main process reads
    /// and forwards to the HTTP client. The fd is "passed" by stdout
    /// inheritance (main↔worker pipe, worker→ffmpeg inherit) — no
    /// userspace copy in the worker; bytes flow ffmpeg → pipe → client.
    Stdout,
}

/// One unit of transcode work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub job_id: JobId,
    pub input: PathBuf,
    pub opts: TranscodeOptions,
    /// Scheduler's chosen device. The worker opens exactly this device
    /// (or fails with `DeviceBusy` so the scheduler can retry elsewhere).
    pub device: DeviceId,
    pub sink: OutputSink,
}

/// One in-process libav "tiny op" — the high-frequency ffmpeg/ffprobe
/// calls that the persistent libav worker services without a per-call
/// fork. Each carries the `JobId` it replies under. The encoded outputs
/// of `Image`/`Trickplay`/`Subtitle` land on disk (the worker writes them
/// directly, same as the segment sink) and reply `Done`; `Probe`/
/// `Waveform` return their (small) data inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TinyOp {
    /// Probe a media file → `WorkerEvent::ProbeResult`.
    Probe { input: PathBuf },
    /// Extract one scaled JPEG frame to `out` → `Done`.
    Image {
        input: PathBuf,
        seek_ms: Option<u64>,
        width: u32,
        quality: i32,
        out: PathBuf,
    },
    /// Extract an embedded attachment stream (a font) to `out` -> `Done`.
    ExtractAttachment {
        input: PathBuf,
        stream_index: u32,
        out: PathBuf,
    },
    /// Dump EVERY embedded attachment (font) to `out_dir/{stream_index}` in a
    /// single source open → `Done` (`out_bytes` carries the count). ASS
    /// subtitles reference many fonts; extracting them one-by-one re-opens the
    /// (NFS, multi-GB) source per font and stalls SubtitlesOctopus.
    ExtractAllAttachments { input: PathBuf, out_dir: PathBuf },
    /// Generate trickplay sprite sheets into `out_dir` (0-based `{i}.jpg`)
    /// → `Done` (`out_bytes` carries the sheet count).
    Trickplay {
        input: PathBuf,
        interval_ms: u64,
        width: u32,
        grid: u32,
        /// Total thumbnails to sample (`ceil(duration/interval)`). The worker
        /// seeks to `i·interval_ms` for `i in 0..thumb_count`.
        thumb_count: u32,
        max_sheets: u32,
        quality: i32,
        out_dir: PathBuf,
    },
    /// Convert a SubRip sidecar to WebVTT, written to `out` → `Done`.
    SrtToWebvtt { input: PathBuf, out: PathBuf },
    /// Audio RMS waveform → `WorkerEvent::WaveformResult`.
    Waveform {
        input: PathBuf,
        samples_per_bin: u64,
        target_bins: u32,
    },
}

/// Scheduler → worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerCmd {
    Job(JobSpec),
    /// A persistent libav-worker request/reply op (probe, image, …).
    Tiny {
        job_id: JobId,
        op: TinyOp,
    },
    Cancel {
        job_id: JobId,
    },
    Shutdown,
}

/// Worker → scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerEvent {
    /// First frame after spawn — worker advertises what it linked + can
    /// actually open. The scheduler validates before trusting it.
    Hello(Handshake),
    Accepted {
        job_id: JobId,
    },
    /// Periodic during a job; doubles as a heartbeat so the scheduler can
    /// distinguish "slow" from "hung/dead".
    Progress {
        job_id: JobId,
        out_bytes: u64,
        frames: u64,
    },
    Done {
        job_id: JobId,
        out_bytes: u64,
    },
    Failed {
        job_id: JobId,
        error: WorkerError,
    },
    /// Reply to `TinyOp::Probe`.
    ProbeResult {
        job_id: JobId,
        info: Box<ProbeInfo>,
    },
    /// Reply to `TinyOp::Waveform` — per-bin RMS dBFS.
    WaveformResult {
        job_id: JobId,
        bins: Vec<f32>,
    },
}

/// What the worker reports about its build + capabilities at handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handshake {
    /// e.g. libav versions, or `"spawn-stub"` for the ffmpeg-binary
    /// fallback worker used in spawn-feature builds + tests.
    pub backend: String,
    /// Devices this worker could actually open (probed `av_hwdevice_ctx_create`
    /// for the FFI worker; `which ffmpeg` + `-hwaccels` for the stub).
    pub openable_devices: smallvec::SmallVec<[DeviceId; 4]>,
}

/// Error classes the scheduler routes on. **Transient** ⇒ cooldown the
/// device + retry on next-best (down to CPU). **Non-recoverable** ⇒
/// fail the job's reply, log, never touch scheduler health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerError {
    /// Transient: device out of encode sessions / `EAGAIN` opening the
    /// hw context. Retry elsewhere.
    DeviceBusy,
    /// Non-recoverable: target codec not encodable by this build. Carries
    /// the underlying reason (which codec / why) so the log is actionable.
    UnsupportedCodec(String),
    /// Non-recoverable: source is malformed / undecodable. Carries the
    /// underlying libav/IO reason (e.g. `open: Invalid data found`) rather
    /// than collapsing every distinct cause to a bare "bad input".
    BadInput(String),
    /// I/O against the sink or input failed.
    Io(String),
    /// Anything else, carrying a human string for the log.
    Other(String),
}

impl WorkerError {
    /// True when the scheduler should retry this job on another device.
    pub fn is_transient(&self) -> bool {
        matches!(self, WorkerError::DeviceBusy)
    }
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerError::DeviceBusy => write!(f, "device busy (out of encode sessions)"),
            WorkerError::UnsupportedCodec(s) => write!(f, "unsupported codec: {s}"),
            WorkerError::BadInput(s) => write!(f, "bad input: {s}"),
            WorkerError::Io(s) => write!(f, "io: {s}"),
            WorkerError::Other(s) => write!(f, "{s}"),
        }
    }
}

/// Frame I/O errors. Kept separate from `WorkerError` (which is a wire
/// payload) — these are channel-level failures.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large: {0} bytes (max {MAX_FRAME})")]
    TooLarge(u32),
    #[error("encode: {0}")]
    Encode(String),
    #[error("decode: {0}")]
    Decode(String),
}

/// Write one length-prefixed bincode frame. Flushes so the peer sees it
/// promptly (control frames are latency-sensitive, not throughput).
pub async fn write_frame<W, T>(w: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = bincode::serialize(value).map_err(|e| FrameError::Encode(e.to_string()))?;
    let len = body.len();
    if len as u64 > MAX_FRAME as u64 {
        return Err(FrameError::TooLarge(len as u32));
    }
    w.write_all(&(len as u32).to_le_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-prefixed bincode frame. Returns `Ok(None)` on a clean
/// EOF at a frame boundary — the primary "peer is gone" signal. A
/// partial frame (EOF mid-body) is an `Io(UnexpectedEof)` error.
pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    let value = bincode::deserialize(&body).map_err(|e| FrameError::Decode(e.to_string()))?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::options::{AudioCodec, Container, VideoCodec};

    fn sample_job() -> JobSpec {
        JobSpec {
            job_id: JobId(7),
            input: PathBuf::from("/m/in.mkv"),
            opts: TranscodeOptions {
                container: Container::Mpegts,
                video: Some(VideoCodec::H264),
                audio: Some(AudioCodec::Aac),
                video_bitrate_bps: Some(2_000_000),
                audio_bitrate_bps: Some(128_000),
                start_position_ticks: 60_000_000,
                duration_ticks: Some(60_000_000),
                audio_source_stream_index: Some(1),
                burn_subtitle_stream_index: None,
            },
            device: DeviceId::hw(HwAccel::Nvenc, 0),
            sink: OutputSink::FileDirect {
                path: PathBuf::from("/cache/1/0.tmp"),
            },
        }
    }

    #[tokio::test]
    async fn cmd_roundtrips_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let cmd = WorkerCmd::Job(sample_job());
        write_frame(&mut a, &cmd).await.unwrap();
        let got: WorkerCmd = read_frame(&mut b).await.unwrap().unwrap();
        match got {
            WorkerCmd::Job(j) => {
                assert_eq!(j.job_id, JobId(7));
                assert_eq!(j.device, DeviceId::hw(HwAccel::Nvenc, 0));
                assert_eq!(j.opts.start_position_ticks, 60_000_000);
                assert_eq!(j.opts.audio_source_stream_index, Some(1));
            }
            other => panic!("expected Job, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn event_variants_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let events = vec![
            WorkerEvent::Hello(Handshake {
                backend: "spawn-stub".into(),
                openable_devices: smallvec::smallvec![
                    DeviceId::Cpu,
                    DeviceId::hw(HwAccel::Vaapi, 0)
                ],
            }),
            WorkerEvent::Accepted { job_id: JobId(1) },
            WorkerEvent::Progress {
                job_id: JobId(1),
                out_bytes: 4096,
                frames: 12,
            },
            WorkerEvent::Done {
                job_id: JobId(1),
                out_bytes: 8192,
            },
            WorkerEvent::Failed {
                job_id: JobId(2),
                error: WorkerError::DeviceBusy,
            },
        ];
        for e in &events {
            write_frame(&mut a, e).await.unwrap();
        }
        for expected in &events {
            let got: WorkerEvent = read_frame(&mut b).await.unwrap().unwrap();
            // bincode is deterministic; compare debug strings to avoid
            // requiring PartialEq on the whole event tree.
            assert_eq!(format!("{got:?}"), format!("{expected:?}"));
        }
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (a, mut b) = tokio::io::duplex(1024);
        drop(a); // close the write side at a frame boundary
        let got: Result<Option<WorkerCmd>, _> = read_frame(&mut b).await;
        assert!(matches!(got, Ok(None)));
    }

    #[tokio::test]
    async fn oversize_length_prefix_rejected() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        // Forge a length prefix above MAX_FRAME with no body.
        a.write_all(&(MAX_FRAME + 1).to_le_bytes()).await.unwrap();
        a.flush().await.unwrap();
        let got: Result<Option<WorkerCmd>, _> = read_frame(&mut b).await;
        assert!(matches!(got, Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn transient_classification() {
        assert!(WorkerError::DeviceBusy.is_transient());
        assert!(!WorkerError::UnsupportedCodec(String::new()).is_transient());
        assert!(!WorkerError::BadInput(String::new()).is_transient());
    }
}
