//! Remembering that OpenSubtitles had nothing for an item, so the background
//! job does not ask again every pass (V569).
//!
//! The same shape as `integrity_memo`: a small JSON file beside the cache,
//! keyed by item and language, valid for the file's exact `(mtime, size)` and
//! for [`MISS_TTL_SECS`]. Losing it costs one round of searches, never
//! correctness, so a load failure is an empty memo.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// A miss is trusted for a week: new uploads do appear, but not by the hour,
/// and every re-ask spends request budget.
pub const MISS_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Bump to drop every remembered miss (a changed query or ranking can turn a
/// past miss into a hit).
pub const MEMO_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    mtime: i64,
    size: u64,
    at_unix: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Document {
    schema_version: i64,
    /// `"<item id>:<language>"`.
    misses: HashMap<String, Entry>,
}

#[derive(Debug)]
pub struct MissMemo {
    path: PathBuf,
    doc: RwLock<Document>,
}

fn key(item_id: u64, language: &str) -> String {
    format!("{item_id}:{language}")
}

impl MissMemo {
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let doc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Document>(&s).ok())
            .filter(|d| d.schema_version == MEMO_SCHEMA_VERSION)
            .unwrap_or(Document {
                schema_version: MEMO_SCHEMA_VERSION,
                misses: HashMap::new(),
            });
        Self {
            path,
            doc: RwLock::new(doc),
        }
    }

    /// True when this item, in this file state, was searched for `language`
    /// and found nothing less than [`MISS_TTL_SECS`] ago.
    pub fn is_fresh_miss(
        &self,
        item_id: u64,
        language: &str,
        sig: (i64, u64),
        now_unix: i64,
    ) -> bool {
        let Ok(doc) = self.doc.read() else {
            return false;
        };
        doc.misses.get(&key(item_id, language)).is_some_and(|e| {
            (e.mtime, e.size) == sig && now_unix.saturating_sub(e.at_unix) < MISS_TTL_SECS
        })
    }

    pub fn record_miss(&self, item_id: u64, language: &str, sig: (i64, u64), now_unix: i64) {
        if let Ok(mut doc) = self.doc.write() {
            doc.misses.insert(
                key(item_id, language),
                Entry {
                    mtime: sig.0,
                    size: sig.1,
                    at_unix: now_unix,
                },
            );
        }
    }

    pub fn len(&self) -> usize {
        self.doc.read().map(|d| d.misses.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write the memo atomically (temp file then rename).
    pub fn save(&self) -> std::io::Result<()> {
        let json = {
            let doc = self
                .doc
                .read()
                .map_err(|_| std::io::Error::other("miss memo lock poisoned"))?;
            serde_json::to_string(&*doc).map_err(std::io::Error::other)?
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const SIG: (i64, u64) = (100, 5000);

    #[test]
    fn a_miss_holds_until_the_ttl_the_file_changes_or_the_language_differs() {
        let dir = tempfile::tempdir().unwrap();
        let m = MissMemo::load(dir.path().join("m.json"));
        assert!(!m.is_fresh_miss(1, "en", SIG, 1000));
        m.record_miss(1, "en", SIG, 1000);
        assert!(m.is_fresh_miss(1, "en", SIG, 1000 + MISS_TTL_SECS - 1));
        assert!(
            !m.is_fresh_miss(1, "en", SIG, 1000 + MISS_TTL_SECS),
            "expired"
        );
        assert!(
            !m.is_fresh_miss(1, "en", (101, 5000), 1000),
            "mtime changed"
        );
        assert!(!m.is_fresh_miss(1, "en", (100, 5001), 1000), "size changed");
        assert!(!m.is_fresh_miss(1, "fr", SIG, 1000), "other language");
        assert!(!m.is_fresh_miss(2, "en", SIG, 1000), "other item");
    }

    #[test]
    fn survives_a_restart_and_drops_on_a_schema_change() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("m.json");
        let m = MissMemo::load(&p);
        m.record_miss(7, "en", SIG, 50);
        m.save().unwrap();
        let again = MissMemo::load(&p);
        assert!(again.is_fresh_miss(7, "en", SIG, 60));
        std::fs::write(&p, r#"{"schema_version":0,"misses":{}}"#).unwrap();
        assert!(MissMemo::load(&p).is_empty());
        std::fs::write(&p, "not json").unwrap();
        assert!(
            MissMemo::load(&p).is_empty(),
            "corrupt memo is empty, not an error"
        );
    }
}
