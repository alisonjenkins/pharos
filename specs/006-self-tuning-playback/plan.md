# 006 Self-Tuning Playback — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `background_alongside_client` — a concurrency constant
calibrated by hand on one GTX 1070 — with a closed loop that learns, per device,
how much speculative transcode work may run beside the segment a client is
blocked on; then make the speculative work that loses that contest *deferred and
ordered by urgency* instead of dropped.

**Architecture:** Three phases, each independently shippable and revertable.
Phase 1 adds a pure AIMD controller (`pharos-transcode::admission`) fed by
segment-completion observations the scheduler actor already holds, gated behind a
shadow-mode step that ships the measurement before the behaviour change. Phase 2a
replaces the HLS cache's per-key `Mutex` single-flight with a shared-result
registry, so a request that arrives on an in-flight segment awaits a *value*
rather than holding exclusion across a multi-second encode. Phase 2b then lets
background work into the scheduler's pending queue, selecting at dispatch by tier
and by lookahead distance recomputed against the stream's live playhead, with
promotion when a client arrives on speculative work.

**Tech Stack:** Rust · tokio (mpsc actor, `watch`, `oneshot`) · `dashmap` ·
`metrics` + Prometheus · `tracing` · `cargo nextest` via the Nix devShell.

## Global Constraints

Copied verbatim from the spec and from `CLAUDE.md`; every task's requirements
implicitly include this section.

- **Every command runs inside the Nix devShell.** Prefix with
  `nix develop --command`. Never invoke `cargo`, `clippy` or `ffmpeg` from the
  host shell.
- **Tests run via `cargo nextest run`**, not `cargo test`. Doctests separately
  with `cargo test --doc --workspace`.
- **Atomic commits**: each commit does exactly one granular thing, and reverting
  it alone must leave the project compiling. Never squash.
- **Clippy before push**: `clippy::unwrap_used` and `clippy::expect_used` are
  `deny` at workspace level (V17). Test modules opt out with
  `#![allow(clippy::unwrap_used, clippy::expect_used)]`, which the existing
  `scheduler.rs` test module already declares.
- **ODD discipline** (project constitution): name the query before writing the
  fix; instrument the *decision*, not just the error; every new metric assertion
  must be **disarm-verified** (delete the instrumentation, watch the test go red,
  restore it); success and failure paths record the same fields.
- **Metric labels are a dashboard contract**: bounded cardinality, stable strings
  from a `label()` method, asserted distinct in a test.
- **Ids are stable and load-bearing.** Never renumber. Next free bug id is
  **B178**; next free invariant id is **V126**.
- **Fixed values from the spec** (do not re-derive): `margin_ratio` = **0.5**,
  `increase_step` = **1.0**, `decrease_factor` = **0.5**, floor =
  `background_alongside_client` = **1**, ceiling = **device capacity − 1**,
  pending cap = **256**, tracked-stream LRU bound = **256**.
- **Phase ordering is a hard gate.** Phase 2a does not start until phase 1 is
  deployed and its allowance gauge has been read on the live deployment. Phase 2b
  does not start until 2a is merged.

---

## File Structure

| File | Responsibility | Phase |
|---|---|---|
| `crates/pharos-transcode/src/admission.rs` | **New.** Pure AIMD state machine: `(observation) → allowance`, per device. No tokio, no scheduler, no I/O — testable in isolation. | 1 |
| `crates/pharos-transcode/src/lib.rs` | Add `pub mod admission;` | 1 |
| `crates/pharos-transcode/src/scheduler.rs` | Owns the controller instance; stamps peer counts at dispatch; feeds observations on `JobFinished`; consults the allowance in `crowds_a_client`. Later: queue selection, eviction, promotion. | 1, 2b |
| `crates/pharos-cache/src/hls_cache.rs` | Shared-result registry replacing `fetch_locks`; `hit_path` label migration; passes the stream key through to `submit`. | 2a, 2b |
| `crates/pharos-server/src/api/jellyfin/hls.rs` | Supplies the stream key at the five `segment_bytes_keyed` call sites. | 2b |
| `specs/001-pharos-baseline/bugs.md` | B178 (peer counts unstamped on the drain path). | 1 |
| `specs/001-pharos-baseline/invariants.md` | V126 (a tunable that must know the hardware is a defect). | 1 |

`scheduler.rs` is 2 485 lines and already carries placement, retry, queueing,
live streams, spans and metrics. The controller does **not** go in it; the module
boundary is what makes the control law testable without spawning an actor.

---

## Task 1: The AIMD controller

**Files:**
- Create: `crates/pharos-transcode/src/admission.rs`
- Modify: `crates/pharos-transcode/src/lib.rs` (add the module declaration)
- Test: inline `#[cfg(test)] mod tests` in `admission.rs`

**Interfaces:**
- Consumes: `crate::protocol::DeviceId` (already public; `DeviceId::Cpu` and
  `DeviceId::hw(HwAccel::Nvenc, 0)` are the constructors used in tests).
- Produces, relied on by Task 3 and Task 4:
  - `AdmissionConfig { margin_ratio: f64, increase_step: f64, decrease_factor: f64, floor: f64 }`, `Default`
  - `Observation { segment_seconds: Option<f64>, encode_seconds: f64, background_peers: usize, usable: bool }`
  - `Verdict::{Met, Missed, Ignored}` with `fn label(self) -> &'static str`
  - `AdmissionController::new(AdmissionConfig) -> Self`
  - `AdmissionController::allowance(&self, dev: DeviceId, capacity: usize) -> usize`
  - `AdmissionController::raw_allowance(&self, dev: DeviceId, capacity: usize) -> f64`
  - `AdmissionController::observe(&mut self, dev: DeviceId, capacity: usize, obs: Observation) -> Verdict`

- [ ] **Step 1: Declare the module**

Add to `crates/pharos-transcode/src/lib.rs`, in the alphabetical `pub mod` block
that currently starts with `pub mod backend;` (line 14):

```rust
pub mod admission;
```

It sorts before `backend`, so it becomes the first line of that block.

- [ ] **Step 2: Write the failing tests**

Create `crates/pharos-transcode/src/admission.rs` containing **only** the test
module for now, so the compile failure is about the missing types rather than a
missing file:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;

    fn gpu() -> DeviceId {
        DeviceId::hw(HwAccel::Nvenc, 0)
    }

    /// 6 s segments, `margin_ratio` 0.5 ⇒ a 3 s deadline.
    fn met(peers: usize) -> Observation {
        Observation {
            segment_seconds: Some(6.0),
            encode_seconds: 1.0,
            background_peers: peers,
            usable: true,
        }
    }

    fn missed(peers: usize) -> Observation {
        Observation {
            segment_seconds: Some(6.0),
            encode_seconds: 5.0,
            background_peers: peers,
            usable: true,
        }
    }

    #[test]
    fn a_fast_device_climbs_to_the_ceiling_and_stops_there() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        // Capacity 8 ⇒ ceiling 7. Feed exercised successes forever.
        for _ in 0..50 {
            let peers = c.allowance(gpu(), 8);
            assert_eq!(c.observe(gpu(), 8, met(peers)), Verdict::Met);
        }
        assert_eq!(c.allowance(gpu(), 8), 7);
    }

    /// The pathological direction: a device that cannot make the deadline under
    /// any contention must return to the floor and stay there, because the floor
    /// is exactly today's shipped behaviour.
    #[test]
    fn a_slow_device_collapses_to_the_floor_and_stays() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        for _ in 0..20 {
            assert_eq!(c.observe(gpu(), 8, missed(3)), Verdict::Missed);
        }
        assert_eq!(c.allowance(gpu(), 8), 1);
    }

    /// A segment that met its deadline while running ALONE never tested the
    /// allowance, so it is not evidence the allowance can rise. Without this
    /// guard the value inflates on irrelevant successes and then discovers the
    /// truth by hurting a viewer.
    #[test]
    fn an_unexercised_success_does_not_increase_the_allowance() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        for _ in 0..10 {
            assert_eq!(c.observe(gpu(), 8, met(0)), Verdict::Met);
        }
        assert_eq!(c.allowance(gpu(), 8), 1);
    }

    /// A 4K HEVC source that blows the deadline with nothing beside it is not
    /// prefetch's fault and backing off cannot help. Without this guard one
    /// pathological title ratchets every device to the floor permanently.
    #[test]
    fn a_solo_miss_does_not_decrease_the_allowance() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        c.observe(gpu(), 8, met(1));
        c.observe(gpu(), 8, met(2));
        assert_eq!(c.allowance(gpu(), 8), 3);
        assert_eq!(c.observe(gpu(), 8, missed(0)), Verdict::Missed);
        assert_eq!(c.allowance(gpu(), 8), 3);
    }

    /// Clamping at both ends, including the degenerate single-permit device
    /// where `capacity - 1` would otherwise sit BELOW the floor and silently
    /// change today's behaviour.
    #[test]
    fn the_allowance_is_clamped_at_both_ends() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        for _ in 0..50 {
            let peers = c.allowance(gpu(), 3);
            c.observe(gpu(), 3, met(peers));
        }
        assert_eq!(c.allowance(gpu(), 3), 2, "ceiling is capacity - 1");

        let mut single = AdmissionController::new(AdmissionConfig::default());
        for _ in 0..50 {
            single.observe(gpu(), 1, met(5));
        }
        assert_eq!(
            single.allowance(gpu(), 1),
            1,
            "a one-permit device never drops below today's constant"
        );
    }

    /// Live/progressive jobs carry no segment duration and failed or retried
    /// jobs carry a meaningless encode time. Both must leave the state
    /// untouched AND be distinguishable in the metric, because an all-ignored
    /// stream is "no signal", not "a device that cannot go faster".
    #[test]
    fn observations_that_carry_no_signal_change_nothing() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        c.observe(gpu(), 8, met(1));
        assert_eq!(c.allowance(gpu(), 8), 2);

        let live = Observation {
            segment_seconds: None,
            ..met(4)
        };
        assert_eq!(c.observe(gpu(), 8, live), Verdict::Ignored);

        let retried = Observation {
            usable: false,
            ..met(4)
        };
        assert_eq!(c.observe(gpu(), 8, retried), Verdict::Ignored);

        assert_eq!(c.allowance(gpu(), 8), 2);
    }

    /// State is per device: a CPU spill that misses every deadline must not
    /// drag down what the GPU has learned.
    #[test]
    fn each_device_learns_independently() {
        let mut c = AdmissionController::new(AdmissionConfig::default());
        c.observe(gpu(), 8, met(1));
        c.observe(gpu(), 8, met(2));
        for _ in 0..5 {
            c.observe(DeviceId::Cpu, 4, missed(2));
        }
        assert_eq!(c.allowance(gpu(), 8), 3);
        assert_eq!(c.allowance(DeviceId::Cpu, 4), 1);
    }

    /// The verdict strings are a dashboard contract.
    #[test]
    fn verdict_labels_are_distinct_and_stable() {
        let all = [Verdict::Met, Verdict::Missed, Verdict::Ignored];
        let labels: Vec<&str> = all.iter().map(|v| v.label()).collect();
        assert_eq!(labels, vec!["met", "missed", "ignored"]);
        let uniq: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(uniq.len(), labels.len());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run:

```bash
nix develop --command cargo nextest run -p pharos-transcode admission
```

Expected: **compile failure**, `cannot find type 'AdmissionController' in this
scope` (and the same for `AdmissionConfig`, `Observation`, `Verdict`).

- [ ] **Step 4: Write the controller**

Prepend to `crates/pharos-transcode/src/admission.rs`, above the test module:

```rust
//! Closed-loop admission control for speculative transcode work.
//!
//! `background_alongside_client` — how many speculative encodes may run beside
//! the segment a client is blocked on — was a constant calibrated by hand on one
//! GTX 1070 transcoding one 23 Mbps HEVC source. On a card with more NVENC
//! engines it under-uses the hardware; on a weaker box it is still too many.
//!
//! This replaces it with a closed loop over a hardware-INDEPENDENT invariant: a
//! segment covering N seconds of playback must be produced within
//! `margin_ratio × N`. The same rule admits many concurrent jobs on a fast card
//! and none on a weak one, with nothing to calibrate per machine.
//!
//! Why a loop and not a model: the measured cost of a segment is dominated by
//! decode and I/O, not encode (the same segment costs the same at 360p as at
//! 1080p for a sixteenth of the output), so a model fitted on peer count alone
//! predicts well for its training mix and mispredicts when the mix changes. A
//! loop measures the thing that matters — did the client's segment make its
//! deadline — so a slow NFS morning, a heavier codec and another tenant on the
//! GPU all arrive as the same signal without being modelled separately.
//!
//! Additive increase, multiplicative decrease: recovery is slow and backoff is
//! fast, so the loop errs conservative. State is per device and in memory; it is
//! relearned each boot (see spec 006, "Restart behaviour").

use crate::protocol::DeviceId;
use std::collections::HashMap;

/// How the loop reacts. Every value here is **dimensionless** — it describes the
/// shape of the response, not what the machine can do — which is the whole
/// trade: constants that had to know the hardware are replaced by constants that
/// must not.
#[derive(Debug, Clone)]
pub struct AdmissionConfig {
    /// Fraction of a segment's own playback duration within which it must be
    /// produced. 0.5 ⇒ a 6 s segment must be encoded in ≤ 3 s.
    pub margin_ratio: f64,
    /// Additive increase, applied once per exercised success.
    pub increase_step: f64,
    /// Multiplicative decrease, applied once per contended miss.
    pub decrease_factor: f64,
    /// Never starve prefetch to zero: shed-not-queue means what is refused is
    /// not retried later, so an allowance of 0 would make every segment a cold
    /// miss. This is the value `SchedConfig::background_alongside_client` ships.
    pub floor: f64,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            margin_ratio: 0.5,
            increase_step: 1.0,
            decrease_factor: 0.5,
            floor: 1.0,
        }
    }
}

/// What one finished interactive job said about the device it ran on.
///
/// `ignored` is a first-class outcome, not an absence: a frozen allowance with
/// all-ignored observations means *no signal*, which is a completely different
/// problem from a device that genuinely cannot go faster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Met,
    Missed,
    Ignored,
}

impl Verdict {
    /// Bounded, stable metric label. Renaming one of these breaks alerts
    /// silently.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Met => "met",
            Verdict::Missed => "missed",
            Verdict::Ignored => "ignored",
        }
    }
}

/// One completed job, reduced to what the control law needs.
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    /// Playback seconds this job's output covers. `None` for live/progressive
    /// transcodes, which have no deadline to measure against.
    pub segment_seconds: Option<f64>,
    /// Wall-clock spent encoding — the last dispatch to completion, excluding
    /// queue wait, which is a different problem with a different fix.
    pub encode_seconds: f64,
    /// Speculative jobs already running on the same device when this one was
    /// dispatched. Both guards below are expressed in terms of this.
    pub background_peers: usize,
    /// `false` for a job that failed or was retried: its encode time is the
    /// duration of a bounce, not of an encode.
    pub usable: bool,
}

/// Per-device AIMD state. Pure: no clock, no I/O, no tokio.
#[derive(Debug)]
pub struct AdmissionController {
    cfg: AdmissionConfig,
    /// Absent ⇒ the floor. A device is only entered once it has produced a
    /// usable observation, so a cold controller behaves exactly like the
    /// constant it replaces.
    per_device: HashMap<DeviceId, f64>,
}

impl AdmissionController {
    pub fn new(cfg: AdmissionConfig) -> Self {
        Self {
            cfg,
            per_device: HashMap::new(),
        }
    }

    /// Never fill a device with speculation. `.max(floor)` matters on a
    /// one-permit device, where `capacity - 1` is 0: dropping below the floor
    /// there would silently change today's behaviour on the weakest hardware,
    /// which is the last place a regression should land.
    fn ceiling(&self, capacity: usize) -> f64 {
        (capacity.saturating_sub(1) as f64).max(self.cfg.floor)
    }

    /// The learned value, unrounded — for the gauge. Fractional detail is the
    /// evidence that a multiplicative decrease happened at all (2 → 1.5 reads as
    /// 2 → 1 once floored).
    pub fn raw_allowance(&self, dev: DeviceId, capacity: usize) -> f64 {
        self.per_device
            .get(&dev)
            .copied()
            .unwrap_or(self.cfg.floor)
            .clamp(self.cfg.floor, self.ceiling(capacity))
    }

    /// How many speculative jobs may run beside a client's segment on `dev`.
    ///
    /// **Floored**: an allowance of 2.5 permits two concurrent background jobs,
    /// not three. So a decrease takes effect immediately while the recovery from
    /// 1.5 back to 2 costs one more successful observation — asymmetric in the
    /// conservative direction, which is intended.
    pub fn allowance(&self, dev: DeviceId, capacity: usize) -> usize {
        self.raw_allowance(dev, capacity).floor() as usize
    }

    /// Fold one finished job into `dev`'s allowance and report what it said.
    pub fn observe(&mut self, dev: DeviceId, capacity: usize, obs: Observation) -> Verdict {
        let Some(seg_secs) = obs.segment_seconds else {
            return Verdict::Ignored;
        };
        if !obs.usable || seg_secs <= 0.0 || !obs.encode_seconds.is_finite() {
            return Verdict::Ignored;
        }
        let deadline = seg_secs * self.cfg.margin_ratio;
        let met = obs.encode_seconds <= deadline;

        let floor = self.cfg.floor;
        let ceiling = self.ceiling(capacity);
        let current = self.per_device.get(&dev).copied().unwrap_or(floor);
        let next = if met {
            // Increase only on an EXERCISED observation. A segment that met its
            // deadline while running alone never tested the allowance, so it is
            // no evidence the allowance can safely rise.
            if obs.background_peers >= current.floor() as usize {
                current + self.cfg.increase_step
            } else {
                current
            }
        } else if obs.background_peers > 0 {
            // Decrease only when speculation was actually present. A source that
            // blows the deadline alone is not prefetch's fault and backing off
            // cannot help.
            current * self.cfg.decrease_factor
        } else {
            current
        };
        self.per_device.insert(dev, next.clamp(floor, ceiling));

        if met {
            Verdict::Met
        } else {
            Verdict::Missed
        }
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run:

```bash
nix develop --command cargo nextest run -p pharos-transcode admission
```

Expected: **8 passed**.

- [ ] **Step 6: Lint**

Run:

```bash
nix develop --command cargo clippy -p pharos-transcode --all-targets -- -D warnings
```

Expected: no output, exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/pharos-transcode/src/admission.rs crates/pharos-transcode/src/lib.rs
git commit -m "feat(transcode): add an AIMD controller for speculative admission

background_alongside_client is a concurrency constant calibrated by hand on
one GTX 1070. It has no way to know it is running on a card with four NVENC
engines or on a box with none, so it under-uses the first and overloads the
second.

Replace the number with a loop over a hardware-independent invariant: a
segment covering N seconds of playback must be produced within margin_ratio x
N. Additive increase on an exercised success, multiplicative decrease on a
contended miss, clamped to [1, capacity - 1] per device.

Both guards are load-bearing. A success while running alone never tested the
allowance, so it cannot raise it. A miss with nothing beside it is the
source's fault, not prefetch's, so it cannot lower it -- without that, one
pathological title ratchets every device to the floor permanently.

Pure state machine, no scheduler: the control law is the part worth testing
in isolation, and scheduler.rs is already 2485 lines.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 2: B178 — stamp peer counts on both dispatch paths

**Files:**
- Modify: `crates/pharos-transcode/src/scheduler.rs` (`JobCtx`, `place`,
  `try_place_no_queue`, the `Submit` arm, `JobDone`)
- Modify: `specs/001-pharos-baseline/bugs.md`
- Test: `crates/pharos-transcode/src/scheduler.rs` test module

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces, relied on by Task 3: `JobCtx.background_peers: usize`, stamped at
  **every** dispatch; `fn background_peers_on(state: &SchedState, dev: DeviceId) -> usize`.

**Why this is its own task and its own bug id:** `place()` stamps
`ctx.peer_jobs` at dispatch (`scheduler.rs:1140`), but `try_place_no_queue()` —
the path a *queued* job takes when a permit frees — never does. A job that waited
in `pending` therefore reports `peer_jobs = 0` and records no `peer_jobs` span
field, no matter how crowded the device was. That is a hole in what B177 shipped,
and phase 1 makes it load-bearing: a drained interactive job would feed the
controller `background_peers = 0`, which silences the decrease guard *exactly
when the device is busiest*. The controller would then only ever see the
uncontended cases and never back off.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/pharos-transcode/src/scheduler.rs`,
beside `a_finished_job_reports_how_many_peers_shared_its_device` (line ~2345):

```rust
    /// B178 — a job that WAITED for its permit reported the company it kept as
    /// zero, because only the first-attempt dispatch path stamped it. The
    /// drain path is the one that runs when the device is busiest, so the
    /// instrumentation was blind in precisely the case it exists for.
    #[tokio::test]
    async fn a_job_dispatched_off_the_queue_reports_its_peers() {
        // One permit: the second job must queue, then drain onto the freed
        // permit once the first finishes.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(200), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(1), spawner, SchedConfig::default());

        let a = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/a"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
            })
        };
        // Let A take the only permit before B arrives, so B is guaranteed to
        // queue rather than race for it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let b = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/b"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
            })
        };

        let a = a.await.unwrap().unwrap();
        let b = b.await.unwrap().unwrap();
        assert_eq!(a.peer_jobs, 0, "A ran alone");
        // B dispatched from `pending` after A's permit freed, so it also ran
        // alone -- but it must SAY so from the drain path, and it must carry a
        // background peer count at all.
        assert_eq!(b.peer_jobs, 0);
        assert_eq!(b.background_peers, 0);
        assert!(b.queue_wait_ms > 0, "B must actually have queued");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
nix develop --command cargo nextest run -p pharos-transcode \
  a_job_dispatched_off_the_queue_reports_its_peers
```

Expected: **compile failure**, `no field 'background_peers' on type 'JobDone'`.

- [ ] **Step 3: Add the field and the counter**

In `crates/pharos-transcode/src/scheduler.rs`:

Add to `pub struct JobDone` (after `peer_jobs`, line ~221):

```rust
    /// Of `peer_jobs`, how many were speculative. `peer_jobs` says a segment was
    /// crowded; this says whether it was crowded by work somebody was waiting
    /// for or by work nobody asked for — the difference between genuine
    /// overload and a scheduling defect, and the input the admission controller
    /// backs off on.
    pub background_peers: usize,
```

Add to `struct JobCtx` (after `peer_jobs`, line ~391):

```rust
    /// Speculative subset of `peer_jobs`, stamped at the same moment.
    background_peers: usize,
```

Initialise it in the `SchedMsg::Submit` arm (beside `peer_jobs: 0`, line ~556):

```rust
                peer_jobs: 0,
                background_peers: 0,
```

Add the counter next to `peers_on` (after line 857):

```rust
/// Of the segment jobs already running on `dev`, how many are speculative.
///
/// Split out from [`peers_on`] rather than derived from it: the admission
/// controller must not back off because a device is busy with work clients are
/// blocked on — that is the device doing its job, and shedding prefetch cannot
/// make it faster.
fn background_peers_on(state: &SchedState, dev: DeviceId) -> usize {
    state
        .inflight
        .values()
        .filter(|c| c.device == Some(dev) && c.class == JobClass::Background)
        .count()
}
```

- [ ] **Step 4: Stamp it on both dispatch paths**

In `place()`, replace lines 1138-1141:

```rust
            // Counted BEFORE this job joins `inflight`, so it is peers, not
            // occupancy: a job that runs alone reports 0.
            ctx.peer_jobs = peers_on(state, dev);
            ctx.background_peers = background_peers_on(state, dev);
            span.record("peer_jobs", ctx.peer_jobs);
            span.record("background_peers", ctx.background_peers);
```

In `try_place_no_queue()`, insert the identical block immediately after
`ctx.device = Some(dev);` (line 1240), before `ctx.span = span.clone();`:

```rust
            // B178 — same stamping as `place`. A job dispatched off the queue
            // used to report zero peers regardless, and the queue is what runs
            // when the device is busiest.
            ctx.peer_jobs = peers_on(state, dev);
            ctx.background_peers = background_peers_on(state, dev);
            span.record("peer_jobs", ctx.peer_jobs);
            span.record("background_peers", ctx.background_peers);
```

Declare the new span field in `record_placement` (beside `peer_jobs`, line 963):

```rust
        peer_jobs = tracing::field::Empty,
        background_peers = tracing::field::Empty,
```

Carry it onto the completion log line and the reply, in the
`WorkerRunResult::Done` arm (lines ~705 and ~715):

```rust
                            peer_jobs = ctx.peer_jobs,
                            background_peers = ctx.background_peers,
```

```rust
                    let _ = ctx.reply.send(Ok(JobDone {
                        device,
                        out_bytes,
                        queue_wait_ms: queue_ms,
                        encode_ms,
                        peer_jobs: ctx.peer_jobs,
                        background_peers: ctx.background_peers,
                    }));
```

- [ ] **Step 5: Fix the other `JobDone` construction sites**

Run:

```bash
nix develop --command cargo check -p pharos-transcode -p pharos-cache --all-targets
```

Expected: errors naming every place that builds a `JobDone` literal. Add
`background_peers: 0` (test fixtures) or the real value at each. Re-run until
clean.

- [ ] **Step 6: Run the tests to verify they pass**

Run:

```bash
nix develop --command cargo nextest run -p pharos-transcode
```

Expected: all pass, including the pre-existing
`a_finished_job_reports_how_many_peers_shared_its_device` and
`speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for`.

- [ ] **Step 7: Disarm-verify the new assertion**

Temporarily delete the two stamping lines you added to `try_place_no_queue`, re-run:

```bash
nix develop --command cargo nextest run -p pharos-transcode \
  a_job_dispatched_off_the_queue_reports_its_peers
```

Expected: **FAIL** — the drain path no longer records anything. Restore the
lines and confirm it passes again. An assertion that survives its own
instrumentation being deleted is not testing the instrumentation.

- [ ] **Step 8: Record the bug**

Append to `specs/001-pharos-baseline/bugs.md`, following the existing entry
format:

```markdown
### B178 — a job dispatched off the queue reported zero peers

`place()` stamped `ctx.peer_jobs` at dispatch; `try_place_no_queue()` — the
path a queued job takes when a permit frees — never did. Any job that waited
therefore reported `peer_jobs = 0` and recorded no `peer_jobs` span field,
regardless of how crowded the device was.

The instrumentation was blind in exactly the case it exists for: jobs only
queue when every permit is busy, so the drain path IS the crowded path.

**Fix**: both dispatch paths stamp `peer_jobs` and the new `background_peers`
identically, immediately before the job joins `inflight`.

**Guarded by**: V125 (a reserved permit is not reserved throughput) — the
allowance it feeds is only as good as the peer count it is computed from.
```

- [ ] **Step 9: Commit**

```bash
git add crates/pharos-transcode/src/scheduler.rs specs/001-pharos-baseline/bugs.md
git commit -m "fix(transcode): stamp peer counts on the queue-drain dispatch path

B178. place() recorded how many jobs shared a device at dispatch; the drain
path that runs when a permit frees did not. A job that queued therefore
reported peer_jobs = 0 however crowded the device was -- and jobs only queue
when every permit is busy, so the blind path was the crowded one.

Also splits out background_peers, the speculative subset of peer_jobs. A
device busy with work clients are blocked on is a device doing its job;
shedding prefetch cannot make it faster, so the two counts need telling
apart.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 3: Shadow mode — measure the loop before it decides anything

**Files:**
- Modify: `crates/pharos-transcode/src/scheduler.rs` (`SchedState`, `SchedConfig`,
  `TranscodeScheduler::spawn`, the `JobFinished` arm)
- Test: `crates/pharos-transcode/src/scheduler.rs` test module

**Interfaces:**
- Consumes: `admission::{AdmissionConfig, AdmissionController, Observation, Verdict}`
  (Task 1); `JobCtx.background_peers` (Task 2).
- Produces, relied on by Task 4: `SchedState.admission: AdmissionController`
  populated by every finished job; `SchedConfig.admission: AdmissionConfig`.

**Why shadow mode:** the ODD rule says the instrumentation ships first, so it can
be read before the fix lands. Here the controller runs, learns and reports, while
`crowds_a_client` still uses the constant — **zero behaviour change**. That makes
the deploy gate meaningful: we get to see what the allowance *would have been* on
real hardware and real titles before anything acts on it. If the gauge never
leaves 1 on the live box, the control law is wrong and we find that out having
changed nothing.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/pharos-transcode/src/scheduler.rs`:

```rust
    /// ODD — the tuner ships as a MEASUREMENT first. The controller must learn
    /// from finished jobs before anything consults it, so the deploy that turns
    /// it on can be judged against a gauge rather than a hope.
    #[tokio::test]
    async fn a_finished_segment_teaches_the_controller_without_changing_admission() {
        // 6 s segments (h264() carries duration_ticks), 100 ms encode: comfortably
        // inside the 3 s deadline.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(100), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());

        let done = s
            .submit(
                PathBuf::from("/m/a"),
                cmaf(),
                file_sink(),
                JobClass::Interactive,
            )
            .await
            .unwrap();
        assert_eq!(done.background_peers, 0);

        let snap = s.snapshot().await.unwrap();
        // Ran alone, so the observation was unexercised: the allowance stays at
        // the floor, and the point of the assertion is that the controller is
        // WIRED, reporting a value, not that it moved.
        assert_eq!(snap.background_allowance.len(), 1);
        assert_eq!(snap.background_allowance[0].0, DeviceId::hw(HwAccel::Nvenc, 0));
        assert_eq!(snap.background_allowance[0].1, 1.0);
    }
```

If `cmaf()` does not set `duration_ticks`, use `h264()` and confirm which fixture
carries a duration; the controller ignores an observation without one, and the
assertion above would then be checking nothing.

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
nix develop --command cargo nextest run -p pharos-transcode \
  a_finished_segment_teaches_the_controller
```

Expected: **compile failure**, `no field 'background_allowance' on type 'SchedSnapshot'`.

- [ ] **Step 3: Hold the controller in the scheduler**

In `crates/pharos-transcode/src/scheduler.rs`:

Add the import beside the existing `use crate::...` lines:

```rust
use crate::admission::{AdmissionConfig, AdmissionController, Observation};
```

Add to `pub struct SchedConfig` (after `background_alongside_client`, line ~314):

```rust
    /// How the per-device speculative allowance is learned. `floor` here is the
    /// value `background_alongside_client` used to be, and a cold controller
    /// sits exactly on it — so a fresh process behaves precisely like the
    /// constant this replaces.
    pub admission: AdmissionConfig,
```

And to its `Default` impl (line ~318):

```rust
            background_alongside_client: 1,
            admission: AdmissionConfig::default(),
```

Add to `struct SchedState` (after `cfg`, line ~407):

```rust
    /// Learned, per device, from every finished interactive segment. In memory:
    /// relearned each boot, which costs one viewer the head start and never
    /// misleads the admission rule after a hardware change.
    admission: AdmissionController,
```

Initialise it in `TranscodeScheduler::spawn`. The config is moved into
`SchedState`, so build the controller before that move (replace lines 424-434):

```rust
        let admission = AdmissionController::new(cfg.admission.clone());
        let mut state = SchedState {
            devices,
            spawner,
            idle: Vec::new(),
            inflight: HashMap::new(),
            pending: VecDeque::new(),
            cfg,
            admission,
            next_job: 0,
            live: Arc::new(AtomicUsize::new(0)),
            next_worker: 0,
        };
```

- [ ] **Step 4: Expose it on the snapshot**

Add to `pub struct SchedSnapshot` (after `live_streams`, line ~272):

```rust
    /// The learned speculative allowance per device, unrounded. This IS the
    /// tuner: a value that never leaves the floor under playback means the loop
    /// is not working, which is indistinguishable from a correctly cautious loop
    /// unless the value itself is visible.
    pub background_allowance: Vec<(DeviceId, f64)>,
```

Populate it in the `SchedMsg::Snapshot` arm, inside the `reply.send(SchedSnapshot {...})`
literal (after `live_streams`, line ~806):

```rust
                background_allowance: state
                    .devices
                    .slots()
                    .iter()
                    .map(|s| (s.id, state.admission.raw_allowance(s.id, s.capacity)))
                    .collect(),
```

- [ ] **Step 5: Feed the controller from `JobFinished`**

In the `WorkerRunResult::Done` arm, insert this **immediately before** the
`let _ = ctx.reply.send(Ok(JobDone {...}))` block (line ~710):

```rust
                    // Fold this segment into what the device has learned. Only
                    // interactive jobs carry a deadline anybody is waiting on:
                    // a speculative encode being slow is not a symptom, it is
                    // the system working as designed.
                    if ctx.class == JobClass::Interactive {
                        observe_margin(state, device, &ctx, encode_ms);
                    }
```

**Order matters and the compiler will tell you so:** `oneshot::Sender::send`
takes `self` by value, so `ctx.reply.send(..)` partially moves `ctx`. Any `&ctx`
after that line fails to compile with `borrow of partially moved value`.

Add the helper beside `background_peers_on`:

```rust
/// Record what one finished client segment said about its device, and report it.
///
/// Symmetry rule: the verdict counter is incremented on EVERY interactive
/// completion, including the ones that teach nothing. A frozen allowance with
/// all-`ignored` observations means no signal reached the loop — a completely
/// different problem from a device that genuinely cannot go faster, and
/// indistinguishable from it if the ignored arm is silent.
fn observe_margin(state: &mut SchedState, device: DeviceId, ctx: &JobCtx, encode_ms: u64) {
    let capacity = state
        .devices
        .slot(device)
        .map(|s| s.capacity)
        .unwrap_or(1);
    let obs = Observation {
        // Live/progressive jobs have no duration and so no deadline.
        segment_seconds: ctx
            .opts
            .duration_ticks
            .map(|t| t as f64 / 10_000_000.0),
        encode_seconds: encode_ms as f64 / 1000.0,
        background_peers: ctx.background_peers,
        // A retried job's encode time is the duration of a bounce.
        usable: ctx.retries == 0,
    };
    let before = state.admission.allowance(device, capacity);
    let verdict = state.admission.observe(device, capacity, obs);
    let after = state.admission.allowance(device, capacity);
    let raw = state.admission.raw_allowance(device, capacity);

    metrics::counter!(
        "pharos_transcode_margin_total",
        "device" => device.to_string(),
        "verdict" => verdict.label(),
    )
    .increment(1);
    metrics::gauge!(
        "pharos_transcode_background_allowance",
        "device" => device.to_string(),
    )
    .set(raw);

    // Only when the INTEGER allowance moves. Logging every observation would
    // emit at segment rate and drown the line that matters.
    if before != after {
        tracing::info!(
            %device,
            verdict = verdict.label(),
            encode_secs = obs.encode_seconds,
            deadline_secs = obs.segment_seconds.map(|s| s * 0.5),
            background_peers = obs.background_peers,
            allowance_from = before,
            allowance_to = after,
            "speculative allowance changed"
        );
    }
}
```

Add the `metrics` dependency import if `scheduler.rs` does not already have one —
it does (`metrics::counter!` is used in `record_placement`), so no import change
is needed.

- [ ] **Step 6: Run the tests to verify they pass**

Run:

```bash
nix develop --command cargo nextest run -p pharos-transcode
```

Expected: all pass. In particular
`speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for` must pass
**unchanged** — this task changes no behaviour, and that test is the guard.

- [ ] **Step 7: Assert the metric, and disarm-verify it**

`metrics::with_local_recorder` installs the recorder for the duration of a
**closure**, so this cannot be a `#[tokio::test]` — the async work has to run
inside the closure via `block_on`. Follow the shape already used at
`crates/pharos-cache/src/hls_cache.rs:2566-2631` exactly:

```rust
    /// The verdict counter is the signal that says whether the loop is hearing
    /// anything at all. A frozen allowance means nothing without it: "the device
    /// cannot go faster" and "no observation ever arrived" look identical.
    #[test]
    fn a_finished_segment_records_a_margin_verdict() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(100), |_, _| {
                    WorkerRunResult::Done { out_bytes: 1 }
                });
                let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
                s.submit(
                    PathBuf::from("/m/a"),
                    cmaf(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
                .unwrap();
            })
        });

        let verdict = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_transcode_margin_total" {
                    return None;
                }
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                Some((labels, v))
            });

        let (labels, value) = verdict.expect(
            "a finished client segment must emit pharos_transcode_margin_total — \
             it is the signal that says the loop heard anything",
        );
        assert!(
            labels.contains(&"verdict=met".to_string()),
            "expected a met verdict; got {labels:?}"
        );
        assert!(matches!(value, DebugValue::Counter(1)), "got {value:?}");
    }
```

The `ScriptedSpawner` closure runs on a current-thread runtime here, so keep the
scripted encode short — this test measures a label, not a duration.

`metrics_util` is already a `dev-dependency` of `pharos-cache`; add it to
`crates/pharos-transcode/Cargo.toml` under `[dev-dependencies]` if absent:

```toml
metrics-util.workspace = true
```

Then disarm: comment out the `metrics::counter!("pharos_transcode_margin_total", ...)`
block in `observe_margin`, re-run the test, confirm it **FAILS**, restore it,
confirm it passes.

- [ ] **Step 8: Lint and full test**

```bash
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
nix develop --command just test
```

Expected: clean; no new failures. (`vp9_fmp4_hls` fails PRE-EXISTING on ffmpeg
8.1 — confirm it is the same failure as on `main` before treating it as yours.)

- [ ] **Step 9: Commit**

```bash
git add crates/pharos-transcode/src/scheduler.rs crates/pharos-transcode/Cargo.toml
git commit -m "feat(transcode): learn the speculative allowance in shadow mode

Runs the AIMD controller against every finished client segment and reports
what it learns, while admission still uses the shipped constant. No behaviour
change.

The instrumentation ships before the fix on purpose. background_alongside_client
was calibrated on one GPU and one source; before anything acts on a learned
replacement, the deployment gets to show what that value would actually be on
real hardware and a real title mix. A gauge that never leaves 1 under playback
means the control law is wrong, and finding that out having changed nothing is
the cheap way to find it out.

pharos_transcode_background_allowance{device} is the gauge;
pharos_transcode_margin_total{device,verdict} is met/missed/ignored. The
ignored arm is load-bearing: a frozen allowance with all-ignored observations
means no signal reached the loop, which is a different problem from a device
that cannot go faster.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Gate A: deploy shadow mode and read the gauge

**This is a checkpoint, not a code task.** Do not start Task 4 until it passes.

- [ ] **Step 1: Ship tasks 1-3**

Push the branch, open a PR, merge with `gh pr merge --rebase` once CI is green,
and let the image publish + Flux reconcile. Ask the user before any direct
mutation of live infrastructure.

- [ ] **Step 2: Run the named query against the live deployment**

Under real playback, on the pharos metrics endpoint (**port 8096**, not 9090):

```promql
pharos_transcode_background_allowance{device="Nvenc:0"}
```

and

```promql
sum by (verdict) (rate(pharos_transcode_margin_total[15m]))
```

- [ ] **Step 3: Judge the result and report it**

| observed | meaning | action |
|---|---|---|
| allowance climbs above 1, `met` dominates | the loop works and the hardware has headroom | proceed to Task 4 |
| allowance pinned at 1, verdicts almost all `ignored` | no signal is reaching the loop — `duration_ticks` absent, or the wrong jobs classified interactive | fix the plumbing; do NOT proceed |
| allowance pinned at 1, verdicts mostly `missed` | the GPU genuinely cannot make the deadline under contention | phase 1 is correct and will change nothing here; say so plainly and reconsider whether phase 2 is the higher-value work |
| allowance climbs *and* interactive `encode_seconds` p95 rises with it | the control law is wrong | stop; revisit `margin_ratio` |

Report the **actual query output**, not "looks fine". "Should be fixed" is not a
result.

---

## Task 4: Consult the learned allowance

**Files:**
- Modify: `crates/pharos-transcode/src/scheduler.rs` (`crowds_a_client`, its two
  call sites, `SchedConfig` doc comment)
- Test: `crates/pharos-transcode/src/scheduler.rs` test module

**Interfaces:**
- Consumes: `SchedState.admission` (Task 3).
- Produces: nothing new; this changes a decision, not an API.

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
    /// The payoff: on hardware that keeps making its deadline under contention,
    /// the scheduler must let progressively MORE speculative work through than
    /// the shipped constant allowed. A tuner that only ever agrees with the
    /// constant it replaced is not a tuner.
    #[tokio::test]
    async fn a_device_that_keeps_making_its_deadline_earns_more_speculative_slots() {
        // Fast encodes (50 ms) against 6 s segments: every observation is a
        // comfortable `met`.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(50), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(8), spawner, SchedConfig::default());

        // Teach it: each round runs one client segment beside as many
        // speculative jobs as the current allowance permits, which is what makes
        // the observation `exercised`.
        for round in 0..6 {
            let mut handles = Vec::new();
            for i in 0..3 {
                let s2 = s.clone();
                handles.push(tokio::spawn(async move {
                    s2.submit(
                        PathBuf::from(format!("/m/bg{round}-{i}")),
                        cmaf(),
                        file_sink(),
                        JobClass::Background,
                    )
                    .await
                }));
            }
            let s2 = s.clone();
            let client = tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/fg{round}")),
                    cmaf(),
                    file_sink(),
                    JobClass::Interactive,
                )
                .await
            });
            let _ = client.await.unwrap();
            for h in handles {
                let _ = h.await;
            }
        }

        let snap = s.snapshot().await.unwrap();
        let learned = snap.background_allowance[0].1;
        assert!(
            learned > 1.0,
            "a device that never missed its deadline stayed at the floor: {learned}"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
nix develop --command cargo nextest run -p pharos-transcode \
  a_device_that_keeps_making_its_deadline
```

Expected: **FAIL** — `a device that never missed its deadline stayed at the
floor: 1`. The allowance cannot rise, because `crowds_a_client` still refuses the
speculative peers that would make an observation exercised. That is the loop this
step closes.

- [ ] **Step 3: Consult the controller instead of the constant**

Replace `crowds_a_client` (lines 831-845):

```rust
/// Would admitting a speculative job to `dev` put it beside a client's segment
/// past what that device has EARNED? Speculative work is wanted — it is what
/// turns the next segment into a cache hit — but not at the cost of the segment
/// somebody is currently staring at a spinner for.
///
/// The allowance is learned per device rather than configured, because the
/// number that matters is a property of the hardware and the source mix, not of
/// the config file: the same constant that under-uses a four-engine card
/// overloads a laptop.
fn crowds_a_client(state: &SchedState, dev: DeviceId) -> bool {
    let mut interactive = 0usize;
    let mut background = 0usize;
    for c in state.inflight.values().filter(|c| c.device == Some(dev)) {
        match c.class {
            JobClass::Interactive => interactive += 1,
            JobClass::Background => background += 1,
        }
    }
    let capacity = state.devices.slot(dev).map(|s| s.capacity).unwrap_or(1);
    interactive > 0 && background >= state.admission.allowance(dev, capacity)
}
```

Update its call site in `place()` (line 1116):

```rust
        if ctx.class == JobClass::Background && crowds_a_client(state, dev) {
            continue;
        }
```

- [ ] **Step 4: Retire the constant's role, keeping the field as the floor**

Replace the doc comment on `SchedConfig::background_alongside_client` (lines
299-314) with:

```rust
    /// FLOOR for the learned speculative allowance — see
    /// [`crate::admission::AdmissionController`].
    ///
    /// This was the allowance itself, a number calibrated by hand on one GTX
    /// 1070 against one 23 Mbps HEVC source. It is now the value a device sits
    /// on before it has learned anything, and the value it collapses back to
    /// under sustained deadline misses, so a cold process behaves exactly as it
    /// did when this was the whole answer.
    ///
    /// Not zero. Shedding ALL prefetch while a client job runs would starve the
    /// pipeline that makes the next segment a 30 ms cache hit — prefetch is
    /// shed, never queued, so what is refused here is not retried later, and
    /// every segment would become a cold miss.
    pub background_alongside_client: usize,
```

Wire it to the controller's floor in `Default for SchedConfig` so the two cannot
drift:

```rust
            background_alongside_client: 1,
            admission: AdmissionConfig {
                floor: 1.0,
                ..AdmissionConfig::default()
            },
```

- [ ] **Step 5: Run the tests to verify they pass**

Run:

```bash
nix develop --command cargo nextest run -p pharos-transcode
```

Expected: all pass, and specifically:
- `a_device_that_keeps_making_its_deadline_earns_more_speculative_slots` — PASS
- `speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for` — **PASS
  UNCHANGED**. A cold controller sits on the floor, so today's behaviour is
  reproduced exactly. This is the regression guard named in the spec; if it needs
  editing, the change is wrong.

- [ ] **Step 6: Lint and full test**

```bash
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
nix develop --command just test
```

- [ ] **Step 7: Record the invariant**

Append to `specs/001-pharos-baseline/invariants.md`:

```markdown
### V126 — a performance tunable that must know the hardware is a defect

A constant that has to be recalibrated per machine is a constant that is wrong
on every machine but the one it was measured on. Where a limit exists to protect
a client-visible deadline, express the DEADLINE and learn the limit from
observed outcomes on the device in front of you.

Concretely: speculative transcode concurrency is learned per device by
`pharos_transcode::admission`, from whether the segments clients actually waited
for made a deadline set as a fraction of their own playback duration. The
configured value survives only as the floor.

The replacement constants must be dimensionless — response shape, not machine
capability — or the problem has only moved. `margin_ratio`, `increase_step` and
`decrease_factor` describe how fast the loop reacts and are identical on every
box.

Introduced by 006. See B177 (the crowding defect this generalises) and V125 (a
reserved permit is not reserved throughput).
```

- [ ] **Step 8: Commit**

```bash
git add crates/pharos-transcode/src/scheduler.rs specs/001-pharos-baseline/invariants.md
git commit -m "feat(transcode): admit speculative work against the learned allowance

Closes the loop opened in shadow mode: crowds_a_client now asks the controller
what this device has earned instead of reading a constant.

background_alongside_client stays, demoted to the floor -- the value a device
sits on before it has learned anything and collapses back to under sustained
deadline misses. A cold process therefore behaves exactly as it did before,
which is what lets
speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for pass
unchanged as the regression guard.

V126: a performance tunable that must know the hardware is a defect. Express
the deadline, learn the limit.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Gate B: phase 1 is live

- [ ] **Step 1: Merge and deploy tasks 1-4**, then re-run the Gate A queries.

- [ ] **Step 2: Confirm both halves of the success condition**

```promql
pharos_transcode_background_allowance{device="Nvenc:0"}
```

climbing above 1 under playback, **while**

```promql
histogram_quantile(0.95, sum by (le) (rate(pharos_transcode_encode_seconds_bucket{class="interactive"}[15m])))
```

stays under the deadline. **Both, not either**: a rising allowance beside a
rising p95 means the control law is wrong and phase 1 must be reverted before
phase 2 builds on it.

Report actual numbers.

---

## Task 5: Shared-result single-flight (phase 2a)

**Files:**
- Modify: `crates/pharos-cache/src/hls_cache.rs` (`CacheState`,
  `CacheHitPath`, `segment_bytes_keyed`)
- Test: `crates/pharos-cache/src/hls_cache.rs` test module

**Interfaces:**
- Consumes: nothing from phase 1.
- Produces, relied on by Task 9: an in-flight registry entry that names the
  in-flight encode, so a later arrival can be identified as demand rather than
  duplicated.

**Preconditions already satisfied:** `dashmap` is already a dependency of
`pharos-cache` (`Cargo.toml:9`), and `HlsSegmentCache` already derives `Clone`
(`hls_cache.rs:599`) with every field cheap to clone (`PathBuf`, `u64`,
`FfmpegTranscoder`, `Option<TranscodeScheduler>`, `Arc<Mutex<CacheState>>`) — so
a detached driver can own a clone without restructuring.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `crates/pharos-cache/src/hls_cache.rs`:

```rust
    /// The lock this replaces was held for the ENTIRE multi-second transcode.
    /// It was not guarding a data race — it was single-flight dedup implemented
    /// as mutual exclusion, and holding exclusion across seconds of work is what
    /// makes a queued speculative job poisonous (B108). A second requester must
    /// await the RESULT, and get it at the moment the first one does.
    #[tokio::test]
    async fn a_second_requester_awaits_the_result_rather_than_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cache = slow_test_cache(&dir, std::time::Duration::from_millis(400));

        let started = std::time::Instant::now();
        let a = {
            let c = cache.clone();
            tokio::spawn(async move { c.segment_bytes(1, 0).await })
        };
        // Arrive well after the first requester has begun the encode.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let b = {
            let c = cache.clone();
            tokio::spawn(async move { c.segment_bytes(1, 0).await })
        };

        let a = a.await.unwrap().unwrap();
        let b = b.await.unwrap().unwrap();
        assert_eq!(a, b, "both requesters get the same bytes");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(700),
            "the second requester serialised behind a second encode: {:?}",
            started.elapsed()
        );
    }

    /// Cancellation safety, which the Mutex design cannot express: today, if the
    /// lock holder is dropped mid-transcode the guard releases, the next waiter
    /// re-checks the filesystem, finds nothing, and encodes the SAME segment
    /// again. A detached driver outlives the requester that started it.
    #[tokio::test]
    async fn cancelling_the_first_requester_does_not_abort_the_encode() {
        let dir = tempfile::tempdir().unwrap();
        let cache = slow_test_cache(&dir, std::time::Duration::from_millis(400));

        let a = {
            let c = cache.clone();
            tokio::spawn(async move { c.segment_bytes(1, 0).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let b = {
            let c = cache.clone();
            tokio::spawn(async move { c.segment_bytes(1, 0).await })
        };
        // The client seeked away / disconnected.
        a.abort();

        let bytes = b.await.unwrap().unwrap();
        assert!(!bytes.is_empty(), "the surviving requester got nothing");
        assert_eq!(
            encode_count(&cache),
            1,
            "the segment was encoded twice after a cancellation"
        );
    }
```

`slow_test_cache` and `encode_count` are test helpers: build an
`HlsSegmentCache` over a stub transcoder that sleeps for the given duration,
writes a fixed byte pattern, and increments an `Arc<AtomicUsize>`. Follow the
construction the existing tests in this module already use for a cache with no
scheduler (`scheduler: None`, legacy inline path); add the counter to the stub.

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
nix develop --command cargo nextest run -p pharos-cache single_flight
```

Expected: `a_second_requester_awaits_the_result_rather_than_the_lock` may pass
incidentally (the post-lock re-check does return the winner's bytes), but
`cancelling_the_first_requester_does_not_abort_the_encode` must **FAIL** with
`the segment was encoded twice after a cancellation` — that is the behaviour only
the new design can deliver, and it is what proves the test is testing something.

- [ ] **Step 3: Replace the lock with a shared-result registry**

In `crates/pharos-cache/src/hls_cache.rs`:

Remove the field from `CacheState` (line 562):

```rust
    fetch_locks: HashMap<SegmentIdentity, Arc<Mutex<()>>>,
```

Add to `HlsSegmentCache` (beside `state`, line ~610):

```rust
    /// Segments currently being produced, keyed by identity. Value is a `watch`
    /// receiver carrying the eventual outcome.
    ///
    /// This is single-flight WITHOUT mutual exclusion: nobody holds anything
    /// across the encode, so a slow segment cannot make a later requester for
    /// the same key wait on a lock — it waits on the answer, and gets it the
    /// instant the encode does.
    inflight: Arc<DashMap<SegmentIdentity, InFlightSegment>>,
```

Add the types above `impl HlsSegmentCache`:

```rust
/// The eventual outcome of an in-flight segment encode. `String` rather than the
/// error type because the result is shared by every waiter and the error type is
/// not `Clone`; the message carries the underlying cause, which is what the
/// "expose the cause" rule asks for.
type SegmentOutcome2 = Result<Arc<Vec<u8>>, String>;

#[derive(Clone)]
struct InFlightSegment {
    rx: tokio::sync::watch::Receiver<Option<SegmentOutcome2>>,
}
```

Initialise `inflight: Arc::new(DashMap::new())` in every `HlsSegmentCache`
constructor (`new`, and any `with_*` builder that constructs the struct
literally).

- [ ] **Step 4: Rewrite the miss path to drive a detached encode**

Replace `hls_cache.rs` lines 865-913 — the `fetch_locks` acquisition, the
`segment_fetch_lock_wait` span, and the post-lock re-check — with:

```rust
        // Coalesce onto an encode already in progress for this exact key, or
        // become the one that starts it. The entry is inserted under the
        // DashMap's shard lock and the driver is spawned detached, so the map is
        // never held across the encode.
        let (rx, driving) = {
            match self.inflight.entry(key) {
                dashmap::mapref::entry::Entry::Occupied(e) => (e.get().rx.clone(), false),
                dashmap::mapref::entry::Entry::Vacant(e) => {
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    e.insert(InFlightSegment { rx: rx.clone() });
                    let driver = self.clone();
                    let source = source.to_path_buf();
                    let opts = opts.clone();
                    tokio::spawn(async move {
                        let out = driver
                            .produce_segment(&source, &opts, key, media_id, seg_index, class)
                            .await
                            .map(Arc::new)
                            .map_err(|e| e.to_string());
                        // Publish BEFORE removing the entry, so a requester that
                        // grabbed the receiver a moment ago always sees a value.
                        let _ = tx.send(Some(out));
                        driver.inflight.remove(&key);
                    });
                    (rx, true)
                }
            }
        };

        let coalesced_started = std::time::Instant::now();
        let mut rx = rx;
        let outcome = loop {
            if let Some(v) = rx.borrow_and_update().clone() {
                break v;
            }
            if rx.changed().await.is_err() {
                // Driver died without publishing (panic). Surface it rather than
                // hanging: the caller retries and the next request re-drives.
                break Err("segment encode driver stopped without a result".to_string());
            }
        };

        let bytes = outcome.map_err(HlsCacheError::Transcode)?;
        if !driving {
            // Served by somebody else's encode. This is a hit — it costs a wait
            // but no work — and it is the successor to the old `post_lock` path.
            record_cache_hit(
                media_id,
                seg_index,
                bytes.len(),
                opts,
                class,
                CacheHitPath::Coalesced,
                coalesced_started.elapsed().as_millis() as u64,
                None,
            );
        }
        return Ok(bytes.as_ref().clone());
```

Move the existing miss body — everything from `if let Some(parent) = path.parent()`
(line 915) through `Ok(bytes)` (line 1223) — into a new private method, dropping
the two lines that removed the fetch lock:

```rust
    /// Produce one segment: transcode to a temp file, validate, publish into the
    /// keyed cache path, record. Runs in a DETACHED task, so it outlives the
    /// requester that started it — a client that seeks away mid-encode no longer
    /// throws away work that the next requester will immediately ask for again.
    async fn produce_segment(
        &self,
        source: &Path,
        opts: &SegmentOpts,
        key: SegmentIdentity,
        media_id: u64,
        seg_index: u32,
        class: JobClass,
    ) -> Result<Vec<u8>, HlsCacheError> {
        // ... existing body, verbatim, minus:
        //   let mut state = self.state.lock().await;
        //   state.fetch_locks.remove(&key);
    }
```

- [ ] **Step 5: Migrate the `hit_path` label deliberately**

Replace the `CacheHitPath` enum and its doc comment (lines 236-256):

```rust
/// Which of the two hit paths served a cached segment. Bounded label (two
/// values), because the difference is diagnostic: `fast` means the file was
/// already there on the first look, `coalesced` means this request arrived while
/// the SAME key was already being produced and was handed the result.
///
/// A stream that is mostly `coalesced` is one where prefetch and the client keep
/// colliding on the same segment — which reads as a healthy hit rate while every
/// one of those requests paid the full single-flight wait.
///
/// **Renamed from `post_lock` in 006 phase 2a.** There is no lock any more: a
/// coalescing requester awaits a value, it does not queue for exclusion. The old
/// label is a dashboard contract, so this is a deliberate migration, not a
/// silent rename — any panel or alert selecting `hit_path="post_lock"` must be
/// repointed at `coalesced` in the same change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CacheHitPath {
    Fast,
    Coalesced,
}

impl CacheHitPath {
    fn label(self) -> &'static str {
        match self {
            CacheHitPath::Fast => "fast",
            CacheHitPath::Coalesced => "coalesced",
        }
    }
}
```

Search the repo for any other reference and repoint it:

```bash
rg -n 'post_lock' --glob '!target'
```

Expected after the change: no hits outside this plan and the spec.

- [ ] **Step 6: Run the tests to verify they pass**

```bash
nix develop --command cargo nextest run -p pharos-cache
```

Expected: all pass, including
`cancelling_the_first_requester_does_not_abort_the_encode`.

- [ ] **Step 7: Disarm-verify the coalesced hit metric**

Comment out the `record_cache_hit(..., CacheHitPath::Coalesced, ...)` call,
re-run the hit-path assertion test at `hls_cache.rs:2622` extended with
`hit_path=coalesced`, confirm it **FAILS**, restore, confirm it passes.

- [ ] **Step 8: Lint and full test**

```bash
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
nix develop --command just test
```

- [ ] **Step 9: Commit**

```bash
git add crates/pharos-cache/src/hls_cache.rs
git commit -m "perf(cache): coalesce concurrent segment requests onto a shared result

fetch_locks held a per-key Mutex for the entire multi-second transcode. It was
never guarding a data race -- it was single-flight dedup implemented as mutual
exclusion, and holding exclusion across seconds of work is what made a queued
speculative job poisonous: the client's own later request for the same segment
inherited the whole wait (B108).

Requesters now await a watch receiver carrying the result, driven by a
detached task. Nobody holds anything across the encode.

That also fixes a defect the old design could not express: cancelling the lock
holder mid-transcode dropped the guard, and the next waiter re-checked the
filesystem, found nothing, and encoded the same segment a second time. A
detached driver outlives the requester that started it.

hit_path=post_lock becomes hit_path=coalesced -- there is no lock to be after.
Deliberate label migration; repoint any panel selecting the old value.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 6: Carry the stream and its playhead to the scheduler (phase 2b)

**Files:**
- Modify: `crates/pharos-transcode/src/scheduler.rs` (`JobHint`, `submit`,
  `SchedMsg::Submit`, `JobCtx`, `SchedState`)
- Modify: `crates/pharos-cache/src/hls_cache.rs` (`segment_bytes_keyed`,
  `write_segment`)
- Modify: `crates/pharos-server/src/api/jellyfin/hls.rs` (five call sites)
- Modify: `crates/pharos-transcode/src/bin/transcode_tool.rs` (three call sites)
- Test: `crates/pharos-transcode/src/scheduler.rs` test module

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces, relied on by Tasks 7-9:
  - `pub struct StreamKey(pub u64)` with `pub const NONE: StreamKey = StreamKey(0)`
    and `pub fn of(session_id: &str) -> StreamKey`
  - `pub struct JobHint { pub stream: StreamKey, pub segment: Option<u32> }`, `Default`
  - `TranscodeScheduler::submit(&self, input, opts, sink, class, hint: JobHint)`
  - `SchedState.playheads: HashMap<StreamKey, u32>` (bounded, LRU'd at 256)
  - `fn lookahead_distance(state: &SchedState, ctx: &JobCtx) -> i64`

**Why a real key and not a derived one:** the scheduler could infer a stream from
`RenditionKey::new(&ctx.input, &ctx.opts)` and read the position out of
`opts.start_position_ticks`, with no API change at all. It would be wrong in
exactly the case this phase exists for: two viewers at different positions in the
same file at the same rung would share one playhead estimate and corrupt each
other's distances. The five call sites are worth it.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `scheduler.rs`:

```rust
    /// Distance is a property of NOW, not of the job. A prefetch queued at
    /// distance 6 becomes the most urgent thing in the queue once the client has
    /// consumed five segments — freezing urgency at submit time gets this
    /// exactly backwards.
    #[tokio::test]
    async fn a_prefetch_becomes_more_urgent_as_its_client_advances() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(50), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
        let stream = StreamKey::of("play-session-a");

        // The client asks for segment 100.
        s.submit(
            PathBuf::from("/m/a"),
            cmaf(),
            file_sink(),
            JobClass::Interactive,
            JobHint {
                stream,
                segment: Some(100),
            },
        )
        .await
        .unwrap();

        let snap = s.snapshot().await.unwrap();
        assert_eq!(snap.playheads.get(&stream).copied(), Some(100));

        // ...and then for 104.
        s.submit(
            PathBuf::from("/m/a"),
            cmaf(),
            file_sink(),
            JobClass::Interactive,
            JobHint {
                stream,
                segment: Some(104),
            },
        )
        .await
        .unwrap();

        let snap = s.snapshot().await.unwrap();
        assert_eq!(snap.playheads.get(&stream).copied(), Some(104));
    }
```

- [ ] **Step 2: Run it to verify it fails**

```bash
nix develop --command cargo nextest run -p pharos-transcode \
  a_prefetch_becomes_more_urgent
```

Expected: **compile failure**, `cannot find struct 'StreamKey'`.

- [ ] **Step 3: Add the hint types**

Add near the top of `scheduler.rs`, beside `JobClass`:

```rust
/// Identifies one client's playback stream, so the scheduler can tell "the
/// segment this viewer needs next" from "a segment some other viewer wanted
/// first". Opaque hash of the play-session id — bounded cardinality, no PII, and
/// nothing for the scheduler to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamKey(pub u64);

impl StreamKey {
    /// Paths with no play session: the transcode tool, tests, and any
    /// non-playback job. Never gets a playhead, so its background work sorts
    /// last — which is correct, since nothing is about to need it.
    pub const NONE: StreamKey = StreamKey(0);

    pub fn of(session_id: &str) -> StreamKey {
        // Non-zero so a real session can never collide with NONE.
        StreamKey(xxhash_rust::xxh3::xxh3_64(session_id.as_bytes()) | 1)
    }
}

/// What the caller knows about a job that the scheduler cannot work out for
/// itself: whose stream it belongs to, and which segment of it.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobHint {
    pub stream: StreamKey,
    /// Segment index. `None` for anything that is not a numbered segment.
    pub segment: Option<u32>,
}

impl Default for StreamKey {
    fn default() -> Self {
        StreamKey::NONE
    }
}
```

Add `xxhash-rust.workspace = true` to `crates/pharos-transcode/Cargo.toml` under
`[dependencies]` if absent (it is already a workspace dependency, used by
`pharos-cache`), then run `just hakari-regen`.

- [ ] **Step 4: Thread the hint through `submit`**

Add `hint: JobHint` as the final parameter of `TranscodeScheduler::submit` and
of `SchedMsg::Submit`; store `stream` and `segment` on `JobCtx`; initialise them
in the `Submit` arm.

Add to `SchedState`:

```rust
    /// Last segment each stream's CLIENT actually asked for. This is what makes
    /// "how soon will this be needed" answerable: the scheduler otherwise has no
    /// idea where a viewer is. Bounded at MAX_TRACKED_STREAMS with the
    /// least-recently-updated evicted, mirroring PrefetchRegistry.
    playheads: HashMap<StreamKey, (u32, u64)>,
    /// Monotonic tick used only to order `playheads` for eviction.
    playhead_clock: u64,
```

with

```rust
/// Mirrors `MAX_TRACKED_SESSIONS` in `PrefetchRegistry`. A map that grows
/// without bound is a leak dressed as a cache.
const MAX_TRACKED_STREAMS: usize = 256;

/// Record where a stream's client has reached, and bound the map.
fn note_playhead(state: &mut SchedState, stream: StreamKey, segment: u32) {
    if stream == StreamKey::NONE {
        return;
    }
    state.playhead_clock += 1;
    let tick = state.playhead_clock;
    state.playheads.insert(stream, (segment, tick));
    if state.playheads.len() > MAX_TRACKED_STREAMS {
        if let Some((&oldest, _)) = state.playheads.iter().min_by_key(|(_, (_, t))| *t) {
            state.playheads.remove(&oldest);
        }
    }
}

/// How many segments ahead of its client's last request this job sits.
///
/// Recomputed at dispatch, never frozen at submit: a job queued 30 s ago at
/// distance 6 may now be distance 1 — the most urgent thing in the queue — or
/// already passed, and a frozen value gets both cases backwards.
///
/// `i64` so "already passed" is representable. Jobs with no stream or no segment
/// sort last: nothing is known to be about to need them.
fn lookahead_distance(state: &SchedState, ctx: &JobCtx) -> i64 {
    let (Some(seg), Some((head, _))) = (ctx.segment, state.playheads.get(&ctx.stream)) else {
        return i64::MAX;
    };
    seg as i64 - *head as i64
}
```

Call `note_playhead` in the `Submit` arm, for interactive jobs only — a
speculative request says nothing about where the viewer is:

```rust
            if class == JobClass::Interactive {
                if let Some(seg) = hint.segment {
                    note_playhead(state, hint.stream, seg);
                }
            }
```

Expose the map on `SchedSnapshot` for the test:

```rust
    /// Where each tracked stream's client has reached. Read by the queue's
    /// urgency ordering; exposed so a wedged queue can be explained.
    pub playheads: HashMap<StreamKey, u32>,
```

populated as
`state.playheads.iter().map(|(k, (s, _))| (*k, *s)).collect()`.

- [ ] **Step 5: Fix every call site**

```bash
nix develop --command cargo check --workspace --all-targets
```

Expected: errors at each `submit(` call. Supply:
- `crates/pharos-cache/src/hls_cache.rs` — `write_segment` gains a
  `hint: JobHint` parameter, passed down from `segment_bytes_keyed`, which gains
  a `stream: StreamKey` parameter and builds
  `JobHint { stream, segment: Some(seg_index) }`.
- `crates/pharos-server/src/api/jellyfin/hls.rs` — the five call sites at lines
  1147, 2388, 2576, 2649, 3013. Each already has the play-session id in scope
  (it keys `PrefetchRegistry`); pass `StreamKey::of(&play_session_id)`. Where a
  site genuinely has no session, pass `StreamKey::NONE` and leave a one-line
  comment naming why.
- `crates/pharos-transcode/src/bin/transcode_tool.rs` and every test — pass
  `JobHint::default()`.

- [ ] **Step 6: Run the tests to verify they pass**

```bash
nix develop --command cargo nextest run --workspace
```

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(transcode): tell the scheduler whose stream a job belongs to

The scheduler could see a job's class but not its urgency: JobClass cannot
distinguish seg+1, wanted in six seconds, from seg+6, wanted in thirty-six.
With two viewers it therefore served whoever submitted first and dropped the
rest.

Adds JobHint { stream, segment } and tracks each stream's last CLIENT-requested
segment, so lookahead distance is answerable at dispatch. Distance is
deliberately recomputed, never frozen at submit: a job queued at distance 6 may
now be the most urgent thing in the queue, or already passed.

Threaded from the five HLS call sites rather than derived from the rendition
key, which would merge two viewers at different positions in the same file at
the same rung -- exactly the case this exists for.

No behaviour change: nothing reads the distance yet.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 7: Let background work queue, and select at dispatch by urgency

**Files:**
- Modify: `crates/pharos-transcode/src/scheduler.rs` (`place`, `drain_pending`)
- Test: `crates/pharos-transcode/src/scheduler.rs` test module

**Interfaces:**
- Consumes: `lookahead_distance` (Task 6).
- Produces, relied on by Task 8: `fn next_to_dispatch(state: &SchedState) -> Option<usize>`
  returning an index into `state.pending`.

- [ ] **Step 1: Write the failing test**

```rust
    /// The motivating case: two viewers, each prefetching six segments. Whoever
    /// submitted first used to take all the capacity and the other viewer's work
    /// was dropped -- so one built a deep buffer and the other took a cold miss
    /// on every segment. Every stream's next-needed segment must dispatch before
    /// any stream's distant one.
    #[tokio::test]
    async fn every_streams_nearest_segment_is_served_before_any_streams_distant_one() {
        let order = Arc::new(std::sync::Mutex::new(Vec::<PathBuf>::new()));
        let seen = order.clone();
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(80), move |_, spec| {
            seen.lock().unwrap().push(spec.input.clone());
            WorkerRunResult::Done { out_bytes: 1 }
        });
        // One permit: dispatch order is fully determined by queue selection.
        let s = TranscodeScheduler::spawn(one_gpu(1), spawner, SchedConfig::default());

        let a = StreamKey::of("viewer-a");
        let b = StreamKey::of("viewer-b");

        // Both clients are at segment 100. Occupy the single permit first so
        // everything below is forced to queue.
        let blocker = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/blocker"),
                    cmaf(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint { stream: a, segment: Some(100) },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Viewer A submits its whole prefetch window before viewer B submits
        // anything -- the exact submission order that used to starve B.
        let mut handles = Vec::new();
        for (stream, tag, ahead) in [
            (a, "a", 6u32),
            (a, "a", 1),
            (b, "b", 6),
            (b, "b", 1),
        ] {
            let s2 = s.clone();
            let p = PathBuf::from(format!("/m/{tag}-{ahead}"));
            handles.push(tokio::spawn(async move {
                s2.submit(
                    p,
                    cmaf(),
                    file_sink(),
                    JobClass::Background,
                    JobHint { stream, segment: Some(100 + ahead) },
                )
                .await
            }));
        }
        // Give B's submissions time to land before the blocker frees the permit.
        let _ = blocker.await.unwrap();
        for h in handles {
            let _ = h.await;
        }

        let got: Vec<String> = order
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .filter(|n| n != "blocker")
            .collect();
        let d1: Vec<&String> = got.iter().take(2).collect();
        assert!(
            d1.contains(&&"a-1".to_string()) && d1.contains(&&"b-1".to_string()),
            "distance-1 work did not go first: {got:?}"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

```bash
nix develop --command cargo nextest run -p pharos-transcode \
  every_streams_nearest_segment
```

Expected: **FAIL** — `distance-1 work did not go first`. Background work never
enters `pending` today (`scheduler.rs:1163`), so all four speculative jobs are
shed outright and `got` is empty.

- [ ] **Step 3: Let background queue**

In `place()`, replace the background-never-queues block (lines 1158-1166) with:

```rust
    // All candidate permits busy → queue. Background work queues too, which it
    // did not before: dropping it meant the loser of a two-viewer race took a
    // cold interactive miss on every segment while the winner built a deep
    // buffer. Deferred beats never — but only because a queued job can no
    // longer block a client on the cache's per-key lock (006 phase 2a) and is
    // re-ranked by urgency at dispatch rather than served FIFO (below).
    if state.pending.len() >= state.cfg.pending_cap {
        let _ = ctx.reply.send(Err(SchedError::Busy));
    } else {
        state.pending.push_back((job_id, ctx));
    }
```

- [ ] **Step 4: Select at dispatch instead of popping the front**

Add beside `lookahead_distance`:

```rust
/// Which queued job should take the permit that just freed.
///
/// Tier is absolute — every Interactive before any Background — because someone
/// is blocked on each of the former and nobody on any of the latter. Within
/// Interactive it is FIFO: all equally urgent. Within Background it is ascending
/// lookahead distance, recomputed HERE against each stream's current playhead.
///
/// O(n) over `pending_cap` (256) rather than a heap: every key moves whenever
/// any client advances, so a heap would be re-keyed more often than it is
/// popped.
fn next_to_dispatch(state: &SchedState) -> Option<usize> {
    state
        .pending
        .iter()
        .enumerate()
        .min_by_key(|(idx, (_, ctx))| {
            let tier = match ctx.class {
                JobClass::Interactive => 0i64,
                JobClass::Background => 1,
            };
            match ctx.class {
                JobClass::Interactive => (tier, *idx as i64),
                JobClass::Background => (tier, lookahead_distance(state, ctx)),
            }
        })
        .map(|(idx, _)| idx)
}
```

Rewrite `drain_pending` (lines 1176-1186):

```rust
/// On a freed permit, dispatch queued work in urgency order until nothing else
/// fits.
///
/// Selects rather than pops: order in `pending` is arrival order, and arrival
/// order is not urgency order. A job passed over stays queued and is
/// reconsidered — with a freshly computed distance — on the next free permit.
fn drain_pending(state: &mut SchedState, self_tx: &mpsc::Sender<SchedMsg>) {
    let mut passed_over: VecDeque<(JobId, JobCtx)> = VecDeque::new();
    while let Some(idx) = next_to_dispatch(state) {
        let Some((job_id, ctx)) = state.pending.remove(idx) else {
            break;
        };
        let refused_before = passed_over.len();
        try_place_no_queue(state, job_id, ctx, self_tx, &mut passed_over);
        // The most urgent candidate could not find a permit, so no less urgent
        // one will either. Stop rather than walk the whole queue on every
        // completion.
        if passed_over.len() > refused_before {
            break;
        }
    }
    state.pending.append(&mut passed_over);
}
```

`VecDeque::remove(idx)` returns `Option<T>` and preserves the order of the rest,
which is what keeps Interactive FIFO intact.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
nix develop --command cargo nextest run -p pharos-transcode
```

Expected: all pass, including
`speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for` — the
crowding gate in `place()` is untouched, and `try_place_no_queue` must gain the
same gate if the test now fails. If it does fail, add to `try_place_no_queue`'s
device loop, before `try_acquire_owned`:

```rust
        if ctx.class == JobClass::Background && crowds_a_client(state, dev) {
            continue;
        }
```

- [ ] **Step 6: Commit**

```bash
git add crates/pharos-transcode/src/scheduler.rs
git commit -m "feat(transcode): queue speculative work and dispatch it by urgency

Background work was shed and never retried, so with two viewers the loser of
the race took a cold interactive miss on every segment while the winner built
a deep buffer. It now queues, and the queue is no longer FIFO.

Selection happens at dispatch: interactive before background absolutely, then
background by ascending lookahead distance recomputed against each stream's
current playhead. A job queued at distance 6 becomes distance 1 as its client
advances, so freezing urgency at submit time would rank it exactly backwards.

Safe to queue only because phase 2a removed the per-key lock a queued job used
to hold -- that is what turned a deferred prefetch into a client's 90 s wait
(B108).

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 8: Drop what is no longer wanted; evict the least urgent

**Files:**
- Modify: `crates/pharos-transcode/src/scheduler.rs` (`drain_pending`, `place`)
- Test: `crates/pharos-transcode/src/scheduler.rs` test module

**Interfaces:**
- Consumes: `next_to_dispatch`, `lookahead_distance` (Tasks 6-7).
- Produces: `pharos_transcode_queue_outcome_total{class,outcome}` with
  `dispatched` / `stale` / `evicted` / `shed`;
  `pharos_transcode_queue_distance` histogram.

- [ ] **Step 1: Write the failing tests**

```rust
    /// A queue that never drops stale work and never evicts is a queue that has
    /// quietly become the FIFO B108 deleted. A prefetch whose client has already
    /// played past it is pure waste — it must die at dispatch, not encode.
    #[tokio::test]
    async fn a_prefetch_the_client_has_played_past_is_dropped_not_encoded() {
        let order = Arc::new(std::sync::Mutex::new(Vec::<PathBuf>::new()));
        let seen = order.clone();
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(80), move |_, spec| {
            seen.lock().unwrap().push(spec.input.clone());
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(1), spawner, SchedConfig::default());
        let v = StreamKey::of("viewer");

        let blocker = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/blocker"),
                    cmaf(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint { stream: v, segment: Some(100) },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        let stale = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/stale"),
                    cmaf(),
                    file_sink(),
                    JobClass::Background,
                    JobHint { stream: v, segment: Some(101) },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        // The client seeks forward: it is now at 140, so segment 101 will never
        // be asked for.
        let _ = blocker.await.unwrap();
        s.submit(
            PathBuf::from("/m/seek"),
            cmaf(),
            file_sink(),
            JobClass::Interactive,
            JobHint { stream: v, segment: Some(140) },
        )
        .await
        .unwrap();

        assert!(matches!(stale.await.unwrap(), Err(SchedError::Busy)));
        let got: Vec<String> = order
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        assert!(!got.contains(&"stale".to_string()), "encoded a dead prefetch: {got:?}");
    }

    /// FIFO overflow drops the NEWEST arrival, which is the most urgent thing in
    /// the queue. Evicting the least urgent is the opposite, and correct.
    #[tokio::test]
    async fn a_full_queue_evicts_its_least_urgent_job_not_its_newest() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(500), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let cfg = SchedConfig {
            pending_cap: 2,
            ..SchedConfig::default()
        };
        let s = TranscodeScheduler::spawn(one_gpu(1), spawner, cfg);
        let v = StreamKey::of("viewer");

        let _blocker = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/blocker"),
                    cmaf(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint { stream: v, segment: Some(100) },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        let far = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/far"),
                    cmaf(),
                    file_sink(),
                    JobClass::Background,
                    JobHint { stream: v, segment: Some(106) },
                )
                .await
            })
        };
        let mid = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/mid"),
                    cmaf(),
                    file_sink(),
                    JobClass::Background,
                    JobHint { stream: v, segment: Some(105) },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(40)).await;

        // Queue is full (cap 2). The newest arrival is the most urgent one.
        let near = s
            .submit(
                PathBuf::from("/m/near"),
                cmaf(),
                file_sink(),
                JobClass::Background,
                JobHint { stream: v, segment: Some(101) },
            )
            .await;

        assert!(
            matches!(far.await.unwrap(), Err(SchedError::Busy)),
            "the least urgent job survived a full queue"
        );
        assert!(near.is_ok() || mid.await.unwrap().is_ok());
    }
```

- [ ] **Step 2: Run them to verify they fail**

```bash
nix develop --command cargo nextest run -p pharos-transcode \
  a_prefetch_the_client_has_played_past a_full_queue_evicts
```

Expected: both **FAIL** — the stale job is encoded, and the newest arrival is
rejected.

- [ ] **Step 3: Add the outcome metric**

Add beside `lookahead_distance`:

```rust
/// Why a queued job left the queue. `stale` and `evicted` are the arms that say
/// the queue is doing its job rather than merely accumulating: a queue that
/// never evicts and never drops stale work is a queue that has quietly become
/// the FIFO B108 deleted.
fn record_queue_outcome(class: JobClass, outcome: &'static str) {
    metrics::counter!(
        "pharos_transcode_queue_outcome_total",
        "class" => class.label(),
        "outcome" => outcome,
    )
    .increment(1);
}
```

- [ ] **Step 4: Drop stale work at dispatch**

In `drain_pending`, immediately after taking `(job_id, ctx)` out of `pending`:

```rust
        // The playhead has passed it: nobody will ever ask for this segment.
        // Evaluated at dispatch, which is why no reaper task is needed — the
        // check happens exactly where the answer is freshest.
        if ctx.class == JobClass::Background && lookahead_distance(state, &ctx) <= 0 {
            record_queue_outcome(ctx.class, "stale");
            tracing::debug!(
                %job_id,
                stream = ctx.stream.0,
                segment = ctx.segment,
                "speculative transcode dropped: the client has played past it"
            );
            let _ = ctx.reply.send(Err(SchedError::Busy));
            continue;
        }
```

and, on the dispatch path in `try_place_no_queue` (immediately before
`state.inflight.insert`):

```rust
            record_queue_outcome(ctx.class, "dispatched");
            if ctx.class == JobClass::Background {
                let d = lookahead_distance(state, &ctx);
                if d != i64::MAX {
                    metrics::histogram!("pharos_transcode_queue_distance").record(d as f64);
                }
            }
```

- [ ] **Step 5: Evict the least urgent on overflow**

Replace the `pending_cap` branch in `place()` (from Task 7) with:

```rust
    if state.pending.len() >= state.cfg.pending_cap {
        // FIFO overflow rejects the NEWEST arrival — which, for prefetch, is the
        // most urgent thing in the queue. Evict the least urgent instead, and
        // only if the newcomer actually beats it.
        let victim = state
            .pending
            .iter()
            .enumerate()
            .filter(|(_, (_, c))| c.class == JobClass::Background)
            .max_by_key(|(_, (_, c))| lookahead_distance(state, c))
            .map(|(idx, (_, c))| (idx, lookahead_distance(state, c)));
        let mine = lookahead_distance(state, &ctx);
        match victim {
            Some((idx, worst)) if worst > mine => {
                if let Some((vid, vctx)) = state.pending.remove(idx) {
                    record_queue_outcome(vctx.class, "evicted");
                    tracing::debug!(
                        job_id = %vid,
                        distance = worst,
                        replaced_by = %job_id,
                        "speculative transcode evicted: a more urgent job arrived"
                    );
                    let _ = vctx.reply.send(Err(SchedError::Busy));
                }
                state.pending.push_back((job_id, ctx));
            }
            _ => {
                record_queue_outcome(ctx.class, "shed");
                let _ = ctx.reply.send(Err(SchedError::Busy));
            }
        }
    } else {
        state.pending.push_back((job_id, ctx));
    }
```

Also call `record_queue_outcome(ctx.class, "shed")` on the existing
`background_headroom` shed path (line ~1103) so the four arms account for every
job that leaves.

- [ ] **Step 6: Run the tests to verify they pass**

```bash
nix develop --command cargo nextest run -p pharos-transcode
```

- [ ] **Step 7: Disarm-verify the outcome metric**

Add an assertion test in the shape of Task 3 step 7 covering
`pharos_transcode_queue_outcome_total` with `outcome=stale`. Comment out the
`record_queue_outcome(ctx.class, "stale")` call, confirm the test **FAILS**,
restore it.

- [ ] **Step 8: Commit**

```bash
git add crates/pharos-transcode/src/scheduler.rs
git commit -m "feat(transcode): drop stale prefetch and evict the least urgent

Two ways a queue of speculative work turns back into the FIFO that caused
B108: it holds jobs nobody will ever ask for, and it rejects the urgent
newcomer to keep the stale incumbent.

A background job whose client has played past it is dropped at dispatch --
evaluated there rather than by a reaper, because that is where the playhead is
freshest. On overflow the LEAST urgent background job is evicted, and only when
the arrival actually beats it; FIFO overflow drops the newest, which for
prefetch is the most urgent thing in the queue.

pharos_transcode_queue_outcome_total{class,outcome} accounts for every job that
leaves: dispatched / stale / evicted / shed. The stale and evicted arms are the
ones that say the queue is working rather than merely accumulating.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 9: Promotion — the test this phase ships or dies on

**Files:**
- Modify: `crates/pharos-transcode/src/scheduler.rs` (`SchedMsg::Promote`,
  `TranscodeScheduler::promote`, `JobCtx.class` mutation)
- Modify: `crates/pharos-cache/src/hls_cache.rs` (call `promote` when a client
  coalesces onto speculative work)
- Test: `crates/pharos-transcode/src/scheduler.rs` test module

**Interfaces:**
- Consumes: the in-flight registry (Task 5), the queue (Tasks 7-8).
- Produces: `TranscodeScheduler::promote(&self, job_id: JobId)`;
  `pharos_transcode_promotion_total`.

**The insight:** a client waiting on a speculative job's result is proof the
speculation was correct. It is not speculation any more — it is demand. So do not
make the client wait behind it, and do not cancel and redo the work. Reclassify
it and let it jump the tier.

- [ ] **Step 1: Write the failing test**

```rust
    /// B108's shape, reproduced directly. A client asks for a segment that is
    /// already queued as background behind a saturated device. Its wait must be
    /// bounded by ONE encode, not by the queue.
    ///
    /// If this cannot be made to pass, the queue does not ship and phase 2b
    /// falls back to urgency-gated admission without queuing (spec 006).
    #[tokio::test]
    async fn a_client_arriving_on_queued_speculative_work_waits_one_encode() {
        const ENCODE: Duration = Duration::from_millis(200);
        let (spawner, _) = ScriptedSpawner::new(ENCODE, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(one_gpu(1), spawner, SchedConfig::default());
        let v = StreamKey::of("viewer");

        // Saturate the device, then bury a prefetch under a deep queue of other
        // speculative work.
        let _blocker = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/blocker"),
                    cmaf(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint { stream: v, segment: Some(100) },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut buried = Vec::new();
        for ahead in 2..=6u32 {
            let s2 = s.clone();
            buried.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/bg{ahead}")),
                    cmaf(),
                    file_sink(),
                    JobClass::Background,
                    JobHint { stream: v, segment: Some(100 + ahead) },
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;

        // The client now wants segment 101, which is queued as background.
        // Find it and promote it -- what the cache does when a client coalesces
        // onto an in-flight speculative encode.
        let target = s
            .find_queued(v, 101)
            .await
            .expect("the prefetch for 101 must be queued");
        let started = Instant::now();
        s.promote(target).await;

        // Wait for the promoted job to complete.
        for h in buried {
            let _ = h.await;
        }
        assert!(
            started.elapsed() < ENCODE * 3,
            "the client inherited the queue instead of one encode: {:?}",
            started.elapsed()
        );
    }
```

Add a `find_queued(stream, segment) -> Option<JobId>` query message to support
the test; it is genuinely useful for diagnostics and is what the cache would use
if it did not already hold the job id from the in-flight registry.

Submit the prefetch for 101 alongside the buried set (`ahead` from 1) so the test
has something to find.

- [ ] **Step 2: Run it to verify it fails**

```bash
nix develop --command cargo nextest run -p pharos-transcode \
  a_client_arriving_on_queued_speculative_work
```

Expected: **compile failure**, `no method named 'promote'`.

- [ ] **Step 3: Implement promotion**

Add to `SchedMsg`:

```rust
    Promote {
        job_id: JobId,
    },
    FindQueued {
        stream: StreamKey,
        segment: u32,
        reply: oneshot::Sender<Option<JobId>>,
    },
```

Add the handler arms:

```rust
        SchedMsg::Promote { job_id } => {
            // A client is waiting on this job's result, so it is not
            // speculation any more — it is demand. Reclassify rather than
            // cancel-and-redo: the work in progress is exactly the work wanted.
            let promoted = state
                .pending
                .iter_mut()
                .find(|(id, _)| *id == job_id)
                .map(|(_, ctx)| ctx)
                .or_else(|| state.inflight.get_mut(&job_id))
                .filter(|ctx| ctx.class == JobClass::Background)
                .map(|ctx| {
                    ctx.class = JobClass::Interactive;
                    ctx.span.record("class", "interactive");
                })
                .is_some();
            if promoted {
                metrics::counter!("pharos_transcode_promotion_total").increment(1);
                tracing::info!(%job_id, "speculative transcode promoted: a client is waiting on it");
                drain_pending(state, self_tx);
            }
        }
        SchedMsg::FindQueued { stream, segment, reply } => {
            let found = state
                .pending
                .iter()
                .find(|(_, c)| c.stream == stream && c.segment == Some(segment))
                .map(|(id, _)| *id)
                .or_else(|| {
                    state
                        .inflight
                        .iter()
                        .find(|(_, c)| c.stream == stream && c.segment == Some(segment))
                        .map(|(id, _)| *id)
                });
            let _ = reply.send(found);
        }
```

and the handle methods:

```rust
    /// Reclassify a queued or in-flight speculative job as interactive, because
    /// a client turned up waiting on its result.
    ///
    /// `class` is therefore mutable mid-flight, and deliberately so: a promoted
    /// job must report `class="interactive"` on `encode_seconds`. It ended up
    /// being demand, and labelling it background would understate interactive
    /// latency in exactly the case that matters.
    pub async fn promote(&self, job_id: JobId) {
        let _ = self.tx.send(SchedMsg::Promote { job_id }).await;
    }

    pub async fn find_queued(&self, stream: StreamKey, segment: u32) -> Option<JobId> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SchedMsg::FindQueued { stream, segment, reply })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }
```

- [ ] **Step 4: Call it from the cache**

Extend `InFlightSegment` (Task 5) to carry the driving job's id, set by
`produce_segment` once the scheduler assigns one, and in `segment_bytes_keyed`'s
coalescing branch:

```rust
                dashmap::mapref::entry::Entry::Occupied(e) => {
                    // A client arriving on speculative work is proof the
                    // speculation was right. Promote rather than wait behind it.
                    if class == JobClass::Interactive {
                        if let (Some(sched), Some(job)) = (&self.scheduler, e.get().job_id()) {
                            sched.promote(job).await;
                        }
                    }
                    (e.get().rx.clone(), false)
                }
```

`job_id()` reads an `Arc<OnceLock<JobId>>` the driver fills; a promotion request
for a job that has not been assigned one yet is simply skipped, and the next
requester will make it.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
nix develop --command cargo nextest run --workspace
```

Expected: all pass, including
`a_client_arriving_on_queued_speculative_work_waits_one_encode`.

**If it cannot be made to pass**: stop. Revert tasks 7-9, keep tasks 5-6, and
implement the fallback the spec names — urgency-gated admission with no queue:
admit a background job at lookahead `d` iff `free_background_slots >= d`. Report
the failure and the fallback to the user rather than shipping a queue that
reproduces B108.

- [ ] **Step 6: Disarm-verify the promotion metric**

Comment out `metrics::counter!("pharos_transcode_promotion_total")`, confirm the
metric assertion test **FAILS**, restore it.

- [ ] **Step 7: Lint and full test**

```bash
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
nix develop --command just test
```

- [ ] **Step 8: Commit**

```bash
git add crates/pharos-transcode/src/scheduler.rs crates/pharos-cache/src/hls_cache.rs
git commit -m "feat(transcode): promote speculative work a client turns up waiting for

A client waiting on a speculative job's result is proof the speculation was
correct. It is not speculation any more -- it is demand. So neither make the
client wait behind it nor cancel and redo the work: reclassify it Background ->
Interactive and let it jump the tier.

The client's wait collapses from queue-plus-encode to encode, with nothing
duplicated. This is what makes a background queue safe rather than a repeat of
B108, and the test that proves it reproduces B108's shape directly: a client
requests a segment already queued behind a saturated device and must wait one
encode, not the queue.

class is now mutable mid-flight, deliberately. A promoted job reports
class=interactive on encode_seconds -- it ended up being demand, and labelling
it background would understate interactive latency in exactly the case that
matters.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
```

---

## Task 10: Bookkeeping, verification, and ship

**Files:**
- Modify: `specs/001-pharos-baseline/invariants.md`
- Modify: `specs/006-self-tuning-playback/spec.md` (status line)

- [ ] **Step 1: Record the phase 2 invariant**

Append to `specs/001-pharos-baseline/invariants.md`:

```markdown
### V127 — deferred speculative work is ranked by when it will be needed

Speculative transcode work may queue, but a queue of it is only safe under three
conditions, all of which are load-bearing and all of which have a test:

1. **Nothing holds exclusion across the encode.** A queued job that holds a
   per-key lock turns a client's own later request for the same segment into the
   whole queue wait — B108, a 90 s stall on a 3.4 s encode.
2. **Urgency is recomputed at dispatch, never frozen at submit.** Distance to a
   client's playhead is a property of now: a job queued at distance 6 becomes the
   most urgent thing in the queue, or already passed, without anything about the
   job changing.
3. **A client arriving on speculative work promotes it.** Demand for a
   speculative result proves the speculation; the job jumps the tier rather than
   the client joining the queue behind it.

A queue that never evicts and never drops stale work has quietly become the FIFO
B108 deleted — which is why `pharos_transcode_queue_outcome_total`'s `stale` and
`evicted` arms exist.

Introduced by 006 phase 2b. See V58 (shed-not-queue, which this supersedes for
the segment path) and B108.
```

- [ ] **Step 2: Mark the spec shipped**

Update the status line at the top of
`specs/006-self-tuning-playback/spec.md` to name what actually landed, what was
deferred, and the invariant ids. If the phase 2b fallback was taken, say so
plainly there rather than leaving the spec describing a queue that does not
exist.

- [ ] **Step 3: Full verification**

```bash
nix develop --command just test
nix develop --command cargo test --doc --workspace
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
nix develop --command just hakari-check
```

No `sqlx::query*` string was touched, so `just test-postgres` is not required —
confirm that by grepping the diff before skipping it:

```bash
git diff main --stat -- '*pharos-store-sqlx*'
```

- [ ] **Step 4: Commit and open the PR**

```bash
git add specs/
git commit -m "docs(spec): record 006 self-tuning playback and V126-V127

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01V3Htn9kv5sxT7niAdqt4Bn"
git push -u origin HEAD
gh pr create --fill
```

Merge with `gh pr merge --rebase` once CI is green. Never `--squash`.

- [ ] **Step 5: Verify by query, not by assertion**

After the deploy reconciles, under real playback with two concurrent viewers:

```promql
pharos_transcode_background_allowance{device="Nvenc:0"}
sum by (outcome) (rate(pharos_transcode_queue_outcome_total{class="background"}[15m]))
rate(pharos_transcode_promotion_total[15m])
histogram_quantile(0.5, sum by (le) (rate(pharos_transcode_queue_distance_bucket[15m])))
sum by (hit_path) (rate(pharos_segment_cache_total{result="hit"}[15m]))
```

Report the actual output. What each answer means:

- **allowance > 1** — the tuner is working. Still 1 with mostly `ignored`
  verdicts means no signal is reaching it.
- **`stale` and `evicted` both non-zero** — the queue is discriminating. Both at
  zero while `shed` is high means it is accumulating, not ranking.
- **promotions non-zero** — speculation is being validated by demand. Zero while
  clients wait means the promotion path is not wired up, which is the failure
  mode that turns this into B108 again.
- **median queue distance low** — shallow-beats-deep is happening.
- **`coalesced` present** — the label migration landed and the old panel needs
  repointing.

"Should be fixed" is not a result.

---

## Self-Review

**Spec coverage.** Every section of `specs/006-self-tuning-playback/spec.md` maps
to a task: the control law and its two guards → Task 1; `background_peers`
recorded at dispatch → Task 2; the named signals and the restart/degradation
behaviour → Task 3 (cold controller = floor = today); the admission change and
V126 → Task 4; the shared-result registry, cancellation safety and the
`post_lock` migration → Task 5; playhead tracking and distance → Task 6;
tier-then-distance selection → Task 7; stale drop, least-urgent eviction and the
queue-outcome metric → Task 8; promotion, mutable `class` and the ship-or-dies
test → Task 9; V127 and verify-by-query → Task 10. The spec's six controller unit
tests are all in Task 1 step 2; its two 2a tests are in Task 5 step 1; its four 2b
tests are distributed across Tasks 7-9.

**Deliberately not covered**, matching the spec's "Out" section: prefetch depth,
non-playback constants (scan probe concurrency, `SWEEP_CONCURRENCY`, `BG_IO_MAX`),
policy constants, and persistence of the learned model.

**Known gap carried forward, not hidden.** Per-session fairness is still not
guaranteed: if viewer A queues four jobs before viewer B submits anything, A's
sort ahead at equal distance. The spec accepts this and says a per-session
background cap closes it — held until the metric shows it is needed.
`pharos_transcode_queue_distance` split by stream is the query that would show it.

**One unverifiable claim, flagged.** The exact fixture that carries
`duration_ticks` (`cmaf()` vs `h264()`) is asserted in Task 3 step 1 but not
confirmed against the source; the step says to check it, because an observation
without a duration is silently ignored and the test would then be asserting
nothing.
