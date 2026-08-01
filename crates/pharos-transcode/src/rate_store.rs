//! Persisted encode-rate probe results — what actually makes placement stable.
//!
//! [`crate::device::device_weight`] turns a measured encode rate into a weight,
//! and a weight decides which device a shared-init fMP4 rendition resolves to.
//! `SegmentIdentity` does **not** include the device, so a rendition that
//! re-places serves cached segments from the old encoder beside fresh ones from
//! the new one, under a single init — undecodable video, delivered with a 200
//! (issue #114). The invariant that has to hold is therefore strong:
//!
//! > two constructions from the same probe result, including across a process
//! > restart on unchanged hardware, place every rendition identically.
//!
//! **Bucketing cannot deliver that**, and this module exists because it can't:
//!
//! - Rounding to buckets does not shrink the zero-margin set, it RELOCATES it
//!   (from `{2^k}` to `{2^(k+0.5)}`). A device sitting on a boundary flips on
//!   arbitrarily small noise, on roughly half of all boots, forever.
//! - Both sides of the ratio are measured, so the log-ratio's variance is the
//!   sum of both devices' noise.
//! - The reference is a `min` over a SET, so ONE noisy device re-places
//!   renditions on every OTHER device.
//! - Worst: a probe TIMEOUT returns `None`, which shifts the reference
//!   discontinuously on hardware that did not change at all. Worked example —
//!   CPU 100, GPU 400, both capacity 4: normally the reference is 100 and the
//!   weights are 16 and 4. If the CPU probe times out the reference becomes
//!   400, and both weigh 4. A 4:1 split collapses to 1:1 from a routine
//!   transient. (Pinned by
//!   `device::weight_tests::one_missing_rate_moves_every_other_devices_weight`.)
//!
//! So the probe result is stored, keyed by a fingerprint of the device set. On
//! unchanged hardware the stored rates are reused and **no probe runs at all**:
//! same rates, same weights, same placement, by construction rather than by
//! probability — and boot gets faster as a side effect. Bucketing is kept, but
//! for what it is actually good at: making the weight insensitive to
//! differences too small to be worth distinguishing.
//!
//! **Scope limit, stated rather than implied.** When the fingerprint DOES
//! differ, renditions re-place — which is correct, because the device set they
//! were placed into no longer exists — but a client holding an init from before
//! the hardware change still holds a stale one, and cached segments for a
//! re-placed rendition are still from the previous encoder. Closing that needs
//! cache invalidation plus getting the client to re-fetch its init, which is
//! out of scope here. This module narrows the hazard from "any boot" to "a boot
//! where the hardware genuinely changed"; it does not remove it.

use crate::protocol::DeviceId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// On-disk format version. Bumping it invalidates every stored result (the
/// fingerprint won't be readable), which is the intended behaviour for a format
/// change: re-probe rather than misread.
const RATE_STORE_VERSION: u32 = 1;

/// Filename under the store directory. Dot-prefixed and parked beside the
/// transcode cache, following the `.gen_version` marker the HLS cache already
/// writes and compares at construction.
///
/// Public because the HLS cache has to name it: `reconcile_generation` deletes
/// every entry in that root except its own marker, so a generation change would
/// otherwise take the rate store with it — forcing a re-probe on exactly the
/// boot where placement stability matters most, and re-placing renditions for a
/// second, avoidable reason. The cache preserves this file by name instead.
pub const RATE_STORE_FILE: &str = ".device_rates.json";

/// A stable digest of the device SET — the thing whose change genuinely should
/// re-place renditions.
///
/// **In**: every device id (encoder family + GPU ordinal) and its permit
/// capacity, order-independent. Adding, removing, or re-sizing a device changes
/// it.
///
/// **Deliberately out**:
/// - the measured rates themselves — they are the payload, and keying on them
///   would make every result its own key and never reuse anything;
/// - which codecs each device can encode — that is probed separately and does
///   not enter the weight;
/// - cooldown and load — transient by definition, and letting them in would
///   reintroduce exactly the non-determinism this exists to remove;
/// - the ffmpeg build. An upgrade can genuinely shift relative rates, but
///   re-placing every rendition on every ffmpeg bump is a worse trade than
///   carrying slightly stale ratios. Delete the stored file to force a
///   re-probe.
///
/// No device family is named in any decision here: the id is hashed as opaque
/// data, and the sort is over its rendering, not over a preference order.
pub fn fingerprint(devices: &[(DeviceId, usize)]) -> u64 {
    let mut rendered: Vec<String> = devices
        .iter()
        .map(|(id, cap)| format!("{id}={cap}"))
        .collect();
    // Sorted so the caller's priority order — which is a scheduling decision,
    // not a hardware fact — cannot change the key.
    rendered.sort_unstable();
    rendered.dedup();
    xxhash_rust::xxh3::xxh3_64(rendered.join(";").as_bytes())
}

#[derive(Serialize, Deserialize)]
struct StoredRate {
    device: DeviceId,
    /// `None` = the probe ran and could not measure this device (timeout,
    /// spawn failure, an encode lost in per-invocation overhead). Persisted as
    /// deliberately as a measurement: see [`RateStore::rates_for`].
    rate: Option<f64>,
}

#[derive(Serialize, Deserialize)]
struct StoredRates {
    version: u32,
    /// Hex so the file stays legible to a human deciding whether to delete it.
    fingerprint: String,
    rates: Vec<StoredRate>,
}

/// The persisted probe result, one file in a directory.
pub struct RateStore {
    path: PathBuf,
}

impl RateStore {
    /// Store the result in `dir` — pass the transcode cache directory, so the
    /// probe result ages out with the segments whose placement it decided.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(RATE_STORE_FILE),
        }
    }

    /// The file this store reads and writes. Public so an operator can be told
    /// exactly what to delete to force a re-probe.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rates for `devices`, in the order given — reused from disk when the
    /// device set is unchanged, otherwise probed and then stored.
    ///
    /// `devices` must include EVERY device that will get a slot, CPU included:
    /// the CPU's rate is one of the measurements the reference `min` is taken
    /// over, so leaving it out would key the store on a different set than the
    /// one being weighted.
    ///
    /// A `None` in a stored result is reused as-is rather than re-probed. That
    /// does freeze a transient failure until the hardware changes — the cost is
    /// real, and the file is deletable. The alternative is worse: re-probing
    /// only the unmeasured devices is precisely the discontinuous reference
    /// shift described at the top of this module, and it would fire on some
    /// boots and not others. A consistently pessimistic table is safe; an
    /// inconsistent one corrupts video.
    ///
    /// Nothing here is fatal. A missing file is a first boot, a corrupt or
    /// unreadable one is reported at WARN and falls back to probing, and a
    /// failed write costs the next boot a re-probe and nothing else.
    pub async fn rates_for<F, Fut>(
        &self,
        devices: &[(DeviceId, usize)],
        probe: F,
    ) -> Vec<(DeviceId, Option<f64>)>
    where
        F: Fn(DeviceId) -> Fut,
        Fut: std::future::Future<Output = Option<f64>>,
    {
        let fp = fingerprint(devices);
        if let Some(reused) = self.load(fp, devices) {
            tracing::info!(
                fingerprint = format!("{fp:016x}"),
                devices = devices.len(),
                path = %self.path.display(),
                "reusing the stored encode-rate probe: device set unchanged, so \
                 every rendition keeps the device it was placed on"
            );
            return reused;
        }

        let mut measured: Vec<(DeviceId, Option<f64>)> = Vec::with_capacity(devices.len());
        for &(id, _) in devices {
            let rate = probe(id).await;
            // Symmetry: a measured rate and a failed measurement are both
            // decisions about placement and both get recorded.
            match rate {
                Some(r) => tracing::info!(device = %id, rate_fps = r, "encode rate probed"),
                None => tracing::warn!(
                    device = %id,
                    "encode rate NOT measurable — this device will weigh its \
                     capacity alone, and it also drops out of the reference \
                     minimum, which shifts every other device's weight"
                ),
            }
            measured.push((id, rate));
        }
        self.save(fp, &measured);
        measured
    }

    /// Stored rates for this exact device set, or `None` when there is nothing
    /// usable to reuse.
    fn load(&self, fp: u64, devices: &[(DeviceId, usize)]) -> Option<Vec<(DeviceId, Option<f64>)>> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A first boot is not a failure and must not look like one.
                tracing::debug!(path = %self.path.display(), "no stored encode-rate probe yet");
                return None;
            }
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %e,
                    "stored encode-rate probe unreadable — re-probing; placement \
                     may move if the measured rates differ"
                );
                return None;
            }
        };

        let stored: StoredRates = match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %e,
                    bytes = raw.len(),
                    "stored encode-rate probe is corrupt — re-probing; placement \
                     may move if the measured rates differ"
                );
                return None;
            }
        };

        if stored.version != RATE_STORE_VERSION {
            tracing::warn!(
                path = %self.path.display(),
                found = stored.version,
                expected = RATE_STORE_VERSION,
                "stored encode-rate probe is a different format version — re-probing"
            );
            return None;
        }

        let want = format!("{fp:016x}");
        if stored.fingerprint != want {
            tracing::info!(
                path = %self.path.display(),
                stored = %stored.fingerprint,
                current = %want,
                "device set changed since the last probe — re-probing, and \
                 renditions may re-place onto the new set"
            );
            return None;
        }

        // Belt and braces: the fingerprint already implies this, but a file
        // that matches the key and yet doesn't cover every device would leave a
        // device silently unmeasured. Refuse it rather than half-reuse it.
        let mut out = Vec::with_capacity(devices.len());
        for &(id, _) in devices {
            match stored.rates.iter().find(|r| r.device == id) {
                Some(r) => out.push((id, sanitised(r.rate))),
                None => {
                    tracing::warn!(
                        path = %self.path.display(),
                        device = %id,
                        "stored encode-rate probe matches the fingerprint but is \
                         missing a device — re-probing"
                    );
                    return None;
                }
            }
        }
        Some(out)
    }

    /// Best-effort write. Written to a sibling temp file and renamed so a torn
    /// write can't leave a half-file that the next boot has to reject.
    fn save(&self, fp: u64, rates: &[(DeviceId, Option<f64>)]) {
        let doc = StoredRates {
            version: RATE_STORE_VERSION,
            fingerprint: format!("{fp:016x}"),
            rates: rates
                .iter()
                .map(|&(device, rate)| StoredRate { device, rate })
                .collect(),
        };
        if let Err(e) = self.write_atomically(&doc) {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "could not store the encode-rate probe — the next boot re-probes, \
                 which is slower and may re-place renditions"
            );
        }
    }

    fn write_atomically(&self, doc: &StoredRates) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(doc)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// A stored rate that is not a usable positive finite number is treated as no
/// measurement. JSON can carry a hand-edited `0` or a negative; `device_weight`
/// already rejects them, and normalising here keeps the reference `min` from
/// having to.
fn sanitised(rate: Option<f64>) -> Option<f64> {
    rate.filter(|r| r.is_finite() && *r > 0.0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn gpu() -> DeviceId {
        DeviceId::hw(HwAccel::Nvenc, 0)
    }

    fn vaapi() -> DeviceId {
        DeviceId::hw(HwAccel::Vaapi, 0)
    }

    /// A stand-in for [`crate::probe::probe_encode_rate`] that RECORDS how many
    /// times it ran, so "did not probe" is an assertion rather than an
    /// inference from equal outputs — two identical probe runs would look the
    /// same as a reuse.
    struct CountingProbe {
        calls: AtomicUsize,
        /// Devices this probe cannot measure — the timeout case.
        unmeasurable: Vec<DeviceId>,
    }

    impl CountingProbe {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                unmeasurable: Vec::new(),
            }
        }

        fn cannot_measure(mut self, d: DeviceId) -> Self {
            self.unmeasurable.push(d);
            self
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn run(&self, id: DeviceId) -> Option<f64> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.unmeasurable.contains(&id) {
                return None;
            }
            match id {
                DeviceId::Cpu => Some(100.0),
                DeviceId::Hw { .. } => Some(400.0),
            }
        }

        /// The `Fn(DeviceId) -> impl Future` shape `rates_for` takes.
        fn as_probe(&self) -> impl Fn(DeviceId) -> std::future::Ready<Option<f64>> + '_ {
            move |d| std::future::ready(self.run(d))
        }
    }

    /// THE invariant this module exists for: on an unchanged device set the
    /// second construction reuses the stored rates and does not probe at all.
    /// Identical rates ⇒ identical weights ⇒ identical placement, by
    /// construction rather than by the probe happening to agree with itself.
    #[tokio::test]
    async fn a_matching_fingerprint_reuses_stored_rates_without_probing() {
        let dir = tempfile::tempdir().unwrap();
        let devices = [(gpu(), 4), (vaapi(), 2), (DeviceId::Cpu, 4)];

        // One device is unmeasurable on purpose: a `None` must round-trip as a
        // `None`, or reuse silently changes the reference minimum and re-places
        // every other device's renditions.
        let first_probe = CountingProbe::new().cannot_measure(vaapi());
        let first = RateStore::new(dir.path())
            .rates_for(&devices, first_probe.as_probe())
            .await;
        assert_eq!(first_probe.calls(), 3, "a first boot probes every device");
        assert_eq!(first[0], (gpu(), Some(400.0)));
        assert_eq!(first[1], (vaapi(), None));
        assert_eq!(first[2], (DeviceId::Cpu, Some(100.0)));

        // The restart.
        let second_probe = CountingProbe::new();
        let second = RateStore::new(dir.path())
            .rates_for(&devices, second_probe.as_probe())
            .await;
        assert_eq!(
            second_probe.calls(),
            0,
            "an unchanged device set must not re-probe — a second measurement is \
             the only way placement can move on unchanged hardware"
        );
        assert_eq!(second, first, "reused rates must be the stored rates");
    }

    /// A changed device set MUST re-probe: the rates on file describe hardware
    /// that is no longer there, and re-placing into the new set is correct.
    #[tokio::test]
    async fn a_changed_fingerprint_reprobes() {
        let dir = tempfile::tempdir().unwrap();
        let before = [(gpu(), 4), (DeviceId::Cpu, 4)];
        let seed = CountingProbe::new();
        RateStore::new(dir.path())
            .rates_for(&before, seed.as_probe())
            .await;
        assert_eq!(seed.calls(), 2);

        // Same devices, one re-sized — a capacity is part of the key.
        let resized = [(gpu(), 8), (DeviceId::Cpu, 4)];
        let after_resize = CountingProbe::new();
        RateStore::new(dir.path())
            .rates_for(&resized, after_resize.as_probe())
            .await;
        assert_eq!(
            after_resize.calls(),
            2,
            "a capacity change is a device-set change and must re-probe"
        );

        // A device removed.
        let cpu_only = [(DeviceId::Cpu, 4)];
        let after_removal = CountingProbe::new();
        RateStore::new(dir.path())
            .rates_for(&cpu_only, after_removal.as_probe())
            .await;
        assert_eq!(
            after_removal.calls(),
            1,
            "losing a device must re-probe the ones that are left"
        );
    }

    /// The caller's priority order is a scheduling decision, not a hardware
    /// fact, so it must not invalidate a stored result.
    #[tokio::test]
    async fn listing_the_same_devices_in_another_order_still_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let a = [(gpu(), 4), (DeviceId::Cpu, 4)];
        let b = [(DeviceId::Cpu, 4), (gpu(), 4)];
        assert_eq!(fingerprint(&a), fingerprint(&b));

        let seed = CountingProbe::new();
        RateStore::new(dir.path())
            .rates_for(&a, seed.as_probe())
            .await;
        let reordered = CountingProbe::new();
        let out = RateStore::new(dir.path())
            .rates_for(&b, reordered.as_probe())
            .await;
        assert_eq!(reordered.calls(), 0);
        // ...and the reused rates come back in the ORDER ASKED FOR, not the
        // order they were stored in — the caller zips this against its own list.
        assert_eq!(out[0], (DeviceId::Cpu, Some(100.0)));
        assert_eq!(out[1], (gpu(), Some(400.0)));
    }

    /// A corrupt file must not be fatal: it falls back to probing so boot
    /// completes, and it is replaced so the next boot reuses again.
    #[tokio::test]
    async fn a_corrupt_file_falls_back_to_probing() {
        let dir = tempfile::tempdir().unwrap();
        let store = RateStore::new(dir.path());
        std::fs::write(store.path(), b"{ not json at all").unwrap();

        let devices = [(gpu(), 4), (DeviceId::Cpu, 4)];
        let probe = CountingProbe::new();
        let out = store.rates_for(&devices, probe.as_probe()).await;
        assert_eq!(probe.calls(), 2, "a corrupt file must degrade to a probe");
        assert_eq!(out[0], (gpu(), Some(400.0)));

        let after = CountingProbe::new();
        RateStore::new(dir.path())
            .rates_for(&devices, after.as_probe())
            .await;
        assert_eq!(after.calls(), 0, "the corrupt file must have been replaced");
    }

    /// A file that matches the key but doesn't cover every device is refused
    /// whole rather than half-reused — a silently unmeasured device shifts the
    /// reference minimum for everything else.
    #[tokio::test]
    async fn a_result_missing_a_device_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let devices = [(gpu(), 4), (DeviceId::Cpu, 4)];
        let store = RateStore::new(dir.path());
        store.write_raw(
            &devices,
            RATE_STORE_VERSION,
            &[(DeviceId::Cpu, Some(100.0))],
        );

        let probe = CountingProbe::new();
        store.rates_for(&devices, probe.as_probe()).await;
        assert_eq!(probe.calls(), 2);
    }

    /// An older format version is not readable as this one, so it must be
    /// re-probed rather than misread.
    #[tokio::test]
    async fn a_different_format_version_reprobes() {
        let dir = tempfile::tempdir().unwrap();
        let devices = [(DeviceId::Cpu, 4)];
        let store = RateStore::new(dir.path());
        store.write_raw(
            &devices,
            RATE_STORE_VERSION + 1,
            &[(DeviceId::Cpu, Some(100.0))],
        );

        let probe = CountingProbe::new();
        store.rates_for(&devices, probe.as_probe()).await;
        assert_eq!(probe.calls(), 1);
    }

    /// A write that cannot land costs the next boot a re-probe and nothing
    /// more — boot itself still completes with a full set of rates.
    #[tokio::test]
    async fn a_failed_write_is_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        // A FILE where the store wants a DIRECTORY: `create_dir_all` fails, so
        // the write cannot succeed by any route.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();

        let devices = [(DeviceId::Cpu, 4)];
        let probe = CountingProbe::new();
        let out = RateStore::new(blocked.join("sub"))
            .rates_for(&devices, probe.as_probe())
            .await;
        assert_eq!(
            out,
            vec![(DeviceId::Cpu, Some(100.0))],
            "boot still gets a full set of rates"
        );
        assert_eq!(probe.calls(), 1);

        // Next boot: still no file, so it probes again. Slower, not broken.
        let again = CountingProbe::new();
        RateStore::new(blocked.join("sub"))
            .rates_for(&devices, again.as_probe())
            .await;
        assert_eq!(again.calls(), 1);
    }

    /// A hand-edited or otherwise nonsensical stored rate is treated as no
    /// measurement rather than as a real one.
    #[tokio::test]
    async fn a_nonsense_stored_rate_reads_back_as_unmeasured() {
        let dir = tempfile::tempdir().unwrap();
        let devices = [(DeviceId::Cpu, 4)];
        let store = RateStore::new(dir.path());
        store.write_raw(&devices, RATE_STORE_VERSION, &[(DeviceId::Cpu, Some(-1.0))]);

        let probe = CountingProbe::new();
        let out = store.rates_for(&devices, probe.as_probe()).await;
        assert_eq!(probe.calls(), 0, "the file is valid, so it is still reused");
        assert_eq!(out, vec![(DeviceId::Cpu, None)], "but the rate is unusable");
    }

    impl RateStore {
        /// Plant an exact on-disk document, so a test can express a file this
        /// module would never write itself.
        fn write_raw(
            &self,
            devices: &[(DeviceId, usize)],
            version: u32,
            rates: &[(DeviceId, Option<f64>)],
        ) {
            let doc = StoredRates {
                version,
                fingerprint: format!("{:016x}", fingerprint(devices)),
                rates: rates
                    .iter()
                    .map(|&(device, rate)| StoredRate { device, rate })
                    .collect(),
            };
            std::fs::write(self.path(), serde_json::to_vec(&doc).unwrap()).unwrap();
        }
    }
}
