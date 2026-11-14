//! A blocking, seekable view of a streamed song: each seek opens a ranged
//! GET at the new offset, reading is sequential from there. For a decoder
//! that wants `Read + Seek` over a stream URL (ADR-0133).
//!
//! Runs on its own thread's runtime, so it must not be used from inside an
//! async task.

use std::io::{self, Read, Seek, SeekFrom};

use tokio::runtime::Runtime;

use super::http::{self, HttpError, Request, Response, Url, CONNECT_TIMEOUT, READ_TIMEOUT};

/// A forward seek shorter than this is read through instead of reopening.
const SKIP_LIMIT: u64 = 64 * 1024;

pub struct RangedReader {
    url: Url,
    rt: Runtime,
    resp: Option<Response>,
    /// Bytes of the body read but not yet handed out.
    chunk: Vec<u8>,
    chunk_at: usize,
    /// The offset the next byte read is at.
    pos: u64,
    len: Option<u64>,
}

fn io_err(e: HttpError) -> io::Error {
    io::Error::other(e.redacted())
}

impl RangedReader {
    /// Open `url` at its start; the length is known once the head arrives.
    pub fn open(url: &str) -> Result<Self, io::Error> {
        let url = Url::parse(url).map_err(io_err)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut me = Self {
            url,
            rt,
            resp: None,
            chunk: Vec::new(),
            chunk_at: 0,
            pos: 0,
            len: None,
        };
        me.connect()?;
        Ok(me)
    }

    /// The song's length in bytes, once the server has said.
    pub fn len(&self) -> Option<u64> {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == Some(0)
    }

    /// Request the body from `self.pos`.
    fn connect(&mut self) -> io::Result<()> {
        self.resp = None;
        self.chunk.clear();
        self.chunk_at = 0;
        let req = Request {
            range_from: (self.pos > 0).then_some(self.pos),
            if_range: None,
        };
        let resp = self
            .rt
            .block_on(http::get(&self.url, &req, CONNECT_TIMEOUT, READ_TIMEOUT))
            .map_err(io_err)?;
        match resp.status {
            200 if self.pos == 0 => {
                self.len = resp.content_length.or(self.len);
            }
            206 => {
                let total = resp.content_range.and_then(|r| r.total);
                self.len = total.or(self.len);
            }
            // Past the end: nothing to read, not a failure.
            416 => {
                self.resp = None;
                return Ok(());
            }
            other => {
                return Err(io::Error::other(format!(
                    "status {other} for a range from {}",
                    self.pos
                )))
            }
        }
        self.resp = Some(resp);
        Ok(())
    }
}

impl Read for RangedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(rest) = self.chunk.get(self.chunk_at..).filter(|r| !r.is_empty()) {
                let n = rest.len().min(out.len());
                out[..n].copy_from_slice(&rest[..n]);
                self.chunk_at += n;
                self.pos += n as u64;
                return Ok(n);
            }
            let Some(resp) = self.resp.as_mut() else {
                return Ok(0);
            };
            match self.rt.block_on(resp.next_chunk()).map_err(io_err)? {
                Some(next) => {
                    self.chunk = next;
                    self.chunk_at = 0;
                }
                None => {
                    self.resp = None;
                    return Ok(0);
                }
            }
        }
    }
}

impl Seek for RangedReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.len.and_then(|l| l.checked_add_signed(d)),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek outside the song"))?;
        if target == self.pos {
            return Ok(target);
        }
        let buffered = (self.chunk.len() - self.chunk_at) as u64;
        if target > self.pos && target - self.pos <= buffered.max(SKIP_LIMIT) && self.resp.is_some()
        {
            let mut skip = target - self.pos;
            let mut sink = [0u8; 8192];
            while skip > 0 {
                let want = skip.min(sink.len() as u64) as usize;
                let n = self.read(&mut sink[..want])?;
                if n == 0 {
                    break;
                }
                skip -= n as u64;
            }
            return Ok(self.pos);
        }
        self.pos = target;
        self.connect()?;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testserver::{Script, TestServer};

    fn body(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    fn server(data: &[u8]) -> TestServer {
        TestServer::start(Script::default().file("/s", data.to_vec()))
    }

    #[test]
    fn it_reads_the_whole_song_and_knows_its_length() {
        let data = body(150_000);
        let srv = server(&data);
        let mut r = RangedReader::open(&srv.url("/s")).unwrap();
        assert_eq!(r.len(), Some(150_000));
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn a_far_seek_asks_for_a_range_from_there() {
        let data = body(500_000);
        let srv = server(&data);
        let mut r = RangedReader::open(&srv.url("/s")).unwrap();
        assert_eq!(r.seek(SeekFrom::Start(300_000)).unwrap(), 300_000);
        let mut got = vec![0u8; 1000];
        r.read_exact(&mut got).unwrap();
        assert_eq!(got, data[300_000..301_000]);
        assert!(srv
            .seen()
            .iter()
            .any(|s| s.range.as_deref() == Some("bytes=300000-")));
    }

    #[test]
    fn a_short_forward_seek_reads_through_without_a_new_request() {
        let data = body(500_000);
        let srv = server(&data);
        let mut r = RangedReader::open(&srv.url("/s")).unwrap();
        r.seek(SeekFrom::Current(10_000)).unwrap();
        let mut got = [0u8; 4];
        r.read_exact(&mut got).unwrap();
        assert_eq!(got, data[10_000..10_004]);
        assert_eq!(srv.seen().len(), 1);
    }

    #[test]
    fn seeking_from_the_end_uses_the_length_and_going_back_reconnects() {
        let data = body(200_000);
        let srv = server(&data);
        let mut r = RangedReader::open(&srv.url("/s")).unwrap();
        assert_eq!(r.seek(SeekFrom::End(-100)).unwrap(), 199_900);
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, data[199_900..]);
        r.seek(SeekFrom::Start(5)).unwrap();
        let mut head = [0u8; 3];
        r.read_exact(&mut head).unwrap();
        assert_eq!(head, data[5..8]);
    }

    #[test]
    fn a_missing_song_is_an_error_naming_the_status() {
        let srv = server(&body(10));
        let e = RangedReader::open(&srv.url("/nope")).err().unwrap();
        assert!(e.to_string().contains("status 404"), "{e}");
    }
}
