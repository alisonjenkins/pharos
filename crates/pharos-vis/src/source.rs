//! Any `Read + Seek` as a decoder source, such as a cached file or a ranged
//! stream.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Mutex;

use symphonia::core::io::MediaSource;

/// `Mutex` only makes a `Send` reader `Sync`, as the decoder's source needs;
/// the decoder is the sole user.
pub struct ReadSeekSource<T> {
    inner: Mutex<T>,
    len: Option<u64>,
}

impl<T: Read + Seek + Send> ReadSeekSource<T> {
    pub fn new(inner: T, len: Option<u64>) -> Self {
        Self {
            inner: Mutex::new(inner),
            len,
        }
    }

    fn get(&mut self) -> &mut T {
        self.inner
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<T: Read + Seek + Send> Read for ReadSeekSource<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.get().read(buf)
    }
}

impl<T: Read + Seek + Send> Seek for ReadSeekSource<T> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.get().seek(to)
    }
}

impl<T: Read + Seek + Send> MediaSource for ReadSeekSource<T> {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::decode::{Decoder, Step};
    use crate::fixture;

    #[test]
    fn a_seekable_reader_decodes_like_a_file() {
        let src = fixture::sine(440.0, 8000, 0.5, 0.5);
        let bytes = fixture::wav(&src, 8000, 1);
        let len = bytes.len() as u64;
        let source = ReadSeekSource::new(Cursor::new(bytes), Some(len));
        let mut d = Decoder::open(Box::new(source), Some("wav")).unwrap();
        let mut out = Vec::new();
        while let Ok(Step::Samples(_)) = d.read(&mut out) {}
        assert_eq!(out.len(), src.len());
    }
}
