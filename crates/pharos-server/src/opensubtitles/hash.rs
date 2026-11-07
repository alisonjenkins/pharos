//! The OpenSubtitles file hash (V562).
//!
//! `size + sum(first 64 KiB as LE u64 words) + sum(last 64 KiB as LE u64
//! words)`, wrapping at 64 bits, written as 16 lowercase hex digits. A file
//! under 128 KiB has no hash: the two windows would overlap or run short, and
//! OpenSubtitles does not index such files by hash.

use std::io::SeekFrom;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const WINDOW: usize = 64 * 1024;

/// Smallest file the hash is defined for (two full windows).
pub const MIN_HASHABLE_SIZE: u64 = (WINDOW as u64) * 2;

fn window_sum(window: &[u8]) -> u64 {
    window
        .chunks_exact(8)
        .map(|w| {
            let mut b = [0u8; 8];
            b.copy_from_slice(w);
            u64::from_le_bytes(b)
        })
        .fold(0u64, u64::wrapping_add)
}

/// Hash from the file size and its two 64 KiB windows. `None` when either
/// window is not exactly 64 KiB or the size is under [`MIN_HASHABLE_SIZE`].
pub fn moviehash_from_parts(size: u64, head: &[u8], tail: &[u8]) -> Option<String> {
    if size < MIN_HASHABLE_SIZE || head.len() != WINDOW || tail.len() != WINDOW {
        return None;
    }
    let sum = size
        .wrapping_add(window_sum(head))
        .wrapping_add(window_sum(tail));
    Some(format!("{sum:016x}"))
}

/// Hash of the file at `path`: two 64 KiB reads, so cheap over NFS. `Ok(None)`
/// for a file under [`MIN_HASHABLE_SIZE`].
pub async fn moviehash(path: &Path) -> std::io::Result<Option<String>> {
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    if size < MIN_HASHABLE_SIZE {
        return Ok(None);
    }
    let mut head = vec![0u8; WINDOW];
    file.read_exact(&mut head).await?;
    file.seek(SeekFrom::Start(size.saturating_sub(WINDOW as u64)))
        .await?;
    let mut tail = vec![0u8; WINDOW];
    file.read_exact(&mut tail).await?;
    Ok(moviehash_from_parts(size, &head, &tail))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Values computed independently with a Python reference of the published
    /// algorithm (`struct.iter_unpack('<Q')` over both windows).
    #[test]
    fn matches_reference_vectors() {
        let counting: Vec<u8> = (0..200_000usize).map(|i| (i % 251) as u8).collect();
        let (head, tail) = (&counting[..WINDOW], &counting[counting.len() - WINDOW..]);
        assert_eq!(
            moviehash_from_parts(200_000, head, tail).as_deref(),
            Some("e19d5212c9812cd6")
        );
        let exact: Vec<u8> = (0..131_072usize).map(|i| ((i * 7) % 256) as u8).collect();
        assert_eq!(
            moviehash_from_parts(131_072, &exact[..WINDOW], &exact[WINDOW..]).as_deref(),
            Some("9fe02060a0e10000")
        );
    }

    #[test]
    fn zero_filled_file_hashes_to_its_size() {
        let zeros = vec![0u8; WINDOW];
        assert_eq!(
            moviehash_from_parts(0x20000, &zeros, &zeros).as_deref(),
            Some("0000000000020000")
        );
    }

    #[test]
    fn sum_wraps_instead_of_overflowing() {
        let ones = vec![0xFFu8; WINDOW];
        let h = moviehash_from_parts(u64::MAX, &ones, &ones);
        assert!(h.is_some());
    }

    #[test]
    fn short_file_has_no_hash() {
        let w = vec![0u8; WINDOW];
        assert_eq!(moviehash_from_parts(MIN_HASHABLE_SIZE - 1, &w, &w), None);
    }

    #[tokio::test]
    async fn file_hash_equals_pure_function() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..200_000usize).map(|i| (i % 251) as u8).collect();
        let p = dir.path().join("movie.mkv");
        tokio::fs::write(&p, &data).await.unwrap();
        assert_eq!(
            moviehash(&p).await.unwrap().as_deref(),
            Some("e19d5212c9812cd6")
        );
        let small = dir.path().join("small.mkv");
        tokio::fs::write(&small, vec![1u8; 1000]).await.unwrap();
        assert_eq!(moviehash(&small).await.unwrap(), None);
    }
}
