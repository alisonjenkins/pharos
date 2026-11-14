//! A local HTTP server for the cache tests: serves bytes with `Range`
//! support and can drop a connection mid-body, answer chunked, cap range
//! windows, or 401 a stale `api_key`. Test scaffolding only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone, Default)]
pub struct Script {
    files: HashMap<String, Vec<u8>>,
    chunked: bool,
    ignore_range: bool,
    window: Option<u64>,
    /// Each body served pops one entry and closes the socket after that many
    /// body bytes.
    drops: VecDeque<usize>,
    valid_token: Option<String>,
    /// Every request seen: path, `Range` header, status sent.
    pub log: Vec<Seen>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub path: String,
    pub range: Option<String>,
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

    pub fn window(mut self, bytes: u64) -> Self {
        self.window = Some(bytes);
        self
    }

    pub fn drop_first_after(mut self, bytes: usize) -> Self {
        self.drops.push_back(bytes);
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
    let token = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("api_key="))
        .map(str::to_owned);

    let (reply, drop_after) = {
        let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
        let r = plan(&mut s, path, token.as_deref(), range.as_deref());
        s.log.push(Seen {
            path: path.to_owned(),
            range: range.clone(),
            status: r.0.status,
        });
        r
    };
    let mut out = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", reply.status).into_bytes();
    if let Some(cr) = &reply.content_range {
        out.extend_from_slice(format!("Content-Range: {cr}\r\n").as_bytes());
    }
    out.extend_from_slice(b"ETag: \"v1\"\r\n");
    if reply.chunked {
        out.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
    } else {
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", reply.body.len()).as_bytes());
    }
    let _ = sock.write_all(&out).await;
    let send = drop_after.map_or(reply.body.len(), |n| n.min(reply.body.len()));
    let part = reply.body.get(..send).unwrap_or_default();
    if reply.chunked {
        for piece in part.chunks(8192) {
            let _ = sock
                .write_all(format!("{:x}\r\n", piece.len()).as_bytes())
                .await;
            let _ = sock.write_all(piece).await;
            let _ = sock.write_all(b"\r\n").await;
        }
        if drop_after.is_none() {
            let _ = sock.write_all(b"0\r\n\r\n").await;
        }
    } else {
        let _ = sock.write_all(part).await;
    }
    let _ = sock.shutdown().await;
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
