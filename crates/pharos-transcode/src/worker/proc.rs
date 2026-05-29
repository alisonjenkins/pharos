//! Main-process side of the worker pool: spawn a `transcode-worker`
//! subprocess, talk to it over an `AF_UNIX` socketpair (control frames
//! on fd 3 in the child), and present it to the scheduler as a
//! [`Worker`].
//!
//! Liveness is detected three ways, any of which collapses a job to
//! `WorkerRunResult::Died` so the scheduler can retry without hanging:
//! 1. EOF on the control socket (`read_frame` → `Ok(None)`) — the
//!    primary signal; covers clean exit + segfault (the kernel closes
//!    the fd).
//! 2. A frame/IO error on the socket.
//! 3. A heartbeat timeout — no `Progress`/terminal frame within
//!    `heartbeat_timeout`; the child is then killed (`kill_on_drop`).
//!
//! The control socket is a socketpair (not stdin/stdout) so it can later
//! carry `SCM_RIGHTS` fds for the live-stream sink.

use crate::protocol::{read_frame, write_frame, Handshake, WorkerCmd, WorkerEvent};
use crate::scheduler::{Worker, WorkerRunResult, WorkerSpawner};
use crate::protocol::{JobSpec, WorkerId};
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(20);

/// Spawns `transcode-worker` subprocesses on demand.
pub struct ProcSpawner {
    worker_bin: PathBuf,
    handshake_timeout: Duration,
    heartbeat_timeout: Duration,
}

impl Default for ProcSpawner {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcSpawner {
    /// Discover the worker binary: `PHAROS_TRANSCODE_WORKER` env, then a
    /// sibling of the current executable, then bare `transcode-worker`
    /// on `PATH`.
    pub fn new() -> Self {
        let worker_bin = discover_worker_bin();
        Self {
            worker_bin,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            heartbeat_timeout: DEFAULT_HEARTBEAT_TIMEOUT,
        }
    }

    pub fn with_worker_bin(p: impl Into<PathBuf>) -> Self {
        Self {
            worker_bin: p.into(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            heartbeat_timeout: DEFAULT_HEARTBEAT_TIMEOUT,
        }
    }

    pub fn with_heartbeat_timeout(mut self, d: Duration) -> Self {
        self.heartbeat_timeout = d;
        self
    }

    pub fn worker_bin(&self) -> &std::path::Path {
        &self.worker_bin
    }
}

fn discover_worker_bin() -> PathBuf {
    if let Ok(p) = std::env::var("PHAROS_TRANSCODE_WORKER") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("transcode-worker");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from("transcode-worker")
}

/// Create a connected `AF_UNIX` `SOCK_STREAM` pair. Returns
/// `(parent_end, child_end)`.
fn make_socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SOCK_STREAM gives a reliable, ordered byte stream for our framing.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socketpair just handed us two fresh owned fds.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

impl WorkerSpawner for ProcSpawner {
    fn spawn(
        &self,
        id: WorkerId,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Worker>>> + Send>> {
        let worker_bin = self.worker_bin.clone();
        let handshake_timeout = self.handshake_timeout;
        let heartbeat_timeout = self.heartbeat_timeout;
        Box::pin(async move {
            let (parent_fd, child_fd) = make_socketpair()?;
            // Don't leak the parent end into the child.
            unsafe {
                libc::fcntl(parent_fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            }
            let child_raw = child_fd.as_raw_fd();

            let mut cmd = Command::new(&worker_bin);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .kill_on_drop(true);
            // SAFETY: dup2 + the (no-op) closes are async-signal-safe.
            unsafe {
                cmd.pre_exec(move || {
                    // Move the child end to the well-known fd 3. dup2
                    // clears CLOEXEC on the new fd, so it survives exec.
                    if libc::dup2(child_raw, 3) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = cmd.spawn()?;
            // Parent no longer needs the child end (the forked child has
            // its own copy, already dup'd to fd 3).
            drop(child_fd);

            // Wrap the parent end as a tokio UnixStream.
            let parent_raw = parent_fd.into_raw_fd();
            // SAFETY: we own parent_raw exclusively now.
            let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(parent_raw) };
            std_stream.set_nonblocking(true)?;
            let stream = tokio::net::UnixStream::from_std(std_stream)?;
            let (mut rd, wr) = stream.into_split();

            // Handshake: the worker's first frame must be Hello.
            let hello =
                tokio::time::timeout(handshake_timeout, read_frame::<_, WorkerEvent>(&mut rd))
                    .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "worker handshake timeout"))?
                .map_err(|e| io::Error::other(format!("worker handshake frame: {e}")))?;
            let handshake = match hello {
                Some(WorkerEvent::Hello(h)) => h,
                Some(other) => {
                    return Err(io::Error::other(format!(
                        "worker sent {other:?} before Hello"
                    )))
                }
                None => return Err(io::Error::other("worker closed before Hello")),
            };

            Ok(Box::new(ProcWorker {
                id,
                rd,
                wr,
                _child: child,
                handshake,
                heartbeat_timeout,
            }) as Box<dyn Worker>)
        })
    }
}

/// A live worker subprocess + its control socket halves.
pub struct ProcWorker {
    id: WorkerId,
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    /// Held for its `kill_on_drop` guarantee — dropping the worker SIGKILLs
    /// the child (KillGuard parity with the in-process transcoder).
    _child: Child,
    handshake: Handshake,
    heartbeat_timeout: Duration,
}

impl ProcWorker {
    pub fn handshake(&self) -> &Handshake {
        &self.handshake
    }
}

impl Worker for ProcWorker {
    fn id(&self) -> WorkerId {
        self.id
    }

    fn run<'a>(
        &'a mut self,
        job: JobSpec,
    ) -> Pin<Box<dyn Future<Output = WorkerRunResult> + Send + 'a>> {
        Box::pin(async move {
            if write_frame(&mut self.wr, &WorkerCmd::Job(job)).await.is_err() {
                return WorkerRunResult::Died;
            }
            loop {
                let next = tokio::time::timeout(
                    self.heartbeat_timeout,
                    read_frame::<_, WorkerEvent>(&mut self.rd),
                )
                .await;
                match next {
                    // Heartbeat timeout — worker hung. Caller drops us →
                    // child killed.
                    Err(_) => return WorkerRunResult::Died,
                    // Frame/IO error.
                    Ok(Err(_)) => return WorkerRunResult::Died,
                    // Clean EOF — worker exited/crashed.
                    Ok(Ok(None)) => return WorkerRunResult::Died,
                    Ok(Ok(Some(ev))) => match ev {
                        WorkerEvent::Accepted { .. } | WorkerEvent::Progress { .. } => {
                            // Progress resets the heartbeat window (next
                            // loop iteration re-arms the timeout).
                            continue;
                        }
                        WorkerEvent::Done { out_bytes, .. } => {
                            return WorkerRunResult::Done { out_bytes }
                        }
                        WorkerEvent::Failed { error, .. } => {
                            return WorkerRunResult::Failed(error)
                        }
                        // Unexpected mid-job Hello — ignore.
                        WorkerEvent::Hello(_) => continue,
                    },
                }
            }
        })
    }
}
