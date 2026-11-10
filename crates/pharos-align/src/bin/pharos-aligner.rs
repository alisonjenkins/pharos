//! The lyric word aligner service; see `pharos_align::http` for the API.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use actix_web::{web, App, HttpServer};
use pharos_align::config::Config;
use pharos_align::device::{self, DeviceMode};
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
    // The resolved path, not /proc/self/exe, so ps and ss name the process
    // pharos-aligner rather than "exe".
    let exe = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
    let mut cmd = std::process::Command::new(exe);
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

/// The CPU pass, its spans tagged with `reason`.
fn cpu_model(
    weights: &Path,
    threads: usize,
    batch_samples: usize,
    reason: &str,
) -> Result<Box<dyn Model>, String> {
    let weights = Weights::open(weights).map_err(|e| e.to_string())?;
    let model = CpuModel::load(weights, threads).map_err(|e| e.to_string())?;
    Ok(Box::new(CpuAligner {
        model,
        batch_samples,
        reason: reason.into(),
    }))
}

/// Every bucket's window through the GPU pass before readiness: an adapter
/// that cannot run the model is caught at start, and the first song finds
/// its kernels compiled.
#[cfg(feature = "vulkan")]
fn check_gpu(
    weights: &Path,
    adapter: &device::Adapter,
) -> Result<pharos_align::forward::vulkan::VulkanModel, String> {
    use pharos_align::forward::vulkan::VulkanModel;
    let run = || -> Result<(VulkanModel, Vec<(usize, Duration)>), String> {
        let w = Weights::open(weights).map_err(|e| e.to_string())?;
        let m = VulkanModel::load(&w, device::burn_device(adapter)).map_err(|e| e.to_string())?;
        let timed = m.warm_up()?;
        Ok((m, timed))
    };
    let started = std::time::Instant::now();
    let (model, timed) =
        std::panic::catch_unwind(run).unwrap_or_else(|_| Err("the GPU pass panicked".into()))?;
    let per_bucket: Vec<String> = timed
        .iter()
        .map(|(b, t)| format!("{}s={}ms", b / 16_000, t.as_millis()))
        .collect();
    tracing::info!(
        buckets = timed.len(),
        total_ms = started.elapsed().as_millis() as u64,
        per_bucket = %per_bucket.join(" "),
        "GPU warm-up"
    );
    Ok(model)
}

/// R20: compiled kernels persist under `PHAROS_ALIGNER_CACHE_DIR` when set.
#[cfg(feature = "vulkan")]
fn kernel_cache(dir: Option<&Path>) -> String {
    match dir {
        None => "off".into(),
        Some(d) if pharos_align::forward::vulkan::use_kernel_cache(d) => d.display().to_string(),
        Some(_) => "off_already_configured".into(),
    }
}

/// R10 and R11: probe (unless forced to the CPU), choose, check the GPU,
/// and log the outcome once.
/// The model the GPU warm-up loaded, which the first song then uses rather
/// than uploading the weights again.
#[cfg(feature = "vulkan")]
type Warmed = Option<pharos_align::forward::vulkan::VulkanModel>;
#[cfg(not(feature = "vulkan"))]
type Warmed = ();

fn choose_device(config: &Config) -> Result<(device::Choice, Warmed), String> {
    let adapters = if config.device == DeviceMode::Cpu {
        Vec::new()
    } else {
        device::probe()
    };
    let choice = device::choose(config.device, &adapters).map_err(|e| e.to_string())?;
    #[cfg(not(feature = "vulkan"))]
    let warmed = ();
    #[cfg(feature = "vulkan")]
    let (choice, warmed) = match &choice.device {
        device::Device::Vulkan(a) => {
            let cache = kernel_cache(config.cache_dir.as_deref());
            tracing::info!(kernel_cache = %cache, "GPU kernel cache");
            let (checked, warmed) = match check_gpu(&config.weights, a) {
                Ok(m) => (Ok(()), Some(m)),
                Err(e) => (Err(e), None),
            };
            let choice = device::after_warmup(config.device, choice.clone(), checked)
                .map_err(|e| e.to_string())?;
            (choice, warmed)
        }
        device::Device::Cpu => (choice, None),
    };
    let adapter = match &choice.device {
        device::Device::Vulkan(a) => a.name.as_str(),
        device::Device::Cpu => "none",
    };
    let seen: Vec<&str> = adapters.iter().map(|a| a.name.as_str()).collect();
    tracing::info!(
        mode = %config.device.label(),
        device = %choice.device.label(),
        reason = %choice.reason,
        adapter = %adapter,
        adapters_seen = ?seen,
        "forward device"
    );
    Ok((choice, warmed))
}

/// Builds the chosen pass on first use and after an idle unload; on the GPU,
/// with the CPU pass behind it for songs the GPU fails.
fn loader(config: &Config, choice: device::Choice, warmed: Warmed) -> Loader {
    let (weights, threads, batch) = (config.weights.clone(), config.threads, config.batch_samples);
    let reason = choice.reason;
    match choice.device {
        device::Device::Cpu => Box::new(move || cpu_model(&weights, threads, batch, &reason)),
        #[cfg(feature = "vulkan")]
        device::Device::Vulkan(adapter) => {
            let warmed = std::sync::Mutex::new(warmed);
            Box::new(move || {
                use pharos_align::forward::fallback::Fallback;
                use pharos_align::forward::vulkan::VulkanModel;
                use pharos_align::forward::VulkanAligner;
                let model = match warmed.lock().ok().and_then(|mut w| w.take()) {
                    Some(m) => m,
                    None => {
                        let w = Weights::open(&weights).map_err(|e| e.to_string())?;
                        VulkanModel::load(&w, device::burn_device(&adapter))
                            .map_err(|e| e.to_string())?
                    }
                };
                let gpu = VulkanAligner {
                    model,
                    reason: reason.clone(),
                    adapter: adapter.name.clone(),
                };
                let weights = weights.clone();
                let cpu = Box::new(move || cpu_model(&weights, threads, batch, "gpu_fallback"));
                Ok(Box::new(Fallback::new(Box::new(gpu), cpu)) as Box<dyn Model>)
            })
        }
        #[cfg(not(feature = "vulkan"))]
        device::Device::Vulkan(_) => {
            let () = warmed;
            Box::new(|| Err("built without the vulkan feature".into()))
        }
    }
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
    let (choice, warmed) = choose_device(&config)?;
    let loader = loader(&config, choice, warmed);
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
        batch_samples = config.batch_samples,
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
