use actix_cors::Cors;
use actix_web::{web, App, HttpServer};
use clap::Parser;
use pharos_cache::{HlsSegmentCache, ImageCache, SubtitleCache, TrickplayCache};
use pharos_discovery::live_tv::build_backend as build_live_tv_backend;
use pharos_server::{
    cli::{AdminOp, Cli, Cmd},
    config::Config,
    health::{ReadinessError, ReadinessHandle},
    middleware::{LowercasePath, RedMetrics},
    obs, router,
    state::AppState,
    sync_resolver,
};
use pharos_sync::{ws::TokenResolverData, GroupRegistry};
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
    use pharos_scanner::FsScanner;
    #[cfg(not(all(unix, feature = "ffmpeg-lib")))]
    use pharos_scanner::FfmpegProber;

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
    // P48 — `ffmpeg-lib` build probes in-process via a resident libav
    // worker (no ffprobe fork per file); the spawn build keeps ffprobe.
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    let scanner = FsScanner::new(pharos_scanner::LibavProber::with_discovered_bin())
        .with_rate_limit_ms(cfg.server.scan_rate_limit_ms);
    #[cfg(not(all(unix, feature = "ffmpeg-lib")))]
    let scanner =
        FsScanner::new(FfmpegProber::new()).with_rate_limit_ms(cfg.server.scan_rate_limit_ms);

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

/// Build the load-balancing transcode scheduler. Returns `None` (so the
/// caller falls back to the inline ffmpeg path) when the worker binary
/// can't be brought up — validated by spawning one probe worker + reading
/// its handshake before committing.
async fn build_transcode_scheduler(
    detected: &[pharos_transcode::HwAccel],
    hw_session_cap: usize,
    probe_caps: bool,
) -> Option<pharos_transcode::scheduler::TranscodeScheduler> {
    use pharos_transcode::device::{default_cpu_permits, enumerate, DeviceTable};
    use pharos_transcode::probe::{probe_device_caps, ProbeConfig};
    use pharos_transcode::protocol::{DeviceId, WorkerId};
    use pharos_transcode::scheduler::{SchedConfig, TranscodeScheduler, WorkerSpawner};
    use pharos_transcode::worker::ProcSpawner;

    // Confirm a worker spawns + handshakes before wiring the scheduler.
    // The probe worker is dropped (killed) immediately.
    let probe = ProcSpawner::new();
    if let Err(e) = probe.spawn(WorkerId(0)).await {
        tracing::warn!(error = %e, worker = %probe.worker_bin().display(), "transcode worker probe failed");
        return None;
    }

    let devices = enumerate(detected);
    let caps: Vec<(DeviceId, usize)> = if probe_caps && !devices.is_empty() {
        // Learn real session caps; fall back to the configured cap for
        // any device the probe couldn't measure.
        let probed = probe_device_caps(&devices, &ProbeConfig::default()).await;
        let caps: Vec<(DeviceId, usize)> = devices
            .iter()
            .map(|d| {
                let c = probed
                    .caps
                    .iter()
                    .find(|(pd, _)| pd == d)
                    .map(|(_, c)| *c)
                    .unwrap_or_else(|| hw_session_cap.max(1));
                (*d, c)
            })
            .collect();
        for (d, c) in &caps {
            tracing::info!(device = %d, sessions = c, "probed device session cap");
        }
        caps
    } else {
        devices
            .into_iter()
            .map(|d| (d, hw_session_cap.max(1)))
            .collect()
    };
    let table = DeviceTable::from_probe(&caps, default_cpu_permits());
    tracing::info!(
        devices = caps.len(),
        cpu_permits = default_cpu_permits(),
        "transcode scheduler device table built"
    );
    Some(TranscodeScheduler::spawn(
        table,
        std::sync::Arc::new(ProcSpawner::new()),
        SchedConfig::default(),
    ))
}

async fn serve(cfg: Config) -> Result<(), AppError> {
    tracing::info!(bind = %cfg.server.bind, db = %cfg.database.url, "starting pharos");

    let stores = SqliteStore::connect(&cfg.database.url).await?;
    let token_resolver: TokenResolverData = sync_resolver::build(stores.clone());
    let mut state = AppState::load(stores, cfg.server.name.clone())
        .await?
        .with_media_roots(cfg.media.roots.clone());
    // P48 — one resident libav worker pool shared by the image + trickplay
    // caches (and the scanner prober) in the `ffmpeg-lib` build. Tiny ops
    // run in-process in crash-isolated workers; the fork/exec is amortised.
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    let libav_pool = pharos_transcode::worker::LibavWorkerPool::with_discovered_bin();
    if let Some(cache_dir) = cfg.server.image_cache_dir.clone() {
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        let image_cache = ImageCache::new(cache_dir).with_pool(libav_pool.clone());
        #[cfg(not(all(unix, feature = "ffmpeg-lib")))]
        let image_cache = ImageCache::new(cache_dir);
        state = state.with_image_cache(image_cache);
    }
    // P14 — resolve `auto` against the live `ffmpeg -hwaccels` output
    // once. Logs the chosen encoder so admins see what's wired.
    let detected = pharos_transcode::hwaccel::detect_available("ffmpeg").await;
    let accel = cfg.server.hwaccel.resolve_auto(&detected);
    tracing::info!(?accel, ?detected, "hardware encoder resolved");
    // Bring up the load-balancing transcode scheduler (multi-GPU +
    // all-CPU, crash-isolated workers) once, shared by the HLS cache
    // (segment path) + the live/uncached path on AppState. Falls back to
    // inline ffmpeg when disabled or when the worker can't be brought up.
    let transcode_scheduler = if cfg.server.transcode_hw_session_cap > 0 {
        match build_transcode_scheduler(
            &detected,
            cfg.server.transcode_hw_session_cap,
            cfg.server.transcode_probe_caps,
        )
        .await
        {
            Some(sched) => {
                tracing::info!("transcode scheduler enabled (load-balanced workers)");
                Some(sched)
            }
            None => {
                tracing::warn!("transcode worker unavailable; using the inline ffmpeg path");
                None
            }
        }
    } else {
        None
    };
    if let Some(sched) = transcode_scheduler.as_ref() {
        state = state.with_transcode_scheduler(sched.clone());
    }
    if let Some(cache_dir) = cfg.server.transcode_cache_dir.clone() {
        let mut hls = HlsSegmentCache::new(cache_dir, cfg.server.transcode_cache_max_bytes)
            .with_hwaccel(accel);
        if let Some(sched) = transcode_scheduler.as_ref() {
            hls = hls.with_scheduler(sched.clone());
        }
        state = state.with_hls_cache(hls);
    }
    // P5 — subtitle cache, always on. Pure in-process; the only
    // tunables are bytes + entry cap. Disabled by setting both to 0.
    if cfg.server.subtitle_cache_max_bytes > 0 && cfg.server.subtitle_cache_max_entries > 0 {
        state = state.with_subtitle_cache(SubtitleCache::new(
            cfg.server.subtitle_cache_max_bytes,
            cfg.server.subtitle_cache_max_entries,
        ));
    }
    if !cfg.server.trickplay_widths.is_empty() {
        if let Some(cache_dir) = cfg.server.trickplay_cache_dir.clone() {
            let trickplay_cache =
                TrickplayCache::new(cache_dir, cfg.server.trickplay_cache_max_bytes);
            #[cfg(all(unix, feature = "ffmpeg-lib"))]
            let trickplay_cache = trickplay_cache.with_pool(libav_pool.clone());
            state = state.with_trickplay_cache(trickplay_cache);
        }
        state = state.with_trickplay_layout(
            cfg.server.trickplay_widths.clone(),
            cfg.server.trickplay_interval_ms,
        );
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
    state = state.with_played_threshold_pct(cfg.server.played_threshold_pct);
    state = state.with_scan_rate_limit_ms(cfg.server.scan_rate_limit_ms);
    let app_state = web::Data::new(state);
    let group_registry = web::Data::new(GroupRegistry::spawn());
    let token_resolver_data = web::Data::new(token_resolver);

    // Probes whose readiness must flip true before /readyz returns 200.
    let readiness = ReadinessHandle::spawn(&["process", "store"]);
    readiness.mark("process").await?;
    readiness.mark("store").await?;

    // T48 phase 2 — opt-in SSDP responder so LAN DLNA / UPnP control
    // points discover us. The task owns its own UDP socket; we keep a
    // handle so the runtime tears it down on shutdown.
    let _ssdp_guard = if cfg.server.ssdp_enabled {
        let advertise = cfg.server.ssdp_advertise_url.clone().unwrap_or_else(|| {
            format!("http://{}", cfg.server.bind.replace("0.0.0.0", "127.0.0.1"))
        });
        match pharos_discovery::ssdp::SsdpResponder::spawn(
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
            .app_data(token_resolver_data.clone())
            // actix runs `.wrap()` layers in REVERSE registration order
            // (last registered = outermost = first on ingress). We want
            // ingress order: cors -> LowercasePath -> RedMetrics ->
            // TracingLogger -> router, so that the path is lowercased
            // BEFORE RedMetrics/TracingLogger label it — otherwise
            // jellyfin-web's PascalCase paths miss the (lowercase-only)
            // route patterns and RedMetrics falls back to the concrete
            // URI, exploding Prometheus label cardinality (one series per
            // item id). To get that ingress order, register bottom-up.
            .wrap(TracingLogger::default())
            .wrap(RedMetrics)
            .wrap(LowercasePath)
            .wrap(cors)
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
    // P31 — shutdown reached. If the SSDP responder is alive, send
    // byebye frames so DLNA clients drop pharos immediately instead
    // of waiting out the CACHE-CONTROL TTL.
    if let Some(g) = _ssdp_guard.as_ref() {
        g.send_byebye().await;
    }
    Ok(())
}
