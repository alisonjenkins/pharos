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
use crate::episode_fit::{self, Candidate, LocalEpisode};
use crate::metadata_language::{record_translation, TranslationOutcome, DEFAULT_LANGUAGE};
use crate::musicbrainz::{AlbumArtResolver, MusicBrainzClient};
use crate::online_enrich::{
    apply_enrichment, download_and_cache_art, EnrichedMetadata, EpisodeListing, OnlineEnricher,
    RemoteArt, TextsFetch,
};
use crate::series_identity::{
    log_series_match, record_episode_rejected, record_series_match, EpisodeRejectReason,
    SeriesMatchOutcome,
};
use crate::state::Stores;
use crate::tmdb::TmdbEnricher;
use crate::tvdb::{ReqwestTransport, TvdbEnricher};
use pharos_cache::image_cache::ImageRole;
use pharos_cache::ImageCache;
use pharos_core::{
    match_decide, ArtworkRole, DomainResult, GenreStore, LocalizedText, MatchDecision,
    MatchOutcome, MediaItem, MediaKind, MediaStore, MetadataTranslationStore, PersonStore,
    PreferenceStore, ProviderIds, SeriesMatchCandidate, SeriesMetadata, SeriesMetadataStore,
    TranslationGap, TranslationOwner,
};
use pharos_scanner::metadata::nfo::ShowNfoEvidence;
use pharos_scanner::FilenameProvider;
use std::collections::HashMap;

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
    artwork_epoch: Arc<std::sync::atomic::AtomicU64>,
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
                    // B155 — a pass that enriched anything may have recorded
                    // new artwork, which changes which track should represent
                    // an album/artist tile. Bump the epoch so the synth-image
                    // memo (built before the cover existed) is dropped instead
                    // of pinning a coverless representative until restart.
                    if s.enriched > 0 {
                        artwork_epoch.fetch_add(1, Ordering::Relaxed);
                    }
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
        + MetadataTranslationStore
        + PreferenceStore
        + pharos_core::UserStore
        + pharos_core::SeriesIdentityStore
        + pharos_store_sqlx::ServerConfigStore,
{
    // No provider configured → nothing to do (mirrors spawn's key gate).
    if tmdb.is_none() && tvdb.is_none() && musicbrainz.is_none() {
        return Ok(PassStats {
            enriched: 0,
            had_work: false,
        });
    }
    if tvdb.is_some() || tmdb.is_some() {
        // Non-fatal: a config store that cannot be read or written must not
        // stop enrichment. The next pass tries again.
        if let Err(e) = readmit_stale_tv_matches(store).await {
            tracing::warn!(error = %e, "T9 metadata backfill: show matcher version check failed");
        }
    }
    let mut series_enriched = 0usize;
    let mut series_work = false;
    // Series-container pass (the series-level record), first: an episode
    // takes its record from its show's match (V892), so a show matched here
    // lets its episodes enrich in the item pass below.
    if tvdb.is_some() || tmdb.is_some() {
        match enrich_series_pass(store, bg_io, cache, tvdb, tmdb, cfg, now).await {
            Ok((n, series_had_work)) => {
                if n > 0 {
                    tracing::info!(enriched = n, "T9-series metadata backfill: shows enriched");
                }
                series_enriched = n;
                series_work = series_had_work;
            }
            Err(e) => tracing::warn!(error = %e, "T9-series metadata backfill: pass aborted"),
        }
    }

    // Items whose last enrichment predates this cutoff (or never matched) are
    // eligible; `manual` rows are excluded by the query itself.
    let ttl_cutoff = now.saturating_sub(i64::from(cfg.refresh_ttl_days) * 86_400);
    let items = store
        .items_needing_match(cfg.max_per_pass, ttl_cutoff)
        .await?;
    let mut enriched = series_enriched;
    // Any eligible row (even one that resolves to `none`/transient) is work:
    // processing it advances the backlog, so the loop should keep draining.
    let mut had_work = series_work || !items.is_empty();
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

    // B564 — text of shows and episodes matched before English was asked for,
    // and of every language a user has picked since.
    if let (Some(tv), true) = (tvdb, cfg.fetch_translations) {
        match translation_pass(store, bg_io, tv, now).await {
            Ok(stored) => had_work |= stored > 0,
            Err(e) => tracing::warn!(error = %e, "metadata translation pass aborted"),
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
    // Bounded metric label; `MediaKind::as_str` is the same table, but this one
    // is a DASHBOARD CONTRACT and is kept separate deliberately so a change to
    // the internal token cannot silently rename a label.
    match kind {
        MediaKind::Movie => "movie",
        MediaKind::Episode => "episode",
        MediaKind::Audio => "audio",
        MediaKind::Book => "book",
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
/// The string a provider search is built from.
///
/// A local item's FILENAME is the better key: it carries the release title and
/// year that the display title has usually had stripped. A remote item's path is
/// synthetic (`ytdlp://<extractor>/<id>` — 008), so its last component is a
/// site's opaque video id — searching a provider for that would match something
/// at random and then persist the match. Its title is what the resolver read off
/// the source, which is the real name.
fn search_stem(item: &MediaItem) -> &str {
    match item.origin().local() {
        Some(p) => p
            .as_path()
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(item.title.as_str()),
        None => item.title.as_str(),
    }
}

/// An episode's own name, from its filename ("S01E03 - The Naked Now"): the
/// cross-check that places it in a provider that numbers its season
/// differently (B570). Not its stored title, which an NFO written in the
/// provider's numbering may have set to the wrong episode's name.
fn episode_name(item: &MediaItem) -> Option<String> {
    if item.kind != MediaKind::Episode {
        return None;
    }
    pharos_scanner::metadata::filename::episode_title(search_stem(item))
}

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
    S: MediaStore + GenreStore + PersonStore + MetadataTranslationStore + SeriesMetadataStore,
{
    let stem = search_stem(&item);
    let name = episode_name(&item);

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
        // 004-books joins audio here: an online book provider is out of scope,
        // and querying TMDB or TVDB for a book would return a confident wrong
        // match — the class that produced B150/B152/B159/B160 for album art.
        // `item.kind` rather than a literal, so the metric label names the kind
        // that was actually skipped.
        MediaKind::Audio | MediaKind::Book => {
            record_enrich(item.kind, EnrichOutcome::Skipped);
            return Ok(false);
        }
    };

    // Provider by kind: Episode prefers TVDB (fallback TMDB when TVDB isn't
    // configured); Movie is TMDB. When the kind's providers are all absent,
    // skip the item (leaves it eligible for a later pass with a key present).
    // A cleared id must reach the store on every path, not only a Hit's
    // `put`: a NoMatch stamps `none`, after which the old search id would
    // pass `trusted_provider_ids` and be fetched as if an NFO named it.
    let trusted = trusted_provider_ids(&item);
    let ids_cleared = trusted != item.metadata.provider_ids;
    item.metadata.provider_ids = trusted;
    let provider_ids = item.metadata.provider_ids.clone();
    let mut nfo_rejected: Option<NfoRejection> = None;
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
                    name.as_deref(),
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
        // V892 — an episode takes its record only from its show's pinned
        // match. Its own manual match (Identify) is the one exception.
        MediaKind::Episode if item.match_source.as_deref() != Some("manual") => {
            let pinned = pinned_show(store, &item).await?;
            let target = pinned.as_ref().and_then(|p| {
                let provider = match p.match_provider.as_deref()? {
                    "tvdb" => "tvdb",
                    "tmdb" => "tmdb",
                    _ => return None,
                };
                matches!(p.match_source.as_deref(), Some("search" | "manual"))
                    .then_some((provider, p.match_external_id.as_deref()?))
            });
            match target {
                // The show is matched to nothing: so is the episode, until the
                // show's match changes.
                None => {
                    let provider = match pinned.as_ref().and_then(|p| p.match_provider.as_deref()) {
                        Some("tmdb") => "tmdb",
                        _ => "tvdb",
                    };
                    (provider, Resolved::NoMatch)
                }
                Some((provider, show_id)) => {
                    let at = PinnedFetch {
                        show_id,
                        confidence: pinned.as_ref().and_then(|p| p.match_confidence),
                        season,
                        episode,
                        name: name.as_deref(),
                    };
                    let (resolved, rejected) = match (provider, tvdb, tmdb) {
                        ("tvdb", Some(t), _) => {
                            resolve_pinned(t, &at, provider_ids.tvdb.as_deref(), bg_io).await
                        }
                        ("tmdb", _, Some(t)) => {
                            resolve_pinned(t, &at, provider_ids.tmdb.as_deref(), bg_io).await
                        }
                        _ => {
                            record_enrich(item.kind, EnrichOutcome::Skipped);
                            return Ok(false);
                        }
                    };
                    nfo_rejected = rejected;
                    (provider, resolved)
                }
            }
        }
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
                        name.as_deref(),
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
                        name.as_deref(),
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
        // 004-books joins audio here: an online book provider is out of scope,
        // and querying TMDB or TVDB for a book would return a confident wrong
        // match — the class that produced B150/B152/B159/B160 for album art.
        // `item.kind` rather than a literal, so the metric label names the kind
        // that was actually skipped.
        MediaKind::Audio | MediaKind::Book => {
            record_enrich(item.kind, EnrichOutcome::Skipped);
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
                if ids_cleared {
                    store.put(item.clone()).await?;
                }
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
                if ids_cleared {
                    store.put(item.clone()).await?;
                }
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
    if let Some(rejected) = &nfo_rejected {
        set_aside_nfo(&mut item, rejected, matched_provider);
    } else if item.kind == MediaKind::Episode && source == "search" {
        observe_title_fit(&item, &enriched, matched_provider, &external_id);
    }

    // Fold the record onto the item (local data always wins — apply_enrichment
    // only fills gaps), then stamp the matched provider id if we hadn't one.
    let counts = store.item_entity_counts(item.id).await?;
    let moved = nfo_rejected.is_some()
        || reidentified(&item, &enriched)
        || nfo_names_another(&item, &enriched);
    if moved {
        adopt_reidentified(&mut item, &enriched);
    }
    let applied = apply_enrichment(&mut item, counts, &enriched);
    // B564 — a refresh of an item stored before English was asked for still
    // holds the base-record text, which fill-if-empty would keep.
    // `fetch_translations` is the kill switch for this rewrite too.
    if cfg.fetch_translations
        && adopt_default_text(
            Some(&mut item.title),
            &mut item.metadata.overview,
            &enriched.texts,
        )
    {
        log_adopted(&TranslationOwner::Item(item.id), "enrichment");
    }
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
    store_texts(
        store,
        &TranslationOwner::Item(item.id),
        &enriched.texts,
        now,
    )
    .await;

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
                tvdb_e
                    .fetch(item.kind, &external_id, None, None, None)
                    .await
            }
            .and_then(|m| m.also_tmdb_id);
            if let Some(tid) = series_tmdb {
                let tmdb_meta = {
                    let _permit = BgPermit::acquire(bg_io).await;
                    tmdb_e.fetch(item.kind, &tid, None, None, None).await
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
    // B570 — a re-identified episode's `<stem>-thumb` was saved by the same
    // scrape as its NFO, so it is the wrong episode's still: the record's
    // still replaces it as the Thumb, which outranks the show-folder Primary
    // the season tiles rely on (and which stays).
    if moved {
        match enriched
            .artwork
            .iter()
            .find(|a| a.role == ArtworkRole::Primary)
        {
            Some(still) => chosen.push((
                matched_provider,
                RemoteArt {
                    role: ArtworkRole::Thumb,
                    url: still.url.clone(),
                },
            )),
            None if local_roles.contains("thumb") => tracing::info!(
                media.id = item.id,
                provider = matched_provider,
                external_id = %external_id,
                "T9 metadata backfill: re-identified episode's record has no still; its scraped Thumb stays"
            ),
            None => {}
        }
    }

    // How many art roles this item ends up covered by — newly cached here, or
    // already held by a local sidecar. Zero on a Hit is the #113 signal: the
    // record resolved but left the item with no image.
    let mut art_covered = 0usize;
    for (prov, art) in &chosen {
        if !CACHED_ART_ROLES.contains(&art.role) {
            tracing::debug!(role = ?art.role, item = item.id, "T9 metadata backfill: skipping art role (not cached)");
            continue;
        }
        let replaces_scrape = moved && art.role == ArtworkRole::Thumb;
        if !replaces_scrape && local_roles.contains(&art.role.as_str().to_ascii_lowercase()) {
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

/// The show an episode's folder (or split group) is matched to, if any.
async fn pinned_show<S: SeriesMetadataStore>(
    store: &S,
    item: &MediaItem,
) -> DomainResult<Option<SeriesMetadata>> {
    let Some(series) = item.series.as_ref() else {
        return Ok(None);
    };
    let key = series.series_key().to_string();
    Ok(store
        .series_metadata_by_keys(std::slice::from_ref(&key))
        .await?
        .remove(&key))
}

/// Where in its pinned show an episode is looked up.
struct PinnedFetch<'a> {
    show_id: &'a str,
    confidence: Option<f32>,
    season: Option<u32>,
    episode: Option<u32>,
    name: Option<&'a str>,
}

/// An episode NFO id whose record belongs to another show (V892).
struct NfoRejection {
    nfo_id: String,
    record_series: Option<String>,
    pinned_series: String,
}

/// V892 — resolve an episode inside its show's pinned match. An NFO id for
/// the pinned provider is used only when its record says it is an episode
/// of the pinned show; otherwise the record is set aside (returned as the
/// rejection) and the episode is looked up by number in the pinned show.
async fn resolve_pinned<E: OnlineEnricher>(
    enricher: &E,
    at: &PinnedFetch<'_>,
    nfo_id: Option<&str>,
    bg_io: &Arc<Semaphore>,
) -> (Resolved, Option<NfoRejection>) {
    let mut rejected = None;
    if let Some(nfo_id) = nfo_id {
        let record = {
            let _permit = BgPermit::acquire(bg_io).await;
            enricher
                .fetch(MediaKind::Episode, nfo_id, at.season, at.episode, at.name)
                .await
        };
        match record {
            Some(e) if e.series_id.as_deref() == Some(at.show_id) => {
                return (
                    Resolved::Hit {
                        external_id: nfo_id.to_string(),
                        source: "nfo_id",
                        confidence: None,
                        enriched: Box::new(e),
                    },
                    None,
                );
            }
            Some(e) => {
                rejected = Some(NfoRejection {
                    nfo_id: nfo_id.to_string(),
                    record_series: e.series_id,
                    pinned_series: at.show_id.to_string(),
                });
            }
            // No record for the id: nothing to compare, so the number decides.
            None => {}
        }
    }
    let record = {
        let _permit = BgPermit::acquire(bg_io).await;
        enricher
            .fetch(
                MediaKind::Episode,
                at.show_id,
                at.season,
                at.episode,
                at.name,
            )
            .await
    };
    let resolved = match record {
        Some(e) => Resolved::Hit {
            external_id: at.show_id.to_string(),
            source: "search",
            confidence: at.confidence,
            enriched: Box::new(e),
        },
        None => Resolved::Transient {
            external_id: at.show_id.to_string(),
            source: "search",
            confidence: at.confidence,
        },
    };
    (resolved, rejected)
}

/// V892 — drop what a rejected episode NFO put on `item`: its provider ids
/// (another show's) and, through the caller's re-identified path, its text
/// and still.
fn set_aside_nfo(item: &mut MediaItem, rejected: &NfoRejection, provider: &str) {
    record_episode_rejected(EpisodeRejectReason::NfoOtherSeries);
    tracing::info!(
        media.id = item.id,
        path = %item.path.display(),
        provider,
        nfo_id = %rejected.nfo_id,
        record_series_id = rejected.record_series.as_deref().unwrap_or_default(),
        pinned_series_id = %rejected.pinned_series,
        reason = EpisodeRejectReason::NfoOtherSeries.label(),
        "episode NFO names another series than its show's match; set aside"
    );
    item.metadata.provider_ids = ProviderIds::default();
    item.metadata.community_rating = None;
}

/// V892 — an episode placed in its pinned show by number whose file names a
/// different episode than the record there, and none of its neighbours.
/// Observation only: the number still decides.
fn observe_title_fit(item: &MediaItem, enriched: &EnrichedMetadata, provider: &str, show_id: &str) {
    let (Some(name), Some(record)) = (episode_name(item), enriched.title.as_deref()) else {
        return;
    };
    let fits = |t: &str| pharos_core::title_similarity(&name, t) >= SAME_SHOW_EPISODE_TITLE;
    if fits(record) || enriched.neighbour_titles.iter().any(|t| fits(t)) {
        return;
    }
    record_episode_rejected(EpisodeRejectReason::TitleMismatch);
    tracing::info!(
        media.id = item.id,
        path = %item.path.display(),
        provider,
        pinned_series_id = show_id,
        file_title = %name,
        record_title = record,
        reason = EpisodeRejectReason::TitleMismatch.label(),
        "episode's file title is not its record's in the pinned show"
    );
}

/// Lowest [`pharos_core::title_similarity`] at which a file's episode title
/// is the record's (spec 033 §2's bar).
const SAME_SHOW_EPISODE_TITLE: f32 = 0.8;

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

/// B570 — whether the provider placed episode `item` on a record numbered
/// differently from its file, by the episode's own name.
fn reidentified(item: &MediaItem, enriched: &EnrichedMetadata) -> bool {
    let file = item
        .series
        .as_ref()
        .and_then(|s| Some((s.season_number?, s.episode_number?)));
    matches!((file, enriched.episode_number), (Some(f), Some(r)) if f != r)
}

/// Lowest [`pharos_core::title_similarity`] at which two episode names are
/// the same episode's (the bar `tvdb::pick_episode` places a record by).
const SAME_EPISODE_NAME: f32 = 0.9;

/// B588 — whether the stored (NFO) title names a NEIGHBOUR of the record
/// the filename's episode name matches. Jellyfin wrote some NFOs in an order
/// shifted from both the files and the provider's, so the record and the
/// file agree (whatever number the record is at) and only the NFO's text and
/// still are another episode's. A stored title that is this episode's name
/// in another language matches no neighbour, so it stays.
fn nfo_names_another(item: &MediaItem, enriched: &EnrichedMetadata) -> bool {
    let (Some(name), Some(record)) = (episode_name(item), enriched.title.as_deref()) else {
        return false;
    };
    let same = |a: &str, b: &str| pharos_core::title_similarity(a, b) >= SAME_EPISODE_NAME;
    let another = same(&name, record)
        && !same(&item.title, record)
        && enriched
            .neighbour_titles
            .iter()
            .any(|n| same(&item.title, n));
    if another {
        tracing::info!(
            media.id = item.id,
            file_name = %name,
            stored_title = %item.title,
            record_title = record,
            "T9 metadata backfill: the record matches the filename's episode, the NFO names another"
        );
    }
    another
}

/// B570 — take a re-identified episode's text from its record. Local data
/// normally wins, but this item's NFO was written in the provider's numbering
/// for the episode next to it, so its title and plot are another episode's.
fn adopt_reidentified(item: &mut MediaItem, enriched: &EnrichedMetadata) {
    tracing::info!(
        media.id = item.id,
        stored_title = %item.title,
        record_title = enriched.title.as_deref().unwrap_or(""),
        record_number = ?enriched.episode_number,
        "T9 metadata backfill: episode re-identified by its name; its NFO text described another episode"
    );
    if let Some(title) = enriched
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        item.title = title.to_string();
    }
    // Replaced even when the record has none: the NFO's plot and air date
    // are the neighbouring episode's, which is worse than none.
    item.metadata.overview = enriched.overview.clone();
    item.metadata.premiere_date = enriched.premiere_date;
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
    S: MediaStore + GenreStore + PersonStore + MetadataTranslationStore + SeriesMetadataStore,
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

/// `item`'s provider ids without the one an earlier search stamped. That id
/// is only the old matcher's answer; fed back to [`resolve`] it would skip the
/// search, keep the show a re-match exists to replace, and be recorded as an
/// NFO id that no later re-admission reaches. An id that differs from the
/// stamped match came from local data and is kept. Only an episode's: the
/// re-admission ([`TV_MATCH_VERSION`]) is for shows, so a movie's TTL
/// refresh keeps fetching the record it matched instead of searching again.
fn trusted_provider_ids(item: &MediaItem) -> ProviderIds {
    let mut ids = item.metadata.provider_ids.clone();
    if item.kind != MediaKind::Episode || item.match_source.as_deref() != Some("search") {
        return ids;
    }
    let stamped = item.match_external_id.as_deref();
    let slot = match item.match_provider.as_deref() {
        Some("tmdb") => &mut ids.tmdb,
        Some("tvdb") => &mut ids.tvdb,
        _ => return ids,
    };
    if slot.as_deref() == stamped {
        *slot = None;
    }
    ids
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
    name: Option<&str>,
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
        match search_decide(enricher, kind, title, year, bg_io, cfg)
            .await
            .chosen
        {
            Some(o) => (o.id, "search", Some(o.confidence)),
            None => return Resolved::NoMatch,
        }
    };
    let enriched = {
        let _permit = BgPermit::acquire(bg_io).await;
        enricher
            .fetch(kind, &external_id, season, episode, name)
            .await
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

/// Search `enricher` and decide among its candidates with
/// [`pharos_core::match_decide`].
async fn search_decide<E: OnlineEnricher>(
    enricher: &E,
    kind: MediaKind,
    title: &str,
    year: Option<u32>,
    bg_io: &Arc<Semaphore>,
    cfg: &MetadataConfig,
) -> MatchDecision {
    let candidates = {
        let _permit = BgPermit::acquire(bg_io).await;
        enricher.search(kind, title, year).await
    };
    match_decide(title, year, &candidates, cfg.match_min_confidence)
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

/// Version of the show matcher ([`pharos_core::match_best`] over the TVDB and
/// TMDB series searches). Bump it when a change would choose a different show
/// for a search already answered, so the stored answers are re-asked.
///
/// 1 (B557): TVDB hits scored by their English translation and aliases, an
/// exact name ahead of a prefix match, a franchise name to the original.
pub(crate) const TV_MATCH_VERSION: u32 = 1;

/// Key under which the [`TV_MATCH_VERSION`] that produced the stored show and
/// episode search matches is recorded.
const TV_MATCH_VERSION_KEY: &str = "tv_match_version";

/// Re-admit every searched show and episode once the matcher that chose them
/// has changed. Unlike album art, an ABSENT record is stale too: every search
/// match made before this key existed came from the matcher B557 replaced,
/// which handed 29 of the deployed library's shows to a fan work, an abridged
/// parody or the wrong entry of a franchise. A fresh install has no searched
/// rows, so re-admitting there costs nothing. NFO and manual matches are not
/// touched. One config read per pass once the versions agree.
async fn readmit_stale_tv_matches<S>(store: &S) -> DomainResult<()>
where
    S: SeriesMetadataStore + pharos_store_sqlx::ServerConfigStore,
{
    let want = TV_MATCH_VERSION.to_string();
    let have = store
        .load_named_config(TV_MATCH_VERSION_KEY)
        .await
        .map_err(|e| pharos_core::DomainError::Backend(e.to_string()))?;
    if have.as_deref() == Some(want.as_str()) {
        return Ok(());
    }
    let readmitted = store.readmit_searched_tv().await?;
    tracing::info!(
        readmitted,
        from = have.as_deref().unwrap_or("none"),
        to = want.as_str(),
        "T9 metadata backfill: show matcher version changed, re-matching searched shows and episodes"
    );
    store
        .set_named_config(TV_MATCH_VERSION_KEY, &want)
        .await
        .map_err(|e| pharos_core::DomainError::Backend(e.to_string()))?;
    Ok(())
}

/// Key under which the album-art matcher version that produced the CURRENT
/// artwork is recorded.
const ALBUM_ART_VERSION_KEY: &str = "musicbrainz_album_art_version";
/// B160 — the version whose change DOES drop cached covers. See
/// [`crate::musicbrainz::ALBUM_MATCH_VERSION`].
const ALBUM_MATCH_VERSION_KEY: &str = "musicbrainz_album_match_version";

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
    // B160 — clearing is keyed on the MATCH version, not the query version.
    // Re-admitting misses needs no clear at all: the miss marker carries the
    // query version and `audio_items_needing_art` re-admits any row stamped by
    // an older one, leaving covered tracks alone. Clearing is only correct when
    // past matches may be WRONG, which is a strictly rarer event.
    let want = crate::musicbrainz::ALBUM_MATCH_VERSION.to_string();
    let have = store
        .load_named_config(ALBUM_MATCH_VERSION_KEY)
        .await
        .map_err(|e| pharos_core::DomainError::Backend(e.to_string()))?;
    // An ABSENT record is not a stale one — a fresh install (and the first boot
    // after this split) has nothing to invalidate. Adopt the version silently;
    // only a recorded value that DIFFERS justifies dropping covers.
    let stale = matches!(have.as_deref(), Some(h) if h != want);
    if stale {
        let cleared = store.clear_provider_artwork("musicbrainz").await?;
        tracing::info!(
            cleared_items = cleared,
            from = have.as_deref().unwrap_or("none"),
            to = want.as_str(),
            "musicbrainz album-art: matcher version changed, dropping its covers for a re-match"
        );
    }
    if have.as_deref() != Some(want.as_str()) {
        store
            .set_named_config(ALBUM_MATCH_VERSION_KEY, &want)
            .await
            .map_err(|e| pharos_core::DomainError::Backend(e.to_string()))?;
    }
    // Record the query version too. Nothing is cleared for it — it exists so
    // the miss marker's shape is greppable next to the stored value.
    let q = crate::musicbrainz::ALBUM_ART_QUERY_VERSION.to_string();
    let have_q = store
        .load_named_config(ALBUM_ART_VERSION_KEY)
        .await
        .map_err(|e| pharos_core::DomainError::Backend(e.to_string()))?;
    if have_q.as_deref() != Some(q.as_str()) {
        tracing::info!(
            from = have_q.as_deref().unwrap_or("none"),
            to = q.as_str(),
            "musicbrainz album-art: query version changed, re-admitting older misses"
        );
        store
            .set_named_config(ALBUM_ART_VERSION_KEY, &q)
            .await
            .map_err(|e| pharos_core::DomainError::Backend(e.to_string()))?;
    }
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
    // B152 — a BLANK tag is an absent tag. Owl City's `Fireflies` carries
    // `album_artist = ""` alongside `artist = "Owl City"`; `Option::or` sees
    // `Some("")` and keeps it, so the good tag was shadowed by an empty one.
    // Downstream that empty string reads as "artist unknown", which the B150
    // consistency check deliberately lets pass — so the track went straight
    // back to matching any album called Fireflies.
    let non_blank = |s: &Option<String>| {
        s.as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
    };
    let artist = non_blank(&item.probe.album_artist).or_else(|| non_blank(&item.probe.artist));
    let artist = artist.as_deref();
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
    S: SeriesMetadataStore + MetadataTranslationStore + pharos_core::SeriesIdentityStore,
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
    let mut lists = EpisodeLists::new();
    for cand in candidates {
        // V6 — one bad show never aborts the pass; log it and carry on.
        let one = enrich_one_series(store, bg_io, cache, &providers, cfg, cand, now, &mut lists);
        match one.await {
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
#[allow(clippy::too_many_arguments)]
async fn enrich_one_series<Tv, Tm, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    providers: &SeriesProviders<'_, Tv, Tm>,
    cfg: &MetadataConfig,
    cand: SeriesMatchCandidate,
    now: i64,
    lists: &mut EpisodeLists,
) -> DomainResult<bool>
where
    Tv: OnlineEnricher,
    Tm: OnlineEnricher,
    S: SeriesMetadataStore + MetadataTranslationStore + pharos_core::SeriesIdentityStore,
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
    let mut transient = None;
    if let Some(tv) = providers.tvdb {
        match resolve_series(store, tv, "tvdb", bg_io, cache, cfg, &cand, lists).await {
            SeriesAttempt::Hit(h) => outcome = Some(h),
            SeriesAttempt::Transient(d) => transient = Some(d),
            SeriesAttempt::NoMatch => {}
        }
    }
    let mut fell_back = false;
    if outcome.is_none() {
        if let Some(tm) = providers.tmdb {
            match resolve_series(store, tm, "tmdb", bg_io, cache, cfg, &cand, lists).await {
                SeriesAttempt::Hit(h) => {
                    outcome = Some(h);
                    fell_back = true;
                }
                SeriesAttempt::Transient(d) => transient = Some(d),
                SeriesAttempt::NoMatch => {}
            }
        }
    }
    // One count per show, with the final outcome over every provider asked
    // (a TVDB miss then a TMDB hit is one `matched`).
    if let Some(hit) = &outcome {
        record_series_match(
            SeriesMatchOutcome::of(&hit.decision),
            hit.decision.decided_by,
        );
    } else if let Some(d) = &transient {
        record_series_match(SeriesMatchOutcome::Transient, d.decided_by);
        return Ok(false);
    } else {
        record_series_match(SeriesMatchOutcome::NoMatch, None);
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

    let flagged = hit.decision.ambiguous;
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
            // The upsert never writes the ratings columns.
            ratings: Default::default(),
            // V891 — every upsert writes the flag, so a clear decision on a
            // later pass clears it.
            match_flag: flagged.then(|| AMBIGUOUS_FLAG.to_string()),
            match_runner_up_id: flagged
                .then(|| hit.decision.runner_up.as_ref().map(|r| r.id.clone()))
                .flatten(),
            match_runner_up_score: flagged.then_some(hit.runner_up_score).flatten(),
        })
        .await?;
    store_texts(
        store,
        &TranslationOwner::Series(cand.series_key.clone()),
        &hit.enriched.texts,
        now,
    )
    .await;
    Ok(true)
}

/// Most shows and episodes one [`translation_pass`] asks the provider about.
/// Each costs one or two requests.
const TRANSLATIONS_PER_PASS: i64 = 500;

/// A language the provider did not have is asked about again after this long:
/// TVDB's volunteers add translations over time.
const MISSING_RETRY_SECS: i64 = 30 * 86_400;

/// A record that could not be read is asked about again after this long, so a
/// show TVDB cannot place does not hold the front of every pass.
const FAILED_RETRY_SECS: i64 = 86_400;

/// Consecutive unreadable records after which a pass stops: the provider is
/// down, and marking everything behind them failed would only delay them.
const MAX_CONSECUTIVE_FAILURES: usize = 3;

/// B564 / ADR-0062 — fill in the provider text missing for matched shows and
/// episodes: English, plus every language a user has picked. Shows and
/// episodes stored before English was asked for get their base-record
/// description replaced by the English one ([`adopt_default_text`]). Returns
/// how many owners got rows.
async fn translation_pass<Tv, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    tvdb: &Tv,
    now: i64,
) -> DomainResult<usize>
where
    Tv: OnlineEnricher,
    S: MediaStore
        + SeriesMetadataStore
        + MetadataTranslationStore
        + PreferenceStore
        + pharos_core::UserStore,
{
    let languages = wanted_languages(store).await;
    let cutoff = now.saturating_sub(MISSING_RETRY_SECS);
    let todo = collect_gaps(
        store,
        tvdb.provider(),
        &languages,
        cutoff,
        TRANSLATIONS_PER_PASS,
    )
    .await?;
    if todo.is_empty() {
        return Ok(0);
    }
    tracing::info!(
        owners = todo.len(),
        languages = ?languages,
        "metadata translation pass: fetching provider text"
    );
    let mut stored = 0usize;
    let mut failures = 0usize;
    for (gap, langs) in todo {
        // The episode's own name places it as enrichment did (B570).
        let name = match &gap.owner {
            TranslationOwner::Item(id) => match MediaStore::get(store, *id).await {
                Ok(item) => episode_name(&item),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        owner = ?gap.owner,
                        "metadata translation pass: could not read the episode; placing it by number"
                    );
                    None
                }
            },
            TranslationOwner::Series(_) => None,
        };
        let texts = {
            let _permit = BgPermit::acquire(bg_io).await;
            tvdb.fetch_texts(
                MediaKind::Episode,
                &gap.series_external_id,
                gap.season,
                gap.episode,
                name.as_deref(),
                &langs,
            )
            .await
        };
        tokio::time::sleep(REQUEST_SPACING).await;
        let misses = || -> Vec<LocalizedText> {
            langs
                .iter()
                .map(|l| LocalizedText {
                    language: l.clone(),
                    ..LocalizedText::default()
                })
                .collect()
        };
        let texts = match texts {
            TextsFetch::Found(texts) => texts,
            // The provider answered and has no such record: not an outage.
            // Every language is recorded missing now, so the record waits
            // MISSING_RETRY_SECS and the records behind it are reached.
            TextsFetch::NotFound => {
                for lang in &langs {
                    record_translation(tvdb.provider(), lang, TranslationOutcome::Missing);
                }
                tracing::info!(
                    owner = ?gap.owner,
                    provider = tvdb.provider(),
                    series_external_id = %gap.series_external_id,
                    season = gap.season,
                    episode = gap.episode,
                    languages = ?langs,
                    reason = "provider has no such record",
                    "tvdb text falls back to the default text"
                );
                failures = 0;
                if store_texts(store, &gap.owner, &misses(), now).await {
                    stored += 1;
                }
                continue;
            }
            TextsFetch::Failed => {
                for lang in &langs {
                    record_translation(tvdb.provider(), lang, TranslationOutcome::Error);
                }
                failures += 1;
                tracing::warn!(
                    owner = ?gap.owner,
                    provider = tvdb.provider(),
                    series_external_id = %gap.series_external_id,
                    season = gap.season,
                    episode = gap.episode,
                    languages = ?langs,
                    "metadata translation pass: record could not be read; text stays the default"
                );
                // Mark the languages missing, aged so they are asked again after
                // FAILED_RETRY_SECS rather than MISSING_RETRY_SECS.
                let aged = now.saturating_sub(MISSING_RETRY_SECS - FAILED_RETRY_SECS);
                store_texts(store, &gap.owner, &misses(), aged).await;
                // Stop only after marking this record, so a run of records the
                // provider cannot place moves the front of the gap list forward
                // instead of holding it.
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    tracing::warn!(
                        failures,
                        "metadata translation pass: stopping, the provider looks unavailable"
                    );
                    break;
                }
                continue;
            }
        };
        failures = 0;
        if let Err(e) = adopt_for_owner(store, &gap.owner, &texts).await {
            tracing::warn!(error = %e, owner = ?gap.owner, "metadata translation pass: could not replace the default text");
        }
        if store_texts(store, &gap.owner, &texts, now).await {
            stored += 1;
        }
    }
    Ok(stored)
}

/// The owners one translation pass asks for, each with the languages it
/// is missing: at most `budget` owners. One request per owner covers all
/// its missing languages.
///
/// Every language first gets an equal share of the budget, so a user's
/// language is fetched alongside the English backlog instead of waiting
/// for all of it to drain (English is a gap on every show stored before
/// it was asked for). Budget a language leaves unused then goes to the
/// languages that still have gaps, in order.
async fn collect_gaps<S>(
    store: &S,
    provider: &str,
    languages: &[String],
    cutoff: i64,
    budget: i64,
) -> DomainResult<Vec<(TranslationGap, Vec<String>)>>
where
    S: MetadataTranslationStore,
{
    let mut todo: Vec<(TranslationGap, Vec<String>)> = Vec::new();
    let mut index: std::collections::HashMap<TranslationOwner, usize> = Default::default();
    let count = i64::try_from(languages.len()).unwrap_or(i64::MAX).max(1);
    let share = (budget / count).max(1);
    // How many gaps each language has asked for so far; a later, larger
    // query returns those first again (the gap order is stable).
    let mut asked = vec![0i64; languages.len()];
    for round in 0..2 {
        for (n, lang) in languages.iter().enumerate() {
            let room = budget - i64::try_from(todo.len()).unwrap_or(i64::MAX);
            if room <= 0 {
                return Ok(todo);
            }
            let limit = if round == 0 {
                share.min(room)
            } else {
                asked[n] + room
            };
            if round == 1 && asked[n] < share {
                // Fewer gaps than its share: none left to ask for.
                continue;
            }
            let gaps = store
                .translation_gaps(provider, lang, cutoff, limit)
                .await?;
            asked[n] = i64::try_from(gaps.len()).unwrap_or(i64::MAX);
            for gap in gaps {
                match index.get(&gap.owner) {
                    Some(&i) => {
                        if !todo[i].1.contains(lang) {
                            todo[i].1.push(lang.clone());
                        }
                    }
                    None => {
                        if i64::try_from(todo.len()).unwrap_or(i64::MAX) >= budget {
                            continue;
                        }
                        index.insert(gap.owner.clone(), todo.len());
                        todo.push((gap, vec![lang.clone()]));
                    }
                }
            }
        }
    }
    Ok(todo)
}

/// English plus every language a user has picked for descriptions, in a
/// stable order. A user whose configuration cannot be read is skipped: the
/// worst case is a language filled in a pass later.
async fn wanted_languages<S>(store: &S) -> Vec<String>
where
    S: PreferenceStore + pharos_core::UserStore,
{
    use crate::metadata_language::DescriptionLanguage;
    let mut out = std::collections::BTreeSet::new();
    match store.list().await {
        Ok(users) => {
            for u in users {
                let Ok(Some(raw)) = store.get_user_configuration(u.id).await else {
                    continue;
                };
                let Ok(cfg) =
                    serde_json::from_str::<pharos_jellyfin_api::dto::UserConfigurationDto>(&raw)
                else {
                    continue;
                };
                if let DescriptionLanguage::Code(c) =
                    DescriptionLanguage::parse(&cfg.description_language_preference)
                {
                    out.insert(c);
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "metadata translation pass: could not list users"),
    }
    let mut langs = vec![DEFAULT_LANGUAGE.to_string()];
    langs.extend(out.into_iter().filter(|l| l != DEFAULT_LANGUAGE));
    langs
}

/// Store `texts` for `owner`, logging rather than failing: the rows are a
/// cache of provider text, and a failed write is retried by the next pass.
/// `true` when there was something to store and it was stored.
async fn store_texts<S: MetadataTranslationStore>(
    store: &S,
    owner: &TranslationOwner,
    texts: &[LocalizedText],
    now: i64,
) -> bool {
    if texts.is_empty() {
        return false;
    }
    match store.upsert_translations(owner, texts, now).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, owner = ?owner, "could not store provider text per language");
            false
        }
    }
}

/// [`adopt_default_text`] for a stored show or episode, written back when
/// it changed anything.
async fn adopt_for_owner<S>(
    store: &S,
    owner: &TranslationOwner,
    texts: &[LocalizedText],
) -> DomainResult<()>
where
    S: MediaStore + SeriesMetadataStore,
{
    match owner {
        TranslationOwner::Item(id) => {
            let mut item = store.get(*id).await?;
            if adopt_default_text(Some(&mut item.title), &mut item.metadata.overview, texts) {
                store.put(item).await?;
                log_adopted(owner, "translation pass");
            }
        }
        TranslationOwner::Series(key) => {
            let mut map = store
                .series_metadata_by_keys(std::slice::from_ref(key))
                .await?;
            if let Some(mut meta) = map.remove(key) {
                if adopt_default_text(None, &mut meta.overview, texts) {
                    store.upsert_series_metadata(meta).await?;
                    log_adopted(owner, "translation pass");
                }
            }
        }
    }
    Ok(())
}

fn log_adopted(owner: &TranslationOwner, by: &'static str) {
    tracing::info!(
        owner = ?owner,
        by,
        language = DEFAULT_LANGUAGE,
        "replaced a stored provider base-record description with the English text"
    );
}

/// B564 / V474 — replace provider base-record text in the default columns
/// with the English text from `texts`. Only a value equal to the base
/// record's own text is replaced, so text from a local NFO, or one a person
/// edited, is never touched (local wins). Nothing changes when the record was
/// written in English, or TVDB has no English text. `true` when something
/// changed.
pub(crate) fn adopt_default_text(
    title: Option<&mut String>,
    overview: &mut Option<String>,
    texts: &[LocalizedText],
) -> bool {
    let Some(orig) = texts.iter().find(|t| t.is_original) else {
        return false;
    };
    if orig.language == DEFAULT_LANGUAGE {
        return false;
    }
    let Some(eng) = texts
        .iter()
        .find(|t| t.language == DEFAULT_LANGUAGE && !t.is_original)
    else {
        return false;
    };
    let same = |stored: &str, base: &Option<String>| {
        base.as_deref().is_some_and(|b| b.trim() == stored.trim())
    };
    let mut changed = false;
    if let Some(new) = &eng.overview {
        if overview.as_deref().is_some_and(|o| same(o, &orig.overview)) {
            *overview = Some(new.clone());
            changed = true;
        }
    }
    if let (Some(title), Some(new)) = (title, &eng.title) {
        if same(title, &orig.title) {
            *title = new.clone();
            changed = true;
        }
    }
    changed
}

/// One provider's answer for a show, with its artwork already cached.
struct SeriesHit {
    decision: MatchDecision,
    /// The runner-up's score in the deciding rule: its episode fit when the
    /// episodes decided, else its match confidence.
    runner_up_score: Option<f32>,
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
    Transient(MatchDecision),
}

/// Search `enricher` for `cand` and, on a confident hit, cache the show's
/// poster + backdrop.
#[allow(clippy::too_many_arguments)]
async fn resolve_series<E, S>(
    store: &S,
    enricher: &E,
    provider: &'static str,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    cfg: &MetadataConfig,
    cand: &SeriesMatchCandidate,
    lists: &mut EpisodeLists,
) -> SeriesAttempt
where
    E: OnlineEnricher,
    S: pharos_core::SeriesIdentityStore,
{
    // kind=Episode + season/episode=None routes to the provider's SERIES-level
    // search+fetch (TvdbEnricher: search_series → get_series; TmdbEnricher:
    // search_tv → tv_detail). No nfo id for a synthesised show, so it always
    // searches.
    let mut decision = search_decide(
        enricher,
        MediaKind::Episode,
        &cand.series_name,
        cand.series_year,
        bg_io,
        cfg,
    )
    .await;
    let fit_runner_up = if needs_episode_fit(&decision) {
        apply_episode_fit(store, enricher, provider, bg_io, cand, &mut decision, lists).await
    } else {
        None
    };
    log_series_match(
        provider,
        &cand.series_key,
        &cand.series_name,
        cand.series_year,
        &decision,
    );
    let Some(chosen) = decision.chosen.clone() else {
        return SeriesAttempt::NoMatch;
    };
    let enriched = {
        let _permit = BgPermit::acquire(bg_io).await;
        enricher
            .fetch(MediaKind::Episode, &chosen.id, None, None, None)
            .await
    };
    let Some(enriched) = enriched else {
        return SeriesAttempt::Transient(decision);
    };
    let (external_id, confidence) = (chosen.id, Some(chosen.confidence));
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
    let runner_up_score =
        fit_runner_up.or_else(|| decision.runner_up.as_ref().map(|r| r.confidence));
    SeriesAttempt::Hit(Box::new(SeriesHit {
        decision,
        runner_up_score,
        provider,
        external_id,
        confidence,
        enriched,
        poster_locator,
        backdrop_locator,
    }))
}

/// Spec 033 (V891) — `series_metadata.match_flag` of a close call.
pub(crate) const AMBIGUOUS_FLAG: &str = "ambiguous";

/// Episode lists fetched this pass, by provider, show id and the seasons
/// asked: shows that share candidates (two Battlestar Galactica folders) ask
/// once.
type EpisodeLists = HashMap<(&'static str, String, Vec<u32>), Vec<EpisodeListing>>;

/// The exact-name candidates scoring as the chosen one does, in the
/// provider's order: shows of the same name that no folder or file year told
/// apart. Empty unless the chosen candidate is one of them.
fn exact_candidates(decision: &MatchDecision) -> Vec<&pharos_core::ScoredCandidate> {
    let Some(chosen) = &decision.chosen else {
        return Vec::new();
    };
    let tied = |c: &&pharos_core::ScoredCandidate| {
        c.exact && (c.score - chosen.confidence).abs() <= f32::EPSILON
    };
    if !decision
        .candidates
        .iter()
        .filter(tied)
        .any(|c| c.id == chosen.id)
    {
        return Vec::new();
    }
    decision.candidates.iter().filter(tied).collect()
}

/// B930 — two or more shows match the name exactly and score alike, so the
/// matcher fell back to provider order or to which of their names matched
/// first (TVDB lists a remake as "Battlestar Galactica (2003)" with the bare
/// name as an alias).
fn needs_episode_fit(decision: &MatchDecision) -> bool {
    exact_candidates(decision).len() >= 2
}

/// The show's local episodes, one per (season, episode), with the filename's
/// episode title.
fn local_episodes(rows: Vec<pharos_core::SeriesEpisodeRow>) -> Vec<LocalEpisode> {
    let mut by_number = std::collections::BTreeMap::new();
    for row in rows {
        let (Some(season), Some(episode)) = (row.season, row.episode) else {
            continue;
        };
        let title = std::path::Path::new(&row.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(pharos_scanner::metadata::filename::episode_title);
        let slot = by_number.entry((season, episode)).or_insert(None);
        if slot.is_none() {
            *slot = title;
        }
    }
    by_number
        .into_iter()
        .map(|((season, episode), title)| LocalEpisode {
            season,
            episode,
            title,
        })
        .collect()
}

/// What the show folder's `tvshow.nfo` says, when the key is a folder.
async fn show_nfo(cand: &SeriesMatchCandidate) -> Option<ShowNfoEvidence> {
    let folder = cand
        .series_key
        .split(pharos_scanner::series_split::SPLIT_KEY_SEPARATOR)
        .next()
        .filter(|f| std::path::Path::new(f).is_absolute())?
        .to_string();
    let (name, year) = (cand.series_name.clone(), cand.series_year);
    tokio::task::spawn_blocking(move || {
        pharos_scanner::metadata::nfo::show_nfo_evidence(std::path::Path::new(&folder), &name, year)
    })
    .await
    .ok()
    .flatten()
}

/// Spec 033 (V891) — replace a provider-order tie-break with the candidate
/// whose episodes fit the local files best, and flag a close call. Returns
/// the runner-up's score when the episodes decided. A list that cannot be
/// fetched leaves the provider's choice in place, flagged.
#[allow(clippy::too_many_arguments)]
async fn apply_episode_fit<E, S>(
    store: &S,
    enricher: &E,
    provider: &'static str,
    bg_io: &Arc<Semaphore>,
    cand: &SeriesMatchCandidate,
    decision: &mut MatchDecision,
    lists: &mut EpisodeLists,
) -> Option<f32>
where
    E: OnlineEnricher,
    S: pharos_core::SeriesIdentityStore,
{
    let exact = exact_candidates(decision);
    let unscored = exact.len().saturating_sub(episode_fit::MAX_SCORED);
    let scored: Vec<(Candidate, f32)> = exact
        .into_iter()
        .take(episode_fit::MAX_SCORED)
        .map(|c| {
            (
                Candidate {
                    id: c.id.clone(),
                    year: c.year,
                },
                c.score,
            )
        })
        .collect();
    let rows = match store.series_episodes(&cand.series_key).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, series_key = %cand.series_key, "series episode fit: local episodes unreadable");
            Vec::new()
        }
    };
    let local = local_episodes(rows);
    let seasons: Vec<u32> = local
        .iter()
        .map(|l| l.season)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut fits = Vec::with_capacity(scored.len());
    for (c, _) in &scored {
        let key = (provider, c.id.clone(), seasons.clone());
        if !lists.contains_key(&key) {
            let listing = {
                let _permit = BgPermit::acquire(bg_io).await;
                enricher.episode_list(&c.id, &seasons).await
            };
            match listing {
                Some(l) => {
                    lists.insert(key.clone(), l);
                }
                None => {
                    decision.ambiguous = true;
                    tracing::warn!(
                        provider,
                        series_key = %cand.series_key,
                        candidate = %c.id,
                        "series episode fit: episode list unavailable; provider order kept, flagged"
                    );
                    return None;
                }
            }
        }
        fits.push(lists.get(&key).map_or(0.0, |l| episode_fit::fit(&local, l)));
    }
    let nfo = show_nfo(cand).await;
    let candidates: Vec<Candidate> = scored.iter().map(|(c, _)| c.clone()).collect();
    let vote = episode_fit::nfo_vote(provider, &candidates, nfo.as_ref());
    let verdict = episode_fit::decide(&fits, vote)?;
    let outcome = |at: Option<usize>| {
        let (c, score) = scored.get(at?)?;
        Some(MatchOutcome {
            id: c.id.clone(),
            confidence: *score,
        })
    };
    decision.chosen = outcome(Some(verdict.chosen));
    decision.runner_up = outcome(verdict.runner_up);
    decision.decided_by = Some(verdict.decided_by);
    decision.ambiguous = verdict.close || unscored > 0;
    let scores: Vec<String> = candidates
        .iter()
        .zip(&verdict.scores)
        .map(|(c, s)| format!("{}:{s:.3}", c.id))
        .collect();
    let runner_up_score = verdict.runner_up.and_then(|at| verdict.score(at));
    tracing::info!(
        provider,
        series_key = %cand.series_key,
        local_episodes = local.len(),
        titled = local.iter().filter(|l| l.title.is_some()).count(),
        scores = ?scores,
        nfo_vote = vote.and_then(|at| candidates.get(at)).map(|c| c.id.as_str()),
        chosen = decision.chosen.as_ref().map(|c| c.id.as_str()),
        runner_up = decision.runner_up.as_ref().map(|c| c.id.as_str()),
        margin = verdict.score(verdict.chosen).zip(runner_up_score).map(|(a, b)| a - b),
        unscored,
        flagged = decision.ambiguous,
        decided_by = verdict.decided_by.label(),
        "series episode fit"
    );
    runner_up_score
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
    use std::collections::HashMap;

    /// A remote item searches by TITLE, a local one by FILENAME.
    ///
    /// Both directions in one test: a `search_stem` that always returned the
    /// title would satisfy the remote half alone, and the filename is genuinely
    /// the better key for a local item — it carries the year the display title
    /// has had stripped. Getting this wrong is not a miss but a WRONG match: the
    /// provider would be searched for `dQw4w9WgXcQ`, and whatever came back
    /// would be persisted against the item.
    #[test]
    fn a_remote_item_searches_by_title_not_by_its_synthetic_path() {
        let remote = MediaItem {
            id: 1,
            path: pharos_core::RemoteRef::new("youtube", "dQw4w9WgXcQ")
                .expect("valid ref")
                .to_synthetic_path(),
            title: "Never Gonna Give You Up".into(),
            ..Default::default()
        };
        assert_eq!(search_stem(&remote), "Never Gonna Give You Up");

        let local = MediaItem {
            id: 2,
            path: "/media/Movies/Arrival (2016)/Arrival.2016.1080p.mkv".into(),
            title: "Arrival".into(),
            ..Default::default()
        };
        assert_eq!(search_stem(&local), "Arrival.2016.1080p");
    }

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
        /// Records by the id fetched; any other id gets `detail`.
        detail_for: HashMap<String, EnrichedMetadata>,
        /// Every id `fetch` was asked for, in order.
        fetched: Arc<std::sync::Mutex<Vec<String>>>,
        image_bytes: Option<Vec<u8>>,
        texts: TextsFetch,
        asked: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        /// Episode lists by show id; any other id has none.
        lists: HashMap<String, Vec<EpisodeListing>>,
        /// Every id `episode_list` was asked for, in order.
        listed: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeEnricher {
        fn tmdb() -> Self {
            Self {
                provider: "tmdb",
                search: Vec::new(),
                detail: None,
                detail_for: HashMap::new(),
                fetched: Default::default(),
                image_bytes: None,
                texts: TextsFetch::Failed,
                asked: Default::default(),
                lists: HashMap::new(),
                listed: Default::default(),
            }
        }

        fn tvdb() -> Self {
            Self {
                provider: "tvdb",
                search: Vec::new(),
                detail: None,
                detail_for: HashMap::new(),
                fetched: Default::default(),
                image_bytes: None,
                texts: TextsFetch::Failed,
                asked: Default::default(),
                lists: HashMap::new(),
                listed: Default::default(),
            }
        }

        fn with_search(mut self, cands: Vec<(&str, &str, Option<u32>)>) -> Self {
            self.search = cands
                .into_iter()
                .map(|(id, title, year)| SearchCandidate {
                    id: id.to_string(),
                    title: title.to_string(),
                    year,
                    aliases: Vec::new(),
                })
                .collect();
            self
        }

        fn with_detail(mut self, detail: EnrichedMetadata) -> Self {
            self.detail = Some(detail);
            self
        }

        fn with_detail_for(mut self, id: &str, detail: EnrichedMetadata) -> Self {
            self.detail_for.insert(id.to_string(), detail);
            self
        }

        fn fetched(&self) -> Vec<String> {
            self.fetched.lock().unwrap().clone()
        }

        fn with_list(mut self, id: &str, episodes: &[(u32, u32, &str)]) -> Self {
            let list = episodes
                .iter()
                .map(|&(season, episode, title)| EpisodeListing {
                    season,
                    episode,
                    title: Some(title.to_string()),
                    aired: None,
                })
                .collect();
            self.lists.insert(id.to_string(), list);
            self
        }

        fn listed(&self) -> Vec<String> {
            self.listed.lock().unwrap().clone()
        }

        fn with_image_bytes(mut self, bytes: Vec<u8>) -> Self {
            self.image_bytes = Some(bytes);
            self
        }

        fn with_texts(mut self, texts: Vec<LocalizedText>) -> Self {
            self.texts = TextsFetch::Found(texts);
            self
        }

        /// Every record answers that the provider has no such record.
        fn with_texts_not_found(mut self) -> Self {
            self.texts = TextsFetch::NotFound;
            self
        }

        /// The languages each `fetch_texts` call asked for.
        fn asked(&self) -> Vec<Vec<String>> {
            self.asked.lock().unwrap().clone()
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
            id: &str,
            _season: Option<u32>,
            _episode: Option<u32>,
            _name: Option<&str>,
        ) -> Option<EnrichedMetadata> {
            self.fetched.lock().unwrap().push(id.to_string());
            self.detail_for
                .get(id)
                .cloned()
                .or_else(|| self.detail.clone())
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
        async fn fetch_texts(
            &self,
            _kind: MediaKind,
            _id: &str,
            _season: Option<u32>,
            _episode: Option<u32>,
            _name: Option<&str>,
            languages: &[String],
        ) -> TextsFetch {
            self.asked.lock().unwrap().push(languages.to_vec());
            self.texts.clone()
        }
        async fn episode_list(&self, id: &str, _seasons: &[u32]) -> Option<Vec<EpisodeListing>> {
            self.listed.lock().unwrap().push(id.to_string());
            self.lists.get(id).cloned()
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
        // The stored MATCH version is deliberately stale — that is the one
        // whose change drops covers (B160).
        pharos_store_sqlx::ServerConfigStore::set_named_config(&s, ALBUM_MATCH_VERSION_KEY, "0")
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
            pharos_store_sqlx::ServerConfigStore::load_named_config(&s, ALBUM_MATCH_VERSION_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(crate::musicbrainz::ALBUM_MATCH_VERSION.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn a_query_bump_re_admits_misses_without_dropping_covers() {
        // B160 — clearing every cached cover is the expensive path and was
        // being taken for every ladder improvement. B152 could only ever turn
        // misses into hits, yet it dropped the library from 1243 covered
        // tracks to 727 and took about an hour of blank tiles to recover. A
        // query bump must leave hits alone; the miss marker already re-admits
        // the rows that need re-asking.
        let s = store().await;
        put_track(&s, 900_420, "Owl City", "Ocean Eyes").await;
        s.set_artwork(
            900_420,
            "Primary",
            "musicbrainz",
            "/cache/primary/audio/ocean-eyes.jpg",
        )
        .await
        .unwrap();
        assert!(s.get(900_420).await.unwrap().has_primary_art);
        // Versions agree on the MATCH, but the QUERY has moved on.
        for (k, v) in [
            (
                ALBUM_MATCH_VERSION_KEY,
                crate::musicbrainz::ALBUM_MATCH_VERSION.to_string(),
            ),
            (ALBUM_ART_VERSION_KEY, "1".to_string()),
        ] {
            pharos_store_sqlx::ServerConfigStore::set_named_config(&s, k, &v)
                .await
                .unwrap();
        }

        invalidate_stale_album_art(&s).await.unwrap();

        assert!(
            s.get(900_420).await.unwrap().has_primary_art,
            "a query-version bump must not drop a cover that is already correct"
        );
        assert_eq!(
            pharos_store_sqlx::ServerConfigStore::load_named_config(&s, ALBUM_ART_VERSION_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(
                crate::musicbrainz::ALBUM_ART_QUERY_VERSION
                    .to_string()
                    .as_str()
            ),
            "the query version is still recorded"
        );
    }

    #[tokio::test]
    async fn a_first_boot_adopts_the_match_version_without_clearing() {
        // B160 — an ABSENT record is not a stale one. A fresh install, and the
        // first boot after this split, must adopt the version silently rather
        // than treat "never recorded" as "changed" and wipe the library.
        let s = store().await;
        put_track(&s, 900_421, "Owl City", "Ocean Eyes").await;
        s.set_artwork(900_421, "Primary", "musicbrainz", "/cache/p.jpg")
            .await
            .unwrap();

        invalidate_stale_album_art(&s).await.unwrap();

        assert!(
            s.get(900_421).await.unwrap().has_primary_art,
            "nothing was recorded, so there was nothing to invalidate"
        );
        assert_eq!(
            pharos_store_sqlx::ServerConfigStore::load_named_config(&s, ALBUM_MATCH_VERSION_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(crate::musicbrainz::ALBUM_MATCH_VERSION.to_string().as_str())
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

    // B152 — Owl City's `Fireflies` carries `album_artist = ""` alongside a
    // perfectly good `artist = "Owl City"`. `Option::or` keeps the `Some("")`,
    // and an empty artist reads downstream as "unknown", which the B150
    // consistency check lets pass — so the artist verification that fix added
    // was silently switched off for exactly the track that motivated it.
    #[tokio::test]
    async fn a_blank_album_artist_falls_through_to_the_track_artist() {
        let s = store().await;
        let item = MediaItem {
            id: 900_408,
            path: "/music/owl city/fireflies.mp3".into(),
            title: "Fireflies".into(),
            kind: MediaKind::Audio,
            probe: pharos_core::MediaProbe {
                album: Some("Fireflies - Single of the Week".into()),
                // Exactly the shape found in production: present but empty.
                album_artist: Some("   ".into()),
                artist: Some("Owl City".into()),
                ..Default::default()
            },
            ..MediaItem::default()
        };
        s.put(item.clone()).await.unwrap();
        let (_td, cache) = cache();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
        let spy = SpyAlbumArt(seen.clone());

        enrich_audio_one(&s, &sem(4), &cache, &spy, &item, NOW)
            .await
            .unwrap();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [Some("Owl City".to_string())],
            "the blank album_artist must not shadow the track artist"
        );
    }

    /// Records the artist each lookup was given, so the tag-selection rule can
    /// be asserted directly rather than inferred from a match.
    struct SpyAlbumArt(std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>);

    impl AlbumArtResolver for SpyAlbumArt {
        async fn album_art(
            &self,
            artist: Option<&str>,
            _album: Option<&str>,
        ) -> Result<AlbumArt, crate::musicbrainz::AlbumArtMiss> {
            self.0
                .lock()
                .expect("spy lock")
                .push(artist.map(str::to_string));
            Err(crate::musicbrainz::AlbumArtMiss::NoAlbumTag)
        }
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

    /// An NFO-identified episode as a scrape in the provider's numbering left
    /// it: the file is "S01E03 - The Naked Now", its NFO and `-thumb` are
    /// Code of Honor's.
    async fn put_shifted_episode(s: &SqliteStore) -> MediaItem {
        let dir = "/tv/Star Trek The Next Generation/Season 01";
        let stem = "Star Trek - The Next Generation - S01E03 - The Naked Now WEBDL-1080p";
        let mut item = MediaItem {
            id: 900_570,
            path: format!("{dir}/{stem}.mkv").into(),
            title: "Code of Honor".into(),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "Star Trek The Next Generation".into(),
                season_number: Some(1),
                episode_number: Some(3),
                series_folder: Some("/tv/Star Trek The Next Generation".into()),
                series_year: None,
                series_key: None,
            }),
            ..MediaItem::default()
        };
        item.metadata.overview = Some("Tasha Yar is kidnapped.".into());
        item.metadata.provider_ids.tvdb = Some("71470".into());
        s.put(item).await.unwrap();
        pin_series(s, "/tv/Star Trek The Next Generation", "71470").await;
        s.set_artwork(
            900_570,
            "Thumb",
            "local",
            &format!("{dir}/{stem}-thumb.jpg"),
        )
        .await
        .unwrap();
        s.set_artwork(
            900_570,
            "Primary",
            "local",
            "/tv/Star Trek The Next Generation/folder.jpg",
        )
        .await
        .unwrap();
        s.get(900_570).await.unwrap()
    }

    fn naked_now(number: (u32, u32)) -> EnrichedMetadata {
        EnrichedMetadata {
            title: Some("The Naked Now".into()),
            overview: Some("An infection.".into()),
            episode_number: Some(number),
            neighbour_titles: vec!["Encounter at Farpoint".into(), "Code of Honor".into()],
            series_id: Some("71470".into()),
            artwork: vec![RemoteArt {
                role: ArtworkRole::Primary,
                url: "https://artworks.thetvdb.com/naked-now.jpg".into(),
            }],
            ..EnrichedMetadata::default()
        }
    }

    /// B570 — an episode the provider placed by its name on another number
    /// takes that record's title, plot and still over its NFO's, which were
    /// written for the episode next to it. The show-folder Primary stays.
    #[tokio::test]
    async fn a_reidentified_episode_replaces_its_scraped_text_and_still() {
        let s = store().await;
        let item = put_shifted_episode(&s).await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_detail(naked_now((1, 2)))
            .with_image_bytes(vec![0xFF, 0xD8, 0xFF]);
        enrich_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap();

        let ep = s.get(900_570).await.unwrap();
        assert_eq!(ep.title, "The Naked Now");
        assert_eq!(ep.metadata.overview.as_deref(), Some("An infection."));
        let art = s.artwork_for(900_570).await.unwrap();
        let role = |r: &str| art.iter().find(|(role, _, _)| role == r).cloned();
        assert_eq!(
            role("Thumb").map(|r| r.1).as_deref(),
            Some("tvdb"),
            "{art:?}"
        );
        assert_eq!(
            role("Primary").map(|r| r.1).as_deref(),
            Some("local"),
            "{art:?}"
        );
    }

    /// B588 — a record at the file's own number that is the episode the
    /// filename names replaces an NFO that names another: the NFO, not the
    /// provider, was written in a shifted numbering.
    #[tokio::test]
    async fn a_record_named_as_the_file_replaces_an_nfo_naming_another() {
        let s = store().await;
        let item = put_shifted_episode(&s).await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_detail(naked_now((1, 3)))
            .with_image_bytes(vec![0xFF, 0xD8, 0xFF]);
        enrich_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap();

        let ep = s.get(900_570).await.unwrap();
        assert_eq!(ep.title, "The Naked Now");
        assert_eq!(ep.metadata.overview.as_deref(), Some("An infection."));
        let art = s.artwork_for(900_570).await.unwrap();
        assert!(
            art.iter()
                .any(|(role, source, _)| role == "Thumb" && source == "tvdb"),
            "{art:?}"
        );
    }

    /// B588 — an NFO title that is this episode's name in another language
    /// names no neighbour, so it stays.
    #[tokio::test]
    async fn an_nfo_title_in_another_language_stays() {
        let s = store().await;
        let mut item = put_shifted_episode(&s).await;
        item.title = "Le Maintenant Nu".into();
        s.put(item.clone()).await.unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_detail(naked_now((1, 3)))
            .with_image_bytes(vec![0xFF, 0xD8, 0xFF]);
        enrich_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(s.get(900_570).await.unwrap().title, "Le Maintenant Nu");
    }

    /// B570, B588 — a record at the file's own number that names neither the
    /// file's episode leaves the NFO's text and still alone.
    #[tokio::test]
    async fn an_episode_on_its_own_number_keeps_its_local_text_and_still() {
        let s = store().await;
        let item = put_shifted_episode(&s).await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_detail(EnrichedMetadata {
                title: Some("Le Maintenant Nu".into()),
                ..naked_now((1, 3))
            })
            .with_image_bytes(vec![0xFF, 0xD8, 0xFF]);
        enrich_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap();

        let ep = s.get(900_570).await.unwrap();
        assert_eq!(ep.title, "Code of Honor");
        assert_eq!(
            ep.metadata.overview.as_deref(),
            Some("Tasha Yar is kidnapped.")
        );
        let art = s.artwork_for(900_570).await.unwrap();
        assert!(
            art.iter()
                .any(|(role, source, _)| role == "Thumb" && source == "local"),
            "{art:?}"
        );
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
                series_key: None,
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

    /// B557 — the show matcher changed, so every show and episode it had
    /// answered by search is asked again at once, not when the TTL expires.
    /// NFO-sourced matches are the user's own and stay put.
    #[tokio::test]
    async fn a_new_show_matcher_re_admits_searched_shows_and_episodes_once() {
        let s = store().await;
        put_episode(&s, 1, "Dragon Ball", "/tv/Dragon Ball").await;
        put_episode(&s, 2, "Dragon Ball", "/tv/Dragon Ball").await;
        s.set_item_match(1, "tvdb", "374456", "search", Some(0.83), NOW)
            .await
            .unwrap();
        s.set_item_match(2, "tvdb", "76666", "nfo_id", None, NOW)
            .await
            .unwrap();
        s.upsert_series_metadata(SeriesMetadata {
            series_key: "/tv/Dragon Ball".into(),
            series_name: "Dragon Ball".into(),
            match_provider: Some("tvdb".into()),
            match_external_id: Some("374456".into()),
            match_source: Some("search".into()),
            metadata_refreshed_at: Some(NOW),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(s
            .series_needing_match(10, NOW - 1)
            .await
            .unwrap()
            .is_empty());
        assert!(s.items_needing_match(10, NOW - 1).await.unwrap().is_empty());

        readmit_stale_tv_matches(&s).await.unwrap();

        let shows = s.series_needing_match(10, NOW - 1).await.unwrap();
        assert_eq!(shows.len(), 1, "the searched show is eligible again");
        let items: Vec<_> = s
            .items_needing_match(10, NOW - 1)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(items, [1], "only the searched episode, not the NFO one");

        // Recorded: the next pass re-admits nothing.
        s.set_item_match(1, "tvdb", "76666", "search", Some(0.85), NOW)
            .await
            .unwrap();
        readmit_stale_tv_matches(&s).await.unwrap();
        assert!(s.items_needing_match(10, NOW - 1).await.unwrap().is_empty());
    }

    /// B557 — a re-admitted episode is searched again. The id its old search
    /// stamped into `provider_ids` must not pass for an NFO id, or the episode
    /// keeps the wrong show and is recorded as `nfo_id`, out of reach of any
    /// later re-admission.
    #[tokio::test]
    async fn a_re_admitted_episode_is_searched_again_not_resolved_by_its_old_id() {
        let s = store().await;
        put_episode(&s, 1, "Dragon Ball", "/tv/Dragon Ball").await;
        let mut item = s.get(1).await.unwrap();
        item.metadata.provider_ids.tvdb = Some("374456".into());
        s.put(item).await.unwrap();
        s.set_item_match(1, "tvdb", "374456", "search", Some(0.83), NOW)
            .await
            .unwrap();
        readmit_stale_tv_matches(&s).await.unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![("76666", "Dragon Ball", Some(1997))])
            .with_detail(enriched_overview("Goku's adventures"));

        run(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW + 1,
        )
        .await
        .unwrap();

        let got = s.get(1).await.unwrap();
        assert_eq!(got.match_source.as_deref(), Some("search"));
        assert_eq!(got.match_external_id.as_deref(), Some("76666"));
        assert_eq!(got.metadata.provider_ids.tvdb.as_deref(), Some("76666"));
    }

    /// A re-admitted episode whose new search finds nothing loses the id its
    /// old search stamped too. Otherwise the `none` stamp makes that id look
    /// trusted, and the next pass fetches it as if an NFO named it.
    /// Disarm check: dropping the NoMatch `put` fails this.
    #[tokio::test]
    async fn a_re_admitted_episodes_old_id_is_dropped_when_its_search_finds_nothing() {
        let s = store().await;
        put_episode(&s, 1, "Dragon Ball", "/tv/Dragon Ball").await;
        let mut item = s.get(1).await.unwrap();
        item.metadata.provider_ids.tvdb = Some("374456".into());
        s.put(item).await.unwrap();
        s.set_item_match(1, "tvdb", "374456", "search", Some(0.83), NOW)
            .await
            .unwrap();
        readmit_stale_tv_matches(&s).await.unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb().with_search(vec![]);

        run(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW + 1,
        )
        .await
        .unwrap();

        let got = s.get(1).await.unwrap();
        assert_eq!(got.match_source.as_deref(), Some("none"));
        assert_eq!(got.metadata.provider_ids.tvdb, None);
    }

    #[test]
    fn a_provider_id_from_local_data_outlives_a_search_match() {
        let mut item = MediaItem {
            kind: MediaKind::Episode,
            match_provider: Some("tvdb".into()),
            match_external_id: Some("374456".into()),
            match_source: Some("search".into()),
            ..MediaItem::default()
        };
        item.metadata.provider_ids.tvdb = Some("374456".into());
        assert_eq!(trusted_provider_ids(&item).tvdb, None);

        item.metadata.provider_ids.tvdb = Some("76666".into());
        assert_eq!(trusted_provider_ids(&item).tvdb.as_deref(), Some("76666"));

        item.match_source = Some("nfo_id".into());
        item.metadata.provider_ids.tvdb = Some("374456".into());
        assert_eq!(trusted_provider_ids(&item).tvdb.as_deref(), Some("374456"));
    }

    /// The show matcher's re-admission does not reach movies, so a movie's
    /// TTL refresh fetches the record its search matched rather than
    /// searching again. Disarm check: dropping the kind guard fails this.
    #[test]
    fn a_movies_searched_id_is_kept_for_its_refresh() {
        let mut item = MediaItem {
            kind: MediaKind::Movie,
            match_provider: Some("tmdb".into()),
            match_external_id: Some("603".into()),
            match_source: Some("search".into()),
            ..MediaItem::default()
        };
        item.metadata.provider_ids.tmdb = Some("603".into());
        assert_eq!(trusted_provider_ids(&item).tmdb.as_deref(), Some("603"));
    }

    // --- B564: provider text per language ---------------------------------

    fn text(lang: &str, title: &str, overview: &str, original: bool) -> LocalizedText {
        LocalizedText {
            language: lang.into(),
            title: Some(title.into()),
            overview: Some(overview.into()),
            is_original: original,
        }
    }

    fn anime_texts() -> Vec<LocalizedText> {
        vec![
            text("jpn", "ブルマと孫悟空", "ブルマが…", true),
            text(
                "eng",
                "The Secret of the Dragon Balls",
                "Bulma meets Goku.",
                false,
            ),
        ]
    }

    #[test]
    fn adopt_replaces_only_the_base_record_text() {
        let mut title = "ブルマと孫悟空".to_string();
        let mut overview = Some(" ブルマが…".to_string());
        assert!(adopt_default_text(
            Some(&mut title),
            &mut overview,
            &anime_texts()
        ));
        assert_eq!(title, "The Secret of the Dragon Balls");
        assert_eq!(overview.as_deref(), Some("Bulma meets Goku."));

        // Local NFO text is not the provider's: left alone.
        let mut title = "Bulma and Son Goku".to_string();
        let mut overview = Some("Une description française.".to_string());
        assert!(!adopt_default_text(
            Some(&mut title),
            &mut overview,
            &anime_texts()
        ));
        assert_eq!(overview.as_deref(), Some("Une description française."));

        // No English at TVDB: the original stays.
        let mut overview = Some("ブルマが…".to_string());
        assert!(!adopt_default_text(
            None,
            &mut overview,
            &anime_texts()[..1]
        ));

        // English-made: nothing to replace.
        let eng_only = [text("eng", "Pilot", "Lord Stark...", true)];
        let mut overview = Some("Lord Stark...".to_string());
        assert!(!adopt_default_text(None, &mut overview, &eng_only));
    }

    /// An anime episode and show stored before B564, with TVDB's Japanese
    /// base-record text. Matched by NFO id, so the show-matcher version
    /// check in `run` does not re-admit them for a fresh search.
    async fn put_anime(s: &SqliteStore) {
        let item = MediaItem {
            id: 77,
            path: "/tv/Dragon Ball/S01E01.mkv".into(),
            title: "ブルマと孫悟空".into(),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "Dragon Ball".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some("/tv/Dragon Ball".into()),
                series_year: Some(1986),
                series_key: None,
            }),
            metadata: pharos_core::MediaMetadata {
                overview: Some("ブルマが…".into()),
                ..Default::default()
            },
            ..MediaItem::default()
        };
        s.put(item).await.unwrap();
        s.set_item_match(77, "tvdb", "76666", "nfo_id", Some(0.9), NOW)
            .await
            .unwrap();
        s.upsert_series_metadata(SeriesMetadata {
            series_key: "/tv/Dragon Ball".into(),
            series_name: "Dragon Ball".into(),
            match_provider: Some("tvdb".into()),
            match_external_id: Some("76666".into()),
            match_source: Some("nfo_id".into()),
            metadata_refreshed_at: Some(NOW),
            overview: Some("悟空の冒険。".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn translation_pass_replaces_stored_base_record_text_with_english() {
        let s = store().await;
        put_anime(&s).await;
        let tvdb = FakeEnricher::tvdb().with_texts(vec![
            text("jpn", "ブルマと孫悟空", "ブルマが…", true),
            text("jpn", "ドラゴンボール", "悟空の冒険。", true),
            text(
                "eng",
                "The Secret of the Dragon Balls",
                "Bulma meets Goku.",
                false,
            ),
        ]);
        // One fake answers for show and episode alike; give each its own.
        let ep_texts = anime_texts();
        let show_texts = vec![
            text("jpn", "ドラゴンボール", "悟空の冒険。", true),
            text("eng", "Dragon Ball", "Goku sets out.", false),
        ];
        adopt_for_owner(&s, &TranslationOwner::Item(77), &ep_texts)
            .await
            .unwrap();
        adopt_for_owner(
            &s,
            &TranslationOwner::Series("/tv/Dragon Ball".into()),
            &show_texts,
        )
        .await
        .unwrap();
        let ep = s.get(77).await.unwrap();
        assert_eq!(ep.title, "The Secret of the Dragon Balls");
        assert_eq!(ep.metadata.overview.as_deref(), Some("Bulma meets Goku."));
        assert_eq!(ep.match_source.as_deref(), Some("nfo_id"), "still matched");
        let show = s
            .series_metadata_by_keys(&["/tv/Dragon Ball".into()])
            .await
            .unwrap();
        assert_eq!(
            show["/tv/Dragon Ball"].overview.as_deref(),
            Some("Goku sets out.")
        );

        // The pass stores rows for both owners and asks once each.
        let stored = translation_pass(&s, &sem(2), &tvdb, NOW).await.unwrap();
        assert_eq!(stored, 2);
        assert_eq!(tvdb.asked(), vec![vec!["eng".to_string()]; 2]);
        let rows = s
            .translations_for(&[TranslationOwner::Item(77)], &[], true)
            .await
            .unwrap();
        assert!(rows
            .iter()
            .any(|(_, t)| t.is_original && t.language == "jpn"));
        // Nothing left to ask.
        assert_eq!(translation_pass(&s, &sem(2), &tvdb, NOW).await.unwrap(), 0);
    }

    /// The record that trips the outage stop is marked before the pass
    /// stops, so the next pass starts behind it rather than on it.
    #[tokio::test]
    async fn the_record_that_stops_a_pass_is_marked_first() {
        let s = store().await;
        put_anime(&s).await;
        for id in [78u32, 79] {
            let mut item = s.get(77).await.unwrap();
            item.id = u64::from(id);
            item.path = format!("/tv/Dragon Ball/S01E{id}.mkv").into();
            if let Some(series) = item.series.as_mut() {
                series.episode_number = Some(id);
            }
            s.put(item).await.unwrap();
            s.set_item_match(u64::from(id), "tvdb", "76666", "nfo_id", Some(0.9), NOW)
                .await
                .unwrap();
        }
        // No texts: every record is unreadable.
        let tvdb = FakeEnricher::tvdb();
        assert_eq!(translation_pass(&s, &sem(2), &tvdb, NOW).await.unwrap(), 0);
        assert_eq!(tvdb.asked().len(), MAX_CONSECUTIVE_FAILURES);
        // Four owners (three episodes and the show); the three asked are all
        // marked, so the next pass asks only the fourth.
        translation_pass(&s, &sem(2), &tvdb, NOW).await.unwrap();
        assert_eq!(tvdb.asked().len(), MAX_CONSECUTIVE_FAILURES + 1);
    }

    #[tokio::test]
    async fn translation_pass_end_to_end_through_run() {
        let s = store().await;
        put_anime(&s).await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb().with_texts(anime_texts());
        let cfg = MetadataConfig::default();
        run(
            &s,
            &sem(2),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            None::<&MusicBrainzClient>,
            &cfg,
            NOW,
        )
        .await
        .unwrap();
        let ep = s.get(77).await.unwrap();
        assert_eq!(ep.metadata.overview.as_deref(), Some("Bulma meets Goku."));
    }

    #[tokio::test]
    async fn the_kill_switch_leaves_stored_text_alone() {
        let s = store().await;
        put_anime(&s).await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb().with_texts(anime_texts());
        let cfg = MetadataConfig {
            fetch_translations: false,
            ..MetadataConfig::default()
        };
        run(
            &s,
            &sem(2),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            None::<&MusicBrainzClient>,
            &cfg,
            NOW,
        )
        .await
        .unwrap();
        assert!(tvdb.asked().is_empty());
        let ep = s.get(77).await.unwrap();
        assert_eq!(ep.metadata.overview.as_deref(), Some("ブルマが…"));
    }

    /// Records the provider has no such record for (anime numbered
    /// absolutely, say) are not an outage: a pass marks every one of them
    /// missing for MISSING_RETRY_SECS and goes on, however many there are,
    /// so the owners behind them are reached.
    #[tokio::test]
    async fn records_the_provider_does_not_have_do_not_stop_a_pass() {
        let s = store().await;
        put_anime(&s).await;
        for id in [78u32, 79, 80, 81] {
            let mut item = s.get(77).await.unwrap();
            item.id = u64::from(id);
            item.path = format!("/tv/Dragon Ball/S01E{id}.mkv").into();
            if let Some(series) = item.series.as_mut() {
                series.episode_number = Some(id);
            }
            s.put(item).await.unwrap();
            s.set_item_match(u64::from(id), "tvdb", "76666", "nfo_id", Some(0.9), NOW)
                .await
                .unwrap();
        }
        let tvdb = FakeEnricher::tvdb().with_texts_not_found();
        // Six owners (five episodes and the show), all asked in one pass.
        assert_eq!(translation_pass(&s, &sem(2), &tvdb, NOW).await.unwrap(), 6);
        assert_eq!(tvdb.asked().len(), 6);
        const { assert!(MAX_CONSECUTIVE_FAILURES < 6) };
        // Marked for the long wait: a day later nothing is asked again.
        translation_pass(&s, &sem(2), &tvdb, NOW + FAILED_RETRY_SECS + 1)
            .await
            .unwrap();
        assert_eq!(tvdb.asked().len(), 6);
    }

    /// A user's language gets its share of a pass even when the English
    /// backlog alone fills the budget: with room for two owners and six
    /// owners missing both languages, French is still asked for.
    #[tokio::test]
    async fn a_user_language_is_not_starved_by_the_english_backlog() {
        let s = store().await;
        put_anime(&s).await;
        for id in [78u32, 79, 80, 81] {
            let mut item = s.get(77).await.unwrap();
            item.id = u64::from(id);
            item.path = format!("/tv/Dragon Ball/S01E{id}.mkv").into();
            if let Some(series) = item.series.as_mut() {
                series.episode_number = Some(id);
            }
            s.put(item).await.unwrap();
            s.set_item_match(u64::from(id), "tvdb", "76666", "nfo_id", Some(0.9), NOW)
                .await
                .unwrap();
        }
        let langs = vec!["eng".to_string(), "fra".to_string()];
        let todo = collect_gaps(&s, "tvdb", &langs, NOW, 2).await.unwrap();
        assert_eq!(todo.len(), 2, "the budget caps the owners");
        assert!(
            todo.iter().any(|(_, l)| l.contains(&"fra".to_string())),
            "French gets a share: {:?}",
            todo.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>()
        );
        // Room the shares leave (both languages picked the same owners)
        // goes back to the languages that still have gaps.
        let todo = collect_gaps(&s, "tvdb", &langs, NOW, 4).await.unwrap();
        assert_eq!(todo.len(), 4);
    }

    /// The kill switch also holds for a re-enrichment: `enrich_one` keeps the
    /// stored base-record text when `fetch_translations` is off, and replaces
    /// it with the English text when it is on.
    #[tokio::test]
    async fn the_kill_switch_also_holds_for_a_re_enrichment() {
        for (fetch, want) in [(false, "砂の惑星…"), (true, "A duke's son...")] {
            let s = store().await;
            put_movie(&s, 900_400, "Dune (2021)").await;
            let mut item = s.get(900_400).await.unwrap();
            item.metadata.overview = Some("砂の惑星…".into());
            s.put(item).await.unwrap();
            let (_td, cache) = cache();
            let tmdb = FakeEnricher::tmdb()
                .with_search(vec![("438631", "Dune", Some(2021))])
                .with_detail(EnrichedMetadata {
                    texts: vec![
                        text("jpn", "Dune", "砂の惑星…", true),
                        text("eng", "Dune", "A duke's son...", false),
                    ],
                    ..EnrichedMetadata::default()
                });
            let cfg = MetadataConfig {
                fetch_translations: fetch,
                ..MetadataConfig::default()
            };
            let item = s.get(900_400).await.unwrap();
            enrich_one(
                &s,
                &sem(4),
                &cache,
                Some(&tmdb),
                None::<&FakeEnricher>,
                &cfg,
                item,
                NOW,
            )
            .await
            .unwrap();
            let got = s.get(900_400).await.unwrap();
            assert_eq!(
                got.metadata.overview.as_deref(),
                Some(want),
                "fetch_translations = {fetch}"
            );
        }
    }

    #[tokio::test]
    async fn a_language_a_user_picked_is_fetched_too() {
        use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
        let s = store().await;
        put_anime(&s).await;
        let user = UserRecord {
            id: UserId::new(),
            name: "amélie".into(),
            password_hash: SecretString::new("$argon2id$fake"),
            policy: UserPolicy::default(),
        };
        let uid = user.id;
        UserStore::create(&s, user).await.unwrap();
        s.set_user_configuration(uid, r#"{"DescriptionLanguagePreference":"fre"}"#)
            .await
            .unwrap();
        assert_eq!(wanted_languages(&s).await, vec!["eng", "fra"]);
        let tvdb = FakeEnricher::tvdb().with_texts(anime_texts());
        translation_pass(&s, &sem(2), &tvdb, NOW).await.unwrap();
        assert!(tvdb
            .asked()
            .iter()
            .all(|langs| langs == &vec!["eng".to_string(), "fra".to_string()]));
    }

    #[tokio::test]
    async fn an_unreadable_record_is_retried_after_a_day_not_every_pass() {
        let s = store().await;
        put_anime(&s).await;
        let tvdb = FakeEnricher::tvdb(); // fetch_texts answers None
        assert_eq!(translation_pass(&s, &sem(2), &tvdb, NOW).await.unwrap(), 0);
        assert_eq!(tvdb.asked().len(), 2);
        translation_pass(&s, &sem(2), &tvdb, NOW + 60)
            .await
            .unwrap();
        assert_eq!(tvdb.asked().len(), 2, "not asked again within the day");
        translation_pass(&s, &sem(2), &tvdb, NOW + FAILED_RETRY_SECS + 1)
            .await
            .unwrap();
        assert_eq!(tvdb.asked().len(), 4, "asked again after a day");
    }

    #[tokio::test]
    async fn enrichment_stores_the_text_rows() {
        let s = store().await;
        put_episode(&s, 1, "Dragon Ball", "/tv/Dragon Ball").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![("76666", "Dragon Ball", Some(1997))])
            .with_detail(EnrichedMetadata {
                overview: Some("Bulma meets Goku.".into()),
                texts: anime_texts(),
                ..EnrichedMetadata::default()
            });
        run(
            &s,
            &sem(2),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            None::<&MusicBrainzClient>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        let rows = s
            .translations_for(&[TranslationOwner::Item(1)], &["eng".into()], true)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        let show = s
            .translations_for(
                &[TranslationOwner::Series("/tv/Dragon Ball".into())],
                &["eng".into()],
                false,
            )
            .await
            .unwrap();
        assert_eq!(show.len(), 1);
    }

    /// Spec 033 — a log buffer for the decision lines. Each nextest test runs
    /// in its own process, so a thread-scoped subscriber sees every callsite.
    #[derive(Clone, Default)]
    struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> LogBuf {
            self.clone()
        }
    }

    impl LogBuf {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    fn log_capture() -> (LogBuf, tracing::subscriber::DefaultGuard) {
        let buf = LogBuf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        (buf, tracing::subscriber::set_default(sub))
    }

    /// Counter value for `name` with exactly these `labels` (0 when absent).
    fn counter_value(
        snap: &pharos_test_metrics::CumulativeSnapshotter,
        name: &str,
        labels: &[(&str, &str)],
    ) -> u64 {
        use metrics_util::debugging::DebugValue;
        snap.snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == name)
            .filter(|(k, ..)| {
                labels
                    .iter()
                    .all(|(n, v)| k.key().labels().any(|l| l.key() == *n && l.value() == *v))
            })
            .map(|(.., v)| match v {
                DebugValue::Counter(c) => c,
                _ => 0,
            })
            .sum()
    }

    async fn put_bare_episode(store: &SqliteStore, id: u64, series: &str, folder: &str) {
        let item = MediaItem {
            id,
            path: format!("{folder}/{series} (2003) - S01E{id:02}.mkv").into(),
            title: format!("{series} ep {id}"),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: series.into(),
                season_number: Some(1),
                episode_number: Some(id as u32),
                series_folder: Some(folder.into()),
                series_year: None,
                series_key: None,
            }),
            ..MediaItem::default()
        };
        store.put(item).await.unwrap();
    }

    /// Spec 033 B930 — a bare `Battlestar Galactica` folder searches with no
    /// year, two shows carry the name, and the provider's order picks the
    /// 1978 one. The decision is counted as ambiguous, decided by provider
    /// order, and logged with both candidates.
    #[tokio::test(flavor = "current_thread")]
    async fn a_same_named_show_decided_by_provider_order_is_counted_and_logged() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();

        let s = store().await;
        put_bare_episode(&s, 1, "Battlestar Galactica", "/tv/Battlestar Galactica").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![
                ("71173", "Battlestar Galactica", Some(1978)),
                ("73545", "Battlestar Galactica", Some(2003)),
            ])
            .with_detail(enriched_overview("A rag-tag fleet."));

        enrich_series_pass(
            &s,
            &sem(4),
            &cache,
            Some(&tvdb),
            None::<&FakeEnricher>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();

        assert_eq!(
            counter_value(
                &snap,
                "pharos_series_match_total",
                &[("outcome", "ambiguous"), ("decided_by", "provider_order")]
            ),
            1
        );
        assert_eq!(
            counter_value(&snap, "pharos_series_match_total", &[]),
            1,
            "one count per show"
        );
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("series match"))
            .unwrap_or_else(|| panic!("no decision line in {text}"));
        for want in [
            "query=\"Battlestar Galactica\"",
            "chosen=\"71173\"",
            "runner_up=\"73545\"",
            "margin=0.0",
            "decided_by=\"provider_order\"",
            "outcome=\"ambiguous\"",
        ] {
            assert!(line.contains(want), "{want} missing from {line}");
        }
    }

    /// One episode file of `folder`, named `"{series} - S..E.. - {title}"`.
    async fn put_titled_episode(
        store: &SqliteStore,
        id: u64,
        folder: &str,
        (season, episode): (u32, u32),
        title: &str,
    ) {
        let series = folder.rsplit('/').next().unwrap_or(folder);
        let item = MediaItem {
            id,
            path: format!(
                "{folder}/Season {season}/{series} - S{season:02}E{episode:02} - {title}.mkv"
            )
            .into(),
            title: title.into(),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: series.into(),
                season_number: Some(season),
                episode_number: Some(episode),
                series_folder: Some(folder.into()),
                series_year: None,
                series_key: None,
            }),
            ..MediaItem::default()
        };
        store.put(item).await.unwrap();
    }

    fn bsg_search(tvdb: FakeEnricher) -> FakeEnricher {
        tvdb.with_search(vec![
            ("71173", "Battlestar Galactica", Some(1978)),
            ("73545", "Battlestar Galactica", Some(2003)),
        ])
        .with_detail(enriched_overview("A rag-tag fleet."))
    }

    async fn series_row(s: &SqliteStore, key: &str) -> SeriesMetadata {
        s.series_metadata_by_keys(&[key.to_string()])
            .await
            .unwrap()
            .remove(key)
            .unwrap_or_else(|| panic!("no series row for {key}"))
    }

    async fn series_pass(s: &SqliteStore, cache: &ImageCache, tvdb: &FakeEnricher) {
        enrich_series_pass(
            s,
            &sem(4),
            cache,
            Some(tvdb),
            None::<&FakeEnricher>,
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
    }

    /// Spec 033 V891 / B930 — two shows named exactly "Battlestar Galactica"
    /// and no year: the one whose episodes the folder holds wins, not the
    /// provider's first. Counted and logged as decided by episode fit, and
    /// each show's list is fetched once per pass however many folders ask.
    #[tokio::test(flavor = "current_thread")]
    async fn a_same_named_show_is_decided_by_its_episodes() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();

        let s = store().await;
        for (id, season, title) in [
            (1, 1, "33"),
            (2, 2, "Valley of Darkness"),
            (3, 4, "He That Believeth in Me"),
        ] {
            put_titled_episode(&s, id, "/tv/Battlestar Galactica", (season, 1), title).await;
            put_titled_episode(&s, id + 10, "/tv2/Battlestar Galactica", (season, 1), title).await;
        }
        let (_td, cache) = cache();
        let tvdb = bsg_search(FakeEnricher::tvdb())
            .with_list(
                "71173",
                &[
                    (1, 1, "Saga of a Star World"),
                    (1, 2, "Lost Planet of the Gods"),
                ],
            )
            .with_list(
                "73545",
                &[
                    (1, 1, "33"),
                    (2, 1, "Valley of Darkness"),
                    (3, 1, "Occupation"),
                    (4, 1, "He That Believeth in Me"),
                ],
            );

        series_pass(&s, &cache, &tvdb).await;

        for key in ["/tv/Battlestar Galactica", "/tv2/Battlestar Galactica"] {
            let row = series_row(&s, key).await;
            assert_eq!(row.match_external_id.as_deref(), Some("73545"), "{key}");
            assert_eq!(row.match_flag, None, "a clear fit is not flagged");
            assert_eq!(row.match_runner_up_id, None);
        }
        let mut listed = tvdb.listed();
        listed.sort();
        assert_eq!(listed, ["71173", "73545"], "one list per show per pass");
        assert!(!tvdb.fetched().contains(&"71173".to_string()));
        assert_eq!(
            counter_value(
                &snap,
                "pharos_series_match_total",
                &[("outcome", "matched"), ("decided_by", "episode_fit")]
            ),
            2
        );
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("series episode fit"))
            .unwrap_or_else(|| panic!("no episode fit line in {text}"));
        for want in [
            "chosen=\"73545\"",
            "runner_up=\"71173\"",
            "local_episodes=3",
            "titled=3",
            "flagged=false",
            "decided_by=\"episode_fit\"",
            "73545:1.000",
            "71173:0.167",
        ] {
            assert!(line.contains(want), "{want} missing from {line}");
        }
        let decision = text
            .lines()
            .find(|l| l.contains("series match"))
            .unwrap_or_else(|| panic!("no decision line in {text}"));
        assert!(
            decision.contains("decided_by=\"episode_fit\""),
            "{decision}"
        );
    }

    /// B930 — TVDB titles the remake "Battlestar Galactica (2003)" with the
    /// bare name as an alias, so the original wins on name position alone.
    /// The episodes still decide.
    #[tokio::test(flavor = "current_thread")]
    async fn a_remake_matching_by_its_alias_is_decided_by_its_episodes() {
        let s = store().await;
        put_titled_episode(
            &s,
            1,
            "/tv/Battlestar Galactica",
            (4, 1),
            "He That Believeth in Me",
        )
        .await;
        let (_td, cache) = cache();
        let mut tvdb = FakeEnricher::tvdb()
            .with_detail(enriched_overview("A rag-tag fleet."))
            .with_list("71173", &[(1, 1, "Saga of a Star World")])
            .with_list("73545", &[(4, 1, "He That Believeth in Me")]);
        tvdb.search = vec![
            SearchCandidate {
                id: "71173".into(),
                title: "Battlestar Galactica".into(),
                year: Some(1978),
                aliases: Vec::new(),
            },
            SearchCandidate {
                id: "73545".into(),
                title: "Battlestar Galactica (2003)".into(),
                year: Some(2003),
                aliases: vec!["Battlestar Galactica".into()],
            },
        ];

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, "/tv/Battlestar Galactica").await;
        assert_eq!(row.match_external_id.as_deref(), Some("73545"));
        assert_eq!(row.match_flag, None);
    }

    /// V891 — when the episodes fit both shows about as well, the best guess
    /// is still used, but flagged with its runner-up for an admin.
    #[tokio::test(flavor = "current_thread")]
    async fn a_close_episode_fit_is_flagged_with_its_runner_up() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let s = store().await;
        put_titled_episode(&s, 1, "/tv/Battlestar Galactica", (1, 1), "Pilot").await;
        let (_td, cache) = cache();
        let tvdb = bsg_search(FakeEnricher::tvdb())
            .with_list("71173", &[(1, 1, "Pilot")])
            .with_list("73545", &[(1, 1, "Pilot")]);

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, "/tv/Battlestar Galactica").await;
        assert_eq!(row.match_external_id.as_deref(), Some("71173"));
        assert_eq!(row.match_flag.as_deref(), Some(AMBIGUOUS_FLAG));
        assert_eq!(row.match_runner_up_id.as_deref(), Some("73545"));
        assert_eq!(row.match_runner_up_score, Some(1.0));
        assert_eq!(
            counter_value(
                &snap,
                "pharos_series_match_total",
                &[("outcome", "ambiguous"), ("decided_by", "episode_fit")]
            ),
            1
        );
    }

    /// V890 — `tvshow.nfo` is one vote: it decides between even fits, and the
    /// call stays flagged because the episodes did not decide it.
    #[tokio::test(flavor = "current_thread")]
    async fn the_show_nfo_votes_between_even_episode_fits() {
        let show = TempDir::new().unwrap();
        let folder = show.path().join("Battlestar Galactica");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("tvshow.nfo"),
            r#"<tvshow><title>Battlestar Galactica</title><uniqueid type="tvdb">73545</uniqueid></tvshow>"#,
        )
        .unwrap();
        let folder = folder.to_string_lossy().into_owned();
        let s = store().await;
        put_titled_episode(&s, 1, &folder, (1, 1), "Pilot").await;
        let (_td, cache) = cache();
        let tvdb = bsg_search(FakeEnricher::tvdb())
            .with_list("71173", &[(1, 1, "Pilot")])
            .with_list("73545", &[(1, 1, "Pilot")]);
        let (logs, _log) = log_capture();

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, &folder).await;
        assert_eq!(row.match_external_id.as_deref(), Some("73545"));
        assert_eq!(row.match_flag.as_deref(), Some(AMBIGUOUS_FLAG));
        assert_eq!(row.match_runner_up_id.as_deref(), Some("71173"));
        let text = logs.text();
        assert!(
            text.lines().any(|l| l.contains("series episode fit")
                && l.contains("nfo_vote=\"73545\"")
                && l.contains("decided_by=\"nfo_vote\"")),
            "{text}"
        );
    }

    /// V891 — more same-named shows than were scored: the best of the scored
    /// three is used and flagged, since an unscored one may fit better.
    #[tokio::test(flavor = "current_thread")]
    async fn more_same_named_shows_than_scored_is_flagged() {
        let s = store().await;
        put_titled_episode(&s, 1, "/tv/Cosmos", (1, 1), "Shores").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![
                ("1", "Cosmos", None),
                ("2", "Cosmos", None),
                ("3", "Cosmos", None),
                ("4", "Cosmos", None),
            ])
            .with_detail(enriched_overview("Space."))
            .with_list("1", &[(2, 1, "x")])
            .with_list("2", &[(1, 1, "Shores")])
            .with_list("3", &[(3, 1, "y")])
            .with_list("4", &[(1, 1, "Shores")]);

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, "/tv/Cosmos").await;
        assert_eq!(row.match_external_id.as_deref(), Some("2"));
        assert_eq!(row.match_flag.as_deref(), Some(AMBIGUOUS_FLAG));
        assert!(
            !tvdb.listed().contains(&"4".to_string()),
            "only three scored"
        );
    }

    /// A list the provider cannot give leaves the provider's choice in place,
    /// flagged rather than silently trusted.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unavailable_episode_list_keeps_the_choice_and_flags_it() {
        let s = store().await;
        put_titled_episode(&s, 1, "/tv/Battlestar Galactica", (1, 1), "33").await;
        let (_td, cache) = cache();
        let tvdb = bsg_search(FakeEnricher::tvdb()).with_list("73545", &[(1, 1, "33")]);

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, "/tv/Battlestar Galactica").await;
        assert_eq!(row.match_external_id.as_deref(), Some("71173"));
        assert_eq!(row.match_flag.as_deref(), Some(AMBIGUOUS_FLAG));
        assert_eq!(row.match_runner_up_id.as_deref(), Some("73545"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_show_with_no_candidate_is_counted_as_no_match() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let s = store().await;
        put_episode(&s, 1, "Obscure Show", "/tv/Obscure Show").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb().with_search(vec![("1", "Something Else", None)]);
        let tmdb = FakeEnricher::tmdb();
        enrich_series_pass(
            &s,
            &sem(4),
            &cache,
            Some(&tvdb),
            Some(&tmdb),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(
            counter_value(
                &snap,
                "pharos_series_match_total",
                &[("outcome", "no_match"), ("decided_by", "none")]
            ),
            1,
            "two providers asked, one show counted"
        );
    }

    async fn pin_series(store: &SqliteStore, key: &str, id: &str) {
        store
            .upsert_series_metadata(SeriesMetadata {
                series_key: key.into(),
                series_name: key.rsplit('/').next().unwrap_or(key).into(),
                match_provider: Some("tvdb".into()),
                match_external_id: Some(id.into()),
                match_source: Some("search".into()),
                metadata_refreshed_at: Some(NOW),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    /// A Cosmos (1980) episode whose NFO id's record belongs to show
    /// `record_series`; the folder is pinned to the 1980 show (74995).
    async fn cosmos_episode_with_record_of(
        record_series: &str,
    ) -> (u64, LogBuf, MediaItem, Vec<String>) {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();
        let s = store().await;
        pin_series(&s, "/tv/Cosmos", "74995").await;
        let mut item = MediaItem {
            id: 7,
            path: "/tv/Cosmos/Season 01/Cosmos - S01E01.mkv".into(),
            title: "Standing Up in the Milky Way".into(),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "Cosmos".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some("/tv/Cosmos".into()),
                series_year: None,
                series_key: None,
            }),
            ..MediaItem::default()
        };
        item.metadata.overview = Some("The 2014 premiere.".into());
        // A Jellyfin episode NFO's own TVDB id, of the remake's episode.
        item.metadata.provider_ids.tvdb = Some("4777210".into());
        item.metadata.provider_ids.imdb = Some("tt2395695".into());
        s.put(item.clone()).await.unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_detail_for(
                "4777210",
                EnrichedMetadata {
                    title: Some("Standing Up in the Milky Way".into()),
                    overview: Some("The 2014 premiere.".into()),
                    series_id: Some(record_series.into()),
                    ..EnrichedMetadata::default()
                },
            )
            .with_detail_for(
                "74995",
                EnrichedMetadata {
                    title: Some("The Shores of the Cosmic Ocean".into()),
                    overview: Some("Sagan's 1980 premiere.".into()),
                    episode_number: Some((1, 1)),
                    series_id: Some("74995".into()),
                    ..EnrichedMetadata::default()
                },
            );
        enrich_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap();
        let n = counter_value(
            &snap,
            "pharos_episode_metadata_rejected_total",
            &[("reason", "nfo_other_series")],
        );
        (n, logs, s.get(7).await.unwrap(), tvdb.fetched())
    }

    /// Spec 033 V892 (B931) — the Cosmos (1980) folder is matched to the 1980
    /// show, but its episode NFOs carry the 2014 remake's ids. The NFO's
    /// record is set aside with its text and ids, the episode is read from
    /// the pinned show by number, and the rejection is counted and logged.
    #[tokio::test(flavor = "current_thread")]
    async fn an_episode_nfo_of_another_series_is_set_aside() {
        let (n, logs, ep, fetched) = cosmos_episode_with_record_of("260586").await;
        assert_eq!(n, 1);
        assert_eq!(fetched, vec!["4777210", "74995"]);
        assert_eq!(ep.title, "The Shores of the Cosmic Ocean");
        assert_eq!(
            ep.metadata.overview.as_deref(),
            Some("Sagan's 1980 premiere.")
        );
        assert_eq!(ep.metadata.provider_ids.imdb, None, "the remake's ids go");
        assert_ne!(ep.metadata.provider_ids.tvdb.as_deref(), Some("4777210"));
        assert_eq!(ep.match_source.as_deref(), Some("search"));
        assert_eq!(ep.match_external_id.as_deref(), Some("74995"));
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("names another series"))
            .unwrap_or_else(|| panic!("no rejection line in {text}"));
        for want in [
            "record_series_id=\"260586\"",
            "pinned_series_id=74995",
            "nfo_id=4777210",
            "reason=\"nfo_other_series\"",
            "Cosmos - S01E01.mkv",
        ] {
            assert!(line.contains(want), "{want} missing from {line}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_episode_nfo_of_its_own_show_is_used() {
        let (n, _, ep, fetched) = cosmos_episode_with_record_of("74995").await;
        assert_eq!(n, 0);
        assert_eq!(fetched, vec!["4777210"]);
        assert_eq!(ep.match_source.as_deref(), Some("nfo_id"));
        assert_eq!(ep.metadata.provider_ids.tvdb.as_deref(), Some("4777210"));
    }

    /// V892 (B933) — an episode never searches on its own: it is read from
    /// its show's pinned match by number, whatever a search would say.
    #[tokio::test]
    async fn an_episode_is_read_from_its_shows_pinned_match_not_searched() {
        let s = store().await;
        put_bare_episode(&s, 41, "Dune: Prophecy", "/tv/Dune Prophecy").await;
        pin_series(&s, "/tv/Dune Prophecy", "432109").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![("999", "Les Rivières pourpres", None)])
            .with_detail(EnrichedMetadata {
                title: Some("Twice Born".into()),
                series_id: Some("432109".into()),
                ..EnrichedMetadata::default()
            });
        let item = s.get(41).await.unwrap();
        assert!(enrich_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap());
        assert_eq!(tvdb.fetched(), vec!["432109"]);
        let ep = s.get(41).await.unwrap();
        assert_eq!(ep.match_external_id.as_deref(), Some("432109"));
    }

    /// V892 — a show pinned on TMDB (TVDB had no match) has its episodes read
    /// from TMDB even when TVDB is configured.
    #[tokio::test]
    async fn an_episode_uses_its_shows_pinned_provider() {
        let s = store().await;
        put_bare_episode(&s, 42, "Death Note", "/tv/Death Note").await;
        s.upsert_series_metadata(SeriesMetadata {
            series_key: "/tv/Death Note".into(),
            series_name: "Death Note".into(),
            match_provider: Some("tmdb".into()),
            match_external_id: Some("13916".into()),
            match_source: Some("search".into()),
            metadata_refreshed_at: Some(NOW),
            ..Default::default()
        })
        .await
        .unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb().with_detail(EnrichedMetadata::default());
        let tmdb = FakeEnricher::tmdb().with_detail(EnrichedMetadata {
            title: Some("Rebirth".into()),
            series_id: Some("13916".into()),
            ..EnrichedMetadata::default()
        });
        let item = s.get(42).await.unwrap();
        enrich_one(
            &s,
            &sem(4),
            &cache,
            Some(&tmdb),
            Some(&tvdb),
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap();
        assert!(tvdb.fetched().is_empty());
        assert_eq!(tmdb.fetched(), vec!["13916"]);
        let ep = s.get(42).await.unwrap();
        assert_eq!(ep.match_provider.as_deref(), Some("tmdb"));
    }

    /// V892 — a show matched to nothing has its episodes matched to nothing,
    /// without a provider call.
    #[tokio::test]
    async fn an_episode_of_an_unmatched_show_is_matched_to_nothing() {
        let s = store().await;
        put_bare_episode(&s, 43, "Home Movies", "/tv/Home Movies").await;
        s.upsert_series_metadata(SeriesMetadata {
            series_key: "/tv/Home Movies".into(),
            series_name: "Home Movies".into(),
            match_provider: Some("tvdb".into()),
            match_source: Some("none".into()),
            metadata_refreshed_at: Some(NOW),
            ..Default::default()
        })
        .await
        .unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb().with_search(vec![("1", "Home Movies", None)]);
        let item = s.get(43).await.unwrap();
        assert!(!enrich_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap());
        assert!(tvdb.fetched().is_empty());
        assert_eq!(
            s.get(43).await.unwrap().match_source.as_deref(),
            Some("none")
        );
    }

    /// V892 — an episode waits for its show: it is not eligible until the
    /// show has a match row, and then it is.
    #[tokio::test]
    async fn an_episode_waits_until_its_show_is_matched() {
        let s = store().await;
        put_bare_episode(&s, 44, "Cosmos", "/tv/Cosmos").await;
        let eligible = |items: Vec<MediaItem>| items.iter().any(|i| i.id == 44);
        assert!(!eligible(s.items_needing_match(100, NOW).await.unwrap()));
        pin_series(&s, "/tv/Cosmos", "74995").await;
        assert!(eligible(s.items_needing_match(100, NOW).await.unwrap()));
    }

    /// V892 — a file whose episode title is neither its record's nor a
    /// neighbour's in the pinned show is counted and logged; the number
    /// still decides.
    #[tokio::test(flavor = "current_thread")]
    async fn an_episode_titled_unlike_its_record_is_counted() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();
        let s = store().await;
        let item = MediaItem {
            id: 45,
            path: "/tv/The Flash/Season 01/The Flash - S01E01 - Pilot.mkv".into(),
            title: "Pilot".into(),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "The Flash".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some("/tv/The Flash".into()),
                series_year: None,
                series_key: None,
            }),
            ..MediaItem::default()
        };
        s.put(item).await.unwrap();
        pin_series(&s, "/tv/The Flash", "279121").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb().with_detail(EnrichedMetadata {
            title: Some("City of Heroes".into()),
            neighbour_titles: vec!["Fastest Man Alive".into()],
            series_id: Some("279121".into()),
            ..EnrichedMetadata::default()
        });
        let item = s.get(45).await.unwrap();
        enrich_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &MetadataConfig::default(),
            item,
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(
            counter_value(
                &snap,
                "pharos_episode_metadata_rejected_total",
                &[("reason", "title_mismatch")]
            ),
            1
        );
        assert!(logs.text().contains("file_title=Pilot"), "{}", logs.text());
    }
}
