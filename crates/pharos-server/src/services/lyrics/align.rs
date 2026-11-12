//! Forced alignment client and worker for lyric word times (ADR-0066).
//!
//! When `[lyrics].aligner_url` is configured, karaoke requests queue a job
//! to compute word-level timestamps from audio and synced lyric text. Jobs
//! are de-duplicated and processed one at a time by a background task. Answers
//! are cached on disk or in memory.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pharos_core::MediaItem;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::Instrument as _;

use super::eta::Eta;
use super::shadow::{Shadow, Source};
use super::{Lyrics, WordStamp};

/// Bumped when the aligner's answers change for the same song and lines, so
/// word times cached from an older aligner are asked for again.
/// 2: star tokens stop words smearing across instrumental breaks.
/// 3: lyrics timed for another cut are shifted onto the audio first (B936).
/// 4: answers an older aligner gave under version 3, cached with no offset,
///    are asked for again (B1007).
/// 5: a line's window opens 750 ms before its stamp, not 250 ms, so a stamp
///    tapped late no longer pins its first word late.
const ALIGN_VERSION: u32 = 5;

/// The oldest aligner whose answers are kept. Its `aligner_version` goes up
/// with each change to how it times words, so an answer from an aligner
/// deployed after the server fails the job (retried after `RETRY_AFTER`)
/// instead of being cached under the new `ALIGN_VERSION` key.
/// 2: a line's window opens 750 ms before its stamp (B1165).
const MIN_ALIGNER_VERSION: u32 = 2;

/// Time alignment of a whole song can take on CPU; a worker may run up to this
/// long before timing out.
const TIMEOUT: Duration = Duration::from_secs(600);

/// How long the styling job waits for an item's sung lines: an opening and
/// an ending are a few minutes of audio, tens of seconds of CPU (ADR-0066),
/// and a stalled aligner must not hold the styling queue for [`TIMEOUT`].
const SING_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a song whose alignment failed is left alone, so a file the
/// aligner cannot read is not sent again on every poll.
const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// The most lines one request may carry; the aligner refuses more.
const MAX_LINES: usize = 500;

/// One alignment job sent to the background task.
#[derive(Debug)]
struct Job {
    id: u64,
    path: String,
    lines: Vec<(u64, u64, String)>,
    key: String,
    /// The song's length, which the time estimate is per second of.
    duration_ms: u64,
    queued_at: Instant,
    /// The estimate given when the job was queued, if there was one.
    predicted_ms: Option<u64>,
}

/// How far a job's queue-to-done time was from the estimate given when it was
/// queued, in seconds, labelled `direction` (`late` or `early`, since a
/// histogram cannot hold a negative) and `outcome` (`aligned` or `failed`).
const ETA_ERROR_SECONDS: &str = "pharos_lyrics_align_eta_error_seconds";

/// How long a karaoke job waited between queueing and being sent to the
/// aligner, so other users of the aligner can be shown not to delay karaoke
/// (spec 040 FR-022).
const WAIT_SECONDS: &str = "pharos_lyrics_align_wait_seconds";

impl Job {
    /// The estimate given at queueing and the time actually taken, for the
    /// log line; the error between them goes to [`ETA_ERROR_SECONDS`] when
    /// there was an estimate.
    fn observe_eta(&self, outcome: &'static str) -> (Option<u64>, u64) {
        let actual_ms = u64::try_from(self.queued_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(predicted_ms) = self.predicted_ms {
            let direction = if actual_ms > predicted_ms {
                "late"
            } else {
                "early"
            };
            metrics::histogram!(ETA_ERROR_SECONDS, "direction" => direction, "outcome" => outcome)
                .record(actual_ms.abs_diff(predicted_ms) as f64 / 1000.0);
        }
        (self.predicted_ms, actual_ms)
    }
}

fn clamp_u32(ms: u64) -> u32 {
    u32::try_from(ms).unwrap_or(u32::MAX)
}

/// The aligner's answer for a song: word stamps per line, and how far it
/// found the song's lyric stamps to be off from the audio (B936).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Aligned {
    pub lines: Vec<Vec<WordStamp>>,
    /// Added to every line's start to land it on the singing; positive when
    /// the lyrics were early.
    pub offset_ms: i64,
    /// Per line, how well its words matched the singing in thousandths of
    /// the mean character probability (aligner 4); empty from an older one.
    pub scores: Vec<Option<u16>>,
}

impl Aligned {
    /// Move a line start by the song's offset, keeping it at or after zero.
    pub fn shift(&self, start_ms: u64) -> u64 {
        start_ms.saturating_add_signed(self.offset_ms)
    }
}

/// Outcome of looking up aligned word stamps for a song.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Word timestamps are ready for every line.
    Ready(Aligned),
    /// Alignment is queued or currently in flight, with the milliseconds
    /// the server expects it to take from now, when it can tell.
    Pending(Option<u32>),
    /// In flight with some lines already timed (ADR-0121): those lines'
    /// words and the song's offset, an empty line for each one still to
    /// come, and the estimate as for [`Lookup::Pending`].
    Partial(Option<u32>, Aligned),
    /// Alignment cannot be performed or the queue is full.
    Unavailable,
}

/// Disk or in-memory cache of aligned word timestamps.
#[derive(Clone)]
pub struct AlignCache {
    backend: Arc<CacheBackend>,
}

/// Word stamps per line, keyed by item id, with the key they were aligned for.
type Remembered = HashMap<u64, (String, Aligned)>;

enum CacheBackend {
    Disk(PathBuf),
    Memory(Mutex<Remembered>),
}

#[derive(Serialize, Deserialize)]
struct WordsRecord {
    key: String,
    lines: Vec<Vec<WordEntry>>,
    #[serde(default)]
    offset_ms: i64,
    #[serde(default)]
    scores: Vec<Option<u16>>,
}

#[derive(Serialize, Deserialize)]
struct WordEntry {
    at: u32,
    start_ms: u64,
}

impl AlignCache {
    /// Create a cache rooted at `dir`, or an in-memory cache when `None`.
    pub fn new(dir: Option<PathBuf>) -> Self {
        let backend = match dir {
            Some(d) => CacheBackend::Disk(d),
            None => CacheBackend::Memory(Mutex::new(HashMap::new())),
        };
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Retrieve cached word stamps for `id` matching `key`, if present.
    pub fn get(&self, id: u64, key: &str) -> Option<Aligned> {
        match &*self.backend {
            CacheBackend::Disk(dir) => {
                let path = dir.join(format!("{id}.words.json"));
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "lyrics: words cache read failed");
                        return None;
                    }
                };
                let record: WordsRecord = match serde_json::from_slice(&bytes) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "lyrics: words cache entry unreadable");
                        return None;
                    }
                };
                if record.key != key {
                    return None;
                }
                Some(Aligned {
                    lines: record
                        .lines
                        .into_iter()
                        .map(|line| {
                            line.into_iter()
                                .map(|w| WordStamp {
                                    at: w.at,
                                    start_ms: w.start_ms,
                                })
                                .collect()
                        })
                        .collect(),
                    offset_ms: record.offset_ms,
                    scores: record.scores,
                })
            }
            CacheBackend::Memory(mem) => {
                let map = match mem.lock() {
                    Ok(m) => m,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let (entry_key, lines) = map.get(&id)?;
                if entry_key != key {
                    return None;
                }
                Some(lines.clone())
            }
        }
    }

    /// Persist word stamps for `id` under `key`.
    pub fn put(&self, id: u64, key: String, aligned: Aligned) {
        match &*self.backend {
            CacheBackend::Disk(dir) => {
                let path = dir.join(format!("{id}.words.json"));
                let record = WordsRecord {
                    key,
                    offset_ms: aligned.offset_ms,
                    scores: aligned.scores.clone(),
                    lines: aligned
                        .lines
                        .iter()
                        .map(|line| {
                            line.iter()
                                .map(|w| WordEntry {
                                    at: w.at,
                                    start_ms: w.start_ms,
                                })
                                .collect()
                        })
                        .collect(),
                };
                if let Err(e) = write_record(dir, &path, &record) {
                    tracing::warn!(media.id = id, dir = %dir.display(), error = %e, "lyrics: words cache write failed");
                }
            }
            CacheBackend::Memory(mem) => {
                let mut map = match mem.lock() {
                    Ok(m) => m,
                    Err(poisoned) => poisoned.into_inner(),
                };
                map.insert(id, (key, aligned));
            }
        }
    }
}

/// Written via a temporary file and atomically renamed over the destination.
fn write_record(dir: &Path, path: &Path, record: &WordsRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, &bytes)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// The lines a streaming aligner has timed so far for a song in flight
/// (ADR-0121); `None` for a line not timed yet.
#[derive(Debug, Default)]
struct Partial {
    /// The job's cache key: lines timed for other lyrics are not shown.
    key: String,
    offset_ms: i64,
    lines: Vec<Option<Vec<WordStamp>>>,
    /// When its first line came, for the job's log line.
    first_line_at: Option<Instant>,
}

/// The streamed `/align` answer's type (ADR-0121).
const NDJSON: &str = "application/x-ndjson";

/// The longest streamed event read. The last one carries the whole answer,
/// at most `MAX_LINES` lines of words, far under this; an aligner that sends
/// more without a newline fails the job instead of growing the buffer.
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// One event of a streamed `/align` answer. `done` carries the whole answer,
/// read by [`parse_answer`]; an event this server does not know is skipped.
#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Offset {
        offset_ms: i64,
        lines: usize,
    },
    Line {
        index: usize,
        words: Vec<ReplyWord>,
    },
    Done,
    Error {
        status: u16,
        error: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ReplyWord {
    at: u32,
    start_ms: u64,
}

impl From<ReplyWord> for WordStamp {
    fn from(w: ReplyWord) -> Self {
        WordStamp {
            at: w.at,
            start_ms: w.start_ms,
        }
    }
}

/// An aligner's answer, with the request that got it and its traceparent,
/// so the shadow can be sent the very same request.
struct Answered {
    aligned: Aligned,
    sent: Vec<u8>,
    traceparent: Option<String>,
}

/// Client and background worker for lyric forced alignment.
pub struct Aligner {
    http: Client,
    base: String,
    media_root: PathBuf,
    cache: AlignCache,
    pending: Mutex<HashSet<u64>>,
    /// What a queued song's wait is guessed from.
    eta: Mutex<Eta>,
    /// Zero for the `now_ms` the estimator is given.
    epoch: Instant,
    /// Songs whose last alignment failed, and when to try them again.
    failed: Mutex<HashMap<u64, std::time::Instant>>,
    /// Lines timed so far for the song in flight, by item id (ADR-0121).
    partial: Mutex<HashMap<u64, Partial>>,
    tx: mpsc::Sender<Job>,
    /// Spec 039, R21: sent each stored song for a comparison, when set.
    shadow: Option<Shadow>,
}

impl Aligner {
    /// An aligner at `base` for files under `media_root`, with no worker
    /// behind its queue; for other modules' tests.
    #[cfg(test)]
    pub(crate) fn for_tests(base: String, media_root: PathBuf) -> Aligner {
        Aligner {
            http: crate::http_client::client(),
            base,
            media_root,
            cache: AlignCache::new(None),
            pending: Mutex::new(HashSet::new()),
            eta: Mutex::default(),
            epoch: Instant::now(),
            failed: Mutex::default(),
            partial: Mutex::default(),
            tx: mpsc::channel(1).0,
            shadow: None,
        }
    }

    /// Start the aligner background worker and return a shared handle.
    /// `shadow_url`, when set, gets each song again once its answer is
    /// stored (spec 039, R21).
    pub fn start(
        url: String,
        shadow_url: Option<String>,
        media_root: PathBuf,
        cache_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        let ua = format!(
            "pharos/{} (https://github.com/alisonjenkins/pharos)",
            env!("CARGO_PKG_VERSION")
        );
        let http = crate::http_client::builder()
            .user_agent(ua)
            .timeout(TIMEOUT)
            .build()
            .unwrap_or_else(|_| crate::http_client::client());

        let (tx, mut rx) = mpsc::channel(64);
        let cache = AlignCache::new(cache_dir);
        let base = url.trim_end_matches('/').to_owned();
        let shadow = shadow_url.map(|u| {
            super::shadow::register_series();
            Shadow::new(http.clone(), &u)
        });
        let aligner = Arc::new(Self {
            shadow,
            http,
            base,
            media_root,
            cache,
            pending: Mutex::new(HashSet::new()),
            eta: Mutex::default(),
            epoch: Instant::now(),
            failed: Mutex::default(),
            partial: Mutex::default(),
            tx,
        });

        let worker = Arc::clone(&aligner);
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                // The aligner joins this span's trace through `traceparent`.
                let span =
                    tracing::info_span!("lyrics.align", media.id = job.id, lines = job.lines.len());
                worker.eta_lock().start(job.id, worker.now_ms());
                let outcome = worker.process_job(&job).instrument(span).await;
                match &outcome {
                    Ok(()) => worker
                        .eta_lock()
                        .finish(job.id, worker.now_ms(), job.duration_ms),
                    Err(_) => worker.eta_lock().abort(job.id),
                }
                if let Err(e) = outcome {
                    worker.job_failed(&job, &e);
                }
                // Stored or failed, the song's partial lines are done with.
                worker.partial_lock().remove(&job.id);
                match worker.pending.lock() {
                    Ok(mut p) => {
                        p.remove(&job.id);
                    }
                    Err(p) => {
                        p.into_inner().remove(&job.id);
                    }
                }
            }
        });

        aligner
    }

    fn partial_lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Partial>> {
        self.partial.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// How many lines were timed before the answer, and when the first came
    /// after queueing, for the job's log line.
    fn streamed(&self, job: &Job) -> (usize, Option<u64>) {
        let partial = self.partial_lock();
        let Some(p) = partial.get(&job.id) else {
            return (0, None);
        };
        let first_ms = p.first_line_at.map(|at| {
            u64::try_from(at.saturating_duration_since(job.queued_at).as_millis())
                .unwrap_or(u64::MAX)
        });
        (p.lines.iter().flatten().count(), first_ms)
    }

    /// Applies one event of a streamed answer for item `id`: an offset or a
    /// line goes to the partial answer, `done` is the whole answer, and
    /// `error` the status the song failed with.
    fn event(
        &self,
        (id, key): (u64, &str),
        line: &[u8],
        n_lines: usize,
    ) -> Result<Option<Aligned>, String> {
        let text = std::str::from_utf8(line).map_err(|e| format!("aligner event: {e}"))?;
        let event: Event =
            serde_json::from_str(text).map_err(|e| format!("invalid aligner event: {e}"))?;
        match event {
            Event::Offset { offset_ms, lines } => {
                // Lines for another count are not shown; `done` fails the job.
                if lines == n_lines {
                    self.partial_lock().insert(
                        id,
                        Partial {
                            key: key.to_owned(),
                            offset_ms,
                            lines: std::iter::repeat_with(|| None).take(n_lines).collect(),
                            first_line_at: None,
                        },
                    );
                }
            }
            Event::Line { index, words } => {
                if let Some(p) = self.partial_lock().get_mut(&id) {
                    if let Some(slot) = p.lines.get_mut(index) {
                        *slot = Some(words.into_iter().map(WordStamp::from).collect());
                        p.first_line_at.get_or_insert_with(Instant::now);
                    }
                }
            }
            Event::Done => return parse_answer(text, n_lines).map(Some),
            Event::Error { status, error } => return Err(format!("status {status}: {error}")),
            Event::Other => {}
        }
        Ok(None)
    }

    /// The lines timed so far for item `id` under cache `key`, as an answer
    /// with an empty line for each still to come; `None` before a line is
    /// timed, or when the job in flight was for other lyrics.
    fn partial_for(&self, id: u64, key: &str) -> Option<Aligned> {
        let partial = self.partial_lock();
        let p = partial.get(&id).filter(|p| p.key == key)?;
        p.lines.iter().any(Option::is_some).then(|| Aligned {
            lines: p
                .lines
                .iter()
                .map(|l| l.clone().unwrap_or_default())
                .collect(),
            offset_ms: p.offset_ms,
            scores: Vec::new(),
        })
    }

    fn eta_lock(&self) -> std::sync::MutexGuard<'_, Eta> {
        self.eta.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// A song job failed: log it with its estimate beside the time taken,
    /// as the success path does, and hold it off until the retry time.
    fn job_failed(&self, job: &Job, error: &str) {
        let (predicted_ms, actual_ms) = job.observe_eta("failed");
        let (lines_streamed, first_line_ms) = self.streamed(job);
        tracing::warn!(
            media.id = job.id,
            error = %error,
            eta.predicted_ms = predicted_ms,
            eta.actual_ms = actual_ms,
            lines_streamed,
            first_line_ms,
            "lyrics: alignment failed"
        );
        let retry = std::time::Instant::now() + RETRY_AFTER;
        match self.failed.lock() {
            Ok(mut f) => f.insert(job.id, retry),
            Err(f) => f.into_inner().insert(job.id, retry),
        };
    }

    async fn process_job(&self, job: &Job) -> Result<(), String> {
        metrics::histogram!(WAIT_SECONDS).record(job.queued_at.elapsed().as_secs_f64());
        let answered = self
            .request(&job.path, &job.lines, TIMEOUT, Some((job.id, &job.key)))
            .await?;
        let offset_ms = answered.aligned.offset_ms;
        self.store(job.id, job.key.clone(), answered, Source::Song);
        // Measured once the answer is stored, the point the client can see
        // it and the estimator's own finish.
        let (predicted_ms, actual_ms) = job.observe_eta("aligned");
        // 0 lines streamed: an aligner from before ADR-0121 answered whole.
        let (lines_streamed, first_line_ms) = self.streamed(job);
        tracing::info!(
            media.id = job.id,
            lines = job.lines.len(),
            offset_ms,
            eta.predicted_ms = predicted_ms,
            eta.actual_ms = actual_ms,
            lines_streamed,
            first_line_ms,
            "lyrics: aligned"
        );
        Ok(())
    }

    /// Caches the primary's answer, then hands the same request to the
    /// shadow when one is set (spec 039, R21). Stored first: the shadow runs
    /// on its own task and never holds up, changes or replaces the answer.
    fn store(&self, id: u64, key: String, answered: Answered, source: Source) -> Aligned {
        let Answered {
            aligned,
            sent,
            traceparent,
        } = answered;
        self.cache.put(id, key, aligned.clone());
        if let Some(shadow) = &self.shadow {
            drop(shadow.spawn(id, source, sent, aligned.clone(), traceparent));
        }
        aligned
    }

    /// One `POST /align` for `lines` of the media-relative `path`. With
    /// `stream`, an item id, the answer is asked for line by line and each
    /// line is kept for that item as it comes (ADR-0121); an aligner that
    /// answers whole is read as before.
    async fn request(
        &self,
        path: &str,
        lines: &[(u64, u64, String)],
        timeout: Duration,
        stream: Option<(u64, &str)>,
    ) -> Result<Answered, String> {
        #[derive(Serialize)]
        struct AlignRequest<'a> {
            path: &'a str,
            lines: Vec<AlignRequestLine<'a>>,
        }

        #[derive(Serialize)]
        struct AlignRequestLine<'a> {
            start_ms: u64,
            end_ms: u64,
            text: &'a str,
        }

        let req_lines: Vec<AlignRequestLine<'_>> = lines
            .iter()
            .map(|(start, end, text)| AlignRequestLine {
                start_ms: *start,
                end_ms: *end,
                text,
            })
            .collect();

        let payload = AlignRequest {
            path,
            lines: req_lines,
        };

        // Bytes, not `.json()`: the shadow is sent the very same request.
        let sent = serde_json::to_vec(&payload).map_err(|e| format!("request body: {e}"))?;
        let url = format!("{}/align", self.base);
        let traceparent = crate::obs::traceparent(&tracing::Span::current());
        let mut request = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(sent.clone())
            .timeout(timeout);
        if let Some(traceparent) = &traceparent {
            request = request.header("traceparent", traceparent.clone());
        }
        if stream.is_some() {
            request = request.header(
                reqwest::header::ACCEPT,
                format!("{NDJSON}, application/json"),
            );
        }
        let mut resp = request.send().await.map_err(|e| format!("request: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("status {status}"));
        }
        let streamed = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with(NDJSON));
        let aligned = match stream.filter(|_| streamed) {
            Some(job) => {
                let too_long = || format!("aligner event over {MAX_EVENT_BYTES} bytes");
                let mut buf = Vec::new();
                let mut done = None;
                while let Some(chunk) = resp.chunk().await.map_err(|e| format!("body: {e}"))? {
                    buf.extend_from_slice(&chunk);
                    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                        if nl > MAX_EVENT_BYTES {
                            return Err(too_long());
                        }
                        let event: Vec<u8> = buf.drain(..=nl).collect();
                        if let Some(answer) = self.event(job, &event, lines.len())? {
                            done = Some(answer);
                        }
                    }
                    if buf.len() > MAX_EVENT_BYTES {
                        return Err(too_long());
                    }
                }
                done.ok_or_else(|| "aligner stream ended before its answer".to_owned())?
            }
            None => {
                let body = resp.text().await.map_err(|e| format!("body: {e}"))?;
                parse_answer(&body, lines.len())?
            }
        };
        Ok(Answered {
            aligned,
            sent,
            traceparent,
        })
    }

    /// Word stamps for sung subtitle lines of a video, `(start, end, text)`
    /// each (B1212): from the cache, else asked for now and cached. Unlike
    /// [`Self::words_for`] this waits for the answer, for a background job
    /// that has nothing to show until it has one.
    pub async fn sing(
        &self,
        item: &MediaItem,
        lines: &[(u64, u64, String)],
    ) -> Result<Aligned, String> {
        if lines.is_empty() || lines.len() > MAX_LINES {
            return Err(format!(
                "{} lines; the aligner takes 1 to {MAX_LINES}",
                lines.len()
            ));
        }
        let rel = item
            .path
            .strip_prefix(&self.media_root)
            .ok()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!(
                    "{} is outside the aligner's media root",
                    item.path.display()
                )
            })?;
        let key = cache_key(&["subtitle"], rel, &item.path, lines);
        if let Some(hit) = self.cache.get(item.id, &key) {
            // No score at all is an aligner from before scores: ask again, so
            // a scoring aligner deployed since is heard.
            if hit.lines.len() == lines.len() && hit.scores.iter().any(Option::is_some) {
                return Ok(hit);
            }
        }
        let answered = self.request(rel, lines, SING_TIMEOUT, None).await?;
        Ok(self.store(item.id, key, answered, Source::Lines))
    }

    /// Check if word timestamps are ready for `lyrics`, or queue a background job.
    pub fn words_for(&self, item: &MediaItem, lyrics: &Lyrics) -> Lookup {
        if !lyrics.synced() {
            return Lookup::Unavailable;
        }
        if lyrics.lines.iter().all(|l| !l.words.is_empty()) || lyrics.lines.len() > MAX_LINES {
            return Lookup::Unavailable;
        }
        let rel_path = match item.path.strip_prefix(&self.media_root) {
            Ok(p) => match p.to_str() {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Lookup::Unavailable,
            },
            Err(_) => return Lookup::Unavailable,
        };

        let mut job_lines = Vec::with_capacity(lyrics.lines.len());
        for (i, line) in lyrics.lines.iter().enumerate() {
            let start_ms = line.start_ms.unwrap_or(0);
            let end_ms = if let Some(next) = lyrics.lines.get(i.saturating_add(1)) {
                next.start_ms
                    .unwrap_or_else(|| start_ms.saturating_add(5_000))
            } else {
                start_ms.saturating_add(5_000)
            };
            let end_ms = end_ms.max(start_ms);
            job_lines.push((start_ms, end_ms, line.text.clone()));
        }

        let key = cache_key(&[], &rel_path, &item.path, &job_lines);

        if let Some(aligned) = self.cache.get(item.id, &key) {
            if aligned.lines.len() == job_lines.len() {
                return Lookup::Ready(aligned);
            }
        }

        let mut pending = match self.pending.lock() {
            Ok(p) => p,
            Err(poisoned) => poisoned.into_inner(),
        };

        if pending.contains(&item.id) {
            let left = self
                .eta_lock()
                .eta_ms(item.id, self.now_ms())
                .map(clamp_u32);
            return match self.partial_for(item.id, &key) {
                Some(partial) => Lookup::Partial(left, partial),
                None => Lookup::Pending(left),
            };
        }
        {
            let mut failed = match self.failed.lock() {
                Ok(f) => f,
                Err(poisoned) => poisoned.into_inner(),
            };
            match failed.get(&item.id) {
                Some(&retry) if std::time::Instant::now() < retry => return Lookup::Unavailable,
                Some(_) => {
                    failed.remove(&item.id);
                }
                None => {}
            }
        }

        pending.insert(item.id);
        let duration_ms = item.probe.duration_ms.unwrap_or(0);
        let predicted_ms = {
            let mut eta = self.eta_lock();
            eta.enqueue(item.id, duration_ms);
            eta.eta_ms(item.id, self.now_ms())
        };
        let job = Job {
            id: item.id,
            path: rel_path,
            lines: job_lines,
            key,
            duration_ms,
            queued_at: Instant::now(),
            predicted_ms,
        };

        match self.tx.try_send(job) {
            Ok(()) => Lookup::Pending(predicted_ms.map(clamp_u32)),
            Err(_) => {
                pending.remove(&item.id);
                self.eta_lock().forget(item.id);
                Lookup::Unavailable
            }
        }
    }
}

/// The cache key for `lines` of the file at `path` (media-relative `rel`):
/// the file's size and mtime are in it, so a replaced file aligns again.
/// `tag` keeps one kind of request's answers apart from another's.
fn cache_key(tag: &[&str], rel: &str, path: &Path, lines: &[(u64, u64, String)]) -> String {
    let (size, mtime) = match std::fs::metadata(path) {
        Ok(m) => {
            let size = m.len();
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            (size, mtime)
        }
        Err(_) => (0, 0),
    };
    let mut parts = vec![serde_json::json!(ALIGN_VERSION)];
    parts.extend(tag.iter().map(|t| serde_json::json!(t)));
    parts.extend([
        serde_json::json!(rel),
        serde_json::json!(size),
        serde_json::json!(mtime),
        serde_json::json!(lines),
    ]);
    let json_bytes = serde_json::to_vec(&parts).unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(&json_bytes);
    let hash = hasher.finalize();
    let mut key = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(key, "{byte:02x}");
    }
    key
}

/// Parse the aligner service's JSON response, rejecting line count mismatches.
/// The error names what was wrong, e.g. serde's "missing field `offset_ms`"
/// from an aligner older than the offset search.
pub(super) fn parse_answer(body: &str, n_lines: usize) -> Result<Aligned, String> {
    #[derive(Deserialize)]
    struct Reply {
        lines: Vec<ReplyLine>,
        /// Required: an aligner older than the offset search sends none, and
        /// caching its answer would pin the song's words to a guess of no
        /// offset until ALIGN_VERSION next changes (B1007). Rejecting it
        /// fails the job, which is retried after `RETRY_AFTER`.
        offset_ms: i64,
        /// Required for the same reason, from an aligner older than B1165.
        aligner_version: u32,
    }

    #[derive(Deserialize)]
    struct ReplyLine {
        words: Vec<ReplyWord>,
        /// From aligner 4 on; `null` for a line with no words.
        #[serde(default)]
        score: Option<f32>,
    }

    let reply: Reply =
        serde_json::from_str(body).map_err(|e| format!("invalid aligner response: {e}"))?;
    if reply.aligner_version < MIN_ALIGNER_VERSION {
        return Err(format!(
            "aligner_version {} is older than {MIN_ALIGNER_VERSION}",
            reply.aligner_version
        ));
    }
    if reply.lines.len() != n_lines {
        return Err(format!(
            "invalid aligner response: {} lines for {n_lines} asked",
            reply.lines.len()
        ));
    }

    let scores = reply
        .lines
        .iter()
        .map(|l| {
            l.score
                .filter(|s| s.is_finite())
                .map(|s| (s.clamp(0.0, 1.0) * 1000.0).round() as u16)
        })
        .collect();
    Ok(Aligned {
        scores,
        lines: reply
            .lines
            .into_iter()
            .map(|l| l.words.into_iter().map(WordStamp::from).collect())
            .collect(),
        offset_ms: reply.offset_ms,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::services::lyrics::{LyricLine, Origin};

    fn test_aligner(media_root: PathBuf, cache: AlignCache, tx: mpsc::Sender<Job>) -> Aligner {
        Aligner {
            http: crate::http_client::client(),
            base: "http://127.0.0.1:8790".into(),
            media_root,
            cache,
            pending: Mutex::new(HashSet::new()),
            eta: Mutex::default(),
            epoch: Instant::now(),
            failed: Mutex::default(),
            partial: Mutex::default(),
            tx,
            shadow: None,
        }
    }

    fn sample_item(path: PathBuf) -> MediaItem {
        MediaItem {
            id: 42,
            path,
            kind: pharos_core::MediaKind::Audio,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn the_primary_answer_is_stored_before_the_shadow_answers_and_never_replaced() {
        use crate::services::lyrics::shadow::tests::{body, by_path, fake};
        use crate::services::lyrics::shadow::SHADOW_VERSION;
        use metrics_util::debugging::DebuggingRecorder;
        use tokio::sync::oneshot;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let stamp = |at, start_ms| WordStamp { at, start_ms };
        let primary = Aligned {
            lines: vec![vec![stamp(0, 1000), stamp(3, 1400)]],
            offset_ms: 0,
            scores: vec![None],
        };
        let differing = Aligned {
            lines: vec![vec![stamp(0, 1000), stamp(3, 1600)]],
            offset_ms: 0,
            scores: vec![None],
        };
        let (primary_base, primary_got) = fake(200, body(&primary, 2), None).await;
        let (release, hold) = oneshot::channel();
        let (shadow_base, shadow_got) =
            fake(200, body(&differing, SHADOW_VERSION), Some(hold)).await;
        let cache = AlignCache::new(None);
        let (tx, _rx) = mpsc::channel(1);
        let aligner = Aligner {
            base: primary_base,
            shadow: Some(Shadow::new(crate::http_client::client(), &shadow_base)),
            ..test_aligner(PathBuf::from("/m"), cache.clone(), tx)
        };
        let job = Job {
            id: 42,
            path: "a/song.flac".into(),
            lines: vec![(1000, 2000, "two words".into())],
            key: "k".into(),
            duration_ms: 0,
            queued_at: Instant::now(),
            predicted_ms: None,
        };

        // The shadow holds its answer back, and the job still finishes.
        tokio::time::timeout(Duration::from_secs(5), aligner.process_job(&job))
            .await
            .expect("the job waits for no shadow")
            .unwrap();
        assert_eq!(cache.get(42, "k"), Some(primary.clone()));
        assert_eq!(
            shadow_got.await.unwrap(),
            primary_got.await.unwrap(),
            "the shadow is sent the primary's request"
        );

        release.send(()).unwrap();
        let seen = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let seen = by_path(&snapshotter);
                if !seen.is_empty() {
                    break seen;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            cache.get(42, "k"),
            Some(primary),
            "the mismatch replaced nothing"
        );
        assert_eq!(seen, vec![("song".into(), "mismatch".into(), 1)]);
    }

    fn shadow_series(body: &str) -> Vec<&str> {
        use crate::services::lyrics::shadow::SHADOW_TOTAL;
        body.lines()
            .filter(|l| l.starts_with(&format!("{SHADOW_TOTAL}{{")))
            .collect()
    }

    /// `/metrics` from a recorder of its own once `Aligner::start` has run
    /// with `shadow_url`, never the process-global one, so tests sharing a
    /// process cannot see each other's series. `start` registers on the
    /// calling thread, which the local recorder covers.
    fn rendered_after_start(shadow_url: Option<String>) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _aligner = metrics::with_local_recorder(&recorder, || {
            Aligner::start(
                "http://127.0.0.1:9".into(),
                shadow_url,
                PathBuf::from("/m"),
                None,
            )
        });
        handle.render()
    }

    #[tokio::test]
    async fn every_shadow_series_is_rendered_at_zero_once_a_shadow_is_set() {
        use crate::services::lyrics::shadow::{Outcome, SHADOW_TOTAL};

        let body = rendered_after_start(Some("http://127.0.0.1:9".into()));
        let series = shadow_series(&body);
        assert_eq!(
            series.len(),
            Outcome::ALL.len() * Source::ALL.len(),
            "{series:?}"
        );
        for source in Source::ALL {
            for outcome in Outcome::ALL {
                let line = format!(
                    "{SHADOW_TOTAL}{{outcome=\"{}\",path=\"{}\"}} 0",
                    outcome.label(),
                    source.label()
                );
                assert!(series.contains(&line.as_str()), "{line} not in {series:?}");
            }
        }
    }

    #[tokio::test]
    async fn no_shadow_series_is_rendered_without_a_shadow() {
        let body = rendered_after_start(None);
        assert_eq!(shadow_series(&body), Vec::<&str>::new());
    }

    #[tokio::test]
    async fn sung_subtitle_lines_are_shadowed_too_after_the_answer_is_stored() {
        use crate::services::lyrics::shadow::tests::{body, by_path, fake};
        use metrics_util::debugging::DebuggingRecorder;
        use tokio::sync::oneshot;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let stamp = |at, start_ms| WordStamp { at, start_ms };
        let primary = Aligned {
            lines: vec![vec![stamp(0, 1200), stamp(4, 1900)]],
            offset_ms: 0,
            scores: vec![Some(610)],
        };
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().join("media");
        std::fs::create_dir_all(&media_root).unwrap();
        let file = media_root.join("episode.mkv");
        std::fs::write(&file, b"x").unwrap();
        let (primary_base, primary_got) = fake(200, body(&primary, 4), None).await;
        let (release, hold) = oneshot::channel();
        let (shadow_base, shadow_got) = fake(200, body(&primary, 5), Some(hold)).await;
        let cache = AlignCache::new(None);
        let (tx, _rx) = mpsc::channel(1);
        let aligner = Aligner {
            base: primary_base,
            shadow: Some(Shadow::new(crate::http_client::client(), &shadow_base)),
            ..test_aligner(media_root, cache.clone(), tx)
        };
        let item = MediaItem {
            kind: pharos_core::MediaKind::Episode,
            ..sample_item(file)
        };
        let lines = vec![(1000, 2500, "Run away".to_owned())];

        // The shadow holds its answer back, and sing still answers.
        let sung = tokio::time::timeout(Duration::from_secs(5), aligner.sing(&item, &lines))
            .await
            .expect("sing waits for no shadow")
            .unwrap();
        assert_eq!(sung, primary);
        assert_eq!(
            shadow_got.await.unwrap(),
            primary_got.await.unwrap(),
            "the shadow is sent the primary's request"
        );

        release.send(()).unwrap();
        let seen = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let seen = by_path(&snapshotter);
                if !seen.is_empty() {
                    break seen;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(seen, vec![("lines".into(), "match".into(), 1)]);
    }

    #[test]
    fn words_for_returns_unavailable_for_plain_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().join("media");
        std::fs::create_dir_all(&media_root).unwrap();
        let song_path = media_root.join("song.mp3");
        std::fs::write(&song_path, b"audio").unwrap();

        let (tx, _rx) = mpsc::channel(64);
        let aligner = test_aligner(media_root, AlignCache::new(None), tx);
        let item = sample_item(song_path);

        let plain = Lyrics::from_lines(
            vec![
                LyricLine {
                    start_ms: None,
                    text: "First line".into(),
                    words: Vec::new(),
                },
                LyricLine {
                    start_ms: Some(2000),
                    text: "Second line".into(),
                    words: Vec::new(),
                },
            ],
            Origin::SidecarText,
        )
        .unwrap();

        assert_eq!(aligner.words_for(&item, &plain), Lookup::Unavailable);
    }

    #[test]
    fn words_for_returns_unavailable_when_all_lines_have_words() {
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().join("media");
        std::fs::create_dir_all(&media_root).unwrap();
        let song_path = media_root.join("song.mp3");
        std::fs::write(&song_path, b"audio").unwrap();

        let (tx, _rx) = mpsc::channel(64);
        let aligner = test_aligner(media_root, AlignCache::new(None), tx);
        let item = sample_item(song_path);

        let with_words = Lyrics::from_lines(
            vec![
                LyricLine {
                    start_ms: Some(1000),
                    text: "First line".into(),
                    words: vec![WordStamp {
                        at: 0,
                        start_ms: 1000,
                    }],
                },
                LyricLine {
                    start_ms: Some(2000),
                    text: "Second line".into(),
                    words: vec![WordStamp {
                        at: 0,
                        start_ms: 2000,
                    }],
                },
            ],
            Origin::SidecarLrc,
        )
        .unwrap();

        assert_eq!(aligner.words_for(&item, &with_words), Lookup::Unavailable);
    }

    #[test]
    fn words_for_returns_unavailable_for_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().join("media");
        std::fs::create_dir_all(&media_root).unwrap();
        let outside_dir = dir.path().join("outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let song_path = outside_dir.join("song.mp3");
        std::fs::write(&song_path, b"audio").unwrap();

        let (tx, _rx) = mpsc::channel(64);
        let aligner = test_aligner(media_root, AlignCache::new(None), tx);
        let item = sample_item(song_path);

        let lyrics = Lyrics::from_lines(
            vec![LyricLine {
                start_ms: Some(1000),
                text: "First line".into(),
                words: Vec::new(),
            }],
            Origin::SidecarLrc,
        )
        .unwrap();

        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Unavailable);
    }

    #[test]
    fn words_for_pending_lifecycle_and_cache_transitions() {
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().join("media");
        std::fs::create_dir_all(&media_root).unwrap();
        let song_path = media_root.join("song.mp3");
        std::fs::write(&song_path, b"audio").unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        let aligner = test_aligner(media_root, AlignCache::new(None), tx);
        let item = sample_item(song_path);

        let lyrics = Lyrics::from_lines(
            vec![
                LyricLine {
                    start_ms: Some(1000),
                    text: "First line".into(),
                    words: Vec::new(),
                },
                LyricLine {
                    start_ms: Some(3000),
                    text: "Second line".into(),
                    words: Vec::new(),
                },
            ],
            Origin::SidecarLrc,
        )
        .unwrap();

        // 1. First call: Pending, job enqueued.
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Pending(None));
        let job = rx.try_recv().expect("job received");
        assert_eq!(job.id, item.id);
        assert_eq!(job.path, "song.mp3");
        assert_eq!(job.lines.len(), 2);
        assert_eq!(job.lines[0], (1000, 3000, "First line".into()));
        assert_eq!(job.lines[1], (3000, 8000, "Second line".into()));

        // 2. Second call while pending: still Pending, no second job enqueued.
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Pending(None));
        assert!(rx.try_recv().is_err());

        // 3. Write cache entry for the key.
        let stamps = vec![
            vec![WordStamp {
                at: 0,
                start_ms: 1000,
            }],
            vec![WordStamp {
                at: 0,
                start_ms: 3000,
            }],
        ];
        let aligned = Aligned {
            lines: stamps,
            offset_ms: 0,
            scores: Vec::new(),
        };
        aligner.cache.put(item.id, job.key.clone(), aligned.clone());

        // 4. Ready returned once cache is written.
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Ready(aligned));

        // Remove from pending as background task would do.
        aligner.pending.lock().unwrap().remove(&item.id);

        // 5. Changed line text yields a different key, triggering Pending again.
        let mut changed_lyrics = lyrics;
        changed_lyrics.lines[0].text = "Changed line".into();
        assert_eq!(
            aligner.words_for(&item, &changed_lyrics),
            Lookup::Pending(None)
        );
        let job2 = rx.try_recv().expect("second job received");
        assert_ne!(job.key, job2.key);
        assert_eq!(job2.lines[0].2, "Changed line");
    }

    #[test]
    fn a_pending_song_is_told_how_long_the_queue_ahead_of_it_will_take() {
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().join("media");
        std::fs::create_dir_all(&media_root).unwrap();
        let song_path = media_root.join("song.mp3");
        std::fs::write(&song_path, b"audio").unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let aligner = test_aligner(media_root, AlignCache::new(None), tx);
        {
            // Learned: 500 ms of work per second of audio.
            let mut eta = aligner.eta_lock();
            eta.enqueue(1, 10_000);
            eta.start(1, 0);
            eta.finish(1, 5_000, 10_000);
        }
        let mut item = sample_item(song_path);
        item.probe.duration_ms = Some(20_000);
        let lyrics = Lyrics::from_lines(
            vec![LyricLine {
                start_ms: Some(1000),
                text: "Line one".into(),
                words: Vec::new(),
            }],
            Origin::SidecarLrc,
        )
        .unwrap();

        assert_eq!(
            aligner.words_for(&item, &lyrics),
            Lookup::Pending(Some(10_000))
        );
        assert_eq!(
            aligner.words_for(&item, &lyrics),
            Lookup::Pending(Some(10_000)),
            "asked again while queued"
        );
    }

    fn eta_errors(
        snapshotter: &metrics_util::debugging::Snapshotter,
    ) -> Vec<(String, String, f64)> {
        use metrics_util::debugging::DebugValue;
        let label = |k: &metrics_util::CompositeKey, name: &str| {
            k.key()
                .labels()
                .find(|l| l.key() == name)
                .map(|l| l.value().to_owned())
                .unwrap_or_default()
        };
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == ETA_ERROR_SECONDS)
            .flat_map(|(k, _, _, v)| match v {
                DebugValue::Histogram(values) => values
                    .into_iter()
                    .map(|x| (label(&k, "direction"), label(&k, "outcome"), x.into_inner()))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[derive(Clone, Default)]
    struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> LogBuf {
            self.clone()
        }
    }

    /// Captures this thread's log lines until the guard drops.
    fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
        let buf = LogBuf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        (buf.clone(), tracing::subscriber::set_default(sub))
    }

    /// The captured line holding `message`, which must carry both ETA fields.
    fn eta_line(buf: &LogBuf, message: &str) -> String {
        let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = log
            .lines()
            .find(|l| l.contains(message))
            .unwrap_or_else(|| panic!("no {message:?} line in {log}"))
            .to_owned();
        assert!(line.contains("eta.predicted_ms=2000"), "{line}");
        assert!(line.contains("eta.actual_ms="), "{line}");
        line
    }

    /// ODD: the estimate's error is on /metrics and in the log line. Disarm
    /// check: with the `histogram!` call in `Job::observe_eta`, or the eta
    /// fields of "lyrics: aligned", commented out, this fails.
    #[tokio::test]
    async fn a_finished_job_reports_how_far_off_its_estimate_was() {
        use crate::services::lyrics::shadow::tests::{body, fake};
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let (logs, _log_guard) = capture_logs();
        let answer = Aligned {
            lines: vec![vec![WordStamp {
                at: 0,
                start_ms: 1000,
            }]],
            offset_ms: 0,
            scores: vec![None],
        };
        let (base, _got) = fake(200, body(&answer, 2), None).await;
        let (tx, _rx) = mpsc::channel(1);
        let aligner = Aligner {
            base,
            ..test_aligner(PathBuf::from("/m"), AlignCache::new(None), tx)
        };
        let job = Job {
            id: 42,
            path: "a/song.flac".into(),
            lines: vec![(1000, 2000, "Line one".into())],
            key: "k".into(),
            duration_ms: 10_000,
            // Queued 5 s ago with 2 s predicted: about 3 s late.
            queued_at: Instant::now().checked_sub(Duration::from_secs(5)).unwrap(),
            predicted_ms: Some(2_000),
        };
        aligner.process_job(&job).await.unwrap();

        let seen = eta_errors(&snapshotter);
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (direction, outcome, seconds) = &seen[0];
        assert_eq!((direction.as_str(), outcome.as_str()), ("late", "aligned"));
        assert!((3.0..4.0).contains(seconds), "{seconds}");
        eta_line(&logs, "lyrics: aligned");
    }

    /// ODD (spec 040 FR-022): the time a job waited before it was sent is on
    /// /metrics. Disarm check: with the `histogram!` call at the top of
    /// `process_job` commented out, this fails.
    #[tokio::test]
    async fn a_sent_job_reports_how_long_it_waited() {
        use crate::services::lyrics::shadow::tests::{body, fake};
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let answer = Aligned {
            lines: vec![vec![WordStamp {
                at: 0,
                start_ms: 1000,
            }]],
            offset_ms: 0,
            scores: vec![None],
        };
        let (base, _got) = fake(200, body(&answer, 2), None).await;
        let (tx, _rx) = mpsc::channel(1);
        let aligner = Aligner {
            base,
            ..test_aligner(PathBuf::from("/m"), AlignCache::new(None), tx)
        };
        let job = Job {
            id: 7,
            path: "a/song.flac".into(),
            lines: vec![(1000, 2000, "Line one".into())],
            key: "k".into(),
            duration_ms: 10_000,
            queued_at: Instant::now().checked_sub(Duration::from_secs(3)).unwrap(),
            predicted_ms: None,
        };
        aligner.process_job(&job).await.unwrap();

        let waits: Vec<f64> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == WAIT_SECONDS)
            .flat_map(|(.., v)| match v {
                DebugValue::Histogram(h) => h.into_iter().map(|x| x.into_inner()).collect(),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(waits.len(), 1, "{waits:?}");
        assert!((3.0..4.0).contains(&waits[0]), "{waits:?}");
    }

    /// ODD symmetry: a failed job reports its estimate's error under
    /// `outcome="failed"` and logs both ETA fields. Disarm check: with the
    /// eta fields of "lyrics: alignment failed" commented out, this fails.
    #[test]
    fn a_failed_job_reports_how_far_off_its_estimate_was() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let (logs, _log_guard) = capture_logs();
        let (tx, _rx) = mpsc::channel(1);
        let aligner = test_aligner(PathBuf::from("/m"), AlignCache::new(None), tx);
        let job = Job {
            id: 42,
            path: "a/song.flac".into(),
            lines: Vec::new(),
            key: "k".into(),
            duration_ms: 10_000,
            // Queued just now with 2 s predicted: about 2 s early.
            queued_at: Instant::now(),
            predicted_ms: Some(2_000),
        };
        aligner.job_failed(&job, "aligner said no");

        let seen = eta_errors(&snapshotter);
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (direction, outcome, seconds) = &seen[0];
        assert_eq!((direction.as_str(), outcome.as_str()), ("early", "failed"));
        assert!((1.0..=2.0).contains(seconds), "{seconds}");
        let line = eta_line(&logs, "lyrics: alignment failed");
        assert!(line.contains("aligner said no"), "{line}");
        assert!(aligner.failed.lock().unwrap().contains_key(&42));
    }

    #[test]
    fn a_job_with_no_estimate_reports_no_error() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let job = Job {
            id: 1,
            path: "a/song.flac".into(),
            lines: Vec::new(),
            key: "k".into(),
            duration_ms: 0,
            queued_at: Instant::now(),
            predicted_ms: None,
        };
        let (predicted, _) = metrics::with_local_recorder(&recorder, || job.observe_eta("failed"));
        assert_eq!(predicted, None);
        assert!(eta_errors(&snapshotter).is_empty());
    }

    #[test]
    fn a_failed_song_is_left_alone_until_the_retry_time() {
        let dir = tempfile::tempdir().unwrap();
        let song_path = dir.path().join("song.mp3");
        std::fs::write(&song_path, b"audio").unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let aligner = test_aligner(dir.path().to_path_buf(), AlignCache::new(None), tx);
        let item = sample_item(song_path);
        let lyrics = Lyrics::from_lines(
            vec![LyricLine {
                start_ms: Some(1000),
                text: "One line".into(),
                words: Vec::new(),
            }],
            Origin::SidecarLrc,
        )
        .unwrap();
        // As the worker leaves a song whose job failed.
        let later = std::time::Instant::now() + RETRY_AFTER;
        aligner.failed.lock().unwrap().insert(item.id, later);
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Unavailable);
        assert!(rx.try_recv().is_err());
        // Once the time has passed it is asked again.
        let past = std::time::Instant::now();
        aligner.failed.lock().unwrap().insert(item.id, past);
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Pending(None));
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn more_lines_than_the_aligner_takes_are_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        let song_path = dir.path().join("song.mp3");
        std::fs::write(&song_path, b"audio").unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let aligner = test_aligner(dir.path().to_path_buf(), AlignCache::new(None), tx);
        let item = sample_item(song_path);
        let lines = (0..=MAX_LINES as u64)
            .map(|i| LyricLine {
                start_ms: Some(i * 1000),
                text: "La".into(),
                words: Vec::new(),
            })
            .collect();
        let lyrics = Lyrics::from_lines(lines, Origin::SidecarLrc).unwrap();
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Unavailable);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cache_round_trip_and_damaged_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("align_cache");
        let cache = AlignCache::new(Some(cache_dir.clone()));

        let stamps = vec![
            vec![WordStamp {
                at: 0,
                start_ms: 500,
            }],
            vec![WordStamp {
                at: 3,
                start_ms: 1500,
            }],
        ];

        let aligned = Aligned {
            lines: stamps,
            offset_ms: -3_700,
            scores: vec![Some(712), None],
        };
        assert_eq!(cache.get(10, "key_a"), None);
        cache.put(10, "key_a".into(), aligned.clone());
        assert_eq!(cache.get(10, "key_a"), Some(aligned));
        assert_eq!(cache.get(10, "key_b"), None);
        assert_eq!(cache.get(11, "key_a"), None);

        // Damaged file yields a miss rather than panicking.
        let file_path = cache_dir.join("10.words.json");
        std::fs::write(&file_path, b"corrupted json").unwrap();
        assert_eq!(cache.get(10, "key_a"), None);
    }

    /// Answers every request with `body`, counting them.
    async fn fake_service(body: &'static str) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = vec![0u8; 64 * 1024];
                let mut got = Vec::new();
                // Headers, then as much body as Content-Length says.
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if got.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (base, hits)
    }

    #[tokio::test]
    async fn sung_subtitle_lines_are_asked_for_once_then_read_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().join("media");
        std::fs::create_dir_all(&media_root).unwrap();
        let file = media_root.join("episode.mkv");
        std::fs::write(&file, b"x").unwrap();
        let (base, hits) = fake_service(
            r#"{"offset_ms": 0, "aligner_version": 4, "lines": [
                {"words": [{"at": 0, "start_ms": 1200}, {"at": 8, "start_ms": 1900}], "score": 0.61}
            ]}"#,
        )
        .await;
        let (tx, _rx) = mpsc::channel(1);
        let mut aligner = test_aligner(media_root, AlignCache::new(None), tx);
        aligner.base = base;
        let item = MediaItem {
            kind: pharos_core::MediaKind::Episode,
            ..sample_item(file)
        };
        let lines = vec![(1000, 2500, "Run away".to_owned())];

        let first = aligner.sing(&item, &lines).await.unwrap();
        assert_eq!(first.scores, [Some(610)]);
        assert_eq!(
            first.lines[0][1],
            WordStamp {
                at: 8,
                start_ms: 1900
            }
        );
        assert_eq!(aligner.sing(&item, &lines).await.unwrap(), first);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        let outside = MediaItem {
            path: PathBuf::from("/elsewhere/x.mkv"),
            ..item.clone()
        };
        assert!(aligner.sing(&outside, &lines).await.is_err());
        assert!(aligner.sing(&item, &[]).await.is_err());
    }

    #[tokio::test]
    async fn an_unscored_cached_answer_is_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let media_root = dir.path().join("media");
        std::fs::create_dir_all(&media_root).unwrap();
        let file = media_root.join("episode.mkv");
        std::fs::write(&file, b"x").unwrap();
        let (base, hits) = fake_service(
            r#"{"offset_ms": 0, "aligner_version": 3, "lines": [
                {"words": [{"at": 0, "start_ms": 1200}, {"at": 8, "start_ms": 1900}]}
            ]}"#,
        )
        .await;
        let (tx, _rx) = mpsc::channel(1);
        let mut aligner = test_aligner(media_root, AlignCache::new(None), tx);
        aligner.base = base;
        let item = MediaItem {
            kind: pharos_core::MediaKind::Episode,
            ..sample_item(file)
        };
        let lines = vec![(1000, 2500, "Run away".to_owned())];
        assert_eq!(aligner.sing(&item, &lines).await.unwrap().scores, [None]);
        aligner.sing(&item, &lines).await.unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn line_scores_are_read_in_thousandths_and_absent_from_an_older_aligner() {
        let answer = r#"{"offset_ms": 0, "aligner_version": 4, "lines": [
            {"words": [{"at": 0, "start_ms": 10}, {"at": 3, "start_ms": 90}], "score": 0.4567},
            {"words": [], "score": null}
        ]}"#;
        assert_eq!(parse_answer(answer, 2).unwrap().scores, [Some(457), None]);
        let older = r#"{"offset_ms": 0, "aligner_version": 2, "lines": [{"words": []}]}"#;
        assert_eq!(parse_answer(older, 1).unwrap().scores, [None]);
    }

    #[test]
    fn parse_answer_validates_and_rejects_line_count_mismatch() {
        let valid_json = r#"{
            "offset_ms": 0,
            "aligner_version": 2,
            "lines": [
                {
                    "words": [
                        { "at": 0, "start_ms": 1000 },
                        { "at": 5, "start_ms": 1500 }
                    ]
                },
                {
                    "words": []
                }
            ]
        }"#;

        let parsed = parse_answer(valid_json, 2).expect("parses correctly").lines;
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].len(), 2);
        assert_eq!(
            parsed[0][0],
            WordStamp {
                at: 0,
                start_ms: 1000
            }
        );
        assert_eq!(
            parsed[0][1],
            WordStamp {
                at: 5,
                start_ms: 1500
            }
        );
        assert!(parsed[1].is_empty());

        // Line count mismatch rejected.
        assert_eq!(
            parse_answer(valid_json, 1),
            Err("invalid aligner response: 2 lines for 1 asked".into())
        );
        assert!(parse_answer(valid_json, 3).is_err());

        // Malformed JSON rejected.
        assert!(parse_answer("not json", 2).is_err());
    }
    #[test]
    fn the_aligner_s_offset_is_read_and_moves_line_starts_but_never_below_zero() {
        let body = r#"{"lines": [{"words": []}], "offset_ms": 3700, "aligner_version": 2}"#;
        let late = parse_answer(body, 1).expect("parses");
        assert_eq!(late.offset_ms, 3_700);
        assert_eq!(late.shift(37_420), 41_120);
        let early = Aligned {
            lines: Vec::new(),
            offset_ms: -2_500,
            scores: Vec::new(),
        };
        assert_eq!(early.shift(10_000), 7_500);
        assert_eq!(early.shift(1_000), 0);
    }

    #[test]
    fn an_answer_from_an_aligner_without_the_offset_search_is_not_kept() {
        let old = r#"{"lines": [{"words": [{"at": 0, "start_ms": 5}]}], "aligner_version": 2}"#;
        let err = parse_answer(old, 1).expect_err("no offset_ms");
        assert!(err.contains("missing field `offset_ms`"), "{err}");
    }

    #[test]
    fn an_answer_from_an_aligner_with_the_narrow_window_is_not_kept() {
        let unnamed = r#"{"lines": [{"words": []}], "offset_ms": 0}"#;
        let err = parse_answer(unnamed, 1).expect_err("no aligner_version");
        assert!(err.contains("missing field `aligner_version`"), "{err}");
        let older = r#"{"lines": [{"words": []}], "offset_ms": 0, "aligner_version": 1}"#;
        assert_eq!(
            parse_answer(older, 1).map(|a| a.offset_ms),
            Err("aligner_version 1 is older than 2".into())
        );
    }

    #[test]
    fn a_words_cache_entry_from_before_the_offset_reads_as_no_offset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("7.words.json"),
            r#"{"key":"k","lines":[[{"at":0,"start_ms":5}]]}"#,
        )
        .unwrap();
        let cache = AlignCache::new(Some(dir.path().to_path_buf()));
        assert_eq!(cache.get(7, "k").map(|a| a.offset_ms), Some(0));
    }

    /// A streaming aligner on an ephemeral port: it answers each request
    /// with `events` as NDJSON chunks, holding back everything after the
    /// first `hold` until `gate` fires.
    async fn fake_stream(
        events: Vec<String>,
        hold: usize,
        gate: tokio::sync::oneshot::Receiver<()>,
        newline: bool,
    ) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 64 * 1024];
            let mut got = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got);
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if got.len() >= end + 4 + len {
                        assert!(
                            text.to_ascii_lowercase()
                                .contains("accept: application/x-ndjson"),
                            "{text}"
                        );
                        break;
                    }
                }
            }
            let head = "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            let end = if newline { "\n" } else { "" };
            let chunk = |e: &str| format!("{:x}\r\n{e}{end}\r\n", e.len() + end.len());
            for e in &events[..hold] {
                sock.write_all(chunk(e).as_bytes()).await.unwrap();
            }
            sock.flush().await.unwrap();
            let _ = gate.await;
            for e in &events[hold..] {
                sock.write_all(chunk(e).as_bytes()).await.unwrap();
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
        });
        base
    }

    fn two_line_job() -> Job {
        Job {
            id: 7,
            path: "a/song.flac".into(),
            lines: vec![
                (1000, 2000, "Line one".into()),
                (3000, 4000, "Line two".into()),
            ],
            key: "k".into(),
            duration_ms: 10_000,
            queued_at: Instant::now(),
            predicted_ms: Some(2_000),
        }
    }

    const DONE: &str = r#"{"event":"done","lines":[{"words":[{"at":0,"start_ms":1100}],"score":0.9},{"words":[{"at":0,"start_ms":3100}],"score":0.8}],"offset_ms":100,"aligner_version":5}"#;

    /// ADR-0121, V1297: each line a streaming aligner times is shown while
    /// the song is still in flight, and only the whole answer is stored.
    /// ODD: the job's log line says how many lines streamed and when the
    /// first came. Disarm check: with the `Event::Line` arm of
    /// `Aligner::event` emptied, or `lines_streamed` dropped from "lyrics:
    /// aligned", this fails.
    #[tokio::test]
    async fn a_streamed_song_shows_each_line_as_it_comes_and_stores_only_the_whole_answer() {
        let (logs, _log_guard) = capture_logs();
        let events = vec![
            r#"{"event":"offset","offset_ms":100,"aligner_version":5,"lines":2}"#.to_owned(),
            r#"{"event":"line","index":0,"words":[{"at":0,"start_ms":1100}],"score":0.9}"#
                .to_owned(),
            r#"{"event":"progress_from_a_newer_aligner"}"#.to_owned(),
            r#"{"event":"line","index":1,"words":[{"at":0,"start_ms":3100}],"score":0.8}"#
                .to_owned(),
            DONE.to_owned(),
        ];
        let (open, gate) = tokio::sync::oneshot::channel();
        let base = fake_stream(events, 3, gate, true).await;
        let (tx, _rx) = mpsc::channel(1);
        let aligner = Arc::new(Aligner {
            base,
            ..test_aligner(PathBuf::from("/m"), AlignCache::new(None), tx)
        });
        let job = two_line_job();
        let running = {
            let aligner = Arc::clone(&aligner);
            tokio::spawn(async move { aligner.process_job(&two_line_job()).await })
        };

        let mut partial = None;
        for _ in 0..200 {
            partial = aligner.partial_for(job.id, &job.key);
            if partial.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let partial = partial.expect("the first line is shown before the song is done");
        assert_eq!(partial.offset_ms, 100);
        assert_eq!(
            partial.lines,
            vec![
                vec![WordStamp {
                    at: 0,
                    start_ms: 1100
                }],
                vec![]
            ]
        );
        assert!(
            aligner.cache.get(job.id, "k").is_none(),
            "stored before done"
        );

        open.send(()).unwrap();
        running.await.unwrap().unwrap();
        let stored = aligner.cache.get(job.id, "k").expect("stored when done");
        assert_eq!(stored.lines.len(), 2);
        assert_eq!(stored.scores, vec![Some(900), Some(800)]);
        let line = eta_line(&logs, "lyrics: aligned");
        assert!(line.contains("lines_streamed=2"), "{line}");
        assert!(line.contains("first_line_ms="), "{line}");
    }

    /// ADR-0121: a song that fails after some lines streamed fails the job
    /// with the aligner's status, and none of its lines are stored.
    #[tokio::test]
    async fn a_streamed_song_that_fails_part_way_stores_nothing() {
        let events = vec![
            r#"{"event":"offset","offset_ms":0,"aligner_version":5,"lines":2}"#.to_owned(),
            r#"{"event":"line","index":0,"words":[{"at":0,"start_ms":1000}],"score":0.9}"#
                .to_owned(),
            r#"{"event":"error","status":422,"error":"ffmpeg failed: bad frame"}"#.to_owned(),
        ];
        let (open, gate) = tokio::sync::oneshot::channel();
        open.send(()).unwrap();
        let base = fake_stream(events, 0, gate, true).await;
        let (tx, _rx) = mpsc::channel(1);
        let aligner = Aligner {
            base,
            ..test_aligner(PathBuf::from("/m"), AlignCache::new(None), tx)
        };
        let err = aligner.process_job(&two_line_job()).await.unwrap_err();
        assert_eq!(err, "status 422: ffmpeg failed: bad frame");
        assert!(aligner.cache.get(7, "k").is_none());
    }

    /// A stream cut before its `done` event fails the job instead of storing
    /// the lines that came.
    #[tokio::test]
    async fn a_stream_cut_before_its_answer_fails_the_job() {
        let events =
            vec![r#"{"event":"offset","offset_ms":0,"aligner_version":5,"lines":2}"#.to_owned()];
        let (open, gate) = tokio::sync::oneshot::channel();
        open.send(()).unwrap();
        let base = fake_stream(events, 0, gate, true).await;
        let (tx, _rx) = mpsc::channel(1);
        let aligner = Aligner {
            base,
            ..test_aligner(PathBuf::from("/m"), AlignCache::new(None), tx)
        };
        let err = aligner.process_job(&two_line_job()).await.unwrap_err();
        assert_eq!(err, "aligner stream ended before its answer");
        assert!(aligner.cache.get(7, "k").is_none());
    }

    /// V1297: a song in flight with lines timed for its current lyrics is
    /// looked up as `Partial`, and with none yet, or only lines timed for
    /// other lyrics, as `Pending`.
    #[test]
    fn a_song_in_flight_is_partial_once_a_line_is_timed() {
        let dir = tempfile::tempdir().unwrap();
        let song = dir.path().join("song.flac");
        std::fs::write(&song, b"x").unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let aligner = test_aligner(dir.path().to_path_buf(), AlignCache::new(None), tx);
        let item = sample_item(song);
        let lyrics = Lyrics::from_lines(
            vec![
                LyricLine {
                    start_ms: Some(1000),
                    text: "Line one".into(),
                    words: vec![],
                },
                LyricLine {
                    start_ms: Some(3000),
                    text: "Line two".into(),
                    words: vec![],
                },
            ],
            Origin::SidecarLrc,
        )
        .unwrap();
        aligner.pending.lock().unwrap().insert(item.id);
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Pending(None));
        // The key words_for asks under: each line ends where the next starts,
        // the last 5 s after its own start.
        let key = cache_key(
            &[],
            "song.flac",
            &item.path,
            &[
                (1000, 3000, "Line one".into()),
                (3000, 8000, "Line two".into()),
            ],
        );

        // Lines timed for other lyrics (an edited sidecar) are not shown.
        for event in [
            &br#"{"event":"offset","offset_ms":50,"aligner_version":5,"lines":2}"#[..],
            br#"{"event":"line","index":0,"words":[{"at":0,"start_ms":1050}],"score":null}"#,
        ] {
            aligner.event((item.id, "older lyrics"), event, 2).unwrap();
        }
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Pending(None));

        aligner
            .event(
                (item.id, &key),
                br#"{"event":"offset","offset_ms":50,"aligner_version":5,"lines":2}"#,
                2,
            )
            .unwrap();
        // The offset alone shows nothing new: no line has words yet.
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Pending(None));
        aligner
            .event(
                (item.id, &key),
                br#"{"event":"line","index":1,"words":[{"at":0,"start_ms":3050}],"score":null}"#,
                2,
            )
            .unwrap();
        let Lookup::Partial(None, partial) = aligner.words_for(&item, &lyrics) else {
            panic!("expected a partial answer");
        };
        assert_eq!(partial.offset_ms, 50);
        assert_eq!(
            partial.lines,
            vec![
                vec![],
                vec![WordStamp {
                    at: 0,
                    start_ms: 3050
                }]
            ]
        );
    }

    /// An aligner that streams an event longer than `MAX_EVENT_BYTES` fails
    /// the job, whether or not the event ends in a newline: without one the
    /// buffer would grow, and with one in the chunk that crosses the limit
    /// the event would otherwise be read whole.
    #[tokio::test]
    async fn an_event_too_long_for_one_line_fails_the_job() {
        for newline in [false, true] {
            let events = vec!["x".repeat(MAX_EVENT_BYTES + 1)];
            let (open, gate) = tokio::sync::oneshot::channel();
            open.send(()).unwrap();
            let base = fake_stream(events, 0, gate, newline).await;
            let (tx, _rx) = mpsc::channel(1);
            let aligner = Aligner {
                base,
                ..test_aligner(PathBuf::from("/m"), AlignCache::new(None), tx)
            };
            let err = aligner.process_job(&two_line_job()).await.unwrap_err();
            assert!(err.contains("event over"), "newline {newline}: {err}");
        }
    }
}
