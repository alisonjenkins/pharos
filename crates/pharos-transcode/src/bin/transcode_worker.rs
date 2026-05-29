//! `transcode-worker` — the out-of-process encode worker.
//!
//! Talks to the scheduler over an `AF_UNIX` socketpair handed in on fd 3.
//! Sends `Hello`, then loops on `WorkerCmd`s. Under the default
//! (`backend-spawn`) build it executes jobs by shelling out to `ffmpeg`
//! (crash-isolated by the process boundary); the `backend-lib` build
//! swaps in the in-process FFI encode path (added in a later step).
//!
//! A crash here (segfault / abort) closes fd 3, which the scheduler reads
//! as the worker dying mid-job and retries elsewhere — the server process
//! is never affected (V6).

#[cfg(unix)]
fn main() {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("transcode-worker: runtime init failed: {e}");
            std::process::exit(1);
        }
    };
    rt.block_on(run());
}

#[cfg(not(unix))]
fn main() {
    eprintln!("transcode-worker is unix-only");
    std::process::exit(1);
}

#[cfg(unix)]
async fn run() {
    use pharos_transcode::protocol::{read_frame, write_frame, Handshake, WorkerCmd, WorkerEvent};
    use pharos_transcode::worker::exec;
    use std::os::fd::FromRawFd;

    // fd 3 is the control socket (dup'd in by ProcSpawner::pre_exec).
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    if let Err(e) = std_stream.set_nonblocking(true) {
        eprintln!("transcode-worker: fd 3 nonblocking failed: {e}");
        std::process::exit(1);
    }
    let stream = match tokio::net::UnixStream::from_std(std_stream) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("transcode-worker: fd 3 not a unix socket: {e}");
            std::process::exit(1);
        }
    };
    let (mut rd, mut wr) = stream.into_split();

    let backend = if cfg!(feature = "backend-lib") {
        "ffi-libav"
    } else {
        "spawn"
    };
    let hello = WorkerEvent::Hello(Handshake {
        backend: backend.to_string(),
        openable_devices: exec::openable_devices().await,
    });
    if write_frame(&mut wr, &hello).await.is_err() {
        return;
    }

    loop {
        match read_frame::<_, WorkerCmd>(&mut rd).await {
            Ok(Some(WorkerCmd::Job(spec))) => run_job(&mut wr, spec).await,
            Ok(Some(WorkerCmd::Cancel { .. })) => {
                // Single-job-at-a-time worker: nothing is in flight
                // between reads, so a cancel is a no-op here.
            }
            Ok(Some(WorkerCmd::Shutdown)) | Ok(None) => break,
            Err(_) => break,
        }
    }
}

#[cfg(all(unix, not(feature = "backend-lib")))]
async fn run_job(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    spec: pharos_transcode::protocol::JobSpec,
) {
    use pharos_transcode::protocol::{write_frame, WorkerError, WorkerEvent};
    use pharos_transcode::worker::exec;
    use std::time::Duration;

    use pharos_transcode::protocol::DeviceId;
    let job_id = spec.job_id;
    let is_hw = matches!(spec.device, DeviceId::Hw { .. });
    let _ = write_frame(wr, &WorkerEvent::Accepted { job_id }).await;

    let (args, out) = match exec::spawn_job_args(&spec) {
        Ok(v) => v,
        Err(e) => {
            let _ = write_frame(wr, &WorkerEvent::Failed { job_id, error: e }).await;
            return;
        }
    };

    let mut cmd = tokio::process::Command::new(exec::ffmpeg_bin());
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = write_frame(
                wr,
                &WorkerEvent::Failed {
                    job_id,
                    error: WorkerError::Io(format!("spawn ffmpeg: {e}")),
                },
            )
            .await;
            return;
        }
    };

    // Drain stderr concurrently so a chatty ffmpeg can't deadlock on a
    // full pipe buffer.
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    // Heartbeat + progress: while ffmpeg runs, periodically report the
    // growing output size so the scheduler's heartbeat window stays open
    // on long encodes.
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let status = loop {
        tokio::select! {
            s = child.wait() => break s,
            _ = ticker.tick() => {
                let sz = tokio::fs::metadata(&out).await.map(|m| m.len()).unwrap_or(0);
                let _ = write_frame(wr, &WorkerEvent::Progress { job_id, out_bytes: sz, frames: 0 }).await;
            }
        }
    };

    let stderr_text = stderr_task.await.unwrap_or_default();
    match status {
        Ok(s) if s.success() => {
            let out_bytes = tokio::fs::metadata(&out).await.map(|m| m.len()).unwrap_or(0);
            let _ = write_frame(wr, &WorkerEvent::Done { job_id, out_bytes }).await;
        }
        Ok(_) => {
            let _ = write_frame(
                wr,
                &WorkerEvent::Failed {
                    job_id,
                    error: exec::classify_failure(&stderr_text, is_hw),
                },
            )
            .await;
        }
        Err(e) => {
            let _ = write_frame(
                wr,
                &WorkerEvent::Failed {
                    job_id,
                    error: WorkerError::Io(format!("ffmpeg wait: {e}")),
                },
            )
            .await;
        }
    }
}

#[cfg(all(unix, feature = "backend-lib"))]
async fn run_job(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    spec: pharos_transcode::protocol::JobSpec,
) {
    use pharos_transcode::protocol::{write_frame, WorkerError, WorkerEvent};
    // FFI encode path lands in a later step; until then the lib build
    // reports a clean non-recoverable error rather than silently doing
    // nothing, so the scheduler fails the job loudly instead of hanging.
    let job_id = spec.job_id;
    let _ = write_frame(wr, &WorkerEvent::Accepted { job_id }).await;
    let _ = write_frame(
        wr,
        &WorkerEvent::Failed {
            job_id,
            error: WorkerError::Other("FFI encode path not yet implemented".into()),
        },
    )
    .await;
}
