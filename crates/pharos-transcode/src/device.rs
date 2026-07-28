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
use crate::options::{Container, TranscodeOptions, VideoCodec};
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

/// Does this encode share ONE init across many segments?
///
/// True for fMP4/CMAF video: every segment decodes under a single `avcC`
/// carrying seg0's parameter sets, and the media segments carry none of their
/// own. Such a rendition must be produced entirely by ONE encoder — see
/// [`device_supports`] and spec 003 — because two encoders' SPS are not
/// interchangeable.
///
/// Keyed on the CONTAINER contract, not on a codec list (spec 003 R6). The
/// hazard belongs to shared-init fMP4 as such: H264 has the CMAF rung today and
/// H265 would carry it identically, and a VP9 fMP4 rung already exists. mpegts
/// repeats parameter sets per segment and is self-describing, so it is exempt.
/// `Copy` and audio-only encodes have no video parameter set to disagree about.
pub fn shared_init_fmp4(opts: &TranscodeOptions) -> bool {
    opts.container == Container::Fmp4 && !matches!(opts.video, None | Some(VideoCodec::Copy))
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
            // NOTE (spec 003): hardware used to be ineligible here for
            // H264+fMP4. The hazard is real — a shared-init CMAF rendition
            // decodes every segment under ONE `avcC` carrying seg0's parameter
            // sets, and two encoders' SPS are not interchangeable, so a spilled
            // segment is undecodable (issue #114). Measured on the deployment
            // GPU: NVENC emits Main profile with log2_max_frame_num 8 where
            // libx264 emits High with 4, and NO ffmpeg flag reconciles them
            // (research R7 — `-profile:v main` aligns the profile but not the
            // slice-header field widths, and libopenh264 is further away
            // still). It cannot be patched in the container either, because
            // log2_max_frame_num is the WIDTH of frame_num in every slice
            // header.
            //
            // What changed is HOW the one-encoder guarantee is enforced. It is
            // no longer "no hardware may ever run this" but "this rendition
            // resolves to exactly one device", via
            // `DeviceTable::rendition_device` — a pure function of the
            // rendition, so it survives a restart, and the scheduler refuses to
            // place the job anywhere else even under cooldown. Excluding
            // hardware wholesale cost the entire GPU for browser playback once
            // PR#70 moved all browser H264 to CMAF (measured: 420 of 423 jobs
            // on CPU at 1.81x realtime with NVENC idle).
            // A hardware device is eligible for a codec when its family names an
            // encoder for it (h264/hevc always; vp9 on VAAPI; av1 on
            // VAAPI/NVENC/QSV). The negotiator only TARGETS a hardware codec the
            // boot trial-encode confirmed, so a family that names an encoder it
            // can't actually run (e.g. av1_nvenc on Pascal) is never picked and
            // thus never reaches here; a runtime failure still falls back to CPU.
            Some(codec) => accel.video_encoder(codec).is_some(),
            // Pure Copy / audio-only → CPU.
            None => false,
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

    /// The ONE device a shared-init fMP4 rendition must use (spec 003 R8).
    ///
    /// A pure function of `rendition_key` over the devices that support the
    /// encode, so it returns the same answer after a restart — an in-memory pin
    /// would not, and a rendition re-pinned to a different device mid-playback
    /// serves segments that no longer match the client's init (issue #114).
    /// Because it is deterministic, cached segments for a rendition were also
    /// necessarily produced by the device that rendition still resolves to.
    ///
    /// Hardware is preferred as a pool; hashing ACROSS that pool spreads
    /// renditions over multiple GPUs while keeping each rendition on one.
    ///
    /// Deliberately ignores cooldown and load: both are transient, and letting
    /// them change the answer would reintroduce the non-determinism this
    /// exists to remove. The caller checks cooldown and FAILS rather than
    /// choosing elsewhere; a merely busy device is queued for.
    pub fn rendition_device(
        &self,
        opts: &TranscodeOptions,
        rendition_key: u64,
    ) -> Option<DeviceId> {
        let supporting: SmallVec<[DeviceId; 5]> = self
            .slots
            .iter()
            .map(|s| s.id)
            .filter(|id| device_supports(*id, opts))
            .collect();
        if supporting.is_empty() {
            return None;
        }
        let hw: SmallVec<[DeviceId; 5]> = supporting
            .iter()
            .copied()
            .filter(|d| matches!(d, DeviceId::Hw { .. }))
            .collect();
        let pool = if hw.is_empty() { supporting } else { hw };
        Some(pool[(rendition_key % pool.len() as u64) as usize])
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
    use crate::options::{AudioCodec, Container, VideoCodec};
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
            source_frame_rate: None,
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
            burn_subtitle_ass_path: None,
            burn_fonts_dir: None,
            decode_preroll_seconds: None,
            muxed_audio_source: None,
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
    fn vp9_routes_to_vaapi_hardware_not_nvenc() {
        let t = table();
        let mut o = h264_opts();
        o.video = Some(VideoCodec::Vp9);
        let elig = t.eligible_for(&o, Instant::now());
        // VAAPI has `vp9_vaapi`; NVENC has no VP9 encoder → excluded. CPU last.
        assert_eq!(
            elig.as_slice(),
            &[DeviceId::hw(HwAccel::Vaapi, 0), DeviceId::Cpu]
        );
    }

    /// Spec 003 US2 — the guarantee. The same rendition must resolve to the same
    /// device every time, including after a restart (the function is pure, so a
    /// fresh table gives the same answer) and regardless of cooldown or load.
    ///
    /// This is the property that replaces the CPU-only exclusion. If it fails,
    /// issue #114 is back: segments produced by a second encoder are undecodable
    /// under the init the client already holds.
    #[test]
    fn a_rendition_always_resolves_to_the_same_device() {
        let mut cmaf = h264_opts();
        cmaf.container = Container::Fmp4;
        let key = 0x1234_5678_9abc_def0u64;

        let t = table();
        let first = t.rendition_device(&cmaf, key).expect("a device");
        for _ in 0..50 {
            assert_eq!(t.rendition_device(&cmaf, key), Some(first));
        }

        // A FRESH table is the restart case: same answer, nothing remembered.
        let restarted = table();
        assert_eq!(restarted.rendition_device(&cmaf, key), Some(first));

        // Cooldown must NOT move it — the caller fails instead of choosing
        // elsewhere, or determinism is lost through the back door.
        let mut cooled = table();
        cooled.set_cooldown(first, Instant::now() + Duration::from_secs(60));
        assert_eq!(
            cooled.rendition_device(&cmaf, key),
            Some(first),
            "cooldown is transient and must not change which device owns a rendition"
        );
    }

    /// Hardware is preferred, and renditions SPREAD across a multi-GPU pool
    /// rather than all piling onto the first device.
    #[test]
    fn renditions_prefer_hardware_and_spread_across_it() {
        let t = table();
        let mut cmaf = h264_opts();
        cmaf.container = Container::Fmp4;

        let picked: Vec<DeviceId> = (0..64)
            .map(|k| t.rendition_device(&cmaf, k).expect("a device"))
            .collect();
        assert!(
            picked.iter().all(|d| matches!(d, DeviceId::Hw { .. })),
            "hardware supports h264, so no rendition should land on CPU"
        );
        let distinct: std::collections::HashSet<_> = picked.iter().collect();
        assert!(
            distinct.len() > 1,
            "with several hw devices, renditions must spread; got {distinct:?}"
        );
    }

    /// With no hardware able to encode the codec, the pool is CPU — still one
    /// deterministic device, so the guarantee holds on a CPU-only host too.
    #[test]
    fn a_codec_no_hardware_can_encode_resolves_to_cpu() {
        let t = table();
        let mut o = h264_opts();
        o.container = Container::Fmp4;
        o.video = Some(VideoCodec::Vp9);
        // NVENC has no VP9 encoder; this table's VAAPI does, so force a table
        // with neither by asking for a codec nothing accelerates.
        let cpu_only = DeviceTable::from_probe(&[], 4);
        assert_eq!(cpu_only.rendition_device(&o, 7), Some(DeviceId::Cpu));
        assert!(t.rendition_device(&o, 7).is_some());
    }

    /// Spec 003 R6 — the shared-init hazard is a property of the CONTAINER, so
    /// the predicate must not be a codec list that a future rung falls off.
    #[test]
    fn shared_init_is_decided_by_the_container_not_the_codec() {
        let mut o = h264_opts();
        o.container = Container::Fmp4;
        assert!(shared_init_fmp4(&o), "h264 CMAF shares one init");

        o.video = Some(VideoCodec::H265);
        assert!(
            shared_init_fmp4(&o),
            "h265 fMP4 would carry the same hazard"
        );

        o.video = Some(VideoCodec::Vp9);
        assert!(shared_init_fmp4(&o), "the vp9 fMP4 rung shares an init too");

        // mpegts repeats parameter sets per segment — self-describing, exempt.
        let mpegts = h264_opts();
        assert_eq!(mpegts.container, Container::Mpegts);
        assert!(!shared_init_fmp4(&mpegts));

        // Nothing to disagree about without a video encode.
        let mut copy = h264_opts();
        copy.container = Container::Fmp4;
        copy.video = Some(VideoCodec::Copy);
        assert!(!shared_init_fmp4(&copy));
        copy.video = None;
        assert!(!shared_init_fmp4(&copy));
    }

    /// Spec 003 — CMAF H264 now reaches hardware, and mpegts is unchanged.
    ///
    /// This test used to assert `&[DeviceId::Cpu]`, because the one-encoder
    /// guarantee that a shared-init CMAF rendition needs was enforced by making
    /// hardware ineligible outright. That guarantee now comes from
    /// `rendition_device` — a rendition resolves to exactly one device and the
    /// scheduler will not place it elsewhere, even under cooldown — so the
    /// exclusion is no longer the mechanism and its cost (the whole GPU,
    /// unreachable for every browser since PR#70) is no longer paid.
    ///
    /// The hazard it guarded against is unchanged and still real: NVENC and
    /// libx264 emit SPS that differ in `log2_max_frame_num`, which is the bit
    /// WIDTH of `frame_num` in every slice header, so mixing them under one
    /// init is undecodable (issue #114, re-measured in research R7).
    #[test]
    fn cmaf_h264_reaches_hardware_and_mpegts_is_unchanged() {
        let t = table();
        let mut cmaf = h264_opts();
        cmaf.container = Container::Fmp4;
        assert_eq!(
            t.eligible_for(&cmaf, Instant::now()).as_slice(),
            &[
                DeviceId::hw(HwAccel::Nvenc, 0),
                DeviceId::hw(HwAccel::Vaapi, 0),
                DeviceId::Cpu
            ],
            "CMAF H264 must now be able to reach hardware"
        );
        // ...but only ONE of them may actually serve the rendition.
        assert!(matches!(
            t.rendition_device(&cmaf, 42).expect("a device"),
            DeviceId::Hw { .. }
        ));

        let mpegts = h264_opts();
        assert_eq!(
            t.eligible_for(&mpegts, Instant::now()).as_slice(),
            &[
                DeviceId::hw(HwAccel::Nvenc, 0),
                DeviceId::hw(HwAccel::Vaapi, 0),
                DeviceId::Cpu
            ],
            "mpegts H264 keeps hardware"
        );
        assert!(
            !shared_init_fmp4(&mpegts),
            "mpegts keeps free load-balancing: it is self-describing per segment"
        );
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
