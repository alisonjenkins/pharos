//! The lyric word aligner service; see `pharos_align::http` for the API.

use std::sync::Arc;
use std::time::Duration;

use actix_web::{web, App, HttpServer};
use pharos_align::config::Config;
use pharos_align::http::{configure, AppState};
use pharos_align::obs::{self, Metrics};
use pharos_align::service::{Decoder, Engine, Loader};

/// Workers only parse and answer; songs run on the blocking pool, one at a
/// time. The default of one a core is 44 idle workers on the production node.
const WORKERS: usize = 2;
/// How often the idle reaper looks, capped by the idle timeout itself.
const IDLE_CHECK: Duration = Duration::from_secs(30);

fn main() -> std::process::ExitCode {
    let config = match Config::from_env(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match actix_web::rt::System::new().block_on(run(config)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "pharos-aligner stopped");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(config: Config) -> Result<(), String> {
    let traced = obs::init(config.otlp_endpoint.as_deref());
    let metrics = Metrics::install()?;
    let ffmpeg = std::env::var("PHAROS_ALIGNER_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let decoder: Decoder =
        Box::new(move |path, start, end| pharos_align::audio::decode(&ffmpeg, path, start, end));
    let loader: Loader = Box::new(|| Err("this build has no acoustic model".into()));
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
