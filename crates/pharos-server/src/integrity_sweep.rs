//! The integrity sweep's admin-facing half: what is in scope, which reader
//! each kind goes to, how a book's verdict is phrased, and the re-check queue.
//!
//! Split from [`crate::integrity_backfill`] because that module needs the
//! libav worker pool and only builds with `ffmpeg-lib` on unix, while the
//! admin routes that read this state build everywhere.

use pharos_core::{MediaItem, MediaKind};
use pharos_transcode::integrity::IntegrityReport;
use std::collections::HashSet;
use std::sync::Mutex;
use tokio::sync::Notify;

/// The sweep's shared, admin-facing state: files waiting to be re-read and
/// files no reader here can verify.
#[derive(Debug, Default)]
pub struct IntegritySweep {
    /// In request order. An id leaves only once its new verdict is recorded,
    /// so "queued" stays true for the whole wait.
    queued: Mutex<Vec<u64>>,
    wake: Notify,
    not_checkable: Mutex<HashSet<u64>>,
}

impl IntegritySweep {
    /// Queue `id` to be read again and wake the sweep. `false` when it was
    /// already queued.
    pub fn request(&self, id: u64) -> bool {
        let added = self
            .queued
            .lock()
            .map(|mut q| {
                if q.contains(&id) {
                    false
                } else {
                    q.push(id);
                    true
                }
            })
            .unwrap_or(false);
        if added {
            self.wake.notify_one();
        }
        added
    }

    /// Resolves when a re-check is requested. Keeps a wake-up that lands
    /// while nobody is waiting, so a request made mid-pass is not lost.
    pub async fn woken(&self) {
        self.wake.notified().await;
    }

    pub fn is_queued(&self, id: u64) -> bool {
        self.queued.lock().map(|q| q.contains(&id)).unwrap_or(false)
    }

    pub fn queued_len(&self) -> usize {
        self.queued.lock().map(|q| q.len()).unwrap_or(0)
    }

    pub fn queued_ids(&self) -> Vec<u64> {
        self.queued.lock().map(|q| q.clone()).unwrap_or_default()
    }

    pub fn done(&self, id: u64) {
        if let Ok(mut q) = self.queued.lock() {
            q.retain(|q| *q != id);
        }
    }

    /// Whether this file was found to be in a format nothing here can verify.
    pub fn is_not_checkable(&self, id: u64) -> bool {
        self.not_checkable
            .lock()
            .map(|s| s.contains(&id))
            .unwrap_or(false)
    }

    pub fn mark_not_checkable(&self, id: u64) {
        if let Ok(mut s) = self.not_checkable.lock() {
            s.insert(id);
        }
    }
}

/// Whether this item can be read end to end at all.
///
/// Remote origins (008) have a synthetic path that is not a file, so opening it
/// is ENOENT. That failure would not be self-limiting: the memo keys on a
/// `stat` that never succeeds, so nothing is ever recorded and the item is
/// retried on every pass, each attempt taking a background-I/O permit away
/// from live playback (V134).
pub fn scannable(it: &MediaItem) -> bool {
    // Every kind is in scope, books included. A book must never reach the
    // libav demux (004-books FR-001): it fails there on every pass, and a
    // failure is deliberately not memoised, so each ebook once cost a
    // background-I/O permit and a WARN every pass forever — 19 002 lines in
    // the week to 2026-09-06. `scan_one` sends books to their own reader.
    match it.kind {
        MediaKind::Movie | MediaKind::Episode | MediaKind::Audio | MediaKind::Book => {
            it.origin().local().is_some()
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reader {
    Libav,
    Book,
}

/// Which reader a kind goes to. A book never reaches libav (FR-001).
pub fn reader_for(kind: MediaKind) -> Reader {
    match kind {
        MediaKind::Movie | MediaKind::Episode | MediaKind::Audio => Reader::Libav,
        MediaKind::Book => Reader::Book,
    }
}

/// A book's verdict in the shape every other file's takes. Parts become
/// packets and broken parts read errors, so `label()` and `is_damaged()` mean
/// the same thing for a book as for a film.
pub fn book_report(b: pharos_scanner::book::integrity::BookIntegrity) -> IntegrityReport {
    IntegrityReport {
        packets: b.parts,
        read_errors: b.faults,
        first_fault: b.first_fault,
        complete: b.complete,
        ..Default::default()
    }
}

/// The pass order: files an admin asked to re-check first, then the library.
/// The bool marks a requested file, which is read whatever the memo says.
pub fn pass_order<'a>(items: &'a [MediaItem], queued: &[u64]) -> Vec<(&'a MediaItem, bool)> {
    let wanted: HashSet<u64> = queued.iter().copied().collect();
    let mut first: Vec<(&MediaItem, bool)> = queued
        .iter()
        .filter_map(|id| items.iter().find(|it| it.id == *id))
        .map(|it| (it, true))
        .collect();
    first.extend(
        items
            .iter()
            .filter(|it| !wanted.contains(&it.id))
            .map(|it| (it, false)),
    );
    first
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn local(path: &str) -> MediaItem {
        MediaItem {
            id: 1,
            kind: MediaKind::Episode,
            path: path.into(),
            title: "ep".into(),
            ..Default::default()
        }
    }

    /// Every kind of media is checked, but a book has no A/V stream: it must
    /// go to the book reader, never the libav demux, where it failed on every
    /// pass forever (19 002 WARN lines in the week to 2026-09-06).
    #[test]
    fn every_kind_is_checked_and_a_book_never_reaches_libav() {
        for kind in [
            MediaKind::Movie,
            MediaKind::Episode,
            MediaKind::Audio,
            MediaKind::Book,
        ] {
            let mut it = local("/lib/file");
            it.kind = kind;
            assert!(scannable(&it), "{kind:?} is in scope");
        }
        assert_eq!(reader_for(MediaKind::Book), Reader::Book);
        assert_eq!(reader_for(MediaKind::Audio), Reader::Libav);
        assert_eq!(reader_for(MediaKind::Movie), Reader::Libav);
        assert_eq!(reader_for(MediaKind::Episode), Reader::Libav);
    }

    /// A book's verdict reads the same as a film's: one broken entry is
    /// "damaged", an archive whose index will not open is "unreadable".
    #[test]
    fn a_book_verdict_uses_the_same_words_as_a_film() {
        use pharos_scanner::book::integrity::BookIntegrity;
        let clean = book_report(BookIntegrity {
            parts: 12,
            complete: true,
            ..Default::default()
        });
        assert_eq!(clean.label(), "clean");
        let damaged = book_report(BookIntegrity {
            parts: 11,
            faults: 1,
            first_fault: Some("OEBPS/ch3.xhtml: Invalid checksum".into()),
            complete: true,
        });
        assert_eq!(damaged.label(), "damaged");
        assert_eq!(
            damaged.first_fault.as_deref(),
            Some("OEBPS/ch3.xhtml: Invalid checksum")
        );
        let unreadable = book_report(BookIntegrity {
            faults: 1,
            first_fault: Some("zip directory: invalid".into()),
            ..Default::default()
        });
        assert_eq!(unreadable.label(), "unreadable");
    }

    #[test]
    fn a_requested_file_goes_first_and_only_once() {
        let items: Vec<MediaItem> = (1..=4)
            .map(|id| MediaItem {
                id,
                ..local("/tv/x.mkv")
            })
            .collect();
        let order: Vec<(u64, bool)> = pass_order(&items, &[3, 99])
            .into_iter()
            .map(|(it, req)| (it.id, req))
            .collect();
        assert_eq!(order, vec![(3, true), (1, false), (2, false), (4, false)]);
    }

    #[test]
    fn a_request_is_queued_once_and_cleared_when_read() {
        let sweep = IntegritySweep::default();
        assert!(sweep.request(5));
        assert!(!sweep.request(5), "already waiting");
        assert!(sweep.is_queued(5));
        assert_eq!(sweep.queued_len(), 1);
        sweep.done(5);
        assert!(!sweep.is_queued(5));
    }

    /// A request must wake a sweep that is asleep between passes, or "re-check"
    /// means "within the hour".
    #[tokio::test]
    async fn a_request_wakes_the_sleeping_sweep() {
        let sweep = Arc::new(IntegritySweep::default());
        let s = sweep.clone();
        let waiter = tokio::spawn(async move { s.woken().await });
        tokio::task::yield_now().await;
        sweep.request(1);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the request must wake the sweep")
            .expect("the waiter must not panic");
    }

    #[test]
    fn a_remote_origin_is_never_read_from_disk() {
        assert!(scannable(&local("/tv/Show/s01e01.mkv")));
        assert!(
            !scannable(&local("ytdlp://youtube/dQw4w9WgXcQ")),
            "a synthetic path is not a file; scanning it can only ever fail"
        );
    }
}
