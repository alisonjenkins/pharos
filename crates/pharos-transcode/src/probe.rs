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
pub async fn probe_caps<F, Fut>(
    devices: &[DeviceId],
    cfg: &ProbeConfig,
    attempt: F,
) -> ProbedCaps
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
            Poll::Ready(pinned.iter_mut().map(|(_, o)| o.take().unwrap_or(false)).collect())
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
    let args = probe_args(device);
    let mut cmd = tokio::process::Command::new(ffmpeg_bin);
    cmd.args(&args)
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
        DeviceId::Hw { accel: HwAccel::Vaapi, .. } => {
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
