//! LIB-A6 — content fingerprint computation.
//!
//! The fingerprint is an 8-byte `xxh3_64` digest over a few cheap,
//! content-derived inputs that uniquely identify a file's bytes without
//! reading the whole thing:
//!
//! 1. the file size (little-endian `u64`),
//! 2. the probed duration in ms (little-endian `u64`, or `0` when unknown),
//! 3. up to the first `SAMPLE_BYTES` (1 MiB) of content,
//! 4. up to the last `SAMPLE_BYTES` (1 MiB) of content.
//!
//! For files smaller than `2 * SAMPLE_BYTES` the head and tail windows
//! would overlap, so we hash the whole file once instead (no double-count).
//!
//! Unlike the path-derived [`stable_id`](crate::stable_id), a fingerprint
//! survives a rename/move because it depends only on content. The scanner
//! uses it to recognise a moved file as the same item.
//!
//! The hash is **deterministic**: the same bytes + duration always yield
//! the same digest (seeded `xxh3_64`, fixed input ordering). All file IO is
//! blocking — [`fingerprint_async`] marshals it onto `spawn_blocking` so it
//! never sits on the async reactor (V5); the bare [`fingerprint`] is the
//! sync core used from blocking contexts and tests.

use pharos_core::Fingerprint;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use xxhash_rust::xxh3::Xxh3;

/// Head/tail sample window: 1 MiB each end.
const SAMPLE_BYTES: u64 = 1024 * 1024;

/// Compute the content [`Fingerprint`] for `path`. `duration_ms` is the
/// probed media duration folded into the digest (`None` → `0`). Blocking
/// file IO — call from a blocking context (see [`fingerprint_async`] for
/// the reactor-safe wrapper). Deterministic.
pub fn fingerprint(path: &Path, duration_ms: Option<u64>) -> io::Result<Fingerprint> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();

    let mut hasher = Xxh3::new();
    hasher.update(&size.to_le_bytes());
    hasher.update(&duration_ms.unwrap_or(0).to_le_bytes());

    if size <= 2 * SAMPLE_BYTES {
        // Small file: hash the whole content once (head+tail would overlap).
        let mut buf = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buf)?;
        hasher.update(&buf);
    } else {
        // Head window.
        let mut head = vec![0u8; SAMPLE_BYTES as usize];
        read_exact_from(&mut file, SeekFrom::Start(0), &mut head)?;
        hasher.update(&head);

        // Tail window — last SAMPLE_BYTES of the file.
        let mut tail = vec![0u8; SAMPLE_BYTES as usize];
        read_exact_from(&mut file, SeekFrom::Start(size - SAMPLE_BYTES), &mut tail)?;
        hasher.update(&tail);
    }

    Ok(hasher.digest().to_le_bytes())
}

/// Reactor-safe [`fingerprint`]: runs the blocking IO inside
/// `spawn_blocking` so it never parks the async runtime (V5).
pub async fn fingerprint_async(path: &Path, duration_ms: Option<u64>) -> io::Result<Fingerprint> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || fingerprint(&path, duration_ms))
        .await
        .map_err(|e| io::Error::other(format!("fingerprint task join: {e}")))?
}

/// Seek to `from` and fill `buf` exactly. Reads at a fixed offset so the
/// head and tail windows are independent of prior cursor position.
fn read_exact_from(file: &mut File, from: SeekFrom, buf: &mut [u8]) -> io::Result<()> {
    file.seek(from)?;
    file.read_exact(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::io::Write;

    fn write_tmp(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        f.sync_all().unwrap();
        p
    }

    #[test]
    fn identical_files_yield_identical_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![7u8; 4096];
        let a = write_tmp(dir.path(), "a.bin", &bytes);
        let b = write_tmp(dir.path(), "b.bin", &bytes);
        let fa = fingerprint(&a, Some(1234)).unwrap();
        let fb = fingerprint(&b, Some(1234)).unwrap();
        assert_eq!(fa, fb);
    }

    #[test]
    fn distinct_content_yields_distinct_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let mut x = vec![1u8; 4096];
        let mut y = x.clone();
        // Differ only in the first byte.
        x[0] = 0xAA;
        y[0] = 0xBB;
        let px = write_tmp(dir.path(), "x.bin", &x);
        let py = write_tmp(dir.path(), "y.bin", &y);
        assert_ne!(
            fingerprint(&px, None).unwrap(),
            fingerprint(&py, None).unwrap()
        );
    }

    #[test]
    fn distinct_tail_bytes_yield_distinct_fingerprint() {
        // A large file (> 2 MiB) so the head/tail windows are used; differ
        // only in the very last byte to prove the tail window is hashed.
        let dir = tempfile::tempdir().unwrap();
        let mut a = vec![3u8; (2 * SAMPLE_BYTES + 4096) as usize];
        let mut b = a.clone();
        *a.last_mut().unwrap() = 0x10;
        *b.last_mut().unwrap() = 0x20;
        let pa = write_tmp(dir.path(), "big_a.bin", &a);
        let pb = write_tmp(dir.path(), "big_b.bin", &b);
        assert_ne!(
            fingerprint(&pa, None).unwrap(),
            fingerprint(&pb, None).unwrap()
        );
    }

    #[test]
    fn duration_is_part_of_digest() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "d.bin", &[9u8; 1024]);
        assert_ne!(
            fingerprint(&p, Some(1000)).unwrap(),
            fingerprint(&p, Some(2000)).unwrap()
        );
    }

    #[test]
    fn small_sub_mib_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "small.bin", &[42u8; 128]);
        // Just needs to succeed and be deterministic.
        let f1 = fingerprint(&p, None).unwrap();
        let f2 = fingerprint(&p, None).unwrap();
        assert_eq!(f1, f2);
    }

    #[test]
    fn empty_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "empty.bin", &[]);
        // No panic; deterministic digest of (size=0, dur=0, no content).
        assert_eq!(
            fingerprint(&p, None).unwrap(),
            fingerprint(&p, None).unwrap()
        );
    }

    #[tokio::test]
    async fn async_matches_sync() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(dir.path(), "async.bin", &[5u8; 8192]);
        let sync = fingerprint(&p, Some(77)).unwrap();
        let asy = fingerprint_async(&p, Some(77)).await.unwrap();
        assert_eq!(sync, asy);
    }
}
