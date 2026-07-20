//! Runtime device table — the load-balancing capacity model.
//!
//! Each transcode device (every detected hardware encoder family, plus
//! the CPU) gets a [`DeviceSlot`] whose `Arc<Semaphore>` holds one permit
//! per concurrent encode session the device can sustain. The session
//! counts come from the boot-time auto-probe ([`crate::probe`]) or a
//! config override.
//!
//! The scheduler never blocks on a permit: it walks
//! [`DeviceTable::eligible_for`] best-first and `try_acquire_owned`s each
//! in turn. The acquired permit is moved into the detached encode task
//! and released by `Drop` — so capacity bookkeeping needs no "release"
//! message and the scheduler actor can't deadlock waiting to send one.
//!
//! `DeviceId` itself is a wire type and lives in [`crate::protocol`];
//! `Semaphore`s aren't serialisable so the runtime table stays here.

use crate::hwaccel::HwAccel;
use crate::options::{TranscodeOptions, VideoCodec};
use crate::protocol::DeviceId;
use smallvec::SmallVec;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

/// Enumerate concrete hardware devices from the detected encoder
/// families. VAAPI expands to one device per DRM render node present
/// (`/dev/dri/renderD128`, `renderD129`, …) so a multi-GPU box gets a
/// slot per card; other families map to a single index-0 device for now.
/// CPU is intentionally not included here — `DeviceTable::from_probe`
/// always appends it as the terminal fallback.
pub fn enumerate(detected: &[HwAccel]) -> Vec<DeviceId> {
    let mut out = Vec::new();
    for &accel in detected {
        match accel {
            HwAccel::Vaapi => {
                let nodes = vaapi_render_node_indices();
                if nodes.is_empty() {
                    // Detected by ffmpeg but no render node visible — keep
                    // a single index-0 device so the path still exists.
                    out.push(DeviceId::hw(HwAccel::Vaapi, 0));
                } else {
                    for idx in nodes {
                        out.push(DeviceId::hw(HwAccel::Vaapi, idx));
                    }
                }
            }
            HwAccel::Nvenc | HwAccel::Qsv | HwAccel::VideoToolbox => {
                out.push(DeviceId::hw(accel, 0));
            }
            // Auto/Off never appear in a detected set.
            HwAccel::Auto | HwAccel::Off => {}
        }
    }
    out
}

/// Default CPU encode-permit budget: `cores / threads-per-encode`. Each
/// permit is one concurrent software encode, and every software video job
/// (encoder AND decoder) is capped at [`crate::sw_encode_threads`] (≈4)
/// threads — so this admits exactly as many jobs as genuinely fit the
/// machine, each running at several× realtime. The old "one permit per
/// core" budget double-counted: each admitted job ALSO auto-threaded
/// across every core, so 16 jobs × all-core encoders thrashed the box and
/// every segment slowed BELOW realtime together (3.5-12 s per 6 s segment
/// measured) — the parallel-small-encodes design self-defeating. Floor at
/// 1 (tiny boxes serialize; prefetch still pipelines ahead).
pub fn default_cpu_permits() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / crate::sw_encode_threads() as usize).max(1)
}

/// Render-node ordinals present under `/dev/dri` (the `N` in
/// `renderD{128+N}`), sorted ascending. Empty when the dir is unreadable.
pub fn vaapi_render_node_indices() -> Vec<u8> {
    let mut idxs = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev/dri") {
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if let Some(num) = name.strip_prefix("renderD") {
                    if let Ok(n) = num.parse::<u32>() {
                        if (128..128 + 256).contains(&n) {
                            idxs.push((n - 128) as u8);
                        }
                    }
                }
            }
        }
    }
    idxs.sort_unstable();
    idxs
}

/// One device's capacity + transient-failure cooldown.
pub struct DeviceSlot {
    pub id: DeviceId,
    /// Permits == sustainable concurrent encode sessions. Acquired with
    /// `try_acquire_owned` before dispatch, released on task `Drop`.
    pub sem: Arc<Semaphore>,
    /// Set when the device returns a transient `DeviceBusy`. While in the
    /// future, the device is skipped by `eligible_for`.
    pub cooldown_until: Option<Instant>,
    /// The probed/configured permit count, retained for snapshots
    /// (`Semaphore` exposes only the *current* available count).
    pub capacity: usize,
}

impl DeviceSlot {
    fn new(id: DeviceId, capacity: usize) -> Self {
        // At least one permit — a device with a probed cap of 0 would be
        // useless; clamp so the table never holds a dead slot.
        let permits = capacity.max(1);
        Self {
            id,
            sem: Arc::new(Semaphore::new(permits)),
            cooldown_until: None,
            capacity: permits,
        }
    }

    /// Permits currently free (not handed to an in-flight encode).
    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }

    /// In-flight encodes on this device right now.
    pub fn in_use(&self) -> usize {
        self.capacity.saturating_sub(self.available())
    }

    fn in_cooldown(&self, now: Instant) -> bool {
        matches!(self.cooldown_until, Some(t) if t > now)
    }
}

/// Whether `device` can encode the video target in `opts`.
///
/// - CPU encodes anything (software libx264/libx265/libvpx/libaom, or a
///   pure copy/remux with no video re-encode).
/// - A hardware device is eligible only when the requested video codec
///   maps to a real encoder on that family (H264/H265 today). VP9/AV1
///   targets and pure `Copy`/audio-only jobs route to the CPU — there is
///   no throughput win from a GPU H264 block on a remux, and consumer
///   GPUs lack VP9/AV1 *encode* on most families.
pub fn device_supports(device: DeviceId, opts: &TranscodeOptions) -> bool {
    match device {
        DeviceId::Cpu => true,
        DeviceId::Hw { accel, .. } => match opts.video {
            Some(VideoCodec::H264) => accel.h264_encoder().is_some(),
            Some(VideoCodec::H265) => accel.hevc_encoder().is_some(),
            // Vp9/Av1/Copy/None → CPU only.
            _ => false,
        },
    }
}

/// Priority-ordered set of devices. Hardware encoders come first (best
/// throughput), CPU is always present and always last (terminal
/// fallback — its permit budget bounds software-encode concurrency).
pub struct DeviceTable {
    slots: SmallVec<[DeviceSlot; 5]>,
}

impl DeviceTable {
    /// Build from probed `(device, session-cap)` pairs plus a CPU permit
    /// budget. `caps` is taken in the caller's preferred priority order
    /// (the scheduler dispatches in this order); CPU is appended last and
    /// deduped if the caller already included it.
    pub fn from_probe(caps: &[(DeviceId, usize)], cpu_permits: usize) -> Self {
        let mut slots: SmallVec<[DeviceSlot; 5]> = SmallVec::new();
        for &(id, cap) in caps {
            if matches!(id, DeviceId::Cpu) {
                continue; // CPU is appended once, below
            }
            if slots.iter().any(|s| s.id == id) {
                continue;
            }
            slots.push(DeviceSlot::new(id, cap));
        }
        slots.push(DeviceSlot::new(DeviceId::Cpu, cpu_permits));
        Self { slots }
    }

    /// Devices that can run `opts` and aren't in cooldown, best-first.
    /// CPU is last and (barring cooldown) always present, guaranteeing a
    /// terminal fallback for any encodable job.
    pub fn eligible_for(&self, opts: &TranscodeOptions, now: Instant) -> SmallVec<[DeviceId; 5]> {
        self.slots
            .iter()
            .filter(|s| device_supports(s.id, opts) && !s.in_cooldown(now))
            .map(|s| s.id)
            .collect()
    }

    pub fn slot(&self, id: DeviceId) -> Option<&DeviceSlot> {
        self.slots.iter().find(|s| s.id == id)
    }

    /// Mark a device transiently unavailable until `until`.
    pub fn set_cooldown(&mut self, id: DeviceId, until: Instant) {
        if let Some(s) = self.slots.iter_mut().find(|s| s.id == id) {
            s.cooldown_until = Some(until);
        }
    }

    /// All slots, for snapshots / the test tool.
    pub fn slots(&self) -> &[DeviceSlot] {
        &self.slots
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;
    use crate::options::{AudioCodec, Container};
    use std::time::Duration;

    #[test]
    fn cpu_permits_match_thread_budget_not_core_count() {
        // permits × threads-per-encode must ≈ cores, never permits = cores
        // (which double-counts: every admitted job also multi-threads, so
        // they all contend and slow below realtime together).
        let cores = std::thread::available_parallelism().unwrap().get();
        let expect = (cores / crate::sw_encode_threads() as usize).max(1);
        assert_eq!(default_cpu_permits(), expect);
        assert!(default_cpu_permits() >= 1);
        assert!(
            default_cpu_permits() * crate::sw_encode_threads() as usize <= cores.max(4),
            "permit budget must not oversubscribe the cores"
        );
    }

    fn h264_opts() -> TranscodeOptions {
        TranscodeOptions {
            container: Container::Mpegts,
            video: Some(VideoCodec::H264),
            audio: Some(AudioCodec::Aac),
            video_bitrate_bps: None,
            audio_bitrate_bps: None,
            start_position_ticks: 0,
            duration_ticks: None,
            audio_source_stream_index: None,
            burn_subtitle_stream_index: None,
            burn_subtitle_is_text: false,
        }
    }

    fn table() -> DeviceTable {
        DeviceTable::from_probe(
            &[
                (DeviceId::hw(HwAccel::Nvenc, 0), 3),
                (DeviceId::hw(HwAccel::Vaapi, 0), 2),
            ],
            4,
        )
    }

    #[test]
    fn cpu_is_always_last_and_present() {
        let t = table();
        let ids: Vec<_> = t.slots().iter().map(|s| s.id).collect();
        assert_eq!(*ids.last().unwrap(), DeviceId::Cpu);
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn eligible_for_h264_lists_hw_first_then_cpu() {
        let t = table();
        let elig = t.eligible_for(&h264_opts(), Instant::now());
        assert_eq!(
            elig.as_slice(),
            &[
                DeviceId::hw(HwAccel::Nvenc, 0),
                DeviceId::hw(HwAccel::Vaapi, 0),
                DeviceId::Cpu
            ]
        );
    }

    #[test]
    fn vp9_target_routes_cpu_only() {
        let t = table();
        let mut o = h264_opts();
        o.video = Some(VideoCodec::Vp9);
        let elig = t.eligible_for(&o, Instant::now());
        assert_eq!(elig.as_slice(), &[DeviceId::Cpu]);
    }

    #[test]
    fn copy_and_audio_only_route_cpu_only() {
        let t = table();
        let mut copy = h264_opts();
        copy.video = Some(VideoCodec::Copy);
        assert_eq!(
            t.eligible_for(&copy, Instant::now()).as_slice(),
            &[DeviceId::Cpu]
        );
        let mut audio_only = h264_opts();
        audio_only.video = None;
        assert_eq!(
            t.eligible_for(&audio_only, Instant::now()).as_slice(),
            &[DeviceId::Cpu]
        );
    }

    #[test]
    fn cooldown_excludes_device_until_expiry() {
        let mut t = table();
        let now = Instant::now();
        t.set_cooldown(
            DeviceId::hw(HwAccel::Nvenc, 0),
            now + Duration::from_secs(2),
        );
        // Mid-cooldown: nvenc gone, vaapi + cpu remain.
        let elig = t.eligible_for(&h264_opts(), now);
        assert_eq!(
            elig.as_slice(),
            &[DeviceId::hw(HwAccel::Vaapi, 0), DeviceId::Cpu]
        );
        // After expiry: nvenc back.
        let elig2 = t.eligible_for(&h264_opts(), now + Duration::from_secs(3));
        assert!(elig2.contains(&DeviceId::hw(HwAccel::Nvenc, 0)));
    }

    #[test]
    fn permit_accounting_tracks_in_use() {
        let t = table();
        let slot = t.slot(DeviceId::hw(HwAccel::Nvenc, 0)).unwrap();
        assert_eq!(slot.available(), 3);
        assert_eq!(slot.in_use(), 0);
        let p1 = slot.sem.clone().try_acquire_owned().unwrap();
        let _p2 = slot.sem.clone().try_acquire_owned().unwrap();
        assert_eq!(slot.in_use(), 2);
        assert_eq!(slot.available(), 1);
        drop(p1);
        assert_eq!(slot.in_use(), 1);
    }

    #[test]
    fn zero_cap_clamps_to_one_permit() {
        let t = DeviceTable::from_probe(&[(DeviceId::hw(HwAccel::Qsv, 0), 0)], 1);
        let slot = t.slot(DeviceId::hw(HwAccel::Qsv, 0)).unwrap();
        assert_eq!(slot.capacity, 1);
        assert_eq!(slot.available(), 1);
    }

    #[test]
    fn duplicate_devices_deduped() {
        let t = DeviceTable::from_probe(
            &[
                (DeviceId::hw(HwAccel::Nvenc, 0), 3),
                (DeviceId::hw(HwAccel::Nvenc, 0), 9),
            ],
            2,
        );
        let n = t
            .slots()
            .iter()
            .filter(|s| s.id == DeviceId::hw(HwAccel::Nvenc, 0))
            .count();
        assert_eq!(n, 1);
        // First wins.
        assert_eq!(t.slot(DeviceId::hw(HwAccel::Nvenc, 0)).unwrap().capacity, 3);
    }
}
