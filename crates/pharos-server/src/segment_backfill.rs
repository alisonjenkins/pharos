//! Intro/outro detection backfill (ADR-0018 Phase 5, T86).
//!
//! Groups episodes by season, fingerprints each episode's head (intro) and
//! tail (credits) windows on the shared libav worker pool — gated by the
//! adaptive `bg_io` semaphore so it never starves live playback (the B49/B52
//! lesson) — runs the season-consensus detector, and persists the resulting
//! Intro/Outro `MediaSegment`s. Fingerprints are cached per episode so a
//! newly-added episode re-runs detection without re-fingerprinting the season
//! (ADR-0018 #2).
//!
//! Compiled on unix only (the libav worker pool). A season is (re)analyzed
//! when any of its episodes lacks a current-`SEGMENT_SCHEMA_VERSION` segment.

use crate::state::Stores;
use pharos_core::{
    DetectedSegment, FingerprintKind, MediaItem, MediaSegmentKind, MediaSegmentStore, MediaStore,
    SEGMENT_SCHEMA_VERSION,
};
use pharos_transcode::fingerprint::align::AlignConfig;
use pharos_transcode::fingerprint::season::{detect_season, EpisodeFingerprint, SeasonConfig};
use pharos_transcode::worker::LibavWorkerPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Delay before the first pass so boot I/O settles.
const WARMUP: Duration = Duration::from_secs(90);
/// Re-scan interval; a pass no-ops fast when every season is analyzed.
const PASS_INTERVAL: Duration = Duration::from_secs(1800);
/// Fraction of the episode head fingerprinted for the intro.
const INTRO_ANALYSIS_FRACTION: f64 = 0.25;
/// Cap on the intro head window (ms) — the plugin's `AnalysisLengthLimit`.
const INTRO_MAX_MS: u64 = 10 * 60 * 1000;
/// Tail window (ms) fingerprinted for credits — the plugin's TV
/// `MaximumCreditsDuration` (450 s).
const CREDITS_WINDOW_MS: u64 = 450 * 1000;
/// A season needs at least this many episodes for cross-episode detection.
const MIN_SEASON_EPISODES: usize = 2;

#[derive(Clone)]
struct Ctx {
    stores: Stores,
    bg_io: Arc<Semaphore>,
    pool: LibavWorkerPool,
}

/// Spawn the segment-detection sweep. No-op handle when the pool is absent.
pub fn spawn(stores: Stores, bg_io: Arc<Semaphore>, pool: LibavWorkerPool) {
    tracing::info!("segment backfill: spawning intro/outro detection sweep");
    let ctx = Ctx {
        stores,
        bg_io,
        pool,
    };
    tokio::spawn(run_sweep(ctx));
}

async fn acquire_gate(bg_io: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    bg_io.clone().acquire_owned().await.ok()
}

async fn run_sweep(ctx: Ctx) {
    tokio::time::sleep(WARMUP).await;
    loop {
        match ctx.stores.list().await {
            Ok(items) => analyze_all_seasons(&ctx, &items).await,
            Err(e) => tracing::warn!(error = %e, "segment backfill: item list failed"),
        }
        tokio::time::sleep(PASS_INTERVAL).await;
    }
}

/// Group episodes by (series identity, season) and analyze each season that
/// isn't already covered at the current schema version.
async fn analyze_all_seasons(ctx: &Ctx, items: &[MediaItem]) {
    let mut seasons: HashMap<String, Vec<&MediaItem>> = HashMap::new();
    for it in items {
        if it.kind != pharos_core::MediaKind::Episode {
            continue;
        }
        let Some(s) = it.series.as_ref() else {
            continue;
        };
        let Some(season) = s.season_number else {
            continue;
        };
        // Folder-keyed identity (falls back to name) so two same-named shows
        // don't merge — matches the wire-id scheme.
        let key = format!(
            "{}::{}",
            s.series_folder.as_deref().unwrap_or(&s.series_name),
            season
        );
        seasons.entry(key).or_default().push(it);
    }

    let mut analyzed = 0usize;
    for (key, eps) in seasons {
        if eps.len() < MIN_SEASON_EPISODES {
            continue;
        }
        if season_is_current(ctx, &eps).await {
            continue;
        }
        if analyze_season(ctx, &eps).await {
            analyzed += 1;
            tracing::info!(season = %key, episodes = eps.len(), "segment backfill: season analyzed");
        }
    }
    if analyzed > 0 {
        tracing::info!(seasons = analyzed, "segment backfill: pass complete");
    }
}

/// A season is "current" when EVERY episode already has at least one detected
/// segment stamped with the current schema version (cheap DB reads).
async fn season_is_current(ctx: &Ctx, eps: &[&MediaItem]) -> bool {
    for ep in eps {
        match ctx.stores.media_segments_for(ep.id).await {
            Ok(segs) if !segs.is_empty() => {}
            _ => return false,
        }
    }
    true
}

/// Fingerprint the intro + credits windows of every episode (cached), run the
/// consensus detector for each, and persist the segments. Returns `true` when
/// it did work.
async fn analyze_season(ctx: &Ctx, eps: &[&MediaItem]) -> bool {
    let mut intro_fps: Vec<EpisodeFingerprint> = Vec::new();
    let mut credit_fps: Vec<EpisodeFingerprint> = Vec::new();

    for ep in eps {
        let Some(dur_ms) = ep.probe.duration_ms else {
            continue;
        };
        // Intro head window.
        let intro_len = ((dur_ms as f64 * INTRO_ANALYSIS_FRACTION) as u64).min(INTRO_MAX_MS);
        if intro_len >= 15_000 {
            if let Some(points) =
                fingerprint_cached(ctx, ep, FingerprintKind::Intro, 0, intro_len).await
            {
                intro_fps.push(EpisodeFingerprint {
                    id: ep.id,
                    points,
                    window_offset_secs: 0.0,
                });
            }
        }
        // Credits tail window.
        let credits_start = dur_ms.saturating_sub(CREDITS_WINDOW_MS);
        let credits_len = dur_ms - credits_start;
        if credits_len >= 15_000 {
            if let Some(points) = fingerprint_cached(
                ctx,
                ep,
                FingerprintKind::Credits,
                credits_start,
                credits_len,
            )
            .await
            {
                credit_fps.push(EpisodeFingerprint {
                    id: ep.id,
                    points,
                    window_offset_secs: credits_start as f64 / 1000.0,
                });
            }
        }
    }

    let cfg = SeasonConfig {
        align: AlignConfig::default(),
        ..SeasonConfig::default()
    };
    let intro_segs = detect_season(&intro_fps, &cfg);
    let outro_segs = detect_season(&credit_fps, &cfg);

    // Persist per episode: an episode may get an Intro, an Outro, both, or
    // neither. Replace the item's segment set atomically.
    let mut by_item: HashMap<u64, Vec<DetectedSegment>> = HashMap::new();
    for s in &intro_segs {
        by_item.entry(s.id).or_default().push(DetectedSegment {
            kind: MediaSegmentKind::Intro,
            start_ms: (s.start_secs * 1000.0).max(0.0) as u64,
            end_ms: (s.end_secs * 1000.0).max(0.0) as u64,
            detector: "chromaprint".into(),
            confidence: s.confidence as f32,
        });
    }
    for s in &outro_segs {
        by_item.entry(s.id).or_default().push(DetectedSegment {
            kind: MediaSegmentKind::Outro,
            start_ms: (s.start_secs * 1000.0).max(0.0) as u64,
            end_ms: (s.end_secs * 1000.0).max(0.0) as u64,
            detector: "chromaprint".into(),
            confidence: s.confidence as f32,
        });
    }

    let mut wrote = false;
    for ep in eps {
        // Even an empty set is written (stamped current) so a season with no
        // detectable intro isn't re-analyzed every pass.
        let segs = by_item.remove(&ep.id).unwrap_or_default();
        if let Err(e) = ctx
            .stores
            .set_media_segments(ep.id, &segs, SEGMENT_SCHEMA_VERSION)
            .await
        {
            tracing::warn!(error = %e, media.id = ep.id, "segment backfill: persist failed");
        } else {
            wrote = true;
        }
    }
    wrote
}

/// A cached fingerprint at the current schema, else compute it on the pool
/// (bg-IO gated) and cache it. `None` on a fingerprint failure.
async fn fingerprint_cached(
    ctx: &Ctx,
    ep: &MediaItem,
    kind: FingerprintKind,
    start_ms: u64,
    dur_ms: u64,
) -> Option<Vec<u32>> {
    if let Ok(Some(points)) = ctx
        .stores
        .episode_fingerprint_for(ep.id, kind, SEGMENT_SCHEMA_VERSION)
        .await
    {
        return Some(points);
    }
    // Whole-window decode → gate against live playback.
    let _permit = acquire_gate(&ctx.bg_io).await;
    let points = match ctx
        .pool
        .fingerprint(ep.path.clone(), start_ms, dur_ms)
        .await
    {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return None,
        Err(e) => {
            tracing::debug!(error = %e, media.id = ep.id, ?kind, "segment backfill: fingerprint failed");
            return None;
        }
    };
    let _ = ctx
        .stores
        .set_episode_fingerprint(ep.id, kind, &points, SEGMENT_SCHEMA_VERSION)
        .await;
    Some(points)
}
