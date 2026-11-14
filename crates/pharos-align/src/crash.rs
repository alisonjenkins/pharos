//! The crash-loop guard on the recogniser's stages (B1435, V1409). A native
//! abort inside a stage (a `GGML_ASSERT`, a driver fault) ends the process
//! where no `catch_unwind` reaches, and the server sends the same song
//! again after the restart. The song in flight is written down before the
//! stages run and cleared after; a marker still there at start-up names
//! the song that ended the last process, and that song skips the stages
//! from then on.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::obs::STAGE_CRASH_TOTAL;

/// The song whose stages are running, as written before they start.
pub const IN_FLIGHT: &str = "stages.inflight";
/// Songs whose stages ended a process, one JSON string per line. Removing
/// a line lets that song try the stages again.
pub const CRASHED: &str = "stages.crashed";

pub struct CrashGuard {
    dir: PathBuf,
    crashed: Mutex<HashSet<String>>,
}

impl CrashGuard {
    /// The guard over `dir`, after moving a song the last process left in
    /// flight onto the crashed list, logged and counted.
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let list = dir.join(CRASHED);
        let mut crashed = HashSet::new();
        match fs::read_to_string(&list) {
            Ok(text) => {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    match serde_json::from_str::<String>(line) {
                        Ok(song) => {
                            crashed.insert(song);
                        }
                        Err(e) => tracing::warn!(
                            list = %list.display(),
                            line,
                            error = %e,
                            "unreadable line in the crashed-songs list, ignored"
                        ),
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let marker = dir.join(IN_FLIGHT);
        match fs::read_to_string(&marker) {
            Ok(song) => {
                tracing::warn!(
                    path = %song,
                    list = %list.display(),
                    "the recogniser stages ended the last process on this song; it skips them from now on"
                );
                metrics::counter!(STAGE_CRASH_TOTAL).increment(1);
                if !crashed.contains(&song) {
                    let mut f = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&list)?;
                    writeln!(f, "{}", serde_json::Value::String(song.clone()))?;
                    f.sync_all()?;
                    crashed.insert(song);
                }
                fs::remove_file(&marker)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if !crashed.is_empty() {
            tracing::info!(
                songs = crashed.len(),
                list = %list.display(),
                "songs skipping the recogniser stages after a crash"
            );
        }
        Ok(Self {
            dir: dir.to_owned(),
            crashed: Mutex::new(crashed),
        })
    }

    /// True when `song`'s stages ended an earlier process.
    pub fn crashed(&self, song: &str) -> bool {
        self.crashed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(song)
    }

    /// Marks `song` in flight until the returned marker drops. The marker
    /// is written whole through a rename, so a crash never leaves half a name.
    pub fn begin(&self, song: &str) -> io::Result<InFlight> {
        let marker = self.dir.join(IN_FLIGHT);
        let part = self.dir.join(format!("{IN_FLIGHT}.part"));
        let mut f = fs::File::create(&part)?;
        f.write_all(song.as_bytes())?;
        f.sync_all()?;
        fs::rename(&part, &marker)?;
        Ok(InFlight { marker })
    }
}

/// A song in flight; dropping it clears the marker.
pub struct InFlight {
    marker: PathBuf,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.marker) {
            // Left behind, it would bar a song that finished cleanly.
            tracing::warn!(
                marker = %self.marker.display(),
                error = %e,
                "could not clear the stages' in-flight marker"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    use super::{CrashGuard, CRASHED, IN_FLIGHT};
    use crate::obs::STAGE_CRASH_TOTAL;

    fn dir(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("pharos-align-crash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_marker_is_there_only_while_the_song_is_in_flight() {
        let d = dir("flight");
        let g = CrashGuard::open(&d).unwrap();
        let flight = g.begin("a/b.mp3").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join(IN_FLIGHT)).unwrap(),
            "a/b.mp3"
        );
        drop(flight);
        assert!(!d.join(IN_FLIGHT).exists());
        assert!(!g.crashed("a/b.mp3"));
    }

    #[test]
    fn a_song_left_in_flight_is_counted_and_skips_from_then_on() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let d = dir("left");
        let g = CrashGuard::open(&d).unwrap();
        // A process that aborts never drops its marker.
        std::mem::forget(g.begin("x\ny.mp3").unwrap());

        let g = CrashGuard::open(&d).unwrap();
        assert!(g.crashed("x\ny.mp3"));
        assert!(!g.crashed("other.mp3"));
        assert!(!d.join(IN_FLIGHT).exists());
        let counted = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find(|(k, ..)| k.key().name() == STAGE_CRASH_TOTAL)
            .map(|(.., v)| v);
        assert_eq!(counted, Some(DebugValue::Counter(1)));

        // The list outlives the restart after that, and holds the song once.
        let g = CrashGuard::open(&d).unwrap();
        assert!(g.crashed("x\ny.mp3"));
        let list = std::fs::read_to_string(d.join(CRASHED)).unwrap();
        assert_eq!(list, "\"x\\ny.mp3\"\n");
    }
}
