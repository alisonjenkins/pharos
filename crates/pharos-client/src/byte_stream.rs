//! A capnp byte stream over a message transport (017 T049).
//!
//! capnp frames its own messages, so the server treats the WebSocket's
//! binary messages as one byte stream (`rpc/socket.rs`). [`ChunkReader`]
//! turns a stream of received messages into `AsyncRead`; [`ChunkWriter`]
//! sends each write as one message. Both are transport-neutral so they can
//! be tested without a browser.

use futures::io::{AsyncRead, AsyncWrite};
use futures::Stream;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Reads the concatenation of the byte chunks `S` yields. The stream ending
/// reads as EOF.
pub struct ChunkReader<S> {
    chunks: S,
    pending: Vec<u8>,
    offset: usize,
}

impl<S> ChunkReader<S> {
    pub fn new(chunks: S) -> Self {
        Self {
            chunks,
            pending: Vec::new(),
            offset: 0,
        }
    }
}

impl<S> AsyncRead for ChunkReader<S>
where
    S: Stream<Item = Vec<u8>> + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        while self.offset == self.pending.len() {
            match Pin::new(&mut self.chunks).poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    self.pending = chunk;
                    self.offset = 0;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(0)),
                Poll::Pending => return Poll::Pending,
            }
        }
        let this = &mut *self;
        let n = buf.len().min(this.pending.len() - this.offset);
        buf[..n].copy_from_slice(&this.pending[this.offset..this.offset + n]);
        this.offset += n;
        Poll::Ready(Ok(n))
    }
}

/// Sends each write through `send` as one message. The transport buffers,
/// so flush and close only report a send that already failed.
pub struct ChunkWriter<F> {
    send: F,
}

impl<F> ChunkWriter<F> {
    pub fn new(send: F) -> Self {
        Self { send }
    }
}

impl<F> AsyncWrite for ChunkWriter<F>
where
    F: FnMut(&[u8]) -> io::Result<()> + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready((self.send)(buf).map(|()| buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use futures::executor::block_on;
    use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// Message boundaries carry nothing: reads see one continuous stream
    /// whatever the chunking, across empty chunks, and EOF at the end.
    #[test]
    fn chunks_read_as_one_stream() {
        let chunks = vec![b"ab".to_vec(), Vec::new(), b"cdefg".to_vec(), b"h".to_vec()];
        let mut reader = ChunkReader::new(futures::stream::iter(chunks));
        let mut small = [0u8; 3];
        let mut got = Vec::new();
        block_on(async {
            loop {
                let n = reader.read(&mut small).await.unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&small[..n]);
            }
        });
        assert_eq!(got, b"abcdefgh");
    }

    #[test]
    fn each_write_is_one_message_and_a_send_failure_surfaces() {
        let mut sent = Vec::new();
        block_on(async {
            let mut writer = ChunkWriter::new(|b: &[u8]| {
                sent.push(b.to_vec());
                Ok(())
            });
            writer.write_all(b"one").await.unwrap();
            writer.write_all(b"two").await.unwrap();
        });
        assert_eq!(sent, [b"one".to_vec(), b"two".to_vec()]);

        let mut failing = ChunkWriter::new(|_: &[u8]| {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "socket closed"))
        });
        let err = block_on(failing.write_all(b"x")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
