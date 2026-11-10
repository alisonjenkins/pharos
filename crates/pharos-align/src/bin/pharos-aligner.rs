//! The lyric word aligner service; see `pharos_align::http` for the API.

use std::sync::Arc;
use std::time::Duration;

use actix_web::{web, App, HttpServer};
use pharos_align::config::Config;
use pharos_align::forward::cpu::CpuModel;
use pharos_align::forward::CpuAligner;
use pharos_align::http::{configure, AppState};
use pharos_align::obs::{self, Metrics};
use pharos_align::pin;
use pharos_align::service::{Decoder, Engine, Loader, Model};
use pharos_align::weights::Weights;

/// Workers only parse and answer; songs run on the blocking pool, one at a
/// time. The default of one a core is 44 idle workers on the production node.
const WORKERS: usize = 2;
/// How often the idle reaper looks, capped by the idle timeout itself.
const IDLE_CHECK: Duration = Duration::from_secs(30);

/// OpenBLAS is the OpenMP build, and libgomp reads its settings once, as it
/// loads. Left at its defaults, every GEMM ran a team of one thread a core
/// that spun between calls on the cores the forward pass's own threads need
/// next, and a song took twice as long. So the process starts again with the
/// team sized to `threads` and waiting passively, unless they were set.
fn openmp_env(threads: usize) {
    let mut cmd = std::process::Command::new("/proc/self/exe");
    let mut changed = false;
    for (name, value) in [
        ("OMP_NUM_THREADS", threads.to_string()),
        ("OMP_WAIT_POLICY", "passive".into()),
    ] {
        if std::env::var_os(name).is_none() {
            cmd.env(name, value);
            changed = true;
        }
    }
    if !changed {
        return;
    }
    use std::os::unix::process::CommandExt as _;
    let err = cmd.args(std::env::args_os().skip(1)).exec();
    eprintln!("re-exec with the OpenMP settings failed, songs will run slower: {err}");
}

/// Where the kernel lists each CPU's core and cache siblings.
const SYSFS_CPU: &str = "/sys/devices/system/cpu";

/// Pins this process before any pool starts, so every worker inherits it.
/// Returns what was decided, for the start log; a failure only costs speed.
fn pin_threads(mode: pin::PinMode, threads: usize) -> (pin::Plan, Option<String>) {
    let allowed = match pin::allowed() {
        Ok(a) => a,
        Err(e) => {
            let plan = pin::Plan {
                cpus: None,
                reason: "affinity_unreadable",
            };
            return (plan, Some(format!("sched_getaffinity: {e}")));
        }
    };
    let plan = pin::plan(
        mode,
        &pin::topology(std::path::Path::new(SYSFS_CPU), &allowed),
        threads,
    );
    let err = plan
        .cpus
        .as_deref()
        .and_then(|cpus| pin::apply(cpus).err())
        .map(|e| format!("sched_setaffinity {:?}: {e}", plan.cpus));
    (plan, err)
}

fn main() -> std::process::ExitCode {
    let config = match Config::from_env(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(e) = pharos_align::forward::cpu::check_cpu() {
        eprintln!("{e}");
        return std::process::ExitCode::FAILURE;
    }
    openmp_env(config.threads);
    let pinned = pin_threads(config.pin, config.threads);
    match actix_web::rt::System::new().block_on(run(config, pinned)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "pharos-aligner stopped");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(config: Config, (plan, pin_err): (pin::Plan, Option<String>)) -> Result<(), String> {
    let traced = obs::init(config.otlp_endpoint.as_deref());
    let cpus = plan
        .cpus
        .as_ref()
        .map_or("unpinned".to_owned(), |c| format!("{c:?}"));
    match pin_err {
        None => {
            tracing::info!(mode = %config.pin.label(), reason = %plan.reason, cpus = %cpus, "thread pinning")
        }
        Some(e) => {
            tracing::warn!(mode = %config.pin.label(), reason = %plan.reason, cpus = %cpus, error = %e, "thread pinning failed, running unpinned")
        }
    }
    let metrics = Metrics::install()?;
    let ffmpeg = std::env::var("PHAROS_ALIGNER_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let decoder: Decoder =
        Box::new(move |path, start, end| pharos_align::audio::decode(&ffmpeg, path, start, end));
    let (weights, threads) = (config.weights.clone(), config.threads);
    let loader: Loader = Box::new(move || {
        let weights = Weights::open(&weights).map_err(|e| e.to_string())?;
        let model = CpuModel::load(weights, threads).map_err(|e| e.to_string())?;
        Ok(Box::new(CpuAligner { model }) as Box<dyn Model>)
    });
    let engine = Arc::new(Engine::new(loader, decoder, config.idle_unload));

    if let Some(idle) = config.idle_unload {
        let engine = Arc::clone(&engine);
        std::thread::Builder::new()
            .name("idle-reaper".into())
            .spawn(move || loop {
                std::thread::sleep(IDLE_CHECK.min(idle));
                engine.unload_if_idle();
            })
            .map_err(|e| format!("idle reaper thread: {e}"))?;
    }

    tracing::info!(
        listen = %format!("{}:{}", config.host, config.port),
        media_root = %config.media_root.display(),
        weights = %config.weights.display(),
        idle_unload_s = %config.idle_unload.map_or("off".to_owned(), |d| d.as_secs().to_string()),
        threads = config.threads,
        traces = %if traced { config.otlp_endpoint.as_deref().unwrap_or("on") } else { "off" },
        "serving pharos-aligner"
    );
    let state = web::Data::new(AppState {
        engine,
        media_root: config.media_root,
        metrics: Some(metrics),
    });
    let served = HttpServer::new(move || App::new().configure(configure(state.clone())))
        .workers(WORKERS)
        .bind((config.host.as_str(), config.port))
        .map_err(|e| format!("bind {}:{}: {e}", config.host, config.port))?
        .run()
        .await
        .map_err(|e| format!("server: {e}"));
    // The last spans flush while the runtime that drives the gRPC channel
    // still runs; blocking on it from this, its only thread, would deadlock.
    let _ = actix_web::rt::task::spawn_blocking(obs::shutdown).await;
    served
}
