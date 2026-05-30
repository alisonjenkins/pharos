//! P48 — `Prober` backed by the persistent libav worker pool. Replaces
//! the per-file `ffprobe` fork (`FfmpegProber`) during a library scan —
//! the standout spawn hotspot — with an in-process libav probe serviced
//! by a resident, crash-isolated worker. Behaviour matches `FfmpegProber`
//! (the worker's `libav::probe` maps the same fields ffprobe reports).

use pharos_core::{DomainError, DomainResult, ProbeInfo, Prober};
use pharos_transcode::worker::{LibavWorkerPool, PoolError};
use std::path::Path;

/// A `Prober` that delegates to a [`LibavWorkerPool`]. Cheap to clone
/// (shares the pool).
#[derive(Clone)]
pub struct LibavProber {
    pool: LibavWorkerPool,
}

impl LibavProber {
    /// Wrap an existing pool (share it with other libav-worker consumers).
    pub fn new(pool: LibavWorkerPool) -> Self {
        Self { pool }
    }

    /// Build a prober owning a freshly-discovered pool (worker binary via
    /// env → sibling → PATH).
    pub fn with_discovered_bin() -> Self {
        Self {
            pool: LibavWorkerPool::with_discovered_bin(),
        }
    }

    /// The underlying pool, for sharing with image/trickplay caches.
    pub fn pool(&self) -> &LibavWorkerPool {
        &self.pool
    }
}

impl Prober for LibavProber {
    async fn probe(&self, path: &Path) -> DomainResult<ProbeInfo> {
        self.pool
            .probe(path.to_path_buf())
            .await
            .map_err(|e| match e {
                // Malformed source — the scanner treats this like ffprobe
                // exiting non-zero on a bad file.
                PoolError::Op(_) => DomainError::Backend(format!("libav probe: {e}")),
                // Worker infra problem (spawn/death) — also a backend error;
                // the caller may retry the file.
                PoolError::Spawn(_) | PoolError::Dead(_) => {
                    DomainError::Backend(format!("libav worker: {e}"))
                }
            })
    }
}
