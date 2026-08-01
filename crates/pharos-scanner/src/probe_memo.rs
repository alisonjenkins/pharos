//! Remembering that a file could not be probed, so the next scan does not try
//! again.
//!
//! A successful probe writes a row carrying `(file_mtime, file_size)`, and the
//! incremental scan's fast path compares a fresh `stat` against it to skip
//! unchanged files. A FAILED probe writes nothing at all — so nothing records
//! that the attempt happened, and every subsequent scan re-reads the same file
//! from scratch, forever.
//!
//! That is not theoretical. On the deployment this was written for, 134 files
//! failed to probe and were re-read on every pass, producing 3,752 warnings in
//! one log window. Each attempt is a whole-file open over NFS holding a
//! background-I/O permit — the permit the regulator parks during playback — to
//! read files that are, in most cases, 1.6 GB of null bytes. The work is
//! unbounded in time and competes with live streams, and the log noise buries
//! the failures that are new.
//!
//! It is the same shape as V134, one layer down: **a failure with no record is
//! retried forever.**
//!
//! # Why a file beside the cache, not a table
//!
//! This is a memo, not domain state. Losing it costs exactly one re-probe pass,
//! which is the behaviour that exists today, so the failure mode of the storage
//! is "no worse than before". `rate_store` set the precedent for the shape:
//! persist a measurement beside the cache, keyed by identity, reuse it when the
//! fingerprint still matches. A table would mean a migration and six
//! `MediaStore` implementations for something whose worst-case loss is a slow
//! scan.
//!
//! # What invalidates an entry
//!
//! The file's own `(mtime, size)` — replace a corrupt file with a good one and
//! it is probed again — and [`PROBE_SCHEMA_VERSION`]. The second matters because
//! a probe change can be a BUG FIX: a file that could not be parsed by the old
//! prober may parse now, and a memo that outlived the fix would hide the very
//! recovery the version bump exists to trigger.
//!
//! [`PROBE_SCHEMA_VERSION`]: pharos_core::PROBE_SCHEMA_VERSION

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// The recorded signature of a file that failed to probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Signature {
    mtime: i64,
    size: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Document {
    /// The probe schema the failures were observed under. A change drops every
    /// entry — see the module docs.
    probe_schema_version: i64,
    /// Keyed by `stable_id(path)`, so a rename re-probes (correctly: the new
    /// path has never been tried).
    entries: HashMap<u64, Signature>,
}

/// A persisted set of "this exact file, in this exact state, does not probe".
#[derive(Debug)]
pub struct ProbeMemo {
    path: PathBuf,
    doc: RwLock<Document>,
}

impl ProbeMemo {
    /// Load the memo at `path`, or start empty if it is absent, unreadable or
    /// written under a different probe schema.
    ///
    /// Every failure to load is an empty memo rather than an error: the worst
    /// consequence is one slow scan, and refusing to scan because a cache file
    /// is corrupt would be a far worse trade.
    pub fn load(path: impl Into<PathBuf>, probe_schema_version: i64) -> Self {
        let path = path.into();
        let doc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Document>(&s).ok())
            .filter(|d| d.probe_schema_version == probe_schema_version)
            .unwrap_or(Document {
                probe_schema_version,
                entries: HashMap::new(),
            });
        Self {
            path,
            doc: RwLock::new(doc),
        }
    }

    /// Whether this file is known not to probe in its current state.
    ///
    /// `None` for mtime/size — an unstattable file — is never "known bad": the
    /// stat failing is itself new information, and skipping on it would let a
    /// transient storage fault mark a healthy file permanently unreadable.
    pub fn is_known_bad(&self, id: u64, sig: Option<(i64, u64)>) -> bool {
        let Some((mtime, size)) = sig else {
            return false;
        };
        self.doc
            .read()
            .is_ok_and(|d| d.entries.get(&id) == Some(&Signature { mtime, size }))
    }

    /// Record that this file, in this state, failed to probe.
    pub fn record(&self, id: u64, sig: Option<(i64, u64)>) {
        let Some((mtime, size)) = sig else {
            // Nothing to key on, so nothing to remember. Re-probing an
            // unstattable file next pass is right: it may be back.
            return;
        };
        if let Ok(mut d) = self.doc.write() {
            d.entries.insert(id, Signature { mtime, size });
        }
    }

    /// Forget a file, because it probed successfully.
    ///
    /// Called on every success, not only on a previously-failing file: the cost
    /// is a hash lookup, and the alternative is an entry that outlives the
    /// damage it describes.
    pub fn forget(&self, id: u64) {
        if let Ok(mut d) = self.doc.write() {
            d.entries.remove(&id);
        }
    }

    /// How many files are currently memoised. For the end-of-scan log line —
    /// a number that climbs pass over pass is the signal that a library is
    /// rotting, and it is invisible without this.
    pub fn len(&self) -> usize {
        self.doc.read().map(|d| d.entries.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Persist. Written whole and atomically via a temp file plus rename, so an
    /// interrupted scan cannot leave a half-written memo that then fails to
    /// parse and silently reverts the whole optimisation.
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
    /// re-probe deletes this file, and needs to be told where it is.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_recorded_failure_is_skipped_until_the_file_changes() {
        let memo = ProbeMemo::load("/nonexistent/memo.json", 7);
        assert!(!memo.is_known_bad(1, Some((100, 500))), "nothing known yet");

        memo.record(1, Some((100, 500)));
        assert!(memo.is_known_bad(1, Some((100, 500))));

        // Replaced with a good copy: mtime moves, so it is probed again. This
        // is the assertion that keeps a corrupt file from being written off
        // permanently.
        assert!(
            !memo.is_known_bad(1, Some((200, 500))),
            "new mtime re-probes"
        );
        assert!(
            !memo.is_known_bad(1, Some((100, 900))),
            "new size re-probes"
        );
        // And a different file is unaffected.
        assert!(!memo.is_known_bad(2, Some((100, 500))));
    }

    #[test]
    fn an_unstattable_file_is_never_written_off() {
        // A stat failing is new information, not evidence of corruption. If it
        // counted as "known bad", one transient storage fault would mark a
        // healthy file unreadable for good.
        let memo = ProbeMemo::load("/nonexistent/memo.json", 7);
        memo.record(1, None);
        assert!(!memo.is_known_bad(1, None));
        assert!(!memo.is_known_bad(1, Some((100, 500))));
        assert_eq!(memo.len(), 0, "there was nothing to key on");
    }

    #[test]
    fn a_successful_probe_forgets_the_entry() {
        let memo = ProbeMemo::load("/nonexistent/memo.json", 7);
        memo.record(1, Some((100, 500)));
        memo.forget(1);
        assert!(!memo.is_known_bad(1, Some((100, 500))));
        assert!(memo.is_empty());
    }

    #[test]
    fn a_memo_round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("probe_failures.json");
        let memo = ProbeMemo::load(&path, 7);
        memo.record(42, Some((100, 500)));
        memo.save().unwrap();

        let reloaded = ProbeMemo::load(&path, 7);
        assert!(
            reloaded.is_known_bad(42, Some((100, 500))),
            "a memo that does not survive a restart saves nothing at all"
        );
        assert_eq!(reloaded.len(), 1);
    }

    #[test]
    fn a_probe_schema_change_drops_the_whole_memo() {
        // A probe change can be a BUG FIX, so a file that could not be parsed
        // before may parse now. A memo that outlived the fix would hide exactly
        // the recovery the version bump exists to trigger.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("probe_failures.json");
        let memo = ProbeMemo::load(&path, 7);
        memo.record(42, Some((100, 500)));
        memo.save().unwrap();

        let newer = ProbeMemo::load(&path, 8);
        assert!(
            !newer.is_known_bad(42, Some((100, 500))),
            "a newer probe schema must re-try every previously-failing file"
        );
        assert!(newer.is_empty());
    }

    #[test]
    fn a_corrupt_memo_file_is_an_empty_memo_not_a_failure() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("probe_failures.json");
        std::fs::write(&path, "{not json").unwrap();
        let memo = ProbeMemo::load(&path, 7);
        assert!(memo.is_empty(), "a bad cache file must not stop a scan");
        // And it recovers: the next save overwrites it.
        memo.record(1, Some((1, 1)));
        memo.save().unwrap();
        assert!(ProbeMemo::load(&path, 7).is_known_bad(1, Some((1, 1))));
    }
}
