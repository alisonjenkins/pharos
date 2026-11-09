//! The integrity sweep's admin-facing half: what is in scope, which reader
//! each kind goes to, how a book's verdict is phrased, and the re-check queue.
//!
//! Split from [`crate::integrity_backfill`] because that module needs the
//! libav worker pool and only builds with `ffmpeg-lib` on unix, while the
//! admin routes that read this state build everywhere.

use pharos_core::{MediaItem, MediaKind};
use pharos_transcode::integrity::IntegrityReport;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tokio::sync::Notify;

/// The sweep's shared, admin-facing state: files waiting to be re-read and
/// files no reader here can verify.
#[derive(Debug, Default)]
pub struct IntegritySweep {
    /// In request order. An id leaves only once its new verdict is recorded,
    /// so "queued" stays true for the whole wait. The flag marks an id asked
    /// for again while already queued: the read under way may have started
    /// before the file was replaced, so the id must outlive one `done`.
    queued: Mutex<Vec<(u64, bool)>>,
    wake: Notify,
    /// Keyed to the file's `(mtime, size)`, so a replaced file is tried again.
    not_checkable: Mutex<HashMap<u64, Option<(i64, u64)>>>,
}

impl IntegritySweep {
    /// Queue `id` to be read again and wake the sweep. `false` when it was
    /// already queued; it is then read once more after the read it is
    /// waiting on.
    pub fn request(&self, id: u64) -> bool {
        let added = self
            .queued
            .lock()
            .map(|mut q| {
                if let Some(entry) = q.iter_mut().find(|(q, _)| *q == id) {
                    entry.1 = true;
                    false
                } else {
                    q.push((id, false));
                    true
                }
            })
            .unwrap_or(false);
        self.wake.notify_one();
        added
    }

    /// Resolves when a re-check is requested. Keeps a wake-up that lands
    /// while nobody is waiting, so a request made mid-pass is not lost.
    pub async fn woken(&self) {
        self.wake.notified().await;
    }

    pub fn is_queued(&self, id: u64) -> bool {
        self.queued
            .lock()
            .map(|q| q.iter().any(|(q, _)| *q == id))
            .unwrap_or(false)
    }

    pub fn queued_len(&self) -> usize {
        self.queued.lock().map(|q| q.len()).unwrap_or(0)
    }

    pub fn queued_ids(&self) -> Vec<u64> {
        self.queued
            .lock()
            .map(|q| q.iter().map(|(id, _)| *id).collect())
            .unwrap_or_default()
    }

    /// The read this request waited on is in. An id asked for again during
    /// that read stays queued, once, for the next pass.
    pub fn done(&self, id: u64) {
        if let Ok(mut q) = self.queued.lock() {
            if let Some(pos) = q.iter().position(|(q, _)| *q == id) {
                if let Some(entry) = q.get_mut(pos).filter(|e| e.1) {
                    entry.1 = false;
                } else {
                    q.remove(pos);
                }
            }
        }
    }

    /// Whether this file, in this state, was found to be in a format nothing
    /// here can verify. An unstattable file never matches, as in the memo.
    pub fn is_not_checkable(&self, id: u64, sig: Option<(i64, u64)>) -> bool {
        sig.is_some()
            && self
                .not_checkable
                .lock()
                .map(|s| s.get(&id) == Some(&sig))
                .unwrap_or(false)
    }

    /// Whether any not-checkable finding is held for this id, whatever state
    /// the file was in. For progress counts, where a `stat` per file is too dear.
    pub fn has_not_checkable(&self, id: u64) -> bool {
        self.not_checkable
            .lock()
            .map(|s| s.contains_key(&id))
            .unwrap_or(false)
    }

    pub fn mark_not_checkable(&self, id: u64, sig: Option<(i64, u64)>) {
        if let Ok(mut s) = self.not_checkable.lock() {
            s.insert(id, sig);
        }
    }

    fn clear_not_checkable(&self, id: u64) {
        if let Ok(mut s) = self.not_checkable.lock() {
            s.remove(&id);
        }
    }

    /// Record what a read found, in exactly one place: a verdict replaces any
    /// earlier not-checkable finding, and a not-checkable finding drops any
    /// earlier verdict, so a file is never listed as both, and a re-check
    /// whose answer is "cannot verify" does not leave a stale "damaged" behind.
    pub fn settle(
        &self,
        memo: &crate::integrity_memo::IntegrityMemo,
        id: u64,
        sig: Option<(i64, u64)>,
        found: Found,
    ) {
        match found {
            Found::Report(report) => {
                self.clear_not_checkable(id);
                memo.record(id, sig, report);
            }
            Found::NotCheckable => {
                memo.forget(id);
                self.mark_not_checkable(id, sig);
            }
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
        // A book has no timeline to mark; empty says the verdict is current.
        ranges: Some(Vec::new()),
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

/// What a read found, as [`IntegritySweep::settle`] takes it.
#[derive(Debug)]
pub enum Found {
    Report(IntegrityReport),
    NotCheckable,
}

/// Count one read's result. Labels are a dashboard contract (bounded, asserted
/// in a test): `verdict` is a `label()` word, `not_checkable` or `error`, and
/// `kind` a `MediaKind::as_str`.
pub fn count_verdict(verdict: &'static str, kind: MediaKind) {
    metrics::counter!(
        "pharos_media_integrity_total",
        "verdict" => verdict,
        "kind" => kind.as_str(),
    )
    .increment(1);
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

    /// A second click after replacing a file, while the first re-check's
    /// read is under way, must get its own read: the one under way may have
    /// opened the old bytes.
    #[test]
    fn a_request_made_during_the_read_gets_a_read_of_its_own() {
        let sweep = IntegritySweep::default();
        assert!(sweep.request(9));
        assert!(!sweep.request(9), "already queued, so asked again");
        sweep.done(9);
        assert!(sweep.is_queued(9), "the second ask is still owed a read");
        sweep.done(9);
        assert!(!sweep.is_queued(9));
    }

    #[test]
    fn a_request_is_queued_once_and_cleared_when_read() {
        let sweep = IntegritySweep::default();
        assert!(sweep.request(5));
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

    fn report(read_errors: u64) -> IntegrityReport {
        IntegrityReport {
            read_errors,
            complete: true,
            ..Default::default()
        }
    }

    /// A re-check that comes back "cannot verify" must not leave the old
    /// "damaged" verdict listed, and a later real verdict must clear the
    /// not-checkable finding: a file is never both.
    #[test]
    fn a_file_is_never_both_damaged_and_not_checkable() {
        let memo = crate::integrity_memo::IntegrityMemo::load("/nonexistent/m.json", 1);
        let sweep = IntegritySweep::default();
        let sig = Some((1, 2));
        sweep.settle(&memo, 9, sig, Found::Report(report(1)));
        assert_eq!(memo.damaged().len(), 1);
        sweep.settle(&memo, 9, sig, Found::NotCheckable);
        assert!(memo.damaged().is_empty(), "the stale verdict is gone");
        assert!(sweep.is_not_checkable(9, sig));
        sweep.settle(&memo, 9, sig, Found::Report(report(0)));
        assert!(!sweep.has_not_checkable(9));
        assert_eq!(memo.recorded_damaged(9), Some(false));
    }

    #[test]
    fn a_replaced_file_is_tried_again_after_not_checkable() {
        let sweep = IntegritySweep::default();
        sweep.mark_not_checkable(3, Some((1, 2)));
        assert!(sweep.is_not_checkable(3, Some((1, 2))));
        assert!(!sweep.is_not_checkable(3, Some((5, 2))), "new mtime");
        assert!(
            !sweep.is_not_checkable(3, None),
            "unstattable never matches"
        );
        assert!(sweep.has_not_checkable(3));
    }

    /// The signal B899 names is `sum by (kind, verdict)`; renaming a label
    /// breaks that query silently, so the labels are pinned here.
    #[test]
    fn a_verdict_is_counted_by_verdict_and_kind() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        count_verdict("not_checkable", MediaKind::Book);
        count_verdict(report(1).label(), MediaKind::Episode);
        let seen: Vec<(String, String)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == "pharos_media_integrity_total")
            .map(|(k, ..)| {
                let label = |n: &str| {
                    k.key()
                        .labels()
                        .find(|l| l.key() == n)
                        .map(|l| l.value().to_string())
                        .unwrap_or_default()
                };
                (label("verdict"), label("kind"))
            })
            .collect();
        for want in [("not_checkable", "book"), ("damaged", "episode")] {
            assert!(
                seen.contains(&(want.0.to_string(), want.1.to_string())),
                "missing {want:?} in {seen:?}"
            );
        }
    }
}
