use actix_cors::Cors;
use actix_web::{web, App, HttpServer};
use clap::Parser;
use pharos_server::{
    cli::{AdminOp, Cli, Cmd},
    config::Config,
    health::{ReadinessError, ReadinessHandle},
    hls_cache::HlsSegmentCache,
    image_cache::ImageCache,
    live_tv::build_backend as build_live_tv_backend,
    middleware::{LowercasePath, RedMetrics},
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
        Cmd::Scan => scan(&cfg).await?,
        Cmd::Admin { op } => match op {
            AdminOp::PrintConfig => {
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                writeln!(lock, "{cfg:#?}")?;
            }
            AdminOp::SeedPlaywrightUser => seed_playwright_user(&cfg).await?,
            AdminOp::CreatePlaywrightUser => create_playwright_user(&cfg).await?,
        },
    }
    Ok(())
}

async fn scan(cfg: &Config) -> Result<(), AppError> {
    use pharos_core::{MediaStore, Scanner};
    use pharos_scanner::{FfmpegProber, FsScanner};

    if cfg.media.roots.is_empty() {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        writeln!(
            lock,
            "no [media].roots configured — nothing to scan. Add roots = [\"…\"] to config.toml."
        )?;
        return Ok(());
    }

    let stores = SqliteStore::connect(&cfg.database.url).await?;
    let scanner = FsScanner::new(FfmpegProber::new());

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let mut total_imported: u64 = 0;
    let mut total_skipped: u64 = 0;
    for root in &cfg.media.roots {
        writeln!(lock, "scanning {}…", root.display())?;
        match scanner.scan(root.as_path()).await {
            Ok(items) => {
                let n = items.len();
                for item in items {
                    let id = item.id;
                    let path = item.path.clone();
                    if let Err(e) = stores.put(item).await {
                        writeln!(lock, "  put id={id} path={} err={e}", path.display())?;
                        total_skipped += 1;
                    } else {
                        total_imported += 1;
                    }
                }
                writeln!(lock, "  {n} items probed")?;
            }
            Err(e) => writeln!(lock, "  scan failed: {e}")?,
        }
    }
    writeln!(
        lock,
        "scan complete: imported={total_imported} skipped(conflict)={total_skipped}",
    )?;
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

    // Generate a 5-second WebM fixture via ffmpeg so the playback
    // Playwright test has real bytes to stream. Path is
    // /tmp/pharos-playwright-media/fixture.webm — overwritten each
    // run so a stale file from a code change doesn't shadow new
    // ffmpeg args.
    //
    // WebM + VP9 + Opus so the FOSS Chromium shipped with Playwright
    // (no proprietary codec support) can actually decode it. H.264 fails
    // with DEMUXER_ERROR_NO_SUPPORTED_STREAMS under headless chromium.
    let fixture_dir = std::path::PathBuf::from("/tmp/pharos-playwright-media");
    tokio::fs::create_dir_all(&fixture_dir)
        .await
        .map_err(AppError::Io)?;
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

    // Materialise four distinct paths so the store's UNIQUE(path)
    // constraint doesn't silently drop items 2/3/4. Each is a copy of
    // the same fixture bytes. Target dir = first configured media
    // root (writable in production / containers), falling back to
    // `fixture_dir` for the devShell case.
    let target_dir = cfg
        .media
        .roots
        .first()
        .cloned()
        .unwrap_or_else(|| fixture_dir.clone());
    tokio::fs::create_dir_all(&target_dir)
        .await
        .map_err(AppError::Io)?;
    let mut per_id_paths: Vec<(u64, MediaKind, std::path::PathBuf)> = Vec::new();
    for (i, kind) in [
        (1u64, MediaKind::Movie),
        (2, MediaKind::Movie),
        (3, MediaKind::Episode),
        (4, MediaKind::Audio),
    ] {
        let per_path = target_dir.join(format!("fixture-{i}.webm"));
        tokio::fs::copy(&fixture_path, &per_path)
            .await
            .map_err(AppError::Io)?;
        per_id_paths.push((i, kind, per_path));
    }
    for (i, kind, path) in per_id_paths {
        let item = MediaItem {
            id: i,
            path,
            title: format!("Playwright Title {i}"),
            kind,
            ..Default::default()
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

/// Like `seed_playwright_user` but stops after creating the user.
/// Used by dev-stack's CC-test-media flow where the media files are
/// populated by a separate one-shot container + `pharos scan`
/// registers them.
async fn create_playwright_user(cfg: &Config) -> Result<(), AppError> {
    use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
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
    match stores.create(user).await {
        Ok(()) => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            writeln!(
                lock,
                "created user 'playwright' (admin) password='playwright-test-pw'"
            )?;
        }
        Err(pharos_core::AuthError::Conflict) => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            writeln!(lock, "user 'playwright' already exists; leaving as-is")?;
        }
        Err(e) => return Err(AppError::Io(std::io::Error::other(e.to_string()))),
    }
    Ok(())
}

async fn serve(cfg: Config) -> Result<(), AppError> {
    tracing::info!(bind = %cfg.server.bind, db = %cfg.database.url, "starting pharos");

    let stores = SqliteStore::connect(&cfg.database.url).await?;
    let mut state = AppState::load(stores, cfg.server.name.clone())
        .await?
        .with_media_roots(cfg.media.roots.clone());
    if let Some(cache_dir) = cfg.server.image_cache_dir.clone() {
        state = state.with_image_cache(ImageCache::new(cache_dir));
    }
    if let Some(cache_dir) = cfg.server.transcode_cache_dir.clone() {
        state = state.with_hls_cache(HlsSegmentCache::new(
            cache_dir,
            cfg.server.transcode_cache_max_bytes,
        ));
    }
    if let Some(backend) = build_live_tv_backend(
        cfg.server.live_tv_m3u.clone(),
        cfg.server.live_tv_xmltv.clone(),
    )
    .await
    .map_err(|e| AppError::Io(std::io::Error::other(e.to_string())))?
    {
        state = state.with_live_tv(backend);
    }
    state = state.with_log_dir(cfg.obs.log_dir.clone());
    let app_state = web::Data::new(state);
    let group_registry = web::Data::new(GroupRegistry::spawn());

    // Probes whose readiness must flip true before /readyz returns 200.
    let readiness = ReadinessHandle::spawn(&["process", "store"]);
    readiness.mark("process").await?;
    readiness.mark("store").await?;

    // T48 phase 2 — opt-in SSDP responder so LAN DLNA / UPnP control
    // points discover us. The task owns its own UDP socket; we keep a
    // handle so the runtime tears it down on shutdown.
    let _ssdp_guard = if cfg.server.ssdp_enabled {
        let advertise = cfg.server.ssdp_advertise_url.clone().unwrap_or_else(|| {
            format!(
                "http://{}",
                cfg.server.bind.replace("0.0.0.0", "127.0.0.1")
            )
        });
        match pharos_server::ssdp::SsdpResponder::spawn(
            app_state.server_id.clone(),
            app_state.server_name.clone(),
            advertise,
        )
        .await
        {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!(error = %e, "ssdp responder failed to bind; LAN discovery disabled");
                None
            }
        }
    } else {
        None
    };

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
            // Lowercase the URI path before metrics + tracing capture
            // it, so RedMetrics labels and TracingLogger spans use the
            // canonical lowercase form (and label cardinality stays
            // bounded). actix runs wraps in registration order, so
            // this needs to sit between cors (outermost) and the
            // observability layers below.
            .wrap(LowercasePath)
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
