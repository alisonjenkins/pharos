//! A local HTTP server for the cache tests: serves bytes with `Range`
//! support and can drop a connection mid-body, answer chunked, cap range
//! windows, or 401 a stale `api_key`. Test scaffolding only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A body that stops after `at` bytes: `reached` is set when it
/// has, and it goes on once `release` is set.
#[derive(Debug, Clone)]
struct Hold {
    at: usize,
    release: Arc<std::sync::atomic::AtomicBool>,
    reached: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone, Default)]
pub struct Script {
    files: HashMap<String, Vec<u8>>,
    chunked: bool,
    /// Bytes per chunk of a chunked body; 8192 when 0.
    piece: usize,
    ignore_range: bool,
    window: Option<u64>,
    /// A chunked body stops after this many bytes until the flag is set.
    hold: Option<Hold>,
    /// Each body served pops one entry and closes the socket after that many
    /// body bytes.
    drops: VecDeque<usize>,
    valid_token: Option<String>,
    /// Paths that answer 503 until the script is changed.
    busy: Vec<String>,
    /// The `ETag` every reply carries; `"v1"` when unset.
    etag: Option<String>,
    /// Replies carry no `ETag` at all.
    no_etag: bool,
    /// Every request seen: path, `Range` header, status sent.
    pub log: Vec<Seen>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub path: String,
    pub range: Option<String>,
    /// The `If-Range` header, when sent.
    pub if_range: Option<String>,
    pub status: u16,
}

impl Script {
    pub fn file(mut self, path: &str, bytes: Vec<u8>) -> Self {
        self.files.insert(path.to_owned(), bytes);
        self
    }

    pub fn chunked(mut self) -> Self {
        self.chunked = true;
        self.ignore_range = true;
        self
    }

    /// Serves a chunked body in pieces of `bytes`, not 8192.
    pub fn chunk_bytes(mut self, bytes: usize) -> Self {
        self.piece = bytes;
        self
    }

    /// Holds a body (chunked or of known length) after `bytes` of it, to see
    /// a download in the middle of its body. Returns the flag that releases it and the flag
    /// the server sets once it has stopped there.
    pub fn hold_after(
        mut self,
        bytes: usize,
    ) -> (
        Self,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.hold = Some(Hold {
            at: bytes,
            release: release.clone(),
            reached: reached.clone(),
        });
        (self, release, reached)
    }

    pub fn window(mut self, bytes: u64) -> Self {
        self.window = Some(bytes);
        self
    }

    pub fn drop_first_after(mut self, bytes: usize) -> Self {
        self.drops.push_back(bytes);
        self
    }

    /// Every reply carries this `ETag` (quoted), not `"v1"`.
    pub fn etag(mut self, tag: &str) -> Self {
        self.etag = Some(tag.to_owned());
        self
    }

    /// Replies carry no `ETag`: a server with no validator to give.
    pub fn no_etag(mut self) -> Self {
        self.no_etag = true;
        self
    }

    /// Serves every request whole (200), whatever `Range` it sends.
    pub fn ignore_range(mut self) -> Self {
        self.ignore_range = true;
        self
    }

    pub fn busy(mut self, path: &str) -> Self {
        self.busy.push(path.to_owned());
        self
    }

    pub fn valid_token(mut self, token: &str) -> Self {
        self.valid_token = Some(token.to_owned());
        self
    }
}

pub struct TestServer {
    addr: SocketAddr,
    state: Arc<Mutex<Script>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    pub fn start(script: Script) -> Self {
        let state = Arc::new(Mutex::new(script));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let st = state.clone();
        let thread = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                let Ok(listener) = TcpListener::bind("127.0.0.1:0").await else {
                    return;
                };
                if let Ok(a) = listener.local_addr() {
                    let _ = addr_tx.send(a);
                }
                tokio::pin!(stopped);
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        acc = listener.accept() => {
                            if let Ok((sock, _)) = acc {
                                tokio::spawn(serve(sock, st.clone()));
                            }
                        }
                    }
                }
            });
        });
        let addr = addr_rx.recv().expect("test server bound");
        Self {
            addr,
            state,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn url_with_token(&self, path: &str, token: &str) -> String {
        format!("http://{}{path}?api_key={token}", self.addr)
    }

    /// Changes the script for the requests from now on.
    pub fn edit(&self, f: impl FnOnce(Script) -> Script) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = std::mem::take(&mut *s);
        *s = f(now);
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .log
            .clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn serve(mut sock: TcpStream, state: Arc<Mutex<Script>>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(tmp.get(..n).unwrap_or_default()),
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let mut lines = head.lines();
    let target = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let range = lines
        .clone()
        .find_map(|l| l.strip_prefix("Range: ").map(str::to_owned));
    let if_range = lines
        .clone()
        .find_map(|l| l.strip_prefix("If-Range: ").map(str::to_owned));
    let token = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("api_key="))
        .map(str::to_owned);

    // One snapshot of the script per request: the body, its etag and how
    // it is paced all come from the same lock, so a test changing the
    // script mid-request never pairs one version's body with another's.
    let (piece, hold, etag, (reply, drop_after)) = {
        let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
        let piece = s.piece;
        let hold = s.hold.clone();
        let etag = (!s.no_etag).then(|| s.etag.clone().unwrap_or_else(|| "v1".to_owned()));
        let r = plan(&mut s, path, token.as_deref(), range.as_deref());
        s.log.push(Seen {
            path: path.to_owned(),
            range: range.clone(),
            if_range,
            status: r.0.status,
        });
        (piece, hold, etag, r)
    };
    let piece = if piece == 0 { 8192 } else { piece };
    let mut out = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", reply.status).into_bytes();
    if let Some(cr) = &reply.content_range {
        out.extend_from_slice(format!("Content-Range: {cr}\r\n").as_bytes());
    }
    if let Some(etag) = etag {
        out.extend_from_slice(format!("ETag: \"{etag}\"\r\n").as_bytes());
    }
    if reply.chunked {
        out.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
    } else {
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", reply.body.len()).as_bytes());
    }
    let _ = sock.write_all(&out).await;
    let send = drop_after.map_or(reply.body.len(), |n| n.min(reply.body.len()));
    let part = reply.body.get(..send).unwrap_or_default();
    let at = hold.as_ref().map(|h| h.at);
    // A hold at 0 stops after the headers, before any body byte: no piece
    // ends there, so the loops below never reach it.
    if at == Some(0) {
        wait_at(hold.as_ref()).await;
    }
    if reply.chunked {
        for (start, end) in pieces(part.len(), piece, at) {
            let piece = part.get(start..end).unwrap_or_default();
            let _ = sock
                .write_all(format!("{:x}\r\n", piece.len()).as_bytes())
                .await;
            let _ = sock.write_all(piece).await;
            let _ = sock.write_all(b"\r\n").await;
            if at == Some(end) {
                wait_at(hold.as_ref()).await;
            }
        }
        if drop_after.is_none() {
            let _ = sock.write_all(b"0\r\n\r\n").await;
        }
    } else if hold.is_some() {
        // A known-length body held after `at` bytes: the client has the
        // length, so its reservation is refreshed as the body arrives.
        for (start, end) in pieces(part.len(), piece, at) {
            let _ = sock
                .write_all(part.get(start..end).unwrap_or_default())
                .await;
            if at == Some(end) {
                wait_at(hold.as_ref()).await;
            }
        }
    } else {
        let _ = sock.write_all(part).await;
    }
    let _ = sock.shutdown().await;
}

/// The byte ranges a body of `len` is sent in: `piece` bytes each, with the
/// piece that crosses `at` split there, so a hold stops after exactly `at`
/// bytes whatever the piece size.
/// Computed one range at a time as the body is sent, so a small piece size
/// on a large body allocates nothing up front.
fn pieces(len: usize, piece: usize, at: Option<usize>) -> impl Iterator<Item = (usize, usize)> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= len {
            return None;
        }
        let mut end = start.saturating_add(piece).min(len);
        if let Some(at) = at.filter(|&a| a > start && a < end) {
            end = at;
        }
        let range = (start, end);
        start = end;
        Some(range)
    })
}

/// Marks the hold reached and waits for its release.
async fn wait_at(hold: Option<&Hold>) {
    let Some(Hold {
        release, reached, ..
    }) = hold
    else {
        return;
    };
    reached.store(true, std::sync::atomic::Ordering::SeqCst);
    while !release.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

struct Reply {
    status: u16,
    content_range: Option<String>,
    chunked: bool,
    body: Vec<u8>,
}

fn plan(
    s: &mut Script,
    path: &str,
    token: Option<&str>,
    range: Option<&str>,
) -> (Reply, Option<usize>) {
    let plain = |status| Reply {
        status,
        content_range: None,
        chunked: false,
        body: Vec::new(),
    };
    if let Some(valid) = &s.valid_token {
        if token != Some(valid.as_str()) {
            return (plain(401), None);
        }
    }
    if s.busy.iter().any(|b| b == path) {
        return (plain(503), None);
    }
    let Some(file) = s.files.get(path) else {
        return (plain(404), None);
    };
    let total = file.len() as u64;
    let from = range
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|r| r.parse::<u64>().ok())
        .filter(|_| !s.ignore_range);
    let reply = match from {
        Some(f) if f >= total => Reply {
            content_range: Some(format!("bytes */{total}")),
            ..plain(416)
        },
        Some(f) => {
            let end = s.window.map_or(total, |w| f.saturating_add(w).min(total));
            let lo = usize::try_from(f).unwrap_or(usize::MAX);
            let hi = usize::try_from(end).unwrap_or(usize::MAX);
            Reply {
                status: 206,
                content_range: Some(format!("bytes {f}-{}/{total}", end.saturating_sub(1))),
                chunked: false,
                body: file.get(lo..hi).unwrap_or_default().to_vec(),
            }
        }
        None => Reply {
            status: 200,
            content_range: None,
            chunked: s.chunked,
            body: file.clone(),
        },
    };
    let drop_after = s.drops.pop_front();
    (reply, drop_after)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A piece size past the address space ends at the body, without
    /// overflowing; ordinary sizes split as before.
    #[test]
    fn an_oversized_piece_ends_at_the_body() {
        assert_eq!(pieces(10, usize::MAX, None).collect::<Vec<_>>(), [(0, 10)]);
        // From a hold past 0, an unclamped end would overflow.
        assert_eq!(
            pieces(2, usize::MAX, Some(1)).collect::<Vec<_>>(),
            [(0, 1), (1, 2)]
        );
        assert_eq!(
            pieces(10, 4, Some(6)).collect::<Vec<_>>(),
            [(0, 4), (4, 6), (6, 10)]
        );
    }
    use std::io::{Read, Write};

    /// Reads what the server sends until the hold is reached, then a moment
    /// more: the body bytes received by then.
    fn body_at_hold(
        script: Script,
        path: &str,
        reached: &std::sync::atomic::AtomicBool,
    ) -> Vec<u8> {
        let server = TestServer::start(script);
        let mut sock = std::net::TcpStream::connect(server.addr).unwrap();
        write!(sock, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 65536];
        let mut waited = 0;
        while waited < 400 {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) if reached.load(std::sync::atomic::Ordering::SeqCst) => break,
                Err(_) => waited += 1,
            }
        }
        assert!(
            reached.load(std::sync::atomic::Ordering::SeqCst),
            "the hold was never reached"
        );
        let head = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        got.split_off(head)
    }

    /// A hold stops after exactly the bytes asked for, on a known-length
    /// body whose last piece crosses it and on a chunked body whose piece
    /// boundary does not fall on it.
    #[test]
    fn a_hold_stops_after_exactly_the_bytes_asked_for() {
        let (script, release, reached) = Script::default()
            .file("/f", vec![7; 10_000])
            .hold_after(9_999);
        let body = body_at_hold(script, "/f", &reached);
        assert_eq!(
            body.len(),
            9_999,
            "the known-length body went past its hold"
        );
        release.store(true, std::sync::atomic::Ordering::SeqCst);

        let (script, release, reached) = Script::default()
            .file("/c", vec![7; 10_000])
            .chunked()
            .chunk_bytes(4096)
            .hold_after(5_000);
        let body = body_at_hold(script, "/c", &reached);
        // Chunk framing: 4096 bytes, then the split 904.
        let data: usize = String::from_utf8_lossy(&body)
            .split("\r\n")
            .collect::<Vec<_>>()
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| c[1].len())
            .sum();
        assert_eq!(data, 5_000, "the chunked body went past its hold");
        release.store(true, std::sync::atomic::Ordering::SeqCst);

        // A hold at 0: the headers, and no body byte.
        let (script, release, reached) =
            Script::default().file("/z", vec![7; 10_000]).hold_after(0);
        let body = body_at_hold(script, "/z", &reached);
        assert!(
            body.is_empty(),
            "{} body bytes went past a hold at 0",
            body.len()
        );
        release.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn pieces_split_at_the_hold_and_keep_aligned_ones() {
        assert_eq!(
            pieces(10, 4, Some(6)).collect::<Vec<_>>(),
            [(0, 4), (4, 6), (6, 10)]
        );
        assert_eq!(
            pieces(10, 4, Some(8)).collect::<Vec<_>>(),
            [(0, 4), (4, 8), (8, 10)]
        );
        assert_eq!(
            pieces(10, 4, None).collect::<Vec<_>>(),
            [(0, 4), (4, 8), (8, 10)]
        );
    }
}
