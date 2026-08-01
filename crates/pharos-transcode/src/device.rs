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

/// How coarsely a measured rate is bucketed before it can affect placement.
/// Doubling steps: a device has to measure twice as fast before placement
/// notices.
///
/// **This buys coarseness, not stability.** It was documented as "the
/// STABILITY CONTRACT" when it landed, and it cannot be one — no bucket width
/// can be. Rounding does not SHRINK the set of rates with zero margin, it
/// RELOCATES it, from `{2^k}` to `{2^(k+0.5)}`. A device that lands on a
/// boundary still flips on arbitrarily small noise, on roughly half of all
/// boots, forever; widening the bucket only moves where the knife-edge is.
/// Two further reasons it could never have worked: both sides of the ratio are
/// measured, so the noise is the sum of both devices'; and the reference is a
/// `min` over a SET, so ONE noisy device moves every OTHER device's weight.
///
/// Stability comes from [`crate::rate_store`] instead: unchanged hardware
/// reuses the stored probe result and is never re-measured, so a rate cannot
/// differ between two boots at all. What this constant contributes is
/// legibility — differences too small to be worth distinguishing don't reach
/// the weight.
const RATE_BUCKET_RATIO: f64 = 2.0;

/// A device's share of new renditions, derived entirely from probe-time facts.
///
/// `capacity` is how many encodes it sustains; `rate` is its measured speed on
/// the shared synthetic clip and `reference_rate` the slowest measured device,
/// so the speed term is a pure ratio and no device family is named anywhere.
///
/// Quantised via [`RATE_BUCKET_RATIO`], which discards differences too small to
/// be worth distinguishing. It does NOT make the weight noise-proof — see that
/// constant, and [`crate::rate_store`] for what actually does.
///
/// Buckets are centred ON each power of two (`.round()`, not `.floor()`), so a
/// bucket's edges sit `sqrt(2)` away from its centre — which is symmetric in
/// LOG space, not in percent: **+41.4% above the centre, −29.3% below it**.
/// `.floor()` would instead put a bucket edge AT every power of
/// two — and a rate that measures exactly on one (a plausible, not even
/// unlucky, outcome against a whole-number reference) would flip buckets on
/// noise alone, which is precisely the instability this function exists to
/// prevent.
pub fn device_weight(capacity: usize, rate: Option<f64>, reference_rate: Option<f64>) -> u32 {
    let cap = capacity.clamp(1, 1024) as u32;
    let speed_bucket = match (rate, reference_rate) {
        (Some(r), Some(base)) if r.is_finite() && base.is_finite() && r > 0.0 && base > 0.0 => {
            // How many BUCKET STEPS above the slowest device this one measured.
            let ratio = (r / base).clamp(1.0, 1024.0);
            let steps = ratio.log(RATE_BUCKET_RATIO).round();
            // The multiplier must be raised in the SAME base the step index was
            // computed in. It used to be `2u32.saturating_pow(steps)`, which
            // coincides with the index's base ONLY at `RATE_BUCKET_RATIO == 2.0`:
            // at any smaller ratio the index grows like `log_ratio` while the
            // multiplier grew like 2^index, so anything below ≈1.249 saturated
            // every measured device to `u32::MAX` and collapsed them all to one
            // weight. That is not a hypothetical — it is why disarming this
            // function by setting the constant to 1.01 came back falsely GREEN:
            // both sides of the comparison saturated to the same number. The
            // constant is documented as a granularity knob, so it has to behave
            // like one at every value, not just at the one it currently holds.
            //
            // `as u32` on `f64` saturates (u32::MAX at the top, 0 at the
            // bottom); `.max(1)` turns that floor into the "no evidence of
            // speed" weight rather than an unreachable device.
            (RATE_BUCKET_RATIO.powf(steps) as u32).max(1)
        }
        // No usable measurement: capacity alone. Absence of a rate is a missing
        // observation, never evidence the device is slow.
        _ => 1,
    };
    cap.saturating_mul(speed_bucket).max(1)
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
    /// Probe-derived share of new renditions — see [`device_weight`]. Fixed for
    /// the life of the table: placement must not move while segments are
    /// cached or a client holds an init. PRIVATE for exactly that reason —
    /// "fixed" was only a doc comment while the field was `pub mut`, and a
    /// single stray assignment anywhere in the tree would re-place live
    /// renditions with nothing to catch it. Read it via [`DeviceSlot::weight`].
    weight: u32,
}

impl DeviceSlot {
    fn new(id: DeviceId, capacity: usize, weight: u32) -> Self {
        // At least one permit — a device with a probed cap of 0 would be
        // useless; clamp so the table never holds a dead slot.
        let permits = capacity.max(1);
        Self {
            id,
            sem: Arc::new(Semaphore::new(permits)),
            cooldown_until: None,
            capacity: permits,
            weight: weight.max(1),
        }
    }

    /// This device's probe-derived share of new renditions. Read-only: see the
    /// field's comment for why it may never be reassigned.
    pub fn weight(&self) -> u32 {
        self.weight
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
    ///
    /// Capacity-only weighting: every device weighs its permit count. Used by
    /// callers with no rate measurement (tests, the CLI tool). Placement still
    /// spreads; it just cannot tell a fast device from a slow one.
    pub fn from_probe(caps: &[(DeviceId, usize)], cpu_permits: usize) -> Self {
        let with_rates: SmallVec<[(DeviceId, usize, Option<f64>); 5]> =
            caps.iter().map(|&(d, c)| (d, c, None)).collect();
        Self::from_probe_weighted(&with_rates, cpu_permits, None)
    }

    /// Build from probed `(device, session-cap, measured-rate)` triples.
    ///
    /// The reference rate is the SLOWEST measured device, so every speed term
    /// is a ratio against something real on this machine rather than against a
    /// constant — which is what keeps the weighting hardware-neutral.
    pub fn from_probe_weighted(
        caps: &[(DeviceId, usize, Option<f64>)],
        cpu_permits: usize,
        cpu_rate: Option<f64>,
    ) -> Self {
        // Resolve WHICH entries become slots BEFORE taking the reference. A
        // `caps` entry that is skipped — a `DeviceId::Cpu` row (CPU is appended
        // once, below, with `cpu_rate`) or a duplicate id (first wins) — has no
        // slot of its own, so letting its rate into the `min` would shift every
        // other device's weight on behalf of a device that isn't in the table.
        // Harmless while callers pass clean input; a silent trap the first time
        // one doesn't.
        let mut kept: SmallVec<[(DeviceId, usize, Option<f64>); 5]> = SmallVec::new();
        for &(id, cap, rate) in caps {
            if matches!(id, DeviceId::Cpu) {
                continue;
            }
            if kept.iter().any(|&(k, _, _)| k == id) {
                continue; // first wins — for the RATE as well as the capacity
            }
            kept.push((id, cap, rate));
        }

        let reference = kept
            .iter()
            .map(|&(_, _, r)| r)
            .chain(std::iter::once(cpu_rate))
            .flatten()
            .filter(|r| r.is_finite() && *r > 0.0)
            .fold(None::<f64>, |acc, r| Some(acc.map_or(r, |a: f64| a.min(r))));

        let mut slots: SmallVec<[DeviceSlot; 5]> = SmallVec::new();
        for &(id, cap, rate) in &kept {
            slots.push(DeviceSlot::new(
                id,
                cap,
                device_weight(cap, rate, reference),
            ));
        }
        slots.push(DeviceSlot::new(
            DeviceId::Cpu,
            cpu_permits,
            device_weight(cpu_permits, cpu_rate, reference),
        ));
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
    /// encode, weighted by what the boot probe measured — so it returns the
    /// same answer after a restart on unchanged hardware. An in-memory or
    /// load-aware pin would not, and a rendition re-pinned mid-playback serves
    /// segments that no longer match the client's init (issue #114).
    ///
    /// Purity matters beyond restarts: `SegmentIdentity` carries no device, so
    /// a rendition whose device changed would serve its CACHED segments from
    /// the old encoder beside fresh ones from the new one, under a single init,
    /// with no restart involved at all.
    ///
    /// Every supporting device is in the pool — including software. The #114
    /// rule is "one rendition, one encoder", never "hardware only", and
    /// excluding software made it unreachable for every codec an accelerator
    /// happened to support, however many permits it had and however fast it
    /// was. Weighting (see [`device_weight`]) is what makes that correct on
    /// hardware where software is the stronger device.
    ///
    /// [`device_supports`] remains the eligibility gate, and only that: a
    /// device with no encoder for the codec genuinely cannot produce it. No
    /// device FAMILY appears in the placement itself, so this adapts to
    /// whatever encoders the machine turns out to have.
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
        let supporting: SmallVec<[(DeviceId, u64); 5]> = self
            .slots
            .iter()
            .filter(|s| device_supports(s.id, opts))
            .map(|s| (s.id, u64::from(s.weight())))
            .collect();
        let last = *supporting.last()?;

        // Weighted pick: lay the devices out as adjacent bands whose widths are
        // their weights, and walk the cumulative widths until the key's position
        // falls inside one. Deterministic in the key, the device order and the
        // weights — all three fixed for the life of the table — which is the
        // whole contract.
        let total: u64 = supporting.iter().map(|&(_, w)| w).sum();
        // `DeviceSlot::new` floors every weight at 1, so `total` is at least 1
        // here; `.max(1)` keeps a future zero weight from dividing by zero
        // rather than depending on that floor from a distance.
        let mut pos = rendition_key % total.max(1);
        for &(id, w) in &supporting {
            if pos < w {
                return Some(id);
            }
            pos -= w;
        }
        // Unreachable: the bands tile `[0, total)` exactly.
        Some(last.0)
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

    /// A digest of exactly the state [`DeviceTable::rendition_device`] reads —
    /// the `(device id, weight)` pairs — so that anything able to MOVE a
    /// rendition is visible to whatever keys the cache on it.
    ///
    /// This exists because "the placement rule changed" and "a constant was
    /// bumped" were two different things, and only the second one invalidated
    /// anything. `rendition_device` bands by WEIGHT, and in production the
    /// weight is the boot probe's capacity — which under-reports on a box
    /// already under encode load ([`crate::probe`] says so itself). So a plain
    /// restart during playback could re-band every rendition while
    /// `HLS_GEN_VERSION` sat still: cached segments from the previous encoder,
    /// served under an init the browser holds `immutable` for a year, delivered
    /// with a 200 (issue #114). Deriving the cache generation from this digest
    /// makes that self-invalidating, including for changes nobody remembered to
    /// bump a constant for.
    ///
    /// **In**: every device id and its weight. **Out**: cooldown and current
    /// load (transient, and letting them in would re-place renditions
    /// mid-playback — the exact non-determinism `rendition_device` exists to
    /// remove), and the caller's priority order (a scheduling decision, not a
    /// placement input — hence the sort). Capacity is in only through the
    /// weight it produced: two tables that weigh the same place the same, and
    /// that is the property being digested.
    ///
    /// Order-stable by sorting the rendered pairs, so the same hardware
    /// enumerated in a different order yields the same digest. No device family
    /// is named: ids are hashed as opaque text.
    pub fn placement_fingerprint(&self) -> u64 {
        let mut rendered: Vec<String> = self
            .slots
            .iter()
            .map(|s| format!("{}={}", s.id, s.weight()))
            .collect();
        rendered.sort_unstable();
        xxhash_rust::xxh3::xxh3_64(rendered.join(";").as_bytes())
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

    /// Renditions SPREAD across the devices that can serve them, in proportion
    /// to the weights the probe derived, rather than piling onto one device
    /// while the rest of the machine idles.
    ///
    /// Renamed from `renditions_prefer_hardware_and_spread_across_it`, and its
    /// first assertion replaced, when spec 007 widened the pool. That name and
    /// that assertion ("hardware supports h264, so no rendition should land on
    /// CPU") stated the rule 007 removes: excluding software made it
    /// unreachable for every codec an accelerator supported, however many
    /// permits it had and however fast it was. The half worth guarding is kept
    /// and strengthened — placement is deterministic AND follows the derived
    /// weights, which is what stops any device being starved.
    ///
    /// Asserted against the table's OWN weights, never a written-down ratio, so
    /// it stays true on hardware nobody has run it on.
    ///
    /// The share is taken over the SUPPORTING subset, not over every slot.
    /// `rendition_device` only lays bands for devices `device_supports` admits,
    /// so a table holding one device that cannot encode this codec would make
    /// the whole-table denominator wrong — and the whole-table version of this
    /// test passed only because every slot in `table()` happens to encode
    /// H.264. That is a property of the fixture, not of the implementation.
    #[test]
    fn renditions_spread_across_every_supporting_device_by_weight() {
        let t = table();
        let mut cmaf = h264_opts();
        cmaf.container = Container::Fmp4;

        let supporting: Vec<&DeviceSlot> = t
            .slots()
            .iter()
            .filter(|s| device_supports(s.id, &cmaf))
            .collect();
        let total: u32 = supporting.iter().map(|s| s.weight()).sum();
        // 500 × the supporting total, so the bands tile the sample exactly.
        let n = u64::from(total) * 500;

        let mut seen: std::collections::HashMap<DeviceId, usize> = std::collections::HashMap::new();
        for k in 0..n {
            *seen
                .entry(t.rendition_device(&cmaf, k).expect("a device"))
                .or_default() += 1;
        }

        for slot in &supporting {
            let expected = f64::from(slot.weight()) / f64::from(total);
            let actual = *seen.get(&slot.id).unwrap_or(&0) as f64 / n as f64;
            assert!(
                (actual - expected).abs() < 0.02,
                "{:?}: expected ~{expected:.3} of renditions by weight, got {actual:.3}",
                slot.id
            );
        }
        assert_eq!(
            seen.len(),
            supporting.len(),
            "every supporting device must take a share; got {seen:?}"
        );
        // A device that cannot encode the codec must take none of them.
        for slot in t.slots().iter().filter(|s| !device_supports(s.id, &cmaf)) {
            assert!(
                !seen.contains_key(&slot.id),
                "{:?} cannot encode this codec and must never be placed on",
                slot.id
            );
        }
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
    ///
    /// The CMAF assertion changed when spec 007 widened the pool: it read
    /// `matches!(t.rendition_device(&cmaf, 42), DeviceId::Hw { .. })`, which
    /// held only because software was excluded outright. Key 42 landing on any
    /// particular device is an accident of the weights, so the claim is now
    /// made over enough keys to be a property of the TABLE rather than of one
    /// arbitrary rendition. That each rendition resolves to exactly one device
    /// is asserted by `a_rendition_always_resolves_to_the_same_device`.
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
        // ...and hardware really does serve CMAF renditions: over enough keys
        // every device in the table is reached, hardware included.
        let reached: std::collections::HashSet<DeviceId> = (0..256u64)
            .filter_map(|k| t.rendition_device(&cmaf, k))
            .collect();
        assert!(
            reached.iter().any(|d| matches!(d, DeviceId::Hw { .. })),
            "CMAF H264 must be able to reach hardware; got {reached:?}"
        );
        // Over the SUPPORTING subset — `rendition_device` lays no band for a
        // device that cannot encode the codec, so demanding every slot be
        // reached would be a claim about this fixture (whose devices all encode
        // H.264) rather than about the rule.
        for slot in t.slots().iter().filter(|s| device_supports(s.id, &cmaf)) {
            assert!(
                reached.contains(&slot.id),
                "{:?} never served a CMAF rendition; got {reached:?}",
                slot.id
            );
        }

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

#[cfg(test)]
mod pool_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;
    use crate::options::{AudioCodec, Container, VideoCodec};

    fn gpu() -> DeviceId {
        DeviceId::hw(HwAccel::Nvenc, 0)
    }

    /// `TranscodeOptions` has no `Default`; built field-by-field the way the
    /// other test modules in this file do.
    fn fmp4(video: VideoCodec) -> TranscodeOptions {
        TranscodeOptions {
            source_frame_rate: None,
            container: Container::Fmp4,
            video: Some(video),
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

    fn h264_fmp4() -> TranscodeOptions {
        fmp4(VideoCodec::H264)
    }

    fn vp9_fmp4() -> TranscodeOptions {
        fmp4(VideoCodec::Vp9)
    }

    fn table(gpu_cap: usize, gpu_rate: f64, cpu_permits: usize, cpu_rate: f64) -> DeviceTable {
        DeviceTable::from_probe_weighted(
            &[(gpu(), gpu_cap, Some(gpu_rate))],
            cpu_permits,
            Some(cpu_rate),
        )
    }

    /// THE invariant. Two tables built from the same probe result must place
    /// every rendition identically, or a restart serves segments from a
    /// different encoder than the init a client is holding (#114).
    #[test]
    fn the_same_probe_result_places_every_rendition_identically() {
        let a = table(8, 400.0, 4, 100.0);
        let b = table(8, 400.0, 4, 100.0);
        for key in 0..500u64 {
            assert_eq!(
                a.rendition_device(&h264_fmp4(), key),
                b.rendition_device(&h264_fmp4(), key),
                "rendition {key} moved between two identical tables"
            );
        }
    }

    /// The defect this task fixes: software was unreachable for any codec an
    /// accelerator supported, however many permits it had.
    #[test]
    fn software_takes_a_share_of_work_an_accelerator_could_also_do() {
        let t = table(8, 400.0, 4, 100.0);
        let mut on_cpu = 0;
        for key in 0..1000u64 {
            if t.rendition_device(&h264_fmp4(), key) == Some(DeviceId::Cpu) {
                on_cpu += 1;
            }
        }
        assert!(
            on_cpu > 0,
            "software got no renditions at all — the pool is still hardware-only"
        );
        assert!(on_cpu < 1000, "the accelerator got no renditions at all");
    }

    /// The split must follow the DERIVED weights, never a written-down ratio.
    /// Asserted against the table's own weights so this test stays true on
    /// hardware nobody has run it on.
    #[test]
    fn the_split_follows_the_derived_weights() {
        let t = table(8, 400.0, 4, 100.0);
        let total: u32 = t.slots().iter().map(|s| s.weight()).sum();
        let cpu_weight = t
            .slots()
            .iter()
            .find(|s| s.id == DeviceId::Cpu)
            .map(|s| s.weight())
            .unwrap();
        let expected = f64::from(cpu_weight) / f64::from(total);

        const N: u64 = 4000;
        let on_cpu = (0..N)
            .filter(|k| t.rendition_device(&h264_fmp4(), *k) == Some(DeviceId::Cpu))
            .count();
        let actual = on_cpu as f64 / N as f64;
        assert!(
            (actual - expected).abs() < 0.05,
            "expected ~{expected:.3} of renditions on software, got {actual:.3}"
        );
    }

    /// A codec no accelerator can encode still resolves to software, on a
    /// table where an accelerator exists.
    #[test]
    fn a_codec_no_accelerator_supports_resolves_to_software() {
        let t = table(8, 400.0, 4, 100.0);
        for key in 0..200u64 {
            assert_eq!(t.rendition_device(&vp9_fmp4(), key), Some(DeviceId::Cpu));
        }
    }

    /// Degenerate tables must not panic and must never return `None` for a
    /// codec something can encode.
    #[test]
    fn degenerate_tables_still_place() {
        let software_only = DeviceTable::from_probe_weighted(&[], 1, Some(50.0));
        assert_eq!(
            software_only.rendition_device(&h264_fmp4(), 7),
            Some(DeviceId::Cpu)
        );

        let equal = table(4, 100.0, 4, 100.0);
        assert!(equal.rendition_device(&h264_fmp4(), 7).is_some());

        let unmeasured = DeviceTable::from_probe_weighted(&[(gpu(), 8, None)], 4, None);
        assert!(unmeasured.rendition_device(&h264_fmp4(), 7).is_some());
    }
}

#[cfg(test)]
mod weight_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn weight_of(t: &DeviceTable, id: DeviceId) -> u32 {
        t.slot(id).expect("device is in the table").weight()
    }

    /// Noise tolerance AT A BUCKET CENTRE — the point of MAXIMUM margin.
    ///
    /// Named for what it actually checks. `400/100 = 4` sits exactly on a
    /// bucket centre, so ±41%/−29% of noise is absorbed; this says nothing
    /// about a rate that lands near a boundary, which
    /// [`a_rate_near_a_bucket_boundary_does_flip_on_noise`] characterises
    /// instead. Bucketing buys coarseness, not stability — stability comes from
    /// [`crate::rate_store`] not re-probing unchanged hardware at all.
    #[test]
    fn measurement_noise_does_not_change_a_weight_at_a_bucket_centre() {
        let reference = Some(100.0);
        let base = device_weight(4, Some(400.0), reference);
        for noise in [0.85_f64, 0.95, 1.0, 1.05, 1.15] {
            assert_eq!(
                device_weight(4, Some(400.0 * noise), reference),
                base,
                "a {noise}x measurement changed the weight"
            );
        }
    }

    /// A CHARACTERISATION of the residual risk, deliberately asserting the
    /// unwanted behaviour rather than hiding it.
    ///
    /// `.round()` bucketing does not shrink the zero-margin set, it RELOCATES
    /// it from `{2^k}` to `{2^(k+0.5)}`. A device whose true ratio is ~2.83
    /// (= 2^1.5) sits exactly between two buckets and needs arbitrarily small
    /// noise to flip — so on a fresh probe every boot it would flip on roughly
    /// half of them, forever. No bucket width fixes this; it only moves where
    /// the knife-edge is.
    ///
    /// This is exactly why the probe result is PERSISTED (`crate::rate_store`):
    /// unchanged hardware reuses the stored rates and never re-measures, so the
    /// knife-edge is never stepped on twice. If this test ever goes green by
    /// the flip disappearing, the reasoning behind that persistence has changed
    /// and should be re-derived, not assumed.
    #[test]
    fn a_rate_near_a_bucket_boundary_does_flip_on_noise() {
        let reference = Some(100.0);
        // 283/100 ≈ 2^1.5 — the midpoint between the 2x and 4x buckets.
        let at_boundary = device_weight(1, Some(283.0), reference);
        let one_percent_slower = device_weight(1, Some(283.0 * 0.99), reference);
        assert_eq!(at_boundary, 4, "2.83 rounds up into the 4x bucket");
        assert_eq!(one_percent_slower, 2, "2.80 rounds down into the 2x bucket");
        assert_ne!(
            at_boundary, one_percent_slower,
            "a boundary rate flips on 1% noise — persistence, not bucketing, \
             is what stops that re-placing a rendition"
        );
    }

    /// The reference is the MINIMUM measured rate, so every speed term is a
    /// ratio against the slowest thing on this machine and the slowest device
    /// weighs its capacity alone. A maximum (or any fixed number) would clamp
    /// every ratio to 1 and flatten the table.
    #[test]
    fn the_reference_rate_is_the_slowest_measured_device() {
        let fast = DeviceId::hw(HwAccel::Nvenc, 0);
        let mid = DeviceId::hw(HwAccel::Vaapi, 0);
        let t = DeviceTable::from_probe_weighted(
            &[(fast, 1, Some(400.0)), (mid, 1, Some(200.0))],
            1,
            Some(100.0),
        );
        assert_eq!(weight_of(&t, fast), 4, "400/100 → 4x the slowest device");
        assert_eq!(weight_of(&t, mid), 2, "200/100 → 2x");
        assert_eq!(
            weight_of(&t, DeviceId::Cpu),
            1,
            "the slowest measured device is the unit, so it weighs capacity only"
        );
    }

    /// The compatibility claim `from_probe` makes: callers with no rate
    /// measurement (tests, the CLI tool) still get a working table, weighted on
    /// capacity alone. If this drifts, every existing caller's placement moves.
    #[test]
    fn from_probe_weighs_capacity_alone() {
        let a = DeviceId::hw(HwAccel::Nvenc, 0);
        let b = DeviceId::hw(HwAccel::Vaapi, 0);
        let t = DeviceTable::from_probe(&[(a, 3), (b, 2)], 4);
        assert_eq!(weight_of(&t, a), 3);
        assert_eq!(weight_of(&t, b), 2);
        assert_eq!(weight_of(&t, DeviceId::Cpu), 4);
    }

    /// A duplicate id contributes NOTHING — not its capacity (already covered),
    /// and not its rate. It has no slot of its own, so folding its rate into the
    /// `min` would shift every other device's weight on behalf of a device that
    /// isn't in the table.
    #[test]
    fn a_duplicate_entry_contributes_neither_its_capacity_nor_its_rate() {
        let dev = DeviceId::hw(HwAccel::Nvenc, 0);
        let t = DeviceTable::from_probe_weighted(
            &[(dev, 1, Some(800.0)), (dev, 9, Some(50.0))],
            1,
            Some(100.0),
        );
        assert_eq!(
            t.slots().iter().filter(|s| s.id == dev).count(),
            1,
            "one slot per device id"
        );
        assert_eq!(
            t.slot(dev).expect("slot").capacity,
            1,
            "first wins for capacity"
        );
        // reference = min(800, 100) = 100. NOT 50: the duplicate is not a slot.
        assert_eq!(
            weight_of(&t, dev),
            8,
            "first wins for the RATE too: 800/100"
        );
        assert_eq!(weight_of(&t, DeviceId::Cpu), 1);
    }

    /// The failure mode most likely to bite in production, pinned with the
    /// worked example that motivated persisting the probe result.
    ///
    /// The reference is a `min` over a SET, so a single device's missing
    /// measurement moves every OTHER device's weight — no code touched that
    /// device, and nothing about it changed. CPU 100 / GPU 400 at capacity 4
    /// each weighs 16 and 4. Lose the CPU measurement to a probe timeout and
    /// the reference jumps to 400: both weigh 4, and a 4:1 split has collapsed
    /// to 1:1 from a routine transient.
    ///
    /// This is correct arithmetic on the evidence available — an absent rate is
    /// a missing observation, never proof a device is slow — so the fix is not
    /// here. It is that `crate::rate_store` does not re-probe unchanged
    /// hardware, so a transient timeout cannot reach a second boot and re-place
    /// live renditions.
    #[test]
    fn one_missing_rate_moves_every_other_devices_weight() {
        let gpu = DeviceId::hw(HwAccel::Nvenc, 0);

        let measured = DeviceTable::from_probe_weighted(&[(gpu, 4, Some(400.0))], 4, Some(100.0));
        assert_eq!(weight_of(&measured, gpu), 16, "cap 4 × 4x bucket");
        assert_eq!(weight_of(&measured, DeviceId::Cpu), 4, "cap 4 × 1x bucket");

        // Same hardware; the CPU probe timed out.
        let cpu_timed_out = DeviceTable::from_probe_weighted(&[(gpu, 4, Some(400.0))], 4, None);
        assert_eq!(
            weight_of(&cpu_timed_out, gpu),
            4,
            "the GPU is now the slowest MEASURED device, so its ratio is 1"
        );
        assert_eq!(weight_of(&cpu_timed_out, DeviceId::Cpu), 4);
        assert_eq!(
            weight_of(&cpu_timed_out, gpu),
            weight_of(&cpu_timed_out, DeviceId::Cpu),
            "4:1 collapsed to 1:1 — the transient that persistence exists to \
             keep out of a second boot"
        );
    }

    /// A faster device must weigh more than a slower one at equal capacity,
    /// and MORE CAPACITY must weigh more at equal speed. Both directions, or
    /// the weight is not measuring what it claims.
    #[test]
    fn weight_rises_with_both_speed_and_capacity() {
        let r = Some(100.0);
        assert!(device_weight(4, Some(400.0), r) > device_weight(4, Some(100.0), r));
        assert!(device_weight(8, Some(100.0), r) > device_weight(4, Some(100.0), r));
    }

    /// The case that makes this portable rather than a second hardware
    /// assumption: a machine whose SOFTWARE encoder is the stronger device.
    /// Nothing may prevent it outweighing an accelerator.
    #[test]
    fn a_fast_software_encoder_can_outweigh_a_slow_accelerator() {
        let reference = Some(500.0);
        let software = device_weight(16, Some(500.0), reference);
        let accelerator = device_weight(2, Some(120.0), reference);
        assert!(
            software > accelerator,
            "software {software} did not outweigh a slower accelerator {accelerator}"
        );
    }

    /// An unmeasurable device must still be placeable, weighted on capacity
    /// alone. A `None` rate is a missing measurement, not a zero-speed device.
    #[test]
    fn an_unmeasured_device_falls_back_to_capacity() {
        assert!(device_weight(4, None, Some(100.0)) > 0);
        assert!(device_weight(8, None, None) > device_weight(4, None, None));
    }

    /// Degenerate inputs must not panic or produce a zero weight, which would
    /// make a device unreachable rather than merely unlikely.
    #[test]
    fn degenerate_inputs_still_yield_a_usable_weight() {
        for (cap, rate, refr) in [
            (0usize, None, None),
            (1, Some(0.0), Some(0.0)),
            (1, Some(f64::NAN), Some(100.0)),
            (1, Some(f64::INFINITY), Some(100.0)),
            (usize::MAX, Some(1e12), Some(1.0)),
        ] {
            let w = device_weight(cap, rate, refr);
            assert!(w > 0, "cap={cap} rate={rate:?} ref={refr:?} gave weight 0");
        }
    }
}

/// [`DeviceTable::placement_fingerprint`] — the digest that lets a cache
/// invalidate itself when placement moves.
///
/// The property under test is narrow and load-bearing: the digest must change
/// when, and only when, something that can move a rendition changes. Too
/// sensitive and every restart wipes a 40 GiB cache; not sensitive enough and
/// #114 ships as a 200.
#[cfg(test)]
mod placement_fingerprint_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;
    use crate::protocol::DeviceId;
    use std::time::Duration;

    fn gpu() -> DeviceId {
        DeviceId::hw(HwAccel::Nvenc, 0)
    }

    /// Same `(id, weight)` pairs → same digest, even when the two tables were
    /// built through different constructors. The digest keys on what placement
    /// READS, not on how the table was made: a capacity-only table and a
    /// rate-weighted one whose ratio bucket is 1 weigh identically, so they must
    /// place identically and must NOT wipe each other's cache.
    #[test]
    fn identical_weights_hash_identically() {
        let capacity_only = DeviceTable::from_probe(&[(gpu(), 4)], 4);
        let rate_weighted =
            DeviceTable::from_probe_weighted(&[(gpu(), 4, Some(100.0))], 4, Some(100.0));

        // Precondition — the two really do weigh the same, or the assertion
        // below would be about the constructor rather than about the weights.
        let weights = |t: &DeviceTable| -> Vec<(DeviceId, u32)> {
            t.slots().iter().map(|s| (s.id, s.weight())).collect()
        };
        assert_eq!(weights(&capacity_only), weights(&rate_weighted));

        assert_eq!(
            capacity_only.placement_fingerprint(),
            rate_weighted.placement_fingerprint(),
            "tables that place identically must not invalidate each other"
        );
    }

    /// A capacity drift of ONE moves the bands, so it must move the digest.
    ///
    /// This is the production failure, not a hypothetical: `probe_device_caps`
    /// ramps trial encodes until one fails and documents that a box already
    /// under encode load under-reports. Boot A measuring 4 and boot B measuring
    /// 3 re-bands every rendition whose key position falls between the two
    /// grids — and before this digest existed, nothing downstream could tell.
    #[test]
    fn a_capacity_drift_of_one_changes_the_digest() {
        let boot_a = DeviceTable::from_probe(&[(gpu(), 4)], 4);
        let boot_b = DeviceTable::from_probe(&[(gpu(), 3)], 4);
        assert_ne!(
            boot_a.placement_fingerprint(),
            boot_b.placement_fingerprint(),
            "an under-reported probe re-places renditions; the cache must know"
        );
    }

    /// Adding or removing a device changes the pool, so it changes the digest.
    #[test]
    fn changing_the_device_set_changes_the_digest() {
        let one = DeviceTable::from_probe(&[(gpu(), 4)], 4);
        let two = DeviceTable::from_probe(&[(gpu(), 4), (DeviceId::hw(HwAccel::Vaapi, 0), 4)], 4);
        assert_ne!(one.placement_fingerprint(), two.placement_fingerprint());
    }

    /// The caller's enumeration order is a scheduling preference, not a
    /// hardware fact. Two tables holding the same devices in different order
    /// place the same renditions on the same devices only if the ORDER is also
    /// the same — but the digest exists to detect hardware change, and a
    /// reshuffled `/dev/dri` scan must not masquerade as one.
    ///
    /// (Order does reach `rendition_device` through the band layout; it is
    /// deliberately out of the digest because `enumerate` is itself sorted, so
    /// the order is a function of the device set. Were that to stop being true,
    /// this test is where the assumption is written down.)
    #[test]
    fn the_device_order_does_not_change_the_digest() {
        let a = DeviceTable::from_probe(&[(gpu(), 3), (DeviceId::hw(HwAccel::Vaapi, 0), 2)], 4);
        let b = DeviceTable::from_probe(&[(DeviceId::hw(HwAccel::Vaapi, 0), 2), (gpu(), 3)], 4);
        assert_eq!(
            a.placement_fingerprint(),
            b.placement_fingerprint(),
            "enumeration order is not a hardware change"
        );
    }

    /// Cooldown and in-flight load are transient. If either reached the digest,
    /// a single `DeviceBusy` would wipe the whole transcode cache and re-place
    /// every live rendition — the failure this machinery exists to prevent,
    /// caused by the machinery itself.
    #[test]
    fn transient_state_stays_out_of_the_digest() {
        let mut t = DeviceTable::from_probe(&[(gpu(), 4)], 4);
        let before = t.placement_fingerprint();
        t.set_cooldown(gpu(), Instant::now() + Duration::from_secs(60));
        assert_eq!(before, t.placement_fingerprint(), "cooldown is transient");

        let permit = t
            .slot(gpu())
            .expect("gpu slot")
            .sem
            .clone()
            .try_acquire_owned()
            .expect("a free permit");
        assert_eq!(before, t.placement_fingerprint(), "load is transient");
        drop(permit);
    }
}
