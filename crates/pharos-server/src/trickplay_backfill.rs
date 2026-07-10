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
use pharos_cache::{ImageCache, SubtitleCache};
use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_jellyfin_api::dto::{build_layout, series_id_for_key};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

const WARMUP: Duration = Duration::from_secs(45);
const PASS_INTERVAL: Duration = Duration::from_secs(600);
const COOLDOWN: Duration = Duration::from_secs(3);

/// Acquire a background-I/O slot for one bulk pre-generation op. The
/// actively-watched seed (`bypass = true`) skips throttling entirely — its
/// previews must appear immediately, mid-session. All other work draws a permit
/// from the shared adaptive gate ([`AppState::bg_io`]), which the server shrinks
/// while a client is streaming: so bulk generation PROGRESSES continuously but
/// throttled (bounded concurrency), instead of the old all-or-nothing "pause
/// entirely while anyone is watching". Hold the returned guard across the heavy
/// op and drop it before cooling down, so the slot frees for the next item.
///
/// [`AppState::bg_io`]: crate::state::AppState
async fn acquire_gate(bypass: bool, bg_io: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    if bypass {
        return None;
    }
    bg_io.clone().acquire_owned().await.ok()
}

/// Handle for nudging the pre-generator to prioritise an item (and its series).
pub type PriorityTx = mpsc::UnboundedSender<u64>;

/// Spawn the background pre-generator and return a priority handle. Callers
/// (e.g. `PlaybackInfo`) send an item id to bump that item's whole series to
/// the front. No task is spawned when trickplay is disabled (empty widths); the
/// returned sender's messages are then simply dropped.
pub fn spawn(
    stores: Stores,
    cache: TrickplayCache,
    subtitles: Option<SubtitleCache>,
    images: Option<ImageCache>,
    bg_io: Arc<Semaphore>,
    widths: Vec<u32>,
    interval_ms: u32,
) -> PriorityTx {
    let (tx, rx) = mpsc::unbounded_channel();
    // Run whenever there's *some* asset to pre-build: trickplay widths, a
    // subtitle cache to warm, or embedded fonts to pre-extract.
    if !widths.is_empty() || subtitles.is_some() || images.is_some() {
        tokio::spawn(run(
            stores,
            cache,
            subtitles,
            images,
            bg_io,
            widths,
            interval_ms,
            rx,
        ));
    }
    tx
}

#[allow(clippy::too_many_arguments)]
async fn run(
    stores: Stores,
    cache: TrickplayCache,
    subtitles: Option<SubtitleCache>,
    images: Option<ImageCache>,
    bg_io: Arc<Semaphore>,
    widths: Vec<u32>,
    interval_ms: u32,
    mut prio_rx: mpsc::UnboundedReceiver<u64>,
) {
    tokio::time::sleep(WARMUP).await;
    let subs = subtitles.as_ref();
    let imgs = images.as_ref();
    let mut pending: Vec<u64> = Vec::new();
    loop {
        drain_into(&mut prio_rx, &mut pending);
        let items = match stores.list().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "asset backfill: item list failed");
                tokio::time::sleep(PASS_INTERVAL).await;
                continue;
            }
        };

        // 1. Priority: actively-watched items, expanded to their whole series.
        for id in pending.drain(..) {
            generate_priority(id, &items, &cache, subs, imgs, &bg_io, &widths, interval_ms).await;
        }

        // 2. General sweep, newest-first.
        let mut general: Vec<&MediaItem> = items.iter().filter(|i| is_video(i)).collect();
        general.sort_by_key(|i| std::cmp::Reverse(i.created_at.unwrap_or(i64::MIN)));
        for item in general {
            // A show that starts playing mid-sweep preempts the rest.
            drain_into(&mut prio_rx, &mut pending);
            for id in pending.drain(..) {
                generate_priority(id, &items, &cache, subs, imgs, &bg_io, &widths, interval_ms)
                    .await;
            }
            generate_item(
                item,
                &cache,
                subs,
                imgs,
                &bg_io,
                &widths,
                interval_ms,
                false,
            )
            .await;
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
#[allow(clippy::too_many_arguments)]
async fn generate_priority(
    id: u64,
    items: &[MediaItem],
    cache: &TrickplayCache,
    subs: Option<&SubtitleCache>,
    imgs: Option<&ImageCache>,
    bg_io: &Arc<Semaphore>,
    widths: &[u32],
    interval_ms: u32,
) {
    let Some(seed) = items.iter().find(|i| i.id == id) else {
        return;
    };
    // The seed is what the viewer is scrubbing right now: generate it first,
    // bypassing the playback-quiet gate so its previews appear mid-session.
    generate_item(seed, cache, subs, imgs, bg_io, widths, interval_ms, true).await;
    // Series siblings are "next up" — worth pre-warming, but they stay gated
    // so their bulk decode still yields to the active stream.
    if let Some(s) = seed.series.as_ref() {
        let sid = series_id_for_key(s.series_folder.as_deref(), &s.series_name);
        for item in items.iter().filter(|i| {
            i.id != seed.id
                && is_video(i)
                && i.series.as_ref().is_some_and(|si| {
                    series_id_for_key(si.series_folder.as_deref(), &si.series_name) == sid
                })
        }) {
            generate_item(item, cache, subs, imgs, bg_io, widths, interval_ms, false).await;
        }
    }
}

/// Pre-build one item's derived assets, skipping anything already cached:
/// trickplay sprites for each configured width, then its text subtitles.
#[allow(clippy::too_many_arguments)]
async fn generate_item(
    item: &MediaItem,
    cache: &TrickplayCache,
    subs: Option<&SubtitleCache>,
    imgs: Option<&ImageCache>,
    bg_io: &Arc<Semaphore>,
    widths: &[u32],
    interval_ms: u32,
    bypass_gate: bool,
) {
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
        // Throttle to the adaptive gate: hold a background-I/O permit across the
        // decode so bulk generation runs at bounded concurrency (yielding NFS +
        // CPU headroom to live streams) yet keeps making progress even while
        // someone is watching. The actively-watched seed bypasses it entirely.
        // Release the permit before the cooldown so the next item can start.
        let generated = {
            let _permit = acquire_gate(bypass_gate, bg_io).await;
            cache.ensure_generated(item.id, layout, &item.path).await
        };
        match generated {
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
    // Warm subtitle extractions so first-scrub playback doesn't stall on a
    // whole-file demux (esp. multi-GB lossless-audio anime).
    if let Some(sc) = subs {
        if !item.probe.subtitle_tracks.is_empty() {
            {
                let _permit = acquire_gate(bypass_gate, bg_io).await;
                crate::api::jellyfin::subtitles::pre_extract_subtitles(sc, item).await;
            }
            tokio::time::sleep(COOLDOWN).await;
        }
    }
    // Warm embedded fonts (attachments) in one source open so an ASS
    // subtitle's SubtitlesOctopus render doesn't stall on "Fetching assets"
    // fetching each font cold. Only matters for titles that carry fonts.
    if let Some(ic) = imgs {
        if !item.probe.attachments.is_empty() {
            let _permit = acquire_gate(bypass_gate, bg_io).await;
            let indices: Vec<u32> = item
                .probe
                .attachments
                .iter()
                .map(|a| a.stream_index)
                .collect();
            if let Err(e) = ic
                .ensure_all_attachments(item.id, &item.path, &indices)
                .await
            {
                tracing::warn!(error = %e, media.id = item.id, "font pre-extract failed");
            }
            tokio::time::sleep(COOLDOWN).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The priority-tier seed (`bypass = true`) generates immediately — it takes
    /// NO permit, so it never blocks even when the gate is fully parked (the
    /// state during live playback). Otherwise the previews for the very item
    /// being watched wouldn't appear during that session.
    #[tokio::test]
    async fn gate_bypass_takes_no_permit_even_when_fully_parked() {
        let gate = Arc::new(Semaphore::new(0)); // no permits available at all
        let r = tokio::time::timeout(Duration::from_millis(100), acquire_gate(true, &gate)).await;
        assert!(
            matches!(r, Ok(None)),
            "bypass acquires no permit + never blocks"
        );
    }

    /// Bulk generation is THROTTLED (not paused): it draws from the shared gate,
    /// so its concurrency is bounded — but as soon as a permit frees it proceeds
    /// (continuous progress even while a client streams).
    #[tokio::test]
    async fn gate_non_bypass_is_bounded_by_permits() {
        let gate = Arc::new(Semaphore::new(1));
        let p1 = acquire_gate(false, &gate).await;
        assert!(p1.is_some(), "first op gets the only permit");
        // A second concurrent op waits for the permit — bounded concurrency.
        let blocked =
            tokio::time::timeout(Duration::from_millis(100), acquire_gate(false, &gate)).await;
        assert!(
            blocked.is_err(),
            "second op is throttled while the first holds it"
        );
        // Freeing the permit lets it proceed — it never permanently stalls.
        drop(p1);
        let freed =
            tokio::time::timeout(Duration::from_millis(100), acquire_gate(false, &gate)).await;
        assert!(matches!(freed, Ok(Some(_))), "permit freed → op proceeds");
    }
}
