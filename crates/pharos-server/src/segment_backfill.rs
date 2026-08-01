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
//! when any of its episodes has not been analysed at the current
//! `SEGMENT_DETECT_VERSION`.

use crate::state::Stores;
use pharos_core::{
    DetectedSegment, FingerprintKind, MediaItem, MediaSegmentKind, MediaSegmentStore, MediaStore,
    SEGMENT_DETECT_VERSION, SEGMENT_SCHEMA_VERSION,
};
use pharos_transcode::fingerprint::align::AlignConfig;
use pharos_transcode::fingerprint::season::{
    detect_season_verbose, EpisodeFingerprint, EpisodeVerdict, SeasonConfig, Verdict,
};
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

/// Run season consensus for both windows behind the background-I/O gate, on a
/// blocking thread. `None` if the detection task itself failed.
///
/// B132 — consensus is CPU-bound and, since the exhaustive shift search
/// (B131), long: a season pairs every episode against every other over
/// thousands of candidate shifts. Both halves of this matter:
///
///   * `spawn_blocking` keeps it off the async runtime's workers. Called
///     inline it pinned a worker for the whole season, starving the very
///     executor that also serves playback — the server went unusable mid-film
///     with no extra I/O happening, because the cost had moved from disk to
///     CPU and only the disk half was regulated.
///   * holding a `bg_io` permit makes it DUCK. The regulator parks all but
///     `BG_IO_BUSY` permits while a client streams, so detection now stands
///     down during playback exactly as scan probes and trickplay generation
///     do, instead of competing with it.
///
/// Fingerprinting was already gated; this closes the other half.
async fn detect_gated(
    bg_io: &Arc<Semaphore>,
    intro: Vec<EpisodeFingerprint>,
    credits: Vec<EpisodeFingerprint>,
    cfg: SeasonConfig,
) -> Option<(Vec<EpisodeVerdict>, Vec<EpisodeVerdict>)> {
    let _permit = acquire_gate(bg_io).await;
    tokio::task::spawn_blocking(move || {
        (
            detect_season_verbose(&intro, &cfg),
            detect_season_verbose(&credits, &cfg),
        )
    })
    .await
    .inspect_err(|e| tracing::warn!(error = %e, "segment backfill: detection task panicked"))
    .ok()
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

/// Whether intro/outro detection can run against this item at all.
///
/// Detection only ever applies to episodes, and it needs a FILE: `fingerprint`
/// and `fingerprint_multi` open `ep.path` directly, so a remote item's synthetic
/// path (008) is ENOENT. That failure is not self-limiting — the season never
/// reaches the current `SEGMENT_DETECT_VERSION`, so `season_is_current` stays
/// false and the whole season is re-analysed on every pass, each attempt taking
/// a background-I/O permit away from live playback (V134).
fn analysable(it: &MediaItem) -> bool {
    it.kind == pharos_core::MediaKind::Episode && it.origin().local().is_some()
}

/// Group episodes by (series identity, season) and analyze each season that
/// isn't already covered at the current schema version.
async fn analyze_all_seasons(ctx: &Ctx, items: &[MediaItem]) {
    let mut seasons: HashMap<String, Vec<&MediaItem>> = HashMap::new();
    for it in items {
        if !analysable(it) {
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
    let mut snapshotted = false;
    for (key, eps) in seasons {
        if eps.len() < MIN_SEASON_EPISODES {
            continue;
        }
        if season_is_current(ctx, &eps).await {
            continue;
        }
        // T103 — this season is about to be re-analysed, which REPLACES its
        // segments. Capture the current table first, once per detect version,
        // so "did recall drop?" is a diff instead of a recollection. Taken
        // lazily (only when there is genuinely work to do) and idempotent by
        // label, so a later pass cannot overwrite the baseline.
        if !snapshotted {
            snapshotted = true;
            let label = format!("pre_detect_v{SEGMENT_DETECT_VERSION}");
            match ctx.stores.snapshot_media_segments(&label).await {
                Ok(0) => tracing::debug!(%label, "segment backfill: snapshot already present"),
                Ok(rows) => {
                    tracing::info!(%label, rows, "segment backfill: snapshotted segments before re-detection")
                }
                Err(e) => {
                    tracing::warn!(error = %e, %label, "segment backfill: snapshot failed; re-detection proceeds unguarded")
                }
            }
        }
        if analyze_season(ctx, &key, &eps).await {
            analyzed += 1;
            tracing::info!(season = %key, episodes = eps.len(), "segment backfill: season analyzed");
        }
    }
    if analyzed > 0 {
        tracing::info!(seasons = analyzed, "segment backfill: pass complete");
    }
}

/// A season is "current" when EVERY episode has been ANALYSED at the current
/// `SEGMENT_DETECT_VERSION` — whatever that analysis found (cheap DB reads).
///
/// B123: this used to ask whether each episode had any segment ROW, which
/// conflated "not analysed" with "analysed, nothing there". Most shows have no
/// shared intro, and an empty result writes no row, so those seasons failed the
/// check on every pass and were re-analysed forever — while a season that HAD
/// segments could never be re-analysed at all, leaving the version stamp
/// unable to force re-detection after a detector change. The scan stamp
/// separates the question from the answer.
async fn season_is_current(ctx: &Ctx, eps: &[&MediaItem]) -> bool {
    for ep in eps {
        match ctx.stores.segment_scan_version(ep.id).await {
            Ok(Some(v)) if v == SEGMENT_DETECT_VERSION => {}
            _ => return false,
        }
    }
    true
}

/// Record WHY each episode did or did not get a segment.
///
/// Detection failing is not an error — most of the time it means the show has
/// no shared intro — so the only way to tell "correctly found nothing" from
/// "found it and threw it away" is to state the gate's inputs. Without this the
/// recall problem is invisible: a season simply produces no rows, exactly as a
/// season with no intro does. The per-season line carries the drop reasons; the
/// counter makes the ratio queryable across the library.
fn record_verdicts(season: &str, kind: &'static str, verdicts: &[EpisodeVerdict]) {
    if verdicts.is_empty() {
        return;
    }
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for v in verdicts {
        *counts.entry(v.verdict.label()).or_default() += 1;
        metrics::counter!(
            "pharos_segment_detect_total",
            "kind" => kind,
            "outcome" => v.verdict.label(),
        )
        .increment(1);
    }
    // The dropped episodes' own numbers — an episode nine peers agreed with is
    // a very different miss from one nothing matched, and the aggregate cannot
    // tell them apart.
    let dropped: Vec<String> = verdicts
        .iter()
        .filter(|v| v.verdict != Verdict::Emitted)
        .map(|v| {
            format!(
                "{}:{}({} matched/{} agreeing/conf {:.2})",
                v.id,
                v.verdict.label(),
                v.matched,
                v.agreeing,
                v.confidence
            )
        })
        .collect();
    tracing::info!(
        season = %season,
        kind,
        episodes = verdicts.len(),
        emitted = counts.get("emitted").copied().unwrap_or(0),
        low_confidence = counts.get("low_confidence").copied().unwrap_or(0),
        few_agreeing = counts.get("few_agreeing").copied().unwrap_or(0),
        no_span = counts.get("no_span").copied().unwrap_or(0),
        dropped = %dropped.join(" "),
        "segment backfill: season detection verdicts"
    );
}

/// Fingerprint the intro + credits windows of every episode (cached), run the
/// consensus detector for each, and persist the segments. Returns `true` when
/// it did work.
async fn analyze_season(ctx: &Ctx, season_key: &str, eps: &[&MediaItem]) -> bool {
    let mut intro_fps: Vec<EpisodeFingerprint> = Vec::new();
    let mut credit_fps: Vec<EpisodeFingerprint> = Vec::new();

    for ep in eps {
        let Some(dur_ms) = ep.probe.duration_ms else {
            continue;
        };
        // Intro head + credits tail windows (each analysed only when ≥15s).
        let intro_len = ((dur_ms as f64 * INTRO_ANALYSIS_FRACTION) as u64).min(INTRO_MAX_MS);
        let credits_start = dur_ms.saturating_sub(CREDITS_WINDOW_MS);
        let credits_len = dur_ms - credits_start;
        let intro_win = (intro_len >= 15_000).then_some((0u64, intro_len));
        let credits_win = (credits_len >= 15_000).then_some((credits_start, credits_len));

        // B72/T96 — resolve both from ONE container open when both are cold.
        let (intro_pts, credit_pts) = fingerprint_episode(ctx, ep, intro_win, credits_win).await;
        if let Some(points) = intro_pts {
            intro_fps.push(EpisodeFingerprint {
                id: ep.id,
                points,
                window_offset_secs: 0.0,
            });
        }
        if let Some(points) = credit_pts {
            credit_fps.push(EpisodeFingerprint {
                id: ep.id,
                points,
                window_offset_secs: credits_start as f64 / 1000.0,
            });
        }
    }

    let cfg = SeasonConfig {
        align: AlignConfig::default(),
        ..SeasonConfig::default()
    };
    // B132 — consensus is CPU-bound and, since the exhaustive shift search
    // (B131), long: a season pairs every episode against every other over
    // thousands of candidate shifts. Run it on a blocking thread, holding a
    // `bg_io` permit, for two separate reasons:
    //
    //   * `spawn_blocking` keeps it off the async runtime's workers. Called
    //     inline it pinned a worker for the whole season, starving the very
    //     executor that serves playback — the server went unusable mid-film
    //     even though no extra I/O was happening.
    //   * the permit makes it DUCK. The regulator parks all but `BG_IO_BUSY`
    //     permits while a client streams, so detection stands down during
    //     playback exactly as scan probes and trickplay generation do,
    //     instead of competing with it.
    //
    // Fingerprinting was already gated; this closes the other half.
    let Some((intro_verdicts, outro_verdicts)) = detect_gated(
        &ctx.bg_io,
        std::mem::take(&mut intro_fps),
        std::mem::take(&mut credit_fps),
        cfg,
    )
    .await
    else {
        tracing::warn!(season = %season_key, "segment backfill: detection task failed");
        return false;
    };
    record_verdicts(season_key, "Intro", &intro_verdicts);
    record_verdicts(season_key, "Outro", &outro_verdicts);

    // Persist per episode: an episode may get an Intro, an Outro, both, or
    // neither. Replace the item's segment set atomically.
    let mut by_item: HashMap<u64, Vec<DetectedSegment>> = HashMap::new();
    for (kind, verdicts) in [
        (MediaSegmentKind::Intro, &intro_verdicts),
        (MediaSegmentKind::Outro, &outro_verdicts),
    ] {
        for v in verdicts.iter().filter(|v| v.verdict == Verdict::Emitted) {
            let Some(span) = v.span else { continue };
            by_item.entry(v.id).or_default().push(DetectedSegment {
                kind,
                start_ms: (span.start * 1000.0).max(0.0) as u64,
                end_ms: (span.end * 1000.0).max(0.0) as u64,
                detector: "chromaprint".into(),
                confidence: v.confidence as f32,
            });
        }
    }

    let mut wrote = false;
    for ep in eps {
        let segs = by_item.remove(&ep.id).unwrap_or_default();
        if let Err(e) = ctx
            .stores
            .set_media_segments(ep.id, &segs, SEGMENT_DETECT_VERSION)
            .await
        {
            tracing::warn!(error = %e, media.id = ep.id, "segment backfill: persist failed");
            continue;
        }
        // Stamp the ANALYSIS, not its result — an episode with no detectable
        // intro writes no segment row, and without this stamp its season is
        // re-analysed on every pass for the life of the server. Only after the
        // results are safely persisted, so a failed write is retried.
        if let Err(e) = ctx
            .stores
            .set_segment_scan(ep.id, SEGMENT_DETECT_VERSION)
            .await
        {
            tracing::warn!(error = %e, media.id = ep.id, "segment backfill: scan stamp failed");
            continue;
        }
        wrote = true;
    }
    wrote
}

/// Resolve an episode's intro + credits fingerprints, computing any that aren't
/// already cached. When BOTH windows are cold, they're fingerprinted from a
/// SINGLE container open (B72/T96) instead of opening the (NFS) source twice.
/// Each tuple element is `Some` only when its window was requested AND yielded
/// a non-empty fingerprint. `intro`/`credits` are `(start_ms, dur_ms)`.
async fn fingerprint_episode(
    ctx: &Ctx,
    ep: &MediaItem,
    intro: Option<(u64, u64)>,
    credits: Option<(u64, u64)>,
) -> (Option<Vec<u32>>, Option<Vec<u32>>) {
    // Cache hits first — never recompute a window already at the current schema.
    let mut intro_pts = match intro {
        Some(_) => cached_fp(ctx, ep, FingerprintKind::Intro).await,
        None => None,
    };
    let mut credit_pts = match credits {
        Some(_) => cached_fp(ctx, ep, FingerprintKind::Credits).await,
        None => None,
    };
    let need_intro = intro.filter(|_| intro_pts.is_none());
    let need_credits = credits.filter(|_| credit_pts.is_none());

    match (need_intro, need_credits) {
        (Some(iw), Some(cw)) => {
            // Both cold → one open, two windows. Gate against live playback.
            let _permit = acquire_gate(&ctx.bg_io).await;
            match ctx
                .pool
                .fingerprint_multi(ep.path.clone(), vec![iw, cw])
                .await
            {
                Ok(v) if v.len() == 2 => {
                    let mut it = v.into_iter();
                    let i = it.next().unwrap_or_default();
                    let c = it.next().unwrap_or_default();
                    intro_pts = store_fp(ctx, ep, FingerprintKind::Intro, i).await;
                    credit_pts = store_fp(ctx, ep, FingerprintKind::Credits, c).await;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(error = %e, media.id = ep.id, "segment backfill: paired fingerprint failed");
                }
            }
        }
        (Some(w), None) => {
            intro_pts = compute_fp(ctx, ep, FingerprintKind::Intro, w).await;
        }
        (None, Some(w)) => {
            credit_pts = compute_fp(ctx, ep, FingerprintKind::Credits, w).await;
        }
        (None, None) => {}
    }
    (intro_pts, credit_pts)
}

/// A cached fingerprint for `kind` at the current schema, if present.
async fn cached_fp(ctx: &Ctx, ep: &MediaItem, kind: FingerprintKind) -> Option<Vec<u32>> {
    ctx.stores
        .episode_fingerprint_for(ep.id, kind, SEGMENT_SCHEMA_VERSION)
        .await
        .ok()
        .flatten()
}

/// Persist a computed fingerprint (skipping empties, which mean "no usable
/// audio"), returning it for immediate use.
async fn store_fp(
    ctx: &Ctx,
    ep: &MediaItem,
    kind: FingerprintKind,
    points: Vec<u32>,
) -> Option<Vec<u32>> {
    if points.is_empty() {
        return None;
    }
    let _ = ctx
        .stores
        .set_episode_fingerprint(ep.id, kind, &points, SEGMENT_SCHEMA_VERSION)
        .await;
    Some(points)
}

/// Compute + cache a single window (the one-cold-window path). Gated.
async fn compute_fp(
    ctx: &Ctx,
    ep: &MediaItem,
    kind: FingerprintKind,
    (start_ms, dur_ms): (u64, u64),
) -> Option<Vec<u32>> {
    let _permit = acquire_gate(&ctx.bg_io).await;
    match ctx
        .pool
        .fingerprint(ep.path.clone(), start_ms, dur_ms)
        .await
    {
        Ok(p) => store_fp(ctx, ep, kind, p).await,
        Err(e) => {
            tracing::debug!(error = %e, media.id = ep.id, ?kind, "segment backfill: fingerprint failed");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_transcode::fingerprint::align::Span;

    /// Detection declines a remote item (008) and still accepts a local
    /// episode. Both directions, because a predicate returning `false` for
    /// everything would satisfy the first half on its own — and the failure this
    /// guards is unbounded: an ENOENT fingerprint never advances the season's
    /// detect version, so the whole season is re-analysed every pass, each try
    /// taking a background-I/O permit from live playback (V134).
    #[test]
    fn detection_declines_a_remote_episode_and_still_takes_a_local_one() {
        let remote = MediaItem {
            id: 1,
            kind: pharos_core::MediaKind::Episode,
            path: pharos_core::RemoteRef::new("youtube", "dQw4w9WgXcQ")
                .expect("valid ref")
                .to_synthetic_path(),
            ..Default::default()
        };
        assert!(!analysable(&remote));

        let local = MediaItem {
            id: 2,
            kind: pharos_core::MediaKind::Episode,
            path: "/tv/Arrow/s01e01.mkv".into(),
            ..Default::default()
        };
        assert!(analysable(&local));

        // Declined for its ORIGIN, not its kind: a local MOVIE is refused too,
        // so an assertion on kind alone would not have caught a missing origin
        // check.
        let movie = MediaItem {
            id: 3,
            kind: pharos_core::MediaKind::Movie,
            path: "/media/Movies/Arrival.mkv".into(),
            ..Default::default()
        };
        assert!(!analysable(&movie));
    }

    fn verdict(
        id: u64,
        matched: u32,
        agreeing: u32,
        confidence: f64,
        v: Verdict,
    ) -> EpisodeVerdict {
        EpisodeVerdict {
            id,
            matched,
            agreeing,
            confidence,
            span: Some(Span {
                start: 10.0,
                end: 30.0,
            }),
            verdict: v,
        }
    }

    /// B132 — detection must stand down while a client is streaming. The
    /// regulator expresses "playback is active" by PARKING `bg_io` permits, so
    /// the only thing that makes consensus duck is holding one for the whole
    /// run. Drain the semaphore and the work must not start; release and it
    /// must complete.
    ///
    /// Disarm by dropping the `acquire_gate` call in `detect_gated` and this
    /// goes red on the first assertion — which is the point: before the fix,
    /// consensus ran regardless of playback, on a runtime worker.
    #[tokio::test]
    async fn detection_stands_down_while_playback_holds_the_gate() {
        let bg_io = Arc::new(Semaphore::new(1));
        let cfg = SeasonConfig {
            align: AlignConfig::default(),
            ..SeasonConfig::default()
        };

        // The regulator's "playback active" state: no permits left.
        let parked = bg_io.clone().acquire_owned().await.unwrap();

        let gate = bg_io.clone();
        let mut task =
            tokio::spawn(async move { detect_gated(&gate, Vec::new(), Vec::new(), cfg).await });

        // Nothing may run while the stream holds the gate.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            futures_util::poll!(&mut task).is_pending(),
            "consensus ran while playback held every bg_io permit — it is not ducking"
        );

        // Playback ends → the regulator returns the permit → work proceeds.
        drop(parked);
        let out = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("detection did not resume once the gate reopened")
            .unwrap();
        assert!(out.is_some(), "detection task failed");
    }

    /// The signal is the contract: an episode the detector threw away must be
    /// countable BY REASON, and the reason must carry the numbers that produced
    /// it. A season that emits nothing looks identical to one with no intro
    /// until this exists.
    #[test]
    fn a_dropped_episode_is_countable_by_reason() {
        let _ = crate::obs::init("info", None);
        record_verdicts(
            "/media/TV/Fringe::1",
            "Intro",
            &[
                verdict(1, 12, 12, 0.63, Verdict::Emitted),
                verdict(2, 9, 9, 0.47, Verdict::LowConfidence),
                verdict(3, 2, 1, 0.05, Verdict::FewAgreeing),
                EpisodeVerdict {
                    id: 4,
                    matched: 0,
                    agreeing: 0,
                    confidence: 0.0,
                    span: None,
                    verdict: Verdict::NoSpan,
                },
            ],
        );
        let rendered = crate::obs::render();
        for outcome in ["emitted", "low_confidence", "few_agreeing", "no_span"] {
            assert!(
                rendered
                    .lines()
                    .any(|l| l.starts_with("pharos_segment_detect_total")
                        && l.contains(&format!("outcome=\"{outcome}\""))
                        && l.contains("kind=\"Intro\"")),
                "every verdict must be countable; {outcome} missing from:\n{rendered}"
            );
        }
    }
}
