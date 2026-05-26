use actix_web::{web, App, HttpServer};
use clap::Parser;
use pharos_server::{
    cli::{AdminOp, Cli, Cmd},
    config::Config,
    health::{ReadinessError, ReadinessHandle},
    middleware::RedMetrics,
    obs, router,
};
use std::io::Write;
use tracing_actix_web::TracingLogger;

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("config: {0}")]
    Config(#[from] pharos_server::config::ConfigError),
    #[error("obs: {0}")]
    Obs(#[from] obs::ObsError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("readiness: {0}")]
    Readiness(#[from] ReadinessError),
}

#[actix_web::main]
async fn main() -> Result<(), AppError> {
    let cli = Cli::parse();
    let cfg = Config::from_path(&cli.config)?.apply_env();
    obs::init(&cfg.obs.log_level)?;

    match cli.cmd {
        Cmd::Serve => serve(cfg).await?,
        Cmd::Scan => tracing::info!("scan not yet implemented (T3)"),
        Cmd::Admin { op } => match op {
            AdminOp::PrintConfig => {
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                writeln!(lock, "{cfg:#?}")?;
            }
        },
    }
    Ok(())
}

async fn serve(cfg: Config) -> Result<(), AppError> {
    tracing::info!(bind = %cfg.server.bind, "starting pharos");

    // Probes whose readiness must flip true before /readyz returns 200.
    // Subsystems registered here as they come online (T2 store wiring, T3 scanner, …).
    let readiness = ReadinessHandle::spawn(&["process"]);
    readiness.mark("process").await?;

    let handle_for_app = readiness.clone();
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(handle_for_app.clone()))
            .wrap(RedMetrics)
            .wrap(TracingLogger::default())
            .configure(router::configure)
    })
    .bind(&cfg.server.bind)?
    .run()
    .await?;
    Ok(())
}
