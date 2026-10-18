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

    use pharos_transcode::protocol::OutputSink;
    loop {
        match read_frame::<_, WorkerCmd>(&mut rd).await {
            Ok(Some(WorkerCmd::Job(spec))) => {
                // A live (Stdout) job is one-shot: after it finishes the
                // worker must exit so its stdout closes and the reading
                // parent sees EOF. Pooled file jobs keep the worker alive
                // for reuse.
                let one_shot = matches!(spec.sink, OutputSink::Stdout);
                run_job(&mut wr, spec).await;
                if one_shot {
                    break;
                }
            }
            Ok(Some(WorkerCmd::Tiny { job_id, op })) => {
                // Persistent libav request/reply op. The worker stays
                // alive afterwards (the fork/exec is amortised across many
                // ops); a libav crash kills only this process (V6).
                run_tiny(&mut wr, job_id, op).await;
            }
            Ok(Some(WorkerCmd::Cancel { .. })) => {
                // Single-job-at-a-time worker: nothing is in flight
                // between reads, so a cancel is a no-op here.
            }
            Ok(Some(WorkerCmd::Shutdown)) | Ok(None) => break,
            Err(_) => break,
        }
    }
}

// Video-segment / live transcode always shells out to ffmpeg, even in the
// `backend-lib` build: the encode time dwarfs the fork/exec, and the spawn
// path already load-balances every GPU + CPU. `backend-lib` adds the
// in-process libav *tiny ops* (probe/image/trickplay/subtitle/waveform) on
// top via `run_tiny`; it does not replace the segment encoder.
#[cfg(unix)]
async fn run_job(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    spec: pharos_transcode::protocol::JobSpec,
) {
    use pharos_transcode::protocol::{write_frame, WorkerError, WorkerEvent};
    use pharos_transcode::stage_timing::{
        apply_mode_args, parse_progress, StageTiming, StageTimingMode, StderrDigest,
    };
    use pharos_transcode::worker::exec;
    use std::time::Duration;

    // 25 ms keeps the first-frame error well under the 100 ms stats period.
    const FIRST_FRAME_POLL: Duration = Duration::from_millis(25);

    use pharos_transcode::protocol::DeviceId;
    let job_id = spec.job_id;
    let is_hw = matches!(spec.device, DeviceId::Hw { .. });
    let _ = write_frame(wr, &WorkerEvent::Accepted { job_id }).await;

    let (args, target) = match exec::spawn_job_args(&spec) {
        Ok(v) => v,
        Err(e) => {
            let _ = write_frame(wr, &WorkerEvent::Failed { job_id, error: e }).await;
            return;
        }
    };
    let stdout_passthrough = matches!(target, exec::SpawnTarget::Stdout);

    let mode = StageTimingMode::from_env();
    let sidecar = match &target {
        exec::SpawnTarget::File(p) if mode != StageTimingMode::Off => {
            Some(pharos_transcode::progress_sidecar_path(p))
        }
        _ => None,
    };
    // A retry reuses the output path, so a sidecar left by the previous
    // attempt would show frames at the first poll, before ffmpeg has opened
    // (and truncated) its own, and first-frame would read as a few ms.
    if let Some(sc) = &sidecar {
        let _ = tokio::fs::remove_file(sc).await;
    }
    let full_args = apply_mode_args(args, mode, !stdout_passthrough);

    let mut cmd = tokio::process::Command::new(exec::ffmpeg_bin());
    cmd.args(&full_args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if stdout_passthrough {
        // ffmpeg writes the muxed stream to `pipe:1` = this worker's
        // stdout, which the spawner connected to the main process's read
        // pipe. Inherit so the bytes flow straight through.
        cmd.stdout(std::process::Stdio::inherit());
    } else {
        cmd.stdout(std::process::Stdio::null());
    }
    let started = std::time::Instant::now();
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
    let spawn_secs = started.elapsed().as_secs_f64();

    // Drain stderr concurrently so a chatty ffmpeg can't deadlock on a
    // full pipe buffer.
    let stderr = child.stderr.take();
    // Retained bounded: a live stream can run for hours, and an unbounded
    // buffer here is worker RSS that never comes back.
    let collect_bench = mode == StageTimingMode::Detailed && !stdout_passthrough;
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut digest = StderrDigest::new(collect_bench);
        if let Some(mut e) = stderr {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = e.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                digest.feed(chunk.get(..n).unwrap_or_default());
            }
        }
        digest.finish()
    });

    // Heartbeat + progress: while ffmpeg runs, periodically report
    // progress so the scheduler's heartbeat window stays open on long
    // encodes. For the file sink we report the growing output size; for
    // the stdout/live sink we just emit a tick (the main process sees
    // bytes flow directly on the pipe).
    let file_out = match &target {
        exec::SpawnTarget::File(p) => Some(p.clone()),
        exec::SpawnTarget::Stdout => None,
    };
    let mut first_frame_secs: Option<f64> = None;
    let status = if let Some(out_path) = file_out.clone() {
        // File sink: emit periodic size-based progress so the scheduler's
        // heartbeat window stays open on long encodes.
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Polls the progress sidecar until ffmpeg reports a first frame. The
        // sidecar is written every `-stats_period`, so the answer is good to
        // about that plus one poll interval.
        let mut first_frame_poll = tokio::time::interval(FIRST_FRAME_POLL);
        first_frame_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                s = child.wait() => break s,
                _ = first_frame_poll.tick(), if sidecar.is_some() && first_frame_secs.is_none() => {
                    if let Some(sc) = &sidecar {
                        // An audio-only job never reports a frame; its first
                        // output is the timeline moving. Either ends the poll.
                        let seen = tokio::fs::read_to_string(sc).await.map(|t| {
                            let p = parse_progress(&t);
                            p.frames > 0 || p.out_time_us > 0 || p.ended
                        });
                        if matches!(seen, Ok(true)) {
                            first_frame_secs = Some(started.elapsed().as_secs_f64());
                        }
                    }
                }
                _ = ticker.tick() => {
                    let sz = tokio::fs::metadata(&out_path).await.map(|m| m.len()).unwrap_or(0);
                    let _ = write_frame(wr, &WorkerEvent::Progress { job_id, out_bytes: sz, frames: 0 }).await;
                }
            }
        }
    } else {
        // Stdout sink: the parent observes bytes flow directly on the
        // pipe, so no progress frames are needed (and the control channel
        // isn't drained during the stream). Just await completion.
        child.wait().await
    };

    let total_secs = started.elapsed().as_secs_f64();
    // `-benchmark_all` lines went into the report; the text kept for failure
    // classification and the echoed tail is everything else, head and tail only.
    let (bench, stderr_text) = stderr_task.await.unwrap_or_default();
    match status {
        Ok(s) if s.success() => {
            // A live job's control socket is never drained, so only segment
            // (file) jobs report timing.
            if mode != StageTimingMode::Off && !stdout_passthrough {
                let progress = match &sidecar {
                    Some(sc) => tokio::fs::read_to_string(sc)
                        .await
                        .ok()
                        .map(|t| parse_progress(&t)),
                    None => None,
                };
                let timing = StageTiming {
                    spawn_secs,
                    first_frame_secs,
                    total_secs,
                    progress,
                    bench,
                };
                let _ = write_frame(wr, &WorkerEvent::Timing { job_id, timing }).await;
            }
            let out_bytes = match &file_out {
                Some(p) => tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0),
                None => 0,
            };
            let _ = write_frame(wr, &WorkerEvent::Done { job_id, out_bytes }).await;
        }
        Ok(s) => {
            // The streaming (Stdout) parent never drains the control socket,
            // so a Failed frame here is invisible — the request just sees a
            // 0-byte stream. Echo ffmpeg's stderr to the worker's own stderr
            // (inherited → server logs) so a broken encode is diagnosable.
            eprintln!(
                "transcode-worker: ffmpeg exited {s} for job {job_id:?}; stderr: {}",
                stderr_text.trim()
            );
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

/// Service one persistent-libav tiny op (`backend-lib` build): run the
/// blocking libav helper off the async reactor and reply on the control
/// socket. A libav SIGSEGV here takes down only this worker process — the
/// pool sees EOF and respawns; the server is untouched (V6).
#[cfg(all(unix, feature = "backend-lib"))]
async fn run_tiny(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    job_id: pharos_transcode::protocol::JobId,
    op: pharos_transcode::protocol::TinyOp,
) {
    use pharos_transcode::protocol::{write_frame, WorkerError, WorkerEvent};
    let ev = tokio::task::spawn_blocking(move || handle_tiny(job_id, op))
        .await
        .unwrap_or_else(|e| WorkerEvent::Failed {
            job_id,
            error: WorkerError::Other(format!("tiny op task join: {e}")),
        });
    let _ = write_frame(wr, &ev).await;
}

/// Blocking dispatch to the Phase-1 libav helpers. Maps their error kinds
/// to the wire `WorkerError` contract (`BadInput` = non-recoverable
/// malformed source; `Other` = internal/encode failure).
#[cfg(all(unix, feature = "backend-lib"))]
fn handle_tiny(
    job_id: pharos_transcode::protocol::JobId,
    op: pharos_transcode::protocol::TinyOp,
) -> pharos_transcode::protocol::WorkerEvent {
    use pharos_transcode::libav;
    use pharos_transcode::libav::frames::FrameError;
    use pharos_transcode::libav::probe::ProbeError;
    use pharos_transcode::protocol::{TinyOp, WorkerError, WorkerEvent};

    fn frame_err(job_id: pharos_transcode::protocol::JobId, e: FrameError) -> WorkerEvent {
        let error = match e {
            FrameError::BadInput(s) => WorkerError::BadInput(s),
            FrameError::Other(s) => WorkerError::Other(s),
        };
        WorkerEvent::Failed { job_id, error }
    }
    fn file_len(p: &std::path::Path) -> u64 {
        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
    }

    match op {
        TinyOp::Probe { input } => match libav::probe::probe(&input) {
            Ok(info) => WorkerEvent::ProbeResult {
                job_id,
                info: Box::new(info),
            },
            Err(ProbeError::BadInput(detail)) => {
                // The wire error now carries the real libav reason (e.g.
                // `open: Invalid data found`), so the caller's log says *why*
                // a file failed to demux instead of a bare "bad input".
                WorkerEvent::Failed {
                    job_id,
                    error: WorkerError::BadInput(format!("{}: {detail}", input.display())),
                }
            }
            Err(ProbeError::Other(s)) => WorkerEvent::Failed {
                job_id,
                error: WorkerError::Other(s),
            },
        },
        TinyOp::Image {
            input,
            seek_ms,
            width,
            quality,
            out,
            video_3d_format,
        } => match libav::image::extract_image_3d(
            &input,
            seek_ms,
            width,
            quality,
            video_3d_format,
            &out,
        ) {
            Ok(()) => WorkerEvent::Done {
                job_id,
                out_bytes: file_len(&out),
            },
            Err(e) => frame_err(job_id, e),
        },
        TinyOp::ExtractAttachment {
            input,
            stream_index,
            out,
        } => match libav::attachment::extract_attachment(&input, stream_index, &out) {
            Ok(()) => WorkerEvent::Done {
                job_id,
                out_bytes: file_len(&out),
            },
            Err(e) => frame_err(job_id, e),
        },
        TinyOp::ExtractAllAttachments { input, out_dir } => {
            match libav::attachment::extract_all_attachments(&input, &out_dir) {
                // out_bytes carries the number of attachments written.
                Ok(count) => WorkerEvent::Done {
                    job_id,
                    out_bytes: count as u64,
                },
                Err(e) => frame_err(job_id, e),
            }
        }
        TinyOp::Trickplay {
            input,
            interval_ms,
            width,
            grid,
            thumb_count,
            max_sheets,
            quality,
            out_dir,
            video_3d_format,
        } => match libav::trickplay::trickplay_sprite_3d(
            &input,
            interval_ms,
            width,
            grid,
            thumb_count,
            max_sheets,
            quality,
            video_3d_format,
            &out_dir,
        ) {
            // out_bytes carries the produced sheet count for this op.
            Ok(produced) => WorkerEvent::Done {
                job_id,
                out_bytes: produced as u64,
            },
            Err(e) => frame_err(job_id, e),
        },
        TinyOp::SrtToWebvtt { input, out } => match std::fs::read_to_string(&input) {
            Ok(srt) => {
                let vtt = libav::subtitle::convert_srt_to_webvtt(&srt);
                match std::fs::write(&out, vtt.as_bytes()) {
                    Ok(()) => WorkerEvent::Done {
                        job_id,
                        out_bytes: file_len(&out),
                    },
                    Err(e) => WorkerEvent::Failed {
                        job_id,
                        error: WorkerError::Io(format!("write {}: {e}", out.display())),
                    },
                }
            }
            Err(e) => WorkerEvent::Failed {
                job_id,
                error: WorkerError::Io(format!("read {}: {e}", input.display())),
            },
        },
        TinyOp::Waveform {
            input,
            samples_per_bin,
            target_bins,
        } => match libav::waveform::waveform_rms(&input, samples_per_bin, target_bins) {
            Ok(bins) => WorkerEvent::WaveformResult { job_id, bins },
            Err(e) => frame_err(job_id, e),
        },
        TinyOp::SubtitleWindows {
            input,
            stream_rel_idx,
        } => match libav::subtitle_windows::subtitle_event_windows(&input, stream_rel_idx) {
            Ok(windows) => WorkerEvent::SubtitleWindowsResult { job_id, windows },
            Err(e) => frame_err(job_id, e),
        },
        TinyOp::Integrity { input } => match libav::integrity::scan_integrity(&input) {
            Ok(report) => WorkerEvent::IntegrityResult {
                job_id,
                report: Box::new(report),
            },
            Err(e) => frame_err(job_id, e),
        },
        TinyOp::Fingerprint {
            input,
            start_ms,
            dur_ms,
        } => match libav::fingerprint::fingerprint_window(&input, start_ms, dur_ms) {
            Ok(points) => WorkerEvent::FingerprintResult { job_id, points },
            Err(e) => frame_err(job_id, e),
        },
        TinyOp::FingerprintMulti { input, windows } => {
            match libav::fingerprint::fingerprint_windows(&input, &windows) {
                Ok(points) => WorkerEvent::FingerprintMultiResult { job_id, points },
                Err(e) => frame_err(job_id, e),
            }
        }
    }
}

/// Tiny ops are unavailable in the spawn-only build — reply with a clean
/// non-recoverable error so a misconfigured caller surfaces it rather than
/// hanging.
#[cfg(all(unix, not(feature = "backend-lib")))]
async fn run_tiny(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    job_id: pharos_transcode::protocol::JobId,
    _op: pharos_transcode::protocol::TinyOp,
) {
    use pharos_transcode::protocol::{write_frame, WorkerError, WorkerEvent};
    let _ = write_frame(
        wr,
        &WorkerEvent::Failed {
            job_id,
            error: WorkerError::Other(
                "libav backend not built (build with --features backend-lib)".into(),
            ),
        },
    )
    .await;
}
