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
pub async fn probe_encode_rate(device: DeviceId, timeout: Duration) -> Option<f64> {
    const RATE_PROBE_FRAMES: u32 = 120;
    let bin = ffmpeg_bin();
    let args = rate_probe_args(device, RATE_PROBE_FRAMES)?;
    let started = Instant::now();
    if !run_ffmpeg_probe(&bin, &args, timeout).await {
        return None;
    }
    let secs = started.elapsed().as_secs_f64();
    if secs <= 0.0 {
        return None;
    }
    Some(f64::from(RATE_PROBE_FRAMES) / secs)
}

/// ffmpeg argv for the timed probe: one synthetic source, one encoder, null
/// muxer. The SOURCE and FRAME COUNT are identical for every device — that is
/// what makes the resulting rates comparable — and only the encoder differs.
///
/// H264 is the probe codec because it is the one target essentially every
/// encoder implements; a device that cannot encode it is measured on its
/// software fallback, which is what it would actually use.
fn rate_probe_args(device: DeviceId, frames: u32) -> Option<Vec<String>> {
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
    fn vaapi_probe_args_carry_device_and_hwupload() {
        let a = probe_args(DeviceId::hw(HwAccel::Vaapi, 1)).join(" ");
        assert!(a.contains("-vaapi_device /dev/dri/renderD129"), "{a}");
        assert!(a.contains("format=nv12,hwupload"), "{a}");
        assert!(a.contains("-c:v h264_vaapi"), "{a}");
        assert!(a.contains("-f null"), "{a}");
    }
}
