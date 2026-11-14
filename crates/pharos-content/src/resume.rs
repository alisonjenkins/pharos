//! Where each long track stopped, so it starts there next time.
//!
//! ```text
//! <files>/resume.json   {"positions": {"<id>": <ms>, ...}}
//! ```
//!
//! Only tracks over [`MIN_TRACK_MS`] are kept: a song restarts, a long
//! mix or a recorded set picks up where it was. Positions near either end
//! clear the entry instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::cache::write_atomic;
use super::error::ContentError;

pub const FILE: &str = "resume.json";
/// Tracks this long or shorter always start at the beginning.
pub const MIN_TRACK_MS: u64 = 10 * 60 * 1000;
/// How often the player saves while playing: a killed process loses at
/// most this much.
pub const SAVE_EVERY: Duration = Duration::from_secs(5);
/// This close to the end counts as finished (outros, applause, silence).
pub const END_MARGIN_MS: u64 = 30 * 1000;
/// This close to the start is not worth resuming.
pub const START_MARGIN_MS: u64 = 5 * 1000;

#[derive(Debug, Default, Serialize, Deserialize)]
struct ResumeFile {
    positions: BTreeMap<String, u64>,
}

/// What [`Resume::record`] did, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Saved,
    Unchanged,
    /// Within [`END_MARGIN_MS`] of the end: the entry is dropped.
    Finished,
    /// Within [`START_MARGIN_MS`] of the start: the entry is dropped.
    AtStart,
    /// Not over [`MIN_TRACK_MS`]: never kept.
    Short,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Saved => "saved",
            Verdict::Unchanged => "unchanged",
            Verdict::Finished => "finished",
            Verdict::AtStart => "at_start",
            Verdict::Short => "short",
        }
    }
}

pub struct Resume {
    path: PathBuf,
    positions: Mutex<BTreeMap<String, u64>>,
}

impl Resume {
    /// Loads `<dir>/resume.json`. A missing or unreadable file starts empty;
    /// an unreadable one is logged and overwritten by the next save.
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let positions = match std::fs::read(&path) {
            Ok(b) => match serde_json::from_slice::<ResumeFile>(&b) {
                Ok(f) => f.positions,
                Err(e) => {
                    log::warn!("resume file unreadable ({e}); starting empty");
                    BTreeMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                log::warn!(
                    "resume file not read ({}): {e}; starting empty",
                    path.display()
                );
                BTreeMap::new()
            }
        };
        Self {
            path,
            positions: Mutex::new(positions),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, u64>> {
        self.positions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Where to start `id`, if it is long enough and has a saved position.
    pub fn position(&self, id: &str, duration_ms: u64) -> Option<u64> {
        if duration_ms <= MIN_TRACK_MS {
            return None;
        }
        self.lock().get(id).copied().filter(|&p| p < duration_ms)
    }

    /// Notes that `id` is at `position_ms` of `duration_ms`, and saves the
    /// file when that changed what it holds.
    pub fn record(
        &self,
        id: &str,
        position_ms: u64,
        duration_ms: u64,
    ) -> Result<Verdict, ContentError> {
        if duration_ms <= MIN_TRACK_MS {
            return Ok(Verdict::Short);
        }
        let mut positions = self.lock();
        let (verdict, changed) = if position_ms.saturating_add(END_MARGIN_MS) >= duration_ms {
            (Verdict::Finished, positions.remove(id).is_some())
        } else if position_ms < START_MARGIN_MS {
            (Verdict::AtStart, positions.remove(id).is_some())
        } else if positions.get(id) == Some(&position_ms) {
            (Verdict::Unchanged, false)
        } else {
            positions.insert(id.to_owned(), position_ms);
            (Verdict::Saved, true)
        };
        if changed {
            self.save(&positions)?;
        }
        Ok(verdict)
    }

    /// Forgets where `id` stopped, so it starts at the beginning. `false`
    /// when nothing was saved for it.
    pub fn clear(&self, id: &str) -> Result<bool, ContentError> {
        let mut positions = self.lock();
        if positions.remove(id).is_none() {
            return Ok(false);
        }
        self.save(&positions)?;
        Ok(true)
    }

    fn save(&self, positions: &BTreeMap<String, u64>) -> Result<(), ContentError> {
        let file = ResumeFile {
            positions: positions.clone(),
        };
        let bytes = serde_json::to_vec(&file).map_err(|e| ContentError::Resume {
            op: "encode".into(),
            why: e.to_string(),
        })?;
        write_atomic(&self.path, &bytes).map_err(|e| ContentError::Resume {
            op: format!("write {}", self.path.display()),
            why: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: u64 = 60 * 60 * 1000;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pharos-resume-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).ok();
        d
    }

    #[test]
    fn a_long_track_resumes_after_a_restart() -> Result<(), ContentError> {
        let d = dir("restart");
        let r = Resume::open(&d);
        assert_eq!(r.record("t1", 125_000, LONG)?, Verdict::Saved);
        assert_eq!(r.record("t1", 125_000, LONG)?, Verdict::Unchanged);
        let again = Resume::open(&d);
        assert_eq!(again.position("t1", LONG), Some(125_000));
        assert_eq!(again.position("t2", LONG), None);
        Ok(())
    }

    #[test]
    fn short_tracks_are_never_kept() -> Result<(), ContentError> {
        let d = dir("short");
        let r = Resume::open(&d);
        assert_eq!(r.record("song", 60_000, MIN_TRACK_MS)?, Verdict::Short);
        assert_eq!(r.position("song", MIN_TRACK_MS), None);
        assert!(!d.join(FILE).exists(), "nothing to save, no file");
        Ok(())
    }

    #[test]
    fn near_either_end_clears_the_entry() -> Result<(), ContentError> {
        let d = dir("ends");
        let r = Resume::open(&d);
        r.record("a", 600_000, LONG)?;
        assert_eq!(
            r.record("a", LONG - END_MARGIN_MS, LONG)?,
            Verdict::Finished
        );
        assert_eq!(Resume::open(&d).position("a", LONG), None);
        r.record("b", 600_000, LONG)?;
        assert_eq!(r.record("b", START_MARGIN_MS - 1, LONG)?, Verdict::AtStart);
        assert_eq!(Resume::open(&d).position("b", LONG), None);
        Ok(())
    }

    #[test]
    fn a_broken_file_starts_empty_and_is_replaced() -> Result<(), ContentError> {
        let d = dir("broken");
        std::fs::write(d.join(FILE), b"{not json").ok();
        let r = Resume::open(&d);
        assert_eq!(r.position("a", LONG), None);
        r.record("a", 700_000, LONG)?;
        assert_eq!(Resume::open(&d).position("a", LONG), Some(700_000));
        Ok(())
    }

    #[test]
    fn clear_forgets_one_track_and_saves() -> Result<(), ContentError> {
        let d = dir("clear");
        let r = Resume::open(&d);
        assert!(!r.clear("a")?, "nothing saved yet");
        r.record("a", 700_000, LONG)?;
        r.record("b", 800_000, LONG)?;
        assert!(r.clear("a")?);
        let back = Resume::open(&d);
        assert_eq!(back.position("a", LONG), None);
        assert_eq!(back.position("b", LONG), Some(800_000), "others kept");
        Ok(())
    }
}
