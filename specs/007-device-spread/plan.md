# 007 Device Spread — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let every encoder the machine actually has take shared-init fMP4 work,
by replacing `rendition_device`'s hardware-only pool with a probe-weighted pool
over every supporting device — without ever letting a rendition's device change.

**Architecture:** Placement stays a **pure function of the rendition and the
probe result**, because `SegmentIdentity` has no device field and a pin that
moved would serve cached segments from one encoder beside fresh ones from another
under the same init. Adaptivity to unknown hardware therefore comes from the
**boot probe** (permit count × measured relative encode rate, quantised so noise
cannot shift a placement), and adaptivity to changing load stays where 006 put it
(the learned per-device admission allowance). Nothing is configured; nothing names
a device family.

**Tech Stack:** Rust · tokio · `metrics` + Prometheus · `tracing` · `cargo nextest`
inside the Nix devShell.

## Global Constraints

- **Every command runs inside the Nix devShell.** Prefix with
  `nix develop --command`. Never invoke `cargo`, `clippy` or `ffmpeg` from the
  host shell.
- **Tests run via `cargo nextest run`**, not `cargo test`.
- **Atomic commits**: one granular thing each; reverting one alone must leave the
  project compiling. Never squash.
- `clippy::unwrap_used` / `expect_used` are `deny` at workspace level; test
  modules opt out with `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
- **No device family may appear in a decision.** No `matches!(d, DeviceId::Hw{..})`
  in placement or preference logic, no vendor name, no hardcoded ratio. Grep for
  these at the end; their absence is part of the deliverable.
- **V126 governs this work**: a performance tunable that must know the hardware is
  a defect. Every number is derived from the probe.
- **The pin may never move for a live rendition.** Any change that lets a
  rendition resolve differently across two constructions from the same probe
  result is a Critical defect, not a tuning question.
- **Metric labels are a dashboard contract**: bounded cardinality, stable strings
  from a `label()` method, asserted distinct in a test.
- **ODD**: name the query before the fix; instrument the decision, not just the
  error; every new metric assertion **disarm-verified** (delete it, watch the test
  go red, restore).
- Ids are append-only. Next free bug id is **B179**; next free invariant id is
  **V129**.
- After any `Cargo.toml` dependency change run `just hakari-regen`, and verify
  with `just hakari-check`.

---

## File Structure

| File | Responsibility | Task |
|---|---|---|
| `crates/pharos-transcode/src/probe.rs` | Gains a **timed** synthetic encode returning a comparable per-device rate, including a software arm (`codec_probe_args` returns `None` for CPU today). | 1 |
| `crates/pharos-transcode/src/device.rs` | `DeviceSlot` gains a probe-derived `weight`; `rendition_device`'s pool becomes every supporting device, weighted; `eligible_for` gains a preference-ordered variant. | 2, 3, 5 |
| `crates/pharos-transcode/src/scheduler.rs` | Emits `pharos_transcode_rendition_pin_total`; uses the preference-ordered candidate list for unpinned work. | 4, 5 |
| `crates/pharos-server/src/main.rs` | Runs the rate probe on the existing boot pass and feeds it into `from_probe`. | 4 |
| `crates/pharos-server/src/router.rs` | Publishes `pharos_transcode_device_weight` beside the existing capacity gauge. | 4 |
| `specs/001-pharos-baseline/invariants.md` | **V129** — placement is probe-derived and immutable; load management is learned. | 6 |

`device.rs` is the natural home for weighting: it already owns capacity, cooldown
and eligibility, and `rendition_device` lives there. Do not put weighting in
`scheduler.rs`, which is 2 400+ lines and owns dispatch, not device facts.

---

## Task 1: A timed synthetic encode, per device

**Files:**
- Modify: `crates/pharos-transcode/src/probe.rs`
- Test: inline `#[cfg(test)] mod tests` in `probe.rs`

**Interfaces:**
- Consumes: `run_ffmpeg_probe(&str, &[String], Duration) -> bool` (`probe.rs:148`),
  `codec_probe_args(DeviceId, VideoCodec) -> Option<Vec<String>>` (`probe.rs:189`),
  `ffmpeg_bin()`.
- Produces, relied on by Tasks 2 and 4:
  - `pub async fn probe_encode_rate(device: DeviceId, timeout: Duration) -> Option<f64>`
    — frames per second on a fixed synthetic clip, `None` when the device cannot
    be measured.

**Why a ratio is legitimate here when 006 said a trial encode is a bad measure.**
006 established that a boot-time trial encode is a bad measure of *absolute*
segment cost, because real cost is decode- and I/O-bound. That stands. This
measures a **ratio between devices on identical synthetic input**, where decode
and I/O are common-mode and cancel. Put that reasoning in the doc comment — the
next reader will otherwise think it contradicts V126's sibling argument.

- [ ] **Step 1: Write the failing test**

Add to `probe.rs`:

```rust
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
}
```

The 3.0 margin is deliberately loose: this runs on shared CI. It is not the
stability guarantee — Task 2's quantisation is. This test exists to catch a probe
that is wildly non-deterministic, which would make quantisation impossible.

- [ ] **Step 2: Run it to verify it fails**

```bash
nix develop --command cargo nextest run -p pharos-transcode probe_encode_rate
```

Expected: **compile failure**, `cannot find function 'probe_encode_rate'`.

- [ ] **Step 3: Implement**

Add to `probe.rs`:

```rust
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
    let started = std::time::Instant::now();
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
    a.push(format!("testsrc2=size=1280x720:rate=30:duration={}", frames as f64 / 30.0));
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
```

If `accel.video_encoder` is not the correct accessor name, find the real one —
`codec_probe_args` at `probe.rs:194` calls it, so copy from there rather than
guessing.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
nix develop --command cargo nextest run -p pharos-transcode rate_tests
```

Expected: **2 passed**. If the software probe takes more than a few seconds,
reduce `RATE_PROBE_FRAMES` — but keep it identical across devices.

- [ ] **Step 5: Lint**

```bash
nix develop --command cargo clippy -p pharos-transcode --all-targets -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add crates/pharos-transcode/src/probe.rs
git commit -m "feat(transcode): measure how fast each device encodes the same clip

Placement needs to know which of a machine's encoders is actually faster, and
nothing measures that today -- the existing probes answer "does this device
work" and "which codecs does it accept", not "how fast".

Deliberately a RATIO instrument. 006 found a boot-time trial encode is a poor
measure of real segment cost, because cost is decode- and I/O-bound; that stands.
Here every device encodes the same synthetic input, so decode and source I/O are
common-mode and cancel. The absolute number means little; the ratio is the point,
and it is the only hardware-neutral way to learn that a many-core software
encoder outruns a weak accelerator on the box this happens to be installed on.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 2: A quantised, probe-derived device weight

**Files:**
- Modify: `crates/pharos-transcode/src/device.rs`
- Test: inline test module in `device.rs`

**Interfaces:**
- Consumes: `DeviceSlot::capacity`, `probe_encode_rate` (Task 1).
- Produces, relied on by Tasks 3 and 4:
  - `pub fn device_weight(capacity: usize, rate: Option<f64>, reference_rate: Option<f64>) -> u32`
  - `DeviceSlot.weight: u32`, and `DeviceTable::from_probe_weighted(caps: &[(DeviceId, usize, Option<f64>)], cpu_permits: usize, cpu_rate: Option<f64>) -> DeviceTable`

**Quantisation is the contract, not an implementation detail.** Two probe runs on
one machine must produce identical weights, because a weight change re-places
renditions and a re-placed rendition serves cached segments from the wrong
encoder. Bucket coarsely.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod weight_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The stability property the whole design rests on: ordinary measurement
    /// noise must not change a weight, because a changed weight re-places
    /// renditions and a re-placed rendition mixes encoders under one init.
    #[test]
    fn measurement_noise_does_not_change_a_weight() {
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
```

- [ ] **Step 2: Run to verify failure**

```bash
nix develop --command cargo nextest run -p pharos-transcode weight_tests
```

Expected: **compile failure**, `cannot find function 'device_weight'`.

- [ ] **Step 3: Implement the weight**

Add to `device.rs`:

```rust
/// How coarsely a measured rate is bucketed before it can affect placement.
///
/// This is the STABILITY CONTRACT, not a tuning knob. A weight change
/// re-places renditions, and `SegmentIdentity` does not include the device —
/// so a re-placed rendition serves cached segments from the old encoder beside
/// fresh ones from the new one, under a single init, with no restart involved.
/// The bucket must therefore be wider than any plausible run-to-run variation
/// in the probe. Doubling steps: a device has to measure twice as fast before
/// placement notices.
const RATE_BUCKET_RATIO: f64 = 2.0;

/// A device's share of new renditions, derived entirely from probe-time facts.
///
/// `capacity` is how many encodes it sustains; `rate` is its measured speed on
/// the shared synthetic clip and `reference_rate` the slowest measured device,
/// so the speed term is a pure ratio and no device family is named anywhere.
///
/// Quantised via [`RATE_BUCKET_RATIO`] so ordinary measurement noise cannot
/// move a rendition — see that constant for why that matters more than
/// precision.
pub fn device_weight(capacity: usize, rate: Option<f64>, reference_rate: Option<f64>) -> u32 {
    let cap = capacity.max(1).min(1024) as u32;
    let speed_bucket = match (rate, reference_rate) {
        (Some(r), Some(base)) if r.is_finite() && base.is_finite() && r > 0.0 && base > 0.0 => {
            // How many doublings above the slowest device this one measured.
            let ratio = (r / base).max(1.0).min(1024.0);
            let doublings = ratio.log(RATE_BUCKET_RATIO).floor();
            2u32.saturating_pow(doublings as u32)
        }
        // No usable measurement: capacity alone. Absence of a rate is a missing
        // observation, never evidence the device is slow.
        _ => 1,
    };
    cap.saturating_mul(speed_bucket).max(1)
}
```

- [ ] **Step 4: Carry the weight on the slot**

Add to `pub struct DeviceSlot` (after `capacity`):

```rust
    /// Probe-derived share of new renditions — see [`device_weight`]. Fixed for
    /// the life of the table: placement must not move while segments are
    /// cached or a client holds an init.
    pub weight: u32,
```

`DeviceSlot::new` currently takes `(id, capacity)`. Add the weight rather than
computing it there — the slot should not know about probing:

```rust
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
```

Keep the existing `from_probe(caps, cpu_permits)` working — it is called from
tests and the CLI tool — by giving every slot a capacity-only weight:

```rust
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
        let reference = caps
            .iter()
            .map(|&(_, _, r)| r)
            .chain(std::iter::once(cpu_rate))
            .flatten()
            .filter(|r| r.is_finite() && *r > 0.0)
            .fold(None::<f64>, |acc, r| Some(acc.map_or(r, |a: f64| a.min(r))));

        let mut slots: SmallVec<[DeviceSlot; 5]> = SmallVec::new();
        for &(id, cap, rate) in caps {
            if matches!(id, DeviceId::Cpu) {
                continue; // CPU is appended once, below
            }
            if slots.iter().any(|s| s.id == id) {
                continue;
            }
            slots.push(DeviceSlot::new(id, cap, device_weight(cap, rate, reference)));
        }
        slots.push(DeviceSlot::new(
            DeviceId::Cpu,
            cpu_permits,
            device_weight(cpu_permits, cpu_rate, reference),
        ));
        Self { slots }
    }
```

- [ ] **Step 5: Fix the other `DeviceSlot::new` call sites**

```bash
nix develop --command cargo check -p pharos-transcode --all-targets
```

Fix each error. Re-run until clean.

- [ ] **Step 6: Run the tests**

```bash
nix develop --command cargo nextest run -p pharos-transcode
```

Expected: all pass, including the five new ones.

- [ ] **Step 7: Disarm-verify the stability test**

Temporarily change `RATE_BUCKET_RATIO` to `1.01`, re-run
`measurement_noise_does_not_change_a_weight`, confirm it **FAILS**, restore `2.0`,
confirm it passes. A quantisation test that survives the quantisation being
removed is not testing it.

- [ ] **Step 8: Commit**

```bash
git add crates/pharos-transcode/src/device.rs
git commit -m "feat(transcode): weigh each device by what the probe measured

Placement needs a share per device, and any number written down here would be
the hardware-specific constant V126 exists to forbid. The weight is capacity
times a bucketed speed ratio against the slowest measured device on this
machine -- so a many-core software encoder outweighs a weak accelerator without
anything naming either.

The bucketing is the contract, not a detail. SegmentIdentity has no device
field, so a weight that moved would re-place a rendition and serve its cached
segments from one encoder beside fresh ones from another, under one init and
with no restart involved. A device must measure twice as fast before placement
notices.

An absent rate weighs on capacity alone: a missing measurement is not evidence
of a slow device.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 3: Every supporting device enters the rendition pool

**Files:**
- Modify: `crates/pharos-transcode/src/device.rs` (`rendition_device`, ~line 257)
- Test: inline test module in `device.rs`

**Interfaces:**
- Consumes: `DeviceSlot.weight` (Task 2), `device_supports`.
- Produces: `rendition_device` unchanged in signature —
  `pub fn rendition_device(&self, opts: &TranscodeOptions, rendition_key: u64) -> Option<DeviceId>`.

**This is the change the spec exists for**, and the one that can serve
undecodable video if it is wrong. The invariant: *the same table and the same
rendition key must always yield the same device.*

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod pool_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;
    use crate::options::{Container, VideoCodec};

    fn gpu() -> DeviceId {
        DeviceId::hw(HwAccel::Nvenc, 0)
    }

    fn h264_fmp4() -> TranscodeOptions {
        let mut o = TranscodeOptions::default();
        o.video = Some(VideoCodec::H264);
        o.container = Container::Fmp4;
        o
    }

    fn vp9_fmp4() -> TranscodeOptions {
        let mut o = TranscodeOptions::default();
        o.video = Some(VideoCodec::Vp9);
        o.container = Container::Fmp4;
        o
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
        assert!(
            on_cpu < 1000,
            "the accelerator got no renditions at all"
        );
    }

    /// The split must follow the DERIVED weights, never a written-down ratio.
    /// Asserted against the table's own weights so this test stays true on
    /// hardware nobody has run it on.
    #[test]
    fn the_split_follows_the_derived_weights() {
        let t = table(8, 400.0, 4, 100.0);
        let total: u32 = t.slots().iter().map(|s| s.weight).sum();
        let cpu_weight = t
            .slots()
            .iter()
            .find(|s| s.id == DeviceId::Cpu)
            .map(|s| s.weight)
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
```

If `TranscodeOptions::default()` does not exist, build the options the way the
existing tests in this file do — copy their construction rather than inventing one.

- [ ] **Step 2: Run to verify failure**

```bash
nix develop --command cargo nextest run -p pharos-transcode pool_tests
```

Expected: `software_takes_a_share_of_work_an_accelerator_could_also_do` **FAILS**
with "software got no renditions at all — the pool is still hardware-only". That
is the defect. Others may pass already; note which in your report.

- [ ] **Step 3: Implement**

Replace `rendition_device`'s body (`device.rs:257-278`):

```rust
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
    pub fn rendition_device(
        &self,
        opts: &TranscodeOptions,
        rendition_key: u64,
    ) -> Option<DeviceId> {
        let supporting: SmallVec<[(DeviceId, u32); 5]> = self
            .slots
            .iter()
            .filter(|s| device_supports(s.id, opts))
            .map(|s| (s.id, s.weight.max(1)))
            .collect();
        if supporting.is_empty() {
            return None;
        }
        let total: u64 = supporting.iter().map(|(_, w)| u64::from(*w)).sum();
        if total == 0 {
            return Some(supporting[0].0);
        }
        // Weighted pick: walk the cumulative weights until the key's position
        // falls inside a device's band. Deterministic in the key and the
        // weights, which is the whole contract.
        let mut pos = rendition_key % total;
        for (id, w) in supporting.iter() {
            let w = u64::from(*w);
            if pos < w {
                return Some(*id);
            }
            pos -= w;
        }
        Some(supporting[supporting.len() - 1].0)
    }
```

Note `device_supports` is deliberately still the eligibility gate — it is what
keeps a codec off an accelerator that cannot encode it, and Task 3 does not
change it.

- [ ] **Step 4: Run the tests**

```bash
nix develop --command cargo nextest run -p pharos-transcode
```

Expected: all pass. **If any pre-existing test in the workspace fails, stop and
report** — a changed placement can invalidate a test that assumed the old
hardware-only pool, and that is a judgement call, not a thing to edit away.

- [ ] **Step 5: Disarm-verify the split test**

Temporarily replace the weighted pick with `supporting[(rendition_key % supporting.len() as u64) as usize].0`
(unweighted), re-run `the_split_follows_the_derived_weights`, confirm it
**FAILS**, restore, confirm it passes.

- [ ] **Step 6: Commit**

```bash
git add crates/pharos-transcode/src/device.rs
git commit -m "feat(transcode): let every capable encoder take a share of renditions

rendition_device dropped every non-hardware device from the pool whenever any
hardware supported the codec, so on a machine with an accelerator the software
encoder was unreachable for everything that accelerator could do -- however many
permits it had and however fast it was. Concurrent renditions serialised onto a
subset of the machine while the rest idled.

The #114 rule is "one rendition, one encoder", not "hardware only". Each
rendition still resolves to exactly one device, still by a pure function of the
rendition and the probe result, so a restart on unchanged hardware places
identically. What changes is that two renditions can occupy two devices.

Weighted by what the probe measured, so this is correct on a box where software
is the stronger device rather than only on one where it is not.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 4: Wire the probe at boot, and publish what it decided

**Files:**
- Modify: `crates/pharos-server/src/main.rs` (~line 1086)
- Modify: `crates/pharos-server/src/router.rs` (~line 47)
- Modify: `crates/pharos-transcode/src/scheduler.rs` (`record_placement`)
- Test: `crates/pharos-transcode/src/scheduler.rs` test module

**Interfaces:**
- Consumes: `probe_encode_rate` (Task 1), `from_probe_weighted` + `DeviceSlot.weight` (Task 2).
- Produces: gauge `pharos_transcode_device_weight{device}`; counter
  `pharos_transcode_rendition_pin_total{device}`.

**Without these two signals a misplacement cannot be told from a mis-weighting**,
and on hardware nobody has tested that is the first question anyone asks.

- [ ] **Step 1: Measure each device on the existing boot pass**

In `main.rs`, after `caps` is built and before `DeviceTable::from_probe(...)`
(~line 1086), measure every confirmed device plus software, then build the
weighted table:

```rust
    // Measure each confirmed device on the SAME synthetic clip, so placement can
    // weigh them against each other. Runs on the boot pass that already trials
    // every device, and a failure is not fatal — an unmeasured device weighs on
    // capacity alone.
    let mut caps_rated: Vec<(DeviceId, usize, Option<f64>)> = Vec::with_capacity(caps.len());
    for &(d, c) in &caps {
        let rate = pharos_transcode::probe::probe_encode_rate(d, probe_timeout).await;
        match rate {
            Some(r) => tracing::info!(device = %d, frames_per_sec = r, "device encode rate measured"),
            None => tracing::warn!(device = %d, "device encode rate NOT measured; weighting on capacity alone"),
        }
        caps_rated.push((d, c, rate));
    }
    let cpu_rate = pharos_transcode::probe::probe_encode_rate(DeviceId::Cpu, probe_timeout).await;
    let table = DeviceTable::from_probe_weighted(&caps_rated, default_cpu_permits(), cpu_rate);
    for s in table.slots() {
        tracing::info!(device = %s.id, capacity = s.capacity, weight = s.weight, "device weighted for rendition placement");
    }
```

Delete the old `let table = DeviceTable::from_probe(&caps, default_cpu_permits());`
line it replaces. Keep the existing `hw_session_budget` computation, which reads
`caps` and is unaffected.

- [ ] **Step 2: Publish the weight beside the capacity gauge**

In `router.rs`, beside the existing `pharos_transcode_device_capacity` emission
(~line 47), add:

```rust
        metrics::gauge!("pharos_transcode_device_weight", "device" => device.clone())
            .set(f64::from(slot.weight));
```

Match the surrounding code's shape for obtaining `device` and the slot — copy it
rather than inventing a second traversal.

- [ ] **Step 3: Write the failing test for the pin counter**

Add to `scheduler.rs`'s test module:

```rust
    /// ODD — placement is now a decision with an input nobody can see. Without
    /// a per-device count of what it decided, a misplacement is
    /// indistinguishable from a mis-weighting, and on unfamiliar hardware that
    /// is the first question.
    #[test]
    fn a_pinned_rendition_records_which_device_it_resolved_to() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(50), |_, _| {
                    WorkerRunResult::Done { out_bytes: 1 }
                });
                let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
                s.submit(
                    PathBuf::from("/m/a"),
                    cmaf_with_duration(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
                .unwrap();
            })
        });

        let found = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_transcode_rendition_pin_total" {
                    return None;
                }
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                Some((labels, v))
            });

        let (labels, value) = found.expect(
            "a pinned rendition must record its device — without it a misplacement \
             cannot be told from a mis-weighting",
        );
        assert!(
            labels.iter().any(|l| l.starts_with("device=")),
            "expected a device label, got {labels:?}"
        );
        assert!(matches!(value, DebugValue::Counter(1)), "got {value:?}");
    }
```

`cmaf_with_duration()` exists in that module (added by 006); a bare `cmaf()` sets
`duration_ticks: None`.

- [ ] **Step 4: Run to verify failure**

```bash
nix develop --command cargo nextest run -p pharos-transcode a_pinned_rendition_records
```

Expected: **FAIL** — the metric does not exist.

- [ ] **Step 5: Record the pin**

In `scheduler.rs`, in the branch that resolves a shared-init rendition (the
`PinOutcome::Followed` path inside `candidates_for`), add beside the existing
outcome recording:

```rust
                metrics::counter!(
                    "pharos_transcode_rendition_pin_total",
                    "device" => d.to_string(),
                )
                .increment(1);
```

**Record it where the pin is FOLLOWED at a terminal decision, not on every
examination.** 006 fixed exactly this defect once: `pin_total{followed}` counted
dispatch attempts while `{invalidated}` counted jobs, and a queued job is
re-examined on every drain pass. Put it where `PinOutcome::Followed` is recorded
now, which was already moved to the dispatch point for that reason.

- [ ] **Step 6: Run the tests**

```bash
nix develop --command cargo nextest run --workspace
```

- [ ] **Step 7: Disarm-verify**

Comment out the new `counter!`, confirm the test **FAILS**, restore, confirm it
passes.

- [ ] **Step 8: Full gate and commit**

```bash
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
nix develop --command just hakari-check
git add crates/pharos-server/src/main.rs crates/pharos-server/src/router.rs crates/pharos-transcode/src/scheduler.rs
git commit -m "feat(transcode): weigh devices from the boot probe, and say what it decided

Measures every confirmed device plus software on the same clip during the boot
pass that already trials them, and builds the device table from those rates. An
unmeasured device weighs on capacity alone and says so at WARN -- silence about a
failed measurement would look identical to a slow device.

Publishes pharos_transcode_device_weight so the derived share is visible, and
counts pharos_transcode_rendition_pin_total per device so placement can be read
rather than inferred. Without both, a misplacement is indistinguishable from a
mis-weighting, which on unfamiliar hardware is the first question anyone asks.

The pin counter records at the terminal decision, not per examination: 006 found
pin_total{followed} counting dispatch attempts while {invalidated} counted jobs,
which made their ratio meaningless.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 5: Class-preferred ordering for unpinned work

**Files:**
- Modify: `crates/pharos-transcode/src/device.rs` (`eligible_for`)
- Modify: `crates/pharos-transcode/src/scheduler.rs` (`candidates_for`)
- Test: inline test modules

**Interfaces:**
- Consumes: `DeviceSlot.weight` (Task 2).
- Produces: `pub fn eligible_for_class(&self, opts: &TranscodeOptions, now: Instant, prefer_strong: bool) -> SmallVec<[DeviceId; 5]>`

**Scope honestly stated:** unpinned **queued** work is mpegts only — one
production call site queues segment jobs (`hls_cache.rs:2276`) and
`SegmentContainer` is `Mpegts` or `Fmp4`. This is the smaller half of the spec.
Do not oversell it in the commit message.

- [ ] **Step 1: Write the failing test**

```rust
    /// A client's job should try the strongest device first and speculative
    /// work the weakest — but "strongest" must come from the PROBE, never from
    /// the device being hardware. On a machine where software measured faster,
    /// software is what a client's job should try first.
    #[test]
    fn preference_follows_the_probe_not_the_device_class() {
        use crate::hwaccel::HwAccel;
        let gpu = DeviceId::hw(HwAccel::Nvenc, 0);
        // Software measured FOUR TIMES the accelerator's rate.
        let t = DeviceTable::from_probe_weighted(&[(gpu, 2, Some(100.0))], 16, Some(400.0));
        let now = Instant::now();

        let urgent = t.eligible_for_class(&h264_mpegts(), now, true);
        assert_eq!(
            urgent.first(),
            Some(&DeviceId::Cpu),
            "a client's job did not try the device the probe says is strongest"
        );

        let speculative = t.eligible_for_class(&h264_mpegts(), now, false);
        assert_eq!(
            speculative.first(),
            Some(&gpu),
            "speculative work did not try the weaker device first"
        );

        // A preference is not a restriction: every eligible device is still
        // present in both orderings, so nothing is ever refused a free permit.
        assert_eq!(urgent.len(), speculative.len());
        assert_eq!(urgent.len(), 2);
    }
```

Add an `h264_mpegts()` helper alongside the existing option builders — same as
`h264_fmp4()` but with `Container::Mpegts`.

- [ ] **Step 2: Run to verify failure**

```bash
nix develop --command cargo nextest run -p pharos-transcode preference_follows_the_probe
```

Expected: **compile failure**, `no method named 'eligible_for_class'`.

- [ ] **Step 3: Implement**

Add to `device.rs`:

```rust
    /// Eligible devices ordered by whether this job wants the strongest device
    /// or should leave it alone.
    ///
    /// `prefer_strong` is the CLASS decision — a client is blocked on this job,
    /// so give it the device the probe measured as best; speculative work takes
    /// the weakest first and leaves the strong one for a client.
    ///
    /// Ordering only. Every eligible device stays in the list, so a preference
    /// can never become a restriction: if the preferred device has no free
    /// permit the job still runs on another. What stops speculation hurting a
    /// client on a shared device is `crowds_a_client`, not this.
    ///
    /// Deliberately keyed on the probe-derived weight rather than on the device
    /// being hardware — on a machine where software measured faster, software is
    /// what a client's job should try first (V126).
    pub fn eligible_for_class(
        &self,
        opts: &TranscodeOptions,
        now: Instant,
        prefer_strong: bool,
    ) -> SmallVec<[DeviceId; 5]> {
        let mut out: SmallVec<[(DeviceId, u32); 5]> = self
            .slots
            .iter()
            .filter(|s| device_supports(s.id, opts) && !s.in_cooldown(now))
            .map(|s| (s.id, s.weight))
            .collect();
        if prefer_strong {
            out.sort_by(|a, b| b.1.cmp(&a.1));
        } else {
            out.sort_by(|a, b| a.1.cmp(&b.1));
        }
        out.into_iter().map(|(id, _)| id).collect()
    }
```

- [ ] **Step 4: Use it for unpinned work**

In `scheduler.rs`'s `candidates_for`, where `full_eligible` is obtained for a job
that is **not** shared-init fMP4, use the class-ordered list:

```rust
    let full_eligible = state.devices.eligible_for_class(
        &ctx.opts,
        now,
        ctx.class == JobClass::Interactive,
    );
```

Leave the pinned path exactly as it is — a pinned rendition has one candidate and
ordering is meaningless for it.

- [ ] **Step 5: Run the tests**

```bash
nix develop --command cargo nextest run --workspace
```

Expected: all pass, including 006's standing guard
`speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for`. **If that
guard fails, stop and report** — it is the branch-standing proof that speculation
cannot crowd a client, and ordering must not have broken it.

- [ ] **Step 6: Disarm-verify**

Change `prefer_strong` to be ignored (always sort descending), re-run
`preference_follows_the_probe_not_the_device_class`, confirm it **FAILS**,
restore, confirm it passes.

- [ ] **Step 7: Commit**

```bash
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
git add crates/pharos-transcode/src/device.rs crates/pharos-transcode/src/scheduler.rs
git commit -m "feat(transcode): let a client's job try the strongest device first

Unpinned work had no class awareness: eligible_for returned devices in table
order and place() took the first with a free permit, so a speculative job could
take the best device while a client's job queued behind it on the same one.

Orders candidates by the probe-derived weight -- descending for a client's job,
ascending for speculation. Keyed on the measurement, not on the device being
hardware, so on a machine where software measured faster software is what a
client tries first.

Ordering only. Every eligible device stays in the list, so a preference can
never become a restriction and no job is refused a free permit; crowds_a_client
remains what protects a client on a shared device.

Narrow by construction: unpinned queued work is mpegts only, since one call site
queues segment jobs and SegmentContainer is Mpegts or Fmp4.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 6: Record the invariant, verify, ship

**Files:**
- Modify: `specs/001-pharos-baseline/invariants.md`
- Modify: `specs/007-device-spread/spec.md` (status line)

- [ ] **Step 1: Append V129**

Match the file's real format — plain `- V1NN: …` prose bullets. Read a neighbour
first; a previous plan's illustrative `### V129` heading format was wrong twice.

```markdown
- V129: encoder PLACEMENT is derived from the boot probe and never moves; encoder LOAD is learned and moves continuously. The two must not be conflated, because they answer to different constraints. Placement decides which device a shared-init rendition uses, and it may never change: `SegmentIdentity` carries no device, so a rendition whose device moved would serve its CACHED segments from the old encoder beside fresh ones from the new one, under a single init, with no restart involved — the #114 failure (undecodable video, served with a 200) reached without any of the conditions #114 was written about. Placement is therefore a pure function of the rendition key and the probe result, weighted by `device_weight` (capacity × a bucketed speed ratio against the slowest measured device), and the bucketing is a CONTRACT rather than a tuning choice: a device must measure twice as fast before placement notices, so ordinary probe noise cannot re-place a rendition. Load, by contrast, has no such constraint — 006's per-device allowance changes on every observation and is the right place for all adaptivity that does not move a rendition. The corollary that keeps this portable (V126): no placement or preference decision may test whether a device is hardware. A machine whose software encoder measures faster than its accelerator must prefer software, and only a probe-derived weight can express that. Guards: device.rs `the_same_probe_result_places_every_rendition_identically`, `measurement_noise_does_not_change_a_weight`, `a_fast_software_encoder_can_outweigh_a_slow_accelerator`, `preference_follows_the_probe_not_the_device_class`.
```

- [ ] **Step 2: Mark the spec shipped**

Update `specs/007-device-spread/spec.md`'s status line to say what landed, and add
a "What shipped, and where it diverged" section if implementation forced any
change to the design — as 006's spec does. If nothing diverged, say that.

- [ ] **Step 3: Prove no device family leaked into a decision**

```bash
rg -n 'DeviceId::Hw|is_hw\(\)' crates/pharos-transcode/src/device.rs
```

Every remaining hit must be in `device_supports` (codec eligibility — legitimate,
a device genuinely cannot encode what it has no encoder for), in `from_probe`'s
CPU-dedup, or in a doc comment. **A hit inside `rendition_device`,
`device_weight` or `eligible_for_class` is a defect** — those are the decisions
that must be probe-derived. Report the full output.

- [ ] **Step 4: Full gate**

```bash
nix develop --command just test
nix develop --command cargo test --doc --workspace
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
nix develop --command just hakari-check
```

No `sqlx::query*` string is touched, so `just test-postgres` is not required —
confirm with `git diff main --stat -- '*pharos-store-sqlx*'` before skipping it.

- [ ] **Step 5: Commit, PR, merge**

```bash
git add specs/
git commit -m "docs(spec): record 007 device spread and V129

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
git push -u origin HEAD
gh pr create --fill
```

Merge with `gh pr merge --rebase` once CI is green. Never `--squash`.

- [ ] **Step 6: Verify by query, not by assertion**

After the deploy reconciles:

```promql
pharos_transcode_device_weight
sum by (device) (pharos_transcode_rendition_pin_total)
pharos_transcode_device_in_use
histogram_quantile(0.95, sum by (le) (rate(pharos_transcode_queue_wait_seconds_bucket{class="interactive"}[15m])))
```

What each answer means:

- **`device_weight` present for every device** — the probe ran. A device missing
  means its measurement failed and it is weighting on capacity alone.
- **`rendition_pin_total` non-zero on more than one device** — placement is
  spreading. All on one device means the weighting collapsed.
- **`device_in_use` non-zero on more than one device simultaneously** under
  concurrent load — the actual goal. Before this change it could not be, for any
  codec an accelerator supported.
- **Interactive `queue_wait_seconds` p95 lower** than before under two concurrent
  renditions. **This is the number that justifies the change.** If it does not
  move, phase A did not help and should be reverted — a spread that costs a
  rendition the better device and buys nothing is worse than the serialisation it
  replaced.

Report actual output. "Should be faster" is not a result.

---

## Self-Review

**Spec coverage.** Phase A → Tasks 1–4 (probe, weight, pool, wiring + signals).
Phase B → Task 5. The quantisation contract → Task 2 (`RATE_BUCKET_RATIO`, with a
disarm). Purity across restarts → Task 3's first test. The "software may be
stronger" case → Task 2's `a_fast_software_encoder_can_outweigh_a_slow_accelerator`
and Task 5's `preference_follows_the_probe_not_the_device_class`. Degenerate
tables → Task 2 step 1 and Task 3's `degenerate_tables_still_place`. Codec
capability → Task 3's VP9 test. Both dispatch paths honouring the pin → covered by
006's existing `candidates_for` unification, which Task 5 does not change; Task 3
step 4 says to stop if any existing test fails. Signals → Task 4. V129 → Task 6.

**Deliberately out of scope**, matching the spec: runtime re-placement, splitting a
rendition across encoders, quality-based preference, non-segment work.

**Known limitation carried forward, not hidden.** Placement is load-blind: an
unlucky key can put a lone rendition on a weaker device while a stronger one
idles. The spec accepts this for phase A because the alternative changes the
purity invariant. Task 6's `device_in_use` query is what would show it.

**One thing I could not verify while writing this.** `TranscodeOptions::default()`
may not exist, and `accel.video_encoder(..)` is read from `codec_probe_args`'s
call rather than from its definition. Tasks 1 and 3 both say to copy the existing
construction rather than guess. If either is wrong the RED step fails to compile,
which surfaces it immediately rather than silently.
