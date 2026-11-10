//! Remembering what a container-integrity scan found, so a library is read
//! once rather than once per pass.
//!
//! The scan behind this memo demuxes a whole file. That is the only way to
//! find damage at minute forty — a head-only sample answers "clean" for a file
//! that breaks two thirds of the way through, and a health check whose
//! confident pass is wrong is worse than no health check. But a whole-file read
//! per pass, over NFS, against a multi-terabyte library, is not something that
//! can happen repeatedly: the cost has to be paid once per file and then never
//! again until the file changes.
//!
//! That is what this is. Every verdict is recorded, clean ones included —
//! unlike `pharos_scanner::probe_memo`, which only records failures because a success
//! there already leaves a database row behind. Here a success leaves no trace
//! anywhere else, so recording only failures would re-read every healthy file
//! in the library on every pass, which is the entire cost and none of the
//! benefit.
//!
//! # Why a file beside the cache, not a table
//!
//! Same reasoning as `pharos_scanner::probe_memo`: this is a measurement, not domain
//! state. Losing it costs one slow re-scan. A table would mean a migration and
//! six `MediaStore` implementations to persist something whose worst-case loss
//! is exactly the behaviour that existed before it.
//!
//! # What invalidates an entry
//!
//! The file's own `(mtime, size)` — replace a damaged file with a good copy and
//! it is re-scanned — and [`INTEGRITY_SCAN_VERSION`]. The version exists for the
//! same reason `PROBE_SCHEMA_VERSION` does one layer up: a change to what the
//! scan *counts* can be a bug fix, and a memo that outlived the fix would hide
//! the very files the fix exists to catch. It is deliberately separate from
//! `PROBE_SCHEMA_VERSION`, so tightening the damage rules does not force the
//! whole library to be re-probed as well.

use pharos_transcode::integrity::IntegrityReport;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Bumping this discards every recorded verdict and re-scans the library.
///
/// Bump it whenever [`pharos_transcode::libav::integrity`] changes what counts
/// as damage — a new signal, a widened threshold, a fixed miss. Not bumping it
/// after such a change means the files the change was written for keep their
/// stale "clean".
pub const INTEGRITY_SCAN_VERSION: i64 = 1;

/// One file's recorded verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub mtime: i64,
    pub size: u64,
    /// The full report, not a boolean. An operator triaging a library needs to
    /// know where the damage starts and what the demuxer called it, and this
    /// file is the only place that survives the scan.
    pub report: IntegrityReport,
    /// When the scan ran, in unix seconds. Absent on entries written before
    /// this field existed; those stay valid rather than forcing a re-read.
    #[serde(default)]
    pub checked_at: Option<i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Document {
    integrity_scan_version: i64,
    /// Keyed by `stable_id(path)`, so a rename re-scans — correctly: the new
    /// path has never been looked at.
    entries: HashMap<u64, Entry>,
}

/// A persisted map of "this exact file, in this exact state, was read end to
/// end and here is what was found".
#[derive(Debug)]
pub struct IntegrityMemo {
    path: PathBuf,
    doc: RwLock<Document>,
}

impl IntegrityMemo {
    /// Load the memo at `path`, or start empty if it is absent, unreadable or
    /// written under a different scan version.
    ///
    /// Every failure to load is an empty memo rather than an error: the worst
    /// consequence is one slow sweep, and refusing to run because a cache file
    /// is corrupt would be a far worse trade.
    pub fn load(path: impl Into<PathBuf>, scan_version: i64) -> Self {
        let path = path.into();
        let doc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Document>(&s).ok())
            .filter(|d| d.integrity_scan_version == scan_version)
            .unwrap_or(Document {
                integrity_scan_version: scan_version,
                entries: HashMap::new(),
            });
        Self {
            path,
            doc: RwLock::new(doc),
        }
    }

    /// The recorded verdict for this file in this exact state, if any.
    ///
    /// `None` for mtime/size — an unstattable file — is never a hit: the stat
    /// failing is itself new information, and treating it as "already scanned"
    /// would let one transient storage fault write a file off unexamined.
    pub fn get(&self, id: u64, sig: Option<(i64, u64)>) -> Option<Entry> {
        let (mtime, size) = sig?;
        let doc = self.doc.read().ok()?;
        doc.entries
            .get(&id)
            .filter(|e| e.mtime == mtime && e.size == size)
            .cloned()
    }

    /// Whether this file, in this state, has already been read end to end.
    ///
    /// A damaged verdict recorded before damage spans existed does not count:
    /// players mark those spans, and the few damaged files are cheap to read
    /// again, unlike bumping [`INTEGRITY_SCAN_VERSION`] for the whole library.
    pub fn is_scanned(&self, id: u64, sig: Option<(i64, u64)>) -> bool {
        self.get(id, sig)
            .is_some_and(|e| !e.report.is_damaged() || e.report.ranges.is_some())
    }

    /// Record a verdict.
    pub fn record(&self, id: u64, sig: Option<(i64, u64)>, report: IntegrityReport) {
        let Some((mtime, size)) = sig else {
            // Nothing to key on, so nothing to remember. Re-scanning an
            // unstattable file next pass is right: it may be back.
            return;
        };
        if let Ok(mut d) = self.doc.write() {
            d.entries.insert(
                id,
                Entry {
                    mtime,
                    size,
                    report,
                    checked_at: now_secs(),
                },
            );
        }
    }

    /// Drop this file's verdict, if any. Persisted at the sweep's next save.
    pub fn forget(&self, id: u64) {
        if let Ok(mut d) = self.doc.write() {
            d.entries.remove(&id);
        }
    }

    /// The recorded verdict's damaged flag for this id, whatever state the
    /// file was in; `None` when there is no verdict. For progress counts.
    pub fn recorded_damaged(&self, id: u64) -> Option<bool> {
        self.doc
            .read()
            .ok()?
            .entries
            .get(&id)
            .map(|e| e.report.is_damaged())
    }

    /// Every file currently recorded as damaged, newest verdict wins. This is
    /// the answer to "what in my library is rotting" — the question the whole
    /// sweep exists to answer, and one nobody can ask of a counter.
    pub fn damaged(&self) -> Vec<(u64, Entry)> {
        let Ok(d) = self.doc.read() else {
            return Vec::new();
        };
        d.entries
            .iter()
            .filter(|(_, e)| e.report.is_damaged())
            .map(|(id, e)| (*id, e.clone()))
            .collect()
    }

    /// The ids of every file whose damage can break playback, for the
    /// library's "Damaged" badges. A file with no seek index plays through, so
    /// it is listed for admins (`damaged`) but not badged (B1162).
    pub fn damaged_ids(&self) -> std::collections::HashSet<u64> {
        let Ok(d) = self.doc.read() else {
            return Default::default();
        };
        d.entries
            .iter()
            .filter(|(_, e)| e.report.breaks_playback())
            .map(|(id, _)| *id)
            .collect()
    }

    /// This file's recorded verdict, whatever state the file was in when it
    /// was read: like the damaged list, it stands until the sweep reads the
    /// file again.
    pub fn verdict(&self, id: u64) -> Option<Entry> {
        self.doc.read().ok()?.entries.get(&id).cloned()
    }

    /// Where an unindexed file's media really ends, when a full read of it
    /// found less than its header claims (B1198, V1158): past that point there is nothing
    /// to encode, so the segment cache serves a filler at once rather than
    /// asking the encoder for frames that do not exist. Only a read that ran
    /// to end-of-file says where the media ends; one that gave up early does
    /// not. The file state the read saw comes along, so a file replaced since
    /// is encoded as normal.
    pub fn known_end(&self, id: u64) -> Option<pharos_cache::KnownSourceEnd> {
        let entry = self.verdict(id)?;
        let report = &entry.report;
        // Only an unindexed file: a seek into one reads it from byte 0 with
        // the same demuxer the scan used, so where the scan stopped is where
        // any encode stops too. An indexed file's demuxer can stop at a bad
        // chunk the index then seeks past, and a filler there would replace
        // real picture.
        let ends_early = report.complete
            && report.is_unindexed()
            && report.declared_ms.is_some_and(|d| d > report.scanned_ms);
        ends_early.then_some(pharos_cache::KnownSourceEnd {
            end_ms: report.scanned_ms,
            mtime_secs: entry.mtime,
            size: entry.size,
        })
    }

    /// How many files have been scanned.
    pub fn len(&self) -> usize {
        self.doc.read().map(|d| d.entries.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Persist. Written whole and atomically via a temp file plus rename, so an
    /// interrupted sweep cannot leave a half-written memo that then fails to
    /// parse and silently discards every verdict earned by a full library read.
    pub fn save(&self) -> std::io::Result<()> {
        let Ok(d) = self.doc.read() else {
            return Ok(());
        };
        let json = serde_json::to_string(&*d)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }

    /// Where this memo lives, for the boot log — an operator who wants a full
    /// re-scan deletes this file, and needs to be told where it is.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn now_secs() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use tempfile::TempDir;

    fn clean() -> IntegrityReport {
        IntegrityReport {
            packets: 500,
            scanned_ms: 20_000,
            complete: true,
            ..Default::default()
        }
    }

    fn damaged() -> IntegrityReport {
        IntegrityReport {
            packets: 70,
            demux_errors: 1,
            first_fault_ms: Some(2_870),
            first_fault: Some("[matroska,webm] invalid as first byte of an EBML number".into()),
            scanned_ms: 19_000,
            complete: true,
            ranges: Some(vec![pharos_transcode::integrity::DamageRange {
                start_ms: 2_870,
                end_ms: 3_400,
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn a_damaged_verdict_from_before_spans_is_read_again() {
        let memo = IntegrityMemo::load("/nonexistent/memo.json", 1);
        let legacy = IntegrityReport {
            ranges: None,
            ..damaged()
        };
        memo.record(1, Some((100, 500)), legacy);
        memo.record(
            2,
            Some((100, 500)),
            IntegrityReport {
                ranges: None,
                ..clean()
            },
        );
        assert!(
            !memo.is_scanned(1, Some((100, 500))),
            "it needs its spans for the seek bar"
        );
        assert!(
            memo.is_scanned(2, Some((100, 500))),
            "a clean file has no spans to gain"
        );
        assert_eq!(
            memo.damaged().len(),
            1,
            "still listed until the re-read lands"
        );
    }

    /// B1198/V1158: only a read of an unindexed file that reached end-of-file
    /// and found less media than the header claims says where it ends.
    #[test]
    fn only_a_complete_read_short_of_the_header_gives_a_known_end() {
        let memo = IntegrityMemo::load("/nonexistent/memo.json", 1);
        let ends_early = IntegrityReport {
            declared_ms: Some(30_000),
            seek_index: Some(false),
            ..clean()
        };
        memo.record(1, Some((100, 500)), ends_early.clone());
        memo.record(
            2,
            Some((100, 500)),
            IntegrityReport {
                complete: false,
                ..ends_early.clone()
            },
        );
        memo.record(
            3,
            Some((100, 500)),
            IntegrityReport {
                declared_ms: Some(20_000),
                ..ends_early.clone()
            },
        );
        memo.record(
            4,
            Some((100, 500)),
            IntegrityReport {
                declared_ms: None,
                ..ends_early.clone()
            },
        );
        memo.record(
            6,
            Some((100, 500)),
            IntegrityReport {
                seek_index: None,
                ..ends_early.clone()
            },
        );
        assert_eq!(
            memo.known_end(1),
            Some(pharos_cache::KnownSourceEnd {
                end_ms: 20_000,
                mtime_secs: 100,
                size: 500,
            })
        );
        assert_eq!(memo.known_end(2), None, "a read that gave up early");
        assert_eq!(memo.known_end(3), None, "all the media the header claims");
        assert_eq!(memo.known_end(4), None, "no header duration");
        assert_eq!(memo.known_end(5), None, "never read");
        assert_eq!(memo.known_end(6), None, "a seekable file");
    }

    /// The load-bearing difference from `probe_memo`: a CLEAN verdict must be
    /// remembered too. Recording only failures would re-read every healthy file
    /// in the library on every pass — the entire cost of the sweep, and none of
    /// the benefit.
    #[test]
    fn a_clean_verdict_is_remembered_not_just_a_damaged_one() {
        let memo = IntegrityMemo::load("/nonexistent/memo.json", 1);
        memo.record(1, Some((100, 500)), clean());
        assert!(
            memo.is_scanned(1, Some((100, 500))),
            "a healthy file must not be read again next pass"
        );
        assert!(!memo.get(1, Some((100, 500))).unwrap().report.is_damaged());
    }

    #[test]
    fn a_changed_file_is_scanned_again() {
        let memo = IntegrityMemo::load("/nonexistent/memo.json", 1);
        memo.record(1, Some((100, 500)), damaged());
        assert!(memo.is_scanned(1, Some((100, 500))));
        // Replaced with a good copy: mtime moves, so it is read again. This is
        // what keeps a re-acquired file from staying condemned.
        assert!(!memo.is_scanned(1, Some((200, 500))), "new mtime re-scans");
        assert!(!memo.is_scanned(1, Some((100, 900))), "new size re-scans");
        assert!(!memo.is_scanned(2, Some((100, 500))), "different file");
    }

    #[test]
    fn an_unstattable_file_is_never_written_off() {
        let memo = IntegrityMemo::load("/nonexistent/memo.json", 1);
        memo.record(1, None, clean());
        assert!(!memo.is_scanned(1, None));
        assert!(!memo.is_scanned(1, Some((100, 500))));
        assert_eq!(memo.len(), 0, "there was nothing to key on");
    }

    #[test]
    fn the_damaged_list_carries_the_reason_across_a_restart() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("integrity.json");
        let memo = IntegrityMemo::load(&path, 1);
        memo.record(1, Some((100, 500)), clean());
        memo.record(2, Some((100, 500)), damaged());
        memo.save().unwrap();

        let reloaded = IntegrityMemo::load(&path, 1);
        assert_eq!(reloaded.len(), 2);
        let bad = reloaded.damaged();
        assert_eq!(bad.len(), 1, "only the damaged file is listed");
        assert_eq!(bad[0].0, 2);
        assert_eq!(
            bad[0].1.report.first_fault.as_deref(),
            Some("[matroska,webm] invalid as first byte of an EBML number"),
            "the demuxer's own words are the point of persisting the report"
        );
        assert_eq!(bad[0].1.report.first_fault_ms, Some(2_870));
    }

    /// A change to what counts as damage can be a bug fix, so a file recorded
    /// clean under the old rules must be read again under the new ones.
    #[test]
    fn a_scan_version_change_drops_the_whole_memo() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("integrity.json");
        let memo = IntegrityMemo::load(&path, 1);
        memo.record(1, Some((100, 500)), clean());
        memo.save().unwrap();

        let newer = IntegrityMemo::load(&path, 2);
        assert!(
            !newer.is_scanned(1, Some((100, 500))),
            "a newer scan version must re-read every file"
        );
        assert!(newer.is_empty());
    }

    #[test]
    fn a_verdict_records_when_it_was_made() {
        let memo = IntegrityMemo::load("/nonexistent/memo.json", 1);
        memo.record(1, Some((100, 500)), clean());
        let at = memo.get(1, Some((100, 500))).unwrap().checked_at.unwrap();
        assert!(at > 1_700_000_000, "a real clock reading: {at}");
    }

    /// Memos on disk today carry no `checked_at`. They must load as they are:
    /// failing to parse would discard every verdict and re-read the library.
    #[test]
    fn a_memo_written_before_checked_at_still_loads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("integrity.json");
        std::fs::write(
            &path,
            r#"{"integrity_scan_version":1,"entries":{"7":{"mtime":1,"size":2,"report":{"packets":3,"corrupt_packets":0,"read_errors":0,"demux_errors":0,"first_fault_ms":null,"first_fault":null,"scanned_ms":4,"declared_ms":null,"complete":true}}}}"#,
        )
        .unwrap();
        let memo = IntegrityMemo::load(&path, 1);
        let e = memo.get(7, Some((1, 2))).expect("old entry survives");
        assert_eq!(e.checked_at, None);
    }

    #[test]
    fn a_corrupt_memo_file_is_an_empty_memo_not_a_failure() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("integrity.json");
        std::fs::write(&path, "{not json").unwrap();
        let memo = IntegrityMemo::load(&path, 1);
        assert!(memo.is_empty(), "a bad cache file must not stop a sweep");
        memo.record(1, Some((1, 1)), clean());
        memo.save().unwrap();
        assert!(IntegrityMemo::load(&path, 1).is_scanned(1, Some((1, 1))));
    }

    /// B1162: a file with no seek index is listed for admins but carries no
    /// "Damaged" badge, since it plays through and only seeks slowly.
    #[test]
    fn an_unindexed_file_is_listed_but_not_badged() {
        let memo = IntegrityMemo::load("/nonexistent/m.json", 1);
        memo.record(
            4,
            Some((1, 1)),
            IntegrityReport {
                complete: true,
                seek_index: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(memo.damaged().len(), 1, "the admin list names it");
        assert!(
            memo.damaged_ids().is_empty(),
            "the library does not badge it"
        );
    }
}
