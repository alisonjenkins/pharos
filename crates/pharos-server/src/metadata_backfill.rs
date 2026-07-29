//! T9 — online metadata-enrichment orchestrator.
//!
//! A paced background sweep that mirrors the T81 person-image backfill
//! ([`crate::person_image_backfill`]): pull every item still eligible for an
//! online match ([`MediaStore::items_needing_match`]), resolve each against
//! the configured providers (TMDB for movies, TVDB→TMDB for episodes), merge
//! the fetched metadata WITHOUT clobbering curated local data
//! ([`apply_enrichment`]), download + cache the chosen artwork
//! ([`download_and_cache_art`]), and record the resulting match state
//! ([`MediaStore::set_item_match`]) so the item drops out of a later pass
//! (self-terminating; TTL re-admits it once stale).
//!
//! Each network call draws a permit from the shared `bg_io` gate so the sweep
//! paces itself against live playback exactly like the trickplay / subtitle /
//! person-image sweeps (V34), with a courtesy [`REQUEST_SPACING`] between
//! items on top.
//!
//! Match-state discipline (mirrors [`items_needing_match`]'s filter): a
//! `manual` row is NEVER reprocessed — a user override survives every pass.
//! Every other row is re-admitted once its `metadata_refreshed_at` predates
//! the TTL cutoff, `nfo_id` included: an NFO id settles WHICH record the item
//! is, not what that record currently says, and [`resolve`] feeds a stored id
//! straight to the detail fetch without ever searching, so re-admitting one
//! cannot change the match. Excluding them meant three quarters of a
//! real library (10,564 of 14,072 items) never received an online field at
//! all — including `original_language`, which the `OriginalLanguage` audio
//! preference needs.
//!
//! [`items_needing_match`]: pharos_core::MediaStore::items_needing_match

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::bg_io::BgPermit;
use crate::config::MetadataConfig;
use crate::musicbrainz::{AlbumArtResolver, MusicBrainzClient};
use crate::online_enrich::{
    apply_enrichment, download_and_cache_art, EnrichedMetadata, OnlineEnricher, RemoteArt,
};
use crate::state::Stores;
use crate::tmdb::TmdbEnricher;
use crate::tvdb::{ReqwestTransport, TvdbEnricher};
use pharos_cache::image_cache::ImageRole;
use pharos_cache::ImageCache;
use pharos_core::{
    match_best, ArtworkRole, DomainResult, GenreStore, MediaItem, MediaKind, MediaStore,
    PersonStore, ProviderIds, SeriesMatchCandidate, SeriesMetadata, SeriesMetadataStore,
};
use pharos_scanner::FilenameProvider;

/// Courtesy delay between items — well under either provider's published rate
/// ceiling so a full backfill never trips limiting. This is on top of the
/// `bg_io` gate (which throttles against playback, not the remote API). Mirrors
/// T81's 120ms.
const REQUEST_SPACING: Duration = Duration::from_millis(120);

/// Delay between passes while enrichment is still making progress. A pass caps
/// at `max_per_pass` items, so a large first-boot backlog needs many passes;
/// this short gap clears it in minutes instead of one batch per pod restart,
/// while still yielding the runtime between passes. Once a pass enriches
/// nothing the loop switches to the long `refresh_interval_secs` idle instead.
const DRAIN_GAP: Duration = Duration::from_secs(30);

/// Outcome of one enrichment pass, driving both the log line and the loop
/// cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PassStats {
    /// Items newly matched + persisted this pass (search/nfo hits) — the
    /// observable "enriched" count.
    pub enriched: usize,
    /// Whether the pass had any eligible work at all (the item query returned
    /// rows, or the series query returned candidates). This — NOT the hit
    /// count — decides the cadence: a pass that pulls 5000 items and marks
    /// them all `none`/transient enriches zero HITS but still made real
    /// progress (those rows now drop out), so the loop must keep draining, not
    /// idle for hours. Only a genuinely empty pass (nothing eligible) idles.
    pub had_work: bool,
}

/// How long the enrichment loop waits before its next pass. `had_work` → the
/// eligible set was non-empty, so drain progress was made and more likely
/// remains: re-run after the short `drain` gap. `!had_work` → nothing was
/// eligible (fully drained), so idle the long `idle` interval, which also
/// re-admits newly-scanned media and TTL-expired rows on the next wake.
///
/// Keying on `had_work` rather than the hit count is load-bearing: the backlog
/// is dominated by rows that resolve to `none` (no confident match) or a
/// transient miss, neither of which is a "hit" — but processing them IS
/// progress (they get stamped and leave the eligible set). Idling 6h after
/// every such pass stalled convergence for days.
fn next_delay(had_work: bool, drain: Duration, idle: Duration) -> Duration {
    if had_work {
        drain
    } else {
        idle
    }
}

/// Artwork roles this pass downloads + caches. Bounds the per-item network
/// cost to the roles clients actually render prominently (poster / backdrop /
/// per-episode still / logo); any other role a provider offers (banners,
/// discs) is logged and skipped. `Thumb` covers the TMDB/TVDB per-episode
/// still image (`RemoteArt{ role: Thumb }` from `tmdb::parse_episode_detail`
/// / `tvdb::parse_episode_detail`) — Task 11.5 closes the gap where episode
/// stills were fetched by the parse layer but silently dropped here.
const CACHED_ART_ROLES: [ArtworkRole; 4] = [
    ArtworkRole::Primary,
    ArtworkRole::Backdrop,
    ArtworkRole::Thumb,
    ArtworkRole::Logo,
];

/// Unix time in whole seconds (0 if the clock is before the epoch). Mirrors
/// the server-wide helper; `run`/`enrich_one` take `now` as a parameter so
/// tests are deterministic, and only [`spawn`] (and the T11 manual-apply
/// handler) reads the wall clock.
pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Spawn the recurring enrichment loop on the tokio runtime. Fire-and-forget:
/// a failure aborts only the current pass (logged), never the loop or the
/// server. Mirrors [`crate::person_image_backfill::spawn`]. `now` is recomputed
/// once per pass so every item in a pass shares one timestamp.
///
/// The loop drives the whole library to convergence without a pod restart:
/// each pass caps at `max_per_pass` items, so after a pass that enriched
/// anything it re-runs after the short [`DRAIN_GAP`] (clearing a first-boot
/// backlog in minutes); after a pass that enriched nothing it idles
/// `cfg.refresh_interval_secs` before re-checking, which also re-admits
/// newly-scanned media and TTL-expired rows.
///
/// Only the elected background-work leader (`is_bg_leader`, B2) enriches, so a
/// rolling-deploy surge never doubles provider API spend; a non-leader replica
/// polls leadership on the same short cadence and takes over promptly if the
/// leader goes away.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    stores: Stores,
    bg_io: Arc<Semaphore>,
    cache: Arc<ImageCache>,
    tmdb: Option<TmdbEnricher>,
    tvdb: Option<TvdbEnricher<ReqwestTransport>>,
    musicbrainz: Option<MusicBrainzClient>,
    cfg: MetadataConfig,
    is_bg_leader: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let idle = Duration::from_secs(cfg.refresh_interval_secs.max(1));
        loop {
            // B2 — defer to the background-work singleton. A follower re-checks
            // on the short cadence so leadership handoff (leader eviction during
            // a rollout) is picked up within `DRAIN_GAP`, not a full interval.
            if !is_bg_leader.load(Ordering::Relaxed) {
                tokio::time::sleep(DRAIN_GAP).await;
                continue;
            }
            let now = now_secs();
            let stats = match run(
                &stores,
                &bg_io,
                cache.as_ref(),
                tmdb.as_ref(),
                tvdb.as_ref(),
                musicbrainz.as_ref(),
                &cfg,
                now,
            )
            .await
            {
                Ok(s) => {
                    tracing::info!(enriched = s.enriched, "T9 metadata backfill: pass complete");
                    s
                }
                Err(e) => {
                    tracing::warn!(error = %e, "T9 metadata backfill: pass aborted");
                    // Treat a hard pass error as "no work" → back off on the
                    // long idle rather than hammering a failing provider on the
                    // short drain cadence.
                    PassStats {
                        enriched: 0,
                        had_work: false,
                    }
                }
            };
            tokio::time::sleep(next_delay(stats.had_work, DRAIN_GAP, idle)).await;
        }
    });
}

/// Run one enrichment pass, returning how many items were newly enriched
/// (fetched + persisted). Generic over the concrete enricher types
/// ([`OnlineEnricher`] is not object-safe — RPITIT — so no `dyn`) and over the
/// store's trait bounds. Extracted from [`spawn`] so it's directly awaitable
/// in tests with fake enrichers + a real in-memory [`SqliteStore`].
///
/// [`SqliteStore`]: pharos_store_sqlx::sqlite::SqliteStore
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run<Tm, Tv, Mb, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    tmdb: Option<&Tm>,
    tvdb: Option<&Tv>,
    musicbrainz: Option<&Mb>,
    cfg: &MetadataConfig,
    now: i64,
) -> DomainResult<PassStats>
where
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
    Mb: AlbumArtResolver,
    S: MediaStore
        + GenreStore
        + PersonStore
        + SeriesMetadataStore
        + pharos_store_sqlx::ServerConfigStore,
{
    // No provider configured → nothing to do (mirrors spawn's key gate).
    if tmdb.is_none() && tvdb.is_none() && musicbrainz.is_none() {
        return Ok(PassStats {
            enriched: 0,
            had_work: false,
        });
    }
    // Items whose last enrichment predates this cutoff (or never matched) are
    // eligible; `manual` rows are excluded by the query itself.
    let ttl_cutoff = now.saturating_sub(i64::from(cfg.refresh_ttl_days) * 86_400);
    let items = store
        .items_needing_match(cfg.max_per_pass, ttl_cutoff)
        .await?;
    let mut enriched = 0usize;
    // Any eligible row (even one that resolves to `none`/transient) is work:
    // processing it advances the backlog, so the loop should keep draining.
    let mut had_work = !items.is_empty();
    if !items.is_empty() {
        tracing::info!(total = items.len(), "T9 metadata backfill: enriching items");
        for item in items {
            // V6 — one bad item (a provider blip, a store hiccup) never aborts
            // the pass; log it and carry on to the next.
            match enrich_one(store, bg_io, cache, tmdb, tvdb, cfg, item, now).await {
                Ok(true) => enriched += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "T9 metadata backfill: item failed"),
            }
            tokio::time::sleep(REQUEST_SPACING).await;
        }
    }

    // Album-art pass. Independent of the item loop above: that query is
    // restricted to movies and episodes, and audio rows are selected on
    // "has no cover" rather than "metadata is stale".
    if let Some(mb) = musicbrainz {
        match enrich_audio_pass(store, bg_io, cache, mb, cfg, now).await {
            Ok((n, audio_had_work)) => {
                enriched += n;
                had_work |= audio_had_work;
            }
            Err(e) => tracing::warn!(error = %e, "musicbrainz album-art pass aborted"),
        }
    }

    // Series-container pass (the series-level record). Runs even when no items
    // needed matching: episodes and shows drain independently.
    if tvdb.is_some() || tmdb.is_some() {
        match enrich_series_pass(store, bg_io, cache, tvdb, tmdb, cfg, now).await {
            Ok((n, series_had_work)) => {
                if n > 0 {
                    tracing::info!(enriched = n, "T9-series metadata backfill: shows enriched");
                }
                enriched += n;
                had_work |= series_had_work;
            }
            Err(e) => tracing::warn!(error = %e, "T9-series metadata backfill: pass aborted"),
        }
    }
    Ok(PassStats { enriched, had_work })
}

/// The outcome of resolving one item against a single provider.
enum Resolved {
    /// A record was fetched and is ready to apply.
    Hit {
        external_id: String,
        source: &'static str,
        confidence: Option<f32>,
        // Boxed: `EnrichedMetadata` is large, and the other variants are
        // empty — boxing keeps the enum small (clippy::large_enum_variant).
        enriched: Box<EnrichedMetadata>,
    },
    /// Search returned no candidate over the confidence floor — mark `none`
    /// so the row isn't re-searched every pass (TTL still re-admits it later).
    NoMatch,
    /// The id resolved (via search or a pre-set provider id) but the detail
    /// fetch came back empty — a transient provider miss. Carries the resolved
    /// id so [`enrich_one`] can stamp the row (`match_source` + `external_id` +
    /// `metadata_refreshed_at = now`) and drop it out of the eligible front,
    /// rather than leaving it untouched to be re-pulled every pass (which would
    /// block lower-id items queued behind it). A `search`-class row is
    /// re-admitted for a retry once the TTL cutoff passes.
    Transient {
        external_id: String,
        source: &'static str,
        confidence: Option<f32>,
    },
}

/// Terminal outcome of one item's enrichment attempt, recorded as
/// `pharos_metadata_enrich_total{kind,outcome}`. The DB residue cannot tell
/// these apart: an episode that resolved but whose detail fetch came back
/// empty (`Transient`), and one whose record simply carried no still
/// (`HitNoArt`), both end `match_source='nfo_id'`, `metadata_refreshed_at=now`,
/// no `Primary` — so the "5 of 76 Code Geass episodes got a still" gap is
/// invisible in production without this counter. The reason travels with the
/// value (provider + external id + season/episode + offered roles) in the
/// decision log, never a bare class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnrichOutcome {
    /// Resolved and cached at least one artwork role.
    HitArt,
    /// Resolved to a record but cached NO artwork — the record offered no
    /// role this path caches, or every offered image failed to download.
    HitNoArt,
    /// The id resolved but the detail fetch returned nothing (rate-limit /
    /// network). Stamped and retried once the TTL re-admits the row.
    Transient,
    /// No confident match; the row was recorded `none`.
    NoMatch,
    /// A concurrent manual Identify won mid-flight; this sweep's write was
    /// skipped rather than reverting the user's choice.
    SkippedManual,
    /// No provider configured for the kind, or an audio item — nothing tried.
    Skipped,
}

impl EnrichOutcome {
    /// Stable, bounded-cardinality metric label. Asserted distinct in a test —
    /// a rename here breaks the dashboard/alert contract silently otherwise.
    fn label(self) -> &'static str {
        match self {
            Self::HitArt => "hit_art",
            Self::HitNoArt => "hit_no_art",
            Self::Transient => "transient",
            Self::NoMatch => "no_match",
            Self::SkippedManual => "skipped_manual",
            Self::Skipped => "skipped",
        }
    }
}

/// A `Hit`'s outcome is decided by whether any art actually landed — the whole
/// point of the signal, split out so it can be unit-tested without a live
/// provider.
fn hit_outcome(cached_art_roles: usize) -> EnrichOutcome {
    if cached_art_roles > 0 {
        EnrichOutcome::HitArt
    } else {
        EnrichOutcome::HitNoArt
    }
}

fn kind_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movie => "movie",
        MediaKind::Episode => "episode",
        MediaKind::Audio => "audio",
    }
}

fn record_enrich(kind: MediaKind, outcome: EnrichOutcome) {
    metrics::counter!(
        "pharos_metadata_enrich_total",
        "kind" => kind_label(kind),
        "outcome" => outcome.label(),
    )
    .increment(1);
}

/// Enrich a single item end-to-end. Returns `Ok(true)` when a record was
/// fetched + persisted (counts toward the pass total), `Ok(false)` when the
/// item was skipped, marked `none`, or hit a transient miss.
///
/// `now` is injected (not read from the clock) so tests are deterministic.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn enrich_one<Tm, Tv, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    tmdb: Option<&Tm>,
    tvdb: Option<&Tv>,
    cfg: &MetadataConfig,
    mut item: MediaItem,
    now: i64,
) -> DomainResult<bool>
where
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
    S: MediaStore + GenreStore + PersonStore,
{
    let stem = item
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(item.title.as_str());

    // Search key: a movie parses (title, year) from its filename; an episode
    // searches by SERIES name/year (the fetch narrows to season/episode) — the
    // episode filename title would never match a series search.
    let (title, year, season, episode) = match item.kind {
        MediaKind::Movie => {
            let parsed = FilenameProvider::parse_stem(stem, true);
            (
                parsed.title.unwrap_or_else(|| item.title.clone()),
                parsed.year,
                None,
                None,
            )
        }
        MediaKind::Episode => {
            let series = item.series.as_ref();
            let title = series
                .map(|s| s.series_name.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    FilenameProvider::parse_stem(stem, false)
                        .title
                        .unwrap_or_else(|| item.title.clone())
                });
            (
                title,
                series.and_then(|s| s.series_year),
                series.and_then(|s| s.season_number),
                series.and_then(|s| s.episode_number),
            )
        }
        // No provider covers audio here — skip (never marked).
        MediaKind::Audio => {
            record_enrich(MediaKind::Audio, EnrichOutcome::Skipped);
            return Ok(false);
        }
    };

    // Provider by kind: Episode prefers TVDB (fallback TMDB when TVDB isn't
    // configured); Movie is TMDB. When the kind's providers are all absent,
    // skip the item (leaves it eligible for a later pass with a key present).
    let provider_ids = item.metadata.provider_ids.clone();
    let (matched_provider, resolved) = match item.kind {
        MediaKind::Movie => match tmdb {
            Some(t) => (
                "tmdb",
                resolve(
                    t,
                    item.kind,
                    "tmdb",
                    &title,
                    year,
                    season,
                    episode,
                    &provider_ids,
                    bg_io,
                    cfg,
                )
                .await,
            ),
            None => {
                record_enrich(item.kind, EnrichOutcome::Skipped);
                return Ok(false);
            }
        },
        MediaKind::Episode => {
            if let Some(t) = tvdb {
                (
                    "tvdb",
                    resolve(
                        t,
                        item.kind,
                        "tvdb",
                        &title,
                        year,
                        season,
                        episode,
                        &provider_ids,
                        bg_io,
                        cfg,
                    )
                    .await,
                )
            } else if let Some(t) = tmdb {
                (
                    "tmdb",
                    resolve(
                        t,
                        item.kind,
                        "tmdb",
                        &title,
                        year,
                        season,
                        episode,
                        &provider_ids,
                        bg_io,
                        cfg,
                    )
                    .await,
                )
            } else {
                record_enrich(item.kind, EnrichOutcome::Skipped);
                return Ok(false);
            }
        }
        MediaKind::Audio => {
            record_enrich(MediaKind::Audio, EnrichOutcome::Skipped);
            return Ok(false);
        }
    };

    let (external_id, source, confidence, enriched) = match resolved {
        Resolved::NoMatch => {
            // No confident hit — record `none` (leaves filename metadata) so
            // the row isn't re-searched until the TTL re-admits it. Guard
            // against a concurrent manual apply that landed while this
            // item's search was in flight (FR1 TOCTOU) — a user override
            // must never be reverted by the sweep's trailing write.
            if is_manual(store, item.id).await {
                record_enrich(item.kind, EnrichOutcome::SkippedManual);
                tracing::debug!(
                    media.id = item.id,
                    "T9 metadata backfill: skipping none-write, item matched manually mid-flight"
                );
            } else {
                record_enrich(item.kind, EnrichOutcome::NoMatch);
                store
                    .set_item_match(item.id, matched_provider, "", "none", None, now)
                    .await?;
            }
            return Ok(false);
        }
        Resolved::Transient {
            external_id,
            source,
            confidence,
        } => {
            // The id resolved but the detail fetch returned nothing. Stamp the
            // resolved id + refresh time so this row leaves the eligible front
            // instead of being re-pulled (and re-blocking items behind it)
            // every pass; the TTL re-admits it for a retry later. Guard the
            // same manual-override TOCTOU as the other terminal writes — never
            // revert a user's Identify that landed mid-flight.
            if is_manual(store, item.id).await {
                record_enrich(item.kind, EnrichOutcome::SkippedManual);
                tracing::debug!(
                    media.id = item.id,
                    "T9 metadata backfill: skipping transient stamp, item matched manually mid-flight"
                );
            } else {
                record_enrich(item.kind, EnrichOutcome::Transient);
                // A transient miss on an already-identified episode is the
                // shape that silently starves stills (issue #113): the row is
                // stamped refreshed and frozen till the TTL with no art. Name
                // it with the value, not a bare class.
                tracing::info!(
                    media.id = item.id,
                    kind = kind_label(item.kind),
                    provider = matched_provider,
                    external_id = %external_id,
                    season,
                    episode,
                    "T9 metadata backfill: detail fetch returned nothing — id resolved but no record; stamped, retried at TTL"
                );
                store
                    .set_item_match(
                        item.id,
                        matched_provider,
                        &external_id,
                        source,
                        confidence,
                        now,
                    )
                    .await?;
            }
            return Ok(false);
        }
        Resolved::Hit {
            external_id,
            source,
            confidence,
            enriched,
        } => (external_id, source, confidence, *enriched),
    };

    // Fold the record onto the item (local data always wins — apply_enrichment
    // only fills gaps), then stamp the matched provider id if we hadn't one.
    let counts = store.item_entity_counts(item.id).await?;
    let applied = apply_enrichment(&mut item, counts, &enriched);
    match matched_provider {
        "tmdb" if item.metadata.provider_ids.tmdb.is_none() => {
            item.metadata.provider_ids.tmdb = Some(external_id.clone());
        }
        "tvdb" if item.metadata.provider_ids.tvdb.is_none() => {
            item.metadata.provider_ids.tvdb = Some(external_id.clone());
        }
        _ => {}
    }
    store.put(item.clone()).await?;

    // Join entities are linked only when the item had none (apply_enrichment's
    // fill-if-empty gate) — a curated NFO cast/genre list is never diluted.
    if !applied.genres.is_empty() {
        store.link_item_genres(item.id, &applied.genres).await?;
    }
    if !applied.people.is_empty() {
        store.link_item_people(item.id, &applied.people).await?;
    }

    // Artwork: start from the matched provider's art, then (for a TVDB-matched
    // episode) prefer TMDB art bridged via the SERIES-level TMDB id.
    let mut chosen: Vec<(&'static str, RemoteArt)> = Vec::new();
    for art in &enriched.artwork {
        upsert_art(&mut chosen, matched_provider, art, false);
    }
    if matched_provider == "tvdb" {
        if let (Some(tvdb_e), Some(tmdb_e)) = (tvdb, tmdb) {
            // CRITICAL: `also_tmdb_id` is SERIES-scoped. The episode record's
            // own `also_tmdb_id` is episode-level and must NOT be used as a
            // series id — refetch the series (season/episode = None) to read
            // the series-level TMDB id.
            let series_tmdb = {
                let _permit = BgPermit::acquire(bg_io).await;
                tvdb_e.fetch(item.kind, &external_id, None, None).await
            }
            .and_then(|m| m.also_tmdb_id);
            if let Some(tid) = series_tmdb {
                let tmdb_meta = {
                    let _permit = BgPermit::acquire(bg_io).await;
                    tmdb_e.fetch(item.kind, &tid, None, None).await
                };
                if let Some(m) = tmdb_meta {
                    for art in &m.artwork {
                        upsert_art(&mut chosen, "tmdb", art, true);
                    }
                }
            }
        }
    }

    // FR2 — a curated local sidecar (scanner-resolved, source == "local")
    // must never be overwritten by online art: `set_artwork` is an upsert
    // keyed on (item, role), so downloading here would silently replace a
    // user's hand-placed poster/backdrop/etc. Computed once per item, at the
    // role level (a local Primary must not block filling an absent Backdrop).
    let local_roles: std::collections::HashSet<String> = store
        .artwork_for(item.id)
        .await?
        .into_iter()
        .filter(|(_, source, _)| source.eq_ignore_ascii_case("local"))
        .map(|(role, _, _)| role.to_ascii_lowercase())
        .collect();

    // How many art roles this item ends up covered by — newly cached here, or
    // already held by a local sidecar. Zero on a Hit is the #113 signal: the
    // record resolved but left the item with no image.
    let mut art_covered = 0usize;
    for (prov, art) in &chosen {
        if !CACHED_ART_ROLES.contains(&art.role) {
            tracing::debug!(role = ?art.role, item = item.id, "T9 metadata backfill: skipping art role (not cached)");
            continue;
        }
        if local_roles.contains(&art.role.as_str().to_ascii_lowercase()) {
            art_covered += 1;
            tracing::debug!(role = ?art.role, item = item.id, "T9 metadata backfill: skipping art role (local sidecar present)");
            continue;
        }
        let bytes = {
            let _permit = BgPermit::acquire(bg_io).await;
            match *prov {
                "tmdb" => match tmdb {
                    Some(t) => t.fetch_image_bytes(&art.url).await,
                    None => None,
                },
                "tvdb" => match tvdb {
                    Some(t) => t.fetch_image_bytes(&art.url).await,
                    None => None,
                },
                _ => None,
            }
        };
        let Some(bytes) = bytes else { continue };
        match download_and_cache_art(cache, store, &item, prov, art, bytes).await {
            Ok(()) => art_covered += 1,
            Err(e) => {
                tracing::warn!(error = %e, role = ?art.role, "T9 metadata backfill: art cache failed")
            }
        }
    }

    // The signal (#113): a Hit that cached no art is indistinguishable from a
    // transient miss in the DB, so name it here with the roles the record DID
    // offer — an empty `offered` says the provider record carried no image;
    // a non-empty one that still produced no cover says every image failed to
    // download or fell to a role this path does not cache.
    let outcome = hit_outcome(art_covered);
    record_enrich(item.kind, outcome);
    if outcome == EnrichOutcome::HitNoArt {
        let offered: Vec<&str> = enriched.artwork.iter().map(|a| a.role.as_str()).collect();
        tracing::info!(
            media.id = item.id,
            kind = kind_label(item.kind),
            provider = matched_provider,
            external_id = %external_id,
            season,
            episode,
            offered = ?offered,
            "T9 metadata backfill: record resolved but cached no art"
        );
    }

    // Record the match state last — the row now carries the enrichment, so a
    // crash before this point simply re-enriches next pass (idempotent).
    // Re-check for a concurrent manual apply (FR1 TOCTOU): the fetch above
    // took real network time, and a `POST /Items/{id}/RemoteSearch/Apply`
    // may have landed a user override during that window — never clobber it
    // with this sweep's "search"/"nfo_id" write. (`apply_manual_match`
    // itself sets the row to "manual" BEFORE calling `enrich_one`, so this
    // guard also correctly no-ops the write on the manual-apply path; that
    // caller re-asserts "manual" afterward regardless.)
    if is_manual(store, item.id).await {
        tracing::debug!(
            media.id = item.id,
            "T9 metadata backfill: skipping match-write, item matched manually mid-flight"
        );
    } else {
        store
            .set_item_match(
                item.id,
                matched_provider,
                &external_id,
                source,
                confidence,
                now,
            )
            .await?;
    }
    Ok(true)
}

/// FR1 — true when `id`'s row is currently `match_source = "manual"`
/// (case-insensitive). Used immediately before every terminal
/// `set_item_match` write in [`enrich_one`] to detect a concurrent manual
/// override (a `POST /Items/{id}/RemoteSearch/Apply` landing mid-flight)
/// that must never be reverted by this sweep's own write. A store error
/// reading the row is treated as "not manual" — the sweep's write proceeds
/// rather than silently stalling on a transient read hiccup; the write
/// itself will surface any real problem.
async fn is_manual<S: MediaStore>(store: &S, id: pharos_core::MediaId) -> bool {
    store
        .get(id)
        .await
        .ok()
        .and_then(|i| i.match_source)
        .is_some_and(|s| s.eq_ignore_ascii_case("manual"))
}

/// T11 — apply a user's manual Identify choice: persist the override with
/// `match_source = "manual"` FIRST (a user assertion of identity that stands
/// even if the fetch below never runs), then attempt an immediate re-enrich
/// of just this item by handing [`enrich_one`] the chosen id up front (via
/// `provider_ids`) so it fetches EXACTLY the record the user picked instead
/// of re-running its own search.
///
/// `enrich_one`'s own persistence may record a different `match_source`
/// (`"nfo_id"`, since the id is now pre-resolved rather than searched) — the
/// manual override is re-asserted afterward UNCONDITIONALLY so the row is
/// guaranteed to end `match_source = "manual"`, matching the caller-visible
/// contract — and `items_needing_match` excludes `"manual"`, so the user's
/// pick is never revisited by a later pass (an `"nfo_id"` row WOULD be
/// re-admitted by the TTL, which is why the re-assertion matters).
///
/// No provider key / no image cache configured → the override is still
/// persisted (a user's stated identity is honoured regardless of whether
/// pharos can currently fetch it), the re-enrich step is skipped, and the
/// skip is logged. Generic over the same `Tm`/`Tv`/`S` bounds as
/// [`enrich_one`] so tests can drive it against a real in-memory
/// `SqliteStore` with a fake enricher, exactly like this module's own tests.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_manual_match<Tm, Tv, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: Option<&ImageCache>,
    tmdb: Option<&Tm>,
    tvdb: Option<&Tv>,
    cfg: &MetadataConfig,
    id: u64,
    provider: &str,
    external_id: &str,
    now: i64,
) -> DomainResult<()>
where
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
    S: MediaStore + GenreStore + PersonStore,
{
    store
        .set_item_match(id, provider, external_id, "manual", None, now)
        .await?;

    let Some(cache) = cache else {
        tracing::info!(
            media.id = id,
            "T11 manual match: immediate re-enrich skipped (no image cache configured)"
        );
        return Ok(());
    };
    if tmdb.is_none() && tvdb.is_none() {
        tracing::info!(
            media.id = id,
            "T11 manual match: immediate re-enrich skipped (no provider key configured)"
        );
        return Ok(());
    }
    let Ok(mut item) = store.get(id).await else {
        // Caller already resolved the item before calling this fn; a row
        // that vanished between calls is not this fn's problem to raise —
        // the manual override above is already persisted either way.
        return Ok(());
    };
    match provider {
        "tmdb" => item.metadata.provider_ids.tmdb = Some(external_id.to_string()),
        "tvdb" => item.metadata.provider_ids.tvdb = Some(external_id.to_string()),
        _ => {}
    }
    store.put(item.clone()).await?;

    if let Err(e) = enrich_one(store, bg_io, cache, tmdb, tvdb, cfg, item, now).await {
        tracing::warn!(
            error = %e,
            media.id = id,
            "T11 manual match: immediate re-enrich failed (match itself is already persisted)"
        );
    }

    // enrich_one's own persistence may have overwritten match_source (e.g.
    // "nfo_id", since we just pre-seeded provider_ids above) — re-assert the
    // manual override so it wins regardless of what the fetch above did.
    store
        .set_item_match(id, provider, external_id, "manual", None, now)
        .await?;
    Ok(())
}

/// Resolve one item against a single provider: determine the external id
/// (NFO id if this provider's slot is set, else search + `match_best`), then
/// fetch the full record. Generic over the concrete enricher (RPITIT → no
/// `dyn`). Each network call holds a `bg_io` permit only for its own duration.
#[allow(clippy::too_many_arguments)]
async fn resolve<E: OnlineEnricher>(
    enricher: &E,
    kind: MediaKind,
    provider: &str,
    title: &str,
    year: Option<u32>,
    season: Option<u32>,
    episode: Option<u32>,
    provider_ids: &ProviderIds,
    bg_io: &Arc<Semaphore>,
    cfg: &MetadataConfig,
) -> Resolved {
    // A pre-existing id for THIS provider (from an NFO) is authoritative —
    // skip the search entirely. An `imdb`-only id can't address a TMDB/TVDB
    // fetch, so it falls through to search rather than being fed to fetch.
    let nfo_id = match provider {
        "tmdb" => provider_ids.tmdb.clone(),
        "tvdb" => provider_ids.tvdb.clone(),
        _ => None,
    };
    let (external_id, source, confidence) = if let Some(id) = nfo_id {
        (id, "nfo_id", None)
    } else {
        let candidates = {
            let _permit = BgPermit::acquire(bg_io).await;
            enricher.search(kind, title, year).await
        };
        match match_best(title, year, &candidates, cfg.match_min_confidence) {
            Some(o) => (o.id, "search", Some(o.confidence)),
            None => return Resolved::NoMatch,
        }
    };
    let enriched = {
        let _permit = BgPermit::acquire(bg_io).await;
        enricher.fetch(kind, &external_id, season, episode).await
    };
    match enriched {
        Some(e) => Resolved::Hit {
            external_id,
            source,
            confidence,
            enriched: Box::new(e),
        },
        None => Resolved::Transient {
            external_id,
            source,
            confidence,
        },
    }
}

/// Insert `art` into `chosen` keyed by its role. When a role is already
/// present, replace it only if `replace` (used to let bridged TMDB art win
/// over the matched provider's art per role); otherwise the first wins.
fn upsert_art(
    chosen: &mut Vec<(&'static str, RemoteArt)>,
    provider: &'static str,
    art: &RemoteArt,
    replace: bool,
) {
    if let Some(slot) = chosen.iter_mut().find(|(_, a)| a.role == art.role) {
        if replace {
            *slot = (provider, art.clone());
        }
    } else {
        chosen.push((provider, art.clone()));
    }
}

/// Key under which the album-art matcher version that produced the CURRENT
/// artwork is recorded.
const ALBUM_ART_VERSION_KEY: &str = "musicbrainz_album_art_version";

/// Drop every MusicBrainz-sourced cover when the matcher that chose them has
/// changed, so a corrected matcher can revisit its own mistakes.
///
/// B150 — the reason this exists. Owl City's `Fireflies` was given The Attic's
/// sleeve, and because `audio_items_needing_art` skips anything that already
/// has art, no later pass would ever have looked at it again. A cached cover is
/// only as trustworthy as the matcher that picked it, so the matcher's version
/// is recorded beside the artwork and a bump clears the lot. Mirrors the
/// `TRICKPLAY_GEN_VERSION` wipe-and-regenerate idiom.
///
/// Costs one config read per pass and nothing else once the versions agree.
async fn invalidate_stale_album_art<S>(store: &S) -> DomainResult<()>
where
    S: MediaStore + pharos_store_sqlx::ServerConfigStore,
{
    let want = crate::musicbrainz::ALBUM_ART_QUERY_VERSION.to_string();
    let have = store
        .load_named_config(ALBUM_ART_VERSION_KEY)
        .await
        .map_err(|e| pharos_core::DomainError::Backend(e.to_string()))?;
    if have.as_deref() == Some(want.as_str()) {
        return Ok(());
    }
    let cleared = store.clear_provider_artwork("musicbrainz").await?;
    tracing::info!(
        cleared_items = cleared,
        from = have.as_deref().unwrap_or("none"),
        to = want.as_str(),
        "musicbrainz album-art: matcher version changed, dropping its covers for a re-match"
    );
    store
        .set_named_config(ALBUM_ART_VERSION_KEY, &want)
        .await
        .map_err(|e| pharos_core::DomainError::Backend(e.to_string()))?;
    Ok(())
}

/// One album-art pass: give every coverless audio track the front cover of its
/// album.
///
/// Deliberately narrow. It does not touch scalars, genres or people — those
/// come from the file's own tags and are already better than anything a
/// release-group lookup would add — and it never overwrites art an item
/// already has, because the query selects on `has_primary_art = false`. What it
/// buys is the tile: a track with a cached Primary flips `has_primary_art`,
/// which is what the album, album-artist and artist views synthesise their own
/// images from.
///
/// Every item is stamped with a match state whatever the outcome, so a track
/// leaves the eligible set either way and a library of unmatched albums does
/// not re-run the same rate-limited searches on every pass. A miss records
/// `none` (the TTL re-admits it later, in case the album is added to
/// MusicBrainz); a hit records the release-group id.
async fn enrich_audio_pass<Mb, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    mb: &Mb,
    cfg: &MetadataConfig,
    now: i64,
) -> DomainResult<(usize, bool)>
where
    Mb: AlbumArtResolver,
    S: MediaStore + pharos_store_sqlx::ServerConfigStore,
{
    // A wrong cover is invisible to the eligibility query, which skips anything
    // that already has art — so a matcher fix cannot reach its own mistakes
    // unless the pass clears them first.
    if let Err(e) = invalidate_stale_album_art(store).await {
        tracing::warn!(error = %e, "album-art invalidation check failed");
    }
    let ttl_cutoff = now.saturating_sub(i64::from(cfg.refresh_ttl_days) * 86_400);
    let items = store
        .audio_items_needing_art(
            cfg.max_per_pass,
            ttl_cutoff,
            &crate::musicbrainz::miss_marker(),
        )
        .await?;
    let had_work = !items.is_empty();
    if !had_work {
        return Ok((0, false));
    }
    tracing::info!(
        total = items.len(),
        "musicbrainz album-art pass: tracks without cover art"
    );

    let mut enriched = 0usize;
    for item in items {
        // V6 — one bad track never aborts the pass.
        match enrich_audio_one(store, bg_io, cache, mb, &item, now).await {
            Ok(true) => enriched += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(
                error = %e,
                media.id = item.id,
                "musicbrainz album-art: track failed"
            ),
        }
        tokio::time::sleep(REQUEST_SPACING).await;
    }
    if enriched > 0 {
        tracing::info!(enriched, "musicbrainz album-art pass: covers cached");
    }
    Ok((enriched, had_work))
}

/// Resolve and cache one track's album cover. `Ok(true)` when art landed.
async fn enrich_audio_one<Mb, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    mb: &Mb,
    item: &MediaItem,
    now: i64,
) -> DomainResult<bool>
where
    Mb: AlbumArtResolver,
    S: MediaStore,
{
    // The album artist is the right key: on a compilation the per-track artist
    // is the performer of that track, which would not match the release-group.
    // Fall back to the track artist when there is no album-artist tag.
    let artist = item
        .probe
        .album_artist
        .as_deref()
        .or(item.probe.artist.as_deref());
    let album = item.probe.album.as_deref();

    // NOT under a `bg_io` permit. MusicBrainz's own rate gate makes this call
    // block for over a second of pure waiting, and `bg_io` parks to a single
    // permit while someone is streaming (V34) — holding that permit through the
    // wait would let one queued album lookup starve every other background
    // sweep. The lookup touches no media storage; the permit is taken below,
    // around the cache write, which does.
    let result = mb.album_art(artist, album).await;

    let art = match result {
        Ok(a) => a,
        Err(miss) => {
            // Symmetry: the miss says WHY, carrying the offending value, on the
            // same fields the hit reports. A reason that doesn't name the value
            // is another round of guessing.
            tracing::info!(
                media.id = item.id,
                artist = artist.unwrap_or("-"),
                album = album.unwrap_or("-"),
                outcome = "miss",
                reason = miss.label(),
                detail = %miss,
                "musicbrainz album-art: decision"
            );
            record_album_art(miss.label());
            // A track with no album tag can never be resolved by an album
            // lookup, so stamping it `none` is right; so is stamping a genuine
            // no-match. A transport failure is NOT the item's fault — leave it
            // eligible so the next pass retries rather than burning its TTL.
            if !matches!(miss, crate::musicbrainz::AlbumArtMiss::Unavailable { .. }) {
                // Stamp the query version alongside the miss so a later
                // improvement to the lookup re-admits this row at once instead
                // of waiting out the full TTL.
                store
                    .set_item_match(
                        item.id,
                        "musicbrainz",
                        &crate::musicbrainz::miss_marker(),
                        "none",
                        None,
                        now,
                    )
                    .await?;
            }
            return Ok(false);
        }
    };

    let remote = art.remote_art();
    // The cache write IS local disk I/O, on the volume playback reads through,
    // so it paces against live playback like every other sweep (V34).
    let permit = BgPermit::acquire(bg_io).await;
    let cached = download_and_cache_art(
        cache,
        store,
        item,
        "musicbrainz",
        &remote,
        art.bytes.clone(),
    )
    .await;
    drop(permit);
    cached?;
    store
        .set_item_match(
            item.id,
            "musicbrainz",
            &art.mbid,
            "search",
            Some(art.confidence),
            now,
        )
        .await?;
    tracing::info!(
        media.id = item.id,
        artist = artist.unwrap_or("-"),
        album = album.unwrap_or("-"),
        outcome = "hit",
        mbid = %art.mbid,
        confidence = art.confidence,
        bytes = art.bytes.len(),
        "musicbrainz album-art: decision"
    );
    record_album_art("hit");
    Ok(true)
}

/// `pharos_album_art_total{outcome}` — one series per decision the album-art
/// pass can reach. `hit` plus every [`AlbumArtMiss`] label, so "how much of the
/// music library still has no cover, and why" is a single query.
///
/// [`AlbumArtMiss`]: crate::musicbrainz::AlbumArtMiss
fn record_album_art(outcome: &'static str) {
    metrics::counter!("pharos_album_art_total", "outcome" => outcome).increment(1);
}

/// One enrichment pass over the Series *containers* (T9-series). A show has no
/// `media_items` row, so this can't ride the item loop — it enumerates distinct
/// shows via [`SeriesMetadataStore::series_needing_match`] and enriches each
/// against the show providers in order. Returns how many shows were newly
/// enriched (counts toward the pass total, so the loop keeps draining while
/// shows remain). Series are TV-only, so this is a no-op when neither provider
/// is configured.
async fn enrich_series_pass<Tv, Tm, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    tvdb: Option<&Tv>,
    tmdb: Option<&Tm>,
    cfg: &MetadataConfig,
    now: i64,
) -> DomainResult<(usize, bool)>
where
    Tv: OnlineEnricher,
    Tm: OnlineEnricher,
    S: SeriesMetadataStore,
{
    let providers = SeriesProviders { tvdb, tmdb };
    let ttl_cutoff = now.saturating_sub(i64::from(cfg.refresh_ttl_days) * 86_400);
    let candidates = store
        .series_needing_match(cfg.max_per_pass, ttl_cutoff)
        .await?;
    // Any eligible show is work (even one that resolves to `none`), so the loop
    // keeps draining rather than idling — see [`PassStats::had_work`].
    let had_work = !candidates.is_empty();
    let mut enriched = 0usize;
    for cand in candidates {
        // V6 — one bad show never aborts the pass; log it and carry on.
        match enrich_one_series(store, bg_io, cache, &providers, cfg, cand, now).await {
            Ok(true) => enriched += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "T9-series metadata backfill: show failed"),
        }
        tokio::time::sleep(REQUEST_SPACING).await;
    }
    Ok((enriched, had_work))
}

/// The show providers, in the order they are asked.
///
/// TVDB first (it is the show-shaped provider), then TMDB. Bundled because
/// they are one decision — "who can identify this show" — rather than two
/// independent inputs, and because a resolver that takes both individually
/// invites a call site that passes only one.
struct SeriesProviders<'a, Tv, Tm> {
    tvdb: Option<&'a Tv>,
    tmdb: Option<&'a Tm>,
}

/// Enrich one show end-to-end: search TVDB by name (+year), fetch the
/// series-level record (season/episode = `None`), cache its poster + backdrop,
/// and upsert the [`SeriesMetadata`] row. Returns `Ok(true)` on a persisted
/// match, `Ok(false)` when it marked the show `none` (no confident hit) or hit
/// a transient miss (left for the next pass). Local data can't be clobbered —
/// a show has no curated local metadata, so this simply writes the record.
async fn enrich_one_series<Tv, Tm, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    providers: &SeriesProviders<'_, Tv, Tm>,
    cfg: &MetadataConfig,
    cand: SeriesMatchCandidate,
    now: i64,
) -> DomainResult<bool>
where
    Tv: OnlineEnricher,
    Tm: OnlineEnricher,
    S: SeriesMetadataStore,
{
    // B125 — TVDB first (it is the show-shaped provider), then TMDB. 44 of the
    // deployed library's 178 shows were marked `none` by TVDB alone, almost all
    // of them anime — Death Note, Dragon Ball GT, Code Geass — which TMDB
    // carries. A show marked `none` has no poster, so its tile falls back to a
    // representative episode and then to an extracted frame.
    let mut outcome = None;
    // A provider BLIP is not a provider saying no. If any attempt was
    // transient we leave the row untouched for the next pass rather than
    // freezing the show as `none` for the whole TTL.
    let mut transient = false;
    if let Some(tv) = providers.tvdb {
        match resolve_series(tv, "tvdb", bg_io, cache, cfg, &cand).await {
            SeriesAttempt::Hit(h) => outcome = Some(h),
            SeriesAttempt::Transient => transient = true,
            SeriesAttempt::NoMatch => {}
        }
    }
    let mut fell_back = false;
    if outcome.is_none() {
        if let Some(tm) = providers.tmdb {
            match resolve_series(tm, "tmdb", bg_io, cache, cfg, &cand).await {
                SeriesAttempt::Hit(h) => {
                    outcome = Some(h);
                    fell_back = true;
                }
                SeriesAttempt::Transient => transient = true,
                SeriesAttempt::NoMatch => {}
            }
        }
    }
    if outcome.is_none() && transient {
        return Ok(false);
    }
    let Some(hit) = outcome else {
        // Every configured provider searched and found nothing over the
        // confidence floor → record `none` so the show isn't re-searched until
        // the TTL re-admits it. Attributed to whichever provider ran last.
        let provider = if providers.tmdb.is_some() {
            "tmdb"
        } else {
            "tvdb"
        };
        store
            .upsert_series_metadata(SeriesMetadata {
                series_key: cand.series_key.clone(),
                series_name: cand.series_name.clone(),
                match_provider: Some(provider.into()),
                match_source: Some("none".into()),
                metadata_refreshed_at: Some(now),
                ..Default::default()
            })
            .await?;
        return Ok(false);
    };
    if fell_back {
        tracing::info!(
            series = %cand.series_name,
            provider = hit.provider,
            "T9-series metadata backfill: matched by the fallback provider"
        );
    }

    let provider_ids = match hit.provider {
        "tmdb" => ProviderIds {
            tmdb: Some(hit.external_id.clone()),
            ..Default::default()
        },
        _ => ProviderIds {
            tvdb: Some(hit.external_id.clone()),
            // The series-level record's `also_tmdb_id` IS series-scoped
            // (unlike an episode's), so it's safe to carry as the show's
            // TMDB id.
            tmdb: hit.enriched.also_tmdb_id.clone(),
            ..Default::default()
        },
    };
    store
        .upsert_series_metadata(SeriesMetadata {
            series_key: cand.series_key.clone(),
            series_name: cand.series_name.clone(),
            match_provider: Some(hit.provider.into()),
            match_external_id: Some(hit.external_id),
            match_source: Some("search".into()),
            match_confidence: hit.confidence,
            metadata_refreshed_at: Some(now),
            overview: hit.enriched.overview.clone(),
            community_rating: hit.enriched.community_rating,
            premiere_date: hit.enriched.premiere_date,
            official_rating: hit.enriched.official_rating.clone(),
            original_language: hit.enriched.original_language.clone(),
            genres: hit.enriched.genres.clone(),
            // Neither series-detail parse carries studios/networks today.
            studios: Vec::new(),
            provider_ids,
            poster_locator: hit.poster_locator,
            backdrop_locator: hit.backdrop_locator,
        })
        .await?;
    Ok(true)
}

/// One provider's answer for a show, with its artwork already cached.
struct SeriesHit {
    provider: &'static str,
    external_id: String,
    confidence: Option<f32>,
    enriched: EnrichedMetadata,
    poster_locator: Option<String>,
    backdrop_locator: Option<String>,
}

/// What one provider had to say about a show. `Transient` is kept distinct
/// from `NoMatch` because only the latter justifies freezing the row as
/// `none` until the TTL re-admits it.
enum SeriesAttempt {
    Hit(Box<SeriesHit>),
    NoMatch,
    Transient,
}

/// Search `enricher` for `cand` and, on a confident hit, cache the show's
/// poster + backdrop.
async fn resolve_series<E: OnlineEnricher>(
    enricher: &E,
    provider: &'static str,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    cfg: &MetadataConfig,
    cand: &SeriesMatchCandidate,
) -> SeriesAttempt {
    // Reuse the item resolver: kind=Episode + season/episode=None routes to the
    // provider's SERIES-level search+fetch (TvdbEnricher: search_series →
    // get_series; TmdbEnricher: search_tv → tv_detail). No nfo id for a
    // synthesised show, so it always searches.
    let resolved = resolve(
        enricher,
        MediaKind::Episode,
        provider,
        &cand.series_name,
        cand.series_year,
        None,
        None,
        &ProviderIds::default(),
        bg_io,
        cfg,
    )
    .await;
    let (external_id, confidence, enriched) = match resolved {
        Resolved::NoMatch => return SeriesAttempt::NoMatch,
        Resolved::Transient { .. } => return SeriesAttempt::Transient,
        Resolved::Hit {
            external_id,
            confidence,
            enriched,
            ..
        } => (external_id, confidence, *enriched),
    };
    let poster_locator = cache_series_art(
        cache,
        enricher,
        bg_io,
        &cand.series_key,
        ArtworkRole::Primary,
        ImageRole::Primary,
        &enriched,
    )
    .await;
    let backdrop_locator = cache_series_art(
        cache,
        enricher,
        bg_io,
        &cand.series_key,
        ArtworkRole::Backdrop,
        ImageRole::Backdrop,
        &enriched,
    )
    .await;
    SeriesAttempt::Hit(Box::new(SeriesHit {
        provider,
        external_id,
        confidence,
        enriched,
        poster_locator,
        backdrop_locator,
    }))
}

/// Download the show's artwork of role `want` (if the record offers it) and
/// cache it under `series_key`, returning the cache locator to store. `None`
/// when the record has no such art or the download/cache failed (non-fatal —
/// the images route falls back to a representative episode frame).
async fn cache_series_art<E: OnlineEnricher>(
    cache: &ImageCache,
    enricher: &E,
    bg_io: &Arc<Semaphore>,
    series_key: &str,
    want: ArtworkRole,
    image_role: ImageRole,
    enriched: &EnrichedMetadata,
) -> Option<String> {
    let art = enriched.artwork.iter().find(|a| a.role == want)?;
    let bytes = {
        let _permit = BgPermit::acquire(bg_io).await;
        enricher.fetch_image_bytes(&art.url).await?
    };
    match cache
        .upload_series_art(series_key, image_role, &bytes)
        .await
    {
        Ok(path) => Some(path.to_string_lossy().into_owned()),
        Err(e) => {
            tracing::warn!(error = %e, series_key, "T9-series: art cache write failed");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::musicbrainz::AlbumArt;
    use pharos_core::{MediaItem, SearchCandidate};
    use pharos_store_sqlx::sqlite::SqliteStore;
    use tempfile::TempDir;

    #[test]
    fn enrich_outcome_labels_are_stable_and_distinct() {
        use std::collections::BTreeSet;
        let all = [
            EnrichOutcome::HitArt,
            EnrichOutcome::HitNoArt,
            EnrichOutcome::Transient,
            EnrichOutcome::NoMatch,
            EnrichOutcome::SkippedManual,
            EnrichOutcome::Skipped,
        ];
        // The label strings are a metric contract — bounded and all distinct.
        let labels: BTreeSet<&str> = all.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), all.len(), "duplicate metric label");
        // Pin the exact strings a dashboard/alert queries.
        assert_eq!(EnrichOutcome::HitArt.label(), "hit_art");
        assert_eq!(EnrichOutcome::HitNoArt.label(), "hit_no_art");
        assert_eq!(EnrichOutcome::Transient.label(), "transient");
    }

    #[test]
    fn hit_with_no_cached_art_is_the_no_art_signal() {
        // The whole point of #113: a Hit that landed zero art is hit_no_art,
        // the state that looks identical to a transient miss in the DB. One
        // cached role or more is hit_art.
        assert_eq!(hit_outcome(0), EnrichOutcome::HitNoArt);
        assert_eq!(hit_outcome(1), EnrichOutcome::HitArt);
        assert_eq!(hit_outcome(3), EnrichOutcome::HitArt);
    }

    #[test]
    fn next_delay_drains_while_work_remains_then_idles_when_empty() {
        let drain = Duration::from_secs(30);
        let idle = Duration::from_secs(21_600);
        // A pass that had eligible work → come back after the short drain gap,
        // regardless of whether any of it produced a match hit (rows that
        // resolve to none/transient are still progress and still drain).
        assert_eq!(next_delay(true, drain, idle), drain);
        // A pass with nothing eligible → the pool is drained; idle the long
        // interval before re-checking for new scans / TTL re-admissions.
        assert_eq!(next_delay(false, drain, idle), idle);
    }

    /// A network-free [`OnlineEnricher`]: returns a fixed candidate list for
    /// any search and a fixed record for any fetch. `image_bytes` is `None`
    /// by default (no image bytes); set via [`Self::with_image_bytes`] for
    /// tests that exercise the art-download/cache path.
    struct FakeEnricher {
        provider: &'static str,
        search: Vec<SearchCandidate>,
        detail: Option<EnrichedMetadata>,
        image_bytes: Option<Vec<u8>>,
    }

    impl FakeEnricher {
        fn tmdb() -> Self {
            Self {
                provider: "tmdb",
                search: Vec::new(),
                detail: None,
                image_bytes: None,
            }
        }

        fn tvdb() -> Self {
            Self {
                provider: "tvdb",
                search: Vec::new(),
                detail: None,
                image_bytes: None,
            }
        }

        fn with_search(mut self, cands: Vec<(&str, &str, Option<u32>)>) -> Self {
            self.search = cands
                .into_iter()
                .map(|(id, title, year)| SearchCandidate {
                    id: id.to_string(),
                    title: title.to_string(),
                    year,
                })
                .collect();
            self
        }

        fn with_detail(mut self, detail: EnrichedMetadata) -> Self {
            self.detail = Some(detail);
            self
        }

        fn with_image_bytes(mut self, bytes: Vec<u8>) -> Self {
            self.image_bytes = Some(bytes);
            self
        }
    }

    impl OnlineEnricher for FakeEnricher {
        fn provider(&self) -> &'static str {
            self.provider
        }
        fn supports(&self, _kind: MediaKind) -> bool {
            true
        }
        async fn search(
            &self,
            _kind: MediaKind,
            _title: &str,
            _year: Option<u32>,
        ) -> Vec<SearchCandidate> {
            self.search.clone()
        }
        async fn fetch(
            &self,
            _kind: MediaKind,
            _id: &str,
            _season: Option<u32>,
            _episode: Option<u32>,
        ) -> Option<EnrichedMetadata> {
            self.detail.clone()
        }
        async fn fetch_image_bytes(&self, _url: &str) -> Option<Vec<u8>> {
            self.image_bytes.clone()
        }
        async fn list_images(
            &self,
            _kind: MediaKind,
            _id: &str,
        ) -> Vec<crate::online_enrich::RemoteImage> {
            vec![]
        }
    }

    fn enriched_overview(overview: &str) -> EnrichedMetadata {
        EnrichedMetadata {
            overview: Some(overview.to_string()),
            ..EnrichedMetadata::default()
        }
    }

    async fn store() -> SqliteStore {
        SqliteStore::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite")
    }

    fn cache() -> (TempDir, ImageCache) {
        let td = TempDir::new().expect("tempdir");
        let cache = ImageCache::new(td.path());
        (td, cache)
    }

    fn sem(n: usize) -> Arc<Semaphore> {
        Arc::new(Semaphore::new(n))
    }

    async fn put_movie(store: &SqliteStore, id: u64, title: &str) {
        let item = MediaItem {
            id,
            path: format!("/movies/{title}.mkv").into(),
            title: title.to_string(),
            kind: MediaKind::Movie,
            ..MediaItem::default()
        };
        store.put(item).await.unwrap();
    }

    const NOW: i64 = 1_700_000_000;

    /// A deterministic [`AlbumArtResolver`] — MusicBrainz has no sandbox and
    /// its rate limit makes a live test hostile to the service.
    struct FakeAlbumArt(Result<AlbumArt, crate::musicbrainz::AlbumArtMiss>);

    impl FakeAlbumArt {
        fn hit() -> Self {
            Self(Ok(AlbumArt {
                mbid: "rg-abc-123".into(),
                title: "Ocean Eyes".into(),
                confidence: 0.99,
                year: Some(2009),
                // A real JPEG magic prefix: `download_and_cache_art` writes
                // these bytes to the cache verbatim.
                bytes: vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3],
            }))
        }

        fn miss(m: crate::musicbrainz::AlbumArtMiss) -> Self {
            Self(Err(m))
        }
    }

    impl AlbumArtResolver for FakeAlbumArt {
        async fn album_art(
            &self,
            _artist: Option<&str>,
            _album: Option<&str>,
        ) -> Result<AlbumArt, crate::musicbrainz::AlbumArtMiss> {
            self.0.clone()
        }
    }

    async fn put_track(store: &SqliteStore, id: u64, artist: &str, album: &str) {
        let item = MediaItem {
            id,
            path: format!("/music/{artist}/{album}/{id}.flac").into(),
            title: format!("track {id}"),
            kind: MediaKind::Audio,
            probe: pharos_core::MediaProbe {
                album: Some(album.to_string()),
                album_artist: Some(artist.to_string()),
                ..Default::default()
            },
            ..MediaItem::default()
        };
        store.put(item).await.unwrap();
    }

    // The whole point of the album-art pass: a coverless track ends up with a
    // cached Primary, which is what flips `has_primary_art` — the flag every
    // album / album-artist / artist tile synthesises its own image from.
    #[tokio::test]
    async fn album_art_pass_caches_a_cover_and_records_the_match() {
        let s = store().await;
        put_track(&s, 900_400, "Owl City", "Ocean Eyes").await;
        let (_td, cache) = cache();

        let (n, had_work) = enrich_audio_pass(
            &s,
            &sem(4),
            &cache,
            &FakeAlbumArt::hit(),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
        assert!(had_work);

        let got = s.get(900_400).await.unwrap();
        assert!(got.has_primary_art, "a cached cover must flip the flag");
        assert_eq!(got.match_provider.as_deref(), Some("musicbrainz"));
        assert_eq!(got.match_external_id.as_deref(), Some("rg-abc-123"));
        assert_eq!(got.match_source.as_deref(), Some("search"));
        assert_eq!(got.metadata_refreshed_at, Some(NOW));

        // The artwork row records the provider, so the served image is
        // attributable and `local_artwork_path` can find it.
        let rows = s.artwork_for(900_400).await.unwrap();
        let (role, source, _) = rows
            .into_iter()
            .find(|(r, _, _)| r.eq_ignore_ascii_case("Primary"))
            .expect("a Primary artwork row");
        assert_eq!(role, "Primary");
        assert_eq!(source, "musicbrainz");

        // And it drops out of the eligible set, so the album's other tracks
        // never re-run a rate-limited search for art that already exists.
        let still_eligible = s
            .audio_items_needing_art(10, NOW + 1, &crate::musicbrainz::miss_marker())
            .await
            .unwrap();
        assert!(still_eligible.is_empty());
    }

    // A genuine no-match is stamped so the track leaves the eligible set — a
    // library of unmatched albums must not re-run the same searches every pass.
    #[tokio::test]
    async fn album_art_no_match_is_stamped_so_it_stops_being_retried() {
        let s = store().await;
        put_track(&s, 900_401, "Nobody", "Unreleased Demos").await;
        let (_td, cache) = cache();

        let (n, had_work) = enrich_audio_pass(
            &s,
            &sem(4),
            &cache,
            &FakeAlbumArt::miss(crate::musicbrainz::AlbumArtMiss::NoCandidates {
                query: "releasegroup:\"Unreleased Demos\"".into(),
            }),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 0, "no art landed");
        assert!(had_work, "an eligible row is still work");

        let got = s.get(900_401).await.unwrap();
        assert!(!got.has_primary_art);
        assert_eq!(got.match_source.as_deref(), Some("none"));
        // The verdict carries the query version that reached it.
        assert_eq!(
            got.match_external_id.as_deref(),
            Some(crate::musicbrainz::miss_marker().as_str())
        );
        assert_eq!(got.metadata_refreshed_at, Some(NOW));
        // `none` + a fresh timestamp means the TTL, not the next pass, decides
        // when to look again.
        assert!(s
            .audio_items_needing_art(10, NOW, &crate::musicbrainz::miss_marker())
            .await
            .unwrap()
            .is_empty());
    }

    // A transport failure is NOT the track's fault. Burning its TTL on a
    // MusicBrainz outage would leave the whole music library blank for a month.
    #[tokio::test]
    async fn provider_outage_leaves_the_track_eligible_for_the_next_pass() {
        let s = store().await;
        put_track(&s, 900_402, "Owl City", "Ocean Eyes").await;
        let (_td, cache) = cache();

        let (n, _) = enrich_audio_pass(
            &s,
            &sem(4),
            &cache,
            &FakeAlbumArt::miss(crate::musicbrainz::AlbumArtMiss::Unavailable {
                cause: "connection refused".into(),
            }),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 0);

        let got = s.get(900_402).await.unwrap();
        assert_eq!(
            got.match_source, None,
            "an outage must not stamp a match state"
        );
        assert_eq!(
            s.audio_items_needing_art(10, NOW, &crate::musicbrainz::miss_marker())
                .await
                .unwrap()
                .len(),
            1,
            "the track must still be eligible after a provider outage"
        );
    }

    // A miss is held by the TTL so the album's other tracks don't re-run the
    // same search — but only while the query that reached it is still current.
    // Without this, B142's better queries would have done nothing for 30 days
    // on exactly the 22 albums they fix.
    #[tokio::test]
    async fn a_miss_from_an_older_query_version_is_retried_at_once() {
        let s = store().await;
        put_track(
            &s,
            900_404,
            "Coldplay",
            "A Rush Of Blood To The Head (Japan)",
        )
        .await;
        // Stamped by a previous, worse query — same shape the shipped v1 wrote.
        s.set_item_match(900_404, "musicbrainz", "miss-v1", "none", None, NOW)
            .await
            .unwrap();
        let (_td, cache) = cache();

        // Fresh timestamp, so the TTL alone would exclude it.
        let (n, had_work) = enrich_audio_pass(
            &s,
            &sem(4),
            &cache,
            &FakeAlbumArt::hit(),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 1, "a stale verdict must be revisited, not waited out");
        assert!(had_work);
        assert!(s.get(900_404).await.unwrap().has_primary_art);
    }

    // The converse: a miss reached by the CURRENT query is left alone, so an
    // album that genuinely has no artwork is not re-searched every pass.
    #[tokio::test]
    async fn a_miss_from_the_current_query_version_is_left_alone() {
        let s = store().await;
        put_track(&s, 900_405, "Nobody", "Unreleased Demos").await;
        s.set_item_match(
            900_405,
            "musicbrainz",
            &crate::musicbrainz::miss_marker(),
            "none",
            None,
            NOW,
        )
        .await
        .unwrap();
        let (_td, cache) = cache();

        let (n, had_work) = enrich_audio_pass(
            &s,
            &sem(4),
            &cache,
            &FakeAlbumArt::hit(),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        assert!(!had_work, "a current-version miss waits for the TTL");
    }

    // B150 — a WRONG cover is invisible to the eligibility query, which skips
    // anything that already has art. Without version-gated invalidation, Owl
    // City's `Fireflies` would keep The Attic's sleeve forever, however much
    // the matcher improved.
    #[tokio::test]
    async fn a_matcher_version_bump_drops_its_own_covers_for_a_re_match() {
        let s = store().await;
        put_track(&s, 900_406, "Owl City", "Fireflies").await;
        // A cover from an older, wrong matcher.
        s.set_artwork(
            900_406,
            "Primary",
            "musicbrainz",
            "/cache/primary/audio/wrong.jpg",
        )
        .await
        .unwrap();
        s.set_item_match(
            900_406,
            "musicbrainz",
            "the-attic-mbid",
            "search",
            Some(1.0),
            NOW,
        )
        .await
        .unwrap();
        assert!(s.get(900_406).await.unwrap().has_primary_art);
        // The stored version is deliberately stale.
        pharos_store_sqlx::ServerConfigStore::set_named_config(&s, ALBUM_ART_VERSION_KEY, "1")
            .await
            .unwrap();
        let (_td, cache) = cache();

        let (n, _) = enrich_audio_pass(
            &s,
            &sem(4),
            &cache,
            &FakeAlbumArt::hit(),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();

        assert_eq!(n, 1, "the track must be re-matched, not skipped");
        let got = s.get(900_406).await.unwrap();
        assert_eq!(
            got.match_external_id.as_deref(),
            Some("rg-abc-123"),
            "the wrong match must be replaced"
        );
        // And the version is recorded, so the next pass does not clear again.
        assert_eq!(
            pharos_store_sqlx::ServerConfigStore::load_named_config(&s, ALBUM_ART_VERSION_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(
                crate::musicbrainz::ALBUM_ART_QUERY_VERSION
                    .to_string()
                    .as_str()
            )
        );
    }

    // The converse: once the versions agree, covers are left alone. A pass that
    // re-cleared every time would re-download the whole library hourly.
    #[tokio::test]
    async fn a_current_matcher_version_leaves_existing_covers_alone() {
        let s = store().await;
        put_track(&s, 900_407, "Owl City", "Ocean Eyes").await;
        s.set_artwork(
            900_407,
            "Primary",
            "musicbrainz",
            "/cache/primary/audio/ok.jpg",
        )
        .await
        .unwrap();
        pharos_store_sqlx::ServerConfigStore::set_named_config(
            &s,
            ALBUM_ART_VERSION_KEY,
            &crate::musicbrainz::ALBUM_ART_QUERY_VERSION.to_string(),
        )
        .await
        .unwrap();
        let (_td, cache) = cache();

        let (n, had_work) = enrich_audio_pass(
            &s,
            &sem(4),
            &cache,
            &FakeAlbumArt::hit(),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        assert!(!had_work, "nothing to do once the versions agree");
        assert!(s.get(900_407).await.unwrap().has_primary_art);
    }

    // Movies and episodes are the other pass's business; the album-art pass
    // must never touch them (it would stamp `musicbrainz` over a TMDB match).
    #[tokio::test]
    async fn album_art_pass_ignores_video_items() {
        let s = store().await;
        put_movie(&s, 900_403, "Dune (2021)").await;
        let (_td, cache) = cache();

        let (n, had_work) = enrich_audio_pass(
            &s,
            &sem(4),
            &cache,
            &FakeAlbumArt::hit(),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        assert!(!had_work, "no audio rows means no work");
        assert_eq!(s.get(900_403).await.unwrap().match_provider, None);
    }

    #[tokio::test]
    async fn backfill_matches_by_search_and_persists_match_state() {
        let s = store().await;
        put_movie(&s, 900_100, "Dune (2021)").await; // no NFO id
        let (_td, cache) = cache();
        let tmdb = FakeEnricher::tmdb()
            .with_search(vec![("438631", "Dune", Some(2021))])
            .with_detail(enriched_overview("A duke's son..."));

        let n = run(
            &s,
            &sem(4),
            &cache,
            Some(&tmdb),
            None::<&FakeEnricher>,
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n.enriched, 1);

        let got = s.get(900_100).await.unwrap();
        assert_eq!(got.match_provider.as_deref(), Some("tmdb"));
        assert_eq!(got.match_source.as_deref(), Some("search"));
        assert_eq!(got.match_external_id.as_deref(), Some("438631"));
        assert_eq!(got.metadata.overview.as_deref(), Some("A duke's son..."));
        assert_eq!(got.metadata_refreshed_at, Some(NOW));
        // The matched TMDB id was stamped onto the provider ids.
        assert_eq!(got.metadata.provider_ids.tmdb.as_deref(), Some("438631"));
    }

    #[tokio::test]
    async fn backfill_caches_thumb_role_alongside_primary() {
        // Task 11.5 (Part A): a per-episode still image comes back from the
        // provider as `RemoteArt{ role: Thumb }` (see tmdb::parse_episode_detail
        // / tvdb::parse_episode_detail) — CACHED_ART_ROLES must include Thumb
        // or the download step silently drops it (the `continue` at the
        // `!CACHED_ART_ROLES.contains` guard in `enrich_one`).
        let s = store().await;
        put_movie(&s, 900_103, "Dune (2021)").await;
        let (_td, cache) = cache();
        let tmdb = FakeEnricher::tmdb()
            .with_search(vec![("438631", "Dune", Some(2021))])
            .with_detail(EnrichedMetadata {
                artwork: vec![
                    RemoteArt {
                        role: pharos_core::ArtworkRole::Primary,
                        url: "https://image.tmdb.org/t/p/original/p.jpg".into(),
                    },
                    RemoteArt {
                        role: pharos_core::ArtworkRole::Thumb,
                        url: "https://image.tmdb.org/t/p/original/still.jpg".into(),
                    },
                ],
                ..EnrichedMetadata::default()
            })
            .with_image_bytes(vec![0xFF, 0xD8, 0xFF]); // minimal JPEG-ish bytes

        let n = run(
            &s,
            &sem(4),
            &cache,
            Some(&tmdb),
            None::<&FakeEnricher>,
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n.enriched, 1);

        let art = s.artwork_for(900_103).await.unwrap();
        let roles: Vec<&str> = art.iter().map(|(role, _, _)| role.as_str()).collect();
        assert!(roles.contains(&"Primary"), "roles: {roles:?}");
        assert!(roles.contains(&"Thumb"), "roles: {roles:?}");
    }

    #[tokio::test]
    async fn backfill_never_reprocesses_manual_override() {
        let s = store().await;
        put_movie(&s, 900_101, "Whatever").await;
        // A user override: manual match is excluded from items_needing_match.
        s.set_item_match(900_101, "tmdb", "1", "manual", None, 1)
            .await
            .unwrap();
        let (_td, cache) = cache();
        let tmdb = FakeEnricher::tmdb().with_search(vec![("2", "Whatever", None)]);

        let n = run(
            &s,
            &sem(4),
            &cache,
            Some(&tmdb),
            None::<&FakeEnricher>,
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n.enriched, 0);
        // Its id is untouched by the pass.
        assert_eq!(
            s.get(900_101).await.unwrap().match_external_id.as_deref(),
            Some("1")
        );
    }

    #[tokio::test]
    async fn backfill_no_confident_hit_marks_none() {
        let s = store().await;
        put_movie(&s, 900_102, "Obscure Home Video").await;
        let (_td, cache) = cache();
        // Only a poor candidate → below the confidence floor → NoMatch.
        let tmdb = FakeEnricher::tmdb().with_search(vec![("5", "Something Else", None)]);

        let n = run(
            &s,
            &sem(4),
            &cache,
            Some(&tmdb),
            None::<&FakeEnricher>,
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n.enriched, 0);
        assert_eq!(
            s.get(900_102).await.unwrap().match_source.as_deref(),
            Some("none")
        );
    }

    #[tokio::test]
    async fn backfill_transient_fetch_miss_is_stamped_out_of_the_front() {
        // A confident search match whose detail fetch returns nothing must not
        // be re-pulled every pass (it would block lower-id items queued behind
        // it). It's stamped with the resolved id + refresh time so it drops out
        // of the eligible set until the TTL re-admits it for a retry.
        let s = store().await;
        put_movie(&s, 900_104, "Dune (2021)").await;
        let (_td, cache) = cache();
        // Search returns a clean match, but the fake has no detail → fetch
        // returns None → Resolved::Transient.
        let tmdb = FakeEnricher::tmdb().with_search(vec![("438631", "Dune", Some(2021))]);

        let n = run(
            &s,
            &sem(4),
            &cache,
            Some(&tmdb),
            None::<&FakeEnricher>,
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n.enriched, 0, "a transient miss is not a hit");

        let got = s.get(900_104).await.unwrap();
        assert_eq!(got.match_source.as_deref(), Some("search"));
        assert_eq!(got.match_external_id.as_deref(), Some("438631"));
        assert_eq!(got.metadata_refreshed_at, Some(NOW));
        // With metadata_refreshed_at = now, a cutoff in the past excludes it —
        // it is no longer re-pulled at the front of the next pass.
        let eligible = s.items_needing_match(10, NOW - 1).await.unwrap();
        assert!(
            !eligible.iter().any(|i| i.id == 900_104),
            "a stamped transient row must drop out of the eligible set"
        );
    }

    #[tokio::test]
    async fn apply_manual_match_persists_manual_and_fetches_the_chosen_id() {
        // T11 — the apply handler's core logic. Deliberately leave the fake
        // enricher's `search` empty: if `apply_manual_match` fell back to
        // searching (instead of handing the chosen id straight to `fetch`
        // via `provider_ids`), this would resolve NoMatch and the overview
        // would stay unset — so a set overview proves the direct-fetch path.
        let s = store().await;
        put_movie(&s, 900_200, "Dune (2021)").await;
        let (_td, cache) = cache();
        let tmdb = FakeEnricher::tmdb().with_detail(enriched_overview("A duke's son..."));

        apply_manual_match(
            &s,
            &sem(4),
            Some(&cache),
            Some(&tmdb),
            None::<&FakeEnricher>,
            &MetadataConfig::default(),
            900_200,
            "tmdb",
            "438631",
            NOW,
        )
        .await
        .unwrap();

        let got = s.get(900_200).await.unwrap();
        // The manual override wins — NOT the "nfo_id" source enrich_one's
        // own internal resolve() would otherwise have recorded.
        assert_eq!(got.match_source.as_deref(), Some("manual"));
        assert_eq!(got.match_provider.as_deref(), Some("tmdb"));
        assert_eq!(got.match_external_id.as_deref(), Some("438631"));
        assert_eq!(got.metadata_refreshed_at, Some(NOW));
        // The immediate re-enrich actually ran and merged the chosen
        // record's metadata.
        assert_eq!(got.metadata.overview.as_deref(), Some("A duke's son..."));
        assert_eq!(got.metadata.provider_ids.tmdb.as_deref(), Some("438631"));
    }

    #[tokio::test]
    async fn enrich_one_skips_match_write_when_manual_lands_mid_flight() {
        // FR1 — TOCTOU: `run` snapshots eligible items, then `enrich_one` does
        // seconds of network I/O before writing match-state keyed only by id.
        // Simulate a concurrent POST /RemoteSearch/Apply landing during that
        // window (the row is now "manual" with its own id) and assert the
        // sweep's trailing write does NOT revert the user's override.
        let s = store().await;
        put_movie(&s, 900_300, "Dune (2021)").await;
        s.set_item_match(900_300, "tmdb", "999", "manual", None, 1)
            .await
            .unwrap();
        let (_td, cache) = cache();
        let tmdb = FakeEnricher::tmdb()
            .with_search(vec![("438631", "Dune", Some(2021))])
            .with_detail(enriched_overview("A duke's son..."));

        let item = s.get(900_300).await.unwrap();
        enrich_one(
            &s,
            &sem(4),
            &cache,
            Some(&tmdb),
            None::<&FakeEnricher>,
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap();

        let got = s.get(900_300).await.unwrap();
        assert_eq!(got.match_source.as_deref(), Some("manual"));
        assert_eq!(got.match_provider.as_deref(), Some("tmdb"));
        assert_eq!(got.match_external_id.as_deref(), Some("999"));
    }

    #[tokio::test]
    async fn enrich_one_preserves_local_artwork_but_adds_new_roles() {
        // FR2 — a curated local sidecar (e.g. hand-placed poster.jpg → Primary)
        // must survive an enrichment pass; a role with no local row is still
        // filled from the provider.
        let s = store().await;
        put_movie(&s, 900_301, "Dune (2021)").await;
        s.set_artwork(900_301, "Primary", "local", "/curated/poster.jpg")
            .await
            .unwrap();
        let (_td, cache) = cache();
        let tmdb = FakeEnricher::tmdb()
            .with_search(vec![("438631", "Dune", Some(2021))])
            .with_detail(EnrichedMetadata {
                artwork: vec![
                    RemoteArt {
                        role: ArtworkRole::Primary,
                        url: "https://image.tmdb.org/t/p/original/p.jpg".into(),
                    },
                    RemoteArt {
                        role: ArtworkRole::Backdrop,
                        url: "https://image.tmdb.org/t/p/original/b.jpg".into(),
                    },
                ],
                ..EnrichedMetadata::default()
            })
            .with_image_bytes(vec![0xFF, 0xD8, 0xFF]);

        let item = s.get(900_301).await.unwrap();
        enrich_one(
            &s,
            &sem(4),
            &cache,
            Some(&tmdb),
            None::<&FakeEnricher>,
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap();

        let art = s.artwork_for(900_301).await.unwrap();
        let primary = art
            .iter()
            .find(|(role, _, _)| role == "Primary")
            .expect("primary row present");
        assert_eq!(primary.1, "local");
        assert_eq!(primary.2, "/curated/poster.jpg");
        let backdrop = art.iter().find(|(role, _, _)| role == "Backdrop");
        assert!(
            backdrop.is_some(),
            "backdrop should still be added: {art:?}"
        );
        assert_eq!(backdrop.unwrap().1, "tmdb");
    }

    #[tokio::test]
    async fn apply_manual_match_persists_even_without_an_enricher() {
        // No provider key configured (mirrors the apply handler's "still set
        // the manual match" behaviour when [tmdb]/[tvdb] api_key is absent —
        // a user's stated identity is honoured even when pharos can't
        // currently fetch it).
        let s = store().await;
        put_movie(&s, 900_201, "Whatever").await;
        let (_td, cache) = cache();

        apply_manual_match(
            &s,
            &sem(4),
            Some(&cache),
            None::<&FakeEnricher>,
            None::<&FakeEnricher>,
            &MetadataConfig::default(),
            900_201,
            "tmdb",
            "999",
            NOW,
        )
        .await
        .unwrap();

        let got = s.get(900_201).await.unwrap();
        assert_eq!(got.match_source.as_deref(), Some("manual"));
        assert_eq!(got.match_external_id.as_deref(), Some("999"));
    }

    // --- T9-series: series-container enrichment pass -----------------------

    async fn put_episode(store: &SqliteStore, id: u64, series: &str, folder: &str) {
        let item = MediaItem {
            id,
            path: format!("{folder}/S01E{id:02}.mkv").into(),
            title: format!("{series} ep {id}"),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: series.into(),
                season_number: Some(1),
                episode_number: Some(id as u32),
                series_folder: Some(folder.into()),
                series_year: Some(1997),
            }),
            ..MediaItem::default()
        };
        store.put(item).await.unwrap();
    }

    /// B125 — TVDB is not the only show provider. 44 of the deployed library's
    /// 178 shows were frozen as `none` by TVDB alone, nearly all anime, and a
    /// show with no poster falls back to a representative episode and then to
    /// an extracted frame.
    #[tokio::test]
    async fn a_show_tvdb_cannot_match_falls_back_to_tmdb() {
        let s = store().await;
        put_episode(&s, 1, "Death Note", "/tv/Death Note").await;
        put_episode(&s, 2, "Death Note", "/tv/Death Note").await;
        let (_td, cache) = cache();
        // TVDB searches and offers nothing that clears the floor.
        let tvdb = FakeEnricher::tvdb().with_search(vec![]);
        let tmdb = FakeEnricher::tmdb()
            // `put_episode` stamps every fixture show with year 1997, and a
            // 9-year gap scores 0.6 — below the floor. A provider result with
            // no year (common on TMDB search rows) is the realistic shape here.
            .with_search(vec![("13916", "Death Note", None)])
            .with_detail(EnrichedMetadata {
                overview: Some("A shinigami's notebook.".into()),
                artwork: vec![RemoteArt {
                    role: pharos_core::ArtworkRole::Primary,
                    url: "https://image.tmdb.org/poster.jpg".into(),
                }],
                ..EnrichedMetadata::default()
            })
            .with_image_bytes(b"\xff\xd8\xff\xe0jpegbytes".to_vec());

        run(
            &s,
            &sem(4),
            &cache,
            Some(&tmdb),
            Some(&tvdb),
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();

        let got = s
            .series_metadata_by_keys(&["/tv/Death Note".into()])
            .await
            .unwrap();
        let meta = got.get("/tv/Death Note").expect("show row written");
        assert_eq!(
            meta.match_source.as_deref(),
            Some("search"),
            "the fallback matched it, so it must not be frozen as `none`: {meta:?}"
        );
        assert_eq!(meta.match_provider.as_deref(), Some("tmdb"));
        assert_eq!(meta.match_external_id.as_deref(), Some("13916"));
        assert_eq!(
            meta.provider_ids.tmdb.as_deref(),
            Some("13916"),
            "a TMDB-matched show carries a TMDB id, not a TVDB one"
        );
        assert!(meta.provider_ids.tvdb.is_none());
        assert!(
            meta.poster_locator.is_some(),
            "the fallback provider's poster is cached, which is the whole point"
        );
    }

    #[tokio::test]
    async fn series_pass_enriches_container_from_tvdb_and_caches_poster() {
        let s = store().await;
        put_episode(&s, 1, "Buffy", "/tv/Buffy (1997)").await;
        put_episode(&s, 2, "Buffy", "/tv/Buffy (1997)").await;
        let (_td, cache) = cache();
        // Candidate title matches the folder-derived series_name ("Buffy") so
        // match_best clears the confidence floor.
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![("70327", "Buffy", Some(1997))])
            .with_detail(EnrichedMetadata {
                overview: Some("One girl in all the world.".into()),
                community_rating: Some(8.7),
                premiere_date: Some(867_715_200),
                official_rating: Some("TV-14".into()),
                genres: vec!["Drama".into(), "Fantasy".into()],
                also_tmdb_id: Some("95".into()),
                artwork: vec![RemoteArt {
                    role: pharos_core::ArtworkRole::Primary,
                    url: "https://artworks.thetvdb.com/poster.jpg".into(),
                }],
                ..EnrichedMetadata::default()
            })
            .with_image_bytes(b"\xff\xd8\xff\xe0jpegbytes".to_vec());

        let n = run(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        // 2 episodes (item loop) + 1 show (series pass) all match the same
        // candidate and enrich in one pass.
        assert_eq!(
            n.enriched, 3,
            "two episodes plus the show container enriched"
        );

        let got = s
            .series_metadata_by_keys(&["/tv/Buffy (1997)".into()])
            .await
            .unwrap();
        let m = got.get("/tv/Buffy (1997)").expect("series row written");
        assert_eq!(m.match_provider.as_deref(), Some("tvdb"));
        assert_eq!(m.match_source.as_deref(), Some("search"));
        assert_eq!(m.match_external_id.as_deref(), Some("70327"));
        assert_eq!(m.overview.as_deref(), Some("One girl in all the world."));
        assert_eq!(m.community_rating, Some(8.7));
        assert_eq!(m.genres, vec!["Drama".to_string(), "Fantasy".to_string()]);
        assert_eq!(m.provider_ids.tvdb.as_deref(), Some("70327"));
        assert_eq!(m.provider_ids.tmdb.as_deref(), Some("95"));
        assert_eq!(m.metadata_refreshed_at, Some(NOW));
        // The poster was downloaded + cached, and the locator points at a real
        // file on disk that the images route can serve.
        let locator = m.poster_locator.as_deref().expect("poster cached");
        assert!(
            std::path::Path::new(locator).exists(),
            "poster locator must point at a written cache file: {locator}"
        );

        // A second pass with the show already fresh does nothing (TTL not due).
        let n2 = run(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(
            n2.enriched, 0,
            "already-enriched show is not re-processed within the TTL"
        );
    }

    #[tokio::test]
    async fn series_pass_no_confident_hit_marks_none() {
        let s = store().await;
        put_episode(&s, 1, "Obscure Show", "/tv/Obscure Show").await;
        let (_td, cache) = cache();
        // Search returns a candidate that won't clear the confidence floor
        // against the query title.
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![("1", "Something Totally Different", None)])
            .with_detail(enriched_overview("should not be used"));

        let n = run(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n.enriched, 0, "no confident hit → nothing counted");

        let got = s
            .series_metadata_by_keys(&["/tv/Obscure Show".into()])
            .await
            .unwrap();
        let m = got.get("/tv/Obscure Show").expect("none-row written");
        assert_eq!(m.match_source.as_deref(), Some("none"));
        assert_eq!(m.overview, None, "no metadata applied on a none-match");

        // The `none` write means the show isn't re-searched until the TTL.
        let need = s.series_needing_match(10, NOW - 1).await.unwrap();
        assert!(
            need.is_empty(),
            "a none-matched show stays out of the eligible set until the TTL cutoff"
        );
    }
}
