use actix_web::{App, HttpServer};
use clap::Parser;
use pharos_server::{
    cli::{AdminOp, Cli, Cmd},
    config::Config,
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

async fn serve(cfg: Config) -> std::io::Result<()> {
    tracing::info!(bind = %cfg.server.bind, "starting pharos");
    HttpServer::new(|| {
        App::new()
            .wrap(TracingLogger::default())
            .configure(router::configure)
    })
    .bind(&cfg.server.bind)?
    .run()
    .await
}
