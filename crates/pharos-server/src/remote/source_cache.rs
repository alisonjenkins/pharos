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

/// Read-through cache over remote sources, served to ffmpeg over loopback.
pub struct SourceCache {
    dir: PathBuf,
    http: reqwest::Client,
    gate: NetworkGate,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
}

impl SourceCache {
    /// Build the cache, clearing anything a previous process left behind.
    ///
    /// The wipe is not tidiness — see the module docs on why a present-set
    /// cannot outlive the process that built it.
    pub async fn new(dir: impl Into<PathBuf>, gate: NetworkGate) -> Self {
        let dir = dir.into();
        let _ = tokio::fs::remove_dir_all(&dir).await;
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            tracing::warn!(path = %dir.display(), error = %e, "could not create the remote source cache directory");
        }
        Self {
            dir,
            http: reqwest::Client::new(),
            gate,
            entries: Mutex::new(HashMap::new()),
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
        tracing::info!(key, total, "remote source registered with the range cache");
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
        self.entries.lock().await.remove(&key);
        let _ = tokio::fs::remove_file(self.dir.join(&key)).await;
    }

    /// Bytes currently held, for the eviction decision and the gauge.
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
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
        let cache = SourceCache::new(dir.path(), NetworkGate::new(2)).await;
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
        let cache = SourceCache::new(dir.path(), NetworkGate::new(2)).await;
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

        let _cache = SourceCache::new(&stale, NetworkGate::new(1)).await;
        assert!(
            !stale.join("abcdef0123456789").exists(),
            "a file surviving startup would be served as if its holes were data"
        );
        assert!(stale.exists(), "but the directory itself is recreated");
    }
}
