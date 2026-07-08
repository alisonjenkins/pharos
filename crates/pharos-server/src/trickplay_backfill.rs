//! Background trickplay pre-generation.
//!
//! Trickplay sprite sheets are expensive to build — a whole-episode decode
//! that takes tens of seconds and burns multiple CPU cores. Generating them on
//! the client request path (first scrub) blew past the ingress timeout (504)
//! and, before the streaming fix, OOM-killed the pod. So — like Jellyfin's
//! `TrickplayImagesTask` — pharos pre-generates them out of band and the HTTP
//! handler only ever serves what's already on disk (404 otherwise).
//!
//! Ordering makes the wait invisible in practice:
//! - **Actively-watched shows jump the queue.** `PlaybackInfo` pushes the item
//!   id onto a priority channel; the backfill expands it to the *whole series*
//!   (you'll scrub the next episodes too) and generates those before anything
//!   else.
//! - **The general sweep is newest-first** (by `created_at`) — freshly-added
//!   media is the most likely to be watched.
//!
//! Generation runs one at a time (the cache dedups per key anyway) with a
//! cooldown after each real build, so the backfill stays low-priority relative
//! to live playback transcodes.

use crate::state::Stores;
use pharos_cache::trickplay_cache::TrickplayCache;
use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_jellyfin_api::dto::{build_layout, series_id_for_key};
use std::time::Duration;
use tokio::sync::mpsc;

const WARMUP: Duration = Duration::from_secs(45);
const PASS_INTERVAL: Duration = Duration::from_secs(600);
const COOLDOWN: Duration = Duration::from_secs(3);

/// Handle for nudging the pre-generator to prioritise an item (and its series).
pub type PriorityTx = mpsc::UnboundedSender<u64>;

/// Spawn the background pre-generator and return a priority handle. Callers
/// (e.g. `PlaybackInfo`) send an item id to bump that item's whole series to
/// the front. No task is spawned when trickplay is disabled (empty widths); the
/// returned sender's messages are then simply dropped.
pub fn spawn(
    stores: Stores,
    cache: TrickplayCache,
    widths: Vec<u32>,
    interval_ms: u32,
) -> PriorityTx {
    let (tx, rx) = mpsc::unbounded_channel();
    if !widths.is_empty() {
        tokio::spawn(run(stores, cache, widths, interval_ms, rx));
    }
    tx
}

async fn run(
    stores: Stores,
    cache: TrickplayCache,
    widths: Vec<u32>,
    interval_ms: u32,
    mut prio_rx: mpsc::UnboundedReceiver<u64>,
) {
    tokio::time::sleep(WARMUP).await;
    let mut pending: Vec<u64> = Vec::new();
    loop {
        drain_into(&mut prio_rx, &mut pending);
        let items = match stores.list().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "trickplay backfill: item list failed");
                tokio::time::sleep(PASS_INTERVAL).await;
                continue;
            }
        };

        // 1. Priority: actively-watched items, expanded to their whole series.
        for id in pending.drain(..) {
            generate_priority(id, &items, &cache, &widths, interval_ms).await;
        }

        // 2. General sweep, newest-first.
        let mut general: Vec<&MediaItem> = items.iter().filter(|i| is_video(i)).collect();
        general.sort_by_key(|i| std::cmp::Reverse(i.created_at.unwrap_or(i64::MIN)));
        for item in general {
            // A show that starts playing mid-sweep preempts the rest.
            drain_into(&mut prio_rx, &mut pending);
            for id in pending.drain(..) {
                generate_priority(id, &items, &cache, &widths, interval_ms).await;
            }
            generate_item(item, &cache, &widths, interval_ms).await;
        }

        // 3. Sweep done — sleep until the next pass, but wake immediately if a
        //    show starts playing so its previews don't wait out the interval.
        tokio::select! {
            maybe = prio_rx.recv() => {
                if let Some(id) = maybe { pending.push(id); }
            }
            _ = tokio::time::sleep(PASS_INTERVAL) => {}
        }
    }
}

fn is_video(item: &MediaItem) -> bool {
    matches!(item.kind, MediaKind::Movie | MediaKind::Episode)
}

/// Non-blocking drain of every queued priority id into `buf`.
fn drain_into(rx: &mut mpsc::UnboundedReceiver<u64>, buf: &mut Vec<u64>) {
    while let Ok(id) = rx.try_recv() {
        buf.push(id);
    }
}

/// Generate trickplay for `id` and, when it's an episode, every other item in
/// the same series — the viewer will scrub the next episodes too.
async fn generate_priority(
    id: u64,
    items: &[MediaItem],
    cache: &TrickplayCache,
    widths: &[u32],
    interval_ms: u32,
) {
    let Some(seed) = items.iter().find(|i| i.id == id) else {
        return;
    };
    match seed.series.as_ref() {
        Some(s) => {
            let sid = series_id_for_key(s.series_folder.as_deref(), &s.series_name);
            for item in items.iter().filter(|i| {
                is_video(i)
                    && i.series.as_ref().is_some_and(|si| {
                        series_id_for_key(si.series_folder.as_deref(), &si.series_name) == sid
                    })
            }) {
                generate_item(item, cache, widths, interval_ms).await;
            }
        }
        None => generate_item(seed, cache, widths, interval_ms).await,
    }
}

/// Generate every configured width for one item, skipping already-cached sets.
async fn generate_item(item: &MediaItem, cache: &TrickplayCache, widths: &[u32], interval_ms: u32) {
    if !is_video(item) {
        return;
    }
    for &width in widths {
        if cache.is_generated(item.id, width).await {
            continue;
        }
        let Some(layout) = build_layout(&item.probe, width, interval_ms) else {
            continue;
        };
        match cache.ensure_generated(item.id, layout, &item.path).await {
            Ok(true) => {
                tracing::debug!(media.id = item.id, width, "trickplay pre-generated");
                // Cool down only after real work so re-scans stay fast.
                tokio::time::sleep(COOLDOWN).await;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, media.id = item.id, width, "trickplay generation failed");
            }
        }
    }
}
