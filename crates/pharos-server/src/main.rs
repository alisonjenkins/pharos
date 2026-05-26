use actix_cors::Cors;
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
            AdminOp::SeedPlaywrightUser => seed_playwright_user(&cfg).await?,
        },
    }
    Ok(())
}

async fn seed_playwright_user(cfg: &Config) -> Result<(), AppError> {
    use pharos_core::{
        MediaItem, MediaKind, MediaStore, SecretString, UserId, UserPolicy, UserRecord, UserStore,
    };
    use pharos_server::auth::BuiltinAuth;

    let stores = SqliteStore::connect(&cfg.database.url).await?;
    let auth = BuiltinAuth::new(stores.clone());

    let hash = auth
        .hash_password(&SecretString::new("playwright-test-pw"))
        .map_err(|e| AppError::Io(std::io::Error::other(e.to_string())))?;
    let user = UserRecord {
        id: UserId::new(),
        name: "playwright".into(),
        password_hash: hash,
        policy: UserPolicy { admin: true },
    };
    if let Err(e) = stores.create(user).await {
        if !matches!(e, pharos_core::AuthError::Conflict) {
            return Err(AppError::Io(std::io::Error::other(e.to_string())));
        }
    }

    // Generate a 5-second H.264/AAC MP4 fixture via ffmpeg so the
    // playback Playwright test has real bytes to stream. Path is
    // /tmp/pharos-playwright-media/fixture.mp4 — overwritten each run
    // so a stale file from a code change doesn't shadow new ffmpeg
    // args.
    let fixture_dir = std::path::PathBuf::from("/tmp/pharos-playwright-media");
    tokio::fs::create_dir_all(&fixture_dir)
        .await
        .map_err(AppError::Io)?;
    // WebM + VP9 + Opus so the FOSS Chromium shipped with Playwright
    // (no proprietary codec support) can actually decode it. H.264 fails
    // with DEMUXER_ERROR_NO_SUPPORTED_STREAMS under headless chromium.
    let fixture_path = fixture_dir.join("fixture.webm");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=5:size=320x240:rate=15",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=5",
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-row-mt",
            "1",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "libopus",
            "-shortest",
        ])
        .arg(&fixture_path)
        .status()
        .await
        .map_err(AppError::Io)?;
    if !status.success() {
        return Err(AppError::Io(std::io::Error::other(
            "ffmpeg fixture generation failed",
        )));
    }

    for (i, kind, path) in [
        (1u64, MediaKind::Movie, fixture_path.clone()),
        (2, MediaKind::Movie, fixture_path.clone()),
        (3, MediaKind::Episode, fixture_path.clone()),
        (4, MediaKind::Audio, fixture_path.clone()),
    ] {
        let item = MediaItem {
            id: i,
            path,
            title: format!("Playwright Title {i}"),
            kind,
        };
        let _ = stores.put(item).await;
    }
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    writeln!(
        lock,
        "seeded: user='playwright' password='playwright-test-pw' (admin), 4 items, fixture={}",
        fixture_path.display()
    )?;
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
        // Permissive CORS so browser-hosted Jellyfin clients (jellyfin-web,
        // Dioxus UI served from a separate origin) can reach the API.
        // Production should narrow this — tracked under §B if it bites.
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header()
            .expose_any_header()
            .max_age(3600);
        let mut app = App::new()
            .app_data(web::Data::new(handle_for_app.clone()))
            .app_data(app_state.clone())
            .app_data(group_registry.clone())
            .wrap(cors)
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
