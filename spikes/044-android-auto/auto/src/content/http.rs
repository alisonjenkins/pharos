//! A small HTTP/1.1 GET client for the audio cache.
//!
//! Only what downloading a track needs: one request per connection, optional
//! `Range` / `If-Range`, a streamed body that is either `Content-Length`
//! delimited, chunked or close-delimited. TLS is rustls with the ring
//! provider and the bundled webpki roots, the same stack the websocket link
//! uses. Cleartext is accepted for loopback only.
//!
//! Errors name the host and the failing step, never the URL: stream URLs carry
//! the media token as `api_key`.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// Longest the TCP and TLS handshakes may take; a black spot hangs here.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest a read may stall; a dead link otherwise leaves the socket open.
pub const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// A response head larger than this is not an audio server.
const MAX_HEAD: usize = 16 * 1024;
const READ_BUF: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("not a usable url: {0}")]
    BadUrl(String),
    #[error("connect {host}: {why}")]
    Connect { host: String, why: String },
    #[error("{host}: no answer within {secs} s while {step}")]
    Stalled {
        host: String,
        step: &'static str,
        secs: u64,
    },
    #[error("{host}: {why}")]
    Io { host: String, why: String },
    #[error("{host}: bad response: {why}")]
    BadResponse { host: String, why: String },
    #[error("{host}: body ended after {got} of {want} bytes")]
    Truncated { host: String, got: u64, want: u64 },
}

/// A parsed `http(s)://host[:port]/path?query`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    tls: bool,
    host: String,
    port: u16,
    target: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Self, HttpError> {
        let bad = |why: &str| HttpError::BadUrl(format!("{why}: {}", redact(url)));
        let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(bad("scheme is not http(s)"));
        };
        let (authority, target) = match rest.find(['/', '?']) {
            Some(i) => {
                let (a, t) = rest.split_at(i);
                let t = if t.starts_with('/') {
                    t.to_owned()
                } else {
                    format!("/{t}")
                };
                (a, t)
            }
            None => (rest, "/".to_owned()),
        };
        if authority.is_empty() || authority.contains('@') {
            return Err(bad("empty host or userinfo"));
        }
        let (host, port) = split_authority(authority).ok_or_else(|| bad("bad host or port"))?;
        let port = port.unwrap_or(if tls { 443 } else { 80 });
        if !tls && !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
            return Err(bad("cleartext http is for loopback only"));
        }
        Ok(Self {
            tls,
            host,
            port,
            target,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }
}

/// `host[:port]` or `[v6][:port]`.
fn split_authority(a: &str) -> Option<(String, Option<u16>)> {
    let (host, tail) = if a.starts_with('[') {
        let end = a.find(']')?;
        let (h, t) = a.split_at(end.checked_add(1)?);
        (h, t)
    } else {
        match a.find(':') {
            Some(i) => a.split_at(i),
            None => (a, ""),
        }
    };
    let port = match tail {
        "" => None,
        t => Some(t.strip_prefix(':')?.parse::<u16>().ok()?),
    };
    Some((host.to_owned(), port))
}

/// `url` with the value of every `api_key` parameter hidden, for logs.
pub fn redact(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let cleaned: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if k.eq_ignore_ascii_case("api_key") => format!("{k}=<redacted>"),
            _ => kv.to_owned(),
        })
        .collect();
    format!("{base}?{}", cleaned.join("&"))
}

/// What one request asks for beyond the URL.
#[derive(Debug, Clone, Default)]
pub struct Request<'a> {
    /// `Range: bytes=N-`.
    pub range_from: Option<u64>,
    /// `If-Range`: only honour the range when the validator still matches.
    pub if_range: Option<&'a str>,
}

/// `Content-Range: bytes first-last/total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    pub first: u64,
    pub last: u64,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    Length(u64),
    Chunked,
    UntilClose,
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub struct Response {
    pub status: u16,
    pub content_length: Option<u64>,
    pub content_range: Option<ContentRange>,
    pub etag: Option<String>,
    host: String,
    io: Box<dyn Io>,
    /// Bytes read past the head that belong to the body, not yet decoded.
    pending: Vec<u8>,
    framing: Framing,
    /// Chunked: bytes left in the current chunk.
    chunk_left: u64,
    done: bool,
    got: u64,
    read_timeout: Duration,
}

fn tls_config() -> Result<Arc<ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
                .map_err(|e| e.to_string())
        })
        .clone()
}

/// Send one GET and read the response head. The body is read with
/// [`Response::next_chunk`].
pub async fn get(
    url: &Url,
    req: &Request<'_>,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<Response, HttpError> {
    let host = url.host.clone();
    let tcp = tokio::time::timeout(
        connect_timeout,
        TcpStream::connect((url.host.trim_matches(['[', ']']), url.port)),
    )
    .await
    .map_err(|_| HttpError::Stalled {
        host: host.clone(),
        step: "connecting",
        secs: connect_timeout.as_secs(),
    })?
    .map_err(|e| HttpError::Connect {
        host: host.clone(),
        why: e.to_string(),
    })?;
    let _ = tcp.set_nodelay(true);
    let mut io: Box<dyn Io> = if url.tls {
        let name = ServerName::try_from(url.host.clone()).map_err(|e| HttpError::Connect {
            host: host.clone(),
            why: format!("bad server name: {e}"),
        })?;
        let config = tls_config().map_err(|why| HttpError::Connect {
            host: host.clone(),
            why: format!("tls setup: {why}"),
        })?;
        let tls = tokio::time::timeout(
            connect_timeout,
            TlsConnector::from(config).connect(name, tcp),
        )
        .await
        .map_err(|_| HttpError::Stalled {
            host: host.clone(),
            step: "tls handshake",
            secs: connect_timeout.as_secs(),
        })?
        .map_err(|e| HttpError::Connect {
            host: host.clone(),
            why: format!("tls: {e}"),
        })?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };

    let mut head = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: pharos-auto\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
        url.target,
        if (url.tls && url.port == 443) || (!url.tls && url.port == 80) {
            url.host.clone()
        } else {
            format!("{}:{}", url.host, url.port)
        }
    );
    if let Some(from) = req.range_from {
        head.push_str(&format!("Range: bytes={from}-\r\n"));
        if let Some(v) = req.if_range {
            head.push_str(&format!("If-Range: {v}\r\n"));
        }
    }
    head.push_str("\r\n");
    let io_err = |host: &str, e: std::io::Error| HttpError::Io {
        host: host.to_owned(),
        why: e.to_string(),
    };
    tokio::time::timeout(read_timeout, io.write_all(head.as_bytes()))
        .await
        .map_err(|_| HttpError::Stalled {
            host: host.clone(),
            step: "sending the request",
            secs: read_timeout.as_secs(),
        })?
        .map_err(|e| io_err(&host, e))?;

    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let (consumed, status, headers) = loop {
        let mut raw = [httparse::EMPTY_HEADER; 48];
        let mut parsed = httparse::Response::new(&mut raw);
        match parsed.parse(&buf) {
            Ok(httparse::Status::Complete(n)) => {
                let headers: Vec<(String, String)> = parsed
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_ascii_lowercase(),
                            String::from_utf8_lossy(h.value).trim().to_owned(),
                        )
                    })
                    .collect();
                break (n, parsed.code.unwrap_or(0), headers);
            }
            Ok(httparse::Status::Partial) => {}
            Err(e) => {
                return Err(HttpError::BadResponse {
                    host,
                    why: format!("head: {e}"),
                })
            }
        }
        if buf.len() > MAX_HEAD {
            return Err(HttpError::BadResponse {
                host,
                why: format!("head longer than {MAX_HEAD} bytes"),
            });
        }
        let n = tokio::time::timeout(read_timeout, io.read(&mut chunk))
            .await
            .map_err(|_| HttpError::Stalled {
                host: host.clone(),
                step: "waiting for the response head",
                secs: read_timeout.as_secs(),
            })?
            .map_err(|e| io_err(&host, e))?;
        if n == 0 {
            return Err(HttpError::BadResponse {
                host,
                why: "closed before the response head ended".into(),
            });
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
    };

    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let content_length = find("content-length").and_then(|v| v.parse::<u64>().ok());
    let chunked =
        find("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let framing = if chunked {
        Framing::Chunked
    } else if let Some(n) = content_length {
        Framing::Length(n)
    } else {
        Framing::UntilClose
    };
    let content_range = find("content-range").and_then(parse_content_range);
    let etag = find("etag").map(str::to_owned);
    let pending = buf.get(consumed..).unwrap_or_default().to_vec();
    Ok(Response {
        status,
        content_length,
        content_range,
        etag,
        host,
        io,
        pending,
        framing,
        chunk_left: 0,
        done: false,
        got: 0,
        read_timeout,
    })
}

fn parse_content_range(v: &str) -> Option<ContentRange> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    let (span, total) = rest.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    Some(ContentRange {
        first: first.trim().parse().ok()?,
        last: last.trim().parse().ok()?,
        total: total.trim().parse().ok(),
    })
}

impl Response {
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Body bytes delivered so far.
    pub fn received(&self) -> u64 {
        self.got
    }

    fn io_err(&self, e: std::io::Error) -> HttpError {
        HttpError::Io {
            host: self.host.clone(),
            why: e.to_string(),
        }
    }

    fn bad(&self, why: &str) -> HttpError {
        HttpError::BadResponse {
            host: self.host.clone(),
            why: why.to_owned(),
        }
    }

    /// Read more bytes from the socket into `pending`; false at EOF.
    async fn fill(&mut self) -> Result<bool, HttpError> {
        let mut buf = vec![0u8; READ_BUF];
        let n = tokio::time::timeout(self.read_timeout, self.io.read(&mut buf))
            .await
            .map_err(|_| HttpError::Stalled {
                host: self.host.clone(),
                step: "reading the body",
                secs: self.read_timeout.as_secs(),
            })?
            .map_err(|e| self.io_err(e))?;
        self.pending
            .extend_from_slice(buf.get(..n).unwrap_or_default());
        Ok(n > 0)
    }

    /// The next piece of the decoded body; `None` once it has ended cleanly.
    /// A body cut short is an error, never a quiet end.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        if self.done {
            return Ok(None);
        }
        match self.framing {
            Framing::Length(want) => {
                if self.got >= want {
                    self.done = true;
                    return Ok(None);
                }
                if self.pending.is_empty() && !self.fill().await? {
                    return Err(HttpError::Truncated {
                        host: self.host.clone(),
                        got: self.got,
                        want,
                    });
                }
                let left = usize::try_from(want.saturating_sub(self.got)).unwrap_or(usize::MAX);
                let take = self.pending.len().min(left);
                let out: Vec<u8> = self.pending.drain(..take).collect();
                self.got = self.got.saturating_add(out.len() as u64);
                Ok(Some(out))
            }
            Framing::UntilClose => {
                if self.pending.is_empty() && !self.fill().await? {
                    self.done = true;
                    return Ok(None);
                }
                let out = std::mem::take(&mut self.pending);
                self.got = self.got.saturating_add(out.len() as u64);
                Ok(Some(out))
            }
            Framing::Chunked => loop {
                if self.chunk_left == 0 {
                    match self.read_chunk_size().await? {
                        0 => {
                            self.done = true;
                            return Ok(None);
                        }
                        n => self.chunk_left = n,
                    }
                }
                if self.pending.is_empty() && !self.fill().await? {
                    return Err(HttpError::Truncated {
                        host: self.host.clone(),
                        got: self.got,
                        want: self.got.saturating_add(self.chunk_left),
                    });
                }
                let left = usize::try_from(self.chunk_left).unwrap_or(usize::MAX);
                let take = self.pending.len().min(left);
                let out: Vec<u8> = self.pending.drain(..take).collect();
                self.chunk_left = self.chunk_left.saturating_sub(out.len() as u64);
                self.got = self.got.saturating_add(out.len() as u64);
                if self.chunk_left == 0 {
                    self.skip_crlf().await?;
                }
                if !out.is_empty() {
                    return Ok(Some(out));
                }
            },
        }
    }

    /// Parse `<hex>[;ext]\r\n`; consumes the trailer section after the last chunk.
    async fn read_chunk_size(&mut self) -> Result<u64, HttpError> {
        let line = loop {
            if let Some(i) = self.pending.windows(2).position(|w| w == b"\r\n") {
                let line: Vec<u8> = self.pending.drain(..i.saturating_add(2)).collect();
                break line;
            }
            if self.pending.len() > 1024 || !self.fill().await? {
                return Err(self.bad("chunk size line missing"));
            }
        };
        let text =
            String::from_utf8_lossy(line.get(..line.len().saturating_sub(2)).unwrap_or_default())
                .into_owned();
        let hex = text.split(';').next().unwrap_or("").trim();
        u64::from_str_radix(hex, 16).map_err(|_| self.bad(&format!("bad chunk size {hex:?}")))
    }

    async fn skip_crlf(&mut self) -> Result<(), HttpError> {
        while self.pending.len() < 2 {
            if !self.fill().await? {
                return Err(self.bad("chunk not terminated"));
            }
        }
        if self.pending.get(..2) != Some(b"\r\n".as_slice()) {
            return Err(self.bad("chunk not terminated by CRLF"));
        }
        self.pending.drain(..2);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::content::testserver::{Script, TestServer};

    const T: Duration = Duration::from_secs(5);

    fn body(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    async fn read_all(r: &mut Response) -> Result<Vec<u8>, HttpError> {
        let mut out = Vec::new();
        while let Some(c) = r.next_chunk().await? {
            out.extend_from_slice(&c);
        }
        Ok(out)
    }

    #[test]
    fn urls_parse_and_cleartext_is_loopback_only() {
        let u = Url::parse("https://pharos.example/audio/ab?x=1&api_key=s3cret").unwrap();
        assert_eq!(u.host(), "pharos.example");
        assert_eq!(u.port, 443);
        assert_eq!(u.target, "/audio/ab?x=1&api_key=s3cret");
        let u = Url::parse("http://127.0.0.1:8080/a").unwrap();
        assert_eq!((u.port, u.tls), (8080, false));
        assert!(Url::parse("http://pharos.example/a").is_err());
        assert!(Url::parse("ftp://x/a").is_err());
        assert!(Url::parse("https://u:p@x/a").is_err());
    }

    #[test]
    fn errors_and_redaction_never_carry_the_token() {
        let e = Url::parse("http://pharos.example/a?api_key=s3cret").unwrap_err();
        assert!(!e.to_string().contains("s3cret"), "{e}");
        assert_eq!(
            redact("https://h/a?x=1&api_key=s3cret&y=2"),
            "https://h/a?x=1&api_key=<redacted>&y=2"
        );
    }

    #[tokio::test]
    async fn a_plain_get_reads_the_whole_body() {
        let data = body(100_000);
        let srv = TestServer::start(Script::default().file("/a", data.clone()));
        let url = Url::parse(&srv.url("/a")).unwrap();
        let mut r = get(&url, &Request::default(), T, T).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_length, Some(100_000));
        assert_eq!(read_all(&mut r).await.unwrap(), data);
    }

    #[tokio::test]
    async fn a_range_get_returns_the_tail_with_its_content_range() {
        let data = body(1000);
        let srv = TestServer::start(Script::default().file("/a", data.clone()));
        let url = Url::parse(&srv.url("/a")).unwrap();
        let req = Request {
            range_from: Some(400),
            if_range: None,
        };
        let mut r = get(&url, &req, T, T).await.unwrap();
        assert_eq!(r.status, 206);
        assert_eq!(
            r.content_range,
            Some(ContentRange {
                first: 400,
                last: 999,
                total: Some(1000)
            })
        );
        assert_eq!(read_all(&mut r).await.unwrap(), data[400..]);
    }

    #[tokio::test]
    async fn a_chunked_body_is_decoded() {
        let data = body(70_000);
        let srv = TestServer::start(Script::default().file("/a", data.clone()).chunked());
        let url = Url::parse(&srv.url("/a")).unwrap();
        let mut r = get(&url, &Request::default(), T, T).await.unwrap();
        assert_eq!(r.content_length, None);
        assert_eq!(read_all(&mut r).await.unwrap(), data);
    }

    #[tokio::test]
    async fn a_connection_dropped_mid_body_is_an_error_not_a_short_file() {
        for chunked in [false, true] {
            let mut s = Script::default()
                .file("/a", body(50_000))
                .drop_first_after(10_000);
            if chunked {
                s = s.chunked();
            }
            let srv = TestServer::start(s);
            let url = Url::parse(&srv.url("/a")).unwrap();
            let mut r = get(&url, &Request::default(), T, T).await.unwrap();
            let e = read_all(&mut r).await.expect_err("cut short");
            assert!(
                matches!(
                    e,
                    HttpError::Truncated { .. }
                        | HttpError::Io { .. }
                        | HttpError::BadResponse { .. }
                ),
                "chunked={chunked}: {e}"
            );
        }
    }

    #[tokio::test]
    async fn a_refused_connection_names_the_host() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/a?api_key=zzz", l.local_addr().unwrap())).unwrap();
        drop(l);
        let e = get(&url, &Request::default(), T, T).await.err().unwrap();
        assert!(
            matches!(&e, HttpError::Connect { host, .. } if host == "127.0.0.1"),
            "{e}"
        );
        assert!(!e.to_string().contains("zzz"));
    }
}
