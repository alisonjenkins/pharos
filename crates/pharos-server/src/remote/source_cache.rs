//! A byte-range read-through cache in front of a remote source — 008.
//!
//! ffmpeg is handed a loopback URL instead of the CDN's. Reads are served from
//! a local file; anything missing is fetched upstream once, in chunks, and kept.
//!
//! # Why this exists
//!
//! pharos cuts each HLS segment with its own ffmpeg invocation, and every one of
//! them has to reach the source independently. Pointed straight at a signed CDN
//! URL that means, per segment: a fresh TLS handshake, a fresh range request,
//! and — for an mp4 — re-reading the container index before it can seek at all.
//! Multiply by a prefetch window and a site sees a burst of requests from one
//! viewer, which is how a source gets rate-limited halfway through a film.
//!
//! # Why a byte-range cache and not a download
//!
//! A sequential download cannot start playback early. For `bestvideo+bestaudio`
//! yt-dlp writes two `.part` files and then MERGES them, so the output does not
//! exist until everything has finished; and an mp4 with `moov` at the end fails
//! `avformat_open_input` outright on a partial file rather than short-reading.
//! Here a seek past what has been fetched FETCHES rather than waits, so playback
//! starts at the head and scrubbing works immediately.
//!
//! # What is deliberately not persisted
//!
//! Which chunks are present lives in memory only, and the cache directory is
//! cleared at startup. A sparse file's holes read back as zeros, which are
//! indistinguishable from real zero bytes — so trusting a bitmap across a
//! restart risks serving a hole as data, and a hole in a video is a corrupt
//! segment that nothing downstream will notice. Re-fetching after a restart
//! costs bandwidth; the alternative risks silent corruption.

use super::chunks::{chunk_bytes, chunks_for, missing_runs};
use crate::bg_io::{BgPermit, NetworkGate};
use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum SourceCacheError {
    #[error("upstream {url} answered {status}")]
    Upstream {
        url: String,
        status: reqwest::StatusCode,
    },
    #[error(
        "upstream {url} did not report a size; a source that cannot be sized cannot be seeked"
    )]
    NoSize { url: String },
    #[error("fetching {url}: {source}")]
    Fetch {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("cache io at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// One cached source: the upstream locator, its size, and what has arrived.
struct Entry {
    upstream: String,
    total: u64,
    /// Guards BOTH the file handle and the present-set, together. They describe
    /// one another — a chunk marked present whose bytes were not written is a
    /// hole served as data — so they are never lockable apart.
    state: Mutex<EntryState>,
}

struct EntryState {
    file: tokio::fs::File,
    present: HashSet<u64>,
}

impl EntryState {
    /// Bytes actually fetched, which is what fills the volume — not the source's
    /// total, most of which is usually still a hole.
    fn held_bytes(&self) -> u64 {
        self.present.len() as u64 * super::chunks::CHUNK
    }
}

/// Read-through cache over remote sources, served to ffmpeg over loopback.
pub struct SourceCache {
    dir: PathBuf,
    http: reqwest::Client,
    gate: NetworkGate,
    max_bytes: u64,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    /// Keys in least-recently-used order, oldest first. A `Vec` rather than
    /// anything cleverer because a household holds a handful of sources at
    /// once, and the cost of a linear scan is nothing beside a range fetch.
    lru: Mutex<Vec<String>>,
}

impl SourceCache {
    /// Build the cache, clearing anything a previous process left behind.
    ///
    /// The wipe is not tidiness — see the module docs on why a present-set
    /// cannot outlive the process that built it.
    pub async fn new(dir: impl Into<PathBuf>, gate: NetworkGate, max_bytes: u64) -> Self {
        let dir = dir.into();
        let _ = tokio::fs::remove_dir_all(&dir).await;
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            tracing::warn!(path = %dir.display(), error = %e, "could not create the remote source cache directory");
        }
        Self {
            dir,
            http: reqwest::Client::new(),
            gate,
            max_bytes,
            entries: Mutex::new(HashMap::new()),
            lru: Mutex::new(Vec::new()),
        }
    }

    /// Register a source and return the key ffmpeg's loopback URL carries.
    ///
    /// Keyed by the UPSTREAM locator, so a re-resolution that returns the same
    /// URL reuses what has been fetched, and one that returns a different URL
    /// gets its own entry rather than mixing two resolutions' bytes in one file.
    pub async fn register(&self, upstream: &str) -> Result<String, SourceCacheError> {
        let key = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(upstream.as_bytes()));
        {
            let entries = self.entries.lock().await;
            if entries.contains_key(&key) {
                return Ok(key);
            }
        }
        // Size comes from the FIRST range GET's `Content-Range`, not a HEAD.
        //
        // One round trip instead of two, and it warms chunk 0 — which is what
        // ffmpeg reads first anyway. It is also the more robust of the two: a
        // HEAD's `Content-Length` is exactly the header this project has
        // already been bitten by (V113), and an origin that answers HEAD
        // differently from GET, or not at all, is common enough that a CDN's
        // range response is the better authority on its own length.
        let (total, head) = self.fetch_head_chunk(upstream).await?;
        let path = self.dir.join(&key);
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .await
            .map_err(|source| SourceCacheError::Io {
                path: path.display().to_string(),
                source,
            })?;
        // Size the sparse file up front so a read past the fetched region has
        // somewhere to land and `seek` arithmetic stays simple.
        file.set_len(total)
            .await
            .map_err(|source| SourceCacheError::Io {
                path: path.display().to_string(),
                source,
            })?;
        let mut state = EntryState {
            file,
            present: HashSet::new(),
        };
        // Chunk 0 is already in hand — write it before anyone can read, so the
        // first segment does not immediately re-fetch what registration just
        // downloaded.
        if !head.is_empty() {
            state
                .file
                .write_all(&head)
                .await
                .map_err(|source| SourceCacheError::Io {
                    path: path.display().to_string(),
                    source,
                })?;
            state.present.insert(0);
        }
        let entry = Arc::new(Entry {
            upstream: upstream.to_string(),
            total,
            state: Mutex::new(state),
        });
        self.entries.lock().await.insert(key.clone(), entry);
        self.touch(&key).await;
        tracing::info!(key, total, "remote source registered with the range cache");
        self.evict_over_cap().await;
        Ok(key)
    }

    /// The source's total size, for the `Content-Length` ffmpeg seeks against.
    pub async fn total(&self, key: &str) -> Option<u64> {
        self.entries.lock().await.get(key).map(|e| e.total)
    }

    /// Serve `[start, end)`, fetching whatever is missing first.
    pub async fn read(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, SourceCacheError> {
        let Some(entry) = self.entries.lock().await.get(key).cloned() else {
            return Ok(Vec::new());
        };
        self.touch(key).await;
        let end = end.min(entry.total);
        if end <= start {
            return Ok(Vec::new());
        }
        let wanted = chunks_for(start, end);

        let mut state = entry.state.lock().await;
        let runs = missing_runs(wanted, &state.present);
        for run in runs {
            let bytes = chunk_bytes(run.start, entry.total).start
                ..chunk_bytes(run.end.saturating_sub(1), entry.total).end;
            if bytes.is_empty() {
                continue;
            }
            let body = self.fetch(&entry.upstream, bytes.clone()).await?;
            state
                .file
                .seek(SeekFrom::Start(bytes.start))
                .await
                .map_err(|source| SourceCacheError::Io {
                    path: key.to_string(),
                    source,
                })?;
            state
                .file
                .write_all(&body)
                .await
                .map_err(|source| SourceCacheError::Io {
                    path: key.to_string(),
                    source,
                })?;
            // Marked present ONLY after the bytes are on disk. The reverse
            // order would let a failed write leave a chunk advertised as
            // cached, and the hole would be served as data forever.
            for i in run {
                state.present.insert(i);
            }
        }

        let len = (end - start) as usize;
        let mut buf = vec![0u8; len];
        state
            .file
            .seek(SeekFrom::Start(start))
            .await
            .map_err(|source| SourceCacheError::Io {
                path: key.to_string(),
                source,
            })?;
        state
            .file
            .read_exact(&mut buf)
            .await
            .map_err(|source| SourceCacheError::Io {
                path: key.to_string(),
                source,
            })?;
        Ok(buf)
    }

    /// Drop a source's entry and its file — for item deletion and eviction.
    pub async fn forget(&self, upstream: &str) {
        let key = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(upstream.as_bytes()));
        self.drop_key(&key).await;
    }

    /// Bytes currently held, for the eviction decision and the gauge.
    pub async fn held_bytes(&self) -> u64 {
        let entries: Vec<_> = self.entries.lock().await.values().cloned().collect();
        let mut sum = 0;
        for e in entries {
            sum += e.state.lock().await.held_bytes();
        }
        sum
    }

    /// Sources currently held.
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    async fn touch(&self, key: &str) {
        let mut lru = self.lru.lock().await;
        lru.retain(|k| k != key);
        lru.push(key.to_string());
    }

    /// Drop least-recently-used sources until the cache is under its cap.
    ///
    /// Evicting a WHOLE source rather than individual chunks: a half-evicted
    /// source is the same hazard as a stale present-set, and the thing being
    /// protected is the volume, which cares about files rather than ranges.
    ///
    /// The most recent source is never evicted even when it alone exceeds the
    /// cap. It is almost certainly what someone is watching, and dropping it
    /// would send the next segment straight back upstream to re-fetch what was
    /// just discarded — a cache that thrashes is worse than none.
    async fn evict_over_cap(&self) {
        loop {
            let held = self.held_bytes().await;
            if held <= self.max_bytes {
                return;
            }
            let victim = {
                let lru = self.lru.lock().await;
                if lru.len() <= 1 {
                    tracing::warn!(
                        held,
                        max = self.max_bytes,
                        "the remote source cache is over its cap with one source held; \
                         not evicting what is being watched",
                    );
                    return;
                }
                lru[0].clone()
            };
            self.drop_key(&victim).await;
            metrics::counter!("pharos_remote_cache_evicted_total").increment(1);
            tracing::info!(
                key = victim,
                held,
                max = self.max_bytes,
                "evicted a remote source"
            );
        }
    }

    async fn drop_key(&self, key: &str) {
        self.entries.lock().await.remove(key);
        self.lru.lock().await.retain(|k| k != key);
        let _ = tokio::fs::remove_file(self.dir.join(key)).await;
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Fetch chunk 0 and learn the source's total size from `Content-Range`.
    ///
    /// Returns `(total, bytes)`. A 200 instead of a 206 means the origin
    /// ignored the range and sent everything, which is still usable — the body
    /// IS the whole source, so its length is the total.
    async fn fetch_head_chunk(&self, url: &str) -> Result<(u64, Vec<u8>), SourceCacheError> {
        let _permit = BgPermit::network(&self.gate).await;
        let resp = self
            .http
            .get(url)
            .header(
                reqwest::header::RANGE,
                format!("bytes=0-{}", super::chunks::CHUNK - 1),
            )
            .send()
            .await
            .map_err(|source| SourceCacheError::Fetch {
                url: url.to_string(),
                source,
            })?;
        if !resp.status().is_success() {
            return Err(SourceCacheError::Upstream {
                url: url.to_string(),
                status: resp.status(),
            });
        }
        let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let total = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next().map(str::to_owned))
            .and_then(|t| t.parse::<u64>().ok());
        let body = resp
            .bytes()
            .await
            .map_err(|source| SourceCacheError::Fetch {
                url: url.to_string(),
                source,
            })?;
        let total = match (total, partial) {
            (Some(t), _) => t,
            // Whole-body response: its length is the source's length.
            (None, false) => body.len() as u64,
            // A 206 with no parseable Content-Range leaves the size unknown,
            // and a source that cannot be sized cannot be seeked — which for a
            // video means every segment after the first fails. Refuse now,
            // where the message names the URL.
            (None, true) => {
                return Err(SourceCacheError::NoSize {
                    url: url.to_string(),
                })
            }
        };
        metrics::counter!("pharos_remote_range_fetch_total", "outcome" => "ok").increment(1);
        metrics::counter!("pharos_remote_range_bytes_total").increment(body.len() as u64);
        Ok((total, body.to_vec()))
    }

    async fn fetch(
        &self,
        url: &str,
        bytes: std::ops::Range<u64>,
    ) -> Result<Vec<u8>, SourceCacheError> {
        // Metered on the NETWORK gate, not the disk one: this contends for
        // upstream bandwidth and a site's rate limit, neither of which the
        // disk regulator knows anything about (V135).
        let _permit = BgPermit::network(&self.gate).await;
        let range = format!("bytes={}-{}", bytes.start, bytes.end.saturating_sub(1));
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::RANGE, &range)
            .send()
            .await
            .map_err(|source| SourceCacheError::Fetch {
                url: url.to_string(),
                source,
            })?;
        if !resp.status().is_success() {
            return Err(SourceCacheError::Upstream {
                url: url.to_string(),
                status: resp.status(),
            });
        }
        metrics::counter!("pharos_remote_range_fetch_total", "outcome" => "ok").increment(1);
        let body = resp
            .bytes()
            .await
            .map_err(|source| SourceCacheError::Fetch {
                url: url.to_string(),
                source,
            })?;
        metrics::counter!("pharos_remote_range_bytes_total").increment(body.len() as u64);
        Ok(body.to_vec())
    }
}

/// The loopback URL ffmpeg is given for a registered source.
pub fn local_url(port: u16, key: &str) -> String {
    format!("http://127.0.0.1:{port}/src/{key}")
}

/// Where a key's bytes live, for tests and for eviction.
pub fn entry_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(key)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::remote::chunks::CHUNK;
    use tempfile::TempDir;

    /// The loopback URL is what ffmpeg opens, so its shape is a contract with
    /// the route that serves it.
    #[test]
    fn the_loopback_url_addresses_a_key_on_localhost() {
        let u = local_url(45123, "deadbeefdeadbeef");
        assert_eq!(u, "http://127.0.0.1:45123/src/deadbeefdeadbeef");
        // Loopback specifically: this listener has no authentication, so it
        // must never be reachable off the host.
        assert!(u.starts_with("http://127.0.0.1:"));
    }

    /// Two different resolutions of one video get different entries.
    ///
    /// Keyed by the upstream locator rather than the item, because a signed URL
    /// rotates and a re-resolution can point at genuinely different bytes.
    /// Sharing one file between them would interleave two encodes.
    #[tokio::test]
    async fn a_rotated_upstream_url_gets_its_own_entry() {
        let dir = TempDir::new().unwrap();
        let cache = SourceCache::new(dir.path(), NetworkGate::new(2), u64::MAX).await;
        assert!(cache.is_empty().await);

        // Unknown keys read as empty rather than erroring — a request for a
        // source that was evicted mid-playback is a miss, not a fault.
        assert!(cache.read("nope", 0, 10).await.unwrap().is_empty());

        // The key is a pure function of the URL, so the same locator reuses one
        // entry and a rotated one does not.
        let a = "https://cdn.example/v?expire=1000&sig=aaa";
        let b = "https://cdn.example/v?expire=2000&sig=bbb";
        let ka = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(a.as_bytes()));
        let kb = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(b.as_bytes()));
        assert_ne!(ka, kb, "a rotated URL must not share a cache file");
        assert_eq!(
            ka,
            format!("{:016x}", xxhash_rust::xxh3::xxh3_64(a.as_bytes())),
            "and the same URL must map to the same file every time"
        );
    }

    /// End to end against a real HTTP server: bytes come back correct, and a
    /// re-read of cached bytes issues NO upstream request.
    ///
    /// The request COUNT is the assertion. Correct bytes alone would pass
    /// against a cache that fetched everything again every time — which is the
    /// exact failure this whole module exists to prevent, and which nothing
    /// else here would notice.
    #[tokio::test]
    async fn a_second_read_of_cached_bytes_touches_the_network_once() {
        use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A body spanning three chunks with a short tail, so the final-chunk
        // clamp is exercised rather than assumed.
        let total = (CHUNK * 2 + 1234) as usize;
        let body: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let hits = Arc::new(AtomicUsize::new(0));

        let served = web::Data::new((body.clone(), hits.clone()));
        let srv = HttpServer::new(move || {
            App::new().app_data(served.clone()).default_service(web::to(
                |req: HttpRequest, d: web::Data<(Vec<u8>, Arc<AtomicUsize>)>| async move {
                    let (body, hits) = (&d.0, &d.1);
                    hits.fetch_add(1, Ordering::SeqCst);
                    let range = req
                        .headers()
                        .get(actix_web::http::header::RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("bytes="))
                        .and_then(|v| v.split_once('-'))
                        .and_then(|(a, b)| {
                            Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                        });
                    let (a, b) = range.unwrap_or((0, body.len() - 1));
                    let b = b.min(body.len() - 1);
                    // A real CDN's 206 carries the TOTAL after the slash, which
                    // is where the cache learns the source's length.
                    HttpResponse::PartialContent()
                        .insert_header((
                            actix_web::http::header::CONTENT_RANGE,
                            format!("bytes {a}-{b}/{}", body.len()),
                        ))
                        .body(body[a..=b].to_vec())
                },
            ))
        })
        .bind("127.0.0.1:0")
        .expect("bind");
        let port = srv.addrs()[0].port();
        let handle = srv.run();
        let stop = handle.handle();
        tokio::spawn(handle);

        let dir = TempDir::new().unwrap();
        let cache = SourceCache::new(dir.path(), NetworkGate::new(2), u64::MAX).await;
        let url = format!("http://127.0.0.1:{port}/source.mp4");
        let key = cache.register(&url).await.expect("register");
        assert_eq!(cache.total(&key).await, Some(total as u64));

        // A read inside chunk 0.
        // Registration already fetched chunk 0, so a read inside it costs
        // NOTHING further — that warming is the point of doing it there.
        let after_register = hits.load(Ordering::SeqCst);
        assert_eq!(after_register, 1, "registration is one request, not two");
        let got = cache.read(&key, 10, 4096).await.expect("read");
        assert_eq!(got, &body[10..4096], "cached bytes must match the source");
        let after_first = hits.load(Ordering::SeqCst);
        assert_eq!(
            after_first, after_register,
            "a read inside the chunk registration already fetched must not re-fetch it"
        );

        // The same range again — served entirely from disk.
        let again = cache.read(&key, 10, 4096).await.expect("read");
        assert_eq!(again, &body[10..4096]);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            after_first,
            "a re-read of cached bytes must NOT reach upstream; without this the \
             cache is a no-op that happens to return the right answer"
        );

        // A seek into the TAIL fetches rather than waits, and the short final
        // chunk comes back at its real length rather than 416ing.
        let tail_start = CHUNK * 2 + 100;
        let tail = cache
            .read(&key, tail_start, total as u64)
            .await
            .expect("tail read");
        assert_eq!(
            tail,
            &body[tail_start as usize..],
            "the short tail must be exact"
        );

        // Reading past the end clamps instead of erroring.
        let past = cache
            .read(&key, total as u64 - 5, total as u64 + 5000)
            .await
            .expect("clamped read");
        assert_eq!(past, &body[total - 5..]);

        // Forgetting drops the file, so a deleted item leaves nothing behind.
        cache.forget(&url).await;
        assert!(!entry_path(dir.path(), &key).exists());

        stop.stop(false).await;
    }

    /// The cache directory is emptied at startup.
    ///
    /// A sparse file's holes read back as zeros, indistinguishable from real
    /// data, so a present-set cannot outlive the process that built it — and a
    /// file left behind without one would be served as if complete.
    #[tokio::test]
    async fn startup_discards_whatever_the_last_process_left() {
        let dir = TempDir::new().unwrap();
        let stale = dir.path().join("cache");
        tokio::fs::create_dir_all(&stale).await.unwrap();
        tokio::fs::write(stale.join("abcdef0123456789"), b"stale bytes")
            .await
            .unwrap();

        let _cache = SourceCache::new(&stale, NetworkGate::new(1), u64::MAX).await;
        assert!(
            !stale.join("abcdef0123456789").exists(),
            "a file surviving startup would be served as if its holes were data"
        );
        assert!(stale.exists(), "but the directory itself is recreated");
    }
}

/// The loopback HTTP listener ffmpeg reads through.
///
/// Bound to 127.0.0.1 on an ephemeral port and deliberately unauthenticated:
/// it exposes only what the cache already holds, addressed by a key that is a
/// hash of a URL the caller must already know, and it is unreachable off the
/// host. Adding auth would mean teaching ffmpeg a credential for no gain.
pub mod server {
    use super::SourceCache;
    use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
    use std::sync::Arc;

    /// Parse a `Range: bytes=a-b` header into a half-open byte range.
    ///
    /// HTTP ranges are INCLUSIVE at both ends and everything else here is
    /// half-open, so the `+1` is the whole point of this function existing
    /// separately — getting it wrong drops the last byte of every read, which
    /// in a video is a truncated frame rather than an error.
    pub fn parse_range(header: Option<&str>, total: u64) -> (u64, u64) {
        let Some(spec) = header.and_then(|h| h.strip_prefix("bytes=")) else {
            return (0, total);
        };
        let (a, b) = match spec.split_once('-') {
            Some(p) => p,
            None => return (0, total),
        };
        let start: u64 = a.parse().unwrap_or(0);
        // An open-ended `bytes=N-` means "to the end", which is what ffmpeg
        // sends when it wants to stream forward from a seek point.
        let end = b
            .trim()
            .parse::<u64>()
            .map(|last| last.saturating_add(1))
            .unwrap_or(total);
        (start.min(total), end.min(total))
    }

    async fn serve(
        req: HttpRequest,
        key: web::Path<String>,
        cache: web::Data<Arc<SourceCache>>,
    ) -> HttpResponse {
        let key = key.into_inner();
        let Some(total) = cache.total(&key).await else {
            return HttpResponse::NotFound().finish();
        };
        let header = req
            .headers()
            .get(actix_web::http::header::RANGE)
            .and_then(|v| v.to_str().ok());
        let ranged = header.is_some();
        let (start, end) = parse_range(header, total);
        match cache.read(&key, start, end).await {
            Ok(bytes) => {
                let len = bytes.len() as u64;
                if ranged {
                    HttpResponse::PartialContent()
                        .insert_header((
                            actix_web::http::header::CONTENT_RANGE,
                            format!("bytes {start}-{}/{total}", start + len.saturating_sub(1)),
                        ))
                        .body(bytes)
                } else {
                    // `Accept-Ranges` is not decoration: without it libavformat
                    // treats the input as unseekable and falls back to reading
                    // from the beginning for every segment, which is precisely
                    // the cost this cache exists to remove.
                    HttpResponse::Ok()
                        .insert_header((actix_web::http::header::ACCEPT_RANGES, "bytes"))
                        .body(bytes)
                }
            }
            Err(e) => {
                tracing::warn!(key, error = %e, "serving a cached remote range failed");
                HttpResponse::BadGateway().finish()
            }
        }
    }

    /// Start the listener, returning the port ffmpeg URLs should address.
    pub fn spawn(cache: Arc<SourceCache>) -> std::io::Result<u16> {
        let data = web::Data::new(cache);
        let srv = HttpServer::new(move || {
            App::new()
                .app_data(data.clone())
                .route("/src/{key}", web::get().to(serve))
                .route("/src/{key}", web::head().to(serve))
        })
        // Workers kept low: this only reads local files and the upstream fetch
        // it may trigger is already bounded by the network gate.
        .workers(2)
        .bind("127.0.0.1:0")?;
        let port = srv.addrs()[0].port();
        tokio::spawn(srv.run());
        tracing::info!(port, "remote source cache listening on loopback");
        Ok(port)
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, clippy::expect_used)]

        use super::*;

        /// HTTP ranges are inclusive at both ends; everything else here is
        /// half-open. Dropping the `+1` loses the last byte of every read,
        /// which in a video is a truncated frame rather than a visible error.
        #[test]
        fn an_http_range_converts_to_a_half_open_range() {
            assert_eq!(parse_range(Some("bytes=0-99"), 1000), (0, 100));
            assert_eq!(parse_range(Some("bytes=100-199"), 1000), (100, 200));
            // Open-ended: what ffmpeg sends to stream forward from a seek.
            assert_eq!(parse_range(Some("bytes=500-"), 1000), (500, 1000));
            // No header at all is the whole body.
            assert_eq!(parse_range(None, 1000), (0, 1000));
            // Past the end clamps rather than overruns.
            assert_eq!(parse_range(Some("bytes=900-5000"), 1000), (900, 1000));
            assert_eq!(parse_range(Some("bytes=5000-6000"), 1000), (1000, 1000));
            // Garbage degrades to the whole body instead of panicking.
            assert_eq!(parse_range(Some("bytes=abc-def"), 1000), (0, 1000));
            assert_eq!(parse_range(Some("nonsense"), 1000), (0, 1000));
        }
    }
}
