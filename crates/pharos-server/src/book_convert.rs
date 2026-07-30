//! 005-kindle-conversion — where a converted Kindle book's EPUB bytes live.
//!
//! # Why delivery converts rather than the scan
//!
//! A scan reads a Kindle file for its metadata and cover, which are the parts
//! an item row persists. It deliberately does NOT keep the EPUB it could have
//! produced on the way past: the media share is mounted **read-only**, so the
//! bytes cannot go beside the source, and writing every book's EPUB into the
//! cache PVC at scan time would materialise output for books nobody opens.
//!
//! So conversion happens on the download that needs it, and the result is
//! cached. It costs 8–62 ms on the deployed library's 17 convertible files —
//! below the threshold where a reader notices an open — and only once per book
//! per cache lifetime.
//!
//! That also makes the cache **self-healing**, which the stored-flag design it
//! replaced was not: wipe the cache PVC and the next request rebuilds, with no
//! rescan and no row to correct. Disk is the authority on whether the bytes
//! exist, exactly as it is for trickplay.

use std::path::{Path, PathBuf};

use pharos_core::MediaId;
use pharos_scanner::book::kindle::{self, ConvertOutcome, ConvertReason, ConvertStage};

/// Converted EPUBs on disk, one file per item.
#[derive(Debug, Clone)]
pub struct ConvertedBookCache {
    dir: PathBuf,
}

impl ConvertedBookCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Where item `id`'s converted EPUB lives.
    ///
    /// Keyed on the media id, not the source path: a path can contain any byte
    /// the filesystem allows, and building a cache filename out of one is how
    /// a traversal gets in. The id is an integer pharos issued.
    pub fn path_for(&self, id: MediaId) -> PathBuf {
        self.dir.join(format!("{id}.epub"))
    }

    /// The converted EPUB for `id`, converting `src` if it is not cached yet.
    ///
    /// # Concurrency
    ///
    /// Two simultaneous first-opens of the same book both convert, and both
    /// write. That is deliberate: the write is a tmp-file rename, so it is
    /// atomic and last-writer-wins on identical bytes, and the duplicated work
    /// is tens of milliseconds. A single-flight lock here would add a shared
    /// map and a lock lifetime to save less than one segment's decode.
    pub async fn get_or_convert(&self, id: MediaId, src: &Path) -> std::io::Result<PathBuf> {
        let dest = self.path_for(id);
        if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            return Ok(dest);
        }

        let ext = kindle::source_ext(src);
        let src_owned = src.to_path_buf();
        // Conversion is CPU-bound and fully synchronous; running it on a
        // worker thread keeps it off the async reactor. A panic inside becomes
        // a `JoinError` rather than a dead process (V119) — the same
        // containment `guard_parser` gives the scan.
        let converted =
            tokio::task::spawn_blocking(move || kindle::convert_to_epub(&src_owned)).await;

        let bytes = match converted {
            Ok(Ok(bytes)) => {
                kindle::record_convert(
                    ConvertStage::Deliver,
                    ext,
                    ConvertOutcome::Converted,
                    ConvertReason::Ok,
                );
                bytes
            }
            Ok(Err(err)) => {
                let (outcome, reason) = err.outcome();
                kindle::record_convert(ConvertStage::Deliver, ext, outcome, reason);
                tracing::warn!(
                    media.id = id,
                    path = %src.display(),
                    source = ext,
                    error = %err,
                    "kindle: conversion failed on delivery"
                );
                // Carry the cause, not the class: "conversion failed" alone
                // cannot tell DRM from a dead mount, and those have different
                // answers for whoever reads this.
                return Err(std::io::Error::other(err.to_string()));
            }
            Err(join) => {
                kindle::record_convert(
                    ConvertStage::Deliver,
                    ext,
                    ConvertOutcome::Failed,
                    ConvertReason::ParserPanic,
                );
                tracing::error!(
                    media.id = id,
                    path = %src.display(),
                    error = %join,
                    "kindle: converter PANICKED on delivery; serving nothing for this book"
                );
                return Err(std::io::Error::other(join.to_string()));
            }
        };

        tokio::fs::create_dir_all(&self.dir).await?;
        // Write-then-rename: a reader that arrives mid-write must never see a
        // truncated zip. epub.js fails opaquely on one, which would look like
        // a conversion bug rather than a half-written file.
        let tmp = self
            .dir
            .join(format!("{id}.epub.tmp{}", std::process::id()));
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &dest).await?;
        tracing::info!(
            media.id = id,
            source = ext,
            bytes = bytes.len(),
            dest = %dest.display(),
            "kindle: converted to epub and cached"
        );
        Ok(dest)
    }
}

/// The converted-EPUB path to advertise for `item`, or `None` if it is not a
/// converted Kindle book (or conversion is disabled).
///
/// Deliberately does NOT touch the disk. This is called once per item on
/// every grid render, and a `stat` per card would put filesystem latency on
/// the hot list path to answer a question the item row already implies. The
/// download route is where presence is established, and it rebuilds on a
/// miss — so a stale advertisement costs one conversion, never a broken link.
pub fn converted_book_path(
    cache: Option<&ConvertedBookCache>,
    item: &pharos_core::MediaItem,
) -> Option<PathBuf> {
    let cache = cache?;
    let book = item.book.as_ref()?;
    kindle::is_converted_kindle(&item.path, book.format).then(|| cache.path_for(item.id))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_is_keyed_on_the_id_not_the_source_path() {
        let c = ConvertedBookCache::new(PathBuf::from("/cache/books"));
        let p = c.path_for(MediaId::from(42u64));
        assert_eq!(p, PathBuf::from("/cache/books/42.epub"));
        // The source filename never reaches the cache key, so a book called
        // `../../etc/passwd.azw3` cannot escape the cache directory.
        assert_eq!(p.parent().unwrap(), Path::new("/cache/books"));
    }

    /// A source that does not exist must fail rather than cache an empty file
    /// — a zero-byte `.epub` left behind would be served forever, because the
    /// cache treats presence as success.
    #[tokio::test]
    async fn a_missing_source_caches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ConvertedBookCache::new(dir.path().join("books"));
        let id = MediaId::from(7u64);
        let err = cache
            .get_or_convert(id, Path::new("/nonexistent/book.azw3"))
            .await
            .expect_err("a missing source must not convert");
        assert!(!err.to_string().is_empty(), "the cause must be carried");
        assert!(
            !cache.path_for(id).exists(),
            "a failed conversion must leave no cached file"
        );
    }
}
