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
/// guarantee is observed rather than merely asserted. The three variants are
/// exhaustive over the shared-init path, so they sum to the number of
/// shared-init jobs placed — a bucket that stops adding up means a branch is
/// resolving devices without saying so.
///
/// `Invalidated` is the one to alert on: it means a rendition's device went
/// unavailable mid-stream and the request was FAILED rather than spilled onto
/// a second encoder, which is a visible stall for the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// The rendition resolved to an eligible device and was placed on it.
    Followed,
    /// The resolved device was not eligible (cooldown / excluded), so the
    /// request failed rather than mixing encoders under one init (#114).
    Invalidated,
    /// No device could be resolved for the rendition at all; placement falls
    /// through to the normal load-balanced path.
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
        }
    }

    /// Watch this slot. Held by anyone who wants to promote the job; keeps no
    /// sender alive, so it cannot keep a dead driver's slot open.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<JobId>> {
        self.tx.subscribe()
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

/// What the caller knows about a job that the scheduler cannot work out for
/// itself: whose stream it belongs to, and which segment of it.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobHint {
    pub stream: StreamKey,
    /// Segment index. `None` for anything that is not a numbered segment.
    pub segment: Option<u32>,
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
    /// FLOOR for the learned speculative allowance — see
    /// [`crate::admission::AdmissionController`].
    ///
    /// This was the allowance itself, a number calibrated by hand on one GTX
    /// 1070 against one 23 Mbps HEVC source. It is now the value a device sits
    /// on before it has learned anything, and the value it collapses back to
    /// under sustained deadline misses, so a cold process behaves exactly as
    /// it did when this was the whole answer.
    ///
    /// Not zero. Refusing ALL prefetch while a client job runs would starve
    /// the pipeline that makes the next segment a 30 ms cache hit. That was
    /// true when refused prefetch was dropped outright, and it stayed true when
    /// it began to wait for a permit instead (V58): a job refused on every
    /// device it could use is a job that is never selected, so a zero allowance
    /// starves it just as completely — it merely starves it in the queue rather
    /// than at the door.
    ///
    /// Derived from `admission.floor` in `Default` rather than restated as
    /// its own literal: this and `AdmissionConfig::floor` are the same number
    /// in two types (`usize` here for the pre-learning admission math that
    /// used to read it directly, `f64` there for the AIMD arithmetic), and a
    /// hand-kept duplicate is exactly the kind of pair that drifts apart one
    /// edit at a time.
    pub background_alongside_client: usize,
    /// How the per-device speculative allowance is learned. `floor` here is
    /// the value `background_alongside_client` used to be — see that field's
    /// doc comment for why the two cannot be set independently.
    pub admission: AdmissionConfig,
}

impl Default for SchedConfig {
    fn default() -> Self {
        let admission = AdmissionConfig::default();
        Self {
            inbox_depth: 256,
            pending_cap: 256,
            cooldown: Duration::from_secs(2),
            max_retries: 3,
            background_headroom: 1,
            background_alongside_client: admission.floor as usize,
            admission,
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
    /// Who is waiting. Carried through retries + requeues so a job's class is
    /// the same wherever it is observed (queued, inflight, finished).
    class: JobClass,
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
    playheads: HashMap<StreamKey, (u32, u64)>,
    /// Monotonic tick used only to order `playheads` for eviction.
    playhead_clock: u64,
}

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
                // Wired in the fd-passing step; not yet schedulable.
                let _ = reply.send(Err(SchedError::Unsupported));
                return;
            }
            // Only an INTERACTIVE submission tells us where the viewer actually
            // is. A speculative request says nothing about the playhead;
            // letting prefetch move it would make the lookahead distance
            // measure itself.
            if class == JobClass::Interactive {
                if let Some(seg) = hint.segment {
                    note_playhead(state, hint.stream, seg);
                }
            }
            let job_id = JobId(state.next_job);
            state.next_job += 1;
            // Published BEFORE placement, so a requester that coalesces onto
            // this job can name it even while it is still queued — which is
            // exactly the case promotion exists for.
            if let Some(slot) = assigned {
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
                class,
                device: None,
                peer_jobs: 0,
                background_peers: 0,
                stream: hint.stream,
                segment: hint.segment,
                // Replaced at dispatch, once the device is known.
                span: tracing::Span::none(),
            };
            place(state, job_id, ctx, self_tx);
        }
        SchedMsg::Promote { job_id } => {
            promote_job(state, job_id, self_tx);
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
                        observe_margin(state, device, &ctx, encode_ms);
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
    let waited_ms = Instant::now()
        .saturating_duration_since(ctx.enqueued)
        .as_millis() as u64;
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
fn observe_margin(state: &mut SchedState, device: DeviceId, ctx: &JobCtx, encode_ms: u64) {
    let capacity = state.devices.slot(device).map(|s| s.capacity).unwrap_or(1);
    // Read before `observe` takes `state.admission` mutably below — this is
    // the actual value the control law applies, not a copy of it, so the log
    // line stays true after anyone tunes `margin_ratio`.
    let margin_ratio = state.cfg.admission.margin_ratio;
    let obs = Observation {
        // Live/progressive jobs have no duration and so no deadline.
        segment_seconds: ctx.opts.duration_ticks.map(|t| t as f64 / 10_000_000.0),
        encode_seconds: encode_ms as f64 / 1000.0,
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

/// May speculative work take one of `candidates`' permits *right now*?
///
/// The one admission rule, consulted identically by both dispatch paths so the
/// arrival path and the drain path cannot drift into reserving different
/// amounts. Recomputed each time it is asked: it is a statement about the
/// device table now, never a verdict cached on a job.
///
/// The reserve is clamped to the candidate pool's own capacity minus one. That
/// clamp used to be unnecessary — a refused speculative job was DROPPED, so a
/// reserve as large as the pool merely meant "no prefetch on this pool". Now
/// the job queues instead, and a job that can never satisfy the reserve is a
/// job whose caller's `submit().await` never resolves. A pool cannot reserve
/// its own last permit against itself.
fn background_may_dispatch(state: &SchedState, candidates: &[DeviceId]) -> bool {
    let capacity: usize = candidates
        .iter()
        .filter_map(|d| state.devices.slot(*d))
        .map(|s| s.capacity)
        .sum();
    let reserve = state
        .cfg
        .background_headroom
        .min(capacity.saturating_sub(1));
    free_permits(state, candidates) > reserve
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
/// `pending_cap` is still a hard stop: a full queue replies `Busy` rather than
/// growing without bound.
fn queue_or_refuse(state: &mut SchedState, job_id: JobId, ctx: JobCtx) {
    if state.pending.len() >= state.cfg.pending_cap {
        let _ = ctx.reply.send(Err(SchedError::Busy));
    } else {
        state.pending.push_back((job_id, ctx));
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
/// O(n) over `pending_cap` (256) rather than a heap: every key moves whenever
/// any client advances, so a heap would be re-keyed far more often than popped.
fn next_to_dispatch(state: &SchedState) -> Option<usize> {
    state
        .pending
        .iter()
        .enumerate()
        .min_by_key(|(idx, (_, ctx))| match ctx.class {
            JobClass::Interactive => (0i64, *idx as i64),
            JobClass::Background => (1i64, lookahead_distance(state, ctx)),
        })
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

/// Try to dispatch `ctx` to its best eligible device; queue if all
/// permits are busy; fail if no device can ever take it.
fn place(state: &mut SchedState, job_id: JobId, mut ctx: JobCtx, self_tx: &mpsc::Sender<SchedMsg>) {
    // Caller gone (client seeked/disconnected → dropped the `submit().await`
    // and its oneshot receiver): don't spend a worker on a segment nobody is
    // waiting for. This is the post-seek contention fix — a dead prefetch job
    // must not sit ahead of the seek-target segment in a device queue.
    if ctx.reply.is_closed() {
        return;
    }
    let now = Instant::now();
    let full_eligible = state.devices.eligible_for(&ctx.opts, now);
    if full_eligible.is_empty() {
        // No supporting device at all (e.g. cooldown could hide all HW
        // but CPU always supports; truly empty ⇒ unsupported target).
        let _ = ctx.reply.send(Err(SchedError::Unsupported));
        return;
    }
    // Spec 003 — a shared-init fMP4 rendition must come from ONE encoder, so it
    // does not get a choice of devices. The device is a pure function of the
    // rendition (see `DeviceTable::rendition_device`), which keeps the answer
    // stable across a restart; an in-memory pin would not, and a rendition
    // re-pinned mid-playback serves segments that no longer match the client's
    // init (issue #114 — undecodable video, served with a 200).
    //
    // Cooldown deliberately does NOT re-route it. Spilling to a second encoder
    // is exactly the failure this prevents, so an unavailable device FAILS the
    // request instead: the client restarts the stream and re-fetches an init
    // that matches whatever produces it next. A visible stall that recovers
    // beats silent corruption.
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
                    let _ = ctx
                        .reply
                        .send(Err(SchedError::Failed(WorkerError::Other(format!(
                        "rendition device {d} unavailable; refusing to mix encoders under one init"
                    )))));
                    return;
                }
                PinOutcome::Followed.record();
                Some(d)
            }
            None => {
                // Previously silent. Without it the counter cannot be read as a
                // total: `followed + invalidated` was always short by however
                // many shared-init jobs resolved to no device, and there was no
                // way to tell that from the metric.
                PinOutcome::Unresolved.record();
                None
            }
        }
    } else {
        None
    };

    // Candidate devices = eligible minus already-tried. A pinned rendition has
    // exactly one candidate and never widens.
    let candidates: SmallVec<[DeviceId; 5]> = match pinned {
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
    if candidates.is_empty() {
        // Every supporting device has been tried + failed transiently.
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
    // every completion.
    if ctx.class == JobClass::Background && !background_may_dispatch(state, &candidates) {
        tracing::debug!(
            %job_id,
            candidates = ?candidates,
            headroom = state.cfg.background_headroom,
            "speculative transcode queued: no spare capacity above the reserve"
        );
        queue_or_refuse(state, job_id, ctx);
        return;
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
        return;
    }
    let now = Instant::now();
    let full_eligible = state.devices.eligible_for(&ctx.opts, now);
    let candidates: SmallVec<[DeviceId; 5]> = full_eligible
        .iter()
        .copied()
        .filter(|d| !ctx.excluded.contains(d))
        .collect();
    if candidates.is_empty() {
        let err = ctx
            .last_error
            .clone()
            .unwrap_or(WorkerError::Other("no device left".into()));
        let _ = ctx.reply.send(Err(SchedError::Failed(err)));
        return;
    }
    // The same reserve the arrival path applies, recomputed against the device
    // table as it is now. Speculative work reached this path for the first time
    // in this task, so without it a queued prefetch would drain straight onto
    // the permit `background_headroom` exists to hold open for a client.
    if ctx.class == JobClass::Background && !background_may_dispatch(state, &candidates) {
        requeue.push_back((job_id, ctx));
        return;
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
        // advance the distance its own urgency is judged against.
        assert!(
            after.playheads.is_empty(),
            "promotion must not move a playhead: {:?}",
            after.playheads
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

    #[tokio::test]
    async fn speculative_work_is_shed_and_never_queues_in_front_of_a_client() {
        // The defect this pins: prefetch is dispatched BEFORE the segment the
        // client is blocked on, and shared one FIFO with it, so speculative
        // encodes could bury a client's own segment. Background work must now
        // run only out of spare capacity, and must never join the queue.
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(400), |_, _| {
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

        // Speculative work is refused immediately rather than queued.
        let shed = s
            .submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
                JobHint::default(),
            )
            .await;
        assert_eq!(
            shed,
            Err(SchedError::Busy),
            "prefetch must be shed, not queued"
        );

        // A client request in the same state still queues and still completes.
        let s2 = s.clone();
        let client = tokio::spawn(async move {
            s2.submit(
                PathBuf::from("/m/client"),
                h264(),
                file_sink(),
                JobClass::Interactive,
                JobHint::default(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        let snap = s.snapshot().await.expect("snapshot");
        assert_eq!(
            snap.pending_background, 0,
            "nothing speculative in the queue"
        );
        assert_eq!(snap.pending_interactive, 1, "the client request is queued");

        assert!(client.await.unwrap().is_ok());
        for h in running {
            assert!(h.await.unwrap().is_ok());
        }
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

    #[tokio::test]
    async fn speculative_work_leaves_headroom_for_a_client() {
        // The last free permit is reserved: background is shed while only
        // `background_headroom` permits remain, so a client request arriving a
        // moment later still finds a slot instead of queueing behind a guess.
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
        assert_eq!(
            s.submit(
                PathBuf::from("/m/prefetch"),
                h264(),
                file_sink(),
                JobClass::Background,
                JobHint::default(),
            )
            .await,
            Err(SchedError::Busy),
            "the reserved permit is not for speculative work"
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
        for h in running {
            assert!(h.await.unwrap().is_ok());
        }
    }
    /// A single GPU, so a job's peers are unambiguous.
    fn one_gpu(capacity: usize) -> DeviceTable {
        DeviceTable::from_probe(&[(DeviceId::hw(HwAccel::Nvenc, 0), capacity)], 0)
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
            .filter(|r| matches!(r, Ok(d) if d.device == gpu))
            .count();
        assert_eq!(
            joined_the_client,
            SchedConfig::default().background_alongside_client,
            "speculative jobs on the client's device: {results:?}"
        );
        assert!(
            results.iter().any(|r| r == &Err(SchedError::Busy)),
            "the cap must actually shed, not merely reorder: {results:?}"
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
        let joined_the_client = results
            .iter()
            .filter(|r| matches!(r, Ok(d) if d.device == gpu))
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
        let joined = results
            .iter()
            .filter(|r| matches!(r, Ok(d) if d.device == gpu))
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

    /// A speculative request says nothing about where the viewer actually is —
    /// only what somebody guessed they might want next. If prefetch could move
    /// the playhead, a deep speculative submission would advance the very
    /// distance measurement its own urgency is judged against.
    #[tokio::test]
    async fn only_interactive_submissions_move_the_playhead() {
        let (spawner, _) = ScriptedSpawner::new(Duration::from_millis(20), |_, _| {
            WorkerRunResult::Done { out_bytes: 1 }
        });
        let s = TranscodeScheduler::spawn(one_gpu(4), spawner, SchedConfig::default());
        let stream = StreamKey::of("play-session-background-only");

        s.submit(
            PathBuf::from("/m/a"),
            cmaf(),
            file_sink(),
            JobClass::Background,
            JobHint {
                stream,
                segment: Some(50),
            },
        )
        .await
        .unwrap();

        let snap = s.snapshot().await.unwrap();
        assert_eq!(
            snap.playheads.get(&stream),
            None,
            "a Background submission must not create a playhead entry"
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
}
