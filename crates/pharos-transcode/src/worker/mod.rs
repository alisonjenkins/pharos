//! Out-of-process worker pool. The scheduler dispatches encodes to
//! `transcode-worker` subprocesses; a worker crash (segfault) takes down
//! only that process, preserving V6 (ffmpeg/libav fault never crashes
//! the server). Unix-only — the control channel is an `AF_UNIX`
//! socketpair.

pub mod exec;
pub mod proc;

pub use proc::{ProcSpawner, ProcWorker};

// `ffi.rs` holds the WIP in-process libav transcode pipeline. It is not
// yet `mod`-included: it needs a few ffmpeg-the-third 3.0.2 API fixes
// (AVChannelLayout accessors, encoder→stream `set_parameters`,
// parameters pointer access) before it compiles. The `backend-lib`
// worker uses the stub in `bin/transcode_worker.rs` until then; the
// spawn worker already delivers full GPU + CPU balancing.
