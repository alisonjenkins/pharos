//! `TranscodeScheduler` — the load-balancing actor.
//!
//! One `tokio` task owns all scheduling state and processes [`SchedMsg`]s
//! from a bounded mpsc inbox (the same actor shape as
//! `pharos-sync::group::GroupHandle`). The actor does **O(1) bookkeeping
//! per message and never `.await`s an encode** — every encode runs in a
//! detached task that owns an `OwnedSemaphorePermit` (RAII release) and
//! reports back via a [`SchedMsg::JobFinished`] message.
//!
//! ## Why this can't deadlock
//! - Permits are taken with `try_acquire_owned` (non-blocking). If none
//!   is free the job is queued, never awaited-on inside the actor.
//! - The permit is released by `Drop` in the detached task **before** it
//!   sends `JobFinished`, so the freed slot is visible the instant the
//!   actor drains the pending queue on that edge. No "release" message
//!   exists, so the actor can never block trying to send one.
//! - Worker *spawning* happens inside the detached task, not the actor,
//!   so a slow fork never stalls the inbox.
//! - The pending queue is bounded; when full, `Submit` replies `Busy`
//!   (backpressure) rather than blocking the inbox.
//! - Every code path resolves the caller's reply exactly once (success,
//!   error, or — if the whole actor dies — a dropped oneshot → clean
//!   `RecvError`). No path leaves a caller hung.

use crate::admission::{AdmissionConfig, AdmissionController, Observation};
use crate::device::DeviceTable;
use crate::options::TranscodeOptions;
use crate::protocol::{DeviceId, JobId, JobSpec, OutputSink, WorkerError, WorkerId};
use bytes::Bytes;
use smallvec::SmallVec;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit};

/// Who is waiting on a job.
///
/// The scheduler's pending queue is shared by client requests and speculative
/// warm-up, and until this existed the two were indistinguishable in every
/// signal the server emitted: a segment a browser was blocked on and a segment
/// nobody had asked for queued identically. "Why did this segment wait 90 s?"
/// was therefore unanswerable — the wait was visible, what it waited behind
/// was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobClass {
    /// A client response is blocked on this job completing.
    Interactive,
    /// Speculative warm-up (prefetch, seek prewarm). Nobody is waiting;
    /// arriving late is worthless and arriving never is cheap.
    Background,
}

impl JobClass {
    /// Stable metric-label string. Bounded cardinality, and a rename here
    /// breaks dashboards silently — pinned by a test.
    pub fn label(self) -> &'static str {
        match self {
            JobClass::Interactive => "interactive",
            JobClass::Background => "background",
        }
    }
}

/// What happened when a shared-init fMP4 rendition was resolved to its encoder.
///
/// Spec 003 pins such a rendition to ONE device because its segments all decode
/// under a single init (V80); `pharos_transcode_pin_total{outcome}` is how that
/// guarantee is observed rather than merely asserted.
///
/// `Followed` and `Unresolved` are recorded exactly once per job, at the point
/// it is actually DISPATCHED (`record_placement`'s call site) — not every time
/// `candidates_for` examines it. A queued pinned job is re-examined on every
/// drain pass while it waits, so recording at examination time counted
/// attempts, not jobs: the denominator inflated without bound under
/// saturation, which is exactly when this counter is read. `Invalidated` is
/// recorded immediately inside `candidates_for` instead, because it IS the
/// terminal decision there — the job fails right then rather than being
/// placed at all, so there is no later dispatch point to defer to. A
/// shared-init job that is still queued, or that reaches a terminal failure
/// for a reason unrelated to the pin (load shed, retries exhausted on an
/// excluded device), records nothing here by design: this counter answers
/// "how did the pin resolve for jobs that reached a decision", not "what
/// happened to every shared-init job ever submitted" — those other outcomes
/// are covered by `SchedError::Busy`/`Failed` and `ctx.last_error`.
///
/// `Invalidated` is the one to alert on: it means a rendition's device went
/// unavailable mid-stream and the request was FAILED rather than spilled onto
/// a second encoder, which is a visible stall for the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// The rendition resolved to an eligible device and the job was placed on
    /// it. Recorded once, at actual dispatch — see the enum docs.
    Followed,
    /// The resolved device was not eligible (cooldown / excluded), so the
    /// request failed rather than mixing encoders under one init (#114).
    /// Recorded immediately: this IS the terminal outcome, there is no later
    /// dispatch to defer to.
    Invalidated,
    /// No device could be resolved for the rendition at all, so placement fell
    /// through to the normal load-balanced path and the job was placed there.
    /// Recorded once, at actual dispatch — see the enum docs.
    Unresolved,
}

impl PinOutcome {
    /// Stable metric-label string. Same contract as [`JobClass::label`]: these
    /// appear as the `outcome` label on `pharos_transcode_pin_total`, so a
    /// rename breaks dashboards silently and a COLLISION is worse still —
    /// `invalidated` folded into `followed` would report a broken pin as a
    /// healthy one. Pinned by a test.
    pub fn label(self) -> &'static str {
        match self {
            PinOutcome::Followed => "followed",
            PinOutcome::Invalidated => "invalidated",
            PinOutcome::Unresolved => "unresolved",
        }
    }

    /// Record this outcome. One call per shared-init placement decision.
    fn record(self) {
        metrics::counter!("pharos_transcode_pin_total", "outcome" => self.label()).increment(1);
    }
}

/// How a job left the scheduler's queue.
///
/// `pending_background` says the queue is deep; it cannot say whether that
/// depth is work about to be needed or work already wasted, and those are the
/// two diagnoses that matter. These arms say which: `stale` and `evicted`
/// together are the statement that the queue DISCRIMINATES rather than merely
/// accumulates. A queue that never drops stale work and never evicts is a queue
/// that has quietly become the FIFO B108 deleted, and no other signal the
/// scheduler emits can tell those apart.
///
/// The arms PARTITION every submitted segment job: exactly one is recorded per
/// job, so their sum is submissions and any one of them over that sum is a
/// meaningful fraction. That is a contract, not a description — see
/// [`record_queue_outcome`] for what enforces it, and why a job re-examined on
/// every drain pass must not be counted per examination (the defect
/// `pharos_transcode_pin_total{outcome="followed"}` shipped with).
///
/// Six values because six is how many ways a job can leave, not because four
/// were nicer: a job whose caller went away and a job no device can encode both
/// vacate the queue, and folding either into `shed` would report the queue
/// managing load when it was doing nothing of the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOutcome {
    /// Took a device permit and started encoding. What happens to it after that
    /// belongs to `pharos_segment_produced_total` and the job's span; a retry
    /// re-dispatches the SAME job and is deliberately not counted again.
    Dispatched,
    /// Speculative work the viewer OUTRAN while it waited. Swept out of the
    /// queue on every drain rather than encoded — see [`is_stale`] for what
    /// distinguishes it from a guess aimed behind the playhead on purpose, and
    /// `reap_stale` for why the sweep is unconditional (a job that leaves by
    /// eviction is counted `evicted`, so dropping only what selection happened
    /// to examine biased this arm downward under exactly the load an operator
    /// reads it at).
    Stale,
    /// Displaced from a full queue by a more urgent arrival.
    Evicted,
    /// Refused for want of capacity: a full queue with nothing less urgent to
    /// displace, or a candidate pool that cannot hold the speculative reserve
    /// and a job at once. Deliberate load management, and the only one of the
    /// six that means the system is working as configured under pressure.
    Shed,
    /// The caller was gone before the job could run — a seek or a track swap
    /// dropped the `submit()` future. Not load, not waste the scheduler chose:
    /// work that stopped existing.
    Abandoned,
    /// No device could take it: an unsupported target, a pinned rendition whose
    /// device went into cooldown (V80), or every candidate excluded by an
    /// earlier transient failure.
    Failed,
}

impl QueueOutcome {
    /// Stable metric-label string. Same contract as [`JobClass::label`] and
    /// [`PinOutcome::label`]: these are the `outcome` label on
    /// `pharos_transcode_queue_outcome_total`, a rename breaks dashboards
    /// silently, and a COLLISION is worse — `stale` folded into `dispatched`
    /// reports a queue throwing work away as a queue doing work. Pinned by a
    /// test.
    pub fn label(self) -> &'static str {
        match self {
            QueueOutcome::Dispatched => "dispatched",
            QueueOutcome::Stale => "stale",
            QueueOutcome::Evicted => "evicted",
            QueueOutcome::Shed => "shed",
            QueueOutcome::Abandoned => "abandoned",
            QueueOutcome::Failed => "failed",
        }
    }

    /// Record this outcome under the job's class AS IT IS NOW.
    ///
    /// `class` is not fixed for a job's lifetime: [`TranscodeScheduler::promote`]
    /// changes it in place when a client turns out to be blocked on somebody
    /// else's guess, and the outcome is recorded at the exit, afterwards. So
    /// `sum(pharos_transcode_queue_outcome_total{class="background"})` is
    /// background SUBMISSIONS MINUS PROMOTIONS, not background submissions, and
    /// the difference is `pharos_transcode_promotion_total{outcome=~"queued|
    /// inflight"}` — read the two together or the speculative denominator is
    /// short by every guess a viewer actually joined.
    ///
    /// `pharos_transcode_queue_distance` inherits the same skew and has no
    /// label to expose it: a promoted job is `Interactive` by the time
    /// `record_dispatch` runs, so it contributes no sample, and the histogram
    /// therefore describes the guesses NOBODY joined. That is the right shape
    /// for "is prefetch depth tuned too far ahead" — a joined guess was, by
    /// definition, not too far ahead — but it is a survivorship-filtered
    /// distribution and reading it as "how deep speculation runs" overstates
    /// the depth.
    ///
    /// Both are stable, bounded label sets either way: two classes, six
    /// outcomes.
    fn record(self, class: JobClass) {
        metrics::counter!(
            "pharos_transcode_queue_outcome_total",
            "class" => class.label(),
            "outcome" => self.label(),
        )
        .increment(1);
    }
}

impl std::fmt::Display for JobClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// What happened to one [`TranscodeScheduler::promote`] request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// The job was waiting for a permit and is now ranked as a client's.
    Queued,
    /// The job was already encoding; its class changed so it stops counting
    /// against the speculative allowance on its device.
    Inflight,
    /// The job was already Interactive — a second requester arriving behind the
    /// first, or a client joining a client.
    AlreadyClient,
    /// No such job: it finished (the joiner will get the published bytes
    /// anyway) or the id names a submission the actor has already resolved.
    Unknown,
    /// A requester wanted to promote a driver that had not reached the
    /// scheduler yet, and gave up waiting for it to. Distinct from `Unknown`
    /// because it means the promotion never had a target, not that it missed
    /// one — a non-zero rate here is the window in which a client can still
    /// inherit a speculative tier.
    Unassigned,
}

impl PromotionOutcome {
    /// Stable metric-label string, same contract as [`PinOutcome::label`]:
    /// these are the `outcome` label on `pharos_transcode_promotion_total`, so
    /// a rename breaks dashboards silently and a collision would hide the case
    /// that matters. Pinned by a test.
    pub fn label(self) -> &'static str {
        match self {
            PromotionOutcome::Queued => "queued",
            PromotionOutcome::Inflight => "inflight",
            PromotionOutcome::AlreadyClient => "already_client",
            PromotionOutcome::Unknown => "unknown",
            PromotionOutcome::Unassigned => "unassigned",
        }
    }

    /// Record this outcome. One call per promotion request, including the ones
    /// that changed nothing: a promotion path that only counts its successes
    /// cannot distinguish "no client ever joined speculative work" from "every
    /// client that did arrived too late to name it".
    pub fn record(self) {
        metrics::counter!("pharos_transcode_promotion_total", "outcome" => self.label())
            .increment(1);
    }
}

/// Where the scheduler publishes the id it assigns to a submission.
///
/// A caller cannot otherwise name a job before it finishes, and naming it is
/// the whole point: an interactive request that coalesces onto a speculative
/// encode already in progress must be able to say "this one is a client's now".
///
/// A `watch` channel rather than a `OnceLock` for two reasons. A joiner can
/// arrive before the driver has reached the scheduler at all — resolving audio
/// alone can take a while — so it needs to WAIT for the id rather than miss it;
/// and a job that is retried submits again under a new id, so the slot must
/// carry the current one, not the first.
///
/// The sender lives with the submitting task. When that task ends without ever
/// submitting, every waiter wakes with an error instead of hanging.
#[derive(Clone, Debug)]
pub struct JobSlot {
    tx: Arc<tokio::sync::watch::Sender<Option<JobId>>>,
    /// See [`JobSlot::is_dispatched`]. A plain flag rather than a second
    /// `watch`: nobody needs to be WOKEN by dispatch, only to ask about it at a
    /// moment of their own choosing.
    dispatched: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for JobSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl JobSlot {
    pub fn new() -> Self {
        Self {
            tx: Arc::new(tokio::sync::watch::channel(None).0),
            dispatched: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Watch this slot. Held by anyone who wants to promote the job; keeps no
    /// sender alive, so it cannot keep a dead driver's slot open.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<JobId>> {
        self.tx.subscribe()
    }

    /// Has this slot's job been handed to a worker?
    ///
    /// Once it has, the caller CANNOT get the capacity back by walking away.
    /// `spawn_run_task` is a detached task owning both the worker and the device
    /// permit, and the `JobFinished` arm ignores a failed reply send, so a
    /// dispatched job runs to completion however the caller feels about it.
    /// `reply.is_closed()` — the only thing that stops a job — is read at
    /// `place`, `reap_abandoned` and `try_place_no_queue`, every one of which is
    /// PRE-dispatch.
    ///
    /// So this is the line between the population an abandonment can reclaim
    /// (queued, and not yet examined) and the population it cannot (running).
    /// A caller that abandons past this line spends the encode anyway and throws
    /// the bytes away, and — because the worker keeps writing to a
    /// deterministic, key-derived output path — leaves an orphan writing where
    /// its own successor is about to write. Callers whose abandonment has that
    /// shape ask here first.
    ///
    /// LATCHED, never cleared. A job re-placed after a transient device failure
    /// has already had a worker touch its output, so "was this ever running?" is
    /// the question that keeps the answer safe, not "is it running now?".
    pub fn is_dispatched(&self) -> bool {
        self.dispatched.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Latch [`Self::is_dispatched`]. The scheduler's to call, at the one place
    /// a job takes a device permit.
    pub fn mark_dispatched(&self) {
        self.dispatched
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn assign(&self, id: JobId) {
        let _ = self.tx.send(Some(id));
    }
}

/// Block until this slot names a job, or until nobody can ever name it.
///
/// Free function rather than a method: the waiter must hold only a receiver
/// (see [`JobSlot`]), and a method would require holding the slot itself and
/// with it the sender that is supposed to die.
pub async fn await_job_id(rx: &mut tokio::sync::watch::Receiver<Option<JobId>>) -> Option<JobId> {
    loop {
        if let Some(id) = *rx.borrow_and_update() {
            return Some(id);
        }
        if rx.changed().await.is_err() {
            return None;
        }
    }
}

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

impl Default for StreamKey {
    fn default() -> Self {
        StreamKey::NONE
    }
}

/// May this submission state where its stream's playback BEGINS?
///
/// The scheduler learns a stream's position from the client's own requests, and
/// a speculative submission must never move that reading — the lookahead
/// distance a guess is ranked by would then be measuring itself. A stream with
/// no reading at all is the one exception, because seeding cannot overwrite
/// anything a client established. But "the map has no entry" is not the same
/// fact as "the caller knows where playback starts", and only the second
/// justifies seeding.
///
/// They come apart because a submission is not the only thing that can leave the
/// map empty. It used to be the whole story — the map was MISS-driven, so a
/// re-watch, a second viewer on the same media, any session whose opening
/// segments were already on disk reached an ordinary deep prefetch with no entry
/// at all. [`TranscodeScheduler::note_playhead`] (T109) closes that particular
/// hole by reporting the hits too, so those sessions now DO have a reading. What
/// it does not close is the gap the rule is actually about: a stream can still
/// be entryless at its very first request — nothing has been served yet, hit or
/// miss — and letting an ordinary guess seed THERE would have it measuring
/// itself, in bounded form. So the entitlement stays on the job rather than
/// inferred from the map, and only the two prewarm call sites — which pick their
/// base from the resume position or the seek target and therefore genuinely know
/// it — set it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlayheadSeed {
    /// This submission is a guess relative to a position somebody else
    /// established. It never writes the playhead map.
    #[default]
    Observes,
    /// This caller knows where this stream's playback starts, and may say so if
    /// nothing else has. Still never OVERWRITES an existing reading.
    StatesTheStart,
}

impl PlayheadSeed {
    fn may_seed(self) -> bool {
        matches!(self, PlayheadSeed::StatesTheStart)
    }
}

/// What the caller knows about a job that the scheduler cannot work out for
/// itself: whose stream it belongs to, which segment of it, and whether the
/// caller is entitled to say where that stream begins.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobHint {
    pub stream: StreamKey,
    /// Segment index. `None` for anything that is not a numbered segment.
    pub segment: Option<u32>,
    /// See [`PlayheadSeed`]. Defaults to `Observes`, so a caller has to opt in
    /// to seeding rather than fall into it.
    pub seeds_playhead: PlayheadSeed,
}

impl JobHint {
    /// The same job, submitted on a JOINER's behalf rather than on the original
    /// requester's — so it speaks for no stream at all.
    ///
    /// A speculative driver that acquires a client re-submits at the client's
    /// tier, because a retry is a NEW job and a joiner promotes only once
    /// (`InFlightSegment::promoted`). That submission is `Interactive`, and
    /// `note_playhead` moves a stream's playhead on every interactive
    /// submission — but the client that made it interactive is on a stream the
    /// segment cache cannot name, while the stream this hint DOES name belongs
    /// to the speculative driver, whose own viewer never asked for this
    /// segment.
    ///
    /// Left alone, viewer A prefetching segment 106 while standing at 100 has
    /// its OWN playhead jumped to 106 the moment anybody joins that guess: A's
    /// queued prefetch for 101–105 is instantly stale, is reaped as such, and A
    /// takes five cold misses before its next request drags the playhead back.
    /// [`promote_job`] refuses to move the playhead for precisely this reason
    /// ("a joiner is waiting on that segment, not standing on it"); a promoted
    /// RE-submission is the same act reaching the actor by another route, and
    /// has to obey the same rule.
    ///
    /// Dropping the stream is the whole answer rather than half of one. A
    /// numbered segment with no stream ranks `i64::MAX`, which is read only
    /// within the Background tier — and this job is Interactive: ranked FIFO
    /// among clients, never a staleness candidate, never an eviction victim. The
    /// shared-init device pin (V80) keys on `(input, opts)`, not on the stream,
    /// so a pinned CMAF rendition still lands where its siblings did.
    pub fn on_behalf_of_a_joiner(self) -> Self {
        Self {
            stream: StreamKey::NONE,
            seeds_playhead: PlayheadSeed::Observes,
            ..self
        }
    }
}

/// A live transcode output as a stream of muxed byte chunks. Boxed so the
/// type stays platform-agnostic (the concrete unix worker stream lives in
/// `worker::proc`). The stream owns the worker process + its device
/// permit; dropping it tears the encode down (broken pipe → ffmpeg exits)
/// and frees the slot.
pub type LiveByteStream = Pin<Box<dyn futures_core::Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Terminal result of one worker running one job.
#[derive(Debug)]
pub enum WorkerRunResult {
    Done {
        out_bytes: u64,
    },
    Failed(WorkerError),
    /// The worker process vanished mid-job (segfault / closed pipe /
    /// heartbeat timeout). The `Box<dyn Worker>` is unusable and dropped.
    Died,
}

/// Boxed future a [`Worker::run`] call returns.
pub type RunFuture<'a> = Pin<Box<dyn Future<Output = WorkerRunResult> + Send + 'a>>;

/// Boxed future a [`WorkerSpawner::spawn`] call returns.
pub type SpawnFuture = Pin<Box<dyn Future<Output = std::io::Result<Box<dyn Worker>>> + Send>>;

/// A reusable worker bound to one job at a time. The implementation is
/// responsible for its own liveness watchdog — `run` must eventually
/// resolve (returning `Died` on a hung/dead worker), never hang, so the
/// scheduler's detached task can't leak.
pub trait Worker: Send {
    fn id(&self) -> WorkerId;
    fn run<'a>(&'a mut self, job: JobSpec) -> RunFuture<'a>;
}

/// Boxed future a [`WorkerSpawner::spawn_streaming`] call returns.
pub type StreamFuture = Pin<Box<dyn Future<Output = std::io::Result<LiveByteStream>> + Send>>;

/// Spawns fresh workers on demand (process fork for the real backend; an
/// in-process stub for tests). Injectable so the scheduler core is
/// testable with zero ffmpeg.
pub trait WorkerSpawner: Send + Sync + 'static {
    fn spawn(&self, id: WorkerId) -> SpawnFuture;

    /// Spawn a one-shot streaming worker for the live path: it encodes
    /// `spec` (sink = `Stdout`) and streams the muxed bytes back. The
    /// default errors so spawners that don't support streaming (e.g. the
    /// in-process test mock) cleanly decline; `ProcSpawner` overrides it.
    fn spawn_streaming(&self, _spec: JobSpec) -> StreamFuture {
        Box::pin(async {
            Err(std::io::Error::other(
                "this spawner does not support live streaming",
            ))
        })
    }
}

/// Where the caller wants output to land.
#[derive(Debug, Clone)]
pub enum SinkRequest {
    /// Worker writes the encoded output straight to `out_path` (caller
    /// owns any subsequent atomic rename). No cross-process byte copy.
    FileDirect { out_path: PathBuf },
    /// Live HTTP path — streamed back via a pipe. Wired in the
    /// fd-passing step; rejected with `Unsupported` until then.
    LiveStream,
}

impl SinkRequest {
    /// Bounded label for the `sink` span field. Stable strings: a dashboard
    /// keyed on these breaks silently if renamed.
    pub fn label(&self) -> &'static str {
        match self {
            Self::FileDirect { .. } => "file",
            Self::LiveStream => "live_stream",
        }
    }

    /// Whether this sink's byte count is something the worker can actually
    /// measure. A live stream's bytes go straight down the pipe to the main
    /// process, so the worker reports `0` — which is indistinguishable from
    /// "produced nothing" in a log, and read as exactly that during the Ghost
    /// in the Shell investigation (two jobs looked like silent empty encodes;
    /// they were healthy live streams).
    pub fn measures_out_bytes(&self) -> bool {
        matches!(self, Self::FileDirect { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDone {
    pub device: DeviceId,
    pub out_bytes: u64,
    /// Time the job spent queued before its (final) dispatch — includes any
    /// failed-device retry churn. High = saturated devices / retries.
    pub queue_wait_ms: u64,
    /// Time the winning device took to actually encode. High = slow encoder.
    pub encode_ms: u64,
    /// How many OTHER jobs were already running on the same device when this
    /// one was dispatched. `encode_ms` says a segment was slow; this says
    /// whether it was slow or merely crowded, which is the difference between
    /// "the encoder is too slow for this source" and "we scheduled six
    /// speculative encodes on top of it". Measured on the deployment: 1 860 ms
    /// alone, 6 229 ms with six peers.
    pub peer_jobs: usize,
    /// Of `peer_jobs`, how many were speculative. `peer_jobs` says a segment was
    /// crowded; this says whether it was crowded by work somebody was waiting
    /// for or by work nobody asked for — the difference between genuine
    /// overload and a scheduling defect, and the input the admission controller
    /// backs off on.
    pub background_peers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedError {
    /// Pending queue full — caller should retry later (backpressure).
    Busy,
    /// No device can encode this job's target.
    Unsupported,
    /// Job failed non-recoverably (or exhausted retries). Carries the
    /// last worker error for the log / caller.
    Failed(WorkerError),
    /// Scheduler channel issue (actor gone, reply dropped).
    Io(String),
}

impl std::fmt::Display for SchedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedError::Busy => write!(f, "transcode scheduler busy"),
            SchedError::Unsupported => write!(f, "no device can encode this target"),
            SchedError::Failed(e) => write!(f, "transcode failed: {e}"),
            SchedError::Io(s) => write!(f, "scheduler io: {s}"),
        }
    }
}

impl std::error::Error for SchedError {}

/// Per-device + queue snapshot for the test tool / metrics.
#[derive(Debug, Clone)]
pub struct SchedSnapshot {
    pub devices: Vec<DeviceStat>,
    pub pending: usize,
    pub idle_workers: usize,
    /// Queue depth split by who is waiting. A backlog that is almost all
    /// `background` is a queue of work nobody asked for, sitting in front of
    /// the segments a client is blocked on — the shape that turns a 3 s encode
    /// into a 90 s wait.
    pub pending_interactive: usize,
    pub pending_background: usize,
    /// How long the oldest queued job has been waiting. `pending` says the
    /// queue is deep; this says whether anything is actually stuck in it.
    pub oldest_pending_ms: Option<u64>,
    /// Jobs holding a device permit via the segment path (they report a
    /// `JobFinished`).
    pub inflight: usize,
    /// Streams holding a device permit via [`TranscodeScheduler::submit_live`].
    /// These never enter `inflight` and never report a `JobFinished`, so a
    /// permit held for the lifetime of a progressive transcode was previously
    /// invisible: `in_use` was occupied with nothing to attribute it to.
    pub live_streams: usize,
    /// The learned speculative allowance per device, unrounded. This IS the
    /// tuner: a value that never leaves the floor under playback means the loop
    /// is not working, which is indistinguishable from a correctly cautious loop
    /// unless the value itself is visible.
    pub background_allowance: Vec<(DeviceId, f64)>,
    /// Where each tracked stream's client has reached. Read by the queue's
    /// urgency ordering; exposed so a wedged queue can be explained.
    pub playheads: HashMap<StreamKey, u32>,
}

#[derive(Debug, Clone)]
pub struct DeviceStat {
    pub id: DeviceId,
    pub capacity: usize,
    pub in_use: usize,
    pub in_cooldown: bool,
    /// Occupancy attributed to segment jobs, split by class. `in_use` minus
    /// these (minus any live stream on this device) is unattributed occupancy.
    pub inflight_interactive: usize,
    pub inflight_background: usize,
}

/// Tunables.
#[derive(Debug, Clone)]
pub struct SchedConfig {
    pub inbox_depth: usize,
    pub pending_cap: usize,
    pub cooldown: Duration,
    pub max_retries: u8,
    /// Permits kept out of reach of [`JobClass::Background`] work. A
    /// speculative job is admitted only while more than this many permits are
    /// free across the devices that could take it, so a burst of prefetch can
    /// never occupy the last slot a client request would have used.
    pub background_headroom: usize,
    /// How the per-device speculative allowance is learned.
    ///
    /// `floor` here is what `background_alongside_client` used to be: a
    /// concurrency constant calibrated by hand on one GTX 1070 against one
    /// 23 Mbps HEVC source, now demoted to the value a device sits on before it
    /// has learned anything and the value it collapses back to under sustained
    /// deadline misses. That field is GONE rather than kept beside this one —
    /// it had become a `usize` copy of `admission.floor` read by nothing except
    /// a test asserting the code against it, which is a guard that cannot fail.
    ///
    /// The floor is not zero. Refusing ALL prefetch while a client job runs
    /// would starve the pipeline that makes the next segment a 30 ms cache hit.
    /// That was true when refused prefetch was dropped outright, and it stayed
    /// true when it began to wait for a permit instead (V58): a job refused on
    /// every device it could use is a job that is never selected, so a zero
    /// allowance starves it just as completely — it merely starves it in the
    /// queue rather than at the door.
    pub admission: AdmissionConfig,
    /// Master switch for 006 phase 2b: may speculative work WAIT for a permit?
    ///
    /// `true` — the shipped behaviour — lets a `Background` job that cannot be
    /// dispatched enter `pending` and be reconsidered, by urgency, on every
    /// freed permit (V58). `false` restores the rule that preceded it: refused
    /// speculation is SHED (`SchedError::Busy`) at the door and never queued.
    ///
    /// It exists because phase 2b deliberately reverses an invariant that was
    /// written in response to a production outage (B108), on a server people
    /// actually watch. `pending_cap = 0` is not the same lever and must not be
    /// used as one: it sheds INTERACTIVE jobs too, so a client segment comes
    /// back as a 500 rather than as a cold miss. This turns the queue off for
    /// speculation ALONE — the interactive queue, the admission controller
    /// (V125/V126) and the shared-result registry are untouched — so a
    /// regression found in production costs a config flip, not a revert of
    /// thirty-odd commits.
    ///
    /// One consequence is worth stating rather than discovering. `false` also
    /// applies to the RETRY path: a `Background` job whose attempt hit a
    /// TRANSIENT device failure is re-placed with that device excluded, and if
    /// no other candidate has a free permit it lands in `queue_or_refuse` like
    /// any other refusal — so it is shed instead of waiting for the permit that
    /// would have carried it. That is the pre-2b rule working exactly as it did
    /// before speculation could queue at all, not a regression; but the caller
    /// sees `SchedError::Busy`, and the `last_error` that
    /// `background_never_error` is careful to preserve on the other refusal path
    /// is dropped with it, so a device flapping under this switch reads as load
    /// shedding rather than as the hardware fault it is.
    pub queue_background: bool,
}

impl Default for SchedConfig {
    fn default() -> Self {
        Self {
            inbox_depth: 256,
            pending_cap: 256,
            cooldown: Duration::from_secs(2),
            max_retries: 3,
            background_headroom: 1,
            admission: AdmissionConfig::default(),
            queue_background: true,
        }
    }
}

/// Caller-facing handle. Clone freely; all clones feed the one actor.
#[derive(Clone)]
pub struct TranscodeScheduler {
    tx: mpsc::Sender<SchedMsg>,
}

enum SchedMsg {
    Submit {
        input: PathBuf,
        opts: TranscodeOptions,
        sink: SinkRequest,
        class: JobClass,
        hint: JobHint,
        /// Filled with the id the actor assigns, the moment it assigns it, for
        /// callers that need to name this job while it is still running.
        assigned: Option<JobSlot>,
        reply: oneshot::Sender<Result<JobDone, SchedError>>,
    },
    /// Somebody a client is blocked on turned out to be speculative work.
    Promote { job_id: JobId },
    /// A client was served a segment the scheduler never saw a job for.
    ///
    /// It cannot ride on [`SchedMsg::Submit`] — there is no job — and it cannot
    /// be inferred in here, because a cache hit returns from
    /// `segment_bytes_keyed` without the scheduler hearing anything at all. So
    /// the reading arrives as its own message or it does not arrive: see
    /// [`TranscodeScheduler::note_playhead`] for why that message is worth a
    /// send per served segment, and [`PlayheadMotion`] for the one way it is
    /// allowed to move the map.
    Playhead { stream: StreamKey, segment: u32 },
    SubmitLive {
        input: PathBuf,
        opts: TranscodeOptions,
        reply: oneshot::Sender<Result<LiveByteStream, SchedError>>,
    },
    JobFinished {
        job_id: JobId,
        device: DeviceId,
        result: WorkerRunResult,
        /// Worker returned for reuse, or `None` if it died.
        worker: Option<Box<dyn Worker>>,
    },
    Snapshot {
        reply: oneshot::Sender<SchedSnapshot>,
    },
}

/// Retry context the actor keeps for an in-flight or queued job. Holds
/// the caller's reply until a terminal outcome resolves it.
struct JobCtx {
    input: PathBuf,
    opts: TranscodeOptions,
    sink: SinkRequest,
    reply: oneshot::Sender<Result<JobDone, SchedError>>,
    /// Devices already tried + failed transiently — excluded from retry.
    excluded: SmallVec<[DeviceId; 4]>,
    retries: u8,
    last_error: Option<WorkerError>,
    /// When the job entered the scheduler (first `Submit`). Used to split
    /// end-to-end latency into queue-wait vs actual encode: the segment-path
    /// `transcode_ms` conflates the two, hiding whether a slow segment is a
    /// saturated device (long queue) or a slow encoder.
    enqueued: Instant,
    /// When the job most recently grabbed a device permit + started running.
    /// `None` while queued. Re-stamped on each (re)dispatch so a retry's wait
    /// is counted in the queue, not the encode.
    dispatched: Option<Instant>,
    /// The submitter's handle on this job, if it kept one. Latched at dispatch
    /// so the submitter can tell the capacity it can still get back from the
    /// capacity it cannot — see [`JobSlot::is_dispatched`]. `None` for callers
    /// that never asked to name their job (the transcode tool, tests).
    assigned: Option<JobSlot>,
    /// Who is waiting. Carried through retries + requeues so a job's class is
    /// the same wherever it is observed (queued, inflight, finished).
    class: JobClass,
    /// When a client turned out to be waiting on this job — the instant
    /// [`promote_job`] reclassified it. `None` for a job that was somebody's
    /// own request from the start.
    ///
    /// It exists because the admission controller judges an interactive
    /// completion against a DEADLINE, and a promoted job's encode began before
    /// any client existed. Measured from `dispatched`, a prefetch started at T0
    /// and joined at T0+4 s reports a 5 s encode against a 3 s deadline when the
    /// client waited one second: the loop then halves the allowance for a case
    /// where the design worked exactly as intended. Worse, the bias is
    /// self-reinforcing in the wrong direction — the deeper the buffer a high
    /// allowance buys, the earlier prefetch runs relative to the join, the
    /// larger the over-attribution.
    ///
    /// Only the CONTROLLER's window moves; `encode_ms` on the log line and on
    /// `JobDone` still measures from dispatch, because that genuinely is how
    /// long the encode took.
    promoted_at: Option<Instant>,
    /// Which client stream this job belongs to, so its urgency can be judged
    /// against that stream's playhead. `StreamKey::NONE` for jobs with no
    /// known session (the transcode tool, tests, non-playback work).
    ///
    /// Read by `lookahead_distance`, which `next_to_dispatch` consults on every
    /// freed permit.
    stream: StreamKey,
    /// Segment index within `stream`. `None` for anything that is not a
    /// numbered segment (e.g. a whole-file/live job), which sorts last: nothing
    /// is known to be about to need it.
    segment: Option<u32>,
    /// Where this job's stream's playhead STOOD when the job was admitted.
    /// `None` when the scheduler had no reading for that stream at all.
    ///
    /// This is what separates "the viewer outran this guess" from "this guess
    /// was aimed behind the viewer on purpose" — two things a negative
    /// [`lookahead_distance`] cannot tell apart on its own, and the second of
    /// which is a shipped feature (the SyncPlay seek prewarm submits for the
    /// seek TARGET on a member's existing stream, before that member's own
    /// interactive request moves the playhead there; a backward group seek
    /// makes every one of those jobs negative by construction). See
    /// [`is_stale`].
    ///
    /// The VALUE, not the tick of the monotonic clock that orders the playhead
    /// map. A tick answers "did the playhead move since?", which is a proxy for
    /// the question and not the question: `note_playhead` bumps the clock on
    /// EVERY interactive submission, so a member still playing forward while a
    /// backward prewarm sits queued for its stream — which is the ordinary
    /// shape, since the prewarm fires seconds before any client applies the
    /// seek — moved the tick past the prewarm and condemned it. The value is
    /// the fact the doc claims: what the submitter was looking at.
    ///
    /// Frozen at admission, unlike the distance, and correctly so: the question
    /// it answers is about the past ("what did we know when this was
    /// submitted?"), not about now.
    playhead_at_submit: Option<u32>,
    /// Device this job is currently running on. `None` while queued. Lets a
    /// snapshot attribute each device's occupancy to the jobs holding it,
    /// instead of reporting a bare count with nothing behind it.
    device: Option<DeviceId>,
    /// Jobs already on that device at the moment this one was dispatched.
    /// Re-stamped on each (re)dispatch, like `dispatched`, so a retry reports
    /// the company it actually kept rather than the company it first met.
    peer_jobs: usize,
    /// Speculative subset of `peer_jobs`, stamped at the same moment.
    background_peers: usize,
    /// The job's own span, opened at dispatch (when the device — and so the
    /// decode path — is finally known) and carried here so the completion
    /// arm, which runs in the ACTOR and not inside the job's task, can still
    /// record the outcome on it. Every event the job emits, and every trace
    /// exported to Tempo, carries the placement facts as a result: a wedged
    /// segment can be read without joining three log lines by job id.
    span: tracing::Span,
}

struct SchedState {
    devices: DeviceTable,
    spawner: Arc<dyn WorkerSpawner>,
    idle: Vec<Box<dyn Worker>>,
    inflight: HashMap<JobId, JobCtx>,
    pending: VecDeque<(JobId, JobCtx)>,
    cfg: SchedConfig,
    /// Learned, per device, from every finished interactive segment. In memory:
    /// relearned each boot, which costs one viewer the head start and never
    /// misleads the admission rule after a hardware change.
    admission: AdmissionController,
    /// Live streams currently holding a permit. Shared with each
    /// [`PermitStream`], which decrements on drop — the only bookkeeping the
    /// live path has, since it reports no `JobFinished`.
    live: Arc<AtomicUsize>,
    next_job: u64,
    next_worker: u64,
    /// Last segment each stream's CLIENT actually asked for. This is what makes
    /// "how soon will this be needed" answerable: the scheduler otherwise has no
    /// idea where a viewer is. Bounded at MAX_TRACKED_STREAMS with the
    /// least-recently-updated evicted, mirroring PrefetchRegistry.
    ///
    /// Moved by an interactive submission, or by a `Playhead` message reporting
    /// a segment the cache served without the scheduler ever seeing a job for it
    /// — the second because a viewer whose buffer is warm takes hits and nothing
    /// else, so a reading only a MISS could move went stale exactly when
    /// playback was going well. A hit may only ADVANCE it; see
    /// [`PlayheadMotion`]. A speculative submission that
    /// carries [`PlayheadSeed::StatesTheStart`] may SEED an entry that does not
    /// exist yet — see the `Submit` arm — which is where a cold-start prewarm's
    /// segments stop being unknowable; it can never overwrite a reading a
    /// client established, and no ordinary prefetch can seed at all.
    playheads: HashMap<StreamKey, (u32, u64)>,
    /// Monotonic tick used only to order `playheads` for eviction.
    playhead_clock: u64,
}

/// Mirrors `MAX_TRACKED_SESSIONS` in `PrefetchRegistry`. A map that grows
/// without bound is a leak dressed as a cache.
const MAX_TRACKED_STREAMS: usize = 256;

/// Which way a new reading is allowed to move an existing one.
///
/// The two writers of the playhead map carry different evidence, and collapsing
/// them onto one rule gets one of them wrong.
///
/// A SUBMISSION is the client's request itself, arriving in the actor in the
/// order the client made it. Wherever it points is where the viewer is, so it
/// may move the reading in either direction — which is how a BACKWARD SEEK is
/// expressed, and the only way it can be.
///
/// A HIT is weaker in exactly one respect: it is evidence the viewer REACHED
/// that segment, and it arrives when the bytes were read rather than when the
/// request was made. Two hits, or a hit and a miss, can therefore land out of
/// order — a viewer's parallel fetches around a seek routinely do — and an
/// unconditional write would let the later-landing, lower-numbered one declare
/// the viewer to be further back than the scheduler already knows they are.
/// Every guess on that stream is then ranked further out than it really is, and
/// the stream's next genuine miss queues behind another viewer's shallower work.
///
/// So a hit may only ADVANCE. That is one rule with no ordering metadata on the
/// message and no per-stream sequence in the map, and it is never worse than
/// the behaviour it replaces, in which a hit moved nothing at all.
///
/// What it does NOT capture is a backward seek that lands on a segment still in
/// the cache — a rewind inside material the viewer just watched, which is the
/// common shape of a rewind and is a HIT, not a miss. The reading then stays
/// where the viewer was rather than following them back. That is a pre-existing
/// staleness, not one introduced here: today the reading stays at the last MISS,
/// which is just as wrong and in the same direction. Fixing it needs the message
/// to carry the instant its REQUEST arrived and `Submit` to carry one too, so
/// the map can be written in request order rather than in completion order; that
/// is a strictly larger change ([`JobHint`] gains a field at every construction
/// site) and it is not what T109 is about.
///
/// That gap has two further consequences on this stream while the rewind
/// holds. Both are benign; neither is a reason to close the gap early.
///
/// First, a WIDER staleness-immunity grant than a miss-driven reading ever
/// gave: with the reading stuck at the pre-rewind position, every later
/// `Background` submission on the stream reads as
/// `submitted_behind_a_known_playhead` and is therefore immune to
/// `reap_stale` and never chosen by eviction (magnitude 0 is never the
/// `max_by_key` winner). Not a clog — those jobs are exactly what the viewer
/// wants next, so they dispatch first and leave immediately — but it is a
/// broader immunity grant than the old reading produced, for as long as the
/// stale-high reading stands.
///
/// Second, one case is genuinely WORSE than before, not merely differently
/// ranked: the forward prefetch queued just ahead of where the rewind lands
/// (segments 141-146, say) now ranks by `abs` against the stale-high reading
/// at magnitude 1-6, instead of the ~21-26 a lagging, miss-driven reading
/// used to give it. Ranking closer to the front of the queue means the
/// scheduler dispatches that work — segments the viewer just abandoned by
/// rewinding — SOONER than it did before this reading existed. It is a
/// sub-second misallocation of one permit, self-healing the moment the
/// viewer's genuine position outruns the cached region and a real miss
/// corrects the reading, but it IS a worse outcome in this one shape, not a
/// no-op: "no consumer changes verdict" describes a ranking claim, and the
/// ranking is exactly what changes here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayheadMotion {
    /// A client's own request: the reading follows it, forwards or backwards.
    Anywhere,
    /// A segment served from cache: it may raise the reading, never lower it.
    ForwardOnly,
}

/// Record where a stream's client has reached, and bound the map.
fn note_playhead(state: &mut SchedState, stream: StreamKey, segment: u32) {
    write_playhead(state, stream, segment, PlayheadMotion::Anywhere);
}

fn write_playhead(state: &mut SchedState, stream: StreamKey, segment: u32, motion: PlayheadMotion) {
    if stream == StreamKey::NONE {
        return;
    }
    state.playhead_clock += 1;
    let tick = state.playhead_clock;
    match state.playheads.entry(stream) {
        std::collections::hash_map::Entry::Occupied(mut e) => {
            let (head, seen) = e.get_mut();
            if motion == PlayheadMotion::Anywhere || segment > *head {
                *head = segment;
            }
            // The tick is refreshed even when the VALUE is not. It orders
            // eviction, and the question it answers is "is this stream still
            // being watched?" — to which a hit that did not advance the reading
            // is a yes. Leaving it stale would let a viewer served entirely out
            // of cache, which is the healthiest stream on the box, be evicted
            // ahead of one that has not asked for anything in a minute.
            *seen = tick;
        }
        std::collections::hash_map::Entry::Vacant(v) => {
            // A hit on a stream with NO reading establishes one, and this is the
            // gap `PlayheadSeed` documents but could not close: the map was
            // miss-driven, so a re-watch or a second viewer on the same media
            // reached its first deep prefetch with nothing recorded, and letting
            // that GUESS seed would have had it ranking itself. A hit is not a
            // guess — it is a client's own request — so it carries exactly the
            // entitlement an interactive `Submit` does.
            v.insert((segment, tick));
        }
    }
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
/// `i64` so "already passed" is REPRESENTABLE — and representable is all it is.
/// A negative distance is not a small distance, it is a segment the viewer went
/// past while the job sat in the queue, so ordering by this value alone gets the
/// second case backwards too: `-5` would sort ahead of `1`, the segment they
/// actually need next. Ranking is `next_to_dispatch`'s job and it sorts passed
/// work behind everything; nothing else may compare these values raw.
///
/// Jobs with no stream or no segment return `i64::MAX`: nothing is known to be
/// about to need them, so they sort behind all useful work — but still ahead of
/// work that is known to be useless.
///
/// What is left in that bucket is work with no viewer at all (the transcode
/// tool, tests, whole-file jobs, `StreamKey::NONE`) plus speculation on a stream
/// nobody has yet stated a position for. The cold-start prewarm is no longer in
/// it: it carries [`PlayheadSeed::StatesTheStart`] and seeds its own stream (see
/// the `Submit` arm), so it is measured rather than merely unknown.
fn lookahead_distance(state: &SchedState, ctx: &JobCtx) -> i64 {
    let (Some(seg), Some((head, _))) = (ctx.segment, state.playheads.get(&ctx.stream)) else {
        return i64::MAX;
    };
    seg as i64 - *head as i64
}

/// Has the viewer gone past the segment a [`lookahead_distance`] describes?
///
/// The ONE definition of "already passed", and the first half of [`is_stale`],
/// which is what the ranking band in `next_to_dispatch`, the drop and the
/// victim choice in `queue_or_refuse` all actually read. Three places that must
/// agree: a queue that ranks a job last for a reason it then declines to drop
/// it for, or evicts a job it would have been happy to dispatch, is a queue
/// with two opinions about the same number.
///
/// STRICTLY negative, and the boundary is the whole content of this function.
/// `lookahead_distance` measures against the last segment the client ASKED FOR,
/// not the last it finished — a playhead advances when an interactive
/// submission arrives — so distance 0 is the segment being fetched right now:
/// wanted, in progress, and not yet produced. It is also the segment
/// `next_to_dispatch` ranks FIRST among speculation, so treating it as passed
/// would have the queue destroy the very job it had just called the most urgent
/// thing in it. Only `seg < head` says the client has moved on.
///
/// `i64::MAX` (no playhead, or a job that is not a numbered segment) is not
/// passed either, and must never be: nothing is KNOWN to need it, which is not
/// the same as knowing nothing does.
fn has_been_passed(distance: i64) -> bool {
    distance < 0
}

/// Is this speculative job work the viewer OUTRAN — as opposed to work aimed
/// behind them deliberately?
///
/// [`has_been_passed`] answers "is this segment behind the playhead", which is
/// necessary and NOT sufficient. A job can be behind the playhead for two
/// opposite reasons:
///
/// * it was submitted ahead of the viewer and the viewer overtook it while it
///   waited — genuinely wasted work, the case the drop exists for; or
/// * it was submitted behind the viewer on purpose, because somebody knows the
///   viewer is about to go there. The SyncPlay seek prewarm does exactly this:
///   it submits `Background` for the seek TARGET on the member's EXISTING
///   stream, before that member issues its own interactive request for the
///   target, so on a backward group seek — the documented rewind shape — every
///   one of its jobs is negative the instant it arrives.
///
/// Dropping the second as if it were the first makes a shipped feature a
/// load-dependent no-op: with a free permit the prewarm goes straight through
/// `place`, and only under saturation does it queue and get thrown away — with
/// nothing to show for it but a `stale` increment indistinguishable from a
/// genuinely wasted guess.
///
/// What tells them apart is [`submitted_behind_a_known_playhead`], and the
/// predicate reads exactly as the sentence does:
///
/// ```text
/// stale  ⟺  seg >= head_at_submit  &&  seg < head_now
/// ```
///
/// The first half is "this job was NOT aimed behind the viewer when it was
/// made"; the second is [`has_been_passed`], "it is behind them now". Only a
/// job that was in front of its viewer and is now behind them was OUTRUN.
///
/// It is deliberately the VALUE at submit and not the TICK at submit. A tick
/// answers "has the playhead moved since?", which sounds like the same question
/// and is not: the clock is bumped on every interactive submission and on every
/// cache hit reported by [`TranscodeScheduler::note_playhead`], whatever either
/// does to the position. So a member still playing FORWARD while a
/// backward prewarm sits queued on its stream — the ordinary interleaving, since
/// the prewarm fires the moment `/SyncPlay/Seek` is dispatched and the member
/// applies the command seconds later — bumped the tick and condemned the
/// prewarm, exactly the failure this predicate exists to prevent, with a
/// narrower trigger. The value is strictly stronger: every job the tick test
/// called stale for a REAL overtake still is (a playhead cannot pass a segment
/// without changing value), and a job submitted behind its viewer is now never
/// stale however the playhead subsequently moves.
///
/// That last clause is a deliberate permanence, not an oversight. A backward
/// prewarm the group then abandons is not swept — it is cancelled, which is a
/// different mechanism and the right one: a seek or a track swap aborts the
/// prefetch task, closing its `oneshot`, and `reap_abandoned` collects it on the
/// next drain.
///
/// A stream with no playhead at all is not stale either: `lookahead_distance`
/// is `i64::MAX` there, which [`has_been_passed`] already declines to call
/// passed.
fn is_stale(state: &SchedState, ctx: &JobCtx) -> bool {
    if ctx.class != JobClass::Background {
        return false;
    }
    if !has_been_passed(lookahead_distance(state, ctx)) {
        return false;
    }
    !submitted_behind_a_known_playhead(ctx)
}

/// Was this job aimed behind its viewer ON PURPOSE — i.e. did the submitter see
/// a playhead and ask for a segment below it anyway?
///
/// Frozen at admission, and the whole of what [`is_stale`] adds to
/// [`has_been_passed`].
///
/// `None` — no reading for that stream when the job was admitted — is NOT
/// deliberate. Nobody chose to aim behind a playhead they could not see, so a
/// job that had no reference and is later found behind one was outrun like any
/// other guess. Getting this arm backwards would give an ordinary prefetch on a
/// stream whose opening segments were cache hits (so no playhead existed yet)
/// permanent immunity from the sweep AND, via [`urgency_key`], the imminent
/// ranking a real backward prewarm earns.
fn submitted_behind_a_known_playhead(ctx: &JobCtx) -> bool {
    match (ctx.segment, ctx.playhead_at_submit) {
        (Some(seg), Some(head)) => seg < head,
        _ => false,
    }
}

/// Count how one job left the queue — at most once, ever, for that job.
///
/// [`QueueOutcome`]'s arms are only a partition if nothing is counted twice,
/// and the one thing that can happen twice to a job is placement: a transient
/// device failure sends it back through `place`, where it may dispatch again,
/// shed again, or fail. `retries` is the flag that says so. It is incremented
/// before every re-placement and the only way to reach one is to have been
/// dispatched, so `retries == 0` means exactly "this job's first pass, not yet
/// counted".
///
/// The alternative — counting each re-placement — would make `dispatched` count
/// DISPATCHES while `stale` and `evicted` count jobs, which is precisely the
/// defect `pharos_transcode_pin_total{outcome="followed"}` shipped with:
/// two arms of one counter with different denominators, whose ratio is
/// meaningless and whose absolute values look fine.
///
/// Returns whether it counted, so that this is the ONLY place the rule is
/// stated. Anything else recorded per terminal outcome — the queue-distance
/// histogram is the one today — asks here rather than re-deriving `retries ==
/// 0` beside it, which is two copies of one decision waiting to disagree.
fn record_queue_outcome(ctx: &JobCtx, outcome: QueueOutcome) -> bool {
    if ctx.retries > 0 {
        return false;
    }
    outcome.record(ctx.class);
    true
}

/// The dispatch arm, plus the distance the job was dispatched AT.
///
/// Recorded here rather than at examination for the reason in
/// [`record_queue_outcome`]: a queued job is re-examined on every drain pass,
/// and a histogram fed there would report the distances of the jobs that waited
/// longest, over and over, instead of the distances work actually ran at.
///
/// No label on the histogram. The two that suggest themselves — the stream and
/// the distance — are unbounded and per-viewer respectively, and the question
/// it exists to answer ("is speculation being served shallow-first, or is the
/// queue serving deep guesses while near ones wait?") is a distribution, not a
/// breakdown. Jobs with no playhead contribute nothing: `i64::MAX` is "no
/// answer", and recording it would drag every percentile to the ceiling.
///
/// NEGATIVE distances contribute nothing either, and the reason is the same
/// one. Before the backward guess was ranked as imminent, a passed job could
/// never be dispatched at all, so this filter had nothing to exclude; now a
/// SyncPlay seek prewarm at −400 reaches an encoder routinely. It is not a
/// measurement of prefetch DEPTH: its magnitude is a property of how far the
/// group seeked, chosen by a viewer pressing a button, and it says nothing
/// about whether the prefetch ladder is tuned too far ahead. Left in, every
/// such sample lands in the bottom bucket of a positively-bounded histogram and
/// drags the low quantiles toward zero, which reads as "prefetch is being
/// served shallow-first" — the healthy verdict — exactly when a group is
/// seeking. The alternative, documenting their presence instead of excluding
/// them, leaves a number no query can separate: there is no label to filter on
/// (see above) and adding one keyed on sign would put a per-viewer behaviour
/// into a series that exists to describe the ladder. `queue_outcome_total`
/// already counts that the work happened; this histogram answers a narrower
/// question and keeps to it.
fn record_dispatch(state: &SchedState, ctx: &JobCtx) {
    // The histogram shares the counter's denominator by ASKING for it rather
    // than re-testing `retries == 0` beside it: one rule, one place, and a
    // retry that stopped counting in one of them cannot silently keep counting
    // in the other.
    if !record_queue_outcome(ctx, QueueOutcome::Dispatched) {
        return;
    }
    if ctx.class == JobClass::Background {
        let distance = lookahead_distance(state, ctx);
        if distance != i64::MAX && !has_been_passed(distance) {
            metrics::histogram!("pharos_transcode_queue_distance").record(distance as f64);
        }
    }
}

impl TranscodeScheduler {
    pub fn spawn(
        devices: DeviceTable,
        spawner: Arc<dyn WorkerSpawner>,
        cfg: SchedConfig,
    ) -> TranscodeScheduler {
        let (tx, mut rx) = mpsc::channel::<SchedMsg>(cfg.inbox_depth);
        let self_tx = tx.clone();
        let admission = AdmissionController::new(cfg.admission.clone());
        // Seed `pharos_transcode_background_allowance` for every device NOW,
        // before the actor task even exists, rather than waiting for the first
        // finished interactive segment to emit it (which `observe_margin` does
        // and may be minutes away, or never, if nothing has been played since
        // boot). The `metrics` crate registers a series lazily on first
        // emission, so until one of the two ran, "not deployed", "deployed but
        // idle", and "deployed and wedged at the floor" were all the same
        // absent series. Seeding this gauge (never the `margin_total` counter,
        // which must stay absent until a real observation happens) makes the
        // first of those three distinguishable from the other two; the other
        // two are already distinguished by whether `margin_total` itself has
        // any samples. Done synchronously here, before `tokio::spawn`, rather
        // than from inside the actor task: this line runs on the caller's
        // thread, which is where a test's `metrics::with_local_recorder`
        // installs its recorder — inside the spawned task it would depend on
        // that task happening to be polled on the same thread before anyone
        // reads the metric back.
        for slot in devices.slots() {
            emit_allowance_gauge(&admission, slot.id, slot.capacity);
        }
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
            playheads: HashMap::new(),
            playhead_clock: 0,
        };
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                handle(&mut state, msg, &self_tx);
            }
        });
        TranscodeScheduler { tx }
    }

    /// Submit a job and await its terminal outcome (FileDirect: resolves
    /// when the file is written; errors on failure/exhaustion/Busy).
    pub async fn submit(
        &self,
        input: PathBuf,
        opts: TranscodeOptions,
        sink: SinkRequest,
        class: JobClass,
        hint: JobHint,
    ) -> Result<JobDone, SchedError> {
        self.submit_tracked(input, opts, sink, class, hint, None)
            .await
    }

    /// [`Self::submit`], additionally publishing the assigned [`JobId`] into
    /// `assigned` as soon as the actor allocates it.
    ///
    /// For callers that may need to change their mind about a running job — in
    /// practice the segment cache, whose speculative encodes acquire a client
    /// when somebody coalesces onto them.
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_tracked(
        &self,
        input: PathBuf,
        opts: TranscodeOptions,
        sink: SinkRequest,
        class: JobClass,
        hint: JobHint,
        assigned: Option<JobSlot>,
    ) -> Result<JobDone, SchedError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SchedMsg::Submit {
                input,
                opts,
                sink,
                class,
                hint,
                assigned,
                reply,
            })
            .await
            .map_err(|_| SchedError::Io("scheduler stopped".into()))?;
        rx.await
            .map_err(|_| SchedError::Io("scheduler dropped reply".into()))?
    }

    /// Re-rank a job somebody is now blocked on as a client's own.
    ///
    /// A client waiting on a speculative job's result is proof the speculation
    /// was correct — and proof that it is no longer speculative. Leaving it at
    /// its old class would make that client wait behind every other client's
    /// work, including work submitted after it started waiting, which is the
    /// B108 shape wearing different clothes.
    ///
    /// Fire-and-forget by design: nothing the caller does depends on the
    /// answer, and a promotion that arrives after the job has finished is
    /// harmless (counted `unknown`). Silent only if the actor is gone, at which
    /// point the caller's own request is about to fail anyway.
    pub async fn promote(&self, job_id: JobId) {
        let _ = self.tx.send(SchedMsg::Promote { job_id }).await;
    }

    /// Say that a client reached `segment` on `stream` without any job being
    /// submitted for it — i.e. the segment came out of the cache.
    ///
    /// Without this the reading is MISS-DRIVEN, and a miss is what stops
    /// happening when the system works: a viewer with a warm buffer takes fast
    /// and coalesced cache hits, neither of which reaches here, so its last
    /// reading is wherever it last missed and the error grows in proportion to
    /// how well prefetch is doing. `lookahead_distance` — and with it which
    /// guess is dispatched (`next_to_dispatch`), which is dropped (`is_stale`),
    /// which is evicted (`queue_or_refuse`) and what
    /// `pharos_transcode_queue_distance` reports — is then wrong for that stream
    /// in one direction, all four at once.
    ///
    /// SYNCHRONOUS and fire-and-forget, which is the whole cost argument. A hit
    /// is the common case, so this runs on the path holding a viewer's segment
    /// response; `try_send` cannot await, so that path never waits on the actor,
    /// and a full inbox drops the update rather than applying backpressure to
    /// the HTTP handler. Dropping is safe by construction: a lost reading leaves
    /// the map exactly as stale as it was before this existed and never staler.
    /// It is counted, because a snapshot showing a reading that stopped moving
    /// otherwise cannot distinguish a viewer who stopped from an inbox that is
    /// saturated.
    ///
    /// See [`PlayheadMotion`] for the only direction a hit may move a reading.
    pub fn note_playhead(&self, stream: StreamKey, segment: u32) {
        if stream == StreamKey::NONE {
            return;
        }
        if self
            .tx
            .try_send(SchedMsg::Playhead { stream, segment })
            .is_err()
        {
            metrics::counter!("pharos_transcode_playhead_dropped_total").increment(1);
        }
    }

    /// Submit a live transcode and get a byte stream of the muxed output.
    /// The job is dispatched to the least-loaded eligible device; the
    /// returned stream owns the worker + its device permit, so the slot
    /// frees when the consumer drops the stream (also tearing down the
    /// encode). Returns `Busy` when no device has a free permit.
    pub async fn submit_live(
        &self,
        input: PathBuf,
        opts: TranscodeOptions,
    ) -> Result<LiveByteStream, SchedError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SchedMsg::SubmitLive { input, opts, reply })
            .await
            .map_err(|_| SchedError::Io("scheduler stopped".into()))?;
        rx.await
            .map_err(|_| SchedError::Io("scheduler dropped reply".into()))?
    }

    pub async fn snapshot(&self) -> Option<SchedSnapshot> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(SchedMsg::Snapshot { reply }).await.ok()?;
        rx.await.ok()
    }
}

/// Wraps a live byte stream so it owns the device permit for its
/// lifetime — dropping the stream frees the slot (RAII), same discipline
/// as the segment path.
struct PermitStream {
    inner: LiveByteStream,
    _permit: OwnedSemaphorePermit,
    job_id: JobId,
    device: DeviceId,
    started: Instant,
    live: Arc<AtomicUsize>,
}

impl futures_core::Stream for PermitStream {
    type Item = std::io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl Drop for PermitStream {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::Relaxed);
        // Closes the pair with the acquisition log. A device sitting at
        // capacity with no segment jobs to account for it is a live stream
        // holding a permit; without a release line there is no way to tell a
        // stream that ended from one still running.
        tracing::info!(
            job_id = %self.job_id,
            device = %self.device,
            held_ms = self.started.elapsed().as_millis() as u64,
            "live transcode released its device permit"
        );
    }
}

fn handle(state: &mut SchedState, msg: SchedMsg, self_tx: &mpsc::Sender<SchedMsg>) {
    match msg {
        SchedMsg::Submit {
            input,
            opts,
            sink,
            class,
            hint,
            assigned,
            reply,
        } => {
            if matches!(sink, SinkRequest::LiveStream) {
                // Wired in the fd-passing step; not yet schedulable. Counted
                // like any other "no device can take this", so the outcome
                // counter's total really is every `Submit` the actor saw.
                QueueOutcome::Failed.record(class);
                let _ = reply.send(Err(SchedError::Unsupported));
                return;
            }
            // Only an INTERACTIVE submission tells us where the viewer actually
            // is. A speculative request says nothing about the playhead;
            // letting prefetch move it would make the lookahead distance
            // measure itself.
            //
            // A stream with NO playhead yet is the one exception, and it is a
            // different act: seeding cannot MOVE an existing reading, so it
            // cannot make any active viewer's lookahead self-referential. What
            // it can do is stop the cold-start prewarm being permanently
            // unknowable. `prewarm_cold_start` submits `Background` against a
            // brand-new `StreamKey` no interactive request has ever touched, so
            // its distance was `i64::MAX` BY CONSTRUCTION for the whole of its
            // life. Ranked behind every live guess for DISPATCH that is fine —
            // deferral is recoverable. Handed to `queue_or_refuse` it made a
            // new viewer's opening segments the second most useless thing in
            // the queue: refused, or admitted and then evicted, in favour of a
            // guess a minute ahead of somebody already playing — and the
            // viewer then took exactly the opening `fragLoadTimeOut` the
            // prewarm exists to prevent. Eviction is not recoverable.
            //
            // The entitlement is the CALLER'S, carried on the hint
            // ([`PlayheadSeed`]), not inferred from the map being empty. The
            // caller already knows the answer — the prewarm picks its base from
            // the resume position or the group's seek target — while "no entry
            // yet" is a much weaker fact than it looks: it holds for any stream
            // nothing has been served on yet, and letting an ordinary guess seed
            // there puts it back to measuring itself.
            if let Some(seg) = hint.segment {
                let may_seed =
                    hint.seeds_playhead.may_seed() && !state.playheads.contains_key(&hint.stream);
                if class == JobClass::Interactive || may_seed {
                    note_playhead(state, hint.stream, seg);
                }
            }
            let job_id = JobId(state.next_job);
            state.next_job += 1;
            // Published BEFORE placement, so a requester that coalesces onto
            // this job can name it even while it is still queued — which is
            // exactly the case promotion exists for.
            if let Some(slot) = &assigned {
                slot.assign(job_id);
            }
            let ctx = JobCtx {
                input,
                opts,
                sink,
                reply,
                excluded: SmallVec::new(),
                retries: 0,
                last_error: None,
                enqueued: Instant::now(),
                dispatched: None,
                assigned,
                class,
                promoted_at: None,
                device: None,
                peer_jobs: 0,
                background_peers: 0,
                stream: hint.stream,
                segment: hint.segment,
                // Read AFTER the `note_playhead` above, so an interactive
                // submission is never behind its own reading and a seeding
                // guess is never behind its own seed.
                playhead_at_submit: state.playheads.get(&hint.stream).map(|(h, _)| *h),
                // Replaced at dispatch, once the device is known.
                span: tracing::Span::none(),
            };
            place(state, job_id, ctx, self_tx);
        }
        SchedMsg::Promote { job_id } => {
            promote_job(state, job_id, self_tx);
        }
        SchedMsg::Playhead { stream, segment } => {
            write_playhead(state, stream, segment, PlayheadMotion::ForwardOnly);
        }
        SchedMsg::SubmitLive { input, opts, reply } => {
            // Live path: acquire a permit best-first, then spawn a
            // streaming worker off-actor. The permit rides inside the
            // returned stream (RAII release on drop) — no inflight
            // bookkeeping, no JobFinished.
            let now = Instant::now();
            let eligible = state.devices.eligible_for(&opts, now);
            let mut acquired = None;
            for dev in eligible.iter().copied() {
                if let Some(slot) = state.devices.slot(dev) {
                    if let Ok(permit) = slot.sem.clone().try_acquire_owned() {
                        acquired = Some((dev, permit));
                        break;
                    }
                }
            }
            let Some((device, permit)) = acquired else {
                // The live path has no queue: a rejected stream falls back to
                // an *unscheduled* inline ffmpeg at the caller, which holds no
                // permit and is therefore invisible to every saturation gauge.
                // A silent rejection here is the start of load the scheduler
                // cannot see, so it is never silent.
                let busy = !eligible.is_empty();
                tracing::warn!(
                    input = %input.display(),
                    eligible = ?eligible,
                    occupancy = ?state
                        .devices
                        .slots()
                        .iter()
                        .map(|s| (s.id, s.in_use(), s.capacity))
                        .collect::<Vec<_>>(),
                    reason = if busy { "all permits busy" } else { "no eligible device" },
                    "live transcode rejected; caller falls back off-scheduler"
                );
                let _ = reply.send(Err(if busy {
                    SchedError::Busy
                } else {
                    SchedError::Unsupported
                }));
                return;
            };
            let job_id = JobId(state.next_job);
            state.next_job += 1;
            // A live stream holds its permit for as long as the client keeps
            // reading — minutes, not the seconds a segment takes. Name the
            // device it took at acquisition; `PermitStream::drop` closes the
            // pair with how long it held it.
            tracing::info!(
                %job_id,
                device = %device,
                eligible = ?eligible,
                input = %input.display(),
                "live transcode took a device permit"
            );
            let live = state.live.clone();
            live.fetch_add(1, Ordering::Relaxed);
            let spec = JobSpec {
                job_id,
                input,
                opts,
                device,
                sink: OutputSink::Stdout,
            };
            let spawner = state.spawner.clone();
            tokio::spawn(async move {
                match spawner.spawn_streaming(spec).await {
                    Ok(inner) => {
                        let stream: LiveByteStream = Box::pin(PermitStream {
                            inner,
                            _permit: permit,
                            job_id,
                            device,
                            started: Instant::now(),
                            live,
                        });
                        let _ = reply.send(Ok(stream));
                    }
                    Err(e) => {
                        drop(permit);
                        live.fetch_sub(1, Ordering::Relaxed);
                        tracing::warn!(%job_id, device = %device, error = %e, "live transcode spawn failed; permit released");
                        let _ = reply.send(Err(SchedError::Io(e.to_string())));
                    }
                }
            });
        }
        SchedMsg::JobFinished {
            job_id,
            device,
            result,
            worker,
        } => {
            // Return a live worker to the idle pool first so a drained
            // pending job can reuse it.
            if let Some(w) = worker {
                state.idle.push(w);
            }
            let Some(mut ctx) = state.inflight.remove(&job_id) else {
                // Unknown job (already resolved / cancelled). Drain anyway.
                drain_pending(state, self_tx);
                return;
            };
            match result {
                WorkerRunResult::Done { out_bytes } => {
                    // Split end-to-end latency: queue-wait (Submit → first
                    // dispatch, includes any failed-device retries) vs encode
                    // (last dispatch → now). A slow segment with high
                    // queue_wait_ms = saturated devices / retry churn; high
                    // encode_ms = a genuinely slow encoder. `retries` > 0 flags
                    // a job that bounced off a device (e.g. a phantom GPU).
                    let now = Instant::now();
                    let queue_ms = ctx
                        .dispatched
                        .map(|d| d.saturating_duration_since(ctx.enqueued).as_millis() as u64)
                        .unwrap_or(0);
                    let encode_ms = ctx
                        .dispatched
                        .map(|d| now.saturating_duration_since(d).as_millis() as u64)
                        .unwrap_or(0);
                    // Close the loop on the span opened at dispatch, so the
                    // trace carries the outcome next to the placement that
                    // produced it rather than in a separate line to be joined
                    // by job id.
                    ctx.span.record("queue_wait_ms", queue_ms);
                    ctx.span.record("encode_ms", encode_ms);
                    // Only record a byte count the worker could actually
                    // measure: a live-stream job's bytes go down the pipe, so
                    // its `0` means "not measured", not "produced nothing".
                    // Leaving the field ABSENT says that; writing 0 lies.
                    if ctx.sink.measures_out_bytes() {
                        ctx.span.record("out_bytes", out_bytes);
                    }
                    ctx.span.record("outcome", "done");
                    ctx.span.in_scope(|| {
                        tracing::info!(
                            %job_id,
                            %device,
                            out_bytes,
                            sink = ctx.sink.label(),
                            class = %ctx.class,
                            queue_wait_ms = queue_ms,
                            encode_ms,
                            peer_jobs = ctx.peer_jobs,
                            background_peers = ctx.background_peers,
                            retries = ctx.retries,
                            "transcode job done"
                        );
                    });
                    // Fold this segment into what the device has learned. Only
                    // interactive jobs carry a deadline anybody is waiting on:
                    // a speculative encode being slow is not a symptom, it is
                    // the system working as designed.
                    if ctx.class == JobClass::Interactive {
                        // The LATER of dispatch and promotion, not `dispatched`.
                        // A job promoted while it was still running began its
                        // encode before any client existed, and charging the
                        // client's deadline for that head start punishes the
                        // system for a prefetch that landed (see
                        // `JobCtx::promoted_at`). A job promoted while it was
                        // still QUEUED takes `dispatched`, which is the same
                        // window every unpromoted interactive job is judged on:
                        // the wait it served before the permit is queue-wait,
                        // and `pharos_transcode_queue_wait_seconds` is where
                        // that is read.
                        let observed_ms = match (ctx.dispatched, ctx.promoted_at) {
                            (Some(d), Some(p)) => {
                                now.saturating_duration_since(d.max(p)).as_millis() as u64
                            }
                            _ => encode_ms,
                        };
                        observe_margin(state, device, &ctx, observed_ms);
                    }
                    let _ = ctx.reply.send(Ok(JobDone {
                        device,
                        out_bytes,
                        queue_wait_ms: queue_ms,
                        encode_ms,
                        peer_jobs: ctx.peer_jobs,
                        background_peers: ctx.background_peers,
                    }));
                }
                WorkerRunResult::Failed(err) if !err.is_transient() => {
                    // Symmetry: whatever the success path records, the failure
                    // path records too. A span that only ever carries an
                    // outcome when the job succeeded is the shape that hides
                    // outages.
                    ctx.span.record("outcome", "failed");
                    ctx.span.in_scope(|| {
                        tracing::warn!(%job_id, %device, error = %err, "transcode job failed (non-recoverable)");
                    });
                    let _ = ctx.reply.send(Err(SchedError::Failed(err)));
                }
                WorkerRunResult::Failed(err) => {
                    // Transient: cool the device + exclude it, retry next-best.
                    // NEVER cool the CPU — it's the terminal fallback; cooling
                    // it would make `eligible_for` empty and surface a spurious
                    // `Unsupported`/`Failed` for a perfectly encodable job (and
                    // for any other job arriving during the window).
                    if device != DeviceId::Cpu {
                        state
                            .devices
                            .set_cooldown(device, Instant::now() + state.cfg.cooldown);
                        ctx.excluded.push(device);
                    }
                    ctx.span.record("outcome", "transient_retry");
                    ctx.retries += 1;
                    ctx.last_error = Some(err);
                    retry_or_fail(state, job_id, ctx, self_tx);
                }
                WorkerRunResult::Died => {
                    // Worker death is not the device's fault — don't cool
                    // the device, but count the retry and re-place. A
                    // fresh worker is spawned on the next dispatch.
                    ctx.span.record("outcome", "worker_died");
                    ctx.span.in_scope(|| {
                        tracing::warn!(%job_id, %device, "transcode worker died mid-job; retrying");
                    });
                    ctx.retries += 1;
                    ctx.last_error = Some(WorkerError::Other("worker died".into()));
                    retry_or_fail(state, job_id, ctx, self_tx);
                }
            }
            // A permit just freed (the detached task dropped it before
            // sending JobFinished) — let queued jobs claim it.
            drain_pending(state, self_tx);
        }
        SchedMsg::Snapshot { reply } => {
            let devices = state
                .devices
                .slots()
                .iter()
                .map(|s| DeviceStat {
                    id: s.id,
                    capacity: s.capacity,
                    in_use: s.in_use(),
                    in_cooldown: matches!(s.cooldown_until, Some(t) if t > Instant::now()),
                    inflight_interactive: state
                        .inflight
                        .values()
                        .filter(|c| c.class == JobClass::Interactive && c.device == Some(s.id))
                        .count(),
                    inflight_background: state
                        .inflight
                        .values()
                        .filter(|c| c.class == JobClass::Background && c.device == Some(s.id))
                        .count(),
                })
                .collect();
            let now = Instant::now();
            let _ = reply.send(SchedSnapshot {
                devices,
                pending: state.pending.len(),
                idle_workers: state.idle.len(),
                pending_interactive: state
                    .pending
                    .iter()
                    .filter(|(_, c)| c.class == JobClass::Interactive)
                    .count(),
                pending_background: state
                    .pending
                    .iter()
                    .filter(|(_, c)| c.class == JobClass::Background)
                    .count(),
                oldest_pending_ms: state
                    .pending
                    .iter()
                    .map(|(_, c)| now.saturating_duration_since(c.enqueued).as_millis() as u64)
                    .max(),
                inflight: state.inflight.len(),
                live_streams: state.live.load(Ordering::Relaxed),
                background_allowance: state
                    .devices
                    .slots()
                    .iter()
                    .map(|s| (s.id, state.admission.raw_allowance(s.id, s.capacity)))
                    .collect(),
                playheads: state.playheads.iter().map(|(k, (s, _))| (*k, *s)).collect(),
            });
        }
    }
}

/// Reclassify a job somebody turned out to be blocked on.
///
/// Looks in `pending` first and `inflight` second, because those are the two
/// places a job can be and they want the change for different reasons: a queued
/// job is re-ranked (a client's work outranks every guess), while an inflight
/// one stops counting against its device's speculative allowance, so the next
/// admission decision does not treat a client's segment as crowding.
///
/// Deliberately does NOT move the stream's playhead. Promotion says a client is
/// waiting on this segment, not that it has reached it — the requester joined
/// work already in progress, and letting that advance the playhead would make
/// the lookahead distance measure itself, which `note_playhead` refuses for the
/// same reason.
fn promote_job(state: &mut SchedState, job_id: JobId, self_tx: &mpsc::Sender<SchedMsg>) {
    let found = state
        .pending
        .iter_mut()
        .find(|(id, _)| *id == job_id)
        .map(|(_, ctx)| (ctx, PromotionOutcome::Queued))
        .or_else(|| {
            state
                .inflight
                .get_mut(&job_id)
                .map(|ctx| (ctx, PromotionOutcome::Inflight))
        });
    let Some((ctx, outcome)) = found else {
        PromotionOutcome::Unknown.record();
        return;
    };
    if ctx.class == JobClass::Interactive {
        PromotionOutcome::AlreadyClient.record();
        return;
    }
    ctx.class = JobClass::Interactive;
    let now = Instant::now();
    // The instant a client started waiting on these bytes. Everything this job
    // does from here is done for that client, and the admission controller
    // judges it against a deadline measured from here rather than from a
    // dispatch that may predate the client by seconds — see
    // `JobCtx::promoted_at`.
    ctx.promoted_at = Some(now);
    let waited_ms = now.saturating_duration_since(ctx.enqueued).as_millis() as u64;
    let input = ctx.input.clone();
    outcome.record();
    tracing::info!(
        %job_id,
        outcome = outcome.label(),
        waited_ms,
        input = %input.display(),
        "speculative transcode promoted: a client is waiting on it"
    );
    // A promoted job may now outrank whatever the queue would otherwise have
    // chosen, and a permit may be free right now (nothing has finished since
    // it was queued). Re-drain rather than wait for the next completion.
    if outcome == PromotionOutcome::Queued {
        drain_pending(state, self_tx);
    }
}

/// Decide what to do with a job that just failed transiently / died.
fn retry_or_fail(
    state: &mut SchedState,
    job_id: JobId,
    ctx: JobCtx,
    self_tx: &mpsc::Sender<SchedMsg>,
) {
    if ctx.retries > state.cfg.max_retries {
        let err = ctx
            .last_error
            .clone()
            .unwrap_or(WorkerError::Other("retries exhausted".into()));
        tracing::warn!(%job_id, error = %err, "transcode job exhausted retries");
        let _ = ctx.reply.send(Err(SchedError::Failed(err)));
        return;
    }
    place(state, job_id, ctx, self_tx);
}

/// Would admitting a speculative job to `dev` put it beside a client's segment
/// past what that device has EARNED? Speculative work is wanted — it is what
/// turns the next segment into a cache hit — but not at the cost of the
/// segment somebody is currently staring at a spinner for.
///
/// The allowance is learned per device rather than configured, because the
/// number that matters is a property of the hardware and the source mix, not
/// of the config file: the same constant that under-uses a four-engine card
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
    let allowance = state.admission.allowance(dev, capacity);
    if interactive > 0 && background >= allowance {
        tracing::debug!(
            %dev,
            allowance,
            background,
            interactive,
            "speculative job refused: device has not earned this slot"
        );
        true
    } else {
        false
    }
}

/// Segment jobs already running on `dev`. This is what sets encode time on a
/// shared encoder — a device at capacity finishes each of its jobs several
/// times slower than the same device running one — and nothing recorded it, so
/// a slow segment and a crowded one produced identical telemetry.
fn peers_on(state: &SchedState, dev: DeviceId) -> usize {
    state
        .inflight
        .values()
        .filter(|c| c.device == Some(dev))
        .count()
}

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

/// Publish the raw (unrounded) speculative allowance for one device.
///
/// The single choke point for `pharos_transcode_background_allowance`: the
/// boot-time seed in [`TranscodeScheduler::spawn`] and the per-observation
/// emission in [`observe_margin`] both call this instead of each holding
/// their own copy of the metric name + label key, so the two can never
/// drift into reporting under different series. Always the *raw* value
/// (never the floored `allowance()`): the fraction is the only evidence a
/// multiplicative decrease ever happened.
fn emit_allowance_gauge(admission: &AdmissionController, device: DeviceId, capacity: usize) {
    metrics::gauge!(
        "pharos_transcode_background_allowance",
        "device" => device.to_string(),
    )
    .set(admission.raw_allowance(device, capacity));
}

/// Record what one finished client segment said about its device, and report it.
///
/// Symmetry rule: the verdict counter is incremented on EVERY interactive
/// completion, including the ones that teach nothing. A frozen allowance with
/// all-`ignored` observations means no signal reached the loop — a completely
/// different problem from a device that genuinely cannot go faster, and
/// indistinguishable from it if the ignored arm is silent.
///
/// `observed_ms` is how long the job ran FOR A CLIENT, which is the encode
/// duration for a job that was a client's from the start and the post-promotion
/// remainder for one a client joined. The caller decides which; this function
/// only reports which it was told, so the log line can never disagree with the
/// arithmetic above it.
fn observe_margin(state: &mut SchedState, device: DeviceId, ctx: &JobCtx, observed_ms: u64) {
    let capacity = state.devices.slot(device).map(|s| s.capacity).unwrap_or(1);
    // Read before `observe` takes `state.admission` mutably below — this is
    // the actual value the control law applies, not a copy of it, so the log
    // line stays true after anyone tunes `margin_ratio`.
    let margin_ratio = state.cfg.admission.margin_ratio;
    let obs = Observation {
        // Live/progressive jobs have no duration and so no deadline.
        segment_seconds: ctx.opts.duration_ticks.map(|t| t as f64 / 10_000_000.0),
        encode_seconds: observed_ms as f64 / 1000.0,
        background_peers: ctx.background_peers,
        // A retried job's encode time is the duration of a bounce.
        usable: ctx.retries == 0,
    };
    let before = state.admission.allowance(device, capacity);
    let verdict = state.admission.observe(device, capacity, obs);
    let after = state.admission.allowance(device, capacity);

    metrics::counter!(
        "pharos_transcode_margin_total",
        "device" => device.to_string(),
        "verdict" => verdict.label(),
    )
    .increment(1);
    emit_allowance_gauge(&state.admission, device, capacity);

    // Only when the INTEGER allowance moves. Logging every observation would
    // emit at segment rate and drown the line that matters.
    if before != after {
        tracing::info!(
            %device,
            verdict = verdict.label(),
            encode_secs = obs.encode_seconds,
            // Which instant the figure above was measured from. Without it, a
            // promoted job's shorter observation looks like a faster encode,
            // and the two need opposite readings.
            measured_from = if ctx.promoted_at.is_some() {
                "promotion"
            } else {
                "dispatch"
            },
            deadline_secs = obs.segment_seconds.map(|s| s * margin_ratio),
            background_peers = obs.background_peers,
            allowance_from = before,
            allowance_to = after,
            "speculative allowance changed"
        );
    }
}

/// Permits currently free across the devices that could take a given job.
/// Counted over the job's own candidate set, not the whole table: capacity on
/// a device that cannot encode this target is not headroom for it.
fn free_permits(state: &SchedState, candidates: &[DeviceId]) -> usize {
    candidates
        .iter()
        .filter_map(|d| state.devices.slot(*d))
        .map(|s| s.capacity.saturating_sub(s.in_use()))
        .sum()
}

/// What speculative work may do with `candidates`' permits *right now*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundAdmission {
    /// There is capacity above the reserve: take a permit.
    Dispatch,
    /// The reserve is intact but currently spoken for. Queue and be
    /// reconsidered, at a freshly computed urgency, on the next freed permit.
    Wait,
    /// The reserve is as large as the pool, so free permits can NEVER exceed it.
    /// Shed.
    Never,
}

/// May speculative work take one of `candidates`' permits *right now*?
///
/// The one admission rule, consulted identically by both dispatch paths so the
/// arrival path and the drain path cannot drift into reserving different
/// amounts. Recomputed each time it is asked: it is a statement about the
/// device table now, never a verdict cached on a job.
///
/// The `Never` arm is what keeps the reserve honest on a small pool. It was
/// once a clamp — `background_headroom.min(capacity - 1)` — which did stop the
/// hang it was written against (a job whose reserve can never be satisfied
/// queues forever, and its caller's `submit().await` never resolves), but paid
/// for it by disabling the reserve completely on a one-permit candidate pool:
/// `min(1, 0) == 0`, so speculation was free to take the only permit a client
/// needed. V58's first clause is that speculative work NEVER takes capacity a
/// client request needs, and it does not carry an exemption for narrow pools —
/// a pinned CMAF rendition resolves to exactly one device, so narrow pools are
/// not hypothetical. Refusing outright resolves the caller just as promptly as
/// the clamp did and keeps the reserve.
/// What a `BackgroundAdmission::Never` shed should actually reply with.
///
/// `background_admission` only ever sees `candidates` — it cannot tell a pool
/// that structurally cannot hold the reserve (genuine load management) apart
/// from a job whose candidate set narrowed to just that pool because a PRIOR
/// attempt on this same job hit a transient worker failure (`JobFinished`'s
/// transient-retry arm sets `ctx.last_error` and excludes the failed device
/// before re-placing). Replying `Busy` — "deliberate load management" — to
/// the second case collapses a real cause into a bare class, which this
/// project's error discipline forbids: a diagnostic must carry the offending
/// value, never a class alone.
///
/// Genuine load shedding (no prior failure on this job) still reports `Busy`:
/// `SchedError::Busy` is a variant callers act on, and it is deliberately
/// logged at DEBUG rather than ERROR so it does not drown real failures
/// beside it (V58) — reporting every shed as a `Failed` here would erase that
/// distinction for the common case this arm exists to serve.
fn background_never_error(ctx: &JobCtx) -> SchedError {
    match ctx.last_error.clone() {
        Some(err) => SchedError::Failed(err),
        None => SchedError::Busy,
    }
}

fn background_admission(state: &SchedState, candidates: &[DeviceId]) -> BackgroundAdmission {
    let capacity: usize = candidates
        .iter()
        .filter_map(|d| state.devices.slot(*d))
        .map(|s| s.capacity)
        .sum();
    let reserve = state.cfg.background_headroom;
    if reserve >= capacity {
        return BackgroundAdmission::Never;
    }
    if free_permits(state, candidates) > reserve {
        BackgroundAdmission::Dispatch
    } else {
        BackgroundAdmission::Wait
    }
}

/// Park a job that could not be dispatched, or refuse it if the queue is full.
///
/// Both classes now come through here. Speculative work queues because dropping
/// it meant the loser of a two-viewer race took a cold interactive miss on every
/// segment while the winner built a deep buffer; deferred beats never. It is
/// safe to queue only because a queued job no longer holds the segment cache's
/// per-key lock (006 phase 2a — that is what turned a deferred prefetch into a
/// client's 90 s wait, B108) and because it is re-ranked by urgency at dispatch
/// rather than served FIFO (see `next_to_dispatch`).
///
/// `pending_cap` is still a hard stop on the queue's SIZE — it never grows past
/// it. What changes when it is reached is WHICH job goes: see below.
fn queue_or_refuse(state: &mut SchedState, job_id: JobId, ctx: JobCtx) {
    // Phase 2b's kill switch. With `queue_background` off, speculative work
    // returns to the rule that preceded it — shed at the door, never parked —
    // while the Interactive queue below carries on unchanged. This is the ONE
    // place a `Background` job can enter `pending`, on either the arrival or
    // the retry path, so refusing here is sufficient: with the switch off
    // `pending` holds no speculative job, and the drain's requeue arms are
    // therefore unreachable rather than merely unused.
    //
    // Deliberately not expressed as `pending_cap = 0`, which looks like the
    // same lever and is not: that sheds INTERACTIVE jobs too, so a segment a
    // browser is waiting for comes back as a 500 instead of as a cold miss.
    if !state.cfg.queue_background && ctx.class == JobClass::Background {
        tracing::debug!(
            %job_id,
            stream = ctx.stream.0,
            segment = ctx.segment,
            pending = state.pending.len(),
            "speculative transcode shed: background queueing is disabled"
        );
        record_queue_outcome(&ctx, QueueOutcome::Shed);
        let _ = ctx.reply.send(Err(SchedError::Busy));
        return;
    }
    if state.pending.len() < state.cfg.pending_cap {
        state.pending.push_back((job_id, ctx));
        return;
    }
    // A full FIFO refuses the newest arrival, and for speculative work the
    // newest arrival is systematically the MOST urgent thing present: prefetch
    // is submitted in playback order, so a job arriving now is close to the
    // playhead while the incumbents are the deep guesses submitted when the
    // client was further back. Overflow therefore threw away the segment the
    // client was about to want and kept the one it would reach in a minute — if
    // it ever did.
    //
    // Evict the least urgent instead, by the same key that ranks dispatch
    // (`next_to_dispatch`) read at its maximum rather than its minimum. Same
    // key, so the queue cannot evict a job it would have been happy to
    // dispatch, and so OUTRUN work — which eviction, running on ARRIVAL between
    // drains, sees before any drain has had the chance to drop it — is the
    // first thing to go. Work aimed behind the playhead on PURPOSE is the last:
    // same key, magnitude 0, which is the point of `urgency_key` reading its
    // two ends rather than each site having its own opinion.
    //
    // Three rules make this a load-management decision rather than a lottery.
    // Only `Background` is a candidate: somebody is blocked on every
    // Interactive job in the queue, and freeing a slot by abandoning a client
    // mid-request is a 500 on a segment somebody is watching. And the newcomer
    // must actually BEAT the victim: an arriving guess deeper than everything
    // queued is refused rather than admitted at the cost of work nearer the
    // viewer, so a burst of far-out prefetch cannot churn the queue.
    //
    // The third is candidacy rather than rank, because rank cannot express it.
    // A job of UNKNOWN urgency (`i64::MAX` — no stream, or not a numbered
    // segment) sorts last for dispatch, which is right: `has_been_passed`'s own
    // reasoning is that nothing is KNOWN to need it, which is not the same as
    // knowing nothing does, and deferring it costs only latency. Reading the
    // same key at its maximum turns that into "destroy it first", and no single
    // total order can say both. So it is filtered OUT of candidacy instead: a
    // speculative arrival may not buy its slot by destroying work whose urgency
    // it cannot compare itself against.
    //
    // A CLIENT'S arrival still may. Otherwise a queue full of unknowable
    // speculation — whole-file work, the transcode tool, anything on
    // `StreamKey::NONE` — would become unevictable and start shedding
    // interactive requests, which is the exact failure `pending_cap` exists to
    // prevent. Between an unrankable guess and a segment somebody is watching,
    // the guess goes.
    //
    // Today the second rule already implies the first — an Interactive
    // incumbent's key is (0, .., its arrival index) and an arrival's is
    // (0, .., pending.len()) at best, so no arrival can ever beat one. The
    // filter is kept anyway, because that is an accident of the Interactive
    // tiebreak being arrival order: give the tier any other tiebreak (a
    // deadline, a client's buffer depth) and the guarantee that nobody evicts a
    // waiting client would disappear without a line of this function changing.
    //
    // The victim's reply is resolved here, exactly as a shed arrival's is. An
    // evicted job whose `oneshot` is merely dropped leaves its caller's
    // `submit().await` resolving through a `RecvError` with nothing to say.
    let mine = urgency_key(state, &ctx, state.pending.len());
    let arrival_is_a_client = ctx.class == JobClass::Interactive;
    let victim = state
        .pending
        .iter()
        .enumerate()
        .filter(|(_, (_, c))| {
            c.class == JobClass::Background
                && (arrival_is_a_client || lookahead_distance(state, c) != i64::MAX)
        })
        .map(|(idx, (_, c))| (idx, urgency_key(state, c, idx)))
        .max_by_key(|(_, key)| *key);
    // The admission is inside the `Some` arm of the REMOVE, not after it. `idx`
    // came from an enumeration of this same queue a few lines up and nothing
    // has touched it since, so `None` is unreachable — but pushing regardless
    // of what `remove` returned leaves "`pending` never exceeds `pending_cap`"
    // resting on that argument rather than on the code, and the argument is the
    // kind that survives a refactor by one line while the invariant does not.
    // Structured so a job only ever takes a slot that was actually freed.
    match victim.and_then(|(idx, worst)| {
        (worst > mine)
            // `VecDeque::remove` preserves the order of everything else, so
            // Interactive arrival order — which `next_to_dispatch` breaks ties
            // on — survives an eviction from the middle of the queue.
            .then(|| state.pending.remove(idx))
            .flatten()
    }) {
        Some((victim_id, victim_ctx)) => {
            tracing::debug!(
                job_id = %victim_id,
                class = %victim_ctx.class,
                stream = victim_ctx.stream.0,
                segment = victim_ctx.segment,
                distance = lookahead_distance(state, &victim_ctx),
                replaced_by = %job_id,
                replaced_by_class = %ctx.class,
                pending = state.pending.len(),
                input = %victim_ctx.input.display(),
                "speculative transcode evicted: a more urgent job arrived at a full queue"
            );
            record_queue_outcome(&victim_ctx, QueueOutcome::Evicted);
            let _ = victim_ctx.reply.send(Err(SchedError::Busy));
            state.pending.push_back((job_id, ctx));
        }
        None => {
            tracing::debug!(
                %job_id,
                class = %ctx.class,
                pending = state.pending.len(),
                evictable = victim.is_some(),
                "transcode job refused: the queue is full of work at least as urgent"
            );
            record_queue_outcome(&ctx, QueueOutcome::Shed);
            let _ = ctx.reply.send(Err(SchedError::Busy));
        }
    }
}

/// Where one queued job ranks. Lower is more urgent; the total order is the
/// scheduler's whole opinion about what to do next, and there is exactly one of
/// it — `next_to_dispatch` takes its minimum, `queue_or_refuse` its maximum.
///
/// `arrival` is the job's index in `pending`, which is arrival order. For a job
/// that is not in the queue yet (an arrival being weighed against the
/// incumbents) pass `pending.len()`: it would go to the back. It cannot change
/// that comparison's answer either way, because it is only ever read within the
/// Interactive tier and eviction only ever considers Background victims.
fn urgency_key(state: &SchedState, ctx: &JobCtx, arrival: usize) -> (i64, bool, i64) {
    match ctx.class {
        JobClass::Interactive => (0, false, arrival as i64),
        JobClass::Background => {
            let d = lookahead_distance(state, ctx);
            // The band is STALENESS, not sign: a job aimed behind the playhead
            // on purpose (the SyncPlay seek prewarm) is live work and must not
            // be ranked, or evicted, as a leftover. See `is_stale`.
            let stale = is_stale(state, ctx);
            // ...and keeping it out of the stale BAND is not enough on its own,
            // because the magnitude decides everything within a band. A
            // deliberate backward guess at −400 has magnitude 400 under `abs`
            // — larger than every forward guess in the queue — so the seek
            // target sorted behind all of them for dispatch (i.e. was encoded
            // after the member had already arrived at it, defeating the feature
            // without dropping anything) and was the preferred EVICTION victim
            // for the next arrival with any smaller magnitude.
            //
            // A deliberate backward guess is a statement that the viewer is
            // about to be there, which is IMMINENT, so it takes magnitude 0:
            // dispatched first, evicted last. That is what the reasoning above
            // always claimed and what the code now does.
            //
            // Only a passed job that survived `is_stale` can be here — outrun
            // work is in the `true` band and swept before selection — so this
            // arm is exactly "aimed behind the viewer on purpose". `i64::MAX`
            // (no playhead) is not passed and keeps its ceiling, so genuinely
            // unrankable work still sorts last.
            //
            // `abs` cannot overflow: both operands are `u32`-derived, so `d`
            // is bounded well inside `i64`, and `i64::MAX` maps to itself.
            let magnitude = if has_been_passed(d) && !stale {
                0
            } else {
                d.abs()
            };
            (1, stale, magnitude)
        }
    }
}

/// Which queued job should take the permit that just freed.
///
/// Tier is absolute — every Interactive before any Background — because someone
/// is blocked on each of the former and nobody on any of the latter. Within
/// Interactive it is FIFO, by queue index: all equally urgent, so the one that
/// asked first goes first. Within Background it is ascending lookahead
/// distance, recomputed HERE against each stream's current playhead.
///
/// Recomputing is the whole point. Distance is a property of NOW: a job queued
/// 30 s ago at distance 6 may now be distance 1 — the most urgent thing in the
/// queue — or already passed. Freezing urgency at submit time ranks both cases
/// exactly backwards, so the distance is never cached on the job.
///
/// Work the viewer has ALREADY PASSED sorts behind work they have not, which is
/// why the key carries `distance < 0` before the magnitude rather than the
/// signed distance. Ascending signed order puts `-5` — a segment abandoned while
/// the job waited — ahead of `1`, the segment the client needs next, so stale
/// speculation would systematically preempt exactly the work this ordering
/// exists to serve, wasting a permit AND delaying the useful job. Under
/// saturation, the only condition where the queue is non-trivial at all,
/// playheads outrun the queue routinely, so this is the common case and not an
/// edge. `reply.is_closed()` does not cover it: only a seek or a track swap
/// aborts a prefetch task, and a playhead advancing past a segment does neither.
///
/// Clamping to zero (`distance.max(0)`) would NOT do: it collapses every passed
/// job onto the same key as distance 0, the segment the viewer is standing on,
/// and `min_by_key` breaks that tie by arrival — so the stale job, which arrived
/// first by construction, would still win. Only ordering passed work as its own
/// band gets it behind ALL live work.
///
/// Within the passed band, nearest-to-the-playhead first (`abs`): none of it is
/// known to be wanted, but `-1` is the likelier of the two to be asked for again
/// than `-30`.
///
/// A DELIBERATE backward guess is not in that band and is not ranked by its
/// magnitude either: it takes magnitude 0, ahead of every forward guess. Its
/// distance is large precisely because the seek is long, and reading that as
/// "far away" dispatched the segment the group is about to land on after every
/// speculative segment queued for anybody still playing.
///
/// THIS function can no longer reach that band, and the honest statement of
/// that belongs here rather than in a comment describing a live path. Stale
/// work is swept unconditionally at the top of `drain_pending` (`reap_stale`),
/// and no job can become stale during a drain — a playhead moves only in a
/// message arm (an interactive `Submit`, or a `Playhead` reporting a cache hit),
/// and the actor processes no messages mid-drain. So by the time selection runs,
/// `pending` holds no stale job.
///
/// The band is not dead: it is read at its MAXIMUM by `queue_or_refuse`, which
/// runs on ARRIVAL, between drains — and an interactive arrival's own
/// `note_playhead` is what turns incumbents stale, a few lines before that
/// arrival picks a victim. That is the ordering the band exists for now: the
/// work a viewer has just outrun is the first thing a full queue gives up.
///
/// O(n) over `pending_cap` (256) rather than a heap: every key moves whenever
/// any client advances, so a heap would be re-keyed far more often than popped.
fn next_to_dispatch(state: &SchedState) -> Option<usize> {
    state
        .pending
        .iter()
        .enumerate()
        .min_by_key(|(idx, (_, ctx))| urgency_key(state, ctx, *idx))
        .map(|(idx, _)| idx)
}

/// Is there a permit anywhere for a queued job to take?
///
/// The one condition under which NO queued job of any class can be placed, and
/// therefore the only safe reason to stop draining early — see `drain_pending`.
fn any_permit_free(state: &SchedState) -> bool {
    state.devices.slots().iter().any(|s| s.available() > 0)
}

/// A CLIENT'S job that waited longer than this before starting is reported with
/// the state of the queue it just escaped. One segment covers 6 s of playback,
/// so a wait past this is already eating a client's buffer.
const LONG_WAIT_MS: u64 = 3_000;

/// The same, for speculative work — an order of magnitude looser because a long
/// wait means something completely different there.
///
/// Prefetch is submitted for a segment nobody has asked for and is now allowed
/// to WAIT for a permit instead of being dropped; sitting in the queue for
/// seconds is the design working, not an incident. Judged at `LONG_WAIT_MS`
/// every queued prefetch under load would warn, and the client-side signal this
/// exists for would be buried in noise the moment it mattered most. It stays
/// reported, though — a prefetch that waited half a minute says the pool is
/// oversubscribed, which is worth knowing — just at a threshold that only fires
/// when the queue has genuinely stopped moving.
const LONG_BACKGROUND_WAIT_MS: u64 = 30_000;

/// Report a job that queued for a long time *together with what it queued
/// behind*. `queue_wait_ms` on the finished job says a segment waited; only the
/// composition of the queue at the moment it was finally dispatched says what it
/// waited behind.
///
/// Read `inflight_background` for "this client waited behind speculation" — that
/// is now the ONLY way it can happen. Queued speculation can no longer be ahead
/// of a client, because selection is tier-absolute (see `next_to_dispatch`), so
/// a large `pending_background` is speculative work being held back, which is
/// the system working rather than the scheduling defect the same number used to
/// mean. `pending_interactive` + `inflight_interactive` remain the genuine
/// overload reading.
fn warn_if_long_wait(
    state: &SchedState,
    job_id: JobId,
    device: DeviceId,
    ctx: &JobCtx,
    dispatched_at: Instant,
) {
    let waited_ms = dispatched_at
        .saturating_duration_since(ctx.enqueued)
        .as_millis() as u64;
    let threshold_ms = match ctx.class {
        JobClass::Interactive => LONG_WAIT_MS,
        JobClass::Background => LONG_BACKGROUND_WAIT_MS,
    };
    if waited_ms < threshold_ms {
        return;
    }
    let (mut pend_i, mut pend_b) = (0usize, 0usize);
    for (_, c) in state.pending.iter() {
        match c.class {
            JobClass::Interactive => pend_i += 1,
            JobClass::Background => pend_b += 1,
        }
    }
    let (mut run_i, mut run_b) = (0usize, 0usize);
    for c in state.inflight.values() {
        match c.class {
            JobClass::Interactive => run_i += 1,
            JobClass::Background => run_b += 1,
        }
    }
    tracing::warn!(
        %job_id,
        %device,
        class = %ctx.class,
        waited_ms,
        // Which bar this line cleared. Without it a reader cannot tell a
        // 4 s client wait from a 4 s prefetch wait that simply did not warn,
        // and would read the absence of speculative lines as an absence of
        // speculative waits.
        threshold_ms,
        retries = ctx.retries,
        pending_interactive = pend_i,
        pending_background = pend_b,
        inflight_interactive = run_i,
        inflight_background = run_b,
        live_streams = state.live.load(Ordering::Relaxed),
        input = %ctx.input.display(),
        "transcode job waited a long time for a device permit"
    );
}

/// Open the job's span and record where it was placed — including whether its
/// SOURCE DECODE landed on the GPU.
///
/// Both dispatch paths (first attempt and a queued job draining onto a freed
/// permit) go through here, so a job that waited is described exactly like one
/// that did not.
///
/// The decode verdict comes from the same [`crate::DecodeAccel`] the argv
/// builder emits `-hwaccel` from, so what the dashboard reports and what
/// ffmpeg is told are one decision. Before this, the only thing recorded was
/// the DEVICE — and a job on `Nvenc:0` may still decode every frame in
/// software, which is indistinguishable in a log that names the device alone.
///
/// At INFO because that is the level the deployment runs at. The line naming
/// the winning device sat at DEBUG while prod placed 60% of its segments on
/// the CPU, so the record that would have said so was never emitted.
fn record_placement(
    job_id: JobId,
    dev: DeviceId,
    ctx: &JobCtx,
    candidates: &[DeviceId],
) -> tracing::Span {
    let decode = crate::DecodeAccel::of(&ctx.opts, dev);
    // `queue_wait_ms`/`encode_ms`/`out_bytes`/`outcome` are declared empty and
    // recorded when the job finishes, so one span carries the whole life of a
    // segment: what it was, where it went, how long it waited, how it ended.
    let span = tracing::info_span!(
        "transcode_job",
        job_id = %job_id,
        device = %dev,
        class = %ctx.class,
        sink = ctx.sink.label(),
        decode_accel = %decode,
        decode_on_gpu = decode.is_gpu(),
        video = ?ctx.opts.video,
        audio = ?ctx.opts.audio,
        container = ?ctx.opts.container,
        seek_secs = ctx.opts.start_position_ticks as f64 / 10_000_000.0,
        dur_secs = ctx.opts.duration_ticks.map(|t| t as f64 / 10_000_000.0),
        retries = ctx.retries,
        peer_jobs = tracing::field::Empty,
        background_peers = tracing::field::Empty,
        queue_wait_ms = tracing::field::Empty,
        encode_ms = tracing::field::Empty,
        out_bytes = tracing::field::Empty,
        outcome = tracing::field::Empty,
        input = %ctx.input.display(),
    );
    // Which device won and what it beat. A deployment silently serving
    // everything from the CPU — because the GPU is in cooldown or was excluded
    // by an earlier transient failure — is indistinguishable from one with no
    // GPU at all unless the losing candidates are named alongside the winner.
    span.in_scope(|| {
        tracing::info!(
            candidates = ?candidates,
            excluded = ?ctx.excluded,
            "transcode dispatch"
        );
    });
    metrics::counter!(
        "pharos_transcode_decode_accel_total",
        "verdict" => decode.label(),
        "device" => dev.to_string(),
        "class" => ctx.class.label(),
    )
    .increment(1);
    span
}

/// The devices a job may actually be placed on: eligible, minus already-tried,
/// with the shared-init fMP4 pin applied.
///
/// Spec 003 — a shared-init fMP4 rendition must come from ONE encoder, so it
/// does not get a choice of devices. The device is a pure function of the
/// rendition (see `DeviceTable::rendition_device`), which keeps the answer
/// stable across a restart; an in-memory pin would not, and a rendition
/// re-pinned mid-playback serves segments that no longer match the client's
/// init (issue #114 — undecodable video, served with a 200).
///
/// Cooldown deliberately does NOT re-route it. Spilling to a second encoder is
/// exactly the failure this prevents, so an unavailable device FAILS the request
/// instead (`Err` here): the client restarts the stream and re-fetches an init
/// that matches whatever produces it next. A visible stall that recovers beats
/// silent corruption.
///
/// Shared by BOTH dispatch paths, and that is the point rather than tidiness.
/// `device_supports` deliberately keeps hardware eligible for H264+fMP4 — the
/// one-encoder guarantee is enforced by this pin, not by excluding the GPU — so
/// a path that skips the pin sees a wide `full_eligible` and will happily hand a
/// pinned rendition to a second encoder. Which is what the drain path did: an
/// fMP4 job whose pinned device was busy fell through to the queue, and drained
/// onto whatever had a free permit. Browser H264 is all CMAF and its prefetch is
/// speculative, so once speculative work could queue at all, the drain path
/// became the DOMINANT producer of fMP4 segments — every one of them able to
/// spill. The pin therefore lives here, where neither path can forget it.
///
/// This runs on EVERY examination of a job, including a queued job
/// re-examined on each drain pass — so it does not itself record `Followed` /
/// `Unresolved` (see [`PinOutcome`]'s docs). It returns the outcome the pin
/// resolved to instead, and the caller records it only if this examination
/// ends in an actual dispatch. `Invalidated` is the exception: it IS terminal
/// here (the job fails and never reaches a dispatch point), so it is recorded
/// immediately, same as before.
fn candidates_for(
    state: &SchedState,
    job_id: JobId,
    ctx: &JobCtx,
    full_eligible: &[DeviceId],
) -> Result<(SmallVec<[DeviceId; 5]>, Option<PinOutcome>), SchedError> {
    let mut pin_outcome = None;
    let pinned = if crate::device::shared_init_fmp4(&ctx.opts) {
        let key = crate::options::RenditionKey::new(&ctx.input, &ctx.opts);
        match state.devices.rendition_device(&ctx.opts, key.value()) {
            Some(d) => {
                if !full_eligible.contains(&d) {
                    PinOutcome::Invalidated.record();
                    tracing::warn!(
                        %job_id,
                        rendition = %key.short(),
                        device = %d,
                        "rendition device unavailable (cooldown); failing rather than spilling to another encoder"
                    );
                    return Err(SchedError::Failed(WorkerError::Other(format!(
                        "rendition device {d} unavailable; refusing to mix encoders under one init"
                    ))));
                }
                pin_outcome = Some(PinOutcome::Followed);
                Some(d)
            }
            None => {
                // Previously silent. Without it the counter cannot be read as a
                // total: `followed + invalidated` was always short by however
                // many shared-init jobs resolved to no device, and there was no
                // way to tell that from the metric.
                pin_outcome = Some(PinOutcome::Unresolved);
                None
            }
        }
    } else {
        None
    };
    // A pinned rendition has exactly one candidate and never widens.
    let candidates = match pinned {
        Some(d) => full_eligible
            .iter()
            .copied()
            .filter(|c| *c == d && !ctx.excluded.contains(c))
            .collect(),
        None => full_eligible
            .iter()
            .copied()
            .filter(|d| !ctx.excluded.contains(d))
            .collect(),
    };
    Ok((candidates, pin_outcome))
}

/// Try to dispatch `ctx` to its best eligible device; queue if all
/// permits are busy; fail if no device can ever take it.
///
/// It does NOT check staleness, and cannot need to. `is_stale` requires the job
/// to have been submitted at or ahead of its stream's playhead, and a job's
/// arrival IS its submission: `playhead_at_submit` is read one statement
/// earlier, from the same actor turn, so a job arriving behind its playhead was
/// submitted behind it. A `Background` job that arrives behind its stream's
/// playhead is therefore never stale by construction — it is a deliberate
/// backward guess, which is exactly the SyncPlay seek prewarm, and dropping it
/// here would delete the feature on the one path where a free permit would
/// otherwise have served it immediately.
fn place(state: &mut SchedState, job_id: JobId, mut ctx: JobCtx, self_tx: &mpsc::Sender<SchedMsg>) {
    // Caller gone (client seeked/disconnected → dropped the `submit().await`
    // and its oneshot receiver): don't spend a worker on a segment nobody is
    // waiting for. This is the post-seek contention fix — a dead prefetch job
    // must not sit ahead of the seek-target segment in a device queue.
    if ctx.reply.is_closed() {
        record_queue_outcome(&ctx, QueueOutcome::Abandoned);
        return;
    }
    let now = Instant::now();
    let full_eligible = state.devices.eligible_for(&ctx.opts, now);
    if full_eligible.is_empty() {
        // No supporting device at all (e.g. cooldown could hide all HW
        // but CPU always supports; truly empty ⇒ unsupported target).
        record_queue_outcome(&ctx, QueueOutcome::Failed);
        let _ = ctx.reply.send(Err(SchedError::Unsupported));
        return;
    }
    // Candidate devices = eligible minus already-tried, with the shared-init
    // fMP4 pin applied. See `candidates_for`. `pin_outcome` is only recorded
    // if THIS examination ends in an actual dispatch below — see
    // `PinOutcome`'s docs.
    let (candidates, pin_outcome) = match candidates_for(state, job_id, &ctx, &full_eligible) {
        Ok(c) => c,
        Err(e) => {
            record_queue_outcome(&ctx, QueueOutcome::Failed);
            let _ = ctx.reply.send(Err(e));
            return;
        }
    };
    if candidates.is_empty() {
        // Every supporting device has been tried + failed transiently.
        record_queue_outcome(&ctx, QueueOutcome::Failed);
        let err = ctx
            .last_error
            .clone()
            .unwrap_or(WorkerError::Other("no device left".into()));
        tracing::warn!(
            %job_id,
            excluded = ?ctx.excluded,
            error = %err,
            "transcode job has no device left to try"
        );
        let _ = ctx.reply.send(Err(SchedError::Failed(err)));
        return;
    }

    // Speculative work waits for nobody, so it must not make anybody wait.
    // Prefetch is dispatched *before* the segment the client is blocked on (it
    // pipelines, by design), and once shared one FIFO with that request: a
    // handful of requests could therefore bury a client's own segment under
    // tens of speculative encodes, turning a 3 s encode into a 90 s wait.
    // Background work therefore takes a permit only out of capacity above the
    // reserve, which is what keeps one within reach of a client arriving a
    // moment later.
    //
    // Refused here it now WAITS rather than dying: the reserve says "not out of
    // this permit", not "not ever", and the queue re-ranks it by urgency on
    // every completion. Unless it really is "not ever" — a pool that cannot
    // hold the reserve and a job at once has nothing to wait FOR.
    if ctx.class == JobClass::Background {
        match background_admission(state, &candidates) {
            BackgroundAdmission::Dispatch => {}
            BackgroundAdmission::Wait => {
                tracing::debug!(
                    %job_id,
                    candidates = ?candidates,
                    headroom = state.cfg.background_headroom,
                    "speculative transcode queued: no spare capacity above the reserve"
                );
                queue_or_refuse(state, job_id, ctx);
                return;
            }
            BackgroundAdmission::Never => {
                let err = background_never_error(&ctx);
                tracing::debug!(
                    %job_id,
                    candidates = ?candidates,
                    headroom = state.cfg.background_headroom,
                    last_error = ?ctx.last_error,
                    load_shed = matches!(err, SchedError::Busy),
                    "speculative transcode admitted to neither wait nor dispatch: \
                     reserve cannot fit in this candidate pool"
                );
                record_queue_outcome(&ctx, QueueOutcome::Shed);
                let _ = ctx.reply.send(Err(err));
                return;
            }
        }
    }

    for dev in candidates.iter().copied() {
        let Some(slot) = state.devices.slot(dev) else {
            continue;
        };
        // A free permit is not the same thing as free throughput. Skip a
        // device already carrying a client's segment plus its speculative
        // allowance, and let this job try the next device — or be shed below.
        // This is what `background_headroom` intends but cannot deliver on its
        // own: reserving a permit gets a client STARTED, not finished.
        if ctx.class == JobClass::Background && crowds_a_client(state, dev) {
            continue;
        }
        if let Ok(permit) = slot.sem.clone().try_acquire_owned() {
            // This examination is ending in an actual dispatch — the one
            // point `Followed`/`Unresolved` are recorded (see `PinOutcome`'s
            // docs). A job that only queues here never reaches this line, so
            // a queued pinned job re-examined on later drains is not
            // double-counted.
            if let Some(outcome) = pin_outcome {
                outcome.record();
            }
            record_dispatch(state, &ctx);
            let span = record_placement(job_id, dev, &ctx, &candidates);
            let worker = state.idle.pop();
            let worker_id = WorkerId(state.next_worker);
            state.next_worker += 1;
            let spec = JobSpec {
                job_id,
                input: ctx.input.clone(),
                opts: ctx.opts.clone(),
                device: dev,
                sink: to_output_sink(&ctx.sink),
            };
            let dispatched_at = Instant::now();
            // The long-wait warning belongs INSIDE the span: the queue
            // composition it reports is only actionable beside what the job
            // was and where it landed.
            span.in_scope(|| warn_if_long_wait(state, job_id, dev, &ctx, dispatched_at));
            ctx.dispatched = Some(dispatched_at);
            // Told to the SUBMITTER, not just recorded here: from this line the
            // permit and the worker are the detached run task's, and no caller
            // can hand them back by walking away. See `JobSlot::is_dispatched`.
            if let Some(slot) = &ctx.assigned {
                slot.mark_dispatched();
            }
            ctx.device = Some(dev);
            // Counted BEFORE this job joins `inflight`, so it is peers, not
            // occupancy: a job that runs alone reports 0.
            ctx.peer_jobs = peers_on(state, dev);
            ctx.background_peers = background_peers_on(state, dev);
            span.record("peer_jobs", ctx.peer_jobs);
            span.record("background_peers", ctx.background_peers);
            ctx.span = span.clone();
            state.inflight.insert(job_id, ctx);
            spawn_run_task(
                state.spawner.clone(),
                worker,
                worker_id,
                permit,
                spec,
                dev,
                self_tx.clone(),
                span,
            );
            return;
        }
    }

    // Every candidate permit is busy (or, for speculative work, held by a
    // client past what this device has earned) → wait for one.
    queue_or_refuse(state, job_id, ctx);
}

/// Drop queued jobs whose caller has gone.
///
/// `try_place_no_queue` already declines to dispatch one, but only a job that
/// gets SELECTED ever reaches it — and under saturation speculative work is
/// never selected (tier is absolute) and the drain loop exits the moment no
/// permit is free. So abandoned prefetch accumulated in `pending` and was reaped
/// only when the pool went quiet, which is precisely when the queue's bound does
/// not matter. Every seek and every track swap orphans a window of it.
///
/// That bound is shared now. While `pending` held interactive work only,
/// `pending_cap` was pure client backpressure; with speculative work in the same
/// queue, a full queue of work NOBODY is waiting for replies `Busy` to a client.
///
/// Swept here rather than by reserving a slice of `pending_cap` for interactive
/// work: a reserve would leave the dead entries in place, still occupying their
/// own share, still growing on every seek, and would add a second tunable to
/// keep in step with the first. This removes the cause. It runs before the
/// permit check, so a drain that can place nothing still reaps — and drains fire
/// on every completion, so under saturation it runs at segment rate, exactly
/// when it is needed.
fn reap_abandoned(state: &mut SchedState) {
    let before = state.pending.len();
    state.pending.retain(|(_, ctx)| {
        if ctx.reply.is_closed() {
            record_queue_outcome(ctx, QueueOutcome::Abandoned);
            false
        } else {
            true
        }
    });
    let dropped = before - state.pending.len();
    if dropped > 0 {
        tracing::debug!(
            dropped,
            pending = state.pending.len(),
            "dropped queued transcode jobs whose caller had gone"
        );
    }
}

/// Drop queued speculation the viewer has OUTRUN, wherever it sits in the
/// order.
///
/// Judged at examination first, in `try_place_no_queue`, which was wrong for
/// the same reason `reap_abandoned` is a sweep. `next_to_dispatch` sorts
/// outrun work last by design, so a job in that band is examined only once it
/// becomes the minimum — and under sustained load every freed permit goes to a
/// live job and `drain_pending` exits with the passed band untouched. Stale
/// jobs were therefore never examined, never dropped, and went on occupying
/// `pending_cap`, which V58 calls client backpressure in the very paragraph
/// that makes `reap_abandoned` unconditional. The eviction backstop only fires
/// AT `pending_cap`, i.e. after the accumulation V58 forbids has already
/// happened.
///
/// The signal consequence is the one that decides it. A stale job that
/// eventually leaves by eviction is counted `evicted`, not `stale` — so the
/// query the arm exists for ("`stale` dominating `dispatched` says prefetch
/// depth is tuned too far ahead") was biased downward precisely under
/// saturation, the only load at which anyone reads it. A metric arm that
/// under-reports in its own diagnostic regime is not a contract.
///
/// Running it unconditionally makes the passed band unreachable from
/// `next_to_dispatch` — see that function, which now says so rather than
/// describing a path nothing takes. The band is still read, by
/// `queue_or_refuse`: eviction runs on ARRIVAL, and an interactive arrival's
/// own `note_playhead` is what turns incumbents stale a few lines before the
/// victim is chosen.
///
/// Which BOUNDS that bias rather than closing it. A job that turns stale
/// between two drains and is chosen as a victim before the next one leaves as
/// `evicted`, exactly as before — the sweep cannot see a job that is no longer
/// in `pending`. What changed is the window: it was "until the pool goes quiet"
/// and is now "until the next freed permit", so `stale` no longer
/// systematically under-reports under saturation. Read the two arms together
/// when the question is how much speculation is being wasted.
///
/// Cheap for the same reason `reap_abandoned` is: O(n) over `pending_cap` on
/// an event that already happens at segment rate, and it recomputes the
/// distance rather than trusting a cached one, so a playhead that moved while
/// the queue sat still is seen the first time a permit frees.
fn reap_stale(state: &mut SchedState) {
    // Taken out so `is_stale` can borrow the rest of the state. Nothing it
    // reads lives in `pending`.
    let pending = std::mem::take(&mut state.pending);
    let before = pending.len();
    let mut kept: VecDeque<(JobId, JobCtx)> = VecDeque::with_capacity(before);
    for (job_id, ctx) in pending {
        if !is_stale(state, &ctx) {
            kept.push_back((job_id, ctx));
            continue;
        }
        tracing::debug!(
            %job_id,
            stream = ctx.stream.0,
            segment = ctx.segment,
            playhead = state.playheads.get(&ctx.stream).map(|(h, _)| *h),
            distance = lookahead_distance(state, &ctx),
            waited_ms = Instant::now()
                .saturating_duration_since(ctx.enqueued)
                .as_millis() as u64,
            input = %ctx.input.display(),
            "speculative transcode dropped: the viewer has played past it"
        );
        record_queue_outcome(&ctx, QueueOutcome::Stale);
        // Resolved, not merely dropped: an evicted or dropped job whose
        // `oneshot` is discarded leaves its caller's `submit().await` resolving
        // through a `RecvError` with nothing to say. Reported as `Busy`, not as
        // a failure — nothing broke, the work stopped being wanted.
        let _ = ctx.reply.send(Err(SchedError::Busy));
    }
    state.pending = kept;
    let dropped = before - state.pending.len();
    if dropped > 0 {
        tracing::debug!(
            dropped,
            pending = state.pending.len(),
            "dropped queued speculative transcodes their viewers had outrun"
        );
    }
}

/// On a freed permit, dispatch queued work in urgency order until nothing else
/// fits.
///
/// SELECTS rather than pops: order in `pending` is arrival order, and arrival
/// order is not urgency order. A job passed over stays queued and is
/// reconsidered — at a freshly computed distance — on the next free permit.
///
/// It deliberately does NOT stop at the first job it could not place. That
/// shortcut reads as "the most urgent candidate found no permit, so no less
/// urgent one will either", and that reasoning does not hold here: jobs have
/// different ELIGIBLE DEVICE SETS. A VP9 job is CPU-only, and a job that has
/// already failed transiently carries its own `excluded` list — so a free NVENC
/// permit can sit idle behind a queued job that could never have used it. The
/// loop instead runs while ANY permit is free, which is the only condition
/// under which no job of any class can be placed, and terminates because every
/// iteration removes exactly one entry from `pending`. Worst case is O(n²) key
/// comparisons with n ≤ `pending_cap`, on an event that happens at segment rate.
///
/// Passed-over jobs go back in FRONT of what was never examined, not behind it.
/// Selection walks Interactive in arrival order (tier is absolute, then queue
/// index), so the examined interactive jobs are always an arrival-order prefix
/// of the queued ones; putting them back at the front is what keeps
/// `next_to_dispatch`'s FIFO tiebreak honest on the next pass. Appending them
/// instead would push the client who waited longest behind the one who waited
/// least, one drain at a time.
fn drain_pending(state: &mut SchedState, self_tx: &mpsc::Sender<SchedMsg>) {
    reap_abandoned(state);
    reap_stale(state);
    let mut passed_over: VecDeque<(JobId, JobCtx)> = VecDeque::new();
    while any_permit_free(state) {
        let Some(idx) = next_to_dispatch(state) else {
            break;
        };
        let Some((job_id, ctx)) = state.pending.remove(idx) else {
            break;
        };
        try_place_no_queue(state, job_id, ctx, self_tx, &mut passed_over);
    }
    passed_over.append(&mut state.pending);
    state.pending = passed_over;
}

/// Like `place` but never re-queues internally — a job that can't get a
/// permit is pushed into `requeue` (preserving order) instead, so
/// `drain_pending` doesn't recurse or reorder.
///
/// Every exit from this function resolves the caller's reply or returns the job
/// to `requeue`; none of them leaves a `submit().await` hanging.
fn try_place_no_queue(
    state: &mut SchedState,
    job_id: JobId,
    mut ctx: JobCtx,
    self_tx: &mpsc::Sender<SchedMsg>,
    requeue: &mut VecDeque<(JobId, JobCtx)>,
) {
    // Drop a queued job whose caller has gone (see `place`): on a freed permit
    // we dispatch the seek-target instead of resurrecting dead prefetch work.
    if ctx.reply.is_closed() {
        record_queue_outcome(&ctx, QueueOutcome::Abandoned);
        return;
    }
    // No staleness check here, and the asymmetry with the line above is the
    // point. Abandonment is driven from OUTSIDE the actor: a caller can drop
    // its `submit().await` at any instant, including halfway through a drain,
    // so it has to be re-checked on every examination. Staleness is driven from
    // INSIDE it — a playhead moves only in a message arm, whether that is an
    // interactive `Submit` or a `Playhead` reporting a cache hit — and the actor
    // processes no messages mid-drain, so no job can become stale between
    // `reap_stale` and here. Checking it again would be a condition that can no
    // longer change.
    let now = Instant::now();
    let full_eligible = state.devices.eligible_for(&ctx.opts, now);
    // The SAME candidate set the arrival path computes, pin included. A job
    // that queued because its pinned device was busy must drain onto that
    // device or onto nothing — `candidates_for`'s `Err` (the pinned device went
    // into cooldown while the job waited) fails the request here exactly as it
    // does on arrival, rather than falling back to the queue: a stall the
    // client recovers from beats a segment no client can decode.
    // `pin_outcome` is only recorded if THIS examination ends in an actual
    // dispatch below — a queued pinned job re-examined on every drain must
    // not record `Followed` once per examination (see `PinOutcome`'s docs).
    let (candidates, pin_outcome) = match candidates_for(state, job_id, &ctx, &full_eligible) {
        Ok(c) => c,
        Err(e) => {
            record_queue_outcome(&ctx, QueueOutcome::Failed);
            let _ = ctx.reply.send(Err(e));
            return;
        }
    };
    if candidates.is_empty() {
        record_queue_outcome(&ctx, QueueOutcome::Failed);
        let err = ctx
            .last_error
            .clone()
            .unwrap_or(WorkerError::Other("no device left".into()));
        // Same line, same fields, as the arrival path's identical exit. This
        // one was silent, and it is the path the deployment's dominant fMP4
        // producer takes: a queued job whose last candidate device fell away
        // failed the client with nothing in the record naming WHICH devices it
        // had already burned. Symmetry is the rule (ODD), and the asymmetry
        // here was between two copies of the same decision.
        tracing::warn!(
            %job_id,
            excluded = ?ctx.excluded,
            error = %err,
            "queued transcode job has no device left to try"
        );
        let _ = ctx.reply.send(Err(SchedError::Failed(err)));
        return;
    }
    // The same reserve the arrival path applies, recomputed against the device
    // table as it is now — and, because `candidates` is now the pinned set,
    // counted over the permits this job can actually use. Counted over the
    // unpinned set it reported another device's free permits as this job's
    // headroom, so a pinned job was admitted against capacity it could never
    // reach. Speculative work reached this path for the first time in this
    // task, so without the reserve at all a queued prefetch would drain
    // straight onto the permit `background_headroom` exists to hold open for a
    // client.
    if ctx.class == JobClass::Background {
        match background_admission(state, &candidates) {
            BackgroundAdmission::Dispatch => {}
            BackgroundAdmission::Wait => {
                requeue.push_back((job_id, ctx));
                return;
            }
            // A cooldown can narrow a queued job's candidate set until the
            // reserve no longer fits in it. Re-queueing then parks it forever
            // on a condition only a device coming back can meet; shed it,
            // exactly as arrival would have — same cause/class distinction as
            // `place` (`background_never_error`): a job that queued after a
            // transient failure narrowed it to nothing must report THAT, not
            // a load-shed classification that discards it.
            BackgroundAdmission::Never => {
                let err = background_never_error(&ctx);
                tracing::debug!(
                    %job_id,
                    candidates = ?candidates,
                    headroom = state.cfg.background_headroom,
                    last_error = ?ctx.last_error,
                    load_shed = matches!(err, SchedError::Busy),
                    "queued speculative transcode admitted to neither wait nor dispatch: \
                     reserve cannot fit in this candidate pool"
                );
                record_queue_outcome(&ctx, QueueOutcome::Shed);
                let _ = ctx.reply.send(Err(err));
                return;
            }
        }
    }
    for dev in candidates.iter().copied() {
        let Some(slot) = state.devices.slot(dev) else {
            continue;
        };
        // ...and the same crowding gate, for the same reason: a free permit is
        // not free throughput, and a queued prefetch that drains onto the
        // device encoding a client's segment slows that client exactly as much
        // as one admitted on arrival would have.
        if ctx.class == JobClass::Background && crowds_a_client(state, dev) {
            continue;
        }
        if let Ok(permit) = slot.sem.clone().try_acquire_owned() {
            // Same rule as `place`: only an examination that actually
            // dispatches records the pin outcome — or the queue outcome.
            if let Some(outcome) = pin_outcome {
                outcome.record();
            }
            record_dispatch(state, &ctx);
            let span = record_placement(job_id, dev, &ctx, &candidates);
            let worker = state.idle.pop();
            let worker_id = WorkerId(state.next_worker);
            state.next_worker += 1;
            let spec = JobSpec {
                job_id,
                input: ctx.input.clone(),
                opts: ctx.opts.clone(),
                device: dev,
                sink: to_output_sink(&ctx.sink),
            };
            let dispatched_at = Instant::now();
            // The long-wait warning belongs INSIDE the span: the queue
            // composition it reports is only actionable beside what the job
            // was and where it landed.
            span.in_scope(|| warn_if_long_wait(state, job_id, dev, &ctx, dispatched_at));
            ctx.dispatched = Some(dispatched_at);
            // Same as `place`, and needed at BOTH: a job that waited in the
            // queue and then drained onto a permit is exactly as unreclaimable
            // as one dispatched on arrival, and the queue is where speculative
            // work spends most of its life.
            if let Some(slot) = &ctx.assigned {
                slot.mark_dispatched();
            }
            ctx.device = Some(dev);
            // B178 — same stamping as `place`. A job dispatched off the queue
            // used to report zero peers regardless, and the queue is what runs
            // when the device is busiest.
            ctx.peer_jobs = peers_on(state, dev);
            ctx.background_peers = background_peers_on(state, dev);
            span.record("peer_jobs", ctx.peer_jobs);
            span.record("background_peers", ctx.background_peers);
            ctx.span = span.clone();
            state.inflight.insert(job_id, ctx);
            spawn_run_task(
                state.spawner.clone(),
                worker,
                worker_id,
                permit,
                spec,
                dev,
                self_tx.clone(),
                span,
            );
            return;
        }
    }
    // No candidate had a permit to give (or every one that did is already
    // carrying a client past what it has earned). Back into the queue.
    requeue.push_back((job_id, ctx));
}

fn to_output_sink(sink: &SinkRequest) -> OutputSink {
    match sink {
        SinkRequest::FileDirect { out_path } => OutputSink::FileDirect {
            path: out_path.clone(),
        },
        // LiveStream is dispatched via `submit_live` (OutputSink::Stdout),
        // never through the segment `place` path; map defensively.
        SinkRequest::LiveStream => OutputSink::Stdout,
    }
}

/// Detached encode driver. Owns the permit (RAII release) + the worker.
/// Spawns a worker if none was reused. Always reports `JobFinished` so
/// the actor can resolve the reply — even on spawn failure.
#[allow(clippy::too_many_arguments)]
fn spawn_run_task(
    spawner: Arc<dyn WorkerSpawner>,
    worker: Option<Box<dyn Worker>>,
    worker_id: WorkerId,
    permit: OwnedSemaphorePermit,
    spec: JobSpec,
    device: DeviceId,
    self_tx: mpsc::Sender<SchedMsg>,
    // The job's span, so a worker-spawn failure or a mid-job death is
    // reported with the placement facts attached rather than a bare job id.
    span: tracing::Span,
) {
    let job_id = spec.job_id;
    tokio::spawn(async move {
        let _guard = span.enter();
        let worker = match worker {
            Some(w) => w,
            None => match spawner.spawn(worker_id).await {
                Ok(w) => w,
                Err(e) => {
                    // Couldn't spawn — release permit, report as a death
                    // so the actor retries (bounded) or fails the reply.
                    drop(permit);
                    let _ = self_tx
                        .send(SchedMsg::JobFinished {
                            job_id,
                            device,
                            result: WorkerRunResult::Died,
                            worker: None,
                        })
                        .await;
                    tracing::warn!(%job_id, %device, error = %e, "worker spawn failed");
                    return;
                }
            },
        };
        // Run the worker on its own task so a PANIC inside arbitrary
        // worker-impl code (FFI/ffmpeg driver) becomes a JoinError we map
        // to `Died` rather than unwinding this task and leaking the
        // caller's reply oneshot (which lives in the actor's `inflight`
        // map, not here). Without this, a worker panic would hang
        // `submit()` forever — the module-level "reply resolved exactly
        // once" invariant depends on this.
        let run_handle = tokio::spawn(async move {
            let mut worker = worker;
            let result = worker.run(spec).await;
            (worker, result)
        });
        let (returned, result) = match run_handle.await {
            Ok((w, WorkerRunResult::Died)) => {
                drop(w);
                (None, WorkerRunResult::Died)
            }
            Ok((w, r)) => (Some(w), r),
            Err(join_err) => {
                // Panic or cancellation inside the worker run — treat as a
                // death so the bounded-retry path resolves the reply.
                tracing::warn!(%job_id, %device, error = %join_err, "worker run task aborted/panicked");
                (None, WorkerRunResult::Died)
            }
        };
        // Release the permit BEFORE notifying so the freed slot is
        // visible when the actor drains the pending queue.
        drop(permit);
        let _ = self_tx
            .send(SchedMsg::JobFinished {
                job_id,
                device,
                result,
                worker: returned,
            })
            .await;
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hwaccel::HwAccel;
    use crate::options::{AudioCodec, Container, VideoCodec};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    #[test]
    fn only_a_file_sink_claims_to_have_measured_its_output() {
        // `out_bytes: 0` from a live-stream job means "the worker never saw the
        // bytes", not "the encode produced nothing". Conflating them cost a
        // wrong lead during the Ghost in the Shell investigation.
        let file = SinkRequest::FileDirect {
            out_path: PathBuf::from("/tmp/x.ts"),
        };
        assert!(file.measures_out_bytes());
        assert!(!SinkRequest::LiveStream.measures_out_bytes());
        assert_eq!(file.label(), "file");
        assert_eq!(SinkRequest::LiveStream.label(), "live_stream");
        assert_ne!(file.label(), SinkRequest::LiveStream.label());
    }

    fn h264() -> TranscodeOptions {
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

    fn file_sink() -> SinkRequest {
        SinkRequest::FileDirect {
            out_path: PathBuf::from("/dev/null"),
        }
    }

    /// The tag a test gave a job, recovered from its input path.
    fn job_name(spec: &JobSpec) -> String {
        spec.input
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    type DelayFn = dyn Fn(&JobSpec) -> Duration + Send + Sync;

    /// Spawner whose per-job duration is a function of the job.
    ///
    /// [`ScriptedSpawner`]'s single delay cannot express "this device's permit
    /// frees while that one is still held", which is the only way to make the
    /// drain run against a PARTLY busy pool — and a partly busy pool is where
    /// every candidate-set bug lives.
    struct VariableSpawner(Arc<DelayFn>);

    impl WorkerSpawner for VariableSpawner {
        fn spawn(&self, id: WorkerId) -> SpawnFuture {
            let f = self.0.clone();
            Box::pin(async move { Ok(Box::new(VariableWorker { id, f }) as Box<dyn Worker>) })
        }
    }

    struct VariableWorker {
        id: WorkerId,
        f: Arc<DelayFn>,
    }

    impl Worker for VariableWorker {
        fn id(&self) -> WorkerId {
            self.id
        }
        fn run<'a>(&'a mut self, job: JobSpec) -> RunFuture<'a> {
            let f = self.f.clone();
            Box::pin(async move {
                let d = f(&job);
                if !d.is_zero() {
                    tokio::time::sleep(d).await;
                }
                WorkerRunResult::Done { out_bytes: 1 }
            })
        }
    }

    type ScriptFn = dyn Fn(WorkerId, &JobSpec) -> WorkerRunResult + Send + Sync;

    /// Spawner whose workers run a scripted outcome after a fixed delay.
    struct ScriptedSpawner {
        f: Arc<ScriptFn>,
        delay: Duration,
        spawned: Arc<AtomicU64>,
    }

    impl ScriptedSpawner {
        fn new(
            delay: Duration,
            f: impl Fn(WorkerId, &JobSpec) -> WorkerRunResult + Send + Sync + 'static,
        ) -> (Arc<Self>, Arc<AtomicU64>) {
            let spawned = Arc::new(AtomicU64::new(0));
            (
                Arc::new(Self {
                    f: Arc::new(f),
                    delay,
                    spawned: spawned.clone(),
                }),
                spawned,
            )
        }
    }

    impl WorkerSpawner for ScriptedSpawner {
        fn spawn(&self, id: WorkerId) -> SpawnFuture {
            self.spawned.fetch_add(1, Ordering::SeqCst);
            let f = self.f.clone();
            let delay = self.delay;
            Box::pin(
                async move { Ok(Box::new(ScriptedWorker { id, f, delay }) as Box<dyn Worker>) },
            )
        }
    }

    struct ScriptedWorker {
        id: WorkerId,
        f: Arc<ScriptFn>,
        delay: Duration,
    }

    impl Worker for ScriptedWorker {
        fn id(&self) -> WorkerId {
            self.id
        }
        fn run<'a>(&'a mut self, job: JobSpec) -> RunFuture<'a> {
            let f = self.f.clone();
            let delay = self.delay;
            let id = self.id;
            Box::pin(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                f(id, &job)
            })
        }
    }

    fn table() -> DeviceTable {
        DeviceTable::from_probe(
            &[
                (DeviceId::hw(HwAccel::Nvenc, 0), 2),
                (DeviceId::hw(HwAccel::Vaapi, 0), 1),
            ],
            2,
        )
    }

    #[tokio::test]
    async fn dispatch_completes_on_best_device() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 42,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let done = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        assert_eq!(done.device, DeviceId::hw(HwAccel::Nvenc, 0)); // best-first
        assert_eq!(done.out_bytes, 42);
    }

    fn cmaf() -> TranscodeOptions {
        let mut o = h264();
        o.container = Container::Fmp4;
        o
    }

    /// `cmaf()` carries no `duration_ticks` (neither does `h264()`, which it is
    /// built from) — an observation without one is deliberately `Ignored` and
    /// teaches the admission controller nothing. Every test that expects the
    /// controller to learn from a finished segment must go through this helper
    /// instead of a bare `cmaf()`, or the assertion proves nothing. 6 s
    /// segments, matching the value used throughout `admission.rs`'s own tests.
    fn cmaf_with_duration() -> TranscodeOptions {
        let mut o = cmaf();
        o.duration_ticks = Some(6 * 10_000_000);
        o
    }

    /// [`cmaf_with_duration`] with the segment length — and so the controller's
    /// deadline, `margin_ratio × this` — chosen by the caller, for tests that
    /// have to place a scripted encode on one side of it or the other without
    /// sleeping for seconds.
    fn cmaf_lasting(secs: f64) -> TranscodeOptions {
        let mut o = cmaf();
        o.duration_ticks = Some((secs * 10_000_000.0) as u64);
        o
    }

    /// Spec 003 US2 — every segment of one CMAF rendition must come from the
    /// SAME device. Dispatch many segments (they differ only in start position,
    /// so they share a rendition key) and assert they all land together, with
    /// other devices free to tempt the load-balancer.
    ///
    /// If this fails, issue #114 is back: a segment from a second encoder is
    /// undecodable under the init the client already holds.
    #[tokio::test]
    async fn every_segment_of_a_cmaf_rendition_lands_on_one_device() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let mut devices = std::collections::HashSet::new();
        for seg in 0..12u64 {
            let mut o = cmaf();
            o.start_position_ticks = seg * 6 * 10_000_000;
            o.duration_ticks = Some(6 * 10_000_000);
            let done = s
                .submit(
                    PathBuf::from("/m/show.mkv"),
                    o,
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
                .unwrap();
            devices.insert(done.device);
        }
        assert_eq!(
            devices.len(),
            1,
            "a shared-init rendition must not mix encoders; got {devices:?}"
        );
    }

    /// A DIFFERENT rendition of the same file (different audio track) is a
    /// different init, so it is free to resolve elsewhere — the guarantee is
    /// per-rendition, not per-file.
    #[tokio::test]
    async fn a_different_rendition_is_free_to_choose_again() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let a = s
            .submit(
                PathBuf::from("/m/x.mkv"),
                cmaf(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        let mut other = cmaf();
        other.audio_source_stream_index = Some(3);
        let b = s
            .submit(
                PathBuf::from("/m/x.mkv"),
                other,
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        // Both are valid; the point is that each is internally consistent and
        // neither is forced to follow the other.
        assert!(matches!(a.device, DeviceId::Hw { .. }));
        assert!(matches!(b.device, DeviceId::Hw { .. }));
    }

    /// Spec 003 US3 — losing the rendition's device must FAIL, not fall back.
    ///
    /// Falling back is precisely issue #114: the client keeps an init produced
    /// by the first encoder and would silently receive segments from a second.
    /// A visible error makes the player restart the stream and re-fetch an init
    /// that matches. CPU is left deliberately free here, so a spill would
    /// succeed if the guard were absent.
    ///
    /// Disarm by deleting the `!full_eligible.contains(&d)` arm in `place()`
    /// and this goes red — the job completes on CPU.
    #[tokio::test]
    async fn a_cooled_rendition_device_fails_instead_of_spilling_to_another_encoder() {
        let mut t = table();
        let dev = t
            .rendition_device(&cmaf(), {
                use crate::options::RenditionKey;
                RenditionKey::new(std::path::Path::new("/m/show.mkv"), &cmaf()).value()
            })
            .expect("a device");
        assert!(matches!(dev, DeviceId::Hw { .. }), "precondition: hardware");
        t.set_cooldown(dev, Instant::now() + Duration::from_secs(300));

        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(t, spawner, SchedConfig::default());
        let res = s
            .submit(
                PathBuf::from("/m/show.mkv"),
                cmaf(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await;
        match res {
            Err(SchedError::Failed(_)) => {}
            other => panic!(
                "a cooled rendition device must fail, not silently re-encode elsewhere; got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn vp9_lands_on_vaapi_hardware() {
        // VAAPI has a VP9 encoder (`vp9_vaapi`); the scheduler routes a VP9 job
        // to it (hardware) rather than the CPU. NVENC has no VP9 encoder so it is
        // never eligible for a VP9 target.
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let mut o = h264();
        o.video = Some(VideoCodec::Vp9);
        let done = s
            .submit(
                PathBuf::from("/m/x"),
                o,
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        assert_eq!(done.device, DeviceId::hw(HwAccel::Vaapi, 0));
    }

    #[tokio::test]
    async fn live_stream_unsupported_for_now() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let r = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                SinkRequest::LiveStream,
                JobClass::Interactive,
                JobHint::default(),
            )
            .await;
        assert_eq!(r, Err(SchedError::Unsupported));
    }

    #[tokio::test]
    async fn busy_backpressure_when_saturated() {
        // Total permits = 2(nvenc)+1(vaapi)+2(cpu) = 5; pending_cap = 0.
        // Hold jobs with a long delay, fire 6 → the 6th can neither get a
        // permit nor queue → Busy.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(300), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let cfg = SchedConfig {
            pending_cap: 0,
            ..SchedConfig::default()
        };
        let s = TranscodeScheduler::spawn(table(), spawner, cfg);
        let mut handles = Vec::new();
        for _ in 0..6 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/x"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            }));
        }
        let mut busy = 0;
        let mut ok = 0;
        for h in handles {
            match h.await.unwrap() {
                Ok(_) => ok += 1,
                Err(SchedError::Busy) => busy += 1,
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert!(
            busy >= 1,
            "expected at least one Busy under saturation, ok={ok} busy={busy}"
        );
    }

    #[tokio::test]
    async fn job_finished_drains_pending() {
        // pending_cap large; saturate permits with delay so extra jobs
        // queue, then all complete once permits free. Proves the
        // JobFinished edge re-dispatches queued work.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(50), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let mut handles = Vec::new();
        for _ in 0..20 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/x"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            }));
        }
        let mut ok = 0;
        for h in handles {
            if h.await.unwrap().is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 20, "all queued jobs must eventually complete");
    }

    #[tokio::test]
    async fn transient_failure_retries_next_best() {
        // Nvenc always DeviceBusy; everything else Done. Job must land
        // off Nvenc (Vaapi or Cpu) and succeed.
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, spec| {
            if spec.device == DeviceId::hw(HwAccel::Nvenc, 0) {
                WorkerRunResult::Failed(WorkerError::DeviceBusy)
            } else {
                WorkerRunResult::Done { out_bytes: 7 }
            }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let done = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        assert_ne!(done.device, DeviceId::hw(HwAccel::Nvenc, 0));
        assert_eq!(done.out_bytes, 7);
    }

    /// Collects the fields of every event, so a test can assert what the
    /// deployment would actually be able to read.
    #[derive(Clone, Default)]
    struct EventCapture(Arc<Mutex<Vec<(String, String)>>>);

    impl<S> tracing_subscriber::Layer<S> for EventCapture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visit(String);
            impl tracing::field::Visit for Visit {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!(" {}={:?}", f.name(), v));
                }
            }
            let mut v = Visit(String::new());
            event.record(&mut v);
            // Span fields are the point of the exercise: an event is only as
            // debuggable as the span it was emitted inside.
            let mut fields = v.0;
            if let Some(scope) = ctx.event_scope(event) {
                for span in scope {
                    if let Some(f) = span
                        .extensions()
                        .get::<tracing_subscriber::fmt::FormattedFields<
                            tracing_subscriber::fmt::format::DefaultFields,
                        >>()
                    {
                        fields.push_str(&format!(" [{}: {}]", span.name(), f.fields));
                    }
                }
            }
            self.0
                .lock()
                .unwrap()
                .push((event.metadata().name().to_string(), fields));
        }
    }

    /// The placement record is a contract, not a convenience: it is the ONLY
    /// thing that can answer "is this deployment decoding on the GPU?".
    ///
    /// A production incident turned on exactly this gap — 60% of segments were
    /// placed on the CPU while the line naming the winning device sat at
    /// DEBUG, so the deployment's own logs could not distinguish a working GPU
    /// from an idle one. Asserting the fields (not just that something was
    /// logged) is what makes a later refactor unable to quietly drop them.
    #[tokio::test]
    async fn a_dispatch_records_its_device_and_whether_decode_is_on_the_gpu() {
        use tracing_subscriber::layer::SubscriberExt;

        let cap = EventCapture::default();
        // INFO, because that is what the deployment runs at. Without this
        // filter the test would pass with the record at DEBUG — which is
        // precisely the state that made a production incident undiagnosable,
        // so a subscriber that captures every level would assert nothing.
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::INFO)
            // `with_ansi(false)`: the fmt layer is only here to populate the
            // span's `FormattedFields`, and styled output would interleave
            // escape codes between the field names being asserted.
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(std::io::sink),
            )
            .with(cap.clone());

        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 7,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        {
            let _g = tracing::subscriber::set_default(subscriber);
            s.submit(
                PathBuf::from("/m/x.mkv"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        }

        let events = cap.0.lock().unwrap().clone();
        let dispatch = events
            .iter()
            .find(|(_, fields)| fields.contains("transcode dispatch"))
            .unwrap_or_else(|| {
                panic!("no \"transcode dispatch\" event was recorded; events: {events:?}")
            });
        // The table's best device is the GPU, so this job decodes there — and
        // the record must SAY so rather than leaving it to be inferred from
        // the device name (a job on Nvenc:0 can still decode in software).
        assert!(
            dispatch.1.contains("decode_accel=gpu:cuda"),
            "dispatch record must name the decode path, got: {}",
            dispatch.1
        );
        assert!(
            dispatch.1.contains("decode_on_gpu=true"),
            "dispatch record must state whether decode was offloaded, got: {}",
            dispatch.1
        );
        assert!(
            dispatch.1.contains("device=Nvenc:0") && dispatch.1.contains("class=interactive"),
            "dispatch record must carry device + class, got: {}",
            dispatch.1
        );
        // Losing candidates: a CPU-only deployment and one whose GPU is in
        // cooldown look identical without them.
        assert!(
            dispatch.1.contains("candidates="),
            "dispatch record must name what the winner beat, got: {}",
            dispatch.1
        );
    }

    #[tokio::test]
    async fn non_recoverable_failure_returns_error() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| {
            WorkerRunResult::Failed(WorkerError::BadInput("scripted bad input".into()))
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let r = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await;
        assert_eq!(
            r,
            Err(SchedError::Failed(WorkerError::BadInput(
                "scripted bad input".into()
            )))
        );
    }

    #[tokio::test]
    async fn worker_death_retries_and_scheduler_survives() {
        // First run on any device dies once; subsequent runs succeed.
        // The job must still complete and the scheduler keeps serving.
        let counter = Arc::new(Mutex::new(0u32));
        let c2 = counter.clone();
        let (spawner, spawned) = ScriptedSpawner::new(Duration::ZERO, move |_, _| {
            let mut n = c2.lock().unwrap();
            *n += 1;
            if *n == 1 {
                WorkerRunResult::Died
            } else {
                WorkerRunResult::Done { out_bytes: 9 }
            }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let done = s
            .submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        assert_eq!(done.out_bytes, 9);
        // A second job still works → scheduler alive after a worker death.
        let done2 = s
            .submit(
                PathBuf::from("/m/y"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        assert_eq!(done2.out_bytes, 9);
        // At least two spawns happened (the dead one + a replacement).
        assert!(spawned.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn worker_panic_does_not_hang_submit() {
        // A worker whose run() panics must not leak the caller's reply.
        // The scheduler maps the panic (JoinError) to Died → bounded
        // retry → eventually resolves with an error, never hangs.
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| {
            panic!("simulated worker/ffi explosion");
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            s.submit(
                PathBuf::from("/m/x"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            ),
        )
        .await
        .expect("submit hung after worker panic");
        assert!(matches!(r, Err(SchedError::Failed(_))), "got {r:?}");
        // Scheduler still serves after absorbing panics.
        let snap = tokio::time::timeout(Duration::from_secs(2), s.snapshot())
            .await
            .expect("snapshot hung");
        assert!(snap.is_some());
    }

    #[tokio::test]
    async fn saturation_no_deadlock_under_timeout() {
        // Fire far more jobs than permits with a small delay; with a
        // generous pending_cap all must finish. Wrap in a timeout so a
        // deadlock fails loudly instead of hanging the suite.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(5), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let fut = async {
            let mut handles = Vec::new();
            for _ in 0..200 {
                let s2 = s.clone();
                handles.push(tokio::spawn(async move {
                    s2.submit(
                        PathBuf::from("/m/x"),
                        h264(),
                        file_sink(),
                        JobClass::Interactive,
                        JobHint::default(),
                    )
                    .await
                }));
            }
            let mut ok = 0;
            for h in handles {
                if h.await.unwrap().is_ok() {
                    ok += 1;
                }
            }
            ok
        };
        let ok = tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .expect("scheduler deadlocked under saturation");
        assert_eq!(ok, 200);
    }

    #[tokio::test]
    async fn abandoned_queued_jobs_are_skipped_not_dispatched() {
        // Post-seek contention: hls.js aborts the in-flight prefetch fetches
        // for the OLD position when the user seeks; actix drops those handler
        // futures, dropping each `submit().await` and its oneshot receiver.
        // A queued job whose caller has gone must NOT burn a worker slot when
        // permits free — otherwise the seek-target segment waits behind dead
        // work and the user stares at a spinner. Proven by counting real
        // worker runs: only the blockers + the live queued job execute; the
        // abandoned jobs are dropped from the pending queue.
        let runs = Arc::new(AtomicU64::new(0));
        let r2 = runs.clone();
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(150), move |_, _| {
            r2.fetch_add(1, Ordering::SeqCst);
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());

        // 5 blockers occupy every permit (2 nvenc + 1 vaapi + 2 cpu).
        let mut blockers = Vec::new();
        for _ in 0..5 {
            let s2 = s.clone();
            blockers.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/block"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;

        // 10 "old position" jobs queue behind the blockers, then their
        // callers vanish (seek) — abort the tasks so the futures (and their
        // oneshot receivers) drop.
        let mut abandoned = Vec::new();
        for _ in 0..10 {
            let s2 = s.clone();
            abandoned.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/old"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        for h in &abandoned {
            h.abort();
        }
        // One legitimate seek-target job that stays queued behind the same
        // blockers — it MUST still complete (we skip only abandoned work).
        let s3 = s.clone();
        let seek_target = tokio::spawn(async move {
            s3.submit(
                PathBuf::from("/m/seek"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        for h in blockers {
            h.await.unwrap().unwrap();
        }
        let seek = tokio::time::timeout(Duration::from_secs(2), seek_target)
            .await
            .expect("seek-target hung behind abandoned work")
            .unwrap();
        assert!(seek.is_ok(), "seek-target segment must complete: {seek:?}");
        // Let any (erroneously) dispatched abandoned jobs finish so the count
        // is stable, then assert none ran.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            6,
            "only 5 blockers + 1 seek-target may run; abandoned queued jobs must be skipped"
        );
    }

    #[tokio::test]
    async fn snapshot_reports_capacity() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let snap = s.snapshot().await.unwrap();
        // 2 hw + cpu.
        assert_eq!(snap.devices.len(), 3);
        let total_cap: usize = snap.devices.iter().map(|d| d.capacity).sum();
        assert_eq!(total_cap, 2 + 1 + 2);
    }

    /// T020a — `pharos_transcode_pin_total{outcome}` is how V80's one-encoder
    /// guarantee is OBSERVED. A collision here is worse than a rename: fold
    /// `invalidated` into `followed` and a rendition whose device went away
    /// mid-stream reports as a healthy pin, so the alert for the #114 failure
    /// mode never fires while the dashboard looks fine.
    ///
    /// This is a real guard only because the strings live on `PinOutcome`.
    /// Asserting two inline literals at their call sites would have compared
    /// constants written in the test to constants written beside them —
    /// tautology that passes whatever the code does.
    #[test]
    fn pin_outcome_labels_are_distinct_and_stable() {
        const ALL: [PinOutcome; 3] = [
            PinOutcome::Followed,
            PinOutcome::Invalidated,
            PinOutcome::Unresolved,
        ];
        assert_eq!(PinOutcome::Followed.label(), "followed");
        assert_eq!(PinOutcome::Invalidated.label(), "invalidated");
        assert_eq!(PinOutcome::Unresolved.label(), "unresolved");

        let labels: std::collections::HashSet<&str> = ALL.iter().map(|o| o.label()).collect();
        assert_eq!(
            labels.len(),
            ALL.len(),
            "pin outcome labels collide: {labels:?} — a folded bucket reports a \
             broken pin as a healthy one"
        );
    }

    /// `candidates_for` runs on every examination of a queued job, including
    /// every drain pass while it waits for its pinned device — but
    /// `pharos_transcode_pin_total{outcome="followed"}` must count JOBS
    /// placed, not examinations. Before the fix, `Followed` was recorded
    /// inside `candidates_for` itself, so a pinned job queued behind its busy
    /// device inflated the counter once per drain it survived — unboundedly
    /// under saturation, exactly when the counter is read to verify the pin.
    ///
    /// The shape: pin the job to a single-permit GPU, hold that GPU busy for
    /// the whole test, and drive several SEPARATE drains (one per completed
    /// CPU job) while the pinned job sits in `pending` and is re-examined on
    /// each one. Only when the GPU finally frees does the job actually
    /// dispatch. `followed` must read exactly 1, not one per drain.
    #[test]
    fn a_pinned_job_examined_across_several_drains_records_followed_once() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let gpu = DeviceId::hw(HwAccel::Nvenc, 0);
                // One GPU permit (what the CMAF job is pinned to), three CPU
                // permits (unrelated jobs whose completions drive drains).
                let spawner =
                    Arc::new(VariableSpawner(Arc::new(
                        |spec: &JobSpec| match job_name(spec).as_str() {
                            "hold-gpu" => Duration::from_millis(220),
                            _ => Duration::from_millis(15),
                        },
                    )));
                let s = TranscodeScheduler::spawn(
                    DeviceTable::from_probe(&[(gpu, 1)], 3),
                    spawner,
                    SchedConfig::default(),
                );

                // Holds the GPU (and with it the pin's only candidate) for
                // the whole test.
                let hold = {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit(
                            PathBuf::from("/m/hold-gpu"),
                            h264(),
                            file_sink(),
                            JobClass::Interactive,
                            JobHint::default(),
                        )
                        .await
                    })
                };
                tokio::time::sleep(Duration::from_millis(20)).await;

                // Queues behind the busy pinned device — this submission is
                // one examination (via `place`) on its own.
                let probe = {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit(
                            PathBuf::from("/m/probe"),
                            cmaf(),
                            file_sink(),
                            JobClass::Interactive,
                            JobHint::default(),
                        )
                        .await
                    })
                };
                tokio::time::sleep(Duration::from_millis(20)).await;
                let snap = s.snapshot().await.expect("snapshot");
                assert_eq!(
                    snap.pending, 1,
                    "precondition: the pinned job must be queued behind its \
                     busy device, not placed or failed"
                );

                // Each of these lands on a free CPU permit while the GPU is
                // still held by `hold-gpu`, and each completion drives its
                // own `drain_pending` — which re-examines the still-queued
                // pinned job (calls `candidates_for` again) without being
                // able to place it (its only candidate, the GPU, has no free
                // permit). Three sequential completions here means at least
                // three more examinations beyond the initial submission.
                for tag in ["cpu-1", "cpu-2", "cpu-3"] {
                    let s2 = s.clone();
                    s2.submit(
                        PathBuf::from(format!("/m/{tag}")),
                        h264(),
                        file_sink(),
                        JobClass::Interactive,
                        JobHint::default(),
                    )
                    .await
                    .expect("cpu job");
                }

                hold.await.unwrap().expect("gpu blocker");
                let done = probe
                    .await
                    .unwrap()
                    .expect("the pinned job must eventually dispatch");
                assert_eq!(
                    done.device, gpu,
                    "must still land on the pinned device, not spill"
                );
            })
        });

        let found = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_transcode_pin_total" {
                    return None;
                }
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                if labels.contains(&"outcome=followed".to_string()) {
                    Some(v)
                } else {
                    None
                }
            });
        let value = found.expect(
            "pharos_transcode_pin_total{outcome=\"followed\"} must be recorded \
             for the one pinned job that dispatched",
        );
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "a pinned job re-examined across several drains must record \
             `followed` exactly ONCE, at actual dispatch — not once per \
             examination; got {value:?}"
        );
    }

    #[test]
    fn promotion_outcome_labels_are_distinct_and_stable() {
        const ALL: [PromotionOutcome; 5] = [
            PromotionOutcome::Queued,
            PromotionOutcome::Inflight,
            PromotionOutcome::AlreadyClient,
            PromotionOutcome::Unknown,
            PromotionOutcome::Unassigned,
        ];
        assert_eq!(PromotionOutcome::Queued.label(), "queued");
        assert_eq!(PromotionOutcome::Inflight.label(), "inflight");
        assert_eq!(PromotionOutcome::AlreadyClient.label(), "already_client");
        assert_eq!(PromotionOutcome::Unknown.label(), "unknown");
        assert_eq!(PromotionOutcome::Unassigned.label(), "unassigned");

        let labels: std::collections::HashSet<&str> = ALL.iter().map(|o| o.label()).collect();
        assert_eq!(
            labels.len(),
            ALL.len(),
            "promotion outcome labels collide: {labels:?} — folding `unassigned` \
             into `unknown` would hide the window where a client can still \
             inherit a speculative tier"
        );
    }

    /// A promoted job is judged on the wait the CLIENT actually paid, not on
    /// the head start the prefetch had before that client existed.
    ///
    /// `observe_margin` fires for anything Interactive at completion, and a
    /// promoted job's `dispatched` is when the SPECULATIVE encode began —
    /// potentially seconds before any client. Measured from there, the worked
    /// case is: a prefetch dispatched at T0, joined at T0+4 s, done at T0+5 s
    /// reports a 5 s encode against a 3 s deadline and the controller HALVES
    /// the device's allowance. The client waited one second. The design worked
    /// exactly as intended and the control loop punished it for working — and
    /// the bias is self-reinforcing in the wrong direction, since the deeper
    /// the buffer a high allowance buys, the earlier prefetch runs relative to
    /// the join, the larger the over-attribution.
    ///
    /// Scripted at a tenth the scale: a 0.5 s segment (deadline 0.25 s), an
    /// 800 ms encode, and a client joining at 700 ms. From dispatch that is
    /// 800 ms — a clear miss; from promotion it is ~100 ms — a comfortable
    /// meet. The VERDICT is the assertion because the verdict is what the
    /// control law consumes; the allowance alone would not discriminate here
    /// (a miss with no speculative peers leaves it where it is), which is
    /// exactly the trap a "the number did not move" assertion falls into.
    #[test]
    fn a_promoted_job_is_judged_from_when_the_client_arrived() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(800), |_, _| {
                    WorkerRunResult::Done { out_bytes: 1 }
                });
                let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
                let slot = JobSlot::new();
                let mut watch_id = slot.subscribe();

                let job = {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit_tracked(
                            PathBuf::from("/m/prefetch"),
                            cmaf_lasting(0.5),
                            file_sink(),
                            JobClass::Background,
                            JobHint {
                                stream: StreamKey::of("viewer"),
                                segment: Some(7),
                                seeds_playhead: PlayheadSeed::Observes,
                            },
                            Some(slot),
                        )
                        .await
                    })
                };
                let job_id = await_job_id(&mut watch_id)
                    .await
                    .expect("the scheduler must name a submission it accepted");
                // The client turns up late in an encode it did not start.
                tokio::time::sleep(Duration::from_millis(700)).await;
                s.promote(job_id).await;
                job.await.unwrap().expect("the encode itself succeeded");
                // Drain the actor so `JobFinished` — and the observation it
                // makes — has certainly been processed before the snapshot.
                let _ = s.snapshot().await;
            })
        });

        let verdicts: Vec<(Vec<String>, DebugValue)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_transcode_margin_total" {
                    return None;
                }
                Some((
                    k.labels()
                        .map(|l| format!("{}={}", l.key(), l.value()))
                        .collect(),
                    v,
                ))
            })
            .collect();

        assert_eq!(
            verdicts.len(),
            1,
            "exactly one interactive completion happened here: {verdicts:?}"
        );
        assert!(
            verdicts[0].0.contains(&"verdict=met".to_string()),
            "a promoted job must be judged from the moment a client joined it, \
             not from a dispatch that predates the client — the client waited \
             ~100 ms against a 250 ms deadline: {verdicts:?}"
        );
        assert!(
            matches!(verdicts[0].1, DebugValue::Counter(1)),
            "got {:?}",
            verdicts[0].1
        );
    }

    /// The mechanism that makes speculative work safe to defer: a client
    /// waiting on a speculative job's result is proof the speculation was
    /// correct, so the job stops being speculative.
    ///
    /// Asserted through the SNAPSHOT rather than through the promotion counter,
    /// because the counter only proves a message was delivered. What matters is
    /// the effect: the job no longer counts against its device's speculative
    /// allowance, which is what `crowds_a_client` reads on every later
    /// admission decision.
    #[tokio::test]
    async fn a_promoted_job_stops_counting_as_speculation() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(400), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(2), spawner, SchedConfig::default());
        let slot = JobSlot::new();
        let mut watch_id = slot.subscribe();

        let job = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit_tracked(
                    PathBuf::from("/m/prefetch"),
                    h264(),
                    file_sink(),
                    JobClass::Background,
                    JobHint {
                        stream: StreamKey::of("viewer"),
                        segment: Some(7),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                    Some(slot),
                )
                .await
            })
        };
        let job_id = await_job_id(&mut watch_id)
            .await
            .expect("the scheduler must name a submission it accepted");

        let before = s.snapshot().await.expect("snapshot");
        assert_eq!(
            before
                .devices
                .iter()
                .map(|d| d.inflight_background)
                .sum::<usize>(),
            1,
            "precondition: it is running as speculation"
        );

        s.promote(job_id).await;
        let after = s.snapshot().await.expect("snapshot");
        assert_eq!(
            after
                .devices
                .iter()
                .map(|d| d.inflight_background)
                .sum::<usize>(),
            0,
            "a promoted job must stop counting against the speculative allowance"
        );
        assert_eq!(
            after
                .devices
                .iter()
                .map(|d| d.inflight_interactive)
                .sum::<usize>(),
            1,
            "...and must count as the client's work it now is"
        );
        // Promotion says a client is WAITING on this segment, not that it has
        // reached it. Moving the playhead here would let work already in flight
        // advance the distance its own urgency is judged against. (The
        // submission itself seeded this stream, since nothing else had ever
        // named it — see `speculation_may_seed_a_playhead_but_never_moves_one`
        // — so the claim is that promotion changed nothing, not that the map
        // is empty.)
        assert_eq!(
            after.playheads, before.playheads,
            "promotion must not move a playhead: {:?} -> {:?}",
            before.playheads, after.playheads
        );
        assert!(job.await.unwrap().is_ok());
    }

    /// Every promotion request is counted, including the ones that change
    /// nothing. A promotion path that books only its successes cannot tell "no
    /// client ever joined speculative work" from "every client that did arrived
    /// after the job had already finished" — opposite problems.
    #[test]
    fn a_promotion_with_no_target_is_counted_not_silently_dropped() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| {
                    WorkerRunResult::Done { out_bytes: 1 }
                });
                let s = TranscodeScheduler::spawn(one_gpu(2), spawner, SchedConfig::default());
                // Nothing was ever submitted, so this id names no job at all.
                s.promote(JobId(4242)).await;
                // Round-trip through the actor so the promotion is processed.
                let _ = s.snapshot().await;
            })
        });

        let found = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_transcode_promotion_total" {
                    return None;
                }
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                Some((labels, v))
            });
        let (labels, value) = found.expect(
            "a promotion that found no job must still be counted: \
             pharos_transcode_promotion_total{outcome=\"unknown\"}",
        );
        assert!(
            labels.contains(&"outcome=unknown".to_string()),
            "expected outcome=unknown; got {labels:?}"
        );
        assert!(
            matches!(value, DebugValue::Counter(1)),
            "exactly one promotion was requested; got {value:?}"
        );
    }

    #[test]
    fn job_class_labels_are_distinct_and_stable() {
        // These strings are a dashboard contract: they appear as the `class`
        // label on pharos_transcode_pending_by_class,
        // pharos_transcode_queue_wait_seconds and
        // pharos_segment_produced_total. A rename breaks every query silently.
        assert_eq!(JobClass::Interactive.label(), "interactive");
        assert_eq!(JobClass::Background.label(), "background");
        assert_ne!(JobClass::Interactive.label(), JobClass::Background.label());
    }

    #[tokio::test]
    async fn snapshot_attributes_the_queue_and_the_devices_to_who_is_waiting() {
        // The signal under test: a snapshot must say how much work is running
        // and queued for each class. Without the split, "5 running, 3 queued"
        // looks the same whether those five are stalled viewers or speculative
        // warm-up sitting in front of them.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(600), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        // 5 permits total (2 nvenc + 1 vaapi + 2 cpu). Start with the pool
        // idle so speculative work is admitted, then fill the rest.
        let mut handles = Vec::new();
        for (class, n) in [(JobClass::Background, 2), (JobClass::Interactive, 3)] {
            for _ in 0..n {
                let s2 = s.clone();
                handles.push(tokio::spawn(async move {
                    s2.submit(
                        PathBuf::from("/m/run"),
                        h264(),
                        file_sink(),
                        class,
                        JobHint::default(),
                    )
                    .await
                }));
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        // Every permit is now held; further client requests queue.
        for _ in 0..3 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/queued"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;

        let snap = s.snapshot().await.expect("snapshot");
        assert_eq!(snap.inflight, 5, "every permit held by a segment job");
        assert_eq!(snap.pending, 3);
        assert_eq!(snap.pending_interactive, 3, "the queue is client requests");
        assert_eq!(snap.pending_background, 0);
        assert!(
            snap.oldest_pending_ms.is_some_and(|ms| ms > 0),
            "the head of the queue has measurably been waiting"
        );
        assert_eq!(snap.live_streams, 0, "no live stream took a permit");
        // Device occupancy is attributable to the jobs holding it: 3
        // interactive + 2 speculative, with nothing left unexplained.
        let by_class = |f: fn(&DeviceStat) -> usize| snap.devices.iter().map(f).sum::<usize>();
        assert_eq!(by_class(|d| d.inflight_interactive), 3);
        assert_eq!(by_class(|d| d.inflight_background), 2);
        assert_eq!(
            by_class(|d| d.inflight_interactive) + by_class(|d| d.inflight_background),
            by_class(|d| d.in_use),
            "no unexplained device occupancy"
        );

        for h in handles {
            assert!(h.await.unwrap().is_ok());
        }
    }

    /// The defect this pins: prefetch is dispatched BEFORE the segment the
    /// client is blocked on, and once shared one FIFO with it, so speculative
    /// encodes could bury a client's own segment.
    ///
    /// The mechanism changed when speculative work began to WAIT for a permit
    /// instead of being dropped (V58). "Never in the queue" was how that used
    /// to be enforced and is no longer true; what must stay true is that being
    /// in the queue never gets it dispatched ahead of a client. So this now
    /// submits the prefetch FIRST and the client second, and asserts the client
    /// still goes first — an ordering the old shed could not even express.
    #[tokio::test]
    async fn speculative_work_waits_and_never_dispatches_in_front_of_a_client() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(400), move |_, spec| {
            seen.lock().unwrap().push(
                spec.input
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        // 5 permits total (2 nvenc + 1 vaapi + 2 cpu). Occupy every one.
        let mut running = Vec::new();
        for _ in 0..5 {
            let s2 = s.clone();
            running.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/run"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Speculative work waits for a permit rather than being dropped.
        let s2 = s.clone();
        let prefetch = tokio::spawn(async move {
            s2.submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
                JobHint::default(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(40)).await;

        // A client asks AFTER it, and must still be served BEFORE it.
        let s3 = s.clone();
        let client = tokio::spawn(async move {
            s3.submit(
                PathBuf::from("/m/client"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        let snap = s.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.pending_background, 1,
            "the prefetch must be held, not dropped"
        );
        assert_eq!(snap.pending_interactive, 1, "the client request is queued");

        assert!(client.await.unwrap().is_ok());
        assert!(prefetch.await.unwrap().is_ok());
        for h in running {
            assert!(h.await.unwrap().is_ok());
        }
        let got: Vec<String> = order
            .lock()
            .unwrap()
            .iter()
            .filter(|n| n != &"run")
            .cloned()
            .collect();
        assert_eq!(
            got,
            ["client", "prefetch"],
            "the client must take the first freed permit even though the \
             prefetch asked for it first: {got:?}"
        );
    }

    #[tokio::test]
    async fn speculative_work_runs_when_there_is_spare_capacity() {
        // Shedding must not become "never prefetch": with the pool idle, a
        // background job is admitted like any other.
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 3,
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        let done = s
            .submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
                JobHint::default(),
            )
            .await
            .expect("idle pool must accept speculative work");
        assert_eq!(done.out_bytes, 3);
    }

    /// The last free permit is reserved: speculative work may not TAKE it while
    /// only `background_headroom` permits remain, so a client request arriving a
    /// moment later still finds a slot instead of queueing behind a guess.
    ///
    /// What the reserve refuses is the PERMIT, not the job. Since V58 the
    /// refused prefetch waits rather than dying, so this asserts the permit was
    /// not taken — the queue holds it and the device count is unchanged — where
    /// it used to assert the error code the drop returned. That is the same
    /// invariant measured one step closer to it: a `Busy` proves nothing about
    /// what the device is doing.
    #[tokio::test]
    async fn speculative_work_leaves_headroom_for_a_client() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(400), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
        // Occupy 4 of the 5 permits, leaving exactly one free.
        let mut running = Vec::new();
        for _ in 0..4 {
            let s2 = s.clone();
            running.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/run"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        let s2 = s.clone();
        let prefetch = tokio::spawn(async move {
            s2.submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
                JobHint::default(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        let snap = s.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.pending_background, 1,
            "the reserved permit is not for speculative work"
        );
        assert_eq!(
            snap.devices.iter().map(|d| d.in_use).sum::<usize>(),
            4,
            "...so the free permit is still free: {:?}",
            snap.devices
        );
        // ...and that reserved permit is there for the client.
        let done = s
            .submit(
                PathBuf::from("/m/client"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .expect("client must get the reserved permit");
        assert_eq!(done.queue_wait_ms, 0, "client did not wait");
        assert!(prefetch.await.unwrap().is_ok(), "the prefetch still runs");
        for h in running {
            assert!(h.await.unwrap().is_ok());
        }
    }
    /// A single GPU, so a job's peers are unambiguous.
    fn one_gpu(capacity: usize) -> DeviceTable {
        DeviceTable::from_probe(&[(DeviceId::hw(HwAccel::Nvenc, 0), capacity)], 0)
    }

    /// The line an abandonment cannot cross, made askable.
    ///
    /// A caller that walks away from a QUEUED job really does hand the capacity
    /// back — `place` / `reap_abandoned` / `try_place_no_queue` all read
    /// `reply.is_closed()` before spending a permit. A caller that walks away
    /// from a DISPATCHED one reclaims nothing: `spawn_run_task` is detached and
    /// owns the worker and the permit, and the `JobFinished` arm ignores a
    /// failed reply send. Nothing said which side of that line a job was on, so
    /// a caller weighing "is abandoning this free?" had to guess — and the
    /// segment cache guessed "always", which let an orphaned worker keep writing
    /// to the output path its own successor was about to use.
    ///
    /// Asserted at both dispatch sites, because a job that waits in the queue
    /// and drains onto a permit later is exactly as unreclaimable as one placed
    /// on arrival, and the queue is where speculative work spends its life.
    #[tokio::test]
    async fn a_slot_says_when_its_job_is_past_reclaiming() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(200), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        // ONE permit in the whole table, so the second submission has to wait
        // for the first — the queue is half of what this test is about.
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig::default(),
        );

        let first = JobSlot::new();
        let second = JobSlot::new();
        assert!(
            !first.is_dispatched(),
            "a slot that has not even been submitted cannot be running"
        );

        let mut handles = Vec::new();
        for (tag, slot) in [("first", first.clone()), ("second", second.clone())] {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit_tracked(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint {
                        stream: StreamKey::NONE,
                        segment: None,
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                    Some(slot),
                )
                .await
            }));
        }
        // Long enough for the actor to have placed what it can, short enough
        // that the running job is still running.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let snap = s.snapshot().await.expect("snapshot");
        assert_eq!(snap.inflight, 1, "precondition: one job holds the permit");
        assert_eq!(snap.pending, 1, "precondition: the other one waits");
        assert!(
            first.is_dispatched(),
            "a job that holds a device permit must say so — abandoning it \
             reclaims nothing and leaves a worker writing to its output path"
        );
        assert!(
            !second.is_dispatched(),
            "a QUEUED job is still reclaimable: reporting it as running would \
             throw away the only reclaim abandonment can actually make"
        );

        for h in handles {
            h.await.unwrap().expect("both jobs must complete");
        }
        assert!(
            second.is_dispatched(),
            "a job dispatched off the QUEUE must latch too — `try_place_no_queue` \
             spends a permit exactly as `place` does"
        );
    }

    /// ODD — what a job encoded BESIDE is the thing that sets how long it took,
    /// and nothing recorded it.
    ///
    /// Measured on the deployment: one 6 s segment costs 1 860 ms alone and
    /// 6 229 ms when six speculative encodes share the device. `encode_ms`
    /// reports the 6 229 and cannot say why, so a slow encode is
    /// indistinguishable from a crowded one — the two need opposite fixes.
    #[tokio::test]
    async fn a_finished_job_reports_how_many_peers_shared_its_device() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(300), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());

        let first = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/a"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(60)).await;

        let second = s
            .submit(
                PathBuf::from("/m/b"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .expect("second job must be admitted");
        assert_eq!(
            second.peer_jobs, 1,
            "a job dispatched onto a device already running one job has one peer"
        );
        assert!(first.await.unwrap().is_ok());
    }

    /// B178 — a job that WAITED for its permit reported the company it kept as
    /// zero, because only the first-attempt dispatch path stamped it. The
    /// drain path is the one that runs when the device is busiest, so the
    /// instrumentation was blind in precisely the case it exists for.
    ///
    /// This is a strengthened version of the brief's original two-job case (A
    /// alone takes the only GPU permit, B queues behind it). That version
    /// does not actually discriminate: `DeviceSlot::new` clamps ANY probed
    /// capacity to at least 1, so a "one GPU permit" table still carries a
    /// free CPU permit, and a second job lands there immediately instead of
    /// queuing at all (confirmed empirically: it dispatched to `DeviceId::Cpu`
    /// with `queue_wait_ms == 0`). And even fixed to force a real queue, "B
    /// ran alone" makes the expected `peer_jobs`/`background_peers` both 0 --
    /// exactly what `JobCtx` already carries as its zero-initialised default,
    /// so a `try_place_no_queue` that stamps nothing would pass it too. This
    /// version keeps a second job (A2, speculative) alive on the GPU when B
    /// drains onto it, and a filler on the ever-present CPU permit so B is
    /// forced to actually queue, so the expected counts are non-zero and a
    /// no-op cannot produce them by accident.
    #[tokio::test]
    async fn a_job_dispatched_off_the_queue_reports_its_peers() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(300), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        // Two GPU permits: A1 and A2 both land there, back to back.
        let s = TranscodeScheduler::spawn(one_gpu(2), spawner, SchedConfig::default());

        let a1 = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/a1"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Second GPU slot -- speculative, so B's drain exercises
        // `background_peers` and not just `peer_jobs`. Started just after A1
        // with the same run length, so it is still inflight when A1's permit
        // frees and B drains onto the device.
        let a2 = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/a2"),
                    h264(),
                    file_sink(),
                    JobClass::Background,
                    JobHint::default(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;

        // The scheduler always keeps at least one CPU permit (a probed
        // capacity of 0 is clamped up to 1 -- see `DeviceSlot::new` -- so the
        // table never holds a dead slot). Without consuming it, B would land
        // on that free CPU permit instead of queuing at all.
        let filler_cpu = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/filler"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;

        // B: both GPU slots and the one CPU slot are taken, so this one must
        // actually queue.
        let b = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/b"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            })
        };

        let a1 = a1.await.unwrap().unwrap();
        let filler_cpu = filler_cpu.await.unwrap().unwrap();
        let b = b.await.unwrap().unwrap();
        let a2 = a2.await.unwrap().unwrap();

        assert_eq!(a1.peer_jobs, 0, "A1 was first onto the GPU, alone");
        assert_eq!(
            filler_cpu.device,
            DeviceId::Cpu,
            "the filler must have taken the always-present CPU permit"
        );
        assert_eq!(a2.device, DeviceId::hw(HwAccel::Nvenc, 0));

        // B dispatched from `pending` once A1's permit freed. A2 -- a
        // Background job -- was still running on the GPU at that moment, so
        // a correctly-stamped B reports the company it kept: one peer, and
        // that peer was speculative.
        assert_eq!(
            b.peer_jobs, 1,
            "A2 was still running on the GPU when B drained onto it"
        );
        assert_eq!(b.background_peers, 1, "that peer (A2) was speculative");
        assert!(b.queue_wait_ms > 0, "B must actually have queued");
    }

    /// The fix. Speculative work must not pile onto the device that is
    /// encoding the segment a client is blocked on.
    ///
    /// `background_headroom` reserves a PERMIT, which is what a client needs to
    /// start; it does not reserve the encoder's throughput, which is what the
    /// client needs to finish. On the deployment one client segment request
    /// launches itself plus six prefetches, all admitted to the one GPU at
    /// once, and the client's own segment slows from 1 860 ms to 6 358 ms.
    ///
    /// The invariant is about the CLIENT'S device, not about refusing
    /// speculative work everywhere: a prefetch that lands on an otherwise idle
    /// second device costs the client nothing, so the assertion counts what
    /// joined the client rather than what was admitted at all.
    ///
    /// The MECHANISM of that count changed when speculative work began to wait
    /// for a permit instead of being dropped (V58). Counting GPU landings over
    /// the whole test measured "beside the client" only while refused work
    /// vanished; now a prefetch held back correctly still runs on that GPU a
    /// moment after the client has finished with it, which is the design
    /// working. `peer_jobs > background_peers` says the job was dispatched next
    /// to at least one non-speculative peer, which is the invariant itself
    /// rather than a proxy for it, and it is what the disarm check moves.
    #[tokio::test]
    async fn speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(400), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
        let gpu = DeviceId::hw(HwAccel::Nvenc, 0);

        // A client is waiting on this one; it takes the GPU.
        let client = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/client"),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(60)).await;

        // The prefetch burst that same request would launch. The GPU has three
        // free permits, so before the fix every one of these was admitted onto
        // it beside the client.
        let mut handles = Vec::new();
        for n in 0..4 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/pre{n}")),
                    h264(),
                    file_sink(),
                    JobClass::Background,
                    JobHint::default(),
                )
                .await
            }));
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }

        let joined_the_client = results
            .iter()
            .filter(|r| matches!(r, Ok(d) if d.device == gpu && d.peer_jobs > d.background_peers))
            .count();
        // A LITERAL, not `SchedConfig::default().<the same constant>`. The
        // expected value and the behaviour under test must not come from one
        // source, or the guard restates the code instead of checking it: read
        // off the floor, this assertion passed for any floor whatever.
        assert_eq!(
            joined_the_client, 1,
            "speculative jobs beside the client on its device: {results:?}"
        );
        assert!(
            results
                .iter()
                .any(|r| matches!(r, Ok(d) if d.queue_wait_ms > 0)),
            "the cap must actually hold work back, not merely reorder it: {results:?}"
        );
        assert!(client.await.unwrap().is_ok());
    }

    /// ...and the cap is about CROWDING A CLIENT, not about throttling prefetch
    /// in general: with no client segment on the device, speculative work is
    /// free to use it, which is how the buffer gets built between requests.
    #[tokio::test]
    async fn speculative_work_may_fill_an_idle_device() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(300), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
        let mut running = Vec::new();
        for n in 0..3 {
            let s2 = s.clone();
            running.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/pre{n}")),
                    h264(),
                    file_sink(),
                    JobClass::Background,
                    JobHint::default(),
                )
                .await
            }));
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        for h in running {
            assert!(
                h.await.unwrap().is_ok(),
                "an idle device is exactly where prefetch belongs"
            );
        }
    }

    /// ODD — the tuner ships as a MEASUREMENT first. The controller must learn
    /// from finished jobs before anything consults it, so the deploy that turns
    /// it on can be judged against a gauge rather than a hope.
    ///
    /// Strengthened per controller review: asserting the allowance is still
    /// 1.0 after one job is satisfied by a controller that is never fed at all
    /// — precisely the wiring bug this test exists to catch. So this puts a
    /// speculative job on the GPU FIRST (a device with nothing else on it
    /// admits it unconditionally either way), then submits the interactive job
    /// the controller actually learns from. It sees one background peer
    /// already running, meets its deadline comfortably, and
    /// `background_peers (1) >= current allowance.floor() (1)` makes the
    /// success EXERCISED — the only kind that raises the allowance. If
    /// `observe` were never called the raw allowance could not move off 1.0.
    ///
    /// Renamed from `..._without_changing_admission`: that name described
    /// shadow mode (Task 3), where the controller learned but nothing
    /// consulted it. Closing the loop (Task 4) means admission now DOES
    /// change once the allowance rises — that is the feature, not a
    /// regression — so the second half now proves admission tracks the
    /// learned value: a fresh client held open while a burst of speculative
    /// jobs targets its device lets exactly `floor(2.0) == 2` of them join,
    /// not the shipped constant's 1. (The case that a constant CANNOT drift
    /// past what it was measured on lives in
    /// `speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for`,
    /// which starts a cold controller and never teaches it, so it still
    /// stays pinned at the floor.)
    #[tokio::test]
    async fn a_finished_segment_teaches_the_controller_and_admission_follows_it() {
        // 6 s segments (via `cmaf_with_duration`), 100 ms encode: comfortably
        // inside the 3 s deadline. `ScriptedSpawner` applies one script to
        // every worker it spawns, so every job below shares this delay.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(100), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
        let gpu = DeviceId::hw(HwAccel::Nvenc, 0);

        // A speculative job claims the GPU first...
        let bg = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/pre"),
                    cmaf_with_duration(),
                    file_sink(),
                    JobClass::Background,
                    JobHint::default(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;

        // ...so the interactive job dispatched next sees exactly one
        // background peer already running beside it: the allowance the
        // shipped constant permits, and the exact shape an exercised success
        // needs.
        let done = s
            .submit(
                PathBuf::from("/m/a"),
                cmaf_with_duration(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        assert_eq!(done.background_peers, 1);
        assert!(bg.await.unwrap().is_ok());

        let snap = s.snapshot().await.unwrap();
        // One entry per device the table actually holds (GPU + the always-
        // present CPU fallback) — proof the gauge is populated from the real
        // device list, not a single hardcoded value.
        assert_eq!(snap.background_allowance.len(), snap.devices.len());
        let learned = snap
            .background_allowance
            .iter()
            .find(|(d, _)| *d == gpu)
            .map(|(_, v)| *v);
        assert_eq!(
            learned,
            Some(2.0),
            "an exercised success (background_peers 1 >= floor 1) must raise \
             the allowance by increase_step -- {snap:?} proves the observation \
             never reached the controller if this is still 1.0"
        );

        // Admission must now TRACK the learned value: with the allowance at
        // 2.0, a fresh client held open while a burst of speculative jobs
        // targets its device lets 2 of them join, not the shipped
        // constant's 1.
        let client2 = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/b"),
                    cmaf_with_duration(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut handles = Vec::new();
        for n in 0..3 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/pre2{n}")),
                    cmaf_with_duration(),
                    file_sink(),
                    JobClass::Background,
                    JobHint::default(),
                )
                .await
            }));
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        // Counted BESIDE the client rather than on the device at all: since V58
        // a job the allowance held back is deferred, not dropped, and runs on
        // this GPU once the client has left it.
        let joined_the_client = results
            .iter()
            .filter(|r| matches!(r, Ok(d) if d.device == gpu && d.peer_jobs > d.background_peers))
            .count();
        assert_eq!(
            joined_the_client, 2,
            "admission must track the learned 2.0 allowance (floor(2.0) == 2), \
             not the shipped constant's 1: {results:?}"
        );
        assert!(client2.await.unwrap().is_ok());
    }

    /// The payoff, proven where it actually has to hold: not in the raw
    /// learned number (that already climbs in shadow mode -- Task 3's own
    /// `a_finished_segment_teaches_the_controller_and_admission_follows_it`
    /// seeds an exercised success from a background job that pre-dates the
    /// client and is never gated by `crowds_a_client` at all, constant or
    /// learned), but in ADMISSION: once the controller has learned an
    /// allowance above the constant it replaces, MORE speculative jobs must
    /// be let onto a device that already has a client's segment running on
    /// it than the constant ever allowed.
    ///
    /// An earlier draft of this test asserted only `background_allowance`
    /// rose above the floor and passed unmodified against the constant-gated
    /// `crowds_a_client` -- confirmed empirically before this version was
    /// written. Learning happens independently of admission (that
    /// independence is the whole point of shadow mode); only a count of
    /// jobs actually ADMITTED beside a running client can tell the two
    /// implementations apart.
    #[tokio::test]
    async fn a_learned_allowance_lets_more_speculative_work_join_an_inflight_client() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(300), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(8), spawner, SchedConfig::default());
        let gpu = DeviceId::hw(HwAccel::Nvenc, 0);

        // Seed: a background job dispatches uncontested -- no client is on
        // the device yet, so `crowds_a_client` cannot block it under EITHER
        // implementation (`interactive > 0` is false). A client dispatched
        // next sees it as a pre-existing peer: exactly the shape
        // `AdmissionController::observe` needs to call the success
        // EXERCISED, raising the learned allowance from the floor (1.0) to
        // 2.0.
        let seed_bg = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/seed"),
                    cmaf_with_duration(),
                    file_sink(),
                    JobClass::Background,
                    JobHint::default(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        let seeded = s
            .submit(
                PathBuf::from("/m/seed-client"),
                cmaf_with_duration(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            seeded.background_peers, 1,
            "the seed job must be pre-existing at the client's own dispatch"
        );
        assert!(seed_bg.await.unwrap().is_ok());

        let snap = s.snapshot().await.unwrap();
        let learned = snap
            .background_allowance
            .iter()
            .find(|(d, _)| *d == gpu)
            .map(|(_, v)| *v);
        assert_eq!(
            learned,
            Some(2.0),
            "one exercised success must raise the allowance by increase_step: {snap:?}"
        );

        // Now prove ADMISSION consults that 2.0, not the shipped constant
        // (1): hold a fresh client open on the same device and let a burst
        // target it, using the same staggered-burst shape
        // `speculative_work_does_not_crowd_the_segment_a_client_is_waiting_for`
        // uses to prove the constant's cap deterministically.
        let client = {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from("/m/held"),
                    cmaf_with_duration(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(60)).await;

        let mut handles = Vec::new();
        for n in 0..4 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/burst{n}")),
                    cmaf_with_duration(),
                    file_sink(),
                    JobClass::Background,
                    JobHint::default(),
                )
                .await
            }));
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        // Counted the same way as the crowding guard, and for the same reason:
        // since V58 a job held back correctly still runs on this GPU once the
        // client is done with it, so "landed on the GPU" is no longer the same
        // question as "ran BESIDE the client".
        let joined = results
            .iter()
            .filter(|r| matches!(r, Ok(d) if d.device == gpu && d.peer_jobs > d.background_peers))
            .count();
        assert_eq!(
            joined, 2,
            "a learned allowance of 2 must admit 2 speculative jobs beside an \
             already-inflight client, not the shipped constant's 1: {results:?}"
        );
        assert!(client.await.unwrap().is_ok());
    }

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
                    cmaf_with_duration(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
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

    /// The `ignored` arm at the SCHEDULER level, not just inside
    /// `AdmissionController::observe` (already unit-tested there). `cmaf()`
    /// carries no `duration_ticks` — a live/progressive job, per its own doc
    /// comment above — so `observe_margin` must translate that into a
    /// `verdict=ignored` count, not silence.
    ///
    /// Asserting the metric matters MORE than asserting the allowance: an
    /// allowance frozen at the floor is also exactly what a controller that
    /// was never wired up at all would produce, which is the trap two earlier
    /// reviews on this branch already caught (see
    /// `a_finished_segment_teaches_the_controller_and_admission_follows_it`'s
    /// doc comment). Only the counter distinguishes "no signal reached the
    /// loop" from "the device genuinely cannot go faster".
    #[test]
    fn a_duration_less_interactive_job_is_ignored_not_silently_dropped() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let allowance = metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(50), |_, _| {
                    WorkerRunResult::Done { out_bytes: 1 }
                });
                let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
                // `cmaf()`, not `cmaf_with_duration()`: no `duration_ticks` at
                // all, the live/progressive shape this arm exists for.
                s.submit(
                    PathBuf::from("/m/live"),
                    cmaf(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
                .unwrap();
                let snap = s.snapshot().await.unwrap();
                snap.background_allowance
                    .iter()
                    .find(|(d, _)| *d == DeviceId::hw(HwAccel::Nvenc, 0))
                    .map(|(_, v)| *v)
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
            "a duration-less interactive job must still emit \
             pharos_transcode_margin_total — silence here is indistinguishable \
             from a controller nothing ever fed",
        );
        assert!(
            labels.contains(&"verdict=ignored".to_string()),
            "expected an ignored verdict; got {labels:?}"
        );
        assert!(matches!(value, DebugValue::Counter(1)), "got {value:?}");

        assert_eq!(
            allowance,
            Some(1.0),
            "a duration-less observation must leave the allowance at the floor \
             (this alone would ALSO be true of a controller that was never \
             wired up, which is why the metric assertion above is the one \
             that matters)"
        );
    }

    /// The other `ignored` condition — a retried job's encode time is the
    /// duration of a bounce, not of an encode (`usable: ctx.retries == 0`).
    /// Driven the same way `worker_death_retries_and_scheduler_survives`
    /// forces a retry: script a worker death on the first attempt so the
    /// job's `ctx.retries` is 1 by the time it completes, then finish
    /// successfully on the retry so `observe_margin` runs at all.
    #[test]
    fn a_retried_interactive_job_is_ignored_not_silently_dropped() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let allowance = metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let counter = Arc::new(Mutex::new(0u32));
                let c2 = counter.clone();
                let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, move |_, _| {
                    let mut n = c2.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        WorkerRunResult::Died
                    } else {
                        WorkerRunResult::Done { out_bytes: 1 }
                    }
                });
                let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
                // A duration IS present here — the point of this test is the
                // `usable` guard, not the `segment_seconds` one covered above.
                s.submit(
                    PathBuf::from("/m/a"),
                    cmaf_with_duration(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
                .unwrap();
                let snap = s.snapshot().await.unwrap();
                snap.background_allowance
                    .iter()
                    .find(|(d, _)| *d == DeviceId::hw(HwAccel::Nvenc, 0))
                    .map(|(_, v)| *v)
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
            "a retried job's completion must still emit \
             pharos_transcode_margin_total — silence here is indistinguishable \
             from a controller nothing ever fed",
        );
        assert!(
            labels.contains(&"verdict=ignored".to_string()),
            "a retried job's encode time is the duration of a bounce, not an \
             encode; expected an ignored verdict, got {labels:?}"
        );
        assert!(matches!(value, DebugValue::Counter(1)), "got {value:?}");

        assert_eq!(
            allowance,
            Some(1.0),
            "an unusable observation must leave the allowance at the floor \
             (this alone would ALSO be true of a controller that was never \
             wired up, which is why the metric assertion above is the one \
             that matters)"
        );
    }

    /// The metrics crate registers a series lazily on first emission, and
    /// `pharos_transcode_background_allowance` used to be emitted ONLY from
    /// `observe_margin`, which runs when an interactive segment finishes. A
    /// freshly booted pod that nobody has streamed from therefore reported
    /// the series as entirely absent — indistinguishable from a build that
    /// predates the controller. Assert the gauge exists, for every device,
    /// the moment the scheduler is spawned, with no `submit` call anywhere in
    /// this test.
    #[test]
    fn the_allowance_gauge_exists_from_boot_before_any_job_is_submitted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| {
                    WorkerRunResult::Done { out_bytes: 1 }
                });
                // Spawning alone must publish the gauge — no `submit` here.
                let _s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
            })
        });

        let gauge = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let k = ck.key();
                if k.name() != "pharos_transcode_background_allowance" {
                    return None;
                }
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                Some((labels, v))
            });

        let (labels, value) = gauge.expect(
            "pharos_transcode_background_allowance must exist the moment the \
             scheduler is spawned — otherwise its absence cannot distinguish \
             'not deployed' from 'deployed but idle' or 'deployed and wedged \
             at the floor'",
        );
        let want_label = format!("device={}", DeviceId::hw(HwAccel::Nvenc, 0));
        assert!(
            labels.contains(&want_label),
            "expected the Nvenc device labeled; got {labels:?}"
        );
        assert!(
            matches!(value, DebugValue::Gauge(v) if v == 1.0),
            "a cold, never-observed device must read the floor (1.0); got {value:?}"
        );
    }

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
                seeds_playhead: PlayheadSeed::Observes,
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
                seeds_playhead: PlayheadSeed::Observes,
            },
        )
        .await
        .unwrap();

        let snap = s.snapshot().await.unwrap();
        assert_eq!(snap.playheads.get(&stream).copied(), Some(104));
    }

    /// A speculative request says nothing about where the viewer actually is —
    /// only what somebody guessed they might want next. If prefetch could MOVE
    /// the playhead, a deep speculative submission would advance the very
    /// distance measurement its own urgency is judged against.
    ///
    /// Seeding a stream that has no reading at all is the one thing it may do,
    /// and it is not the same act: it cannot overwrite anything a client
    /// established, and without it the cold-start prewarm — which submits
    /// `Background` against a `StreamKey` no interactive request has ever
    /// touched — is unknowable for its whole life and is the first thing a full
    /// queue throws away.
    ///
    /// The entitlement is the CALLER'S ([`PlayheadSeed`]), not "the map has no
    /// entry". Those look equivalent and are not: a stream has no entry until
    /// something has been SERVED on it, and a deep prefetch can be the first
    /// thing submitted for one — an entry-shaped gate would let that prefetch
    /// seed the stream at its own index and rank itself top of the band, which
    /// is the self-measurement the rule exists to forbid, in bounded form.
    ///
    /// All three halves in one test: an ordinary guess seeding brings that
    /// back; a prewarm NOT seeding brings back the permanently-unknown
    /// cold start; and either one MOVING a reading lets prefetch walk the
    /// playhead forward.
    #[tokio::test]
    async fn speculation_may_seed_a_playhead_but_never_moves_one() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(20), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
        let stream = StreamKey::of("play-session-background-only");

        let speculate = |seg: u32, seeds_playhead: PlayheadSeed| {
            let s2 = s.clone();
            async move {
                s2.submit(
                    PathBuf::from("/m/a"),
                    cmaf(),
                    file_sink(),
                    JobClass::Background,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead,
                    },
                )
                .await
                .unwrap();
            }
        };

        // An ordinary prefetch on an unseen stream — which is what a re-watch's
        // deep guesses look like, its opening segments having been served from
        // cache — says nothing about where the viewer is.
        speculate(50, PlayheadSeed::Observes).await;
        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.get(&stream).copied(),
            None,
            "an ordinary guess must not seed a stream just because nothing has \
             been recorded for it yet: it would be ranking itself"
        );

        // A prewarm does know: it picked its base from the resume position.
        speculate(50, PlayheadSeed::StatesTheStart).await;
        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.get(&stream).copied(),
            Some(50),
            "a caller that knows where playback starts must be able to say so, \
             or a cold-start prewarm can never be ranked against anything"
        );

        // ...and from here on it is frozen against speculation, however deep,
        // and whatever it claims to know.
        speculate(400, PlayheadSeed::StatesTheStart).await;
        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.get(&stream).copied(),
            Some(50),
            "a speculative submission must never MOVE a playhead: the distance \
             it is judged by would then be measuring itself"
        );

        // A client's own request is still the only thing that moves it.
        s.submit(
            PathBuf::from("/m/a"),
            cmaf(),
            file_sink(),
            JobClass::Interactive,
            JobHint {
                stream,
                segment: Some(60),
                seeds_playhead: PlayheadSeed::Observes,
            },
        )
        .await
        .unwrap();
        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.get(&stream).copied(),
            Some(60),
            "the client's own request must still overwrite the seed"
        );
    }

    /// T109 — a segment SERVED FROM CACHE is the same evidence about where a
    /// viewer stands as one that had to be encoded, and until now only the
    /// second reached the scheduler.
    ///
    /// The rule this pins has two halves, and each is a different message. A
    /// SUBMISSION is the client's request itself, so it may move the reading
    /// anywhere — forward on ordinary playback, backward on a seek. A HIT may
    /// only ever ADVANCE it: it is evidence the viewer reached that segment and
    /// never evidence they went back to it, so a hit for segment N landing after
    /// a request for N+2 (parallel fetches around a seek) cannot drag the
    /// reading back to N.
    ///
    /// Ordering is guaranteed without sleeping: `note_playhead` and `snapshot`
    /// travel the same mpsc, which the actor drains in order, so a snapshot
    /// taken after a send has necessarily seen it.
    #[tokio::test]
    async fn a_hit_may_advance_a_playhead_but_never_drag_it_backwards() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
        let stream = StreamKey::of("play-session-warm-buffer");

        // A stream whose OPENING segments were already on disk — a re-watch, a
        // second viewer on the same media — reaches the scheduler with no
        // reading at all. A hit is a client's own request, so it may establish
        // one; that is the case `PlayheadSeed` had to leave unknowable, because
        // the map was miss-driven and a guess is not allowed to seed itself.
        s.note_playhead(stream, 40);
        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.get(&stream).copied(),
            Some(40),
            "a client's request served from cache must establish a reading: it is \
             the same evidence as the request that missed"
        );

        // The warm viewer plays on, entirely out of cache.
        s.note_playhead(stream, 41);
        s.note_playhead(stream, 42);
        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.get(&stream).copied(),
            Some(42),
            "the better prefetch works the more of playback is hits — a reading \
             that only misses can move goes stale exactly when the system is \
             working"
        );

        // The out-of-order hit: segment 41's bytes land after 42's did.
        s.note_playhead(stream, 41);
        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.get(&stream).copied(),
            Some(42),
            "a hit is evidence the viewer REACHED that segment, never evidence \
             they went back to it — a late arrival must not rank every one of \
             this stream's guesses a segment further out than it is"
        );

        // A backward seek is expressed by the SUBMISSION it causes, which may
        // move the reading anywhere. That is what keeps `ForwardOnly` from
        // being a one-way ratchet.
        s.submit(
            PathBuf::from("/m/a"),
            cmaf(),
            file_sink(),
            JobClass::Interactive,
            JobHint {
                stream,
                segment: Some(7),
                seeds_playhead: PlayheadSeed::Observes,
            },
        )
        .await
        .unwrap();
        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.get(&stream).copied(),
            Some(7),
            "a client's own request still says where the viewer is, in either \
             direction"
        );
    }

    /// T109 (a) — the cost decision, pinned as behaviour rather than left as a
    /// claim in a commit message.
    ///
    /// A hit is the COMMON case, so this send happens per served segment on a
    /// channel already fed at segment rate by submissions and completions.
    /// `note_playhead` is therefore synchronous and fire-and-forget: it never
    /// awaits, and a full inbox drops the update instead of applying
    /// backpressure to the HTTP handler that is holding a viewer's segment.
    /// Dropping is safe precisely because a lost update leaves the reading
    /// exactly as stale as it is today and never staler.
    ///
    /// Deterministic without any timing: on a current-thread runtime the actor
    /// task cannot be polled while this test never awaits, so the inbox fills
    /// at `inbox_depth` and every later send must be refused. If `note_playhead`
    /// ever became a blocking `send`, this test would deadlock rather than fail
    /// — which is the failure it exists to make impossible to ship.
    #[test]
    fn a_playhead_update_is_dropped_rather_than_stalling_the_viewer_serving_it() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| {
                    WorkerRunResult::Done { out_bytes: 1 }
                });
                let s = TranscodeScheduler::spawn(
                    one_gpu(4),
                    spawner,
                    SchedConfig {
                        inbox_depth: 8,
                        ..SchedConfig::default()
                    },
                );
                let stream = StreamKey::of("play-session-saturated");
                // No await anywhere in this loop, so the actor never runs.
                for seg in 0..64u32 {
                    s.note_playhead(stream, seg);
                }
            })
        });

        let dropped = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                (ck.key().name() == "pharos_transcode_playhead_dropped_total").then_some(v)
            });
        assert!(
            matches!(dropped, Some(DebugValue::Counter(n)) if n > 0),
            "a refused playhead update must be COUNTED: the snapshot shows a \
             reading that stopped moving, and this is the only signal that says \
             whether it stopped because the viewer stopped or because the \
             actor's inbox is saturated; got {dropped:?}"
        );
    }

    /// Mirrors `MAX_TRACKED_SESSIONS` in `PrefetchRegistry`: an unbounded
    /// playhead map is a leak dressed as a cache. Push the map one past its
    /// cap and check both halves of the contract — the size never exceeds it,
    /// and the entry evicted is the LEAST-recently-touched one, not an
    /// arbitrary one (which would risk dropping a stream still actively
    /// playing in favour of one touched once and abandoned).
    #[tokio::test]
    async fn the_playhead_map_is_bounded_and_evicts_the_stream_touched_longest_ago() {
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| WorkerRunResult::Done {
            out_bytes: 1,
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());

        let oldest = StreamKey::of("stream-oldest");
        s.submit(
            PathBuf::from("/m/a"),
            cmaf(),
            file_sink(),
            JobClass::Interactive,
            JobHint {
                stream: oldest,
                segment: Some(0),
                seeds_playhead: PlayheadSeed::Observes,
            },
        )
        .await
        .unwrap();

        // Touch 256 MORE distinct streams — one past the cap — so `oldest`
        // is now the least-recently-updated entry.
        let mut last = oldest;
        for n in 1..=256u32 {
            let stream = StreamKey::of(&format!("stream-{n}"));
            s.submit(
                PathBuf::from("/m/a"),
                cmaf(),
                file_sink(),
                JobClass::Interactive,
                JobHint {
                    stream,
                    segment: Some(n),
                    seeds_playhead: PlayheadSeed::Observes,
                },
            )
            .await
            .unwrap();
            last = stream;
        }

        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.len(),
            256,
            "the map must stay bounded at MAX_TRACKED_STREAMS regardless of how many \
             distinct streams have submitted"
        );
        assert!(
            !snap.playheads.contains_key(&oldest),
            "the least-recently-updated stream must be the one evicted, not an arbitrary one"
        );
        assert_eq!(
            snap.playheads.get(&last).copied(),
            Some(256),
            "the most recently touched stream must survive"
        );
    }

    /// What makes it safe for speculative work to wait at all: the client that
    /// joins it does not wait at its tier.
    ///
    /// The segment cache shares one encode between everybody who asks for that
    /// segment, so a client arriving behind a prefetch waits on the prefetch's
    /// job. If that job kept its class, the client would sit behind every guess
    /// in the queue and behind every other client's work — including work
    /// submitted after it started waiting. Promotion is what breaks that, and
    /// this asserts it beats the distance ordering rather than merely joining
    /// it: the promoted job is the FURTHEST from its playhead, so speculative
    /// ranking alone would dispatch it last of the three.
    #[tokio::test]
    async fn a_promoted_queued_job_outranks_every_speculative_one() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(120), move |_, spec| {
            seen.lock().unwrap().push(
                spec.input
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(1), spawner, SchedConfig::default());
        let stream = StreamKey::of("viewer");

        // Two client requests occupy both permits; the first also puts the
        // viewer's playhead at 100.
        let mut blockers = Vec::new();
        for (tag, seg) in [("block-1", Some(100)), ("block-2", None)] {
            let s2 = s.clone();
            blockers.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint {
                        stream,
                        segment: seg,
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;

        // `far` is six segments out; `near`/`mid` are one and two. On distance
        // alone `far` goes last.
        let slot = JobSlot::new();
        let mut watch_id = slot.subscribe();
        let mut handles = Vec::new();
        for (tag, seg, assigned) in [
            ("far", 106u32, Some(slot)),
            ("near", 101, None),
            ("mid", 102, None),
        ] {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit_tracked(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    JobClass::Background,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                    assigned,
                )
                .await
            }));
        }
        let far_id = await_job_id(&mut watch_id).await.expect("an assigned id");
        tokio::time::sleep(Duration::from_millis(30)).await;
        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(queued.pending_background, 3, "precondition: all three wait");

        // A client asks for `far`'s segment and coalesces onto it.
        s.promote(far_id).await;

        for h in blockers {
            h.await.unwrap().unwrap();
        }
        for h in handles {
            h.await.unwrap().expect("queued work must still complete");
        }
        let got: Vec<String> = order
            .lock()
            .unwrap()
            .iter()
            .filter(|n| !n.starts_with("block-"))
            .cloned()
            .collect();
        assert_eq!(
            got,
            ["far", "near", "mid"],
            "the job a client is blocked on must go first even though it is the \
             least urgent guess in the queue"
        );
    }

    /// Two things about the drain loop that a queue of interchangeable jobs
    /// cannot show, because they only appear when queued jobs have DIFFERENT
    /// eligible device sets. VP9 has no NVENC encoder, so a VP9 job here can
    /// only ever run on the CPU while an H264 job can use either device.
    ///
    /// 1. A job that cannot be placed must not stall the queue behind it. "The
    ///    most urgent candidate found no permit, so no less urgent one will
    ///    either" is false: `i1` is stuck waiting for the CPU while the GPU
    ///    permit that just freed is one `i2` could take. Stopping there leaves
    ///    an encoder idle for a whole encode.
    /// 2. ...and it must not lose its turn for being passed over. `i1` asked
    ///    before `i3`; putting passed-over work at the BACK of the queue would
    ///    hand `i3` the next CPU permit and push the client who has already
    ///    waited longest further back, one drain at a time.
    #[tokio::test]
    async fn a_queued_job_that_cannot_be_placed_neither_stalls_the_queue_nor_loses_its_turn() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(400), move |_, spec| {
            seen.lock().unwrap().push(
                spec.input
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(1), spawner, SchedConfig::default());
        let vp9 = || {
            let mut o = h264();
            o.video = Some(VideoCodec::Vp9);
            o
        };
        let spawn_job = |tag: &'static str, opts: TranscodeOptions| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    opts,
                    file_sink(),
                    JobClass::Interactive,
                    JobHint::default(),
                )
                .await
            })
        };

        // The GPU permit frees first (same encode duration, dispatched first),
        // while the CPU one is still held.
        let block_gpu = spawn_job("block-gpu", h264());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let block_cpu = spawn_job("block-cpu", vp9());
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Queued in arrival order: a CPU-only job, then two that could use
        // either device.
        let i1 = spawn_job("i1", vp9());
        tokio::time::sleep(Duration::from_millis(10)).await;
        let i2 = spawn_job("i2", h264());
        tokio::time::sleep(Duration::from_millis(10)).await;
        let i3 = spawn_job("i3", h264());

        for h in [block_gpu, block_cpu, i1, i2, i3] {
            h.await.unwrap().unwrap();
        }
        let got: Vec<String> = order
            .lock()
            .unwrap()
            .iter()
            .filter(|n| !n.starts_with("block-"))
            .cloned()
            .collect();
        assert_eq!(
            got,
            ["i2", "i1", "i3"],
            "i2 must take the freed GPU permit i1 could not use, and i1 must \
             still keep its place ahead of i3"
        );
    }

    /// The motivating case: two viewers, each prefetching a window. Speculative
    /// work used to be dropped the instant no permit was free, so whoever
    /// submitted first took all the capacity and the loser of the race took a
    /// cold interactive miss on every segment while the winner built a deep
    /// buffer.
    ///
    /// Arrival order and urgency order are made to DISAGREE on purpose: viewer
    /// A queues its whole window before viewer B queues anything, and inside
    /// each window the DISTANT segment is submitted first. A queue that still
    /// popped its front would dispatch `a-6` first and `b-1` last, so nothing
    /// asserted here is reachable by leaving the dispatch order alone.
    #[tokio::test]
    async fn every_streams_nearest_segment_is_served_before_any_streams_distant_one() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(120), move |_, spec| {
            seen.lock().unwrap().push(
                spec.input
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            WorkerRunResult::Done { out_bytes: 1 }
        });
        // One GPU permit plus the always-present CPU one. `h264()` (mpegts)
        // rather than `cmaf()`: a shared-init fMP4 job is pinned to a single
        // device, which would take the CPU permit out of play and make the
        // capacity under test something other than what it looks like.
        let s = TranscodeScheduler::spawn(one_gpu(1), spawner, SchedConfig::default());

        let a = StreamKey::of("viewer-a");
        let b = StreamKey::of("viewer-b");

        // Both viewers are at segment 100, and each says so the only way a
        // playhead is ever set: with a request a client is blocked on. The two
        // blockers also occupy every permit, so everything below must queue.
        let mut blockers = Vec::new();
        for (stream, tag) in [(a, "block-a"), (b, "block-b")] {
            let s2 = s.clone();
            blockers.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint {
                        stream,
                        segment: Some(100),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Viewer A submits its whole prefetch window before viewer B submits
        // anything — the exact submission order that used to starve B.
        let mut handles = Vec::new();
        for (stream, tag, ahead) in [(a, "a", 6u32), (a, "a", 1), (b, "b", 6), (b, "b", 1)] {
            let s2 = s.clone();
            let p = PathBuf::from(format!("/m/{tag}-{ahead}"));
            handles.push(tokio::spawn(async move {
                s2.submit(
                    p,
                    h264(),
                    file_sink(),
                    JobClass::Background,
                    JobHint {
                        stream,
                        segment: Some(100 + ahead),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.pending_background, 4,
            "speculative work must wait for a permit, not be dropped on the floor"
        );

        for h in blockers {
            h.await.unwrap().unwrap();
        }
        for h in handles {
            h.await
                .unwrap()
                .expect("queued speculative work must still complete");
        }

        let got: Vec<String> = order
            .lock()
            .unwrap()
            .iter()
            .filter(|n| !n.starts_with("block-"))
            .cloned()
            .collect();
        assert_eq!(got.len(), 4, "every queued job must have run: {got:?}");
        let first_two: std::collections::BTreeSet<&str> =
            got[..2].iter().map(String::as_str).collect();
        assert_eq!(
            first_two,
            ["a-1", "b-1"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "both viewers' next-needed segment must run before either viewer's \
             distant one, whatever order they were submitted in: {got:?}"
        );
    }

    /// V58's first clause has no exemption for narrow pools.
    ///
    /// The reserve used to be clamped to the candidate pool's capacity minus
    /// one, which resolved the caller of a job that could otherwise never be
    /// admitted — but on a ONE-permit pool that clamp is `min(1, 0) == 0`, so
    /// the reserve vanished and speculation was free to take the only permit a
    /// client needed. A pinned shared-init rendition (V80) resolves to exactly
    /// one device, so a one-permit candidate pool is not hypothetical.
    ///
    /// Refusing outright resolves the caller just as promptly and keeps the
    /// reserve. The device must be left untouched, which is the part the old
    /// clamp got wrong.
    #[tokio::test]
    async fn a_pool_that_cannot_hold_the_reserve_sheds_speculation_rather_than_queueing_it() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(400), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        // One permit, and `background_headroom` reserves one.
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig::default(),
        );

        let got = s
            .submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
                JobHint::default(),
            )
            .await;
        assert_eq!(
            got.err(),
            Some(SchedError::Busy),
            "a reserve that can never be satisfied must shed, not park a caller \
             on a condition that cannot arrive"
        );
        let snap = s.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.devices.iter().map(|d| d.in_use).sum::<usize>(),
            0,
            "the permit the reserve exists to hold open must still be free"
        );
        assert_eq!(snap.pending, 0, "and nothing must be left in the queue");
    }

    /// `BackgroundAdmission::Never` used to reply `SchedError::Busy` —
    /// "deliberate load management" — unconditionally, even when the reason
    /// the candidate pool could no longer hold the reserve was that a PRIOR
    /// attempt on this exact job hit a transient worker failure and excluded
    /// the device that carried the pool's spare capacity. That collapses a
    /// real cause into a bare class, which this project's error discipline
    /// forbids: a diagnostic must carry the offending value, never a class
    /// alone. A caller reading `Busy` here has no way to learn a device
    /// actually failed.
    ///
    /// The shape: one hw permit + one CPU permit, `background_headroom` 1
    /// (the default, so CPU alone can never hold it). The hw device is
    /// tried first (`eligible_for_h264_lists_hw_first_then_cpu`) and fails
    /// transiently, which excludes it and retries — leaving CPU alone as the
    /// candidate pool, capacity 1, exactly the reserve, so
    /// `background_admission` returns `Never` on the retry. The reply must
    /// be `Failed(DeviceBusy)`, the real cause, not `Busy`.
    #[tokio::test]
    async fn a_background_job_shed_after_its_own_transient_failure_reports_the_real_cause() {
        let gpu = DeviceId::hw(HwAccel::Nvenc, 0);
        let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, move |_, spec| {
            if spec.device == gpu {
                WorkerRunResult::Failed(WorkerError::DeviceBusy)
            } else {
                WorkerRunResult::Done { out_bytes: 1 }
            }
        });
        // One hw permit (fails first attempt), one CPU permit — CPU alone
        // exactly meets `background_headroom` (1), so once hw is excluded
        // the pool can never hold the reserve.
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[(gpu, 1)], 1),
            spawner,
            SchedConfig::default(),
        );

        let got = s
            .submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
                JobHint::default(),
            )
            .await;
        match got {
            Err(SchedError::Failed(WorkerError::DeviceBusy)) => {}
            other => panic!(
                "a job shed after its own transient failure must report that \
                 failure as the cause, not a bare load-shed `Busy`; got {other:?}"
            ),
        }
    }

    /// `pending_cap` is client backpressure, and speculative work now shares it.
    ///
    /// A queued job whose caller has gone is only noticed inside
    /// `try_place_no_queue`, which only a SELECTED job reaches — and under
    /// saturation speculative work is never selected (tier is absolute) and the
    /// drain stops the moment no permit is free. So orphaned prefetch, which
    /// every seek and every track swap produces a window of, sat in `pending`
    /// until the pool went quiet: exactly the opposite of when the cap matters.
    /// A queue full of work nobody is waiting for then replies `Busy` to a
    /// client.
    ///
    /// Here the drain runs, places the one job somebody IS waiting for, and
    /// stops with the pool full — so nothing else in the queue is ever examined.
    #[tokio::test]
    async fn abandoned_speculation_must_not_fill_the_queue_a_client_needs() {
        let spawner = Arc::new(VariableSpawner(Arc::new(|spec: &JobSpec| {
            match job_name(spec).as_str() {
                // Both of these outlive the whole observation, so the pool is
                // full again the moment the drain has placed `waiter` and
                // nothing further in the queue is ever looked at.
                "hold-long" | "waiter" => Duration::from_millis(800),
                // Frees one permit — and with it a drain — while the other is
                // still held, so the drain has exactly one job's worth of room.
                "hold-short" => Duration::from_millis(150),
                _ => Duration::from_millis(20),
            }
        })));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 2),
            spawner,
            SchedConfig {
                pending_cap: 4,
                ..SchedConfig::default()
            },
        );
        let submit = |tag: &str, class: JobClass| {
            let s2 = s.clone();
            let p = PathBuf::from(format!("/m/{tag}"));
            tokio::spawn(async move {
                s2.submit(p, h264(), file_sink(), class, JobHint::default())
                    .await
            })
        };

        let long = submit("hold-long", JobClass::Interactive);
        let short = submit("hold-short", JobClass::Interactive);
        tokio::time::sleep(Duration::from_millis(40)).await;

        // Three prefetches queue, and then their callers go — a seek, a track
        // swap, a client that closed the connection. Aborting the task drops
        // the `submit()` future and with it the reply receiver, which is
        // precisely what those do.
        let orphans: Vec<_> = (0..3)
            .map(|i| submit(&format!("orphan-{i}"), JobClass::Background))
            .collect();
        // One job somebody IS waiting for, queued behind them.
        let waiter = submit("waiter", JobClass::Interactive);
        tokio::time::sleep(Duration::from_millis(40)).await;
        let before = s.snapshot().await.expect("snapshot");
        assert_eq!(
            (before.pending_background, before.pending_interactive),
            (3, 1),
            "precondition: the queue must be full, three of it speculative"
        );
        for o in &orphans {
            o.abort();
        }
        for o in orphans {
            assert!(o.await.is_err(), "the orphan's caller must really be gone");
        }

        // `hold-short` finishes, the drain places `waiter` in the freed permit,
        // and the pool is full again with the orphans never examined.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after = s.snapshot().await.expect("snapshot");
        assert_eq!(
            after.pending_background, 0,
            "work whose caller has gone must not still be occupying the queue"
        );

        // ...and the cap those entries were holding must be a client's to use.
        let c1 = submit("client-1", JobClass::Interactive);
        let c2 = submit("client-2", JobClass::Interactive);
        for (tag, h) in [
            ("hold-short", short),
            ("waiter", waiter),
            ("client-1", c1),
            ("client-2", c2),
            ("hold-long", long),
        ] {
            h.await
                .unwrap()
                .unwrap_or_else(|e| panic!("{tag} was refused: {e:?}"));
        }
    }

    /// The same fact for staleness, which had the same placement bug and a
    /// worse consequence.
    ///
    /// Dropping stale work only where a SELECTED job reaches it is dropping it
    /// nowhere under load: `next_to_dispatch` sorts outrun work last by design,
    /// so it is examined only once it becomes the minimum, and under saturation
    /// every freed permit goes to a live job and the drain exits with the band
    /// untouched. The jobs then sat in `pending` — the cap V58 calls client
    /// backpressure — until the pool went quiet.
    ///
    /// The signal half is what makes it block rather than merely disappoint. A
    /// stale job that eventually leaves by eviction is counted `evicted`, so
    /// `stale` — the arm whose whole purpose is to say "prefetch depth is tuned
    /// too far ahead" — under-reported precisely under saturation, the only
    /// load at which anyone reads it. Both halves are asserted: the queue is
    /// empty of it, and the counter says so, both DURING the run rather than
    /// after the pool drains, because after the pool drains the old behaviour
    /// gets the same numbers eventually.
    #[test]
    fn stale_speculation_leaves_the_queue_under_load_and_is_counted_where_it_is_read() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let spawner = Arc::new(VariableSpawner(Arc::new(|spec: &JobSpec| {
                    match job_name(spec).as_str() {
                        // Outlive the whole observation, so the pool is full
                        // again the moment the drain has placed `waiter` and
                        // nothing further in the queue is ever selected.
                        "hold-long" | "waiter" => Duration::from_millis(900),
                        // Frees one permit — and with it a drain — while the
                        // other is still held.
                        "hold-short" => Duration::from_millis(150),
                        _ => Duration::from_millis(20),
                    }
                })));
                let s = TranscodeScheduler::spawn(
                    DeviceTable::from_probe(&[], 2),
                    spawner,
                    SchedConfig {
                        pending_cap: 4,
                        ..SchedConfig::default()
                    },
                );
                let stream = StreamKey::of("viewer");
                let submit = |tag: String, class: JobClass, seg: u32| {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit(
                            PathBuf::from(format!("/m/{tag}")),
                            h264(),
                            file_sink(),
                            class,
                            JobHint {
                                stream,
                                segment: Some(seg),
                                seeds_playhead: PlayheadSeed::Observes,
                            },
                        )
                        .await
                    })
                };

                // Both permits taken by the client, playhead at 100.
                let long = submit("hold-long".into(), JobClass::Interactive, 100);
                let short = submit("hold-short".into(), JobClass::Interactive, 100);
                tokio::time::sleep(Duration::from_millis(40)).await;

                // Three prefetches queue, all AHEAD of the playhead and all
                // good work at this moment.
                let prefetch: Vec<_> = (0..3)
                    .map(|i| submit(format!("prefetch-{i}"), JobClass::Background, 101 + i))
                    .collect();
                // ...and then the viewer jumps, leaving all three behind. This
                // request is also the one job somebody IS waiting for.
                let waiter = submit("waiter".into(), JobClass::Interactive, 200);
                tokio::time::sleep(Duration::from_millis(40)).await;

                let before = s.snapshot().await.expect("snapshot");
                assert_eq!(
                    (before.pending_background, before.pending_interactive),
                    (3, 1),
                    "precondition: the queue must be full, three of it speculative"
                );

                // `hold-short` finishes, the drain places `waiter` in the freed
                // permit, and the pool is full again — so under the old
                // placement none of the three is ever examined.
                tokio::time::sleep(Duration::from_millis(220)).await;
                let after = s.snapshot().await.expect("snapshot");
                assert_eq!(
                    after.pending_background, 0,
                    "speculation the viewer outran must not still be occupying \
                     the cap a client needs"
                );
                let m = Metrics::capture(&snapshotter);
                assert_eq!(
                    m.counter(
                        "pharos_transcode_queue_outcome_total",
                        &["class=background", "outcome=stale"],
                    ),
                    3,
                    "...and it must be counted as `stale` while the pool is \
                     still saturated, which is when the arm is read"
                );

                for (i, h) in prefetch.into_iter().enumerate() {
                    let r = h.await.unwrap();
                    assert!(
                        matches!(r, Err(SchedError::Busy)),
                        "prefetch-{i} must be told its bytes are no longer \
                         wanted rather than left waiting: {r:?}"
                    );
                }
                for (tag, h) in [
                    ("hold-short", short),
                    ("waiter", waiter),
                    ("hold-long", long),
                ] {
                    h.await
                        .unwrap()
                        .unwrap_or_else(|e| panic!("{tag} was refused: {e:?}"));
                }
            })
        });
    }

    /// A viewer outruns their own prefetch, which under saturation is the
    /// normal case rather than an edge — the queue only gets deep when encodes
    /// are slower than playback, and that is exactly when a playhead passes
    /// segments still sitting in it.
    ///
    /// `lookahead_distance` is signed so "already passed" is representable, and
    /// ascending order on the raw value then ranks the most useless job in the
    /// queue as the most urgent one: distance `-5` ahead of distance `1`, the
    /// segment the client needs next. That spends a permit on bytes nobody will
    /// fetch and delays the ones somebody will. Nothing else catches it —
    /// `reply.is_closed()` only fires when a seek or a track swap aborted the
    /// prefetch, and a playhead simply advancing does neither.
    ///
    /// Submission order is stale-first on purpose, so FIFO cannot produce the
    /// expected order either. `distance.max(0)` cannot produce it either: it
    /// would tie the stale job with distance 0 and hand it the tiebreak on
    /// arrival.
    ///
    /// The viewer ADVANCES here rather than the job being submitted behind a
    /// standing playhead: staleness is being outrun, and a job submitted behind
    /// a playhead that never moved is a deliberate backward guess instead (see
    /// `is_stale` and
    /// `a_backward_prewarm_survives_while_work_the_viewer_outran_still_dies`).
    ///
    /// Ranking passed work last was the first half of the answer and is no
    /// longer the whole of it: a job the viewer has gone past is now DROPPED
    /// when it is examined, because a permit spent on it buys nothing at any
    /// position in the order. The ordering still decides which of the two
    /// things happens first — live speculation is dispatched nearest-first
    /// before any passed job is looked at — and it is what picks the victim
    /// when a full queue has to evict (`queue_or_refuse`), so both halves are
    /// asserted here.
    #[tokio::test]
    async fn work_the_viewer_has_already_passed_is_dropped_rather_than_dispatched_last() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(120), move |_, spec| {
            seen.lock().unwrap().push(job_name(spec));
            WorkerRunResult::Done { out_bytes: 1 }
        });
        // One permit, so the queue drains strictly one job at a time and the
        // recorded order IS the dispatch order. `background_headroom` has to be
        // 0 to say that: a pool that cannot hold the reserve and a job at once
        // sheds speculation outright rather than queueing it, and there would
        // be no queue left to rank. Ranking is what is under test here; the
        // reserve has its own guards.
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let stream = StreamKey::of("viewer");

        let interactive = |tag: &'static str, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    JobClass::Interactive,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        // The client's own request occupies the one permit and puts the
        // playhead at 90 — the only way a playhead is ever set.
        let blocker = interactive("block", 90);
        tokio::time::sleep(Duration::from_millis(30)).await;

        // All three are AHEAD of the playhead when submitted, so all three are
        // good work at that moment. Submitted stale-first.
        let mut handles = Vec::new();
        for (tag, seg) in [("stale", 95u32), ("next", 101), ("far", 110)] {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    JobClass::Background,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        let snap = s.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.pending_background, 3,
            "precondition: all three must be waiting, or there is no order to rank"
        );
        // ...and now the viewer moves to 100, leaving `stale` behind it. This
        // is what makes it stale: not where it sits, but being overtaken.
        let advance = interactive("advance", 100);
        tokio::time::sleep(Duration::from_millis(30)).await;

        blocker.await.unwrap().expect("the client's own segment");
        advance.await.unwrap().expect("the client's next segment");
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        // Submitted first, so this is `results[0]`.
        assert!(
            matches!(results[0], Err(SchedError::Busy)),
            "a segment the viewer has gone past must be dropped, and its \
             caller told so rather than left waiting: {results:?}"
        );
        for (tag, r) in ["next", "far"].iter().zip(&results[1..]) {
            assert!(r.is_ok(), "{tag} must still complete: {r:?}");
        }
        let got: Vec<String> = order
            .lock()
            .unwrap()
            .iter()
            .filter(|n| *n != "block" && *n != "advance")
            .cloned()
            .collect();
        assert_eq!(
            got,
            ["next", "far"],
            "a segment the viewer already went past must neither outrank the \
             one they need next nor be encoded at all: {got:?}"
        );
    }

    /// The other half of the same fact, and the one that says the distance is
    /// judged at DISPATCH rather than frozen at submit: every job here was
    /// still AHEAD of the playhead when it was submitted, and one of them stops
    /// being so while it waits.
    ///
    /// A client seeking forward is the ordinary way this happens. It does not
    /// abort the prefetch tasks it just orphaned — `reply.is_closed()` fires
    /// only when a seek CANCELS the request, and the prefetch for the segments
    /// between the old playhead and the new one is simply left behind, still
    /// queued, still wanted by nobody.
    ///
    /// The positive control matters as much as the drop: `live`, submitted at
    /// the same moment and one segment ahead of where the client landed, must
    /// still be encoded. "Background work stopped running" would pass a test
    /// that only checked the stale job.
    #[tokio::test]
    async fn a_prefetch_the_viewer_outruns_while_it_waits_is_dropped_at_dispatch() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                // Holds the one permit while everything else piles up behind
                // it, so the whole queue is in place before anything drains.
                "blocker" => Duration::from_millis(300),
                _ => Duration::from_millis(60),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        // One permit, so the queue drains one job at a time and the recorded
        // order IS the dispatch order. `background_headroom: 0` for the reason
        // given in the ranking test: a pool that cannot hold the reserve and a
        // job at once sheds speculation instead of queueing it, and there would
        // be no queue to examine.
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let stream = StreamKey::of("viewer");

        let submit = |tag: &'static str, class: JobClass, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        // The client is at 100 and its own request holds the permit.
        let blocker = submit("blocker", JobClass::Interactive, 100);
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Prefetch for the next segment — one AHEAD of the playhead, so this is
        // perfectly good work at the moment it is queued.
        let stale = submit("stale", JobClass::Background, 101);
        tokio::time::sleep(Duration::from_millis(30)).await;
        // ...and then the viewer seeks to 140. Segment 101 will never be asked
        // for; 141 is now the next one.
        let seek = submit("seek", JobClass::Interactive, 140);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let live = submit("live", JobClass::Background, 141);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            (queued.pending_background, queued.pending_interactive),
            (2, 1),
            "precondition: all three must be waiting behind the blocker, or \
             nothing is examined at dispatch"
        );

        blocker.await.unwrap().expect("the client's first segment");
        seek.await.unwrap().expect("the client's seek target");
        live.await
            .unwrap()
            .expect("speculation still ahead of the playhead must survive");
        let refused = stale.await.unwrap();
        assert!(
            matches!(refused, Err(SchedError::Busy)),
            "a prefetch the viewer outran must be dropped: {refused:?}"
        );

        let got = order.lock().unwrap().clone();
        assert_eq!(
            got,
            ["blocker", "seek", "live"],
            "the segment the client left behind must never reach an encoder: \
             {got:?}"
        );
    }

    /// Behind the playhead is not the same as outrun, and the queue must not
    /// treat a deliberate backward guess as a leftover.
    ///
    /// The SyncPlay seek prewarm submits `Background` for the seek TARGET on a
    /// member's EXISTING stream, before that member's own interactive request
    /// moves the playhead there. On a backward group seek every one of those
    /// jobs is negative the instant it arrives — so a drop keyed on the sign
    /// alone silently deletes a shipped feature, and deletes it only under
    /// saturation (with a free permit the same job goes straight through
    /// `place`, which never checks). Load-dependent, and the only trace is a
    /// `stale` increment that looks exactly like a genuinely wasted guess.
    ///
    /// Both directions in one test, on two streams so neither disarms the
    /// other: `prewarm` is behind its viewer and must survive; `outrun` was
    /// ahead of its viewer when submitted, was overtaken while it waited, and
    /// must still die. Reverting `is_stale` to the bare sign test kills
    /// `prewarm`; removing the drop encodes `outrun`.
    #[tokio::test]
    async fn a_backward_prewarm_survives_while_work_the_viewer_outran_still_dies() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                // Holds the one permit until the whole queue is in place.
                "blocker" => Duration::from_millis(400),
                _ => Duration::from_millis(40),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        // Two viewers. One is about to be rewound by its group; the other just
        // keeps playing forward and leaves its own prefetch behind.
        let rewinder = StreamKey::of("rewinder");
        let runner = StreamKey::of("runner");

        let submit = |tag: &'static str, class: JobClass, stream: StreamKey, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        // The rewinder is at 500 and its own request holds the only permit.
        let blocker = submit("blocker", JobClass::Interactive, rewinder, 500);
        tokio::time::sleep(Duration::from_millis(40)).await;
        // The group seeks back to 100: warm the landing segment BEFORE the
        // member asks for it. Distance -400 the moment it is submitted, and
        // deliberately so.
        let prewarm = submit("prewarm", JobClass::Background, rewinder, 100);
        tokio::time::sleep(Duration::from_millis(20)).await;

        // The other viewer, on its own stream: a request at 10, prefetch for
        // 11 (good work when submitted), then a jump to 40 that leaves it
        // behind.
        let head = submit("head", JobClass::Interactive, runner, 10);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let outrun = submit("outrun", JobClass::Background, runner, 11);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let jump = submit("jump", JobClass::Interactive, runner, 40);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            (queued.pending_background, queued.pending_interactive),
            (2, 2),
            "precondition: everything must be waiting behind the blocker, or \
             nothing is examined at dispatch"
        );

        blocker.await.unwrap().expect("the rewinder's own segment");
        head.await.unwrap().expect("the runner's own segment");
        jump.await.unwrap().expect("the runner's seek target");
        prewarm
            .await
            .unwrap()
            .expect("a seek target warmed before the member asked for it is not stale");
        let dropped = outrun.await.unwrap();
        assert!(
            matches!(dropped, Err(SchedError::Busy)),
            "a prefetch its viewer overtook must still be dropped: {dropped:?}"
        );

        let got = order.lock().unwrap().clone();
        assert!(
            got.contains(&"prewarm".to_string()),
            "the backward seek target must reach an encoder: {got:?}"
        );
        assert!(
            !got.contains(&"outrun".to_string()),
            "the segment the runner left behind must never reach an encoder: \
             {got:?}"
        );
    }

    /// The member is still PLAYING while the prewarm for its seek target
    /// waits — and that must not condemn the prewarm.
    ///
    /// `prewarm_group_seek` fires the moment `/SyncPlay/Seek` is dispatched,
    /// which is seconds before any client applies the command. During those
    /// seconds the member keeps playing forward, and under saturation — the only
    /// load at which the prewarm queues at all — prefetch is being shed, so the
    /// member's next segment request misses the cache and reaches the scheduler
    /// as an Interactive submission on the SAME stream.
    ///
    /// That forward request moves the playhead by one and, under a staleness
    /// test keyed on "has the playhead moved since?", turned the queued backward
    /// prewarm into a leftover — the original failure with a narrower but
    /// entirely ordinary trigger. Keying on the playhead's VALUE at admission
    /// instead states the property directly: the submitter saw 500 and asked for
    /// 100 anyway, and nothing the viewer does afterwards changes what the
    /// submitter saw.
    ///
    /// Distinct from
    /// `a_backward_prewarm_survives_while_work_the_viewer_outran_still_dies`,
    /// which only ever moves the rewinder's playhead TO the target (where the
    /// distance goes to 0 and the sign test is satisfied anyway). Here it moves
    /// AWAY, which is the case that test cannot see.
    #[tokio::test]
    async fn a_member_still_playing_forward_does_not_stale_its_own_seek_prewarm() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                "blocker" => Duration::from_millis(400),
                _ => Duration::from_millis(40),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let member = StreamKey::of("group-member");

        let submit = |tag: &'static str, class: JobClass, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream: member,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        // The member is at 500 and its own request holds the only permit.
        let blocker = submit("blocker", JobClass::Interactive, 500);
        tokio::time::sleep(Duration::from_millis(40)).await;
        // The group seeks back to 100; the server warms the landing segment
        // before the member has applied the command.
        let prewarm = submit("prewarm", JobClass::Background, 100);
        tokio::time::sleep(Duration::from_millis(20)).await;
        // ...and while that waits, the member — which has not applied the seek
        // yet — asks for the segment AFTER the one it is playing.
        let forward = submit("forward", JobClass::Interactive, 501);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            (queued.pending_background, queued.pending_interactive),
            (1, 1),
            "precondition: both must be waiting behind the blocker, or the \
             sweep never examines the prewarm"
        );
        assert_eq!(
            queued.playheads.get(&member).copied(),
            Some(501),
            "precondition: the member's playhead must have moved FORWARD, away \
             from the seek target"
        );

        blocker.await.unwrap().expect("the member's own segment");
        forward
            .await
            .unwrap()
            .expect("the member's next segment, still playing forward");
        prewarm.await.unwrap().expect(
            "a seek target warmed before the member applied the seek must \
             survive that member playing forward in the meantime",
        );

        let got = order.lock().unwrap().clone();
        assert!(
            got.contains(&"prewarm".to_string()),
            "the backward seek target must still reach an encoder: {got:?}"
        );
    }

    /// Surviving the sweep is not the same as being ranked as live work.
    ///
    /// A deliberate backward guess at −400 has magnitude 400 under `abs`, which
    /// is larger than every forward guess in the queue — so the seek target was
    /// dispatched LAST among speculation, i.e. after the member had already
    /// arrived at it. Nothing is dropped and no counter moves; the feature is
    /// simply defeated. Magnitude 0 says what it means: the viewer is about to
    /// be there, so it goes first.
    ///
    /// Two background jobs on the rewinder's own stream, because one cannot
    /// show an ordering.
    #[tokio::test]
    async fn a_backward_prewarm_is_dispatched_before_the_forward_guesses_beside_it() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                "blocker" => Duration::from_millis(400),
                _ => Duration::from_millis(40),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let rewinder = StreamKey::of("rewinder");

        let submit = |tag: &'static str, class: JobClass, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream: rewinder,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        // The member is at 500 and its own request holds the only permit.
        let blocker = submit("blocker", JobClass::Interactive, 500);
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Its ordinary prefetch: the next segment it would play if the group
        // had not just seeked. Distance +5.
        let forward = submit("forward", JobClass::Background, 505);
        tokio::time::sleep(Duration::from_millis(20)).await;
        // The group seeks back to 100. Distance −400 — deliberately, and the
        // segment the member will ask for first.
        let prewarm = submit("prewarm", JobClass::Background, 100);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            queued.pending_background, 2,
            "precondition: both guesses must be waiting behind the blocker, or \
             nothing is ranked at all"
        );

        blocker.await.unwrap().expect("the member's own segment");
        prewarm.await.unwrap().expect("the seek target");
        forward.await.unwrap().expect("the ordinary guess");

        let got = order.lock().unwrap().clone();
        assert_eq!(
            got,
            ["blocker", "prewarm", "forward"],
            "the segment the group is about to land on must be encoded before \
             a guess about a future the group has just cancelled: {got:?}"
        );
    }

    /// ...and at a full queue it must be the LAST thing given up, not the
    /// first.
    ///
    /// `queue_or_refuse` reads the same key at its maximum, and a deliberate
    /// backward guess is not excluded from candidacy — its distance is −400,
    /// not `i64::MAX`. Under `abs` it was therefore the preferred victim for
    /// the next arrival with any smaller magnitude, and left as `evicted`.
    /// Magnitude 0 makes it the least evictable speculative job present, which
    /// is what "the viewer is about to be there" means at the other end of the
    /// order.
    #[tokio::test]
    async fn a_full_queue_gives_up_a_forward_guess_before_a_backward_prewarm() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                "blocker" => Duration::from_millis(500),
                _ => Duration::from_millis(40),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        // One permit, room for two queued jobs.
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                pending_cap: 2,
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let rewinder = StreamKey::of("rewinder");

        let submit = |tag: &'static str, class: JobClass, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream: rewinder,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        // The member is at 500 and holds the only permit.
        let blocker = submit("blocker", JobClass::Interactive, 500);
        tokio::time::sleep(Duration::from_millis(30)).await;
        // The seek prewarm and one ordinary forward guess fill the queue.
        let prewarm = submit("prewarm", JobClass::Background, 100);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let forward = submit("forward", JobClass::Background, 505);
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A third guess arrives at a full queue, nearer than `forward` (+1) but
        // still a guess about a future the group has cancelled. It may take
        // `forward`'s slot; it may not take the seek target's.
        let nearer = submit("nearer", JobClass::Background, 501);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            queued.pending_background, 2,
            "precondition: the queue must be at its cap, never above it"
        );

        blocker.await.unwrap().expect("the member's own segment");
        let evicted = forward.await.unwrap();
        assert!(
            matches!(evicted, Err(SchedError::Busy)),
            "the forward guess must be the one given up: {evicted:?}"
        );
        prewarm
            .await
            .unwrap()
            .expect("the segment the group is about to land on must survive a full queue");
        nearer.await.unwrap().expect("the arrival that beat it");

        let got = order.lock().unwrap().clone();
        assert_eq!(
            got,
            ["blocker", "prewarm", "nearer"],
            "a full queue must keep the seek target: {got:?}"
        );
    }

    /// Where the line is, and it is not `<= 0`.
    ///
    /// `lookahead_distance` measures against the last segment the client ASKED
    /// FOR, not the last it finished — so distance 0 is the segment being
    /// fetched right now: wanted, and not yet produced. It is also the segment
    /// `next_to_dispatch` ranks FIRST among speculation, so dropping it would
    /// have the queue destroy the very job it had just called the most urgent
    /// thing in it.
    ///
    /// Both sides are asserted in one place because each disarms a different
    /// mistake: widening the test to `<= 0` kills `standing`, and removing the
    /// drop entirely encodes `passed`.
    #[tokio::test]
    async fn the_segment_the_viewer_is_standing_on_is_not_treated_as_passed() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                "blocker" => Duration::from_millis(300),
                _ => Duration::from_millis(60),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let stream = StreamKey::of("viewer");

        let submit = |tag: &'static str, class: JobClass, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        // The client's own request holds the permit and puts the playhead at
        // 98. Both speculative jobs are AHEAD of it when submitted, so the
        // viewer has to overtake one of them for staleness to be in play at
        // all (see `is_stale`).
        let blocker = submit("blocker", JobClass::Interactive, 98);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let standing = submit("standing", JobClass::Background, 100);
        let passed = submit("passed", JobClass::Background, 99);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            queued.pending_background, 2,
            "precondition: both must be waiting, or neither boundary is tested"
        );

        // The client now asks for 100, so 100 is what it is waiting for: 99 is
        // behind it, 100 is the segment being fetched right now.
        let advance = submit("advance", JobClass::Interactive, 100);
        tokio::time::sleep(Duration::from_millis(30)).await;

        blocker.await.unwrap().expect("the client's own segment");
        advance.await.unwrap().expect("the client's next segment");
        standing
            .await
            .unwrap()
            .expect("the segment the viewer is standing on is still wanted");
        let refused = passed.await.unwrap();
        assert!(
            matches!(refused, Err(SchedError::Busy)),
            "the segment BEHIND the playhead must be dropped: {refused:?}"
        );

        let got = order.lock().unwrap().clone();
        assert_eq!(
            got,
            ["blocker", "advance", "standing"],
            "distance 0 is the segment being fetched, not one that has been \
             played past: {got:?}"
        );
    }

    /// A FIFO that is full refuses the NEWEST arrival, and for speculative work
    /// the newest arrival is systematically the MOST urgent thing present:
    /// prefetch is submitted in playback order, so the job that just arrived is
    /// the one closest to the playhead while the incumbents are the deep
    /// guesses submitted when the client was further back. Overflow therefore
    /// threw away exactly the segment the client was about to want and kept the
    /// one it would reach in a minute — if it ever did.
    ///
    /// Two halves, both asserted here because each without the other is a
    /// different bug: eviction must take the LEAST urgent job, and it must only
    /// happen when the arrival actually beats it. `further`, submitted last and
    /// ten segments beyond anything queued, is refused rather than admitted at
    /// the cost of work nearer the viewer.
    ///
    /// Asserted as an ORDER of encodes plus the queue's own occupancy, not as
    /// error codes alone: FIFO overflow would refuse `near` and `further` and
    /// encode `mid` then `far`, so no arrangement of "did nothing" produces
    /// this.
    #[tokio::test]
    async fn a_full_queue_evicts_its_least_urgent_speculation_not_its_newest_arrival() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                "blocker" => Duration::from_millis(400),
                _ => Duration::from_millis(60),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        // One permit and room for two queued jobs. `background_headroom: 0` for
        // the usual reason: a pool that cannot hold the reserve and a job at
        // once sheds speculation outright, and there would be no queue to fill.
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                pending_cap: 2,
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let stream = StreamKey::of("viewer");

        let submit = |tag: &'static str, class: JobClass, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        // The client is at 100 and holds the only permit.
        let blocker = submit("blocker", JobClass::Interactive, 100);
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Two deep guesses fill the queue...
        let far = submit("far", JobClass::Background, 110);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mid = submit("mid", JobClass::Background, 105);
        tokio::time::sleep(Duration::from_millis(20)).await;
        // ...and then the segment the client will ask for next arrives to a
        // full queue.
        let near = submit("near", JobClass::Background, 101);
        tokio::time::sleep(Duration::from_millis(20)).await;
        // ...as does one deeper than anything already queued, which must NOT
        // displace work nearer the viewer.
        let further = submit("further", JobClass::Background, 120);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            queued.pending_background, 2,
            "precondition: the queue must be at its cap, never above it"
        );

        blocker.await.unwrap().expect("the client's own segment");
        let far = far.await.unwrap();
        assert!(
            matches!(far, Err(SchedError::Busy)),
            "the least urgent queued job must be the one evicted: {far:?}"
        );
        let further = further.await.unwrap();
        assert!(
            matches!(further, Err(SchedError::Busy)),
            "an arrival that does not beat the incumbents must be refused, not \
             admitted at their expense: {further:?}"
        );
        near.await
            .unwrap()
            .expect("the segment the client needs next must have taken the slot");
        mid.await.unwrap().expect("the surviving incumbent");

        let got = order.lock().unwrap().clone();
        assert_eq!(
            got,
            ["blocker", "near", "mid"],
            "a full queue must keep the work nearest the viewer: {got:?}"
        );
    }

    /// A second viewer pressing play must not lose its opening segments to a
    /// guess a minute ahead of somebody already playing.
    ///
    /// `prewarm_cold_start` submits `Background` against a brand-new
    /// `StreamKey` that no interactive request has ever touched, so its
    /// lookahead distance was `i64::MAX` by construction for its whole life —
    /// which `queue_or_refuse`, reading the same key at its MAXIMUM, made the
    /// second most useless thing in the queue. The prewarm was refused at a
    /// full queue however deep the incumbents were, and evicted first if it
    /// ever got in. The new viewer then took exactly the opening
    /// `fragLoadTimeOut` the prewarm exists to prevent.
    ///
    /// Both halves, because each without the other still loses the feature:
    /// `prewarm` must be able to displace a deep incumbent when it ARRIVES, and
    /// must not be the victim when the next arrival displaces something.
    /// Reverting the seed refuses it outright; ranking it as unknown again
    /// makes `closer` take it instead of `mid`.
    #[tokio::test]
    async fn a_new_viewers_cold_start_beats_a_deep_guess_at_a_full_queue() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                "blocker" => Duration::from_millis(500),
                _ => Duration::from_millis(40),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                pending_cap: 2,
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let incumbent = StreamKey::of("already-playing");
        // Never named by an interactive request — this is what a session looks
        // like at `PlaybackInfo`, before the client has fetched anything.
        let newcomer = StreamKey::of("just-pressed-play");

        // Only the cold-start prewarm claims to know where its stream begins;
        // the incumbent's own prefetch is an ordinary guess.
        let submit = |tag: &'static str, class: JobClass, stream: StreamKey, seg: u32| {
            let s2 = s.clone();
            let seeds_playhead = if tag == "prewarm" {
                PlayheadSeed::StatesTheStart
            } else {
                PlayheadSeed::Observes
            };
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead,
                    },
                )
                .await
            })
        };

        // The incumbent viewer is at 100 and holds the only permit.
        let blocker = submit("blocker", JobClass::Interactive, incumbent, 100);
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Its own prefetch fills the queue: a minute out, and six segments out.
        let deep = submit("deep", JobClass::Background, incumbent, 112);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mid = submit("mid", JobClass::Background, incumbent, 106);
        tokio::time::sleep(Duration::from_millis(20)).await;

        // A second viewer presses play. Its prewarm is the first thing anybody
        // has ever said about this stream, and it must beat `deep`.
        let prewarm = submit("prewarm", JobClass::Background, newcomer, 300);
        tokio::time::sleep(Duration::from_millis(20)).await;
        // ...and then the incumbent guesses three ahead, which must take `mid`
        // — the least urgent thing left — and not the newcomer's opening
        // segment.
        let closer = submit("closer", JobClass::Background, incumbent, 103);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            queued.pending_background, 2,
            "precondition: the queue must be at its cap, never above it"
        );

        blocker.await.unwrap().expect("the client's own segment");
        let deep = deep.await.unwrap();
        assert!(
            matches!(deep, Err(SchedError::Busy)),
            "a guess a minute out must lose to a viewer pressing play: {deep:?}"
        );
        let mid = mid.await.unwrap();
        assert!(
            matches!(mid, Err(SchedError::Busy)),
            "the least urgent job LEFT must be the next victim: {mid:?}"
        );
        prewarm
            .await
            .unwrap()
            .expect("a new viewer's opening segment must survive both arrivals");
        closer.await.unwrap().expect("the nearer incumbent guess");

        let got = order.lock().unwrap().clone();
        assert_eq!(
            got,
            ["blocker", "prewarm", "closer"],
            "the queue must keep the work nearest each viewer, including the \
             viewer who has not fetched anything yet: {got:?}"
        );
    }

    /// Unknown is not the same as useless, and eviction is the one place that
    /// distinction cannot be expressed as a rank.
    ///
    /// A job with no stream or no segment has distance `i64::MAX`. Sorting it
    /// last for DISPATCH is right — nothing is known to need it, and deferral
    /// costs only latency. Reading the same key at its maximum makes it the
    /// first thing destroyed, and no single total order says both. It is
    /// therefore filtered out of candidacy: a speculative arrival may not buy
    /// its slot by destroying work it cannot rank itself against.
    #[tokio::test]
    async fn a_guess_may_not_displace_work_whose_urgency_is_unknown() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                "blocker" => Duration::from_millis(400),
                _ => Duration::from_millis(40),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                pending_cap: 2,
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let stream = StreamKey::of("viewer");

        let submit = |tag: &'static str, class: JobClass, hint: JobHint| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    hint,
                )
                .await
            })
        };
        let on = |seg: u32| JobHint {
            stream,
            segment: Some(seg),
            seeds_playhead: PlayheadSeed::Observes,
        };

        // The client is at 100 and holds the only permit.
        let blocker = submit("blocker", JobClass::Interactive, on(100));
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Whole-file work with no viewer behind it at all: `JobHint::default()`
        // is `StreamKey::NONE` and no segment, so nothing can be said about
        // when — or whether — it is needed.
        let unknown = submit("unknown", JobClass::Background, JobHint::default());
        tokio::time::sleep(Duration::from_millis(20)).await;
        let near = submit("near", JobClass::Background, on(101));
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A guess ten segments out arrives at the full queue. It beats nothing
        // it is allowed to compare itself against.
        let deep = submit("deep", JobClass::Background, on(110));
        tokio::time::sleep(Duration::from_millis(20)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            queued.pending_background, 2,
            "precondition: the queue must be at its cap, never above it"
        );

        blocker.await.unwrap().expect("the client's own segment");
        let deep = deep.await.unwrap();
        assert!(
            matches!(deep, Err(SchedError::Busy)),
            "a guess must be refused rather than admitted by destroying work \
             it cannot rank itself against: {deep:?}"
        );
        near.await.unwrap().expect("the nearer incumbent");
        unknown
            .await
            .unwrap()
            .expect("work of unknown urgency must not be evicted for a guess");

        let got = order.lock().unwrap().clone();
        assert_eq!(
            got,
            ["blocker", "near", "unknown"],
            "unknown still sorts LAST for dispatch — it just is not destroyed \
             first: {got:?}"
        );
    }

    /// ...and the carve-out that keeps that from becoming a worse bug. If
    /// unknown work were simply unevictable, a queue full of it would start
    /// shedding CLIENTS, which is the exact failure `pending_cap` exists to
    /// prevent. Between an unrankable guess and a segment somebody is watching,
    /// the guess goes.
    #[tokio::test]
    async fn a_client_may_still_displace_work_whose_urgency_is_unknown() {
        let spawner = Arc::new(VariableSpawner(Arc::new(
            |spec: &JobSpec| match job_name(spec).as_str() {
                "blocker" => Duration::from_millis(400),
                _ => Duration::from_millis(40),
            },
        )));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                pending_cap: 2,
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let submit = |tag: String, class: JobClass, hint: JobHint| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    hint,
                )
                .await
            })
        };

        let stream = StreamKey::of("viewer");
        let blocker = submit(
            "blocker".into(),
            JobClass::Interactive,
            JobHint {
                stream,
                segment: Some(100),
                seeds_playhead: PlayheadSeed::Observes,
            },
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        // A queue with nothing rankable in it.
        let unknowns: Vec<_> = (0..2)
            .map(|i| {
                submit(
                    format!("unknown-{i}"),
                    JobClass::Background,
                    JobHint::default(),
                )
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(30)).await;
        // ...and a client's own segment arriving at it.
        let client = submit(
            "client".into(),
            JobClass::Interactive,
            JobHint {
                stream,
                segment: Some(101),
                seeds_playhead: PlayheadSeed::Observes,
            },
        );
        tokio::time::sleep(Duration::from_millis(30)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            (queued.pending_background, queued.pending_interactive),
            (1, 1),
            "precondition: the client must have taken a speculative job's place"
        );

        blocker.await.unwrap().expect("the client's own segment");
        client
            .await
            .unwrap()
            .expect("a client must never be shed behind unrankable speculation");
        let outcomes: Vec<_> = futures_util::future::join_all(unknowns)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|r| matches!(r, Err(SchedError::Busy)))
                .count(),
            1,
            "exactly one unknown job must have made way, not both and not \
             neither: {outcomes:?}"
        );
    }

    /// The kill switch for phase 2b, exercised in its OFF state.
    ///
    /// Letting speculative work wait for a permit reverses a rule that was
    /// written in response to a production outage (B108). The reversal is
    /// argued and guarded, but it runs on a server people watch, so there has
    /// to be a way back that is not a revert of thirty-odd commits.
    ///
    /// Both halves are asserted, and the second is the one that makes this a
    /// switch rather than a bigger hammer: with it off, speculation is shed at
    /// the door exactly as it was before phase 2b, AND a client's request still
    /// queues and is still served. `pending_cap = 0` — the only lever that
    /// existed before this — fails that second half: it sheds Interactive jobs
    /// too, so a video segment comes back as a 500 rather than as a cold miss.
    #[tokio::test]
    async fn background_queueing_can_be_turned_off_without_shedding_clients() {
        let spawner = Arc::new(VariableSpawner(Arc::new(
            |spec: &JobSpec| match job_name(spec).as_str() {
                "blocker" => Duration::from_millis(300),
                _ => Duration::from_millis(20),
            },
        )));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                // Generous, so nothing here is refused for want of queue SIZE:
                // whatever is shed is shed by the switch alone.
                pending_cap: 64,
                background_headroom: 0,
                queue_background: false,
                ..SchedConfig::default()
            },
        );
        let stream = StreamKey::of("viewer");
        let submit = |tag: &'static str, class: JobClass, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        // The one permit is taken for the whole test by a client's segment.
        let blocker = submit("blocker", JobClass::Interactive, 100);
        tokio::time::sleep(Duration::from_millis(40)).await;
        // A guess arriving at a busy pool, and a client's own next segment
        // arriving right behind it.
        let guess = submit("guess", JobClass::Background, 101);
        let client = submit("client", JobClass::Interactive, 102);
        tokio::time::sleep(Duration::from_millis(40)).await;

        // The guess is already resolved — shed, not parked — while the client
        // is still waiting its turn in the queue the switch left alone.
        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            (queued.pending_background, queued.pending_interactive),
            (0, 1),
            "with background queueing off, `pending` must hold the client's \
             segment and no speculation at all"
        );

        let guess = guess.await.unwrap();
        assert!(
            matches!(guess, Err(SchedError::Busy)),
            "the switch must return speculation to shed-not-queue: {guess:?}"
        );
        blocker.await.unwrap().expect("the client's first segment");
        client.await.unwrap().expect(
            "turning the speculative queue off must not shed clients — that is \
             the difference between this switch and pending_cap = 0",
        );
    }

    /// Eviction is a decision about SPECULATION, and it must never reach past
    /// it. Somebody is blocked on every Interactive job in the queue, so an
    /// eviction rule that ranks purely by urgency and forgets the tier would
    /// free a slot by abandoning a client mid-request — a 500 on a segment
    /// somebody is watching, which is the exact shape of B134.
    ///
    /// The arrival here is itself a client's request, so this also pins the
    /// other direction: a client arriving at a full queue takes a speculative
    /// job's place rather than being refused, which is what `pending_cap` was
    /// for before speculation started sharing it.
    #[tokio::test]
    async fn eviction_never_takes_the_job_a_client_is_blocked_on() {
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        let spawner = Arc::new(VariableSpawner(Arc::new(move |spec: &JobSpec| {
            let name = job_name(spec);
            let d = match name.as_str() {
                "blocker" => Duration::from_millis(400),
                _ => Duration::from_millis(60),
            };
            seen.lock().unwrap().push(name);
            d
        })));
        let s = TranscodeScheduler::spawn(
            DeviceTable::from_probe(&[], 1),
            spawner,
            SchedConfig {
                pending_cap: 2,
                background_headroom: 0,
                ..SchedConfig::default()
            },
        );
        let stream = StreamKey::of("viewer");

        let submit = |tag: &'static str, class: JobClass, seg: u32| {
            let s2 = s.clone();
            tokio::spawn(async move {
                s2.submit(
                    PathBuf::from(format!("/m/{tag}")),
                    h264(),
                    file_sink(),
                    class,
                    JobHint {
                        stream,
                        segment: Some(seg),
                        seeds_playhead: PlayheadSeed::Observes,
                    },
                )
                .await
            })
        };

        let blocker = submit("blocker", JobClass::Interactive, 100);
        tokio::time::sleep(Duration::from_millis(30)).await;
        // A second client request queues behind it, and one prefetch fills the
        // cap.
        let waiter = submit("waiter", JobClass::Interactive, 101);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let guess = submit("guess", JobClass::Background, 110);
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A third client request arrives to a full queue.
        let client = submit("client", JobClass::Interactive, 102);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let queued = s.snapshot().await.expect("snapshot");
        assert_eq!(
            (queued.pending_interactive, queued.pending_background),
            (2, 0),
            "the arriving client must have taken the prefetch's slot, and the \
             client already waiting must still be in the queue"
        );

        blocker.await.unwrap().expect("the client's first segment");
        let guess = guess.await.unwrap();
        assert!(
            matches!(guess, Err(SchedError::Busy)),
            "the speculative job is the only one that may be evicted: {guess:?}"
        );
        waiter
            .await
            .unwrap()
            .expect("a client already in the queue must never be evicted");
        client.await.unwrap().expect(
            "a client arriving at a full queue must not be refused \
                     while speculation occupies it",
        );

        let got = order.lock().unwrap().clone();
        assert_eq!(
            got,
            ["blocker", "waiter", "client"],
            "eviction must leave Interactive arrival order intact: {got:?}"
        );
    }

    /// One series per outcome, and no two of them the same string. A collision
    /// here is worse than a missing metric: `stale` folded into `dispatched`
    /// reports a queue throwing work away as a queue doing work, and the
    /// dashboard looks healthier the worse it gets.
    ///
    /// A real guard only because the strings live on the enum. Asserting them
    /// at their call sites would compare constants in the test against
    /// constants written beside them.
    #[test]
    fn queue_outcome_labels_are_distinct_and_stable() {
        const ALL: [QueueOutcome; 6] = [
            QueueOutcome::Dispatched,
            QueueOutcome::Stale,
            QueueOutcome::Evicted,
            QueueOutcome::Shed,
            QueueOutcome::Abandoned,
            QueueOutcome::Failed,
        ];
        assert_eq!(QueueOutcome::Dispatched.label(), "dispatched");
        assert_eq!(QueueOutcome::Stale.label(), "stale");
        assert_eq!(QueueOutcome::Evicted.label(), "evicted");
        assert_eq!(QueueOutcome::Shed.label(), "shed");
        assert_eq!(QueueOutcome::Abandoned.label(), "abandoned");
        assert_eq!(QueueOutcome::Failed.label(), "failed");

        let labels: std::collections::HashSet<&str> = ALL.iter().map(|o| o.label()).collect();
        assert_eq!(
            labels.len(),
            ALL.len(),
            "queue outcome labels collide: {labels:?} — a folded bucket reports \
             work thrown away as work done"
        );
    }

    /// One snapshot, queried many times.
    ///
    /// `Snapshotter::snapshot()` DRAINS histogram buckets, so taking a second
    /// one to answer a second assertion reads an empty histogram and is
    /// indistinguishable from an instrument that was never recorded. Captured
    /// once, into plain values, so every assertion in a test sees the same run.
    struct Metrics {
        counters: Vec<(String, Vec<String>, u64)>,
        histograms: Vec<(String, Vec<f64>)>,
    }

    impl Metrics {
        fn capture(snapshotter: &metrics_util::debugging::Snapshotter) -> Metrics {
            use metrics_util::debugging::DebugValue;
            let mut counters = Vec::new();
            let mut histograms = Vec::new();
            for (ck, _, _, v) in snapshotter.snapshot().into_vec() {
                let k = ck.key();
                let name = k.name().to_string();
                let labels: Vec<String> = k
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                match v {
                    DebugValue::Counter(n) => counters.push((name, labels, n)),
                    DebugValue::Histogram(vals) => {
                        histograms.push((name, vals.iter().map(|v| v.0).collect()))
                    }
                    _ => {}
                }
            }
            Metrics {
                counters,
                histograms,
            }
        }

        /// Absent is zero: a series that was never recorded is exactly the
        /// "this never happened" the assertions are checking for.
        fn counter(&self, name: &str, labels: &[&str]) -> u64 {
            self.counters
                .iter()
                .find(|(n, got, _)| {
                    n == name && labels.iter().all(|want| got.iter().any(|g| g == want))
                })
                .map(|(_, _, v)| *v)
                .unwrap_or(0)
        }

        fn histogram(&self, name: &str) -> Vec<f64> {
            self.histograms
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        }
    }

    /// The `pin_total{followed}` lesson, applied before it can be repeated: a
    /// queued job is re-examined on EVERY drain pass, so a counter incremented
    /// where the job is looked at counts examinations while its sibling arms
    /// count jobs, and their ratio means nothing. `dispatched` must be recorded
    /// where the job actually takes a permit, once.
    ///
    /// The histogram rides along, for the same reason and in the same place:
    /// "shallow beats deep" is only a query if each dispatched job contributes
    /// one sample of the distance it was dispatched AT.
    ///
    /// The shape: pin a speculative job to a single-permit GPU, hold that GPU
    /// for the whole test, and drive three separate drains off unrelated CPU
    /// completions while it waits.
    #[test]
    fn a_queued_job_is_counted_once_however_many_drains_examine_it() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let gpu = DeviceId::hw(HwAccel::Nvenc, 0);
                let spawner =
                    Arc::new(VariableSpawner(Arc::new(
                        |spec: &JobSpec| match job_name(spec).as_str() {
                            "hold-gpu" => Duration::from_millis(260),
                            _ => Duration::from_millis(15),
                        },
                    )));
                let s = TranscodeScheduler::spawn(
                    DeviceTable::from_probe(&[(gpu, 1)], 3),
                    spawner,
                    // The pinned pool is one permit; with the default reserve
                    // it could hold no speculative job at all and there would
                    // be nothing queued to examine.
                    SchedConfig {
                        background_headroom: 0,
                        ..SchedConfig::default()
                    },
                );
                let stream = StreamKey::of("viewer");

                // Holds the GPU — and the pin's only candidate — and puts the
                // viewer's playhead at 100.
                let hold = {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit(
                            PathBuf::from("/m/hold-gpu"),
                            cmaf(),
                            file_sink(),
                            JobClass::Interactive,
                            JobHint {
                                stream,
                                segment: Some(100),
                                seeds_playhead: PlayheadSeed::Observes,
                            },
                        )
                        .await
                    })
                };
                tokio::time::sleep(Duration::from_millis(20)).await;

                // Four segments ahead, and pinned to the same busy device.
                let probe = {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit(
                            PathBuf::from("/m/probe"),
                            cmaf(),
                            file_sink(),
                            JobClass::Background,
                            JobHint {
                                stream,
                                segment: Some(104),
                                seeds_playhead: PlayheadSeed::Observes,
                            },
                        )
                        .await
                    })
                };
                tokio::time::sleep(Duration::from_millis(20)).await;
                let snap = s.snapshot().await.expect("snapshot");
                assert_eq!(
                    snap.pending_background, 1,
                    "precondition: the speculative job must be QUEUED, or there \
                     are no re-examinations to over-count"
                );

                // Each of these lands on a free CPU permit and its completion
                // drives a drain that re-examines the still-queued job.
                for tag in ["cpu-1", "cpu-2", "cpu-3"] {
                    s.submit(
                        PathBuf::from(format!("/m/{tag}")),
                        h264(),
                        file_sink(),
                        JobClass::Interactive,
                        JobHint::default(),
                    )
                    .await
                    .expect("cpu job");
                }

                hold.await.unwrap().expect("gpu blocker");
                let done = probe.await.unwrap().expect("the queued job");
                assert!(
                    done.queue_wait_ms > 0,
                    "precondition: it must actually have waited"
                );
            })
        });

        let m = Metrics::capture(&snapshotter);
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=background", "outcome=dispatched"],
            ),
            1,
            "a job examined across several drains must be counted ONCE, where \
             it takes a permit — not once per examination"
        );
        assert_eq!(
            m.histogram("pharos_transcode_queue_distance"),
            [4.0],
            "one sample per dispatched speculative job, carrying the distance \
             it was dispatched at"
        );
    }

    /// The histogram asks one question — "is the prefetch ladder tuned too far
    /// ahead?" — and a deliberate backward guess is not an answer to it.
    ///
    /// Until a passed job could be dispatched at all, this could not arise. Now
    /// a SyncPlay seek prewarm at −400 reaches an encoder routinely, and its
    /// magnitude is a property of how far a viewer seeked rather than of the
    /// ladder. Recorded, every one of them lands in the bottom bucket of a
    /// positively-bounded histogram and drags the low quantiles toward zero —
    /// which reads as the HEALTHY verdict ("served shallow-first") precisely
    /// while a group is seeking. There is no label to separate them by either:
    /// the series deliberately has none.
    ///
    /// Both a forward and a backward guess, so the filter cannot pass by
    /// recording nothing at all.
    #[test]
    fn the_queue_distance_histogram_takes_no_sample_from_a_backward_guess() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(5), |_, _| {
                    WorkerRunResult::Done { out_bytes: 1 }
                });
                let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
                let stream = StreamKey::of("rewinder");

                let submit = |tag: &'static str, class: JobClass, seg: u32| {
                    let s2 = s.clone();
                    async move {
                        s2.submit(
                            PathBuf::from(format!("/m/{tag}")),
                            cmaf(),
                            file_sink(),
                            class,
                            JobHint {
                                stream,
                                segment: Some(seg),
                                seeds_playhead: PlayheadSeed::Observes,
                            },
                        )
                        .await
                        .expect(tag)
                    }
                };

                // The member is at 500...
                submit("head", JobClass::Interactive, 500).await;
                // ...its ordinary prefetch is five ahead...
                submit("forward", JobClass::Background, 505).await;
                // ...and the group has just seeked it back to 100.
                submit("prewarm", JobClass::Background, 100).await;
            })
        });

        let m = Metrics::capture(&snapshotter);
        assert_eq!(
            m.histogram("pharos_transcode_queue_distance"),
            [5.0],
            "only forward speculation measures the prefetch ladder; a backward \
             guess measures the seek that caused it"
        );
    }

    /// The load-bearing arm. A queue that never drops stale work is a queue
    /// that has quietly become the FIFO B108 deleted, and nothing else in the
    /// scheduler's telemetry can tell the two apart: `pending_background` looks
    /// identical whether the depth is work about to be needed or work already
    /// wasted.
    ///
    /// Both background jobs here are the same shape, one segment apart, so the
    /// assertion is that the counter DISCRIMINATES between them — not merely
    /// that some series exists.
    #[test]
    fn a_prefetch_dropped_for_staleness_is_counted_as_stale() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let spawner =
                    Arc::new(VariableSpawner(Arc::new(
                        |spec: &JobSpec| match job_name(spec).as_str() {
                            "blocker" => Duration::from_millis(260),
                            _ => Duration::from_millis(40),
                        },
                    )));
                let s = TranscodeScheduler::spawn(
                    DeviceTable::from_probe(&[], 1),
                    spawner,
                    SchedConfig {
                        background_headroom: 0,
                        ..SchedConfig::default()
                    },
                );
                let stream = StreamKey::of("viewer");
                let submit = |tag: &'static str, class: JobClass, seg: u32| {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit(
                            PathBuf::from(format!("/m/{tag}")),
                            h264(),
                            file_sink(),
                            class,
                            JobHint {
                                stream,
                                segment: Some(seg),
                                seeds_playhead: PlayheadSeed::Observes,
                            },
                        )
                        .await
                    })
                };

                let blocker = submit("blocker", JobClass::Interactive, 98);
                tokio::time::sleep(Duration::from_millis(20)).await;
                let standing = submit("standing", JobClass::Background, 100);
                let passed = submit("passed", JobClass::Background, 99);
                tokio::time::sleep(Duration::from_millis(20)).await;
                // The viewer overtakes `passed`, which is what makes it stale
                // rather than a deliberate backward guess (`is_stale`).
                let advance = submit("advance", JobClass::Interactive, 100);
                tokio::time::sleep(Duration::from_millis(20)).await;

                blocker.await.unwrap().expect("the client's own segment");
                advance.await.unwrap().expect("the client's next segment");
                standing.await.unwrap().expect("still wanted");
                assert!(
                    matches!(passed.await.unwrap(), Err(SchedError::Busy)),
                    "precondition: the passed job must actually have been dropped"
                );
            })
        });

        let m = Metrics::capture(&snapshotter);
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=background", "outcome=stale"],
            ),
            1,
            "the dropped prefetch must be counted as `stale`, or a queue full \
             of work nobody wants is indistinguishable from a busy one"
        );
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=background", "outcome=dispatched"],
            ),
            1,
            "...and the one still wanted must be counted as dispatched, in the \
             same denominator"
        );
    }

    /// The other load-bearing arm, and its opposite in the same run: a queue
    /// that evicts nothing under pressure has simply become a FIFO with extra
    /// steps, and one that evicts on every arrival is churning. `evicted` and
    /// `shed` say which is happening.
    #[test]
    fn an_evicted_job_and_a_refused_arrival_are_counted_apart() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let spawner =
                    Arc::new(VariableSpawner(Arc::new(
                        |spec: &JobSpec| match job_name(spec).as_str() {
                            "blocker" => Duration::from_millis(300),
                            _ => Duration::from_millis(40),
                        },
                    )));
                let s = TranscodeScheduler::spawn(
                    DeviceTable::from_probe(&[], 1),
                    spawner,
                    SchedConfig {
                        pending_cap: 2,
                        background_headroom: 0,
                        ..SchedConfig::default()
                    },
                );
                let stream = StreamKey::of("viewer");
                let submit = |tag: &'static str, class: JobClass, seg: u32| {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit(
                            PathBuf::from(format!("/m/{tag}")),
                            h264(),
                            file_sink(),
                            class,
                            JobHint {
                                stream,
                                segment: Some(seg),
                                seeds_playhead: PlayheadSeed::Observes,
                            },
                        )
                        .await
                    })
                };

                let blocker = submit("blocker", JobClass::Interactive, 100);
                tokio::time::sleep(Duration::from_millis(20)).await;
                let far = submit("far", JobClass::Background, 110);
                let mid = submit("mid", JobClass::Background, 105);
                tokio::time::sleep(Duration::from_millis(20)).await;
                // Beats `far`, so it takes its slot.
                let near = submit("near", JobClass::Background, 101);
                tokio::time::sleep(Duration::from_millis(20)).await;
                // Beats nothing, so it is refused instead.
                let further = submit("further", JobClass::Background, 120);
                tokio::time::sleep(Duration::from_millis(20)).await;

                blocker.await.unwrap().expect("the client's own segment");
                assert!(
                    matches!(far.await.unwrap(), Err(SchedError::Busy)),
                    "precondition: the least urgent incumbent must have been evicted"
                );
                assert!(
                    matches!(further.await.unwrap(), Err(SchedError::Busy)),
                    "precondition: the deepest arrival must have been refused"
                );
                near.await.unwrap().expect("near");
                mid.await.unwrap().expect("mid");
            })
        });

        let m = Metrics::capture(&snapshotter);
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=background", "outcome=evicted"],
            ),
            1,
            "the job displaced from a full queue must be counted as `evicted`"
        );
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=background", "outcome=shed"],
            ),
            1,
            "the arrival refused at a full queue must be counted as `shed` — \
             folding the two would make eviction pressure unreadable"
        );
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=background", "outcome=dispatched"],
            ),
            2,
            "...in the same denominator as the two that survived"
        );
    }

    /// The three arms nothing else asserts, so the partition is a claim the
    /// tests make rather than one the prose makes.
    ///
    /// `abandoned` and `failed` are the two exits that are NOT load management,
    /// and folding either into `shed` would report the queue managing pressure
    /// when it was doing nothing of the kind — a dashboard reading "we are
    /// shedding" during an outage that was actually an unsupported target or a
    /// wave of seeks. The third is `shed` reached with NO victim available at
    /// all: a full queue of work a client is blocked on, which the eviction
    /// rule must refuse to touch, so the arrival is turned away instead.
    ///
    /// Three phases on three schedulers under one recorder, because each needs
    /// a different pool shape and the counters are what is being read.
    #[test]
    fn the_partition_covers_abandonment_failure_and_a_shed_with_no_victim() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                // 1. `failed` — a target no device can ever take. Nothing about
                //    load, and nothing a retry or more capacity would help.
                let (spawner, _) = ScriptedSpawner::new(Duration::ZERO, |_, _| {
                    WorkerRunResult::Done { out_bytes: 1 }
                });
                let s = TranscodeScheduler::spawn(table(), spawner, SchedConfig::default());
                let r = s
                    .submit(
                        PathBuf::from("/m/live"),
                        h264(),
                        SinkRequest::LiveStream,
                        JobClass::Interactive,
                        JobHint::default(),
                    )
                    .await;
                assert_eq!(
                    r,
                    Err(SchedError::Unsupported),
                    "precondition: the live sink must still be rejected"
                );

                // 2. `abandoned` — a queued guess whose caller went away. The
                //    seek that orphaned it is not load either.
                let spawner =
                    Arc::new(VariableSpawner(Arc::new(
                        |spec: &JobSpec| match job_name(spec).as_str() {
                            "hold" => Duration::from_millis(200),
                            _ => Duration::from_millis(20),
                        },
                    )));
                let s = TranscodeScheduler::spawn(
                    DeviceTable::from_probe(&[], 1),
                    spawner,
                    SchedConfig {
                        background_headroom: 0,
                        ..SchedConfig::default()
                    },
                );
                let submit = |tag: &'static str, class: JobClass| {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit(
                            PathBuf::from(format!("/m/{tag}")),
                            h264(),
                            file_sink(),
                            class,
                            JobHint::default(),
                        )
                        .await
                    })
                };
                let hold = submit("hold", JobClass::Interactive);
                tokio::time::sleep(Duration::from_millis(30)).await;
                let orphan = submit("orphan", JobClass::Background);
                tokio::time::sleep(Duration::from_millis(30)).await;
                orphan.abort();
                assert!(
                    orphan.await.is_err(),
                    "precondition: the orphan's caller must really be gone"
                );
                hold.await.unwrap().expect("the client's own segment");
                tokio::time::sleep(Duration::from_millis(60)).await;

                // 3. `shed` with no victim — a full queue of work clients are
                //    blocked on. Eviction may not reach past speculation, so
                //    there is nothing to displace and the guess is turned away.
                let spawner =
                    Arc::new(VariableSpawner(Arc::new(
                        |spec: &JobSpec| match job_name(spec).as_str() {
                            "hold2" => Duration::from_millis(300),
                            _ => Duration::from_millis(20),
                        },
                    )));
                let s = TranscodeScheduler::spawn(
                    DeviceTable::from_probe(&[], 1),
                    spawner,
                    SchedConfig {
                        pending_cap: 2,
                        background_headroom: 0,
                        ..SchedConfig::default()
                    },
                );
                let submit = |tag: &'static str, class: JobClass| {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        s2.submit(
                            PathBuf::from(format!("/m/{tag}")),
                            h264(),
                            file_sink(),
                            class,
                            JobHint::default(),
                        )
                        .await
                    })
                };
                let hold2 = submit("hold2", JobClass::Interactive);
                tokio::time::sleep(Duration::from_millis(30)).await;
                let c1 = submit("client-1", JobClass::Interactive);
                let c2 = submit("client-2", JobClass::Interactive);
                tokio::time::sleep(Duration::from_millis(30)).await;
                let guess = submit("guess", JobClass::Background);
                tokio::time::sleep(Duration::from_millis(30)).await;
                let guess = guess.await.unwrap();
                assert!(
                    matches!(guess, Err(SchedError::Busy)),
                    "a queue full of clients has nothing an arrival may take: {guess:?}"
                );
                for (tag, h) in [("hold2", hold2), ("client-1", c1), ("client-2", c2)] {
                    h.await
                        .unwrap()
                        .unwrap_or_else(|e| panic!("{tag} was refused: {e:?}"));
                }
            })
        });

        let m = Metrics::capture(&snapshotter);
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=interactive", "outcome=failed"],
            ),
            1,
            "a target no device can take must be counted as `failed`, not as \
             load the scheduler chose to shed"
        );
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=background", "outcome=abandoned"],
            ),
            1,
            "a queued job whose caller went away must be counted as \
             `abandoned`: work that stopped existing, not work refused"
        );
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=background", "outcome=shed"],
            ),
            1,
            "an arrival at a queue with no evictable victim must still be \
             counted as `shed`"
        );
        assert_eq!(
            m.counter(
                "pharos_transcode_queue_outcome_total",
                &["class=background", "outcome=evicted"],
            ),
            0,
            "...and nothing may be recorded as evicted when nothing was: the \
             partition is only a partition if each arm means one thing"
        );
    }

    /// Spec 003 R8 / issue #114, on the path the queue created.
    ///
    /// A shared-init fMP4 rendition resolves to exactly ONE device, and
    /// `device_supports` keeps hardware ELIGIBLE for H264+fMP4 on purpose — the
    /// one-encoder guarantee is the pin, not an exclusion, because excluding
    /// hardware wholesale cost the GPU for all browser playback. So a CMAF job
    /// sees a wide `eligible_for`, and any dispatch path that rebuilds its
    /// candidate set without re-applying the pin will hand it to a second
    /// encoder: libx264 output (High, `log2_max_frame_num` 4) under an init
    /// carrying NVENC's SPS (Main, 8), which no ffmpeg flag reconciles.
    /// Undecodable video, served with a 200.
    ///
    /// The QUEUE is what makes that reachable in volume rather than in theory:
    /// browser H264 is all CMAF, its prefetch is `Background`, and speculative
    /// work only began to queue at all in this change — so the drain became the
    /// dominant producer of fMP4 segments. Run for both classes: an interactive
    /// job could already reach the drain, and the reserve arithmetic that gates
    /// the speculative one is counted over the candidate set, so both have to be
    /// pinned by the same code.
    ///
    /// The shape: the pinned device is busy for the whole test, a permit on
    /// ANOTHER device frees underneath the queued job, and it must still wait.
    #[tokio::test]
    async fn a_queued_cmaf_job_waits_for_its_pinned_device_rather_than_spilling() {
        let gpu = DeviceId::hw(HwAccel::Nvenc, 0);
        for class in [JobClass::Interactive, JobClass::Background] {
            let spawner = Arc::new(VariableSpawner(Arc::new(|spec: &JobSpec| {
                match job_name(spec).as_str() {
                    // Frees a CPU permit — and with it a drain — while the GPU
                    // is still busy.
                    "free-cpu" => Duration::from_millis(60),
                    // Everything else holds the pinned device past the drain.
                    _ => Duration::from_millis(400),
                }
            })));
            // Two GPU permits and two CPU ones. Both numbers matter: the GPU
            // pool must be able to hold `background_headroom` AND a job, or
            // speculation is shed outright rather than queued; and the CPU must
            // still have spare capacity above the reserve when the drain runs,
            // so that the reserve is not what saves this.
            let s = TranscodeScheduler::spawn(
                DeviceTable::from_probe(&[(gpu, 2)], 2),
                spawner,
                SchedConfig::default(),
            );

            let mut holds = Vec::new();
            for tag in ["hold-gpu-a", "hold-gpu-b"] {
                let s2 = s.clone();
                holds.push(tokio::spawn(async move {
                    s2.submit(
                        PathBuf::from(format!("/m/{tag}")),
                        h264(),
                        file_sink(),
                        JobClass::Interactive,
                        JobHint::default(),
                    )
                    .await
                }));
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            // VP9 has no NVENC encoder, so this can only land on the CPU.
            let free_cpu = {
                let s2 = s.clone();
                let mut o = h264();
                o.video = Some(VideoCodec::Vp9);
                tokio::spawn(async move {
                    s2.submit(
                        PathBuf::from("/m/free-cpu"),
                        o,
                        file_sink(),
                        JobClass::Interactive,
                        JobHint::default(),
                    )
                    .await
                })
            };
            tokio::time::sleep(Duration::from_millis(30)).await;

            let probe = {
                let s2 = s.clone();
                tokio::spawn(async move {
                    s2.submit(
                        PathBuf::from("/m/probe"),
                        cmaf(),
                        file_sink(),
                        class,
                        JobHint::default(),
                    )
                    .await
                })
            };
            tokio::time::sleep(Duration::from_millis(30)).await;
            let snap = s.snapshot().await.expect("snapshot");
            assert_eq!(
                snap.pending, 1,
                "precondition ({class:?}): the pinned job must be queued, not \
                 already placed elsewhere"
            );

            free_cpu.await.unwrap().expect("cpu blocker");
            for h in holds {
                h.await.unwrap().expect("gpu blocker");
            }
            let done = probe
                .await
                .unwrap()
                .expect("a queued shared-init job must still complete");
            assert_eq!(
                done.device, gpu,
                "a queued CMAF job ({class:?}) must drain onto the device its \
                 rendition pins to, never onto whichever permit freed first"
            );
            assert!(
                done.queue_wait_ms > 0,
                "precondition ({class:?}): the job must actually have waited, \
                 or this proves nothing about the drain path"
            );
        }
    }
}
