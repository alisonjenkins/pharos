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
use pharos_cache::SubtitleCache;
use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_jellyfin_api::dto::{build_layout, series_id_for_key};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const WARMUP: Duration = Duration::from_secs(45);
const PASS_INTERVAL: Duration = Duration::from_secs(600);
const COOLDOWN: Duration = Duration::from_secs(3);
/// A whole-file decode contends with live segment transcoding, so we never
/// start one while a client is streaming. "Streaming" = a segment was pulled
/// within this window; playback must be quiet this long before background
/// work resumes. Larger than a client's read-ahead gap so a buffered player
/// doesn't repeatedly wave the backfill through between segment bursts.
const PLAYBACK_QUIET: Duration = Duration::from_secs(30);
/// How often to re-check the playback gate while parked.
const GATE_POLL: Duration = Duration::from_secs(3);

/// Unix seconds, mirroring `AppState`'s clock, for the playback-quiet gate.
fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Block until live playback has been quiet for `PLAYBACK_QUIET`. This is the
/// yield-to-playback gate: the backfill parks here rather than launching a
/// CPU/IO-heavy decode that would make an active stream buffer.
async fn await_playback_quiet(playback: &AtomicI64) {
    loop {
        let idle = unix_now_secs().saturating_sub(playback.load(Ordering::Relaxed));
        if idle >= PLAYBACK_QUIET.as_secs() as i64 {
            return;
        }
        tokio::time::sleep(GATE_POLL).await;
    }
}

/// The playback-yield gate, with a `bypass` escape hatch. Bulk pre-generation
/// parks on [`await_playback_quiet`] so a whole-file decode never makes an
/// active stream buffer. But the *actively-watched* item (the priority-tier
/// seed) is exactly what the viewer is about to scrub — it must be generated
/// immediately, even mid-playback, or its previews never appear during the
/// session that wants them. `bypass = true` skips the gate for that case.
async fn await_gate(bypass: bool, playback: &AtomicI64) {
    if bypass {
        return;
    }
    await_playback_quiet(playback).await;
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
    playback: Arc<AtomicI64>,
    widths: Vec<u32>,
    interval_ms: u32,
) -> PriorityTx {
    let (tx, rx) = mpsc::unbounded_channel();
    // Run whenever there's *some* asset to pre-build: trickplay widths or a
    // subtitle cache to warm.
    if !widths.is_empty() || subtitles.is_some() {
        tokio::spawn(run(
            stores,
            cache,
            subtitles,
            playback,
            widths,
            interval_ms,
            rx,
        ));
    }
    tx
}

async fn run(
    stores: Stores,
    cache: TrickplayCache,
    subtitles: Option<SubtitleCache>,
    playback: Arc<AtomicI64>,
    widths: Vec<u32>,
    interval_ms: u32,
    mut prio_rx: mpsc::UnboundedReceiver<u64>,
) {
    tokio::time::sleep(WARMUP).await;
    let subs = subtitles.as_ref();
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
            generate_priority(id, &items, &cache, subs, &playback, &widths, interval_ms).await;
        }

        // 2. General sweep, newest-first.
        let mut general: Vec<&MediaItem> = items.iter().filter(|i| is_video(i)).collect();
        general.sort_by_key(|i| std::cmp::Reverse(i.created_at.unwrap_or(i64::MIN)));
        for item in general {
            // A show that starts playing mid-sweep preempts the rest.
            drain_into(&mut prio_rx, &mut pending);
            for id in pending.drain(..) {
                generate_priority(id, &items, &cache, subs, &playback, &widths, interval_ms).await;
            }
            generate_item(item, &cache, subs, &playback, &widths, interval_ms, false).await;
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
    subs: Option<&SubtitleCache>,
    playback: &AtomicI64,
    widths: &[u32],
    interval_ms: u32,
) {
    let Some(seed) = items.iter().find(|i| i.id == id) else {
        return;
    };
    // The seed is what the viewer is scrubbing right now: generate it first,
    // bypassing the playback-quiet gate so its previews appear mid-session.
    generate_item(seed, cache, subs, playback, widths, interval_ms, true).await;
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
            generate_item(item, cache, subs, playback, widths, interval_ms, false).await;
        }
    }
}

/// Pre-build one item's derived assets, skipping anything already cached:
/// trickplay sprites for each configured width, then its text subtitles.
async fn generate_item(
    item: &MediaItem,
    cache: &TrickplayCache,
    subs: Option<&SubtitleCache>,
    playback: &AtomicI64,
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
        // Yield to live playback: park here (not mid-decode) until streaming
        // has been quiet, so this whole-file decode never makes a viewer buffer.
        // The actively-watched seed (`bypass_gate`) skips the wait so its
        // previews appear during the session that wants them.
        await_gate(bypass_gate, playback).await;
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
    // Warm subtitle extractions so first-scrub playback doesn't stall on a
    // whole-file demux (esp. multi-GB lossless-audio anime).
    if let Some(sc) = subs {
        if !item.probe.subtitle_tracks.is_empty() {
            await_gate(bypass_gate, playback).await;
            crate::api::jellyfin::subtitles::pre_extract_subtitles(sc, item).await;
            tokio::time::sleep(COOLDOWN).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The priority-tier seed (`bypass = true`) must generate immediately even
    /// while playback is active — otherwise the previews for the very item
    /// being watched never appear during that session.
    #[tokio::test]
    async fn gate_bypass_returns_immediately_during_active_playback() {
        let active = AtomicI64::new(unix_now_secs()); // "playback just happened"
        let r = tokio::time::timeout(Duration::from_millis(100), await_gate(true, &active)).await;
        assert!(r.is_ok(), "bypass must not park on the playback-quiet gate");
    }

    /// Non-bypass (bulk) generation still parks while playback is active, so a
    /// whole-file decode never steals cycles from a live stream.
    #[tokio::test]
    async fn gate_without_bypass_parks_during_active_playback() {
        let active = AtomicI64::new(unix_now_secs());
        let r = tokio::time::timeout(Duration::from_millis(100), await_gate(false, &active)).await;
        assert!(r.is_err(), "bulk generation must yield to active playback");
    }
}
