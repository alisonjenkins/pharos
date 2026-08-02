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
    pub fn is_scanned(&self, id: u64, sig: Option<(i64, u64)>) -> bool {
        self.get(id, sig).is_some()
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
                },
            );
        }
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
            ..Default::default()
        }
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
}
