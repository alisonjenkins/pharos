//! Background trickplay pre-generation.
//!
//! Trickplay sprite sheets are expensive to build — a whole-episode decode
//! that takes tens of seconds and burns multiple CPU cores. Generating them on
//! the client request path (first scrub) blew past the ingress timeout (504)
//! and, before the streaming fix, OOM-killed the pod. So — like Jellyfin's
//! `TrickplayImagesTask` — pharos pre-generates them out of band and the HTTP
//! handler only ever serves what's already on disk (404 otherwise).
//!
//! This task runs one generation at a time (the cache dedups per key anyway),
//! spacing generations out so the backfill stays low-priority relative to live
//! playback transcodes, and re-scans periodically to pick up newly-added media.

use crate::state::Stores;
use pharos_cache::trickplay_cache::TrickplayCache;
use pharos_core::{MediaKind, MediaStore};
use pharos_jellyfin_api::dto::build_layout;
use std::time::Duration;

/// Delay before the first pass, giving the boot scan time to populate the
/// library so the first sweep isn't empty.
const WARMUP: Duration = Duration::from_secs(45);
/// Gap between full re-scan passes (picks up newly-scanned items).
const PASS_INTERVAL: Duration = Duration::from_secs(600);
/// Cooldown after each *actual* generation so the backfill yields CPU to live
/// playback rather than pinning cores back-to-back.
const COOLDOWN: Duration = Duration::from_secs(3);

/// Spawn the background pre-generator. No-op when trickplay is disabled
/// (empty widths) — nothing to generate.
pub fn spawn(stores: Stores, cache: TrickplayCache, widths: Vec<u32>, interval_ms: u32) {
    if widths.is_empty() {
        return;
    }
    tokio::spawn(run(stores, cache, widths, interval_ms));
}

async fn run(stores: Stores, cache: TrickplayCache, widths: Vec<u32>, interval_ms: u32) {
    tokio::time::sleep(WARMUP).await;
    loop {
        let generated = pass(&stores, &cache, &widths, interval_ms).await;
        if generated > 0 {
            tracing::info!(generated, "trickplay backfill: pass complete");
        }
        tokio::time::sleep(PASS_INTERVAL).await;
    }
}

/// One sweep over every video item × configured width, generating any sprite
/// set that isn't cached yet. Returns how many were generated this pass.
async fn pass(stores: &Stores, cache: &TrickplayCache, widths: &[u32], interval_ms: u32) -> u64 {
    let items = match stores.list().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "trickplay backfill: item list failed");
            return 0;
        }
    };
    let mut generated: u64 = 0;
    for item in items {
        if !matches!(item.kind, MediaKind::Movie | MediaKind::Episode) {
            continue;
        }
        for &width in widths {
            // Cheap disk probe — skip finished items without touching ffmpeg.
            if cache.is_generated(item.id, width).await {
                continue;
            }
            let Some(layout) = build_layout(&item.probe, width, interval_ms) else {
                continue; // missing duration/dimensions — nothing to lay out
            };
            match cache.ensure_generated(item.id, layout, &item.path).await {
                Ok(true) => {
                    generated += 1;
                    tracing::debug!(media.id = item.id, width, "trickplay pre-generated");
                    // Only cool down after real work, so re-scan passes over an
                    // already-generated library stay fast.
                    tokio::time::sleep(COOLDOWN).await;
                }
                Ok(false) => {} // generated concurrently — fine
                Err(e) => {
                    tracing::warn!(error = %e, media.id = item.id, width, "trickplay generation failed");
                }
            }
        }
    }
    generated
}
