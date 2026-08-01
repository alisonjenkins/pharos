//! Boot-time auto-probe of real per-device encode-session caps.
//!
//! Hardware encoders have concurrent-session limits ffmpeg doesn't
//! report (consumer NVENC caps at a handful; VAAPI is bounded by GPU
//! memory). We learn the real number by ramping concurrent trivial
//! encodes on each device until a level fails: the highest level where
//! every concurrent encode succeeded is the cap.
//!
//! The ramp ([`probe_caps`]) is generic over an `attempt` closure so it's
//! unit-testable with a fake; [`probe_device_caps`] wires the real
//! ffmpeg-backed attempt.
//!
//! Accepted fragility (the cost of probing): consumer caps are
//! driver-version-dependent, and probing momentarily holds N sessions, so
//! probing a box already under encode load under-reports. Probe at boot
//! before serving traffic; a config override exists for known hardware.

use crate::protocol::DeviceId;
use crate::worker::exec::ffmpeg_bin;
use smallvec::SmallVec;
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    /// Hard ceiling per device — stops a device that never fails (e.g. a
    /// server card) from probing forever.
    pub max_attempts: usize,
    /// Timeout for a single trial encode (catches a hung probe).
    pub per_attempt_timeout: Duration,
    /// Total wall-clock budget across all devices; bail with what we
    /// learned so far when exceeded.
    pub overall_timeout: Duration,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            max_attempts: 8,
            per_attempt_timeout: Duration::from_secs(5),
            overall_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProbedCaps {
    /// `(device, max concurrent sessions)`. Devices that couldn't run
    /// even one encode are omitted.
    pub caps: SmallVec<[(DeviceId, usize); 5]>,
}

/// Ramp concurrent attempts per device until a level fails. `attempt`
/// runs one trial encode on a device and reports success. Returns the
/// highest concurrency where *every* simultaneous attempt succeeded.
pub async fn probe_caps<F, Fut>(devices: &[DeviceId], cfg: &ProbeConfig, attempt: F) -> ProbedCaps
where
    F: Fn(DeviceId) -> Fut,
    Fut: Future<Output = bool>,
{
    let start = Instant::now();
    let mut caps: SmallVec<[(DeviceId, usize); 5]> = SmallVec::new();
    for &device in devices {
        let mut cap = 0usize;
        for n in 1..=cfg.max_attempts {
            if start.elapsed() >= cfg.overall_timeout {
                break;
            }
            // Launch n attempts concurrently; all must succeed for this
            // level to count.
            let mut futs = Vec::with_capacity(n);
            for _ in 0..n {
                futs.push(attempt(device));
            }
            let results = futures_join_all(futs).await;
            if results.iter().all(|ok| *ok) {
                cap = n;
            } else {
                break;
            }
        }
        if cap > 0 {
            caps.push((device, cap));
        }
    }
    ProbedCaps { caps }
}

/// Minimal `join_all` over a Vec of futures without pulling `futures`
/// into the non-dev dep set. Polls all to completion, preserving order.
async fn futures_join_all<Fut: Future<Output = bool>>(futs: Vec<Fut>) -> Vec<bool> {
    // Pin each and poll round-robin via a tiny join. tokio::join! needs a
    // fixed arity, so hand-roll with FuturesUnordered-free logic: spawn
    // is overkill (futures aren't Send-bound), so poll sequentially —
    // they're already running concurrently at the ffmpeg/process level
    // because each `attempt` spawns its own subprocess before its first
    // await returns is NOT guaranteed; to get true concurrency we drive
    // them together.
    use std::pin::Pin;
    use std::task::{Context, Poll};
    let mut pinned: Vec<(Pin<Box<Fut>>, Option<bool>)> =
        futs.into_iter().map(|f| (Box::pin(f), None)).collect();
    std::future::poll_fn(move |cx: &mut Context<'_>| {
        let mut all_done = true;
        for (fut, out) in pinned.iter_mut() {
            if out.is_none() {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(v) => *out = Some(v),
                    Poll::Pending => all_done = false,
                }
            }
        }
        if all_done {
            Poll::Ready(
                pinned
                    .iter_mut()
                    .map(|(_, o)| o.take().unwrap_or(false))
                    .collect(),
            )
        } else {
            Poll::Pending
        }
    })
    .await
}

/// Probe the real session cap for each detected device via trial ffmpeg
/// encodes. CPU is not probed here (its budget is the core count).
pub async fn probe_device_caps(devices: &[DeviceId], cfg: &ProbeConfig) -> ProbedCaps {
    let bin = ffmpeg_bin();
    let per = cfg.per_attempt_timeout;
    probe_caps(devices, cfg, |device| {
        let bin = bin.clone();
        async move { ffmpeg_probe_attempt(device, &bin, per).await }
    })
    .await
}

/// Run one trivial encode on `device`; `true` on success. A failure
/// (nonzero exit / timeout) means the device couldn't take another
/// concurrent session.
async fn ffmpeg_probe_attempt(device: DeviceId, ffmpeg_bin: &str, timeout: Duration) -> bool {
    run_ffmpeg_probe(ffmpeg_bin, &probe_args(device), timeout).await
}

/// Spawn a throwaway ffmpeg with `args` and report whether it exited 0 within
/// `timeout`. Shared by the session-cap probe and the per-codec encode probe.
async fn run_ffmpeg_probe(ffmpeg_bin: &str, args: &[String], timeout: Duration) -> bool {
    let mut cmd = tokio::process::Command::new(ffmpeg_bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(
        tokio::time::timeout(timeout, wait_status(child)).await,
        Ok(Some(true))
    )
}

/// Which of the target video codecs `device` can ACTUALLY hardware-encode,
/// confirmed by a real trial encode each. This is stricter than "the family
/// names an encoder": a Pascal NVENC names `av1_nvenc` but has no AV1 block, so
/// the trial fails and AV1 is excluded. CPU returns empty (software codecs come
/// from `ffmpeg -encoders`, not a device trial). Drives the negotiator's
/// `ServerEncodeCapabilities`, so it only ever advertises hardware codecs a real
/// encode proved.
pub async fn probe_encodable_codecs(device: DeviceId, timeout: Duration) -> Vec<crate::VideoCodec> {
    use crate::VideoCodec::{Av1, Vp9, H264, H265};
    let bin = ffmpeg_bin();
    let mut out = Vec::new();
    for codec in [H264, H265, Vp9, Av1] {
        if let Some(args) = codec_probe_args(device, codec) {
            if run_ffmpeg_probe(&bin, &args, timeout).await {
                out.push(codec);
            }
        }
    }
    out
}

/// Which SOURCE codecs `device` can actually hardware-DECODE, confirmed by a
/// real trial decode each.
///
/// The mirror of [`probe_encodable_codecs`], and needed for the same reason
/// that one is: naming a decoder is not having one. `-hwaccel cuda` was emitted
/// for every job placed on an NVENC device, decided from the DEVICE alone and
/// never from the source codec, on the strength of a comment asserting that
/// ffmpeg "falls back to software and exits 0" when the codec is unsupported.
/// That assertion is false. On 2026-08-01 a 10-bit AV1 source on a GTX 1070
/// (Pascal — H.264/HEVC/VP9 decode blocks, no AV1) produced
/// `hwaccel initialisation returned error`, a decode error rate of 1.0, and a
/// dead rendition. There is no way to know which codecs a card decodes short of
/// asking it, and a hand-written per-family table would be exactly the
/// hardware-specific constant V126 exists to forbid — NVDEC's AV1 support
/// arrived mid-generation, so "NVENC" is not the unit the answer varies on.
///
/// Each codec costs two throwaway ffmpeg runs: a five-frame 128x128 software
/// encode to a scratch file, then a decode of that file with the device's
/// `-hwaccel`. A codec whose SAMPLE fails to encode is skipped rather than
/// reported undecodable — the probe failing to build its own input says nothing
/// about the card.
///
/// The CPU and any device whose family has no `-hwaccel` name return empty:
/// there is no offload to gate.
pub async fn probe_decodable_codecs(
    device: DeviceId,
    timeout: Duration,
) -> Vec<crate::options::SourceCodec> {
    use crate::options::SourceCodec;
    if !matches!(device, DeviceId::Hw { .. }) || device.hwaccel().decoder_hwaccel().is_none() {
        return Vec::new();
    }
    let Some(dir) = decode_probe_scratch_dir(device) else {
        tracing::warn!(
            device = %device,
            "could not create a scratch directory for the decode probe; this \
             device will decode every source in software"
        );
        return Vec::new();
    };
    let bin = ffmpeg_bin();
    let mut out = Vec::new();
    for codec in SourceCodec::ALL {
        let sample = dir.join(format!("sample.{}", codec.ffmpeg_name()));
        if !run_ffmpeg_probe(&bin, &decode_sample_args(codec, &sample), timeout).await {
            tracing::debug!(
                device = %device,
                codec = codec.label(),
                encoder = codec.sample_encoder(),
                "decode probe could not build its own sample; codec not trialled"
            );
            continue;
        }
        if run_ffmpeg_probe(&bin, &decode_probe_args(device, &sample), timeout).await {
            out.push(codec);
        }
    }
    // Best-effort: the samples are a few kilobytes and the directory is
    // pid-scoped, so a failure to clean up cannot collide with another run.
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// A pid-scoped scratch directory for one device's decode probe samples.
/// Removed by the caller; pid-scoped so two servers probing at once cannot
/// read each other's half-written samples.
fn decode_probe_scratch_dir(device: DeviceId) -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "pharos-decode-probe-{}-{}",
        std::process::id(),
        device.to_string().replace([':', '/'], "-"),
    ));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// ffmpeg argv writing a tiny SOFTWARE-encoded sample of `codec` to `path` —
/// the input the decode trial then reads back.
///
/// Five frames of 128x128, the same shape [`codec_probe_args`] uses, so boot
/// pays milliseconds per codec. `-pix_fmt yuv420p` is explicit because the
/// filter source's default is encoder-dependent and a rejected pixel format
/// would look like "this codec cannot be sampled".
fn decode_sample_args(codec: crate::options::SourceCodec, path: &std::path::Path) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "testsrc=size=128x128:rate=5:duration=1".into(),
        "-c:v".into(),
        codec.sample_encoder().into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-frames:v".into(),
        "5".into(),
        // Matroska takes every one of these codecs, so the container is one
        // constant rather than a per-codec mapping that could go stale.
        "-f".into(),
        "matroska".into(),
        path.to_string_lossy().into_owned(),
    ]
}

/// ffmpeg argv decoding `sample` on `device`'s hardware decoder into the null
/// muxer. Exit status is the verdict — which is exactly the failure the live
/// incident produced, and exactly what the emitted `-hwaccel` will do to a real
/// segment.
///
/// The flags mirror [`crate::ffmpeg_transcode_args`]'s decode block so the
/// probe tests the same thing production runs: same `-hwaccel` name, and the
/// same `-hwaccel_device` pinning for CUDA so a multi-GPU box probes the card
/// it will encode on rather than device 0.
fn decode_probe_args(device: DeviceId, sample: &std::path::Path) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    if let Some(node) = device.vaapi_render_node() {
        a.push("-vaapi_device".into());
        a.push(node);
    }
    if let Some(name) = device.hwaccel().decoder_hwaccel() {
        a.push("-hwaccel".into());
        a.push(name.into());
        if name == "cuda" {
            if let Some(idx) = device.index() {
                a.push("-hwaccel_device".into());
                a.push(idx.to_string());
            }
        }
    }
    a.push("-i".into());
    a.push(sample.to_string_lossy().into_owned());
    a.push("-f".into());
    a.push("null".into());
    a.push("-".into());
    a
}

/// ffmpeg argv for a tiny throwaway encode of `codec` on `device` (lavfi source
/// → the device's hardware encoder → null muxer). `None` when the device's
/// family names no hardware encoder for `codec` (so there's nothing to trial).
fn codec_probe_args(device: DeviceId, codec: crate::VideoCodec) -> Option<Vec<String>> {
    use crate::hwaccel::HwAccel;
    let DeviceId::Hw { accel, index } = device else {
        return None;
    };
    let encoder = accel.video_encoder(codec)?;
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    if let Some(node) = device.vaapi_render_node() {
        a.push("-vaapi_device".into());
        a.push(node);
    }
    a.push("-f".into());
    a.push("lavfi".into());
    a.push("-i".into());
    a.push("testsrc=size=128x128:rate=5:duration=1".into());
    if matches!(accel, HwAccel::Vaapi) {
        // VAAPI encodes from a GPU-resident surface.
        a.push("-vf".into());
        a.push("format=nv12,hwupload".into());
    }
    a.push("-c:v".into());
    a.push(encoder.into());
    if matches!(accel, HwAccel::Nvenc) {
        a.push("-gpu".into());
        a.push(index.to_string());
    }
    a.push("-frames:v".into());
    a.push("5".into());
    a.push("-f".into());
    a.push("null".into());
    a.push("-".into());
    Some(a)
}

/// Frames per second this device encodes a fixed synthetic clip, or `None`
/// when it cannot be measured.
///
/// **This is a RATIO instrument, not a cost model.** 006 established that a
/// boot-time trial encode is a poor measure of what a real segment costs,
/// because real cost is dominated by decode and I/O rather than by the encoder.
/// That finding is unchanged and this does not contradict it: every device
/// encodes the SAME synthetic input here, so decode and source I/O are
/// common-mode and cancel in the comparison. The absolute number is close to
/// meaningless; the ratio between two devices is the thing being measured, and
/// it is the only hardware-neutral way to know that (say) a many-core software
/// encoder outruns a weak accelerator on the machine this happens to be
/// installed on.
///
/// Deliberately short and fixed-length so boot is not delayed; the frame count
/// is what makes two devices comparable, so it must not vary by device.
///
/// The timed region includes process spawn and ffmpeg init/filter-graph
/// setup, which are roughly constant per invocation regardless of device. On
/// a fast enough hardware encoder the 120-frame encode itself could complete
/// in the same order of magnitude as that fixed overhead, which would
/// compress the fast end of exactly the ratio this instrument exists to
/// produce. Rather than picking a frame count large enough for today's
/// fastest accelerator (a hardware-specific constant this whole spec exists
/// to avoid — V126), the fixed overhead is measured once per process by
/// [`rate_probe_baseline`] and subtracted before computing the rate.
pub async fn probe_encode_rate(device: DeviceId, timeout: Duration) -> Option<f64> {
    const RATE_PROBE_FRAMES: u32 = 120;
    let bin = ffmpeg_bin();
    let args = rate_probe_args(device, RATE_PROBE_FRAMES)?;
    let baseline = rate_probe_baseline().await;
    let started = Instant::now();
    if !run_ffmpeg_probe(&bin, &args, timeout).await {
        return None;
    }
    let wall = started.elapsed();
    // The baseline is spawn + init + teardown with essentially no encoding.
    // If it's not strictly less than this device's measured wall time, the
    // actual encode was lost in that noise; a rate here would be mostly a
    // spawn-time measurement, not a device measurement, so the honest
    // answer is None rather than a huge or negative number.
    if wall <= baseline {
        return None;
    }
    let secs = (wall - baseline).as_secs_f64();
    if secs <= 0.0 {
        return None;
    }
    Some(f64::from(RATE_PROBE_FRAMES) / secs)
}

/// This machine's fixed per-invocation overhead for the rate probe's argv
/// shape: process spawn + ffmpeg init/filter-graph setup + teardown, with
/// (almost) no actual encoding. Measured once by running the same probe argv
/// at the minimum possible frame count (1) on the CPU device — always
/// available, and the shape every device shares before its encoder-specific
/// args.
///
/// Cached in a `tokio::sync::OnceCell` (the same pattern already used by
/// `hwaccel::DETECTED` and `capability::ENCODERS` in this crate) rather than
/// threaded through as a parameter: the baseline is a property of the
/// machine, not of any one device probe, so every caller of
/// `probe_encode_rate` should see the same value without having to know it
/// exists or pass it explicitly. `OnceCell` (vs. `std::sync::OnceLock`) is
/// needed because computing it requires `.await`ing a real ffmpeg spawn.
async fn rate_probe_baseline() -> Duration {
    static BASELINE: OnceCell<Duration> = OnceCell::const_new();
    *BASELINE
        .get_or_init(|| async {
            let bin = ffmpeg_bin();
            let Some(args) = rate_probe_args(DeviceId::Cpu, 1) else {
                return Duration::ZERO;
            };
            let started = Instant::now();
            // Best-effort: if this throwaway spawn itself fails, treat the
            // overhead as zero rather than poisoning every later probe on
            // this process with a bogus timeout-sized value. A real device
            // probe sharing the same ffmpeg binary will fail the same way
            // and correctly report None on its own.
            let _ = run_ffmpeg_probe(&bin, &args, Duration::from_secs(30)).await;
            started.elapsed()
        })
        .await
}

/// ffmpeg argv for the timed probe: one synthetic source, one encoder, null
/// muxer. The SOURCE and FRAME COUNT are identical for every device — that is
/// what makes the resulting rates comparable — and only the encoder differs.
///
/// H264 is the probe codec because it is the one target essentially every
/// encoder implements; a device that cannot encode it is measured on its
/// software fallback, which is what it would actually use.
///
/// The VAAPI `format=nv12,hwupload` filter runs inside the timed region
/// (same as `probe_args`/`codec_probe_args`), not before it. That cost is
/// unavoidable on this device family — a real segment transcode pays it too
/// — so excluding it would make the VAAPI number incomparable to what the
/// device actually costs in production, the same comparability concern 006
/// raises about the instrument as a whole.
///
/// Always returns `Some`: an `Hw` device with no H264 encoder falls back to
/// `libx264` rather than bailing, so there is currently no input that makes
/// this fail to build an argv. That's still worth keeping as `Option` rather
/// than collapsing it, though: with the GPU index now wired through (below),
/// a *nonexistent* NVENC index is a valid argv here but fails at ffmpeg
/// runtime, which is exactly where `probe_encode_rate`'s `?`/`None` handling
/// is supposed to catch it — the fallibility has moved from argv
/// construction to process exit status, not disappeared.
fn rate_probe_args(device: DeviceId, frames: u32) -> Option<Vec<String>> {
    use crate::hwaccel::HwAccel;
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    if let Some(node) = device.vaapi_render_node() {
        a.push("-vaapi_device".into());
        a.push(node);
    }
    a.push("-f".into());
    a.push("lavfi".into());
    a.push("-i".into());
    a.push(format!(
        "testsrc2=size=1280x720:rate=30:duration={}",
        f64::from(frames) / 30.0
    ));
    let encoder = match device {
        DeviceId::Cpu => "libx264".to_string(),
        DeviceId::Hw { accel, .. } => match accel.video_encoder(crate::VideoCodec::H264) {
            Some(e) => e.to_string(),
            None => "libx264".to_string(),
        },
    };
    if device.vaapi_render_node().is_some() {
        a.push("-vf".into());
        a.push("format=nv12,hwupload".into());
    }
    a.push("-c:v".into());
    a.push(encoder);
    if let DeviceId::Hw { accel, index } = device {
        if matches!(accel, HwAccel::Nvenc) {
            a.push("-gpu".into());
            a.push(index.to_string());
        }
    }
    a.push("-frames:v".into());
    a.push(frames.to_string());
    a.push("-f".into());
    a.push("null".into());
    a.push("-".into());
    Some(a)
}

async fn wait_status(mut child: tokio::process::Child) -> Option<bool> {
    child.wait().await.ok().map(|s| s.success())
}

/// ffmpeg argv for a tiny throwaway encode on `device` (lavfi source →
/// device H.264 → null muxer). Used only to test whether the device can
/// open another encode session.
fn probe_args(device: DeviceId) -> Vec<String> {
    use crate::hwaccel::HwAccel;
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    // VAAPI device must precede -i so hwupload resolves it.
    if let Some(node) = device.vaapi_render_node() {
        a.push("-vaapi_device".into());
        a.push(node);
    }
    a.push("-f".into());
    a.push("lavfi".into());
    a.push("-i".into());
    a.push("testsrc=size=128x128:rate=5:duration=1".into());
    match device {
        DeviceId::Hw {
            accel: HwAccel::Vaapi,
            ..
        } => {
            a.push("-vf".into());
            a.push("format=nv12,hwupload".into());
            a.push("-c:v".into());
            a.push("h264_vaapi".into());
        }
        DeviceId::Hw { accel, index } => {
            a.push("-c:v".into());
            a.push(accel.h264_encoder().unwrap_or("libx264").into());
            if matches!(accel, HwAccel::Nvenc) {
                a.push("-gpu".into());
                a.push(index.to_string());
            }
        }
        DeviceId::Cpu => {
            a.push("-c:v".into());
            a.push("libx264".into());
        }
    }
    a.push("-frames:v".into());
    a.push("5".into());
    a.push("-f".into());
    a.push("null".into());
    a.push("-".into());
    a
}

#[cfg(test)]
mod rate_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::time::Duration;

    /// The CPU can always be measured — it is the terminal fallback and the
    /// only device guaranteed to exist. If this returns `None` the weighting
    /// has no software reference point and every ratio is unanchored.
    #[tokio::test]
    async fn the_software_encoder_reports_a_rate() {
        let r = probe_encode_rate(DeviceId::Cpu, Duration::from_secs(30)).await;
        let r = r.expect("the software encoder must be measurable");
        assert!(
            r > 0.0 && r.is_finite(),
            "a rate must be positive and finite, got {r}"
        );
    }

    /// Two runs on the same machine must agree closely enough that Task 2's
    /// quantisation cannot see a difference. This is the property the whole
    /// design's stability rests on.
    #[tokio::test]
    async fn two_runs_on_one_device_agree_within_a_wide_margin() {
        let t = Duration::from_secs(30);
        let a = probe_encode_rate(DeviceId::Cpu, t).await.unwrap();
        let b = probe_encode_rate(DeviceId::Cpu, t).await.unwrap();
        let ratio = a.max(b) / a.min(b);
        assert!(
            ratio < 3.0,
            "two probes of the same device disagreed by {ratio}x (a={a}, b={b}); \
             Task 2's quantisation must be coarser than this"
        );
    }

    /// The two tests above would both pass a stub that always returns
    /// `Some(1.0)` instantly (ratio 1.0/1.0 < 3.0 either way). Anchor the
    /// result to real elapsed wall-clock time so a no-op cannot pass: an
    /// actual ffmpeg encode of the fixed synthetic clip cannot complete in a
    /// handful of milliseconds, and the reported rate must be internally
    /// consistent with how long the call actually took.
    #[tokio::test]
    async fn the_rate_is_backed_by_real_elapsed_encode_time() {
        let started = std::time::Instant::now();
        let r = probe_encode_rate(DeviceId::Cpu, Duration::from_secs(30))
            .await
            .expect("the software encoder must be measurable");
        let wall = started.elapsed().as_secs_f64();
        assert!(
            wall > 0.05,
            "probe_encode_rate returned in {wall}s, far too fast to have \
             actually run ffmpeg on the synthetic clip -- suspect a stub"
        );
        // rate = frames / secs by construction, so frames = rate * secs
        // must land near the fixed probe frame count, not an unrelated
        // constant a stub could have returned regardless of timing.
        let implied_frames = r * wall;
        assert!(
            (60.0..=240.0).contains(&implied_frames),
            "reported rate {r} over {wall}s implies {implied_frames} \
             frames encoded; expected close to the fixed 120-frame clip"
        );
    }

    /// Finding 2: the fixed per-invocation overhead (spawn + ffmpeg init) must
    /// be measured and subtracted before computing the rate, not folded into
    /// it. There is no fast accelerator on this machine to make the effect
    /// dramatic, but the arithmetic itself is directly observable without
    /// one: the encode time implied by the reported rate (`frames / rate`)
    /// must be shorter than the call's own wall-clock time by at least the
    /// measured baseline. Without the subtraction, rate = frames / wall
    /// exactly, so the implied encode time equals wall and this margin would
    /// be zero — this assertion would fail without the fix.
    #[tokio::test]
    async fn reported_rate_implies_the_overhead_was_subtracted() {
        // Prime the baseline first so its one-time cost isn't attributed to
        // the timed call below.
        let baseline = rate_probe_baseline().await;
        assert!(
            baseline > Duration::ZERO,
            "expected a measurable nonzero per-invocation overhead on a real \
             ffmpeg binary, got {baseline:?}"
        );

        let started = Instant::now();
        let r = probe_encode_rate(DeviceId::Cpu, Duration::from_secs(30))
            .await
            .expect("the software encoder must be measurable");
        let wall = started.elapsed();

        let implied_encode_time = Duration::from_secs_f64(120.0 / r);
        assert!(
            implied_encode_time + baseline <= wall,
            "implied encode time {implied_encode_time:?} + measured baseline \
             {baseline:?} should not exceed wall {wall:?}; if it does, the \
             baseline was never subtracted from the timed region"
        );
    }

    /// No test previously covered `probe_encode_rate`'s `None` path. A
    /// timeout too short for ffmpeg to possibly finish spawning and encoding
    /// is a cheap, hardware-independent way to force it without needing a
    /// slow or absent device.
    #[tokio::test]
    async fn a_timeout_too_short_to_finish_reports_none() {
        let r = probe_encode_rate(DeviceId::Cpu, Duration::from_nanos(1)).await;
        assert!(
            r.is_none(),
            "a timeout that cannot possibly complete must report None, got {r:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn fast_cfg() -> ProbeConfig {
        ProbeConfig {
            max_attempts: 8,
            per_attempt_timeout: Duration::from_secs(1),
            overall_timeout: Duration::from_secs(10),
        }
    }

    #[tokio::test]
    async fn ramp_finds_session_cap() {
        // Fake device that sustains at most 3 concurrent sessions: an
        // attempt fails when live concurrency would exceed 3.
        const CAP: usize = 3;
        let live = Arc::new(AtomicUsize::new(0));
        let dev = DeviceId::hw(HwAccel::Nvenc, 0);
        let caps = probe_caps(&[dev], &fast_cfg(), |_d| {
            let live = live.clone();
            async move {
                let n = live.fetch_add(1, Ordering::SeqCst) + 1;
                // Hold the slot briefly so concurrent attempts overlap.
                tokio::time::sleep(Duration::from_millis(10)).await;
                let ok = n <= CAP;
                live.fetch_sub(1, Ordering::SeqCst);
                ok
            }
        })
        .await;
        assert_eq!(caps.caps.as_slice(), &[(dev, CAP)]);
    }

    #[tokio::test]
    async fn device_that_cannot_encode_is_omitted() {
        let dev = DeviceId::hw(HwAccel::Vaapi, 0);
        let caps = probe_caps(&[dev], &fast_cfg(), |_d| async { false }).await;
        assert!(caps.caps.is_empty());
    }

    #[tokio::test]
    async fn unbounded_device_clamps_to_max_attempts() {
        let dev = DeviceId::hw(HwAccel::Vaapi, 1);
        let cfg = ProbeConfig {
            max_attempts: 4,
            ..fast_cfg()
        };
        let caps = probe_caps(&[dev], &cfg, |_d| async { true }).await;
        assert_eq!(caps.caps.as_slice(), &[(dev, 4)]);
    }

    #[test]
    /// The decode trial must test the SAME thing production runs, or a codec it
    /// blesses can still fail on a real segment. Both flags matter: `-hwaccel`
    /// is what production emits, and `-hwaccel_device` is what pins the trial to
    /// the card that will encode rather than to CUDA device 0 — a multi-GPU box
    /// would otherwise probe one card and offload to another.
    #[test]
    fn the_decode_trial_uses_the_same_hwaccel_flags_production_emits() {
        let sample = std::path::Path::new("/tmp/s.av1");
        let a = decode_probe_args(DeviceId::hw(HwAccel::Nvenc, 2), sample).join(" ");
        assert!(a.contains("-hwaccel cuda"), "{a}");
        assert!(a.contains("-hwaccel_device 2"), "{a}");
        assert!(a.contains("-i /tmp/s.av1"), "{a}");
        assert!(a.contains("-f null"), "{a}");

        let v = decode_probe_args(DeviceId::hw(HwAccel::Vaapi, 1), sample).join(" ");
        assert!(v.contains("-vaapi_device /dev/dri/renderD129"), "{v}");
        assert!(v.contains("-hwaccel vaapi"), "{v}");
        // `-hwaccel_device` is CUDA's index flag; VAAPI selects by render node.
        assert!(!v.contains("-hwaccel_device"), "{v}");
    }

    /// Every codec the probe trials must be able to build its own sample, or
    /// the probe reports "undecodable" for a codec it never actually tested and
    /// silently drops GPU decode for it. Runs real ffmpeg — the encoder names
    /// are the thing being checked, and a table of them tests nothing.
    #[tokio::test]
    async fn every_probed_codec_can_have_a_sample_built_for_it() {
        use crate::options::SourceCodec;
        let dir = std::env::temp_dir().join(format!("pharos-sample-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = ffmpeg_bin();
        for codec in SourceCodec::ALL {
            let path = dir.join(format!("s.{}", codec.ffmpeg_name()));
            let ok = run_ffmpeg_probe(
                &bin,
                &decode_sample_args(codec, &path),
                Duration::from_secs(30),
            )
            .await;
            assert!(
                ok,
                "no sample could be built for {} with {} — the decode probe \
                 would skip this codec and it would never be offloaded",
                codec.ffmpeg_name(),
                codec.sample_encoder()
            );
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            assert!(len > 0, "{} sample is empty", codec.ffmpeg_name());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CPU has no decode offload to gate, and a probe that returned codecs
    /// for it would let `may_offload_decode` answer about a device with no
    /// `-hwaccel` name at all.
    #[tokio::test]
    async fn the_cpu_has_no_decodable_set() {
        assert!(
            probe_decodable_codecs(DeviceId::Cpu, Duration::from_secs(5))
                .await
                .is_empty()
        );
    }

    #[test]
    fn vaapi_probe_args_carry_device_and_hwupload() {
        let a = probe_args(DeviceId::hw(HwAccel::Vaapi, 1)).join(" ");
        assert!(a.contains("-vaapi_device /dev/dri/renderD129"), "{a}");
        assert!(a.contains("format=nv12,hwupload"), "{a}");
        assert!(a.contains("-c:v h264_vaapi"), "{a}");
        assert!(a.contains("-f null"), "{a}");
    }

    /// Finding 1: `rate_probe_args` must carry the requested NVENC index
    /// through as `-gpu`, exactly like `probe_args` and `codec_probe_args`
    /// already do. Without it, every index probes ffmpeg's default GPU 0,
    /// so distinct indices become indistinguishable (or a nonexistent index
    /// silently "succeeds" against GPU 0 instead of failing).
    #[test]
    fn rate_probe_args_carry_the_requested_gpu_index() {
        let a = rate_probe_args(DeviceId::hw(HwAccel::Nvenc, 3), 120)
            .expect("rate_probe_args is currently infallible")
            .join(" ");
        assert!(
            a.contains("-gpu 3"),
            "rate probe for NVENC index 3 must pin -gpu 3, got: {a}"
        );
    }
}
