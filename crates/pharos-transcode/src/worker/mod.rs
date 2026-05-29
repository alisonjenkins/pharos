//! Out-of-process worker pool. The scheduler dispatches encodes to
//! `transcode-worker` subprocesses; a worker crash (segfault) takes down
//! only that process, preserving V6 (ffmpeg/libav fault never crashes
//! the server). Unix-only — the control channel is an `AF_UNIX`
//! socketpair.

pub mod exec;
pub mod proc;

pub use proc::{ProcSpawner, ProcWorker};
