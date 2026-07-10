//! `transcode-tool` — standalone harness for the transcode scheduler.
//!
//! Exercises the scheduler / device-table / worker-pool machinery end to
//! end (real `transcode-worker` subprocesses shelling to ffmpeg) without
//! building the rest of pharos. Subcommands:
//!
//!   transcode-tool detect
//!   transcode-tool run   <input> <output>
//!   transcode-tool stress <input> [-n N] [--saturate]
//!   transcode-tool bench <input>
//!
//! Device binary discovery uses the same `ProcSpawner` the server uses;
//! point it at a worker build via `PHAROS_TRANSCODE_WORKER` if needed.

#[cfg(not(unix))]
fn main() {
    eprintln!("transcode-tool is unix-only");
    std::process::exit(1);
}

#[cfg(unix)]
fn main() {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("transcode-tool: tokio runtime init failed: {e}");
            std::process::exit(1);
        }
    };
    let code = rt.block_on(real_main());
    std::process::exit(code);
}

#[cfg(unix)]
async fn real_main() -> i32 {
    use std::path::PathBuf;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");
    match cmd {
        "detect" => detect().await,
        "run" => {
            let (Some(input), Some(output)) = (args.get(1), args.get(2)) else {
                eprintln!("usage: transcode-tool run <input> <output>");
                return 2;
            };
            run_one(PathBuf::from(input), PathBuf::from(output)).await
        }
        "stress" => {
            let Some(input) = args.get(1) else {
                eprintln!("usage: transcode-tool stress <input> [-n N] [--saturate]");
                return 2;
            };
            let n = parse_flag_usize(&args, "-n").unwrap_or(16);
            let saturate = args.iter().any(|a| a == "--saturate");
            stress(PathBuf::from(input), n, saturate).await
        }
        "bench" => {
            let Some(input) = args.get(1) else {
                eprintln!("usage: transcode-tool bench <input>");
                return 2;
            };
            bench(PathBuf::from(input)).await
        }
        "probe" => {
            let Some(input) = args.get(1) else {
                eprintln!("usage: transcode-tool probe <input>");
                return 2;
            };
            probe_one(PathBuf::from(input)).await
        }
        _ => {
            eprintln!(
                "transcode-tool <detect|run|stress|bench|probe>\n  \
                 detect                         list encoders + slot caps\n  \
                 run    <input> <output>        transcode one file\n  \
                 stress <input> [-n N] [--saturate]   N concurrent jobs\n  \
                 bench  <input>                 per-device throughput\n  \
                 probe  <input>                 probe one file via the libav worker (dumps streams)"
            );
            2
        }
    }
}

#[cfg(unix)]
fn parse_flag_usize(args: &[String], flag: &str) -> Option<usize> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1)?.parse().ok()
}

/// Probe ONE file through the exact deployed path — the libav worker pool —
/// and dump the extracted streams. Purpose-built to confirm subtitle /
/// attachment extraction on a single known file before committing to a
/// whole-library scan. Exits 0 on success, 1 on probe failure.
#[cfg(unix)]
async fn probe_one(input: std::path::PathBuf) -> i32 {
    let pool = pharos_transcode::worker::LibavWorkerPool::with_discovered_bin();
    let info = match pool.probe(&input).await {
        Ok(info) => info,
        Err(e) => {
            eprintln!("probe failed: {e}");
            return 1;
        }
    };
    let p = &info.probe;
    println!("kind: {:?}", info.kind);
    println!(
        "container={:?} duration_ms={:?} {}x{}",
        p.container,
        p.duration_ms,
        p.width.unwrap_or(0),
        p.height.unwrap_or(0),
    );
    println!("video_codec={:?}", p.video_codec);
    println!("audio_tracks: {}", p.audio_tracks.len());
    for a in &p.audio_tracks {
        println!(
            "  [a] idx={} codec={:?} ch={:?} lang={:?} title={:?} default={}",
            a.stream_index, a.codec, a.channels, a.language, a.title, a.is_default,
        );
    }
    println!("subtitle_tracks: {}", p.subtitle_tracks.len());
    for s in &p.subtitle_tracks {
        println!(
            "  [s] idx={} codec={:?} lang={:?} title={:?} default={} forced={}",
            s.stream_index, s.codec, s.language, s.title, s.is_default, s.is_forced,
        );
    }
    println!("attachments: {}", p.attachments.len());
    for at in &p.attachments {
        println!(
            "  [t] idx={} codec={:?} filename={:?} mime={:?}",
            at.stream_index, at.codec, at.filename, at.mime_type,
        );
    }
    0
}

#[cfg(unix)]
fn cpu_permits() -> usize {
    // "use all CPUs" — full logical core count.
    pharos_transcode::device::default_cpu_permits()
}

/// H264/AAC into the container implied by `out`'s extension (default
/// MPEG-TS). Good enough for the harness; the server builds real opts.
#[cfg(unix)]
fn tool_opts(out: &std::path::Path) -> pharos_transcode::options::TranscodeOptions {
    use pharos_transcode::options::{AudioCodec, Container, TranscodeOptions, VideoCodec};
    let container = out
        .extension()
        .and_then(|e| e.to_str())
        .and_then(Container::from_name)
        .unwrap_or(Container::Mpegts);
    TranscodeOptions {
        container,
        video: Some(VideoCodec::H264),
        audio: Some(AudioCodec::Aac),
        video_bitrate_bps: None,
        audio_bitrate_bps: None,
        start_position_ticks: 0,
        duration_ticks: None,
        audio_source_stream_index: None,
        burn_subtitle_stream_index: None,
        continuous_audio_path: None,
    }
}

#[cfg(unix)]
async fn detect() -> i32 {
    use pharos_transcode::hwaccel::{detect_available, HwAccel};
    use pharos_transcode::protocol::WorkerId;
    use pharos_transcode::scheduler::WorkerSpawner;
    use pharos_transcode::worker::{exec, ProcSpawner};

    let ffmpeg = exec::ffmpeg_bin();
    let detected = detect_available(&ffmpeg).await;
    println!("ffmpeg binary:     {ffmpeg}");
    println!("detected hwaccels: {detected:?}");
    println!(
        "resolved (auto):   {:?}",
        HwAccel::Auto.resolve_auto(&detected)
    );
    println!("cpu permit budget: {}", cpu_permits());

    let spawner = ProcSpawner::new();
    println!("worker binary:     {}", spawner.worker_bin().display());
    match spawner.spawn(WorkerId(0)).await {
        Ok(w) => {
            // Downcast to read the handshake.
            // (ProcWorker is the only impl; print via Debug of devices.)
            println!("worker handshake:  OK (id {})", w.id());
            println!("  note: probed session caps land in the probe step;");
            println!("        defaults until then: hw=2 cpu={}", cpu_permits());
        }
        Err(e) => {
            eprintln!("worker handshake FAILED: {e}");
            return 1;
        }
    }
    0
}

#[cfg(unix)]
fn build_table(
    detected: &[pharos_transcode::hwaccel::HwAccel],
    hw_cap: usize,
) -> pharos_transcode::device::DeviceTable {
    use pharos_transcode::device::{enumerate, DeviceTable};
    use pharos_transcode::protocol::DeviceId;
    // One slot per concrete GPU (VAAPI expands per render node).
    let caps: Vec<(DeviceId, usize)> = enumerate(detected)
        .into_iter()
        .map(|d| (d, hw_cap))
        .collect();
    DeviceTable::from_probe(&caps, cpu_permits())
}

#[cfg(unix)]
async fn run_one(input: std::path::PathBuf, output: std::path::PathBuf) -> i32 {
    use pharos_transcode::hwaccel::detect_available;
    use pharos_transcode::scheduler::{SchedConfig, SinkRequest, TranscodeScheduler};
    use pharos_transcode::worker::{exec, ProcSpawner};
    use std::sync::Arc;
    use std::time::Instant;

    let detected = detect_available(&exec::ffmpeg_bin()).await;
    let table = build_table(&detected, 2);
    let sched =
        TranscodeScheduler::spawn(table, Arc::new(ProcSpawner::new()), SchedConfig::default());

    let opts = tool_opts(&output);
    let t0 = Instant::now();
    let res = sched
        .submit(
            input,
            opts,
            SinkRequest::FileDirect {
                out_path: output.clone(),
            },
        )
        .await;
    let elapsed = t0.elapsed();
    match res {
        Ok(done) => {
            let mb = done.out_bytes as f64 / 1.0e6;
            let mbps = mb / elapsed.as_secs_f64().max(1e-9);
            println!("device:     {}", done.device);
            println!("out:        {} ({:.2} MB)", output.display(), mb);
            println!("elapsed:    {:.3}s", elapsed.as_secs_f64());
            println!("throughput: {mbps:.2} MB/s");
            0
        }
        Err(e) => {
            eprintln!("transcode failed: {e}");
            1
        }
    }
}

#[cfg(unix)]
async fn stress(input: std::path::PathBuf, n: usize, saturate: bool) -> i32 {
    use pharos_transcode::hwaccel::detect_available;
    use pharos_transcode::scheduler::{SchedConfig, SinkRequest, TranscodeScheduler};
    use pharos_transcode::worker::{exec, ProcSpawner};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let detected = detect_available(&exec::ffmpeg_bin()).await;
    let table = build_table(&detected, 2);
    let cfg = if saturate {
        SchedConfig {
            pending_cap: 0,
            ..SchedConfig::default()
        }
    } else {
        SchedConfig::default()
    };
    let sched = Arc::new(TranscodeScheduler::spawn(
        table,
        Arc::new(ProcSpawner::new()),
        cfg,
    ));

    let tmp = std::env::temp_dir();
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for i in 0..n {
        let sched = sched.clone();
        let input = input.clone();
        let out = tmp.join(format!("transcode-stress-{i}.ts"));
        let opts = tool_opts(&out);
        handles.push(tokio::spawn(async move {
            sched
                .submit(input, opts, SinkRequest::FileDirect { out_path: out })
                .await
        }));
    }

    // Live load display until all jobs settle.
    let monitor = {
        let sched = sched.clone();
        tokio::spawn(async move {
            loop {
                if let Some(s) = sched.snapshot().await {
                    let parts: Vec<String> = s
                        .devices
                        .iter()
                        .map(|d| format!("{}={}/{}", d.id, d.in_use, d.capacity))
                        .collect();
                    println!(
                        "  [{:>5.1}s] {} | pending={} idle_workers={}",
                        t0.elapsed().as_secs_f64(),
                        parts.join(" "),
                        s.pending,
                        s.idle_workers
                    );
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
    };

    let mut ok = 0;
    let mut busy = 0;
    let mut failed = 0;
    for h in handles {
        match h.await {
            Ok(Ok(_)) => ok += 1,
            Ok(Err(pharos_transcode::scheduler::SchedError::Busy)) => busy += 1,
            Ok(Err(_)) => failed += 1,
            Err(_) => failed += 1,
        }
    }
    monitor.abort();
    println!(
        "\nstress: n={n} ok={ok} busy={busy} failed={failed} in {:.2}s",
        t0.elapsed().as_secs_f64()
    );
    // No-deadlock proof: every job reached a terminal state.
    if ok + busy + failed == n {
        println!("all jobs reached a terminal state (no deadlock)");
        0
    } else {
        eprintln!("LOST JOBS: {} unaccounted", n - (ok + busy + failed));
        1
    }
}

#[cfg(unix)]
async fn bench(input: std::path::PathBuf) -> i32 {
    use pharos_transcode::device::DeviceTable;
    use pharos_transcode::hwaccel::detect_available;
    use pharos_transcode::protocol::DeviceId;
    use pharos_transcode::scheduler::{SchedConfig, SinkRequest, TranscodeScheduler};
    use pharos_transcode::worker::{exec, ProcSpawner};
    use std::sync::Arc;
    use std::time::Instant;

    let detected = detect_available(&exec::ffmpeg_bin()).await;
    let mut devices: Vec<DeviceId> = pharos_transcode::device::enumerate(&detected);
    devices.push(DeviceId::Cpu);

    let tmp = std::env::temp_dir();
    println!(
        "{:<14} {:>10} {:>10} {:>12}",
        "device", "elapsed", "MB", "MB/s"
    );
    for dev in devices {
        // Single-device table so the job lands exactly there. cpu_permits=0
        // (clamped to 1) keeps a CPU fallback present but best-first picks
        // the HW device under test.
        let table = match dev {
            DeviceId::Cpu => DeviceTable::from_probe(&[], cpu_permits()),
            hw => DeviceTable::from_probe(&[(hw, 1)], 0),
        };
        let sched =
            TranscodeScheduler::spawn(table, Arc::new(ProcSpawner::new()), SchedConfig::default());
        let out = tmp.join(format!("transcode-bench-{dev}.ts"));
        let opts = tool_opts(&out);
        let t0 = Instant::now();
        let res = sched
            .submit(
                input.clone(),
                opts,
                SinkRequest::FileDirect { out_path: out },
            )
            .await;
        let el = t0.elapsed().as_secs_f64();
        match res {
            Ok(done) => {
                let mb = done.out_bytes as f64 / 1.0e6;
                println!(
                    "{:<14} {:>9.3}s {:>10.2} {:>12.2}",
                    done.device.to_string(),
                    el,
                    mb,
                    mb / el.max(1e-9)
                );
            }
            Err(e) => println!("{:<14} {:>10} ({e})", dev.to_string(), "FAILED"),
        }
    }
    0
}
