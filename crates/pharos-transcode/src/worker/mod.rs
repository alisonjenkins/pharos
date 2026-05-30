//! Out-of-process worker pool. The scheduler dispatches encodes to
//! `transcode-worker` subprocesses; a worker crash (segfault) takes down
//! only that process, preserving V6 (ffmpeg/libav fault never crashes
//! the server). Unix-only — the control channel is an `AF_UNIX`
//! socketpair.

pub mod exec;
pub mod libav_pool;
pub mod proc;

pub use libav_pool::{LibavWorkerPool, PoolError};
pub use proc::{ProcSpawner, ProcWorker};

// Video-segment / live transcode always runs through the spawn path
// (`bin/transcode_worker.rs` shells to ffmpeg), even in the `backend-lib`
// build — encode time dwarfs fork/exec and the spawn worker already
// balances every GPU + CPU. `backend-lib` adds the in-process libav
// *tiny ops* (probe/image/trickplay/subtitle/waveform) via the
// `LibavWorkerPool`, not an in-process segment encoder. The parked
// `ffi.rs` segment scaffold is intentionally not `mod`-included.
