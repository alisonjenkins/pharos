//! 004-books (T063) — where an extracted book cover's bytes land.
//!
//! Every other artwork source pharos knows about is already a file: a
//! `poster.jpg` beside the media, or an image a provider downloaded into the
//! cache. A book's cover is neither — it lives INSIDE the book file, so there
//! is no path for `set_artwork` to record until the bytes have been written
//! somewhere.
//!
//! That somewhere is the existing on-disk image cache, which means a book's
//! cover is stored, scaled per requested width, ETagged and served by exactly
//! the same machinery as a movie poster. Nothing in the image route needs to
//! know a book is different.
//!
//! The scanner cannot reach the cache directly — `pharos-scanner` does not
//! depend on `pharos-cache`, deliberately, so its domain logic stays testable
//! with no cache and no ffmpeg (V12). So the destination arrives as a
//! [`CoverSink`] the server implements here.

use std::path::PathBuf;

use pharos_cache::image_cache::{ImageCache, ImageRole};
use pharos_core::{MediaId, MediaKind};
use pharos_scanner::fs::CoverSink;

/// Writes book covers into the server's image cache.
pub struct ImageCacheCoverSink {
    cache: ImageCache,
}

impl ImageCacheCoverSink {
    pub fn new(cache: ImageCache) -> Self {
        Self { cache }
    }
}

impl CoverSink for ImageCacheCoverSink {
    fn store_cover<'a>(
        &'a self,
        item_id: MediaId,
        bytes: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<PathBuf>> + Send + 'a>>
    {
        Box::pin(async move {
            // `upload` is the same atomic tmp-write-then-rename path a manual
            // `POST /Items/{id}/Images/Primary` takes, and it drops the
            // healed-blank marker — so the cover is never mistaken for a failed
            // frame extraction and regenerated over (which for a book would
            // mean regenerated into nothing).
            self.cache
                .upload(item_id, ImageRole::Primary, MediaKind::Book, 0, &bytes)
                .await
                .map_err(std::io::Error::other)
        })
    }
}
