use actix_web::{web, App, HttpServer};
use clap::Parser;
use pharos_server::{
    cli::{AdminOp, Cli, Cmd},
    config::Config,
    health::{ReadinessError, ReadinessHandle},
    image_cache::ImageCache,
    middleware::RedMetrics,
    obs, router,
    state::AppState,
    sync::GroupRegistry,
};
use pharos_store_sqlx::sqlite::SqliteStore;
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
    #[error("store: {0}")]
    Store(#[from] pharos_store_sqlx::StoreError),
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
    tracing::info!(bind = %cfg.server.bind, db = %cfg.database.url, "starting pharos");

    let stores = SqliteStore::connect(&cfg.database.url).await?;
    let mut state = AppState::new(stores, cfg.server.name.clone());
    if let Some(cache_dir) = cfg.server.image_cache_dir.clone() {
        state = state.with_image_cache(ImageCache::new(cache_dir));
    }
    let app_state = web::Data::new(state);
    let group_registry = web::Data::new(GroupRegistry::spawn());

    // Probes whose readiness must flip true before /readyz returns 200.
    let readiness = ReadinessHandle::spawn(&["process", "store"]);
    readiness.mark("process").await?;
    readiness.mark("store").await?;

    let handle_for_app = readiness.clone();
    let ui_dir = cfg.server.ui_dir.clone();
    HttpServer::new(move || {
        let mut app = App::new()
            .app_data(web::Data::new(handle_for_app.clone()))
            .app_data(app_state.clone())
            .app_data(group_registry.clone())
            .wrap(RedMetrics)
            .wrap(TracingLogger::default())
            .configure(router::configure);
        if let Some(path) = ui_dir.as_ref() {
            // Dioxus default-routes the SPA in hash mode unless configured
            // otherwise, so a single `Files::new` with `index_file` is
            // enough — no history-mode fallback needed.
            app = app.service(
                actix_files::Files::new("/ui", path)
                    .index_file("index.html")
                    .use_last_modified(true),
            );
        }
        app
    })
    .bind(&cfg.server.bind)?
    .run()
    .await?;
    Ok(())
}
