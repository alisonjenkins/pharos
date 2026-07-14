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
    state::{AppState, Stores},
    sync_resolver,
};
use pharos_sync::{ws::TokenResolverData, GroupRegistry};
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
    #[error("domain: {0}")]
    Domain(#[from] pharos_core::DomainError),
}

/// LIB-C1 — reconcile the `libraries` table from `[media]` config and
/// backfill `media_items.library_id` by path-prefix, returning the typed
/// libraries for `AppState`. Idempotent: re-runs on every boot, upserting
/// one row per configured root/typed-library (the typed kind/name winning
/// for a path listed both ways) and re-pointing each item's library_id.
async fn reconcile_libraries(
    stores: &Stores,
    cfg: &Config,
) -> Result<Vec<pharos_core::Library>, AppError> {
    use pharos_core::LibraryStore;
    use pharos_server::api::jellyfin::items::library_id_for_root;
    for (name, path, kind) in cfg.media.library_specs() {
        let wire_id = library_id_for_root(&path);
        stores
            .upsert_library(&name, &path.to_string_lossy(), kind, &wire_id)
            .await?;
    }
    let assigned = stores.backfill_library_ids().await?;
    let libraries = stores.libraries().await?;
    tracing::info!(
        libraries = libraries.len(),
        items_assigned = assigned,
        "reconciled typed libraries"
    );
    Ok(libraries)
}

#[actix_web::main]
async fn main() -> Result<(), AppError> {
    let cli = Cli::parse();
    let cfg = Config::from_path(&cli.config)?.apply_env();
    obs::init(&cfg.obs.log_level, cfg.obs.otlp_endpoint.as_deref())?;

    match cli.cmd {
        Cmd::Serve => serve(cfg).await?,
        Cmd::Scan { force } => scan(&cfg, force).await?,
        Cmd::Admin { op } => match op {
            AdminOp::PrintConfig => {
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                writeln!(lock, "{cfg:#?}")?;
            }
            AdminOp::CreateUser {
                name,
                password,
                admin,
            } => create_user(&cfg, &name, &password, admin).await?,
            AdminOp::ResetPassword { name, password } => {
                reset_password(&cfg, &name, &password).await?
            }
            #[cfg(debug_assertions)]
            AdminOp::SeedPlaywrightUser => seed_playwright_user(&cfg).await?,
            #[cfg(debug_assertions)]
            AdminOp::CreatePlaywrightUser => create_playwright_user(&cfg).await?,
            #[cfg(feature = "postgres")]
            AdminOp::DbMigrate { to } => db_migrate(&cfg, &to).await?,
        },
    }
    Ok(())
}

async fn scan(cfg: &Config, force: bool) -> Result<(), AppError> {
    #[cfg(not(all(unix, feature = "ffmpeg-lib")))]
    use pharos_scanner::FfmpegProber;
    use pharos_scanner::FsScanner;

    // LIB-C1 — the union of `[media].roots` + any `[[media.libraries]]`
    // paths, so a path declared only as a typed library is still scanned.
    let scan_roots = cfg.media.scan_roots();
    if scan_roots.is_empty() {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        writeln!(
            lock,
            "no [media].roots configured — nothing to scan. Add roots = [\"…\"] to config.toml."
        )?;
        return Ok(());
    }

    let stores = Stores::connect(&cfg.database.url).await?;
    // P48 — `ffmpeg-lib` build probes in-process via a resident libav
    // worker (no ffprobe fork per file); the spawn build keeps ffprobe.
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    let scanner = FsScanner::new(pharos_scanner::LibavProber::with_discovered_bin())
        .with_rate_limit_ms(cfg.server.scan_rate_limit_ms)
        .with_probe_concurrency_opt(cfg.server.scan_probe_concurrency)
        .with_force(force);
    #[cfg(not(all(unix, feature = "ffmpeg-lib")))]
    let scanner = FsScanner::new(FfmpegProber::new())
        .with_rate_limit_ms(cfg.server.scan_rate_limit_ms)
        .with_probe_concurrency_opt(cfg.server.scan_probe_concurrency)
        .with_force(force);

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    // LIB-A4 — drive the incremental `scan_into` path so the CLI gets the
    // structured outcome (added/updated/removed/skipped) plus the same
    // skip-unchanged + deletion-reconciliation behaviour the admin refresh
    // already uses, then print a per-run summary.
    let mut total_added: usize = 0;
    let mut total_updated: usize = 0;
    let mut total_removed: usize = 0;
    let mut total_skipped: usize = 0;
    for root in &scan_roots {
        writeln!(lock, "scanning {}…", root.display())?;
        match scanner.scan_into(root.as_path(), &stores).await {
            Ok(outcome) => {
                writeln!(
                    lock,
                    "  added={} updated={} removed={} skipped={}",
                    outcome.added.len(),
                    outcome.updated.len(),
                    outcome.removed.len(),
                    outcome.skipped,
                )?;
                total_added += outcome.added.len();
                total_updated += outcome.updated.len();
                total_removed += outcome.removed.len();
                total_skipped += outcome.skipped;
            }
            Err(e) => writeln!(lock, "  scan failed: {e}")?,
        }
    }
    writeln!(
        lock,
        "scan complete: added={total_added} updated={total_updated} removed={total_removed} skipped={total_skipped}",
    )?;
    Ok(())
}

#[cfg(debug_assertions)]
async fn seed_playwright_user(cfg: &Config) -> Result<(), AppError> {
    use pharos_core::{
        MediaItem, MediaKind, MediaStore, SecretString, UserId, UserPolicy, UserRecord, UserStore,
    };
    use pharos_server::auth::BuiltinAuth;

    let stores = Stores::connect(&cfg.database.url).await?;
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

/// Bootstrap a user (the supported way to create the first admin on a
/// fresh deployment). Idempotent — a name collision is reported and left
/// as-is. Password comes from the CLI arg / `PHAROS_ADMIN_PASSWORD` env
/// (see `cli::AdminOp::CreateUser`).
async fn create_user(
    cfg: &Config,
    name: &str,
    password: &str,
    admin: bool,
) -> Result<(), AppError> {
    use pharos_core::SecretString;
    use pharos_server::auth::{BuiltinAuth, CreateUserOutcome};

    let stores = Stores::connect(&cfg.database.url).await?;
    let auth = BuiltinAuth::new(stores.clone());

    let outcome = auth
        .create_user(name, &SecretString::new(password.to_string()), admin)
        .await
        .map_err(|e| AppError::Io(std::io::Error::other(e.to_string())))?;

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match outcome {
        CreateUserOutcome::Created => {
            let role = if admin { " (admin)" } else { "" };
            writeln!(lock, "created user '{name}'{role}")?;
        }
        CreateUserOutcome::AlreadyExists => {
            writeln!(lock, "user '{name}' already exists; leaving as-is")?;
        }
    }
    Ok(())
}

/// Reset an existing user's password (recovery path — see
/// `cli::AdminOp::ResetPassword`). Writes the store directly (Argon2id hash +
/// atomic `set_password`), so it works even when every admin is locked out.
async fn reset_password(cfg: &Config, name: &str, password: &str) -> Result<(), AppError> {
    use pharos_core::{SecretString, UserStore};
    use pharos_server::auth::BuiltinAuth;

    let stores = Stores::connect(&cfg.database.url).await?;
    let auth = BuiltinAuth::new(stores.clone());

    let record = stores
        .lookup_by_name(name)
        .await
        .map_err(|e| AppError::Io(std::io::Error::other(format!("look up user '{name}': {e}"))))?;
    let hash = auth
        .hash_password(&SecretString::new(password.to_string()))
        .map_err(|e| AppError::Io(std::io::Error::other(e.to_string())))?;
    stores
        .set_password(record.id, hash)
        .await
        .map_err(|e| AppError::Io(std::io::Error::other(e.to_string())))?;

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    writeln!(lock, "reset password for user '{name}'")?;
    Ok(())
}

/// One-shot cutover: copy every domain table from the currently-configured
/// SQLite store into `to` (a Postgres URL), then report per-table row
/// counts. `to`'s schema is created automatically (`PostgresStore::connect`
/// runs the postgres migrations), but the target must be an EMPTY database
/// — the copy inserts into every table, so a pre-populated target would
/// collide (see `cli::AdminOp::DbMigrate`).
#[cfg(feature = "postgres")]
async fn db_migrate(cfg: &Config, to: &str) -> Result<(), AppError> {
    use pharos_store_sqlx::{
        migrate::migrate_sqlite_to_postgres, postgres::PostgresStore, sqlite::SqliteStore,
    };

    if cfg.database.url.starts_with("postgres://") || cfg.database.url.starts_with("postgresql://")
    {
        return Err(AppError::Io(std::io::Error::other(format!(
            "db-migrate source must be the SQLite store, but [database].url is already \
             postgres: '{}'",
            cfg.database.url
        ))));
    }

    let src = SqliteStore::connect(&cfg.database.url).await?;
    let dst = PostgresStore::connect(to).await?;

    let report = migrate_sqlite_to_postgres(&src, &dst)
        .await
        .map_err(|e| AppError::Io(std::io::Error::other(format!("migration failed: {e}"))))?;

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let mut total = 0u64;
    for (table, rows) in &report.tables {
        writeln!(lock, "{table}: {rows} rows")?;
        total += rows;
    }
    writeln!(
        lock,
        "migration complete: {total} rows across {} tables",
        report.tables.len()
    )?;
    Ok(())
}

/// Like `seed_playwright_user` but stops after creating the user.
/// Used by dev-stack's CC-test-media flow where the media files are
/// populated by a separate one-shot container + `pharos scan`
/// registers them.
#[cfg(debug_assertions)]
async fn create_playwright_user(cfg: &Config) -> Result<(), AppError> {
    use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
    use pharos_server::auth::BuiltinAuth;

    let stores = Stores::connect(&cfg.database.url).await?;
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
        // Learn real session caps via a trial encode per device. A device the
        // probe COULDN'T encode on (cap 0 → omitted from `probed.caps`) is a
        // PHANTOM: ffmpeg lists h264_nvenc/vaapi as compiled-in even with no
        // usable GPU (no libcuda / no VA render node), and routing real jobs to
        // it just fails + retries on CPU — wasted work + latency. So EXCLUDE
        // unconfirmed devices rather than falling back to a guessed cap. No
        // working GPU ⇒ CPU-only; add a GPU later ⇒ the trial passes and it's
        // picked up automatically.
        let probed = probe_device_caps(&devices, &ProbeConfig::default()).await;
        let caps: Vec<(DeviceId, usize)> = devices
            .iter()
            .filter_map(|d| {
                probed
                    .caps
                    .iter()
                    .find(|(pd, _)| pd == d)
                    .map(|(_, c)| (*d, *c))
            })
            .collect();
        for d in &devices {
            if caps.iter().any(|(cd, _)| cd == d) {
                let c = caps
                    .iter()
                    .find(|(cd, _)| cd == d)
                    .map(|(_, c)| *c)
                    .unwrap_or(0);
                tracing::info!(device = %d, sessions = c, "hardware encoder confirmed by trial encode");
            } else {
                tracing::warn!(device = %d, "hardware encoder detected but trial encode FAILED (no usable GPU?) — excluded; transcodes stay on CPU");
            }
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

/// Resolve when the process is asked to shut down: SIGTERM (k8s pod
/// termination) or SIGINT (Ctrl-C in dev). On non-unix, Ctrl-C only.
async fn await_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // If a stream fails to install, fall back to never-resolving so the
        // other signal still drives shutdown (rather than exiting the task).
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler install failed");
                std::future::pending::<()>().await;
                return;
            }
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "SIGINT handler install failed");
                std::future::pending::<()>().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn serve(cfg: Config) -> Result<(), AppError> {
    tracing::info!(bind = %cfg.server.bind, db = %cfg.database.url, "starting pharos");

    let stores = Stores::connect(&cfg.database.url).await?;
    let token_resolver: TokenResolverData = sync_resolver::build(stores.clone());
    // LIB-C1 — reconcile the typed `libraries` table from config (one row
    // per configured root/typed-library, with its kind + stable wire id),
    // then backfill `media_items.library_id` by path-prefix so the
    // /Items?ParentId=<library id> pivot resolves via an indexed join.
    let libraries = reconcile_libraries(&stores, &cfg).await?;
    let scan_roots = cfg.media.scan_roots();
    let mut state = AppState::load(stores, cfg.server.name.clone())
        .await?
        .with_media_roots(scan_roots.clone())
        .with_libraries(libraries);
    // P48 — one resident libav worker pool shared by the image + trickplay
    // caches (and the scanner prober) in the `ffmpeg-lib` build. Tiny ops
    // run in-process in crash-isolated workers; the fork/exec is amortised.
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    let libav_pool = pharos_transcode::worker::LibavWorkerPool::with_discovered_bin();
    if let Some(cache_dir) = cfg.server.image_cache_dir.clone() {
        let image_cache =
            ImageCache::new(cache_dir).with_seek_seconds(cfg.server.image_seek_seconds);
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        let image_cache = image_cache.with_pool(libav_pool.clone());
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
        let mut sub_cache = SubtitleCache::new(
            cfg.server.subtitle_cache_max_bytes,
            cfg.server.subtitle_cache_max_entries,
        );
        // Persist under the cache PVC so a subtitle's whole-file demux is a
        // once-ever cost, not re-paid on every restart. Use the explicit dir if
        // set, else a `subtitles` sibling of an existing cache dir.
        let sub_dir = cfg.server.subtitle_cache_dir.clone().or_else(|| {
            cfg.server
                .transcode_cache_dir
                .as_ref()
                .or(cfg.server.image_cache_dir.as_ref())
                .and_then(|d| d.parent().map(|p| p.join("subtitles")))
        });
        if let Some(dir) = sub_dir {
            tracing::info!(dir = %dir.display(), "subtitle cache persisting to disk");
            sub_cache = sub_cache.with_disk(dir);
        }
        // Event-window scans (burn gating) share the resident libav pool and
        // yield to live playback via the adaptive bg-I/O gate (a whole-file
        // NFS read ungated starves the very stream that triggered it).
        let sub_cache = sub_cache.with_bg_gate(state.bg_io.clone());
        #[cfg(all(unix, feature = "ffmpeg-lib"))]
        let sub_cache = sub_cache.with_pool(libav_pool.clone());
        state = state.with_subtitle_cache(sub_cache);
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
        // Pre-generate sprite sheets in the background so the request path only
        // ever serves cached tiles (the handler 404s on a miss). Keeps the slow,
        // CPU-heavy whole-video generation off the playback path. The returned
        // handle lets PlaybackInfo bump an actively-watched show to the front.
        if let Some(tp) = state.trickplay.clone() {
            let prio = pharos_server::trickplay_backfill::spawn(
                state.stores.clone(),
                tp,
                state.subtitles.clone(),
                state.images.clone(),
                state.bg_io.clone(),
                cfg.server.trickplay_widths.clone(),
                cfg.server.trickplay_interval_ms,
            );
            state = state.with_trickplay_priority(prio);
        }
    }
    // T86 — intro/outro detection sweep on the resident libav pool, gated by
    // the same adaptive bg-IO semaphore so it yields to live playback.
    #[cfg(all(unix, feature = "ffmpeg-lib"))]
    pharos_server::segment_backfill::spawn(
        state.stores.clone(),
        state.bg_io.clone(),
        libav_pool.clone(),
    );
    // Cap the extracted-image cache. Unlike the trickplay/HLS caches it has no
    // in-line eviction, so on a large library posters/backdrops/thumbs/scaled
    // artwork can slowly fill the shared cache volume. A periodic janitor
    // recounts the tree and evicts the oldest files once it exceeds the cap
    // (evicted images are re-extracted on next request). Disabled when the cap
    // is 0 (unbounded — the historical behaviour).
    if cfg.server.image_cache_max_bytes > 0 {
        if let Some(images) = state.images.clone() {
            let cap = cfg.server.image_cache_max_bytes;
            // Phase B2 — only the bg-leader replica evicts, so two replicas
            // don't fight over (and re-extract) the same shared cache tree.
            let is_leader = state.is_bg_leader.clone();
            tokio::spawn(async move {
                // Warm up, then sweep every 10 minutes.
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
                loop {
                    tick.tick().await;
                    if !is_leader.load(std::sync::atomic::Ordering::Relaxed) {
                        continue;
                    }
                    images.enforce_cap(cap).await;
                }
            });
            tracing::info!(
                cap_bytes = cfg.server.image_cache_max_bytes,
                "image cache janitor enabled"
            );
        }
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
    state = state.with_scan_probe_concurrency(cfg.server.scan_probe_concurrency);
    let app_state = web::Data::new(state);

    // Phase B2 — elect the background-work singleton. Under Postgres this
    // holds an advisory lock so exactly one replica runs the DB-writing
    // background loops during a rolling-deploy surge; the loops below gate on
    // `is_bg_leader`. Under SQLite this wins immediately.
    pharos_server::state::AppState::spawn_bg_leadership(app_state.clone().into_inner());

    // LIB-A9 — tiered library change-detection. Each media root picks the best
    // mode it can sustain (native watch on a local fs when the `watch` feature
    // is built + enabled; periodic incremental rescan on network/fuse roots or
    // when watch is off; manual /Library/Refresh as the floor). Deltas
    // broadcast to /socket via the same A4 mechanism the manual refresh uses.
    // The guards keep native watches alive for the process lifetime.
    let _library_watch_guards = {
        use pharos_server::library_watch::{spawn_for_roots, WatchConfig};
        let watch_cfg = WatchConfig {
            watch_enabled: cfg.server.library_watch_enabled,
            poll_interval: std::time::Duration::from_secs(cfg.server.library_poll_interval_secs),
            rate_limit_ms: cfg.server.scan_rate_limit_ms,
        };
        let rate_limit_ms = cfg.server.scan_rate_limit_ms;
        let probe_concurrency = cfg.server.scan_probe_concurrency;
        // Adaptive backpressure — the periodic incremental rescan draws its
        // probe reads through the same shared I/O gate the server shrinks
        // during live playback, so it paces itself down while streaming.
        let io_gate = app_state.bg_io.clone();
        // P48 — same prober selection as the CLI scan + admin refresh paths.
        // The closure builds a fresh owned scanner per root (each spawned task
        // owns its own — the prober isn't required to be Clone).
        let make_scanner = move || {
            #[cfg(all(unix, feature = "ffmpeg-lib"))]
            {
                pharos_scanner::FsScanner::new(pharos_scanner::LibavProber::with_discovered_bin())
                    .with_rate_limit_ms(rate_limit_ms)
                    .with_probe_concurrency_opt(probe_concurrency)
                    .with_io_gate(io_gate.clone())
            }
            #[cfg(not(all(unix, feature = "ffmpeg-lib")))]
            {
                pharos_scanner::FsScanner::new(pharos_scanner::FfmpegProber::new())
                    .with_rate_limit_ms(rate_limit_ms)
                    .with_probe_concurrency_opt(probe_concurrency)
                    .with_io_gate(io_gate.clone())
            }
        };
        spawn_for_roots(app_state.clone(), &scan_roots, watch_cfg, make_scanner)
    };

    // Backfill the existing library's text subtitles into the persistent cache
    // (playback-gated) so a viewer's first play of any already-indexed title
    // finds warm subs, not a cold ~30 s whole-file demux. New/changed items are
    // warmed at scan time; this covers everything already on disk.
    // Adaptive background-I/O regulator: throttles scan probes + subtitle
    // warm-demuxes down to a trickle while a client is streaming so they never
    // starve live playback, then reopens when quiet.
    pharos_server::state::AppState::spawn_bg_io_regulator(app_state.clone().into_inner());
    // B34 — NO separate library-wide subtitle warm-all: the trickplay
    // backfill sweep already warms each item's subtitles + fonts right after
    // its sprites (newest-first). Running both walkers concurrently starved
    // trickplay COMPLETELY: the warm-all's cold whole-file demuxes (minutes
    // each) monopolized the shared bg_io gate AND all 4 libav pool workers
    // (pool `permits.acquire()` has no timeout), so even the gate-bypassing
    // priority path queued forever — live coverage after days: 119/12100.
    // Single-item playback-priority warms (PlaybackInfo) are untouched.
    // Phase B1 — evict stale durable transcode-session rows (failover
    // breadcrumbs) so the table doesn't grow unbounded.
    app_state.transcode_sessions.spawn_pruner();

    // Per-replica member-sink table: the group actors deliver into it and the
    // `/socket` layer registers each socket's sink. On a single replica this is
    // direct in-process delivery (`LocalDelivery`); the multi-replica Postgres
    // path swaps in a bus-backed delivery + per-group ownership (Phase B4.3d).
    let member_sinks = pharos_sync::MemberSinks::new();
    let local_registry = || {
        GroupRegistry::spawn(std::sync::Arc::new(pharos_sync::LocalDelivery::new(
            member_sinks.clone(),
        )))
    };
    #[cfg(feature = "postgres")]
    let group_registry = {
        let is_postgres = cfg.database.url.starts_with("postgres://")
            || cfg.database.url.starts_with("postgresql://");
        if is_postgres {
            match pharos_server::sync_distributed::build(
                app_state.stores.clone(),
                &cfg.database.url,
                member_sinks.clone(),
            )
            .await
            {
                Ok(reg) => {
                    tracing::info!("SyncPlay: distributed multi-replica coordinator active");
                    reg
                }
                Err(e) => {
                    tracing::error!(error = %e, "SyncPlay distributed init failed; single-replica fallback");
                    local_registry()
                }
            }
        } else {
            local_registry()
        }
    };
    #[cfg(not(feature = "postgres"))]
    let group_registry = local_registry();
    let group_registry = web::Data::new(group_registry);
    // T83 — GC orphaned SyncPlay snapshots (a crash/kill skips the actor's
    // own remove-on-empty; without this the sync_groups table grew forever
    // and B24 recovery could re-attach a device to week-old leftovers).
    pharos_server::sync_recovery::spawn_snapshot_janitor(app_state.stores.clone());
    // B29 — hydrate fresh persisted groups at boot so the ghost prune can
    // dissolve no-show parties within minutes instead of them haunting the
    // join picker until the janitor's 48h cutoff.
    pharos_server::sync_recovery::spawn_boot_reconciliation(
        app_state.stores.clone(),
        group_registry.get_ref().clone(),
    );
    let member_sinks_data = web::Data::new(member_sinks);
    // Bridges the HTTP `/SyncPlay/*` command surface (keyed by deviceId) to the
    // per-`/socket` group member sinks. One instance shared across workers.
    let session_hub = web::Data::new(pharos_sync::SessionHub::new());
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

    // B26 — observe SIGTERM/SIGINT BEFORE actix runs its own graceful
    // shutdown, so socket-teardown paths can tell a draining process from a
    // departing client and leave SyncPlay memberships (+ persisted group
    // snapshots) intact for the next replica to recover.
    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM listener failed to install");
                return;
            }
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "SIGINT listener failed to install");
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        tracing::info!("shutdown signal observed — draining (SyncPlay memberships preserved)");
        pharos_server::state::begin_shutdown();
    });

    let handle_for_app = readiness.clone();
    let ui_dir = cfg.server.ui_dir.clone();
    let server = HttpServer::new(move || {
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
            .app_data(session_hub.clone())
            .app_data(member_sinks_data.clone())
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
            .wrap(TracingLogger::<pharos_server::obs::StatusRootSpanBuilder>::new())
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
    // Phase B3 — we own the SIGTERM/SIGINT → graceful-drain sequencing
    // below, so actix must not install its own signal handlers (which would
    // stop the server immediately, before the LB drains).
    .disable_signals()
    // In-flight requests (including a live segment GET) get this long to
    // finish after the graceful stop begins before being force-dropped.
    .shutdown_timeout(30)
    .run();

    // Phase B3 — graceful drain. On SIGTERM (rolling deploy) or SIGINT:
    // flip /readyz unready, wait `drain_grace_secs` for the load balancer to
    // observe it and stop routing new requests, then stop the server
    // gracefully (in-flight requests get `shutdown_timeout`). This keeps a
    // deploy from cutting a viewer mid-segment.
    let drain_handle = server.handle();
    let drain_readiness = readiness.clone();
    let drain_grace = cfg.server.drain_grace_secs;
    tokio::spawn(async move {
        await_shutdown_signal().await;
        tracing::info!(
            grace_secs = drain_grace,
            "shutdown signal received; draining"
        );
        let _ = drain_readiness.drain().await;
        tokio::time::sleep(std::time::Duration::from_secs(drain_grace)).await;
        drain_handle.stop(true).await;
    });

    server.await?;
    // P31 — shutdown reached. If the SSDP responder is alive, send
    // byebye frames so DLNA clients drop pharos immediately instead
    // of waiting out the CACHE-CONTROL TTL.
    if let Some(g) = _ssdp_guard.as_ref() {
        g.send_byebye().await;
    }
    Ok(())
}
