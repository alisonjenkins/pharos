//! Background trickplay pre-generation.
//!
//! Trickplay sprite sheets are expensive to build — a whole-episode decode
//! that takes tens of seconds and burns multiple CPU cores. Generating them on
//! the client request path (first scrub) blew past the ingress timeout (504)
//! and, before the streaming fix, OOM-killed the pod. So — like Jellyfin's
//! `TrickplayImagesTask` — pharos pre-generates them out of band and the HTTP
//! handler only ever serves what's already on disk (404 otherwise).
//!
//! Two independent tasks share the work (and the per-key dedup in the cache
//! makes their overlap harmless):
//!
//! - **Priority worker** — `PlaybackInfo` + `/Sessions/Playing[/Progress]`
//!   push the playing item's id onto a channel. The worker generates that item
//!   IMMEDIATELY (bypassing the background-I/O gate — its previews must appear
//!   mid-session), then pre-warms the rest of its series in watch order (the
//!   episodes AFTER the seed first — that's what gets watched next). A fresh
//!   nudge preempts the remaining siblings of the previous one, so the video
//!   someone is watching *right now* never waits behind another show's tail.
//! - **General sweep** — newest-first (by `created_at`) pass over the whole
//!   library every `PASS_INTERVAL`, gate-throttled, `SWEEP_CONCURRENCY` items
//!   in flight. Freshly-added media is the most likely to be watched.
//!
//! The old design ran both in ONE loop: a priority nudge was only noticed
//! between sweep chunks, and never while a previous seed's series was being
//! expanded — the currently-watched episode could wait hours behind a 70-ep
//! backfill. Splitting the tasks removes every such wait.

use crate::state::Stores;
use pharos_cache::trickplay_cache::TrickplayCache;
use pharos_cache::{ImageCache, SubtitleCache};
use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_jellyfin_api::dto::{build_layout, series_id_for_key};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

const WARMUP: Duration = Duration::from_secs(45);
const PASS_INTERVAL: Duration = Duration::from_secs(600);
const COOLDOWN: Duration = Duration::from_secs(3);
/// How many general-sweep items to generate CONCURRENTLY. Generation is
/// largely NFS-I/O-bound (keyframe seeks leave the CPU near-idle), so a strictly
/// sequential sweep wastes almost all of a multi-core box while a 13k-item
/// library crawls. Overlapping items is a near-linear win. Real concurrency is
/// still bounded by the shared `bg_io` gate (full when idle, throttled to
/// `BG_IO_BUSY` while streaming), so this only sets how many are kept in flight;
/// it never exceeds the gate's live pressure ceiling. Kept a touch above the
/// idle gate width so a permit freed mid-item is grabbed immediately.
const SWEEP_CONCURRENCY: usize = 10;

/// Everything one generation op needs — cloned into both worker tasks.
#[derive(Clone)]
struct GenCtx {
    stores: Stores,
    cache: TrickplayCache,
    subtitles: Option<SubtitleCache>,
    images: Option<ImageCache>,
    bg_io: Arc<Semaphore>,
    widths: Vec<u32>,
    interval_ms: u32,
}

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

/// Spawn the background pre-generators and return a priority handle. Callers
/// (`PlaybackInfo`, `/Sessions/Playing`, `/Sessions/Playing/Progress`) send an
/// item id to bump that item — then its whole series — to the front. No task
/// is spawned when trickplay is disabled (empty widths); the returned sender's
/// messages are then simply dropped.
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
        tracing::info!(
            ?widths,
            interval_ms,
            "trickplay backfill: spawning priority worker + sweep"
        );
        let ctx = GenCtx {
            stores,
            cache,
            subtitles,
            images,
            bg_io,
            widths,
            interval_ms,
        };
        tokio::spawn(run_priority(ctx.clone(), rx));
        tokio::spawn(run_sweep(ctx));
    }
    tx
}

/// Priority worker: dedicated task so a nudge is acted on IMMEDIATELY — never
/// behind the sweep's in-flight chunk, never behind a previous seed's series
/// expansion. No warm-up sleep either: someone is watching this item right now.
async fn run_priority(ctx: GenCtx, mut rx: mpsc::UnboundedReceiver<u64>) {
    // Seeds already expanded once this process — progress reports re-nudge the
    // same id every ~10s for the whole session; only the first does work.
    // (Bounded by library size; the sweep re-covers anything evicted later.)
    let mut done: HashSet<u64> = HashSet::new();
    let mut queue: VecDeque<(MediaItem, bool)> = VecDeque::new();
    loop {
        if queue.is_empty() {
            match rx.recv().await {
                Some(id) => enqueue_seed(id, &ctx, &mut done, &mut queue).await,
                None => return, // server shutting down
            }
        }
        // Fresh nudges PREEMPT: their units go to the front, so the remaining
        // siblings of an earlier seed wait behind the newly-watched item.
        while let Ok(id) = rx.try_recv() {
            enqueue_seed(id, &ctx, &mut done, &mut queue).await;
        }
        if let Some((item, bypass)) = queue.pop_front() {
            generate_item(&item, &ctx, bypass).await;
        }
    }
}

/// Expand a nudged seed into work units and PREPEND them to the queue:
/// the seed itself first (gate-bypassing), then its series siblings in watch
/// order. Duplicate nudges (progress reports) are dropped via `done`; items
/// already generated cost one cache check inside `generate_item`.
async fn enqueue_seed(
    id: u64,
    ctx: &GenCtx,
    done: &mut HashSet<u64>,
    queue: &mut VecDeque<(MediaItem, bool)>,
) {
    if !done.insert(id) {
        return;
    }
    tracing::info!(
        media.id = id,
        "trickplay priority: nudge accepted, expanding"
    );
    let items = match ctx.stores.list().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, media.id = id, "trickplay priority: item list failed");
            done.remove(&id); // transient — let a later nudge retry
            return;
        }
    };
    for unit in priority_units(id, &items).into_iter().rev() {
        queue.push_front(unit);
    }
}

/// The ordered work a nudge implies: the seed itself (bypass = true — the
/// viewer is scrubbing it right now), then every other episode of its series
/// gate-throttled, NEXT episodes first (ascending from the seed — that's the
/// watch direction), then the earlier ones closest-first (a rewatch scrubs
/// backwards too). Movies have no siblings; non-video seeds yield nothing.
fn priority_units(id: u64, items: &[MediaItem]) -> Vec<(MediaItem, bool)> {
    let Some(seed) = items.iter().find(|i| i.id == id) else {
        return Vec::new();
    };
    if !is_video(seed) {
        return Vec::new();
    }
    let mut units = vec![(seed.clone(), true)];
    if let Some(s) = seed.series.as_ref() {
        let sid = series_id_for_key(s.series_folder.as_deref(), &s.series_name);
        let pos = |i: &MediaItem| {
            let si = i.series.as_ref();
            (
                si.and_then(|s| s.season_number).unwrap_or(u32::MAX),
                si.and_then(|s| s.episode_number).unwrap_or(u32::MAX),
            )
        };
        let seed_pos = pos(seed);
        let mut siblings: Vec<&MediaItem> = items
            .iter()
            .filter(|i| {
                i.id != seed.id
                    && is_video(i)
                    && i.series.as_ref().is_some_and(|si| {
                        series_id_for_key(si.series_folder.as_deref(), &si.series_name) == sid
                    })
            })
            .collect();
        siblings.sort_by_key(|i| pos(i));
        let split = siblings.partition_point(|i| pos(i) < seed_pos);
        let (before, after) = siblings.split_at(split);
        units.extend(after.iter().map(|i| ((*i).clone(), false)));
        units.extend(before.iter().rev().map(|i| ((*i).clone(), false)));
    }
    units
}

/// General sweep: newest-first pass over the whole library, repeated every
/// `PASS_INTERVAL`. Chunks of `SWEEP_CONCURRENCY` run concurrently; each item
/// still draws its own `bg_io` permit per heavy op, so true parallelism is
/// gate-bounded (full `BG_IO_MAX` when idle, throttled to `BG_IO_BUSY` while
/// streaming) — the chunking only stops the sweep from wasting idle capacity
/// one-item-at-a-time.
async fn run_sweep(ctx: GenCtx) {
    tokio::time::sleep(WARMUP).await;
    loop {
        let items = match ctx.stores.list().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "asset backfill: item list failed");
                tokio::time::sleep(PASS_INTERVAL).await;
                continue;
            }
        };
        let mut general: Vec<&MediaItem> = items.iter().filter(|i| is_video(i)).collect();
        general.sort_by_key(|i| std::cmp::Reverse(i.created_at.unwrap_or(i64::MIN)));
        tracing::info!(videos = general.len(), "trickplay sweep: pass starting");
        let mut done_before = 0usize;
        for item in general.iter() {
            if ctx
                .cache
                .is_generated(item.id, *ctx.widths.first().unwrap_or(&320))
                .await
            {
                done_before += 1;
            }
        }
        tracing::info!(
            already_generated = done_before,
            "trickplay sweep: coverage before pass"
        );
        for chunk in general.chunks(SWEEP_CONCURRENCY) {
            let batch = chunk.iter().map(|item| generate_item(item, &ctx, false));
            futures_util::future::join_all(batch).await;
        }
        tracing::info!("trickplay sweep: pass complete");
        tokio::time::sleep(PASS_INTERVAL).await;
    }
}

fn is_video(item: &MediaItem) -> bool {
    matches!(item.kind, MediaKind::Movie | MediaKind::Episode)
}

/// Pre-build one item's derived assets, skipping anything already cached:
/// trickplay sprites for each configured width, then its text subtitles.
async fn generate_item(item: &MediaItem, ctx: &GenCtx, bypass_gate: bool) {
    if !is_video(item) {
        return;
    }
    for &width in &ctx.widths {
        if ctx.cache.is_generated(item.id, width).await {
            continue;
        }
        let Some(layout) = build_layout(&item.probe, width, ctx.interval_ms) else {
            continue;
        };
        // Throttle to the adaptive gate: hold a background-I/O permit across the
        // decode so bulk generation runs at bounded concurrency (yielding NFS +
        // CPU headroom to live streams) yet keeps making progress even while
        // someone is watching. The actively-watched seed bypasses it entirely.
        // Release the permit before the cooldown so the next item can start.
        let generated = {
            let _permit = acquire_gate(bypass_gate, &ctx.bg_io).await;
            ctx.cache
                .ensure_generated(item.id, layout, &item.path)
                .await
        };
        match generated {
            Ok(true) => {
                tracing::info!(media.id = item.id, width, "trickplay pre-generated");
                // Cool down only after real work so re-scans stay fast — but
                // never on the actively-watched seed: its remaining widths (and
                // the next episode behind it) should follow immediately.
                if !bypass_gate {
                    tokio::time::sleep(COOLDOWN).await;
                }
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, media.id = item.id, width, "trickplay generation failed");
            }
        }
    }
    // Warm subtitle extractions so first-scrub playback doesn't stall on a
    // whole-file demux (esp. multi-GB lossless-audio anime).
    if let Some(sc) = ctx.subtitles.as_ref() {
        if !item.probe.subtitle_tracks.is_empty() {
            {
                let _permit = acquire_gate(bypass_gate, &ctx.bg_io).await;
                crate::api::jellyfin::subtitles::pre_extract_subtitles(sc, item).await;
            }
            if !bypass_gate {
                tokio::time::sleep(COOLDOWN).await;
            }
        }
    }
    // Warm embedded fonts (attachments) in one source open so an ASS
    // subtitle's SubtitlesOctopus render doesn't stall on "Fetching assets"
    // fetching each font cold. Only matters for titles that carry fonts.
    if let Some(ic) = ctx.images.as_ref() {
        if !item.probe.attachments.is_empty() {
            let _permit = acquire_gate(bypass_gate, &ctx.bg_io).await;
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
            if !bypass_gate {
                tokio::time::sleep(COOLDOWN).await;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_core::SeriesInfo;

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

    fn ep(id: u64, series: &str, season: u32, episode: u32) -> MediaItem {
        MediaItem {
            id,
            kind: MediaKind::Episode,
            path: format!("/tv/{series}/s{season:02}e{episode:02}.mkv").into(),
            title: format!("{series} s{season:02}e{episode:02}"),
            series: Some(SeriesInfo {
                series_name: series.into(),
                season_number: Some(season),
                episode_number: Some(episode),
                series_folder: Some(format!("/tv/{series}")),
                series_year: None,
            }),
            ..Default::default()
        }
    }

    fn movie(id: u64) -> MediaItem {
        MediaItem {
            id,
            kind: MediaKind::Movie,
            title: format!("movie {id}"),
            ..Default::default()
        }
    }

    /// The nudged episode generates FIRST and gate-bypassing; its series
    /// follows in watch order — episodes after the seed ascending (that's
    /// what plays next), then the earlier ones closest-first. Other series
    /// and movies never ride along.
    #[test]
    fn priority_units_orders_series_by_watch_proximity() {
        let items = vec![
            ep(1, "arrow", 1, 1),
            ep(2, "arrow", 1, 2),
            ep(3, "arrow", 1, 3),
            ep(4, "arrow", 2, 1),
            ep(9, "flash", 1, 1), // different series — excluded
            movie(50),            // not a sibling of anything
        ];
        let units = priority_units(3, &items); // watching arrow s01e03
        let order: Vec<(u64, bool)> = units.iter().map(|(i, b)| (i.id, *b)).collect();
        assert_eq!(
            order,
            vec![
                (3, true),  // the seed itself, gate-bypassing
                (4, false), // next up: s02e01
                (2, false), // then backwards, closest first
                (1, false),
            ]
        );
    }

    /// A movie has no siblings: just the seed, bypassing.
    #[test]
    fn priority_units_movie_is_seed_only() {
        let items = vec![movie(50), ep(1, "arrow", 1, 1)];
        let units = priority_units(50, &items);
        let order: Vec<(u64, bool)> = units.iter().map(|(i, b)| (i.id, *b)).collect();
        assert_eq!(order, vec![(50, true)]);
    }

    /// Unknown ids and non-video seeds yield no work.
    #[test]
    fn priority_units_ignores_unknown_and_non_video() {
        let items = vec![ep(1, "arrow", 1, 1)];
        assert!(priority_units(999, &items).is_empty());
        let audio = MediaItem {
            id: 7,
            kind: MediaKind::Audio,
            ..Default::default()
        };
        assert!(priority_units(7, &[audio]).is_empty());
    }

    /// PREEMPTION: a fresh nudge's units are PREPENDED, so the item someone
    /// just started watching runs before the remaining siblings of the
    /// previous seed — the exact wait the single-loop design suffered from.
    #[tokio::test]
    async fn fresh_nudge_preempts_previous_series_tail() {
        let stores = Stores::connect("sqlite::memory:").await.expect("stores");
        for item in [
            ep(1, "arrow", 1, 1),
            ep(2, "arrow", 1, 2),
            ep(3, "flash", 1, 1),
        ] {
            stores.put(item).await.expect("put");
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = GenCtx {
            stores,
            cache: TrickplayCache::new(dir.path(), 1),
            subtitles: None,
            images: None,
            bg_io: Arc::new(Semaphore::new(1)),
            widths: vec![320],
            interval_ms: 10_000,
        };
        let mut done = HashSet::new();
        let mut queue = VecDeque::new();
        enqueue_seed(1, &ctx, &mut done, &mut queue).await;
        // arrow s01e01 seed + its sibling queued…
        assert_eq!(
            queue.iter().map(|(i, _)| i.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        // …then someone starts flash s01e01: it must jump the arrow tail.
        enqueue_seed(3, &ctx, &mut done, &mut queue).await;
        let order: Vec<(u64, bool)> = queue.iter().map(|(i, b)| (i.id, *b)).collect();
        assert_eq!(order, vec![(3, true), (1, true), (2, false)]);
        // A duplicate nudge (progress report every ~10s) is a no-op.
        enqueue_seed(3, &ctx, &mut done, &mut queue).await;
        assert_eq!(queue.len(), 3, "duplicate nudge must not re-enqueue");
    }
}
