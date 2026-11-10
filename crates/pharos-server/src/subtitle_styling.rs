//! Anime subtitle styling, in the background (spec 028 FR-012, FR-014 to
//! FR-016, FR-021; ADR-0067).
//!
//! # Shape
//!
//! A slow background job in the mould of [`crate::audio_analysis`]. After a
//! warm-up it waits for the background leader, then runs passes forever: each
//! pass lists the library, keeps the videos in scope, and styles those whose
//! stored row is stale, one at a time. Each item runs the stages `select`
//! (genre, source track, signature), `read` (the source script), `signals`
//! (one libav pass over the audio), `direct` (the pure director) and `store`.
//!
//! # Why it is cheap
//!
//! - **Once per file.** A row stores the video's mtime and size, the sidecar's
//!   when the source is one, the source index and `DIRECTOR_VERSION`. A pass
//!   over a styled library reads and decodes nothing.
//! - **Genre first.** Only items whose own genres, or whose series' genres
//!   (one batched lookup per pass), include [`STYLED_GENRES`] are looked at.
//!   This is the phase-1 default pending the FR-015 open question.
//! - **One at a time, at background priority.** Reading the script and the
//!   signal pass each take a `bg_io` permit, so the job yields to playback
//!   (V34). It also starts no item while the transcode scheduler has jobs
//!   queued, polling every few seconds.
//! - **A failure is a result.** A script that does not parse, or a file the
//!   signal pass cannot decode, gets a version-only row (`plan_json` empty,
//!   outcome `undecodable`) so it is not retried until the file changes.
//!   Failing to extract the script, a worker crash or timeout, or a store
//!   failure says nothing about the file: no row, retried next pass.
//! - **No source is a result too.** An item with no usable text track gets a
//!   version-only row keyed [`NO_SOURCE_INDEX`] with outcome `no_source`.
//!
//! # On demand
//!
//! [`OnDemand::request`] puts the item a viewer just opened ahead of the
//! backfill: it is a small deduped FIFO, checked before every backfill item
//! and woken between passes.
//!
//! # Signals
//!
//! ```text
//! pharos_subtitle_styling_total{outcome,stage}   outcome: styled | no_source | undecodable | error | missing | unscanned
//!                                                stage:   select | read | signals | direct | store
//! pharos_subtitle_styling_seconds{stage}         read | signals | direct, for styled items
//!                                                (signals only when measured, not read back)
//! pharos_subtitle_styling_pending                items needing work at the start of a pass
//! pharos_subtitle_effect_total{intent,source}    one per styled line; source: rule (phase 1)
//! pharos_subtitle_signals_total{source,outcome}  source: stored | measured; outcome: ok | failed
//! pharos_subtitle_bottom_text_total{verdict}     one per styled line: unchecked | clear | lifted | kept
//! pharos_subtitle_sung_total{outcome}            one per styled item: off | none | asked | failed
//! pharos_subtitle_karaoke_total{timing}          one per karaoke line: source | aligned | paced
//!                                                (text burned into the bottom of the picture, B1207)
//! ```
//!
//! The signal pass is stored per item (B1202): a re-direct of an unchanged
//! file over the same line windows and audio stream reads it back instead of
//! decoding the audio again (`signals_source=stored` on the item line).
//!
//! Every item the job works on is counted once, at the stage it ended in
//! (`styled` ends in `store`); as in `audio_analysis`, a `missing` or
//! `unscanned` item is counted again each pass until the scanner catches up, and leaves one log line with its inputs and verdict, failures
//! included (V535). Each non-plain line decision is a debug line,
//! `subtitle effect`, with the rule and numbers that chose it.

use crate::api::jellyfin::subtitles::{
    discover_sidecars, embedded_ass_script, embedded_vtt, is_image_subtitle_codec,
    sidecar_base_index, sidecar_language_from_name,
};
use crate::bg_io::BgPermit;
use crate::services::lyrics::align::Aligner;
use pharos_cache::SubtitleCache;
use pharos_core::{
    AudioTrack, DetectedSegment, GenreStore, MediaId, MediaItem, MediaKind, MediaSegmentKind,
    MediaSegmentStore, MediaStore, SeriesMetadata, SeriesMetadataStore, SubtitleSignalsRow,
    SubtitleStyling, SubtitleStylingStore,
};
use pharos_styling::plan::{Intent, Placement};
use pharos_styling::signals::{LineSignals, Segment, SegmentKind, SungLine, TrackSignals};
use pharos_styling::source::SourceLine;
use pharos_styling::DIRECTOR_VERSION;
use pharos_transcode::protocol::WorkerError;
use pharos_transcode::scheduler::TranscodeScheduler;
use pharos_transcode::subtitle_signals::{SubtitleSignals, SIGNALS_VERSION};
use pharos_transcode::worker::{LibavWorkerPool, PoolError};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
#[cfg(all(unix, feature = "ffmpeg-lib"))]
use std::sync::atomic::AtomicBool;
#[cfg(any(test, all(unix, feature = "ffmpeg-lib")))]
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};

/// A subtitle track the job could style: an embedded stream or a sidecar file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleCandidate {
    /// The stream index the HTTP routes use; sidecars sit above
    /// `sidecar_base_index`.
    pub index: u32,
    pub language: Option<String>,
    pub codec: Option<String>,
    pub hearing_impaired: bool,
    pub is_default: bool,
    pub forced: bool,
    /// The file, for a sidecar; `None` for an embedded stream.
    pub sidecar: Option<PathBuf>,
}

impl SubtitleCandidate {
    fn is_text(&self) -> bool {
        self.codec
            .as_deref()
            .is_some_and(|c| !c.is_empty() && !is_image_subtitle_codec(c))
    }

    fn is_ass(&self) -> bool {
        self.codec.as_deref().is_some_and(|c| {
            matches!(
                c.to_ascii_lowercase().as_str(),
                "ass" | "ssa" | "advanced substation alpha"
            )
        })
    }
}

fn is_english(language: Option<&str>) -> bool {
    let Some(l) = language else { return false };
    let l = l.trim().to_ascii_lowercase();
    matches!(l.as_str(), "eng" | "en" | "english")
        || l.starts_with("en-")
        || l.starts_with("en_")
        || l.starts_with("eng-")
}

fn is_untagged(language: Option<&str>) -> bool {
    language.map_or(true, |l| {
        let l = l.trim();
        l.is_empty() || l.eq_ignore_ascii_case("und")
    })
}

/// The source track (FR-016): a full English text track, never a forced one,
/// or an untagged one when it is the only text track. Among those, hearing
/// impaired first (its sound tags feed the director), then ASS/SSA over the
/// rest (italics and positions survive), then the default flag, then the
/// lowest index. Image codecs are never chosen.
pub fn choose_source(candidates: &[SubtitleCandidate]) -> Option<&SubtitleCandidate> {
    let text_tracks = candidates.iter().filter(|c| c.is_text()).count();
    candidates
        .iter()
        .filter(|c| c.is_text() && !c.forced)
        .filter(|c| {
            is_english(c.language.as_deref())
                || (text_tracks == 1 && is_untagged(c.language.as_deref()))
        })
        .min_by_key(|c| (!c.hearing_impaired, !c.is_ass(), !c.is_default, c.index))
}

/// The audio stream the subtitles are timed to (FR-012): the default track
/// when it is not English, else the first non-English track (an English dub
/// is not what the subtitles follow), else the default, else the first.
pub fn choose_audio(tracks: &[AudioTrack]) -> Option<u32> {
    let default = tracks.iter().find(|t| t.is_default);
    let non_english = |t: &&AudioTrack| !is_english(t.language.as_deref());
    default
        .filter(non_english)
        .or_else(|| tracks.iter().find(non_english))
        .or(default)
        .or_else(|| tracks.first())
        .map(|t| t.stream_index)
}

/// Genres that put an item in scope (FR-015). Phase-1 default: the spec's open
/// question is whether every video with an English text track should be
/// styled; until Alison answers, only these genres are.
pub const STYLED_GENRES: [&str; 2] = ["anime", "animation"];

/// Whether any of `genres` is one of [`STYLED_GENRES`], ignoring case.
pub fn has_styled_genre(genres: &[String]) -> bool {
    genres.iter().any(|g| {
        let g = g.trim();
        STYLED_GENRES.iter().any(|s| g.eq_ignore_ascii_case(s))
    })
}

/// Whether the item's own genre tag (`|` or `,` separated) is a styled genre.
fn item_has_styled_genre(i: &MediaItem) -> bool {
    i.probe
        .genre
        .as_deref()
        .is_some_and(|g| has_styled_genre(&pharos_core::split_genre_field(g)))
}

/// `source_index` of a row written when an item has no usable source track.
/// No real stream index is this large; the row only has to be keyed by
/// something so the item is not retried until its file or the director changes.
pub const NO_SOURCE_INDEX: u32 = u32::MAX;

/// How often a waiting item re-checks the transcode queue.
#[cfg(all(unix, feature = "ffmpeg-lib"))]
const QUEUE_POLL: Duration = Duration::from_secs(5);
/// How long reading one source track may take. A cold embedded extraction
/// demuxes the whole file, so this is generous; it exists so a hung mount
/// cannot hold the job (and its background permit) forever.
const READ_TIMEOUT: Duration = Duration::from_secs(600);

/// The `source` label of `pharos_subtitle_effect_total` (FR-021): what chose
/// the effect, `rule` for the phase 1 director and `model` once a learned one
/// exists. The signal that fired is in the line's `reason`, logged per line,
/// never a label.
pub const EFFECT_SOURCE: &str = "rule";

/// The signal pass the job needs. The libav pool in production; a fake in tests.
pub trait Signals: Send + Sync {
    fn signals(
        &self,
        input: PathBuf,
        audio_stream: Option<u32>,
        windows: Vec<(u64, u64)>,
    ) -> impl std::future::Future<Output = Result<SubtitleSignals, PoolError>> + Send;
}

impl Signals for LibavWorkerPool {
    async fn signals(
        &self,
        input: PathBuf,
        audio_stream: Option<u32>,
        windows: Vec<(u64, u64)>,
    ) -> Result<SubtitleSignals, PoolError> {
        self.subtitle_signals(input, audio_stream, windows).await
    }
}

/// How many transcode jobs are queued (design §8a: styling starts nothing
/// while a viewer's transcode waits). The scheduler in production.
pub trait TranscodeQueue: Send + Sync {
    fn pending(&self) -> impl std::future::Future<Output = usize> + Send;
}

impl TranscodeQueue for Option<TranscodeScheduler> {
    async fn pending(&self) -> usize {
        match self {
            Some(s) => s.snapshot().await.map_or(0, |snap| snap.pending),
            None => 0,
        }
    }
}

/// Items a viewer just opened, to be styled ahead of the backfill.
///
/// A cheap-to-clone handle on a small shared FIFO. It is not a channel:
/// `request` must never block or fail a page load, so it only appends under
/// a short lock. It dedupes (opening the same title twice queues it once) and
/// is capped at [`ON_DEMAND_CAP`]; a request past the cap is dropped, which is
/// harmless because the backfill reaches every item anyway.
#[derive(Clone, Default)]
pub struct OnDemand {
    inner: Arc<OnDemandInner>,
}

#[derive(Default)]
struct OnDemandInner {
    queue: std::sync::Mutex<VecDeque<MediaId>>,
    wake: Notify,
}

/// Most items the on-demand queue holds.
const ON_DEMAND_CAP: usize = 256;

impl OnDemand {
    /// Ask for `item` to be styled next. Never blocks.
    pub fn request(&self, item: MediaId) {
        {
            let mut q = self
                .inner
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if q.contains(&item) || q.len() >= ON_DEMAND_CAP {
                return;
            }
            q.push_back(item);
        }
        self.inner.wake.notify_one();
    }

    fn pop(&self) -> Option<MediaId> {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }
}

/// What the stores must offer the job.
pub trait JobStore:
    MediaStore + SubtitleStylingStore + SeriesMetadataStore + MediaSegmentStore + GenreStore
{
}
impl<S> JobStore for S where
    S: MediaStore + SubtitleStylingStore + SeriesMetadataStore + MediaSegmentStore + GenreStore
{
}

/// The `stage` label: the step an item ended in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Select,
    Read,
    Signals,
    Direct,
    Store,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Select => "select",
            Stage::Read => "read",
            Stage::Signals => "signals",
            Stage::Direct => "direct",
            Stage::Store => "store",
        }
    }
}

/// How one item ended: the `outcome` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A plan was made and stored.
    Styled,
    /// No usable source track; stored as such so it is not looked at again.
    NoSource,
    /// The source or the media could not be read; stored version-only.
    Undecodable,
    /// The worker died or timed out, or the store failed; nothing stored, so
    /// the next pass retries.
    Error,
    /// The file could not be stat'd.
    Missing,
    /// The file differs from what the last scan saw.
    Unscanned,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Styled => "styled",
            Outcome::NoSource => "no_source",
            Outcome::Undecodable => "undecodable",
            Outcome::Error => "error",
            Outcome::Missing => "missing",
            Outcome::Unscanned => "unscanned",
        }
    }
}

/// Totals for one pass, for its summary line and the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassSummary {
    /// Items that needed work at the start of the pass.
    pub pending: usize,
    /// Items outside the job's scope (not a local video, or not a styled genre).
    pub skipped: usize,
    pub styled: usize,
    pub no_source: usize,
    pub undecodable: usize,
    pub errors: usize,
    pub missing: usize,
    pub unscanned: usize,
}

impl PassSummary {
    fn record(&mut self, o: Outcome) {
        match o {
            Outcome::Styled => self.styled += 1,
            Outcome::NoSource => self.no_source += 1,
            Outcome::Undecodable => self.undecodable += 1,
            Outcome::Error => self.errors += 1,
            Outcome::Missing => self.missing += 1,
            Outcome::Unscanned => self.unscanned += 1,
        }
    }
}

/// One item that passed every gate and needs styling.
struct Task {
    item: MediaItem,
    /// The video file's signature (whole-second mtime, size).
    sig: (i64, u64),
    /// `None`: the item has no usable source track.
    source: Option<SubtitleCandidate>,
    sidecar_sig: Option<(i64, u64)>,
    audio_stream: Option<u32>,
}

/// A failed or ended-early item: what to count and log.
struct Ended {
    outcome: Outcome,
    stage: Stage,
    error: Option<String>,
}

/// Everything the one log line per item reports.
#[derive(Default)]
struct Report {
    lines: usize,
    plain_share: f64,
    counts: String,
    read: Option<Duration>,
    signals: Option<Duration>,
    direct: Option<Duration>,
    /// Demux read errors in the signal pass; past its limit the rest of the
    /// audio is skipped and those lines read as having no signal.
    read_errors: u32,
    /// Seconds of audio the signal pass decoded.
    analysed_s: Option<f32>,
    signals_source: Option<SignalsSource>,
    /// The dialogue loudness the director measured against (B1204).
    loudness: Option<pharos_styling::director::Loudness>,
    /// Why a stored pass was not used, when there was one.
    signals_stale: Option<&'static str>,
    /// Lines per bottom-text verdict, `verdict=n` (B1207).
    bottom_text: String,
    /// Lines passed through as signs in their own style (B1208).
    signs: usize,
    /// Signs per cause, `tag=n style7=n` (B1208).
    sign_causes: String,
    /// Whether the aligner was asked about the sung lines (B1212).
    sung: Option<SungAsk>,
    /// Median aligner score of the sung lines.
    sung_score: Option<f32>,
    /// Karaoke lines per timing, `timing=n` (B1212).
    karaoke: String,
    /// Speakers the plan gives a colour slot.
    speakers: usize,
}

/// What came of asking the aligner about an item's sung lines (B1212).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SungAsk {
    /// No aligner is configured.
    Off,
    /// No line inside an intro or outro could take a fill.
    None,
    Asked,
    /// The aligner could not answer; the lines are paced instead.
    Failed,
}

impl SungAsk {
    pub const ALL: [SungAsk; 4] = [SungAsk::Off, SungAsk::None, SungAsk::Asked, SungAsk::Failed];

    /// The metric label; a dashboard contract.
    pub fn label(self) -> &'static str {
        match self {
            SungAsk::Off => "off",
            SungAsk::None => "none",
            SungAsk::Asked => "asked",
            SungAsk::Failed => "failed",
        }
    }
}

/// A stored signal pass: the windows it measured and what it found.
/// `keyframes_ms` is stored empty; the director uses `cuts_ms`.
#[derive(serde::Deserialize)]
struct StoredSignals {
    windows: Vec<(u64, u64)>,
    signals: SubtitleSignals,
}

/// [`StoredSignals`] borrowed, for writing it without copying the pass.
#[derive(serde::Serialize)]
struct StoredSignalsRef<'a> {
    windows: &'a [(u64, u64)],
    signals: SignalsRef<'a>,
}

/// [`SubtitleSignals`] field for field, borrowed, without the keyframes.
#[derive(serde::Serialize)]
struct SignalsRef<'a> {
    lines: &'a [pharos_transcode::subtitle_signals::LineSignals],
    keyframes_ms: [u64; 0],
    cuts_ms: &'a [u64],
    bottom_text: &'a [Option<f32>],
    audio_stream: Option<u32>,
    channel: pharos_transcode::subtitle_signals::ChannelPick,
    analysed_seconds: f32,
    read_errors: u32,
}

/// Where an item's signals came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SignalsSource {
    #[default]
    Measured,
    Stored,
}

impl SignalsSource {
    pub fn label(self) -> &'static str {
        match self {
            SignalsSource::Measured => "measured",
            SignalsSource::Stored => "stored",
        }
    }
}

/// The job: stores, the signal pass, and the gates around them.
pub struct Styler<S, A, Q> {
    pub store: S,
    pub signals: A,
    pub queue: Q,
    pub bg_io: Arc<Semaphore>,
    pub cache: Option<SubtitleCache>,
    pub queue_poll: Duration,
    /// Times sung lines that have no `\k` (B1212); `None` paces them.
    pub aligner: Option<Arc<Aligner>>,
}

/// Spawn the job. Fire-and-forget: a failure aborts only the job (logged),
/// never the server. Returns the on-demand handle.
#[cfg(all(unix, feature = "ffmpeg-lib"))]
pub fn spawn<S>(
    store: S,
    bg_leader: Arc<AtomicBool>,
    bg_io: Arc<Semaphore>,
    pool: LibavWorkerPool,
    cache: Option<SubtitleCache>,
    scheduler: Option<TranscodeScheduler>,
    aligner: Option<Arc<Aligner>>,
) -> OnDemand
where
    S: JobStore + 'static,
{
    tracing::info!(
        director_version = DIRECTOR_VERSION,
        "subtitle styling: spawning the background job"
    );
    let on_demand = OnDemand::default();
    let handle = on_demand.clone();
    let styler = Styler {
        store,
        signals: pool,
        queue: scheduler,
        bg_io,
        cache,
        queue_poll: QUEUE_POLL,
        aligner,
    };
    tokio::spawn(async move {
        tokio::time::sleep(crate::audio_analysis::WARMUP).await;
        // One replica styles; a second would read the same files again.
        while !bg_leader.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        styler
            .run(&on_demand, crate::audio_analysis::PASS_INTERVAL)
            .await;
    });
    handle
}

impl<S: JobStore, A: Signals, Q: TranscodeQueue> Styler<S, A, Q> {
    /// Pass after pass, forever. Between passes the job sleeps until the next
    /// one is due or an on-demand request arrives.
    pub async fn run(&self, on_demand: &OnDemand, pass_interval: Duration) {
        loop {
            self.run_pass(on_demand).await;
            let due = tokio::time::Instant::now() + pass_interval;
            loop {
                self.drain_on_demand(on_demand, &mut HashSet::new()).await;
                tokio::select! {
                    () = tokio::time::sleep_until(due) => break,
                    () = on_demand.inner.wake.notified() => {}
                }
            }
        }
    }

    /// One backfill pass over the library, taking on-demand items ahead of
    /// each backfill item.
    pub async fn run_pass(&self, on_demand: &OnDemand) -> PassSummary {
        let mut sum = PassSummary::default();
        let items = match self.store.list().await {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!(error = %e, "subtitle styling: item list failed");
                return sum;
            }
        };
        let rows: HashMap<MediaId, SubtitleStyling> =
            match self.store.subtitle_styling_index().await {
                Ok(rows) => rows.into_iter().map(|r| (r.item_id, r)).collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "subtitle styling: row index read failed");
                    return sum;
                }
            };
        let in_kind: Vec<&MediaItem> = items.iter().filter(|i| in_scope_kind(i)).collect();
        let styled_ids = self.styled_genre_ids(&in_kind).await;
        sum.skipped = items.len() - styled_ids.len();

        let mut work = Vec::new();
        for item in items.iter().filter(|i| styled_ids.contains(&i.id)) {
            match self.prepare(item, rows.get(&item.id), &mut sum).await {
                Some(task) => work.push(task),
                None => continue,
            }
        }
        // V613: an item that had a styled plan goes before one that never
        // had one. After a director bump that plan is still being served,
        // so it is what viewers see; a file changed since is also redone
        // early. The index rows carry no plan, so `outcome` says which had
        // one. The sort is stable, so each group keeps the library's order.
        work.sort_by_cached_key(|t| {
            !rows
                .get(&t.item.id)
                .is_some_and(|r| r.outcome == Outcome::Styled.label())
        });
        sum.pending = work.len();
        metrics::gauge!("pharos_subtitle_styling_pending").set(work.len() as f64);
        if work.is_empty() && sum.missing + sum.unscanned == 0 {
            return sum;
        }
        tracing::info!(pending = work.len(), "subtitle styling: pass starting");

        let mut done = HashSet::new();
        for task in work {
            self.drain_on_demand(on_demand, &mut done).await;
            if !done.insert(task.item.id) {
                continue;
            }
            let ended = self.process(&task).await;
            sum.record(ended);
        }
        tracing::info!(
            pending = sum.pending,
            skipped = sum.skipped,
            styled = sum.styled,
            no_source = sum.no_source,
            undecodable = sum.undecodable,
            errors = sum.errors,
            missing = sum.missing,
            unscanned = sum.unscanned,
            "subtitle styling: pass complete",
        );
        sum
    }

    /// Style every queued on-demand item that needs it.
    async fn drain_on_demand(&self, on_demand: &OnDemand, done: &mut HashSet<MediaId>) {
        while let Some(id) = on_demand.pop() {
            if !done.insert(id) {
                continue;
            }
            let item = match self.store.get(id).await {
                Ok(item) => item,
                Err(e) => {
                    tracing::debug!(item = id, error = %e, "subtitle styling: on-demand item unreadable");
                    continue;
                }
            };
            if !in_scope_kind(&item) || !self.styled_genre_ids(&[&item]).await.contains(&id) {
                continue;
            }
            let row = match self.store.subtitle_styling(id).await {
                Ok(row) => row,
                Err(e) => {
                    tracing::warn!(item = id, error = %e, "subtitle styling: row read failed");
                    continue;
                }
            };
            let mut sum = PassSummary::default();
            if let Some(task) = self.prepare(&item, row.as_ref(), &mut sum).await {
                self.process(&task).await;
            }
        }
    }

    /// The ids among `items` whose own genres, or whose series' genres,
    /// include a styled genre. An episode's genres usually live in the genre
    /// links (`item_genres`, from its NFO or provider), not in the probe's tag,
    /// so those are read first: one lookup per styled genre name. Then one
    /// batched series lookup for the rest.
    async fn styled_genre_ids(&self, items: &[&MediaItem]) -> HashSet<MediaId> {
        let mut linked: HashSet<MediaId> = HashSet::new();
        match self.store.genres_with_counts().await {
            Ok(all) => {
                for g in all
                    .iter()
                    .filter(|g| has_styled_genre(std::slice::from_ref(&g.genre.name)))
                {
                    match self.store.item_ids_for_genre(&g.genre.wire_id).await {
                        Ok(ids) => linked.extend(ids),
                        Err(e) => {
                            tracing::warn!(genre = %g.genre.name, error = %e, "subtitle styling: genre items read failed")
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "subtitle styling: genre list read failed"),
        }
        let tagged = |i: &MediaItem| linked.contains(&i.id) || item_has_styled_genre(i);
        let mut keys: Vec<String> = items
            .iter()
            .filter(|i| !tagged(i))
            .filter_map(|i| i.series.as_ref().map(|s| s.series_key().to_owned()))
            .collect();
        keys.sort();
        keys.dedup();
        let mut series: HashMap<String, SeriesMetadata> = HashMap::new();
        for chunk in keys.chunks(500) {
            match self.store.series_metadata_by_keys(chunk).await {
                Ok(found) => series.extend(found),
                Err(e) => tracing::warn!(error = %e, "subtitle styling: series genres read failed"),
            }
        }
        items
            .iter()
            .filter(|i| {
                tagged(i)
                    || i.series
                        .as_ref()
                        .and_then(|s| series.get(s.series_key()))
                        .is_some_and(|m| has_styled_genre(&m.genres))
            })
            .map(|i| i.id)
            .collect()
    }

    /// The remaining gates for an in-scope item: the file, the source track,
    /// and whether the stored row is still current. `None` = nothing to do
    /// (counted when it is a missing or unscanned file).
    async fn prepare(
        &self,
        item: &MediaItem,
        row: Option<&SubtitleStyling>,
        sum: &mut PassSummary,
    ) -> Option<Task> {
        let Some(sig) = signature(&item.path).await else {
            tracing::warn!(
                item = item.id,
                path = %item.path.display(),
                "subtitle styling: file not found; skipped until the scanner catches up",
            );
            count(Outcome::Missing, Stage::Select);
            sum.record(Outcome::Missing);
            return None;
        };
        let candidates = candidates_for(item).await;
        let source = choose_source(&candidates).cloned();
        let sidecar_sig = match source.as_ref().and_then(|s| s.sidecar.as_deref()) {
            Some(p) => signature(p).await,
            None => None,
        };
        let index = source.as_ref().map_or(NO_SOURCE_INDEX, |s| s.index);
        if row.is_some_and(|r| is_current(r, sig, index, sidecar_sig)) {
            return None;
        }
        // A file changed since the last scan is left to the scanner, which
        // stamps the new signature first (as audio analysis does).
        match self.store.scan_state(item.id).await {
            Ok(Some(st)) if st.file_mtime != 0 && (st.file_mtime, st.file_size) != sig => {
                tracing::debug!(
                    item = item.id,
                    path = %item.path.display(),
                    "subtitle styling: file changed since the last scan; waiting for the scanner",
                );
                count(Outcome::Unscanned, Stage::Select);
                sum.record(Outcome::Unscanned);
                return None;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(item = item.id, error = %e, "subtitle styling: scan state read failed");
            }
        }
        Some(Task {
            item: item.clone(),
            sig,
            source,
            sidecar_sig,
            audio_stream: choose_audio(&item.probe.audio_tracks),
        })
    }

    /// Wait out the transcode queue, then style one item and report it.
    async fn process(&self, task: &Task) -> Outcome {
        let mut waited = false;
        while self.queue.pending().await > 0 {
            if !waited {
                waited = true;
                tracing::debug!(
                    item = task.item.id,
                    "subtitle styling: waiting for the transcode queue to empty",
                );
            }
            tokio::time::sleep(self.queue_poll).await;
        }
        let mut report = Report::default();
        let ended = match self.style(task, &mut report).await {
            Ok(()) => Ended {
                outcome: Outcome::Styled,
                stage: Stage::Store,
                error: None,
            },
            Err(ended) => ended,
        };
        self.emit(task, &report, &ended);
        ended.outcome
    }

    /// Asks the aligner where the item's sung lines are sung (B1212). A
    /// failure is logged and the lines are paced: the item is styled anyway.
    async fn sung(
        &self,
        item: &MediaItem,
        lines: &[SourceLine],
        segments: &[Segment],
    ) -> (Vec<Option<SungLine>>, SungAsk) {
        let Some(aligner) = &self.aligner else {
            return (Vec::new(), SungAsk::Off);
        };
        let asked = pharos_styling::director::sung_candidates(lines, segments);
        if asked.is_empty() {
            return (Vec::new(), SungAsk::None);
        }
        let spans: Vec<(u64, u64, String)> = asked
            .iter()
            .map(|&i| {
                let l = &lines[i];
                let ms = |v: i64| u64::try_from(v).unwrap_or(0);
                (ms(l.start_ms), ms(l.end_ms), l.text.clone())
            })
            .collect();
        let t = Instant::now();
        match aligner.sing(item, &spans).await {
            Ok(answer) => (sung_lines(lines.len(), &asked, &answer), SungAsk::Asked),
            Err(e) => {
                tracing::warn!(
                    item = item.id,
                    lines = asked.len(),
                    elapsed_s = t.elapsed().as_secs_f32(),
                    error = %e,
                    "subtitle styling: aligner failed; sung lines are paced"
                );
                (Vec::new(), SungAsk::Failed)
            }
        }
    }

    /// The stages after `select`. `Err` carries how the item ended early.
    async fn style(&self, task: &Task, report: &mut Report) -> Result<(), Ended> {
        let Some(source) = task.source.as_ref() else {
            self.put(task, Outcome::NoSource, NO_SOURCE_INDEX, None)
                .await?;
            return Err(Ended {
                outcome: Outcome::NoSource,
                stage: Stage::Select,
                error: None,
            });
        };

        let t = Instant::now();
        let read = {
            let permit = BgPermit::acquire(&self.bg_io).await;
            tokio::time::timeout(
                READ_TIMEOUT,
                read_source(self.cache.as_ref(), &task.item, source, &permit),
            )
            .await
        };
        report.read = Some(t.elapsed());
        // Failing to get the text out (an unreadable sidecar, ffmpeg failing
        // or hanging on a slow mount) says nothing about the track itself:
        // no row, retried next pass. Only text that does not parse is
        // remembered as undecodable.
        let script = match read {
            Ok(Ok(script)) => script,
            Ok(Err(e)) => {
                return Err(Ended {
                    outcome: Outcome::Error,
                    stage: Stage::Read,
                    error: Some(e),
                })
            }
            Err(_) => {
                return Err(Ended {
                    outcome: Outcome::Error,
                    stage: Stage::Read,
                    error: Some(format!("read timed out after {}s", READ_TIMEOUT.as_secs())),
                })
            }
        };
        let parsed = pharos_styling::source::read(&script)
            .map_err(|e| format!("parse: {e}"))
            .and_then(|s| {
                if s.lines.is_empty() {
                    Err("no lines".to_owned())
                } else {
                    Ok(s)
                }
            });
        let parsed = match parsed {
            Ok(p) => p,
            Err(e) => return self.undecodable(task, source, Stage::Read, e).await,
        };
        report.lines = parsed.lines.len();

        let windows: Vec<(u64, u64)> = parsed
            .lines
            .iter()
            .map(|l| (non_negative(l.start_ms), non_negative(l.end_ms)))
            .collect();
        let t = Instant::now();
        let stored = match self.stored_signals(task, &windows).await {
            Ok(s) => Some(s),
            Err(why) => {
                report.signals_stale = why;
                None
            }
        };
        let from = if stored.is_some() {
            SignalsSource::Stored
        } else {
            SignalsSource::Measured
        };
        let result = match stored {
            Some(s) => Ok(s),
            None => {
                let _permit = BgPermit::acquire(&self.bg_io).await;
                self.signals
                    .signals(task.item.path.clone(), task.audio_stream, windows.clone())
                    .await
            }
        };
        report.signals = Some(t.elapsed());
        report.signals_source = Some(from);
        metrics::counter!(
            "pharos_subtitle_signals_total",
            "source" => from.label(),
            "outcome" => if result.is_ok() { "ok" } else { "failed" },
        )
        .increment(1);
        let sig = match result {
            Ok(s) => {
                if from == SignalsSource::Measured {
                    report.read_errors = s.read_errors;
                    report.analysed_s = Some(s.analysed_seconds);
                    // A pass that skipped audio may do better next time.
                    if s.read_errors == 0 {
                        self.store_signals(task, &windows, &s).await;
                    }
                }
                s
            }
            Err(e) if is_bad_input(&e) => {
                return self
                    .undecodable(task, source, Stage::Signals, e.to_string())
                    .await
            }
            Err(e) => {
                return Err(Ended {
                    outcome: Outcome::Error,
                    stage: Stage::Signals,
                    error: Some(e.to_string()),
                })
            }
        };

        let segments = match self.store.media_segments_for(task.item.id).await {
            Ok(found) => found.iter().map(segment_of).collect(),
            Err(e) => {
                tracing::warn!(item = task.item.id, error = %e, "subtitle styling: segments read failed; directing without them");
                Vec::new()
            }
        };
        let (sung, ask) = self.sung(&task.item, &parsed.lines, &segments).await;
        report.sung = Some(ask);
        let t = Instant::now();
        let track = TrackSignals {
            lines: sig.lines.iter().map(line_signals).collect(),
            cuts_ms: sig.cuts_ms,
            bottom_text: sig.bottom_text,
            sung,
        };
        let direction = pharos_styling::direct(&parsed.lines, &track, &segments);
        report.direct = Some(t.elapsed());
        report.loudness = Some(direction.loudness);
        let plan_json = serde_json::to_string(&direction.plan).map_err(|e| Ended {
            outcome: Outcome::Error,
            stage: Stage::Direct,
            error: Some(format!("serialise plan: {e}")),
        })?;
        let plain = direction.counts.get(&Intent::Plain).copied().unwrap_or(0);
        report.plain_share = plain as f64 / parsed.lines.len() as f64;
        report.counts = direction
            .counts
            .iter()
            .map(|(i, n)| format!("{}={n}", i.label()))
            .collect::<Vec<_>>()
            .join(" ");
        report.signs = parsed.lines.iter().filter(|l| l.typeset).count();
        report.sign_causes = sign_causes(&parsed.lines);
        report.sung_score = direction.sung_score;
        report.speakers = direction.speakers;
        report.karaoke = direction
            .karaoke
            .iter()
            .map(|(t, n)| format!("{}={n}", t.label()))
            .collect::<Vec<_>>()
            .join(" ");
        report.bottom_text = direction
            .bottom_text
            .iter()
            .map(|(v, n)| format!("{}={n}", v.label()))
            .collect::<Vec<_>>()
            .join(" ");

        self.put(task, Outcome::Styled, source.index, Some(plan_json))
            .await?;

        // A stored pass is a database read, not the decode this stage times.
        let measured = report.signals_source == Some(SignalsSource::Measured);
        for (stage, d) in [
            ("read", report.read),
            ("signals", report.signals.filter(|_| measured)),
            ("direct", report.direct),
        ] {
            if let Some(d) = d {
                metrics::histogram!("pharos_subtitle_styling_seconds", "stage" => stage)
                    .record(d.as_secs_f64());
            }
        }
        // Counted once the item is stored, so a failed store and its retry
        // do not count the item twice.
        metrics::counter!("pharos_subtitle_sung_total", "outcome" => ask.label()).increment(1);
        for (timing, n) in &direction.karaoke {
            metrics::counter!("pharos_subtitle_karaoke_total", "timing" => timing.label())
                .increment(*n as u64);
        }
        for (verdict, n) in &direction.bottom_text {
            metrics::counter!(
                "pharos_subtitle_bottom_text_total",
                "verdict" => verdict.label(),
            )
            .increment(*n as u64);
        }
        for fx in &direction.plan.lines {
            metrics::counter!(
                "pharos_subtitle_effect_total",
                "intent" => fx.intent.label(),
                "source" => EFFECT_SOURCE,
            )
            .increment(1);
            if fx.intent != Intent::Plain || fx.placement != Placement::Default {
                tracing::debug!(
                    item = task.item.id,
                    line = fx.line,
                    start_ms = parsed.lines.get(fx.line).map(|l| l.start_ms),
                    intent = fx.intent.label(),
                    intensity = fx.intensity,
                    reason = %fx.reason,
                    "subtitle effect",
                );
            }
        }
        Ok(())
    }

    /// The item's stored signal pass when it still describes this file, audio
    /// stream and set of line windows. `Err` names why not (`None`: there was
    /// none stored, or the read failed and was logged).
    async fn stored_signals(
        &self,
        task: &Task,
        windows: &[(u64, u64)],
    ) -> Result<SubtitleSignals, Option<&'static str>> {
        let row = match self.store.subtitle_signals(task.item.id).await {
            Ok(Some(row)) => row,
            Ok(None) => return Err(None),
            Err(e) => {
                tracing::warn!(item = task.item.id, error = %e, "subtitle styling: stored signals read failed; measuring");
                return Err(None);
            }
        };
        if row.signals_version != SIGNALS_VERSION {
            return Err(Some("version"));
        }
        if (row.source_mtime, row.source_size) != task.sig {
            return Err(Some("file"));
        }
        if row.audio_stream != task.audio_stream {
            return Err(Some("audio_stream"));
        }
        let stored: StoredSignals = match serde_json::from_str(&row.signals_json) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(item = task.item.id, error = %e, "subtitle styling: stored signals do not parse; measuring");
                return Err(Some("parse"));
            }
        };
        if stored.windows != windows || stored.signals.lines.len() != windows.len() {
            return Err(Some("windows"));
        }
        Ok(stored.signals)
    }

    /// Keep a fresh signal pass for the next re-direct. A failure costs only
    /// a later re-measure, so it is logged and the item goes on.
    async fn store_signals(&self, task: &Task, windows: &[(u64, u64)], s: &SubtitleSignals) {
        let stored = StoredSignalsRef {
            windows,
            signals: SignalsRef {
                lines: &s.lines,
                keyframes_ms: [],
                cuts_ms: &s.cuts_ms,
                bottom_text: &s.bottom_text,
                audio_stream: s.audio_stream,
                channel: s.channel,
                analysed_seconds: s.analysed_seconds,
                read_errors: s.read_errors,
            },
        };
        let json = match serde_json::to_string(&stored) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(item = task.item.id, error = %e, "subtitle styling: signals not stored (serialise)");
                return;
            }
        };
        let row = SubtitleSignalsRow {
            item_id: task.item.id,
            source_mtime: task.sig.0,
            source_size: task.sig.1,
            audio_stream: task.audio_stream,
            signals_version: SIGNALS_VERSION,
            signals_json: json,
            measured_at: now_secs(),
        };
        if let Err(e) = self.store.put_subtitle_signals(row).await {
            tracing::warn!(item = task.item.id, error = %e, "subtitle styling: signals not stored");
        }
    }

    /// Store a version-only row for a source the media cannot give us, and
    /// end the item as undecodable.
    async fn undecodable(
        &self,
        task: &Task,
        source: &SubtitleCandidate,
        stage: Stage,
        error: String,
    ) -> Result<(), Ended> {
        self.put(task, Outcome::Undecodable, source.index, None)
            .await?;
        Err(Ended {
            outcome: Outcome::Undecodable,
            stage,
            error: Some(error),
        })
    }

    /// Write the item's row. A store failure ends the item as `error` at the
    /// `store` stage, leaving it for the next pass.
    async fn put(
        &self,
        task: &Task,
        outcome: Outcome,
        source_index: u32,
        plan_json: Option<String>,
    ) -> Result<(), Ended> {
        let row = SubtitleStyling {
            item_id: task.item.id,
            source_index,
            source_mtime: task.sig.0,
            source_size: task.sig.1,
            sidecar_mtime: task.sidecar_sig.map(|s| s.0),
            sidecar_size: task.sidecar_sig.map(|s| s.1),
            director_version: DIRECTOR_VERSION,
            outcome: outcome.label().to_owned(),
            plan_json,
            styled_at: now_secs(),
        };
        self.store
            .put_subtitle_styling(row)
            .await
            .map_err(|e| Ended {
                outcome: Outcome::Error,
                stage: Stage::Store,
                error: Some(format!("store write failed: {e}")),
            })
    }

    /// The one place an item's end is counted and logged, so every outcome
    /// is counted exactly once and failures carry the same fields as
    /// successes (V535).
    fn emit(&self, task: &Task, report: &Report, ended: &Ended) {
        count(ended.outcome, ended.stage);
        let secs = |d: Option<Duration>| d.map(|d| d.as_secs_f64());
        let source = task.source.as_ref();
        let styled = ended.outcome == Outcome::Styled;
        macro_rules! line {
            ($mac:ident, $msg:literal) => {
                tracing::$mac!(
                    item = task.item.id,
                    path = %task.item.path.display(),
                    source_index = source.map(|s| s.index),
                    source_codec = source.and_then(|s| s.codec.as_deref()),
                    audio_stream = task.audio_stream,
                    lines = report.lines,
                    plain_share = report.plain_share,
                    intents = %report.counts,
                    read_s = secs(report.read),
                    signals_s = secs(report.signals),
                    direct_s = secs(report.direct),
                    read_errors = report.read_errors,
                    analysed_s = report.analysed_s,
                    signals_source = report.signals_source.map(SignalsSource::label),
                    signals_stale = report.signals_stale,
                    dialogue_lufs = report.loudness.and_then(|l| l.median_lufs),
                    dialogue_spread_lu = report.loudness.and_then(|l| l.spread_lu),
                    loudness_scale = report.loudness.map(|l| l.scale),
                    bottom_text = %report.bottom_text,
                    signs = report.signs,
                    sign_causes = %report.sign_causes,
                    sung = report.sung.map(SungAsk::label),
                    sung_score = report.sung_score,
                    karaoke = %report.karaoke,
                    speakers = report.speakers,
                    outcome = ended.outcome.label(),
                    stage = ended.stage.label(),
                    error = ended.error.as_deref(),
                    $msg,
                )
            };
        }
        if styled || ended.outcome == Outcome::NoSource {
            if styled {
                line!(info, "subtitle styling: item styled");
            } else {
                line!(info, "subtitle styling: item has no source track");
            }
        } else {
            line!(warn, "subtitle styling: item not styled");
        }
    }
}

fn count(outcome: Outcome, stage: Stage) {
    metrics::counter!(
        "pharos_subtitle_styling_total",
        "outcome" => outcome.label(),
        "stage" => stage.label(),
    )
    .increment(1);
}

/// A local movie or episode: the only items the job looks at.
fn in_scope_kind(i: &MediaItem) -> bool {
    matches!(i.kind, MediaKind::Movie | MediaKind::Episode) && i.origin().local().is_some()
}

/// Whether `row` still describes this file, source and director. Stricter
/// than [`crate::styled_track::current`], which keeps serving an older
/// director's plan until this job replaces it (V613).
fn is_current(
    row: &SubtitleStyling,
    sig: (i64, u64),
    source_index: u32,
    sidecar_sig: Option<(i64, u64)>,
) -> bool {
    row.director_version == DIRECTOR_VERSION
        && row.source_index == source_index
        && (row.source_mtime, row.source_size) == sig
        && row.sidecar_mtime.zip(row.sidecar_size) == sidecar_sig
}

fn non_negative(ms: i64) -> u64 {
    u64::try_from(ms).unwrap_or(0)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The file's mtime (whole seconds) and size, as the scanner records them.
pub(crate) async fn signature(path: &std::path::Path) -> Option<(i64, u64)> {
    let m = tokio::fs::metadata(path).await.ok()?;
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    Some((mtime, m.len()))
}

/// The file's own fault (as opposed to a worker that died): remember it.
fn is_bad_input(e: &PoolError) -> bool {
    matches!(
        e,
        PoolError::Op(
            WorkerError::BadInput(_)
                | WorkerError::DecodeFailed(_)
                | WorkerError::UnsupportedCodec(_)
        )
    )
}

fn segment_of(d: &DetectedSegment) -> Segment {
    Segment {
        kind: match d.kind {
            MediaSegmentKind::Intro => SegmentKind::Intro,
            MediaSegmentKind::Outro => SegmentKind::Outro,
            MediaSegmentKind::Recap => SegmentKind::Recap,
            MediaSegmentKind::Preview | MediaSegmentKind::Commercial => SegmentKind::Other,
        },
        start_ms: d.start_ms,
        end_ms: d.end_ms,
    }
}

fn line_signals(s: &pharos_transcode::subtitle_signals::LineSignals) -> LineSignals {
    LineSignals {
        loudness_lufs: s.loudness_lufs,
        peak_dbfs: s.peak_dbfs,
        pitch_hz: s.pitch_hz,
        voiced_ratio: s.voiced_ratio,
        high_band_share: s.high_band_share,
    }
}

/// A sidecar file as a source candidate. Its codec comes from the extension,
/// its language and SDH/forced flags from the dotted name tokens.
fn sidecar_candidate(index: u32, path: &std::path::Path) -> SubtitleCandidate {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    let codec = match lower.rsplit_once('.').map(|(_, e)| e) {
        Some("ass") => "ass",
        Some("ssa") => "ssa",
        Some("vtt") => "webvtt",
        _ => "subrip",
    };
    let has_token = |toks: &[&str]| lower.split('.').any(|t| toks.contains(&t));
    SubtitleCandidate {
        index,
        language: sidecar_language_from_name(name),
        codec: Some(codec.to_owned()),
        hearing_impaired: has_token(&["sdh", "hi", "cc"]),
        is_default: has_token(&["default"]),
        forced: has_token(&["forced"]),
        sidecar: Some(path.to_path_buf()),
    }
}

/// Every subtitle track of `item` the job could style: its embedded streams,
/// then its sidecar files at `sidecar_base_index` + position.
async fn candidates_for(item: &MediaItem) -> Vec<SubtitleCandidate> {
    let mut out: Vec<SubtitleCandidate> = item
        .probe
        .subtitle_tracks
        .iter()
        .map(|t| SubtitleCandidate {
            index: t.stream_index,
            language: t.language.clone(),
            codec: t.codec.clone(),
            hearing_impaired: t.is_hearing_impaired,
            is_default: t.is_default,
            forced: t.is_forced,
            sidecar: None,
        })
        .collect();
    let base = sidecar_base_index(&item.probe);
    for (offset, (path, _)) in discover_sidecars(&item.path).await.iter().enumerate() {
        let index = base.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        out.push(sidecar_candidate(index, path));
    }
    out
}

/// The source track's script text: a sidecar from disk as it is (the styling
/// reader takes SRT, WebVTT and ASS), an embedded ASS stream as ASS, any
/// other embedded text stream as WebVTT, both through the subtitle cache.
async fn read_source(
    cache: Option<&SubtitleCache>,
    item: &MediaItem,
    source: &SubtitleCandidate,
    permit: &BgPermit,
) -> Result<String, String> {
    let bytes = if let Some(path) = source.sidecar.as_deref() {
        tokio::fs::read(path)
            .await
            .map_err(|e| format!("read {}: {e}", path.display()))?
    } else if source.is_ass() {
        embedded_ass_script(cache, item, source.index, permit)
            .await
            .map_err(|e| format!("extract ass: {e}"))?
    } else {
        embedded_vtt(cache, item, source.index, permit)
            .await
            .map_err(|e| format!("extract vtt: {e}"))?
    };
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.trim_start_matches('\u{feff}').to_owned())
}

/// The aligner's answer for the `asked` lines, by source line index.
fn sung_lines(
    n: usize,
    asked: &[usize],
    answer: &crate::services::lyrics::align::Aligned,
) -> Vec<Option<SungLine>> {
    let mut out = vec![None; n];
    for (k, &i) in asked.iter().enumerate() {
        let Some(words) = answer.lines.get(k) else {
            continue;
        };
        if words.is_empty() || i >= n {
            continue;
        }
        out[i] = Some(SungLine {
            words: words
                .iter()
                .map(|w| (w.at as usize, i64::try_from(w.start_ms).unwrap_or(i64::MAX)))
                .collect(),
            score: answer
                .scores
                .get(k)
                .copied()
                .flatten()
                .map(|s| f32::from(s) / 1000.0),
        });
    }
    out
}

/// `tag=n style<alignment>=n`, so a mis-read style alignment shows by value.
fn sign_causes(lines: &[pharos_styling::source::SourceLine]) -> String {
    use pharos_styling::source::SignCause;
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    for cause in lines.iter().filter_map(|l| l.sign_cause()) {
        let key = match cause {
            SignCause::Tag => cause.label().to_owned(),
            SignCause::Style(a) => format!("{}{a}", cause.label()),
        };
        *by.entry(key).or_default() += 1;
    }
    by.iter()
        .map(|(k, n)| format!("{k}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn an_aligner_answer_lands_on_the_lines_it_was_asked_about() {
        use crate::services::lyrics::{align::Aligned, WordStamp};
        let answer = Aligned {
            lines: vec![
                vec![
                    WordStamp {
                        at: 0,
                        start_ms: 1200,
                    },
                    WordStamp {
                        at: 3,
                        start_ms: 1500,
                    },
                ],
                vec![],
            ],
            offset_ms: 0,
            scores: vec![Some(640), None],
        };
        let sung = sung_lines(4, &[1, 3], &answer);
        assert_eq!(sung.len(), 4);
        assert_eq!(sung[0], None);
        assert_eq!(
            sung[1],
            Some(SungLine {
                words: vec![(0, 1200), (3, 1500)],
                score: Some(0.64),
            })
        );
        assert_eq!(sung[3], None, "a line with no words is left to pace");
    }

    #[test]
    fn sung_labels_are_distinct() {
        let labels: HashSet<_> = SungAsk::ALL.iter().map(|a| a.label()).collect();
        assert_eq!(labels.len(), SungAsk::ALL.len());
    }

    #[test]
    fn signs_are_counted_by_cause_and_alignment() {
        let ass = "[V4+ Styles]\n\
            Format: Name, Fontname, Fontsize, Alignment, MarginV\n\
            Style: Menu,Arial,36,7,90\n\
            Style: Default,Arial,48,2,30\n\
            [Events]\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Menu,,0,0,0,,Stats\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Menu,,0,0,0,,More\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\pos(1,1)}Sign\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hello\n";
        let lines = pharos_styling::source::read(ass).unwrap().lines;
        assert_eq!(sign_causes(&lines), "style7=2 tag=1");
    }

    fn sub(index: u32, lang: Option<&str>, codec: &str) -> SubtitleCandidate {
        SubtitleCandidate {
            index,
            language: lang.map(str::to_owned),
            codec: Some(codec.to_owned()),
            hearing_impaired: false,
            is_default: false,
            forced: false,
            sidecar: None,
        }
    }

    fn chosen(c: &[SubtitleCandidate]) -> Option<u32> {
        choose_source(c).map(|c| c.index)
    }

    #[test]
    fn source_is_a_full_english_text_track() {
        assert_eq!(chosen(&[]), None);
        assert_eq!(
            chosen(&[sub(2, Some("jpn"), "ass"), sub(3, Some("eng"), "subrip")]),
            Some(3)
        );
        for lang in ["eng", "en", "English", "en-US"] {
            assert_eq!(chosen(&[sub(4, Some(lang), "subrip")]), Some(4), "{lang}");
        }
        assert_eq!(chosen(&[sub(2, Some("jpn"), "ass")]), None);
    }

    #[test]
    fn forced_and_image_tracks_are_never_the_source() {
        let mut forced = sub(2, Some("eng"), "ass");
        forced.forced = true;
        assert_eq!(chosen(&[forced]), None);
        assert_eq!(
            chosen(&[
                sub(2, Some("eng"), "hdmv_pgs_subtitle"),
                sub(3, Some("eng"), "dvd_subtitle")
            ]),
            None
        );
    }

    #[test]
    fn untagged_counts_only_as_the_single_text_track() {
        assert_eq!(chosen(&[sub(2, None, "subrip")]), Some(2));
        assert_eq!(chosen(&[sub(2, Some("und"), "subrip")]), Some(2));
        assert_eq!(
            chosen(&[sub(2, None, "subrip"), sub(3, Some("jpn"), "ass")]),
            None,
            "a second text track makes the untagged one a guess"
        );
        assert_eq!(
            chosen(&[sub(2, None, "subrip"), sub(3, Some("eng"), "subrip")]),
            Some(3)
        );
        // An image track beside it does not make it ambiguous.
        assert_eq!(
            chosen(&[
                sub(2, None, "subrip"),
                sub(3, Some("jpn"), "hdmv_pgs_subtitle")
            ]),
            Some(2)
        );
    }

    #[test]
    fn source_preference_is_sdh_then_ass_then_default_then_index() {
        let plain_ass = sub(5, Some("eng"), "ass");
        let mut sdh_srt = sub(7, Some("eng"), "subrip");
        sdh_srt.hearing_impaired = true;
        assert_eq!(chosen(&[plain_ass.clone(), sdh_srt.clone()]), Some(7));

        let srt = sub(2, Some("eng"), "subrip");
        assert_eq!(chosen(&[srt.clone(), plain_ass.clone()]), Some(5));

        let mut default_srt = sub(9, Some("eng"), "subrip");
        default_srt.is_default = true;
        assert_eq!(chosen(&[srt.clone(), default_srt]), Some(9));
        assert_eq!(chosen(&[sub(8, Some("eng"), "subrip"), srt]), Some(2));
    }

    fn audio(index: u32, lang: Option<&str>, default: bool) -> AudioTrack {
        AudioTrack {
            stream_index: index,
            language: lang.map(str::to_owned),
            is_default: default,
            ..AudioTrack::default()
        }
    }

    #[test]
    fn audio_follows_the_original_language_not_the_dub() {
        assert_eq!(choose_audio(&[]), None);
        // A Japanese default wins.
        assert_eq!(
            choose_audio(&[audio(1, Some("jpn"), true), audio(2, Some("eng"), false)]),
            Some(1)
        );
        // An English default yields to the first non-English track.
        assert_eq!(
            choose_audio(&[audio(1, Some("eng"), true), audio(2, Some("jpn"), false)]),
            Some(2)
        );
        // No default: the first non-English one.
        assert_eq!(
            choose_audio(&[audio(1, Some("eng"), false), audio(2, Some("jpn"), false)]),
            Some(2)
        );
        // Only English: the default, else the first.
        assert_eq!(
            choose_audio(&[audio(1, Some("eng"), false), audio(2, Some("en"), true)]),
            Some(2)
        );
        assert_eq!(
            choose_audio(&[audio(3, Some("eng"), false), audio(4, Some("eng"), false)]),
            Some(3)
        );
    }

    // ---- the job ----

    use metrics_util::debugging::DebugValue;
    use pharos_core::{MediaProbe, SeriesInfo};
    use pharos_store_sqlx::sqlite::SqliteStore;
    use pharos_transcode::subtitle_signals::{ChannelPick, LineSignals as WireSignals};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Scripted signal pass: a verdict per video file name, and every call.
    #[derive(Default)]
    struct FakeSignals {
        script: HashMap<String, &'static str>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeSignals {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Signals for FakeSignals {
        async fn signals(
            &self,
            input: PathBuf,
            audio_stream: Option<u32>,
            windows: Vec<(u64, u64)>,
        ) -> Result<SubtitleSignals, PoolError> {
            let name = input.file_name().unwrap().to_string_lossy().into_owned();
            self.calls.lock().unwrap().push(name.clone());
            match self.script.get(&name).copied().unwrap_or("ok") {
                "dead" => Err(PoolError::Dead("worker hung".into())),
                "bad" => Err(PoolError::Op(WorkerError::BadInput("no audio".into()))),
                _ => Ok(SubtitleSignals {
                    // The third line is far louder than the rest: a Shout.
                    lines: (0..windows.len())
                        .map(|i| WireSignals {
                            loudness_lufs: Some(if i == 2 { -10.0 } else { -30.0 }),
                            peak_dbfs: Some(-3.0),
                            pitch_hz: Some(200.0),
                            voiced_ratio: 0.9,
                            high_band_share: Some(0.4),
                        })
                        .collect(),
                    keyframes_ms: Vec::new(),
                    cuts_ms: Vec::new(),
                    // Text burned in under the first line only.
                    bottom_text: (0..windows.len())
                        .map(|i| Some(if i == 0 { 0.05 } else { 0.0 }))
                        .collect(),
                    audio_stream,
                    channel: ChannelPick::Downmix,
                    analysed_seconds: 10.0,
                    read_errors: 0,
                }),
            }
        }
    }

    /// A transcode queue that is busy for its first `busy` looks.
    struct FakeQueue {
        busy: usize,
        looks: AtomicUsize,
    }

    impl TranscodeQueue for FakeQueue {
        async fn pending(&self) -> usize {
            let n = self.looks.fetch_add(1, Ordering::SeqCst);
            usize::from(n < self.busy) * 3
        }
    }

    fn idle() -> FakeQueue {
        FakeQueue {
            busy: 0,
            looks: AtomicUsize::new(0),
        }
    }

    const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\nHello there.\n\n\
        2\n00:00:03,000 --> 00:00:04,000\nWhat is it?\n\n\
        3\n00:00:05,000 --> 00:00:06,000\nLook out!\n\n\
        4\n00:00:07,000 --> 00:00:08,000\nI see.\n\n\
        5\n00:00:09,000 --> 00:00:10,000\nLet us go.\n";

    struct Lib {
        dir: TempDir,
        store: SqliteStore,
    }

    /// A library of real files. `(id, file name, genre, has an English
    /// sidecar)`; every item is a movie.
    async fn library(files: &[(u64, &str, Option<&str>, bool)]) -> Lib {
        let dir = TempDir::new().unwrap();
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        for &(id, name, genre, sidecar) in files {
            let path = dir.path().join(name);
            std::fs::write(&path, b"video").unwrap();
            if sidecar {
                let stem = name.rsplit_once('.').unwrap().0;
                std::fs::write(dir.path().join(format!("{stem}.en.srt")), SRT).unwrap();
            }
            store
                .put(MediaItem {
                    id,
                    path,
                    title: name.into(),
                    kind: MediaKind::Movie,
                    probe: MediaProbe {
                        genre: genre.map(str::to_owned),
                        duration_ms: Some(60_000),
                        ..MediaProbe::default()
                    },
                    ..MediaItem::default()
                })
                .await
                .unwrap();
        }
        Lib { dir, store }
    }

    fn styler<'a>(
        lib: &'a Lib,
        signals: &'a FakeSignals,
        queue: FakeQueue,
    ) -> Styler<SqliteStore, &'a FakeSignals, FakeQueue> {
        Styler {
            store: lib.store.clone(),
            signals,
            queue,
            bg_io: Arc::new(Semaphore::new(1)),
            cache: None,
            queue_poll: Duration::from_millis(1),
            aligner: None,
        }
    }

    // `&S` stores: the job only needs the traits through a reference.
    impl<T: Signals> Signals for &T {
        async fn signals(
            &self,
            input: PathBuf,
            audio_stream: Option<u32>,
            windows: Vec<(u64, u64)>,
        ) -> Result<SubtitleSignals, PoolError> {
            (**self).signals(input, audio_stream, windows).await
        }
    }

    const ANIME: Option<&str> = Some("Anime");

    /// (outcome, stage, value) of every `pharos_subtitle_styling_total` sample.
    fn totals(snap: &pharos_test_metrics::CumulativeSnapshotter) -> Vec<(String, String, u64)> {
        let mut seen = Vec::new();
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            if ck.key().name() != "pharos_subtitle_styling_total" {
                continue;
            }
            let label = |n: &str| {
                ck.key()
                    .labels()
                    .find(|l| l.key() == n)
                    .map(|l| l.value().to_owned())
                    .unwrap()
            };
            let DebugValue::Counter(c) = v else {
                panic!("{v:?}")
            };
            seen.push((label("outcome"), label("stage"), c));
        }
        seen.sort();
        seen
    }

    fn recorder() -> (
        metrics_util::debugging::DebuggingRecorder,
        pharos_test_metrics::CumulativeSnapshotter,
    ) {
        let r = metrics_util::debugging::DebuggingRecorder::new();
        let s = pharos_test_metrics::snapshotter(&r);
        (r, s)
    }

    /// V535 — a styled item stores its plan and is counted once, at `store`;
    /// every line is counted in `pharos_subtitle_effect_total`.
    #[tokio::test]
    async fn a_styled_item_stores_a_plan_and_is_counted_once() {
        let (rec, snap) = recorder();
        let _guard = metrics::set_default_local_recorder(&rec);
        let lib = library(&[(1, "a.mkv", ANIME, true)]).await;
        let fake = FakeSignals::default();
        let sum = styler(&lib, &fake, idle())
            .run_pass(&OnDemand::default())
            .await;
        assert_eq!((sum.pending, sum.styled), (1, 1));

        let row = lib.store.subtitle_styling(1).await.unwrap().unwrap();
        assert_eq!(row.outcome, "styled");
        assert_eq!(row.director_version, DIRECTOR_VERSION);
        assert!(row.sidecar_mtime.is_some());
        let plan: pharos_styling::EffectPlan =
            serde_json::from_str(row.plan_json.as_deref().unwrap()).unwrap();
        assert_eq!(plan.lines.len(), 5);
        assert!(plan.lines.iter().any(|l| l.intent == Intent::Shout));

        assert_eq!(
            totals(&snap),
            [("styled".to_owned(), "store".to_owned(), 1)]
        );
        let mut effects = 0;
        let mut shout_rule = false;
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            if ck.key().name() == "pharos_subtitle_effect_total" {
                let DebugValue::Counter(c) = v else { panic!() };
                effects += c;
                let l = |n: &str| {
                    ck.key()
                        .labels()
                        .find(|l| l.key() == n)
                        .unwrap()
                        .value()
                        .to_owned()
                };
                shout_rule |= l("intent") == "shout" && l("source") == "rule";
            }
        }
        assert_eq!(effects, 5, "plain lines are counted too");
        assert!(shout_rule);

        // B1207: each line's bottom-text verdict is counted, and the line
        // over burned-in text is lifted in the stored plan.
        let mut verdicts = std::collections::BTreeMap::new();
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            if ck.key().name() == "pharos_subtitle_bottom_text_total" {
                let DebugValue::Counter(c) = v else { panic!() };
                let verdict = ck.key().labels().next().unwrap().value().to_owned();
                verdicts.insert(verdict, c);
            }
        }
        assert_eq!(
            verdicts,
            std::collections::BTreeMap::from([("clear".to_owned(), 4), ("lifted".to_owned(), 1)])
        );
        assert_eq!(plan.lines[0].placement, Placement::Top);

        // B1212: with no aligner configured, the item says so.
        let sung: Vec<(String, u64)> = snap
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| ck.key().name() == "pharos_subtitle_sung_total")
            .map(|(ck, _, _, v)| {
                let DebugValue::Counter(c) = v else { panic!() };
                (ck.key().labels().next().unwrap().value().to_owned(), c)
            })
            .collect();
        assert_eq!(sung, [("off".to_owned(), 1)]);
    }

    /// The counter values of `name` by its first label.
    fn by_label(
        snap: &pharos_test_metrics::CumulativeSnapshotter,
        name: &str,
    ) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = snap
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| ck.key().name() == name)
            .map(|(ck, _, _, v)| {
                let DebugValue::Counter(c) = v else { panic!() };
                (ck.key().labels().next().unwrap().value().to_owned(), c)
            })
            .collect();
        out.sort();
        out
    }

    /// B1212: an aligner that cannot be reached is counted as failed, the
    /// item is still styled, and its song lines are paced.
    #[tokio::test]
    async fn an_unreachable_aligner_is_counted_and_the_song_is_paced() {
        let (rec, snap) = recorder();
        let _guard = metrics::set_default_local_recorder(&rec);
        let lib = library(&[(1, "a.mkv", ANIME, true)]).await;
        lib.store
            .set_media_segments(
                1,
                &[pharos_core::DetectedSegment {
                    kind: MediaSegmentKind::Intro,
                    start_ms: 0,
                    end_ms: 60_000,
                    detector: "chapter".into(),
                    confidence: 1.0,
                }],
                1,
            )
            .await
            .unwrap();
        let fake = FakeSignals::default();
        let mut job = styler(&lib, &fake, idle());
        // Port 9 (discard) on loopback: nothing listens there.
        job.aligner = Some(Arc::new(
            crate::services::lyrics::align::Aligner::for_tests(
                "http://127.0.0.1:9".into(),
                lib.dir.path().to_path_buf(),
            ),
        ));
        let sum = job.run_pass(&OnDemand::default()).await;
        assert_eq!((sum.pending, sum.styled), (1, 1));
        assert_eq!(
            by_label(&snap, "pharos_subtitle_sung_total"),
            [("failed".to_owned(), 1)]
        );
        // "Look out!" is a shout, not a plain line, so it takes no fill.
        assert_eq!(
            by_label(&snap, "pharos_subtitle_karaoke_total"),
            [("paced".to_owned(), 4)]
        );
    }

    /// FR-014 — a file the media cannot give us is remembered and not tried
    /// again; a worker that died says nothing about it and is retried.
    #[tokio::test]
    async fn undecodable_is_remembered_and_a_worker_crash_is_retried() {
        let (rec, snap) = recorder();
        let _guard = metrics::set_default_local_recorder(&rec);
        let lib = library(&[(1, "bad.mkv", ANIME, true), (2, "dead.mkv", ANIME, true)]).await;
        let fake = FakeSignals {
            script: HashMap::from([("bad.mkv".into(), "bad"), ("dead.mkv".into(), "dead")]),
            ..FakeSignals::default()
        };
        let job = styler(&lib, &fake, idle());
        let sum = job.run_pass(&OnDemand::default()).await;
        assert_eq!((sum.undecodable, sum.errors), (1, 1));

        let bad = lib.store.subtitle_styling(1).await.unwrap().unwrap();
        assert_eq!((bad.outcome.as_str(), bad.plan_json), ("undecodable", None));
        assert_eq!(bad.director_version, DIRECTOR_VERSION);
        assert!(lib.store.subtitle_styling(2).await.unwrap().is_none());

        job.run_pass(&OnDemand::default()).await;
        assert_eq!(fake.calls(), ["bad.mkv", "dead.mkv", "dead.mkv"]);
        assert_eq!(
            totals(&snap),
            [
                ("error".to_owned(), "signals".to_owned(), 2),
                ("undecodable".to_owned(), "signals".to_owned(), 1),
            ]
        );
        // A failed measure is counted as well as a good one (V1162).
        let mut failed = 0;
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            if ck.key().name() == "pharos_subtitle_signals_total"
                && ck
                    .key()
                    .labels()
                    .any(|l| l.key() == "outcome" && l.value() == "failed")
            {
                let DebugValue::Counter(c) = v else { panic!() };
                failed += c;
            }
        }
        assert_eq!(failed, 3);
    }

    /// A source that parses as nothing is undecodable at the `read` stage.
    #[tokio::test]
    async fn an_unreadable_source_ends_at_the_read_stage() {
        let (rec, snap) = recorder();
        let _guard = metrics::set_default_local_recorder(&rec);
        let lib = library(&[(1, "a.mkv", ANIME, true)]).await;
        std::fs::write(lib.dir.path().join("a.en.srt"), "not a subtitle file").unwrap();
        let fake = FakeSignals::default();
        let sum = styler(&lib, &fake, idle())
            .run_pass(&OnDemand::default())
            .await;
        assert_eq!(sum.undecodable, 1);
        assert!(fake.calls().is_empty(), "no signal pass for a dead source");
        assert_eq!(
            totals(&snap),
            [("undecodable".to_owned(), "read".to_owned(), 1)]
        );
    }

    /// A source whose text cannot be got out at all (here a sidecar path that
    /// is a directory, standing in for an extraction or I/O failure) says
    /// nothing about the track: no row, counted as an error at `read`, and
    /// tried again next pass.
    #[tokio::test]
    async fn a_source_read_failure_is_retried() {
        let (rec, snap) = recorder();
        let _guard = metrics::set_default_local_recorder(&rec);
        let lib = library(&[(1, "a.mkv", ANIME, false)]).await;
        std::fs::create_dir(lib.dir.path().join("a.en.srt")).unwrap();
        let fake = FakeSignals::default();
        let job = styler(&lib, &fake, idle());
        let sum = job.run_pass(&OnDemand::default()).await;
        assert_eq!(sum.undecodable, 0);
        assert!(lib.store.subtitle_styling(1).await.unwrap().is_none());
        assert!(fake.calls().is_empty());
        job.run_pass(&OnDemand::default()).await;
        assert_eq!(totals(&snap), [("error".to_owned(), "read".to_owned(), 2)]);
    }

    /// No usable text track: a version-only row keyed by the sentinel, counted
    /// once at `select`, not looked at again.
    #[tokio::test]
    async fn an_item_with_no_source_track_is_remembered() {
        let (rec, snap) = recorder();
        let _guard = metrics::set_default_local_recorder(&rec);
        let lib = library(&[(1, "a.mkv", ANIME, false)]).await;
        let fake = FakeSignals::default();
        let job = styler(&lib, &fake, idle());
        let sum = job.run_pass(&OnDemand::default()).await;
        assert_eq!(sum.no_source, 1);
        let row = lib.store.subtitle_styling(1).await.unwrap().unwrap();
        assert_eq!(
            (row.source_index, row.outcome.as_str(), row.plan_json),
            (NO_SOURCE_INDEX, "no_source", None)
        );
        let sum = job.run_pass(&OnDemand::default()).await;
        assert_eq!(sum.pending, 0);
        assert!(fake.calls().is_empty());
        assert_eq!(
            totals(&snap),
            [("no_source".to_owned(), "select".to_owned(), 1)]
        );
    }

    /// FR-015 — a title outside the styled genres is not looked at, but its
    /// genre links and its series' genres count.
    #[tokio::test]
    async fn only_styled_genres_are_in_scope() {
        let lib = library(&[
            (1, "drama.mkv", Some("Drama"), true),
            (2, "none.mkv", None, true),
            (3, "toon.mkv", Some("Comedy, ANIMATION"), true),
            (4, "ep.mkv", None, true),
            (5, "linked.mkv", None, true),
        ])
        .await;
        // An episode's NFO genres reach it as genre links, not as its tag.
        lib.store
            .link_item_genres(5, &["Animation".to_owned()])
            .await
            .unwrap();
        let mut ep = lib.store.get(4).await.unwrap();
        ep.kind = MediaKind::Episode;
        ep.series = Some(SeriesInfo {
            series_name: "Show".into(),
            series_folder: Some("/tv/Show".into()),
            ..SeriesInfo::default()
        });
        lib.store.put(ep).await.unwrap();
        lib.store
            .upsert_series_metadata(SeriesMetadata {
                series_key: "/tv/Show".into(),
                series_name: "Show".into(),
                genres: vec!["Anime".into()],
                ..SeriesMetadata::default()
            })
            .await
            .unwrap();

        let fake = FakeSignals::default();
        let sum = styler(&lib, &fake, idle())
            .run_pass(&OnDemand::default())
            .await;
        assert_eq!((sum.skipped, sum.styled), (2, 3));
        let mut calls = fake.calls();
        calls.sort();
        assert_eq!(calls, ["ep.mkv", "linked.mkv", "toon.mkv"]);
    }

    /// An unchanged file is styled once; a changed file, or a new director
    /// version, is styled again.
    #[tokio::test]
    async fn an_item_is_redone_only_when_its_inputs_change() {
        let lib = library(&[(1, "a.mkv", ANIME, true)]).await;
        let fake = FakeSignals::default();
        let job = styler(&lib, &fake, idle());
        job.run_pass(&OnDemand::default()).await;
        job.run_pass(&OnDemand::default()).await;
        assert_eq!(fake.calls().len(), 1, "unchanged: styled once");

        let mut row = lib.store.subtitle_styling(1).await.unwrap().unwrap();
        row.director_version = DIRECTOR_VERSION.wrapping_sub(1);
        lib.store.put_subtitle_styling(row).await.unwrap();
        job.run_pass(&OnDemand::default()).await;
        let row = lib.store.subtitle_styling(1).await.unwrap().unwrap();
        assert_eq!(
            row.director_version, DIRECTOR_VERSION,
            "older director: redone"
        );
        assert_eq!(fake.calls().len(), 1, "from the stored signals (V1162)");

        std::fs::write(lib.dir.path().join("a.mkv"), b"a longer video file").unwrap();
        job.run_pass(&OnDemand::default()).await;
        assert_eq!(fake.calls().len(), 2, "changed file: redone and measured");

        std::fs::write(lib.dir.path().join("a.en.srt"), format!("{SRT}\n")).unwrap();
        job.run_pass(&OnDemand::default()).await;
        let row = lib.store.subtitle_styling(1).await.unwrap().unwrap();
        assert!(row.sidecar_size.is_some_and(|n| n == SRT.len() as u64 + 1));
        assert_eq!(
            fake.calls().len(),
            2,
            "changed sidecar, same windows: stored"
        );

        let retimed = SRT.replace("00:00:09,000", "00:00:08,500");
        std::fs::write(lib.dir.path().join("a.en.srt"), retimed).unwrap();
        job.run_pass(&OnDemand::default()).await;
        assert_eq!(fake.calls().len(), 3, "new line windows: measured");
    }

    /// V1162 — a re-direct from a stored pass is counted `stored`, a fresh
    /// one `measured`, and a stored pass from an older signal pass, or for
    /// another audio stream, is measured again.
    #[tokio::test]
    async fn stored_signals_are_counted_and_refused_when_stale() {
        let (rec, snap) = recorder();
        let _guard = metrics::set_default_local_recorder(&rec);
        let lib = library(&[(1, "a.mkv", ANIME, true)]).await;
        let fake = FakeSignals::default();
        let job = styler(&lib, &fake, idle());
        let redirect = || async {
            let mut row = lib.store.subtitle_styling(1).await.unwrap().unwrap();
            row.director_version = DIRECTOR_VERSION.wrapping_sub(1);
            lib.store.put_subtitle_styling(row).await.unwrap();
            job.run_pass(&OnDemand::default()).await;
        };
        job.run_pass(&OnDemand::default()).await;
        redirect().await;
        assert_eq!(fake.calls().len(), 1);

        let mut stored = lib.store.subtitle_signals(1).await.unwrap().unwrap();
        stored.signals_version = SIGNALS_VERSION.wrapping_sub(1);
        lib.store
            .put_subtitle_signals(stored.clone())
            .await
            .unwrap();
        redirect().await;
        assert_eq!(fake.calls().len(), 2, "older signal pass: measured");

        stored.signals_version = SIGNALS_VERSION;
        stored.audio_stream = Some(7);
        lib.store.put_subtitle_signals(stored).await.unwrap();
        redirect().await;
        assert_eq!(fake.calls().len(), 3, "another audio stream: measured");

        let mut by_source = Vec::new();
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            if ck.key().name() == "pharos_subtitle_signals_total" {
                let DebugValue::Counter(c) = v else { panic!() };
                let l = |n: &str| {
                    ck.key()
                        .labels()
                        .find(|l| l.key() == n)
                        .unwrap()
                        .value()
                        .to_owned()
                };
                assert_eq!(l("outcome"), "ok");
                by_source.push((l("source"), c));
            }
        }
        by_source.sort();
        assert_eq!(
            by_source,
            [("measured".to_owned(), 3), ("stored".to_owned(), 1)]
        );
        let kept = lib.store.subtitle_signals(1).await.unwrap().unwrap();
        let parsed: StoredSignals = serde_json::from_str(&kept.signals_json).unwrap();
        assert_eq!(parsed.windows.len(), 5);
        assert!(
            parsed.signals.keyframes_ms.is_empty(),
            "keyframes are not kept"
        );
    }

    /// V613: after a director bump, an item whose old plan is still being
    /// served is re-styled before items that were never styled.
    #[tokio::test]
    async fn a_served_stale_plan_is_redone_before_new_items() {
        let lib = library(&[(5, "e.mkv", ANIME, true)]).await;
        let fake = FakeSignals::default();
        let job = styler(&lib, &fake, idle());
        job.run_pass(&OnDemand::default()).await;
        let mut row = lib.store.subtitle_styling(5).await.unwrap().unwrap();
        row.director_version = DIRECTOR_VERSION - 1;
        lib.store.put_subtitle_styling(row).await.unwrap();
        // An older signal pass too, so the order shows in the measured calls.
        let mut sig = lib.store.subtitle_signals(5).await.unwrap().unwrap();
        sig.signals_version = SIGNALS_VERSION - 1;
        lib.store.put_subtitle_signals(sig).await.unwrap();
        for (id, name) in [(1, "a.mkv"), (2, "b.mkv")] {
            let path = lib.dir.path().join(name);
            std::fs::write(&path, b"video").unwrap();
            let stem = name.rsplit_once('.').unwrap().0;
            std::fs::write(lib.dir.path().join(format!("{stem}.en.srt")), SRT).unwrap();
            lib.store
                .put(MediaItem {
                    id,
                    path,
                    title: name.into(),
                    kind: MediaKind::Movie,
                    probe: MediaProbe {
                        genre: ANIME.map(str::to_owned),
                        duration_ms: Some(60_000),
                        ..MediaProbe::default()
                    },
                    ..MediaItem::default()
                })
                .await
                .unwrap();
        }
        job.run_pass(&OnDemand::default()).await;
        assert_eq!(fake.calls(), ["e.mkv", "e.mkv", "a.mkv", "b.mkv"]);
    }

    /// FR-015 — the item a viewer opened goes ahead of the backfill.
    #[tokio::test]
    async fn an_on_demand_item_is_styled_before_the_backfill() {
        let lib = library(&[
            (1, "a.mkv", ANIME, true),
            (2, "b.mkv", ANIME, true),
            (3, "c.mkv", ANIME, true),
        ])
        .await;
        let fake = FakeSignals::default();
        let queue = OnDemand::default();
        queue.request(3);
        queue.request(3);
        let sum = styler(&lib, &fake, idle()).run_pass(&queue).await;
        assert_eq!(
            sum.styled, 2,
            "the on-demand item is not counted by the pass"
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], "c.mkv");
        let mut rest = calls[1..].to_vec();
        rest.sort();
        assert_eq!(rest, ["a.mkv", "b.mkv"]);
    }

    /// FR-014 — nothing starts while transcodes are queued.
    #[tokio::test]
    async fn a_busy_transcode_queue_delays_the_item() {
        let lib = library(&[(1, "a.mkv", ANIME, true)]).await;
        let fake = FakeSignals::default();
        let queue = FakeQueue {
            busy: 3,
            looks: AtomicUsize::new(0),
        };
        let job = styler(&lib, &fake, queue);
        let sum = job.run_pass(&OnDemand::default()).await;
        assert_eq!(sum.styled, 1);
        assert_eq!(
            job.queue.looks.load(Ordering::SeqCst),
            4,
            "three busy looks, then the fourth finds it empty"
        );
    }

    #[test]
    fn on_demand_is_a_deduped_capped_fifo() {
        let q = OnDemand::default();
        for id in [5, 6, 5, 7] {
            q.request(id);
        }
        assert_eq!(
            [q.pop(), q.pop(), q.pop(), q.pop()],
            [Some(5), Some(6), Some(7), None]
        );
        for id in 0..(ON_DEMAND_CAP as u64 + 10) {
            q.request(id);
        }
        let mut n = 0;
        while q.pop().is_some() {
            n += 1;
        }
        assert_eq!(n, ON_DEMAND_CAP, "a full queue drops further requests");
    }

    #[test]
    fn genre_gate_ignores_case_and_wants_the_whole_name() {
        let g =
            |s: &[&str]| has_styled_genre(&s.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert!(g(&["Drama", "ANIME"]));
        assert!(g(&[" animation "]));
        assert!(!g(&["Drama", "Documentary"]));
        assert!(!g(&["Animated Shorts"]));
        assert!(!g(&[]));
    }

    #[test]
    fn sidecar_names_give_language_and_flags() {
        let c = |n: &str| sidecar_candidate(100, std::path::Path::new(n));
        let a = c("/m/Show.en.sdh.ass");
        assert_eq!(
            (
                a.language.as_deref(),
                a.codec.as_deref(),
                a.hearing_impaired
            ),
            (Some("en"), Some("ass"), true)
        );
        let b = c("/m/Show.eng.forced.srt");
        assert!(b.forced);
        assert_eq!(b.codec.as_deref(), Some("subrip"));
        assert_eq!(c("/m/Show.srt").language, None);
        assert_eq!(c("/m/Show.vtt").codec.as_deref(), Some("webvtt"));
    }

    #[test]
    fn outcome_and_stage_labels_are_distinct() {
        let outcomes: HashSet<_> = [
            Outcome::Styled,
            Outcome::NoSource,
            Outcome::Undecodable,
            Outcome::Error,
            Outcome::Missing,
            Outcome::Unscanned,
        ]
        .iter()
        .map(|o| o.label())
        .collect();
        assert_eq!(outcomes.len(), 6);
        let stages: HashSet<_> = [
            Stage::Select,
            Stage::Read,
            Stage::Signals,
            Stage::Direct,
            Stage::Store,
        ]
        .iter()
        .map(|s| s.label())
        .collect();
        assert_eq!(stages.len(), 5);
    }
}
