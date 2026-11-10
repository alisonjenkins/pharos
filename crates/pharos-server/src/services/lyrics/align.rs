//! Forced alignment client and worker for lyric word times (ADR-0066).
//!
//! When `[lyrics].aligner_url` is configured, karaoke requests queue a job
//! to compute word-level timestamps from audio and synced lyric text. Jobs
//! are de-duplicated and processed one at a time by a background task. Answers
//! are cached on disk or in memory.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pharos_core::MediaItem;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

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
}

/// The aligner's answer for a song: word stamps per line, and how far it
/// found the song's lyric stamps to be off from the audio (B936).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Aligned {
    pub lines: Vec<Vec<WordStamp>>,
    /// Added to every line's start to land it on the singing; positive when
    /// the lyrics were early.
    pub offset_ms: i64,
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
    /// Alignment is queued or currently in flight.
    Pending,
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

/// Client and background worker for lyric forced alignment.
pub struct Aligner {
    http: Client,
    base: String,
    media_root: PathBuf,
    cache: AlignCache,
    pending: Mutex<HashSet<u64>>,
    /// Songs whose last alignment failed, and when to try them again.
    failed: Mutex<HashMap<u64, std::time::Instant>>,
    tx: mpsc::Sender<Job>,
}

impl Aligner {
    /// Start the aligner background worker and return a shared handle.
    pub fn start(url: String, media_root: PathBuf, cache_dir: Option<PathBuf>) -> Arc<Self> {
        let ua = format!(
            "pharos/{} (https://github.com/alisonjenkins/pharos)",
            env!("CARGO_PKG_VERSION")
        );
        let http = Client::builder()
            .user_agent(ua)
            .timeout(TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());

        let (tx, mut rx) = mpsc::channel(64);
        let cache = AlignCache::new(cache_dir);
        let base = url.trim_end_matches('/').to_owned();
        let aligner = Arc::new(Self {
            http,
            base,
            media_root,
            cache,
            pending: Mutex::new(HashSet::new()),
            failed: Mutex::default(),
            tx,
        });

        let worker = Arc::clone(&aligner);
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                if let Err(e) = worker.process_job(&job).await {
                    tracing::warn!(media.id = job.id, error = %e, "lyrics: alignment failed");
                    let retry = std::time::Instant::now() + RETRY_AFTER;
                    match worker.failed.lock() {
                        Ok(mut f) => f.insert(job.id, retry),
                        Err(f) => f.into_inner().insert(job.id, retry),
                    };
                }
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

    async fn process_job(&self, job: &Job) -> Result<(), String> {
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

        let req_lines: Vec<AlignRequestLine<'_>> = job
            .lines
            .iter()
            .map(|(start, end, text)| AlignRequestLine {
                start_ms: *start,
                end_ms: *end,
                text,
            })
            .collect();

        let payload = AlignRequest {
            path: &job.path,
            lines: req_lines,
        };

        let url = format!("{}/align", self.base);
        let resp = self
            .http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("request: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("status {status}"));
        }

        let body = resp.text().await.map_err(|e| format!("body: {e}"))?;
        let aligned = parse_answer(&body, job.lines.len())?;
        tracing::info!(
            media.id = job.id,
            lines = job.lines.len(),
            offset_ms = aligned.offset_ms,
            "lyrics: aligned"
        );

        self.cache.put(job.id, job.key.clone(), aligned);
        Ok(())
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

        let (size, mtime) = match std::fs::metadata(&item.path) {
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

        let json_bytes = serde_json::to_vec(&serde_json::json!([
            ALIGN_VERSION,
            &rel_path,
            size,
            mtime,
            &job_lines,
        ]))
        .unwrap_or_default();

        let mut hasher = Sha256::new();
        hasher.update(&json_bytes);
        let hash = hasher.finalize();
        let mut key = String::with_capacity(64);
        for byte in hash {
            use std::fmt::Write as _;
            let _ = write!(key, "{byte:02x}");
        }

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
            return Lookup::Pending;
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
        let job = Job {
            id: item.id,
            path: rel_path,
            lines: job_lines,
            key,
        };

        match self.tx.try_send(job) {
            Ok(()) => Lookup::Pending,
            Err(_) => {
                pending.remove(&item.id);
                Lookup::Unavailable
            }
        }
    }
}

/// Parse the aligner service's JSON response, rejecting line count mismatches.
/// The error names what was wrong, e.g. serde's "missing field `offset_ms`"
/// from an aligner older than the offset search.
fn parse_answer(body: &str, n_lines: usize) -> Result<Aligned, String> {
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
    }

    #[derive(Deserialize)]
    struct ReplyWord {
        at: u32,
        start_ms: u64,
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

    Ok(Aligned {
        lines: reply
            .lines
            .into_iter()
            .map(|l| {
                l.words
                    .into_iter()
                    .map(|w| WordStamp {
                        at: w.at,
                        start_ms: w.start_ms,
                    })
                    .collect()
            })
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
            http: Client::new(),
            base: "http://127.0.0.1:8790".into(),
            media_root,
            cache,
            pending: Mutex::new(HashSet::new()),
            failed: Mutex::default(),
            tx,
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
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Pending);
        let job = rx.try_recv().expect("job received");
        assert_eq!(job.id, item.id);
        assert_eq!(job.path, "song.mp3");
        assert_eq!(job.lines.len(), 2);
        assert_eq!(job.lines[0], (1000, 3000, "First line".into()));
        assert_eq!(job.lines[1], (3000, 8000, "Second line".into()));

        // 2. Second call while pending: still Pending, no second job enqueued.
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Pending);
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
        };
        aligner.cache.put(item.id, job.key.clone(), aligned.clone());

        // 4. Ready returned once cache is written.
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Ready(aligned));

        // Remove from pending as background task would do.
        aligner.pending.lock().unwrap().remove(&item.id);

        // 5. Changed line text yields a different key, triggering Pending again.
        let mut changed_lyrics = lyrics;
        changed_lyrics.lines[0].text = "Changed line".into();
        assert_eq!(aligner.words_for(&item, &changed_lyrics), Lookup::Pending);
        let job2 = rx.try_recv().expect("second job received");
        assert_ne!(job.key, job2.key);
        assert_eq!(job2.lines[0].2, "Changed line");
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
        assert_eq!(aligner.words_for(&item, &lyrics), Lookup::Pending);
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
}
