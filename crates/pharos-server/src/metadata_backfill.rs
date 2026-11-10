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
    log_series_match, record_episode_rejected, record_rematch, record_series_match,
    EpisodeRejectReason, RematchStep, SeriesMatchOutcome,
};
use crate::state::Stores;
use crate::tmdb::TmdbEnricher;
use crate::tvdb::{ReqwestTransport, TvdbEnricher};
use pharos_cache::image_cache::ImageRole;
use pharos_cache::ImageCache;
use pharos_core::{
    match_decide, ArtworkRole, DomainResult, GenreStore, LocalizedText, MatchDecision,
    MatchOutcome, MediaItem, MediaKind, MediaStore, MetadataTranslationStore, PersonStore,
    PreferenceStore, ProviderIds, ScoredCandidate, SeriesMatchCandidate, SeriesMetadata,
    SeriesMetadataStore, TranslationGap, TranslationOwner,
};
use pharos_scanner::metadata::nfo::ShowNfoEvidence;
use pharos_scanner::series_split::RepairMode;
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
        let providers = SeriesProviders { tvdb, tmdb };
        if let Err(e) = readmit_stale_tv_matches(store, &providers, bg_io, cfg, now).await {
            tracing::warn!(error = %e, "T9 metadata backfill: show matcher version check failed");
        }
    }
    let _tvdb_pass = PassScope::open(tvdb);
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

    // V940 — episodes whose show's match changed are rebuilt against the new
    // match before the item pass, which would only fill their empty fields.
    if tvdb.is_some() || tmdb.is_some() {
        match reconcile_pass(store, bg_io, cache, tmdb, tvdb, cfg, now).await {
            Ok((n, pending)) => {
                series_enriched += n;
                series_work |= pending;
            }
            Err(e) => tracing::warn!(error = %e, "series identity repair: reconcile pass aborted"),
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

/// B989 — holds a provider's per-pass state open for exactly one backfill
/// pass: begun on creation, ended on drop, so every exit from the pass (an
/// early `?` included) drops what the provider kept.
struct PassScope<'a, E: OnlineEnricher>(Option<&'a E>);

impl<'a, E: OnlineEnricher> PassScope<'a, E> {
    fn open(provider: Option<&'a E>) -> Self {
        if let Some(p) = provider {
            p.begin_pass();
        }
        Self(provider)
    }
}

impl<E: OnlineEnricher> Drop for PassScope<'_, E> {
    fn drop(&mut self) {
        if let Some(p) = self.0 {
            p.end_pass();
        }
    }
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
    /// `best` is the highest-scoring candidate that fell under the floor, or
    /// `None` when the provider returned nothing (or nothing was searched).
    NoMatch { best: Option<ScoredCandidate> },
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
                    (provider, Resolved::NoMatch { best: None })
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
        Resolved::NoMatch { best } => {
            // The verdict with its inputs: what was searched and the nearest
            // miss, so a `none` row can be explained from the log alone.
            tracing::info!(
                media.id = item.id,
                kind = kind_label(item.kind),
                provider = matched_provider,
                searched_title = title.as_str(),
                searched_year = year,
                best_title = best.as_ref().map(|c| c.title.as_str()),
                best_year = best.as_ref().and_then(|c| c.year),
                best_score = best.as_ref().map(|c| c.score),
                "T9 metadata backfill: no confident match"
            );
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
    // B1181 — a local row blocks online art only while its file exists (V1141).
    // A sidecar that was deleted is no curated art, so the provider fills the
    // role. The stale-art sweep removes such rows; this keeps the gap closed
    // until it runs.
    // When the media file cannot be seen either the share is probably down,
    // and a missing sidecar proves nothing: keep treating it as curated.
    // Both stats run only for a local row, and the media one only once a
    // sidecar is found missing, so an item with provider art costs nothing.
    let mut media_visible: Option<bool> = None;
    let mut local_roles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (role, source, locator) in store.artwork_for(item.id).await? {
        if !source.eq_ignore_ascii_case("local") {
            continue;
        }
        let gone = matches!(tokio::fs::try_exists(&locator).await, Ok(false));
        let curated = !gone || {
            let visible = match media_visible {
                Some(v) => v,
                None => {
                    let v = matches!(tokio::fs::try_exists(&item.path).await, Ok(true));
                    media_visible = Some(v);
                    v
                }
            };
            !visible
        };
        if curated {
            local_roles.insert(role.to_ascii_lowercase());
        }
    }
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

/// The provider and id a stored show match names, if it names one.
fn stored_match(m: &SeriesMetadata) -> Option<(&str, &str)> {
    if m.match_source.as_deref() == Some("none") {
        return None;
    }
    Some((
        m.match_provider.as_deref()?,
        m.match_external_id.as_deref()?,
    ))
}

/// Spec 033 (V940) — before a show's stored match changes automatically,
/// durably mark its episodes for reconciliation against the new one. Marked
/// first, so a crash between the two writes leaves them marked rather than
/// stranded on the old show's text. A show with no row yet has nothing to
/// reconcile; `new` is `None` for a show matched to nothing.
async fn mark_if_match_changed<S>(
    store: &S,
    series_key: &str,
    new: Option<(&str, &str)>,
    now: i64,
) -> DomainResult<()>
where
    S: SeriesMetadataStore + pharos_core::SeriesIdentityStore,
{
    let key = series_key.to_string();
    let Some(prior) = store
        .series_metadata_by_keys(std::slice::from_ref(&key))
        .await?
        .remove(&key)
    else {
        return Ok(());
    };
    let old = stored_match(&prior);
    if old == new {
        return Ok(());
    }
    let marked = store.mark_series_reconcile(series_key, now).await?;
    metrics::counter!("pharos_series_reconcile_marked_total", "reason" => "match_changed")
        .increment(marked);
    tracing::info!(
        series_key,
        old_provider = old.map(|(p, _)| p),
        old_id = old.map(|(_, id)| id),
        new_provider = new.map(|(p, _)| p),
        new_id = new.map(|(_, id)| id),
        marked,
        "series identity repair: show match changed, episodes marked for reconciliation"
    );
    Ok(())
}

/// Spec 033 (V940) — rebuild the episodes marked for reconciliation against
/// their show's current match: reset their provider-derived fields to local
/// evidence, delete their stored translations, then enrich them from the
/// pinned match. Only under `series_identity_repair = "apply"`; a dry run
/// counts what is pending and changes nothing. A mark clears only once the
/// episode's enrichment finished (a record, or `none`); a transient miss
/// marks it again with `now`, behind the rest, for a later pass. Returns
/// (rebuilt, anything pending).
async fn reconcile_pass<Tm, Tv, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    tmdb: Option<&Tm>,
    tvdb: Option<&Tv>,
    cfg: &MetadataConfig,
    now: i64,
) -> DomainResult<(usize, bool)>
where
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
    S: MediaStore
        + GenreStore
        + PersonStore
        + MetadataTranslationStore
        + SeriesMetadataStore
        + pharos_core::SeriesIdentityStore,
{
    let mode = cfg.series_identity_repair;
    let pending = store.series_reconcile_pending(cfg.max_per_pass).await?;
    #[allow(clippy::cast_precision_loss)]
    metrics::gauge!("pharos_series_reconcile_pending", "mode" => mode.label())
        .set(pending.len() as f64);
    if pending.is_empty() {
        return Ok((0, false));
    }
    if mode == RepairMode::DryRun {
        tracing::debug!(
            pending = pending.len(),
            "series identity repair: episodes would be rebuilt (dry run)"
        );
        return Ok((0, false));
    }
    let mut rebuilt = 0usize;
    for (id, marked_at) in pending {
        match reconcile_one(store, bg_io, cache, tmdb, tvdb, cfg, id, marked_at, now).await {
            Ok(true) => rebuilt += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, media.id = id, "series identity repair: episode rebuild failed")
            }
        }
        tokio::time::sleep(REQUEST_SPACING).await;
    }
    Ok((rebuilt, true))
}

/// What one reconcile attempt came to, recorded as
/// `pharos_series_reconcile_total{outcome}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileOutcome {
    /// Rebuilt from its show's match; the mark cleared.
    Rebuilt,
    /// The attempt did not finish (a transient miss); still marked, behind
    /// the rest.
    Retry,
    /// The episode's show has no match yet; still marked.
    Deferred,
}

impl ReconcileOutcome {
    /// Stable, bounded-cardinality metric label, asserted distinct in a test.
    fn label(self) -> &'static str {
        match self {
            Self::Rebuilt => "rebuilt",
            Self::Retry => "retry",
            Self::Deferred => "deferred",
        }
    }
}

fn record_reconcile(outcome: ReconcileOutcome) {
    metrics::counter!("pharos_series_reconcile_total", "outcome" => outcome.label()).increment(1);
}

/// Rebuild one marked episode. `Ok(true)` when it was rebuilt and its mark
/// cleared.
#[allow(clippy::too_many_arguments)]
async fn reconcile_one<Tm, Tv, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    tmdb: Option<&Tm>,
    tvdb: Option<&Tv>,
    cfg: &MetadataConfig,
    id: pharos_core::MediaId,
    marked_at: i64,
    now: i64,
) -> DomainResult<bool>
where
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
    S: MediaStore
        + GenreStore
        + PersonStore
        + MetadataTranslationStore
        + SeriesMetadataStore
        + pharos_core::SeriesIdentityStore,
{
    let item = match store.get(id).await {
        Ok(item) => item,
        // The row is gone, and its mark with it.
        Err(pharos_core::DomainError::NotFound(_)) => return Ok(false),
        Err(e) => return Err(e),
    };
    // An episode's own manual match is never rebuilt (the store never marks
    // one; this covers an Identify that landed after the mark).
    if item.match_source.as_deref() == Some("manual") {
        store.clear_series_reconcile(id, marked_at).await?;
        return Ok(false);
    }
    // A row the scanner moved to a key with no match row yet waits, behind
    // the rest, until the series pass has matched its new show.
    if item.kind == MediaKind::Episode && pinned_show(store, &item).await?.is_none() {
        store.mark_reconcile_items(&[id], now).await?;
        record_reconcile(ReconcileOutcome::Deferred);
        tracing::debug!(
            media.id = id,
            "series identity repair: episode's show has no match yet, rebuild deferred"
        );
        return Ok(false);
    }
    let old_title = item.title.clone();
    let mut item = tokio::task::spawn_blocking(move || {
        let mut item = item;
        tokio::runtime::Handle::current()
            .block_on(pharos_scanner::fs::reset_to_local_metadata(&mut item));
        item
    })
    .await
    .map_err(|e| pharos_core::DomainError::Backend(format!("local metadata reset: {e}")))?;
    let translations = store
        .delete_translations(&TranslationOwner::Item(id))
        .await?;
    // The old show's stills: the row and the cached bytes behind it, so the
    // image route falls back to a local still or a frame grab until the new
    // match's art is cached.
    let art = store.delete_provider_artwork(id).await?;
    for (role, _) in &art {
        if let Some(role) = ImageRole::from_str_ci(role) {
            if let Err(e) = cache.remove(id, role, item.kind, 0).await {
                tracing::warn!(
                    error = %e,
                    media.id = id,
                    role = ?role,
                    "series identity repair: cached provider art not removed"
                );
            }
        }
    }
    store.put(item.clone()).await?;
    item.match_source = None;
    let hit = enrich_one(store, bg_io, cache, tmdb, tvdb, cfg, item, now).await?;
    if !art.is_empty() {
        store.bump_art_version(id).await?;
    }
    let after = store.get(id).await?;
    let done = hit || matches!(after.match_source.as_deref(), Some("none" | "manual"));
    if done {
        store.clear_series_reconcile(id, marked_at).await?;
        record_reconcile(ReconcileOutcome::Rebuilt);
    } else {
        store.mark_reconcile_items(&[id], now).await?;
        record_reconcile(ReconcileOutcome::Retry);
    }
    tracing::info!(
        media.id = id,
        old_title = %old_title,
        new_title = %after.title,
        match_source = after.match_source.as_deref(),
        translations_deleted = translations,
        art_deleted = art.len(),
        done,
        "series identity repair: episode rebuilt"
    );
    Ok(done)
}

/// Spec 033 §7 (V896) — rebuild `ids` against their show's current match
/// now, whatever `series_identity_repair` says: a hand fix is the admin's
/// resolution, not the scanner's. Each is marked first, so one whose show has
/// no match yet, or whose record did not arrive, stays marked for the
/// background pass. Returns how many were rebuilt.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn rebuild_now<Tm, Tv, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    tmdb: Option<&Tm>,
    tvdb: Option<&Tv>,
    cfg: &MetadataConfig,
    ids: &[pharos_core::MediaId],
    now: i64,
) -> DomainResult<usize>
where
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
    S: MediaStore
        + GenreStore
        + PersonStore
        + MetadataTranslationStore
        + SeriesMetadataStore
        + pharos_core::SeriesIdentityStore,
{
    store.mark_reconcile_items(ids, now).await?;
    let mut rebuilt = 0usize;
    for &id in ids {
        if reconcile_one(store, bg_io, cache, tmdb, tvdb, cfg, id, now, now).await? {
            rebuilt += 1;
        }
    }
    Ok(rebuilt)
}

/// What [`pin_series_by_hand`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandPin {
    /// The match before, as `provider:id`; `None` when there was none.
    pub old: Option<String>,
    pub new: String,
}

/// Spec 033 §7 (V896) — an admin's pick of `series_key`'s show, or with
/// `pick: None` their confirmation of the stored match. Either way the row
/// becomes `match_source = 'manual'` with its ambiguity flag cleared, so
/// `series_needing_match` never re-admits it. A pick fetches the show's
/// record and art and marks its episodes when the match moved. `Ok(None)`
/// when there is nothing to confirm, or the pick's record did not arrive.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pin_series_by_hand<Tm, Tv, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    tmdb: Option<&Tm>,
    tvdb: Option<&Tv>,
    series_key: &str,
    series_name: &str,
    pick: Option<(&str, &str)>,
    now: i64,
) -> DomainResult<Option<HandPin>>
where
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
    S: SeriesMetadataStore + MetadataTranslationStore + pharos_core::SeriesIdentityStore,
{
    let key = series_key.to_string();
    let prior = store
        .series_metadata_by_keys(std::slice::from_ref(&key))
        .await?
        .remove(&key);
    let old = prior
        .as_ref()
        .and_then(stored_match)
        .map(|(p, id)| format!("{p}:{id}"));
    let Some((provider, external_id)) = pick else {
        let Some(mut row) = prior.filter(|m| stored_match(m).is_some()) else {
            return Ok(None);
        };
        let new = old.clone().unwrap_or_default();
        row.match_source = Some("manual".into());
        row.match_flag = None;
        row.match_runner_up_id = None;
        row.match_runner_up_score = None;
        store.upsert_series_metadata(row).await?;
        return Ok(Some(HandPin { old, new }));
    };
    let enriched = match provider {
        "tvdb" => match tvdb {
            Some(e) => hand_pick_record(e, bg_io, cache, series_key, external_id).await,
            None => None,
        },
        "tmdb" => match tmdb {
            Some(e) => hand_pick_record(e, bg_io, cache, series_key, external_id).await,
            None => None,
        },
        _ => None,
    };
    let Some((enriched, poster_locator, backdrop_locator)) = enriched else {
        return Ok(None);
    };
    mark_if_match_changed(store, series_key, Some((provider, external_id)), now).await?;
    let provider_ids = match provider {
        "tmdb" => ProviderIds {
            tmdb: Some(external_id.to_string()),
            ..Default::default()
        },
        _ => ProviderIds {
            tvdb: Some(external_id.to_string()),
            tmdb: enriched.also_tmdb_id.clone(),
            ..Default::default()
        },
    };
    store
        .upsert_series_metadata(SeriesMetadata {
            locked: Default::default(),
            series_key: key,
            series_name: prior
                .as_ref()
                .map_or_else(|| series_name.to_string(), |m| m.series_name.clone()),
            match_provider: Some(provider.into()),
            match_external_id: Some(external_id.into()),
            match_source: Some("manual".into()),
            match_confidence: None,
            metadata_refreshed_at: Some(now),
            overview: enriched.overview.clone(),
            community_rating: enriched.community_rating,
            premiere_date: enriched.premiere_date,
            official_rating: enriched.official_rating.clone(),
            original_language: enriched.original_language.clone(),
            genres: enriched.genres.clone(),
            studios: Vec::new(),
            provider_ids,
            poster_locator,
            backdrop_locator,
            ratings: Default::default(),
            match_flag: None,
            match_runner_up_id: None,
            match_runner_up_score: None,
        })
        .await?;
    store_texts(
        store,
        &TranslationOwner::Series(series_key.to_string()),
        &enriched.texts,
        now,
    )
    .await;
    Ok(Some(HandPin {
        old,
        new: format!("{provider}:{external_id}"),
    }))
}

/// The show record an admin picked, with its poster and backdrop cached.
async fn hand_pick_record<E: OnlineEnricher>(
    enricher: &E,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    series_key: &str,
    external_id: &str,
) -> Option<(EnrichedMetadata, Option<String>, Option<String>)> {
    let enriched = {
        let _permit = BgPermit::acquire(bg_io).await;
        enricher
            .fetch(MediaKind::Episode, external_id, None, None, None)
            .await
    }?;
    let poster = cache_series_art(
        cache,
        enricher,
        bg_io,
        series_key,
        ArtworkRole::Primary,
        ImageRole::Primary,
        &enriched,
    )
    .await;
    let backdrop = cache_series_art(
        cache,
        enricher,
        bg_io,
        series_key,
        ArtworkRole::Backdrop,
        ImageRole::Backdrop,
        &enriched,
    )
    .await;
    Some((enriched, poster, backdrop))
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
        let decision = search_decide(enricher, kind, title, year, bg_io, cfg).await;
        match decision.chosen {
            Some(o) => (o.id, "search", Some(o.confidence)),
            None => {
                let best = decision
                    .candidates
                    .iter()
                    .max_by(|a, b| a.score.total_cmp(&b.score))
                    .cloned();
                return Resolved::NoMatch { best };
            }
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

/// B990 — how far a candidate's year may be from the stated year and still
/// be read as that year, once a search narrowed by the year found nothing: a
/// provider dates a show by its first episode, which may be a pilot a year or
/// two before or after the year the folder or files state.
const STATED_YEAR_SLACK: u32 = 2;

/// Search for a show and decide among its candidates (V891's
/// [`match_decide`]), leaving out `excluded` ids.
///
/// TVDB's `year` and TMDB's `first_air_date_year` keep only shows the
/// provider dates that year, so a show stated as 2003 that the provider dates
/// 2005 is lost (B990). A search narrowed by a year that leaves no candidate
/// is asked again without it; its candidates are ranked nearest year first,
/// and one within [`STATED_YEAR_SLACK`] reads as the stated year. On either
/// search, a candidate whose name is the show's name with that year
/// (`Show (2003)`) reads as that year too, so it is not lost to a bare exact
/// name of another year.
async fn series_search_decide<E: OnlineEnricher>(
    enricher: &E,
    series_key: &str,
    title: &str,
    year: Option<u32>,
    excluded: &[String],
    bg_io: &Arc<Semaphore>,
    cfg: &MetadataConfig,
) -> MatchDecision {
    let search = |y: Option<u32>| async move {
        let _permit = BgPermit::acquire(bg_io).await;
        enricher.search(MediaKind::Episode, title, y).await
    };
    let keep = |c: &pharos_core::SearchCandidate| !excluded.contains(&c.id);
    let mut candidates: Vec<pharos_core::SearchCandidate> =
        search(year).await.into_iter().filter(keep).collect();
    let mut retried = false;
    if let (true, Some(y)) = (candidates.is_empty(), year) {
        candidates = search(None).await.into_iter().filter(keep).collect();
        retried = true;
        let found: Vec<String> = candidates
            .iter()
            .map(|c| match c.year {
                Some(cy) => format!("{} ({cy})", c.id),
                None => format!("{} (no year)", c.id),
            })
            .collect();
        tracing::info!(
            provider = enricher.provider(),
            series_key,
            query = title,
            year = y,
            candidates = %found.join(", "),
            "series match: no candidate with the year, searched again without it"
        );
        record_rematch(RematchStep::YearRetry);
    }
    let (candidates, read) = read_stated_year(title, year, candidates, retried);
    if !read.is_empty() {
        tracing::info!(
            provider = enricher.provider(),
            series_key,
            query = title,
            year,
            retried,
            read = %read.join(", "),
            "series match: candidates read as the stated year"
        );
    }
    match_decide(title, year, &candidates, cfg.match_min_confidence)
}

/// B990 — candidates as [`series_search_decide`] scores them against the
/// stated `year`: one named `"{title} ({year})"` takes that year, and after a
/// retry without the year they are ordered nearest year first (the order is
/// [`match_decide`]'s last tie-break) and one within [`STATED_YEAR_SLACK`]
/// takes it as well. Also returns each candidate whose year it changed, as
/// `id provider_year->year`.
fn read_stated_year(
    title: &str,
    year: Option<u32>,
    mut candidates: Vec<pharos_core::SearchCandidate>,
    retried: bool,
) -> (Vec<pharos_core::SearchCandidate>, Vec<String>) {
    let Some(year) = year else {
        return (candidates, Vec::new());
    };
    let mut read = Vec::new();
    let named = format!("{title} ({year})");
    if retried {
        candidates.sort_by_key(|c| c.year.map_or(u32::MAX, |cy| cy.abs_diff(year)));
    }
    for c in &mut candidates {
        let carries = std::iter::once(&c.title)
            .chain(&c.aliases)
            .any(|n| pharos_core::title_similarity(n, &named) >= 1.0);
        let near = retried
            && c.year
                .is_some_and(|cy| cy.abs_diff(year) <= STATED_YEAR_SLACK);
        if (carries || near) && c.year != Some(year) {
            read.push(match c.year {
                Some(cy) => format!("{} {cy}->{year}", c.id),
                None => format!("{} none->{year}", c.id),
            });
            c.year = Some(year);
        }
    }
    (candidates, read)
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
/// 2 (spec 033, B930): same-named shows told apart by their episodes (V891).
/// Re-asks only what [`survey_series_identity`] selects, never every show.
pub(crate) const TV_MATCH_VERSION: u32 = 2;

/// Key under which the [`TV_MATCH_VERSION`] that produced the stored show and
/// episode search matches is recorded.
const TV_MATCH_VERSION_KEY: &str = "tv_match_version";

/// Key recording the [`dry_run_marker`] whose survey a dry run has already
/// logged, so a dry run surveys once per version and survey revision rather
/// than every pass.
const TV_MATCH_SURVEY_DRY_RUN_KEY: &str = "tv_match_survey_dry_run";

/// Key recording the [`dry_run_marker`] whose survey `apply` has run, so a
/// survey revision bump re-runs it under a matcher version already recorded.
const TV_MATCH_SURVEY_APPLIED_KEY: &str = "tv_match_survey_applied";

/// Bump when [`survey_series_identity`] selects differently, so the next
/// dry run, and `apply` ([`TV_MATCH_SURVEY_APPLIED_KEY`]), surveys again
/// under the same matcher version. r2: B982/B983 run window. r3: B984 date
/// 0, B985 title agreement. r4: B986 run too short. r5: B987 joiners. r6:
/// B990 re-matches with the files' year and without a too-short pin.
const SURVEY_REVISION: u32 = 6;

/// What [`TV_MATCH_SURVEY_DRY_RUN_KEY`] holds once a dry run surveyed `version`.
fn dry_run_marker(version: &str) -> String {
    format!("{version}.r{SURVEY_REVISION}")
}

/// Re-ask stored matches once the matcher that chose them has changed.
///
/// An ABSENT record re-admits every searched show and episode: every search
/// match made before this key existed came from the matcher B557 replaced,
/// which handed 29 of the deployed library's shows to a fan work, an abridged
/// parody or the wrong entry of a franchise. A fresh install has no searched
/// rows, so re-admitting there costs nothing.
///
/// From version 1 on, only what [`survey_series_identity`] selects is asked
/// again (V940), and only under `series_identity_repair = "apply"`, which
/// then records the new version. A dry run logs and counts the selection once
/// per version and leaves the version key alone, so switching to `"apply"`
/// still runs it. `apply` records the survey revision it ran beside the
/// version, and surveys again when [`SURVEY_REVISION`] moves. NFO and manual
/// matches are not touched. One or two config reads per pass once settled.
async fn readmit_stale_tv_matches<Tv, Tm, S>(
    store: &S,
    providers: &SeriesProviders<'_, Tv, Tm>,
    bg_io: &Arc<Semaphore>,
    cfg: &MetadataConfig,
    now: i64,
) -> DomainResult<()>
where
    Tv: OnlineEnricher,
    Tm: OnlineEnricher,
    S: SeriesMetadataStore
        + pharos_core::SeriesIdentityStore
        + pharos_store_sqlx::ServerConfigStore,
{
    let config =
        |e: pharos_store_sqlx::StoreError| pharos_core::DomainError::Backend(e.to_string());
    let want = TV_MATCH_VERSION.to_string();
    let marker = dry_run_marker(&want);
    let mode = cfg.series_identity_repair;
    let have = store
        .load_named_config(TV_MATCH_VERSION_KEY)
        .await
        .map_err(config)?;
    // B990 — under `apply` a recorded version is settled only once this
    // survey revision ran too, so a revision bump re-asks what it now finds.
    if have.as_deref() == Some(want.as_str())
        && (mode == RepairMode::DryRun
            || store
                .load_named_config(TV_MATCH_SURVEY_APPLIED_KEY)
                .await
                .map_err(config)?
                .as_deref()
                == Some(marker.as_str()))
    {
        return Ok(());
    }
    let Some(from) = have else {
        let readmitted = store.readmit_searched_tv().await?;
        tracing::info!(
            readmitted,
            from = "none",
            to = want.as_str(),
            "T9 metadata backfill: show matcher version changed, re-matching searched shows and episodes"
        );
        // Every searched show is asked again, so no survey is owed.
        store
            .set_named_config(TV_MATCH_SURVEY_APPLIED_KEY, &marker)
            .await
            .map_err(config)?;
        return store
            .set_named_config(TV_MATCH_VERSION_KEY, &want)
            .await
            .map_err(config);
    };
    if mode == RepairMode::DryRun
        && store
            .load_named_config(TV_MATCH_SURVEY_DRY_RUN_KEY)
            .await
            .map_err(config)?
            .as_deref()
            == Some(marker.as_str())
    {
        return Ok(());
    }
    let survey = survey_series_identity(store, providers, bg_io, cfg).await?;
    survey.record(mode, &from, &want);
    if mode == RepairMode::DryRun {
        return store
            .set_named_config(TV_MATCH_SURVEY_DRY_RUN_KEY, &marker)
            .await
            .map_err(config);
    }
    let keys: Vec<String> = survey.reask.keys().cloned().collect();
    let readmitted = store.readmit_series(&keys).await?;
    let mut ids: Vec<pharos_core::MediaId> = survey.episodes.iter().map(|(id, _)| *id).collect();
    let mut rows_written = 0usize;
    for split in &survey.title_splits {
        rows_written += write_title_split(store, split, now).await?;
        ids.extend(split.files.iter().map(|f| f.id));
        ids.extend(split.joiners.iter().copied());
    }
    let marked = store.mark_reconcile_items(&ids, now).await?;
    tracing::info!(
        readmitted,
        marked,
        override_rows = rows_written,
        from = from.as_str(),
        to = want.as_str(),
        "series identity repair: show matcher version changed, re-asking the surveyed shows"
    );
    store
        .set_named_config(TV_MATCH_SURVEY_APPLIED_KEY, &marker)
        .await
        .map_err(config)?;
    store
        .set_named_config(TV_MATCH_VERSION_KEY, &want)
        .await
        .map_err(config)
}

/// Why the version-2 survey selected a show to re-ask or an episode to
/// rebuild. A metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RepairReason {
    /// Another show matches the name exactly and no year separates them.
    ExactRival,
    /// The matched show premiered more than [`STATED_YEAR_SLACK`] years
    /// from the stated year.
    YearMismatch,
    /// The folder is split, or a hand fix names it.
    Split,
    /// The episode's own old search matched another show than its series'.
    OtherShow,
    /// The episode aired outside its show's run.
    OutsideRun,
    /// B986 — the matched show's whole run is shorter than the show's local
    /// seasons could span: a miniseries matched for a series.
    RunTooShort,
    /// The unyeared file's episode title is a same-named show's of another
    /// year, so it splits off (V893).
    EpisodeTitles,
}

impl RepairReason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ExactRival => "exact_rival",
            Self::YearMismatch => "year_mismatch",
            Self::Split => "split",
            Self::OtherShow => "other_show",
            Self::OutsideRun => "outside_run",
            Self::RunTooShort => "run_too_short",
            Self::EpisodeTitles => "episode_titles",
        }
    }
}

/// Slack on each side of a show's run before an episode's air date counts as
/// outside it: a special or a pilot can air apart from its season, but not
/// the year or more that separates a show from its remake.
const RUN_SLACK_SECS: i64 = 180 * 86_400;

/// What the version-2 survey selected: shows to re-ask (search matches only)
/// and episodes to rebuild against their show's match.
#[derive(Debug, Default)]
pub(crate) struct Survey {
    pub reask: std::collections::BTreeMap<String, RepairReason>,
    pub episodes: Vec<(pharos_core::MediaId, RepairReason)>,
    /// Folders whose unyeared files belong to a same-named show by their
    /// episode titles: split by `scanner` rows on the next scan, so neither
    /// re-asked nor rebuilt until then.
    pub title_splits: Vec<TitleSplit>,
}

/// Spec 033 §2 (V893) — a folder's unyeared files whose filename episode
/// titles are a same-named show's of another year, not its own year's.
#[derive(Debug)]
pub(crate) struct TitleSplit {
    pub folder: String,
    /// The year whose group keeps the plain key: the one within a year of the
    /// stored match's premiere, if any.
    pub owner_year: Option<u32>,
    pub files: Vec<TitleMove>,
    /// B987 — unyeared files that stay and join an owner of another year
    /// than the folder's files state, while holding another show's episode.
    pub joiners: Vec<pharos_core::MediaId>,
}

/// One unyeared file and the show its episode title names.
#[derive(Debug)]
pub(crate) struct TitleMove {
    pub id: pharos_core::MediaId,
    pub path: String,
    pub year: u32,
}

impl Survey {
    /// The `series identity repair: survey` line and the selection gauges.
    fn record(&self, mode: RepairMode, from: &str, to: &str) {
        let mut counts: std::collections::BTreeMap<(&str, RepairReason), usize> =
            std::collections::BTreeMap::new();
        for reason in self.reask.values() {
            *counts.entry(("show", *reason)).or_default() += 1;
        }
        for (_, reason) in &self.episodes {
            *counts.entry(("episode", *reason)).or_default() += 1;
        }
        for split in &self.title_splits {
            *counts
                .entry(("episode", RepairReason::EpisodeTitles))
                .or_default() += split.files.len();
            *counts.entry(("episode", RepairReason::Split)).or_default() += split.joiners.len();
        }
        for kind in ["show", "episode"] {
            for reason in [
                RepairReason::ExactRival,
                RepairReason::YearMismatch,
                RepairReason::Split,
                RepairReason::OtherShow,
                RepairReason::OutsideRun,
                RepairReason::RunTooShort,
                RepairReason::EpisodeTitles,
            ] {
                let n = counts.get(&(kind, reason)).copied().unwrap_or(0);
                #[allow(clippy::cast_precision_loss)]
                metrics::gauge!(
                    "pharos_series_identity_repair_selected",
                    "kind" => kind,
                    "reason" => reason.label(),
                    "mode" => mode.label(),
                )
                .set(n as f64);
            }
        }
        let reask: Vec<String> = self
            .reask
            .iter()
            .map(|(k, r)| format!("{k}:{}", r.label()))
            .collect();
        tracing::info!(
            mode = mode.label(),
            from,
            to,
            shows = self.reask.len(),
            episodes = self.episodes.len(),
            title_splits = self.title_splits.len(),
            reask = ?reask,
            "series identity repair: survey"
        );
    }
}

/// Spec 033 §6 (V940) — the stored matches with conflicting evidence. A
/// searched show is re-asked when its folder is split or named by a hand fix,
/// when its match premiered more than two years from its stated year, or when a
/// fresh search finds another show of the exact name no year separates. An
/// episode is rebuilt when its own old search matched another show than its
/// series' pinned one, or when it aired outside that show's run (by the
/// show's episode list). Manual shows are left alone. One search per searched
/// show and one episode list per matched show with dated episodes.
async fn survey_series_identity<Tv, Tm, S>(
    store: &S,
    providers: &SeriesProviders<'_, Tv, Tm>,
    bg_io: &Arc<Semaphore>,
    cfg: &MetadataConfig,
) -> DomainResult<Survey>
where
    Tv: OnlineEnricher,
    Tm: OnlineEnricher,
    S: SeriesMetadataStore + pharos_core::SeriesIdentityStore,
{
    let shows = store.series_needing_match(i64::MAX, i64::MAX).await?;
    let keys: Vec<String> = shows.iter().map(|c| c.series_key.clone()).collect();
    let stored = store.series_metadata_by_keys(&keys).await?;
    let mut survey = Survey::default();
    for cand in &shows {
        let Some(m) = stored.get(&cand.series_key) else {
            continue;
        };
        let Some((provider, show_id)) = stored_match(m) else {
            continue;
        };
        if m.match_source.as_deref() != Some("manual") {
            if let Some(split) =
                title_evidence(store, providers, bg_io, cfg, cand, m, provider).await?
            {
                tracing::info!(
                    folder = %split.folder,
                    files = split.files.len(),
                    owner_year = split.owner_year,
                    "series identity repair: split pending on episode titles, not re-asked"
                );
                survey.title_splits.push(split);
                tokio::time::sleep(REQUEST_SPACING).await;
                continue;
            }
        }
        let rows = store.series_episodes(&cand.series_key).await?;
        if m.match_source.as_deref() == Some("search") {
            let stated = stated_year(cand, &rows);
            let reason =
                show_conflict(store, providers, bg_io, cfg, cand, stated, m, provider).await?;
            if let Some((reason, candidates)) = reason {
                tracing::info!(
                    series_key = %cand.series_key,
                    provider,
                    match_id = show_id,
                    reason = reason.label(),
                    candidates = %candidates,
                    "series identity repair: re-ask"
                );
                survey.reask.insert(cand.series_key.clone(), reason);
            }
        }
        let mut picked = 0usize;
        let mut window = None;
        for row in &rows {
            let own_search = row.match_source.as_deref() == Some("search")
                && row.match_provider.as_deref() == Some(provider);
            if own_search && row.match_external_id.as_deref() != Some(show_id) {
                survey.episodes.push((row.id, RepairReason::OtherShow));
                picked += 1;
            }
        }
        if rows.iter().any(|r| aired(r).is_some()) {
            let seasons: Vec<u32> = rows
                .iter()
                .filter_map(|r| r.season)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let (listing, series_run) = match provider {
                "tvdb" => match providers.tvdb {
                    Some(t) => show_run(t, bg_io, show_id, &seasons).await,
                    None => (None, None),
                },
                "tmdb" => match providers.tmdb {
                    Some(t) => show_run(t, bg_io, show_id, &seasons).await,
                    None => (None, None),
                },
                _ => (None, None),
            };
            window = run_window(listing.iter().flatten().filter_map(|l| l.aired), series_run);
            let too_short = m.match_source.as_deref() == Some("search")
                && window.is_some_and(|w| run_too_short(w, &rows));
            if too_short && !survey.reask.contains_key(&cand.series_key) {
                tracing::info!(
                    series_key = %cand.series_key,
                    provider,
                    match_id = show_id,
                    reason = RepairReason::RunTooShort.label(),
                    run_first = window.map(|(first, _)| first),
                    run_last = window.and_then(|(_, last)| last),
                    seasons = regular_seasons(&rows),
                    candidates = "",
                    "series identity repair: re-ask"
                );
                survey
                    .reask
                    .insert(cand.series_key.clone(), RepairReason::RunTooShort);
            }
            // A re-asked show's episodes wait for its new match: the pin's
            // run is no measure of them (B986).
            let window = window.filter(|_| !too_short);
            if let Some((first, last)) = window {
                let mut agreed: Vec<&str> = Vec::new();
                for row in &rows {
                    let outside = aired(row).is_some_and(|d| {
                        d < first - RUN_SLACK_SECS
                            || last.is_some_and(|last| d > last + RUN_SLACK_SECS)
                    });
                    let taken = survey.episodes.iter().any(|(id, _)| *id == row.id);
                    if !outside || taken {
                        continue;
                    }
                    if file_title_agrees(row) {
                        agreed.push(&row.path);
                    } else {
                        survey.episodes.push((row.id, RepairReason::OutsideRun));
                        picked += 1;
                    }
                }
                if let Some(sample) = agreed.first() {
                    tracing::info!(
                        series_key = %cand.series_key,
                        provider,
                        match_id = show_id,
                        episodes = agreed.len(),
                        sample = %sample,
                        "series identity repair: outside the run, titles agree, not marked"
                    );
                }
            }
        }
        if picked > 0 {
            tracing::info!(
                series_key = %cand.series_key,
                provider,
                match_id = show_id,
                episodes = picked,
                run_first = window.map(|(first, _)| first),
                run_last = window.and_then(|(_, last)| last),
                "series identity repair: episodes to rebuild"
            );
        }
        tokio::time::sleep(REQUEST_SPACING).await;
    }
    Ok(survey)
}

/// B984 — a row's stored air date, with unix second 0 read as none: rows
/// imported before the NFO parser dropped the `1970-01-01` placeholder still
/// hold it until their next probe.
fn aired(row: &pharos_core::SeriesEpisodeRow) -> Option<i64> {
    row.premiere_date.filter(|&d| d != 0)
}

/// B986 — a run this short is one event (a film, a miniseries, a special),
/// never a show with seasons apart.
const SHORT_RUN_SECS: i64 = 30 * 86_400;

/// The show's distinct local seasons, specials (season 0) aside.
fn regular_seasons(rows: &[pharos_core::SeriesEpisodeRow]) -> usize {
    rows.iter()
        .filter_map(|r| r.season.filter(|&s| s > 0))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// B986 — whether a show with two or more local seasons is matched to a show
/// that ran under [`SHORT_RUN_SECS`] from first to last air date.
fn run_too_short(
    (first, last): (i64, Option<i64>),
    rows: &[pharos_core::SeriesEpisodeRow],
) -> bool {
    last.is_some_and(|last| last - first < SHORT_RUN_SECS) && regular_seasons(rows) >= 2
}

/// B985 — whether the episode title the file names is the one stored for
/// it: then the episode is right and only its date is odd. A file naming no
/// title cannot vouch for its row, nor can a stored title the local resolver
/// took from the file name itself (`Show - S03E33 - …`): it would agree with
/// any file, right or wrong show.
fn file_title_agrees(row: &pharos_core::SeriesEpisodeRow) -> bool {
    use pharos_scanner::metadata::filename::episode_title;
    if episode_title(&row.title).is_some() {
        return false;
    }
    let stored = &row.title;
    std::path::Path::new(&row.path)
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(episode_title)
        .is_some_and(|file| {
            pharos_core::title_similarity(&file, stored) >= crate::episode_fit::SAME_TITLE
        })
}

/// B982 — the show's episode listing and its series-level run.
async fn show_run<E: OnlineEnricher>(
    enricher: &E,
    bg_io: &Arc<Semaphore>,
    show_id: &str,
    seasons: &[u32],
) -> (
    Option<Vec<EpisodeListing>>,
    Option<crate::online_enrich::SeriesRun>,
) {
    let listing = list_episodes(enricher, bg_io, show_id, seasons).await;
    let _permit = BgPermit::acquire(bg_io).await;
    (listing, enricher.series_run(show_id).await)
}

/// B982 — a show's run as `(first, last)` air dates: the span of its listed
/// episodes widened to its series-level run, with no end while it is still
/// airing. A listing covers only the seasons asked for, so an
/// absolute-numbered or renumbered episode falls outside it.
fn run_window(
    listed: impl Iterator<Item = i64> + Clone,
    series: Option<crate::online_enrich::SeriesRun>,
) -> Option<(i64, Option<i64>)> {
    let series = series.unwrap_or_default();
    let first = listed.clone().min().into_iter().chain(series.first).min()?;
    let last = listed.max().into_iter().chain(series.last).max();
    Some((first, if series.ongoing { None } else { last }))
}

async fn list_episodes<E: OnlineEnricher>(
    enricher: &E,
    bg_io: &Arc<Semaphore>,
    show_id: &str,
    seasons: &[u32],
) -> Option<Vec<EpisodeListing>> {
    let _permit = BgPermit::acquire(bg_io).await;
    enricher.episode_list(show_id, seasons).await
}

/// Spec 033 §2 (V893) — record a [`TitleSplit`] as `scanner` override rows:
/// one per moved file with its show's year, and the folder-wide owner row,
/// so a later change of the folder's match cannot hand its plain key to the
/// other show. Existing rows are kept. Returns the rows inserted.
async fn write_title_split<S: pharos_core::SeriesIdentityStore>(
    store: &S,
    split: &TitleSplit,
    now: i64,
) -> DomainResult<usize> {
    use pharos_core::{OverrideSource, SeriesOverride};
    let row = |path: Option<String>, target_key: String, year: u32| SeriesOverride {
        id: 0,
        series_folder: split.folder.clone(),
        path,
        target_key,
        target_name: None,
        target_year: Some(year),
        source: OverrideSource::Scanner,
        created_by: None,
        created_at: now,
    };
    let mut written = 0;
    if let Some(year) = split.owner_year {
        written += usize::from(
            store
                .insert_series_override(row(None, split.folder.clone(), year))
                .await?,
        );
    }
    for f in &split.files {
        let key = pharos_scanner::series_split::split_key(&split.folder, f.year);
        written += usize::from(
            store
                .insert_series_override(row(Some(f.path.clone()), key, f.year))
                .await?,
        );
    }
    Ok(written)
}

/// Spec 033 §2 (V893) — a folder whose files state one show year but also
/// hold unyeared files with episode titles: each such title is compared with
/// the stated year's show and with the exact-name shows of other years at
/// the same season and episode. A file whose title names a rival's episode
/// (similarity ≥ 0.8) and not its own year's show's belongs to that rival.
/// Only these folders cost a search and the lists of the shows compared.
async fn title_evidence<Tv, Tm, S>(
    store: &S,
    providers: &SeriesProviders<'_, Tv, Tm>,
    bg_io: &Arc<Semaphore>,
    cfg: &MetadataConfig,
    cand: &SeriesMatchCandidate,
    stored: &SeriesMetadata,
    provider: &str,
) -> DomainResult<Option<TitleSplit>>
where
    Tv: OnlineEnricher,
    Tm: OnlineEnricher,
    S: pharos_core::SeriesIdentityStore,
{
    let folder = cand.series_key.clone();
    if folder.contains(pharos_scanner::series_split::SPLIT_KEY_SEPARATOR) {
        return Ok(None);
    }
    let rows = store.series_episodes(&folder).await?;
    let mut years = std::collections::BTreeSet::new();
    let mut unyeared = Vec::new();
    for row in &rows {
        let path = std::path::Path::new(&row.path);
        match pharos_scanner::series_split::stated_show_year(path) {
            Some(y) => {
                years.insert(y);
            }
            None => {
                let title = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(pharos_scanner::metadata::filename::episode_title);
                if let (Some(season), Some(episode), Some(title)) = (row.season, row.episode, title)
                {
                    unyeared.push((row, season, episode, title));
                }
            }
        }
    }
    let stated = match (years.len(), years.first()) {
        (1, Some(&y)) => y,
        _ => return Ok(None),
    };
    if unyeared.is_empty() {
        return Ok(None);
    }
    match provider {
        "tvdb" => match providers.tvdb {
            Some(t) => title_moves(t, provider, bg_io, cfg, cand, stored, stated, &unyeared).await,
            None => Ok(None),
        },
        "tmdb" => match providers.tmdb {
            Some(t) => title_moves(t, provider, bg_io, cfg, cand, stored, stated, &unyeared).await,
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn title_moves<E: OnlineEnricher>(
    enricher: &E,
    provider: &str,
    bg_io: &Arc<Semaphore>,
    cfg: &MetadataConfig,
    cand: &SeriesMatchCandidate,
    stored: &SeriesMetadata,
    stated: u32,
    unyeared: &[(&pharos_core::SeriesEpisodeRow, u32, u32, String)],
) -> DomainResult<Option<TitleSplit>> {
    let decision = search_decide(
        enricher,
        MediaKind::Episode,
        &cand.series_name,
        None,
        bg_io,
        cfg,
    )
    .await;
    let exact: Vec<&pharos_core::ScoredCandidate> =
        decision.candidates.iter().filter(|c| c.exact).collect();
    let Some(own) = exact
        .iter()
        .filter(|c| c.year.is_some_and(|y| y.abs_diff(stated) <= 1))
        .min_by_key(|c| c.year.map(|y| y.abs_diff(stated)))
    else {
        return Ok(None);
    };
    let rivals: Vec<(&str, u32)> = exact
        .iter()
        .filter_map(|c| Some((c.id.as_str(), c.year.filter(|y| y.abs_diff(stated) > 1)?)))
        .take(episode_fit::MAX_SCORED)
        .collect();
    if rivals.is_empty() {
        return Ok(None);
    }
    let seasons: Vec<u32> = unyeared
        .iter()
        .map(|(_, s, _, _)| *s)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let Some(own_list) = list_episodes(enricher, bg_io, &own.id, &seasons).await else {
        tracing::info!(
            series_key = %cand.series_key,
            provider,
            show_id = %own.id,
            "series identity repair: episode list unavailable, folder not split on episode titles"
        );
        return Ok(None);
    };
    let mut rival_lists = Vec::new();
    for (id, year) in &rivals {
        if let Some(list) = list_episodes(enricher, bg_io, id, &seasons).await {
            rival_lists.push((*id, *year, list));
        }
    }
    let listed = |list: &[EpisodeListing], s: u32, e: u32| {
        list.iter()
            .find(|l| l.season == s && l.episode == e)
            .and_then(|l| l.title.clone())
    };
    let sim = |a: &str, b: Option<&str>| b.map_or(0.0, |b| pharos_core::title_similarity(a, b));
    let mut files = Vec::new();
    let mut stays = Vec::new();
    for (row, s, e, title) in unyeared {
        let own_title = listed(&own_list, *s, *e);
        let own_score = sim(title, own_title.as_deref());
        if own_score >= episode_fit::SAME_TITLE {
            stays.push((*row, *s, *e));
            continue;
        }
        let best = rival_lists
            .iter()
            .filter_map(|(id, year, list)| {
                let t = listed(list, *s, *e);
                let score = sim(title, t.as_deref());
                (score >= episode_fit::SAME_TITLE).then_some((*id, *year, t, score))
            })
            .max_by(|a, b| a.3.total_cmp(&b.3));
        let Some((rival_id, year, rival_title, rival_score)) = best else {
            stays.push((*row, *s, *e));
            continue;
        };
        tracing::info!(
            path = %row.path,
            season = s,
            episode = e,
            title = %title,
            own_id = %own.id,
            own_title = own_title.as_deref(),
            own_score,
            rival_id,
            rival_year = year,
            rival_title = rival_title.as_deref(),
            rival_score,
            "series identity repair: file's episode title names a same-named show"
        );
        files.push(TitleMove {
            id: row.id,
            path: row.path.clone(),
            year,
        });
    }
    if files.is_empty() {
        return Ok(None);
    }
    let premiered = stored.premiere_date.and_then(pharos_store_sqlx::unix_year);
    let owner_year = premiered.and_then(|p| {
        std::iter::once(stated)
            .chain(files.iter().map(|f| f.year))
            .filter(|y| y.abs_diff(p) <= 1)
            .min_by_key(|y| y.abs_diff(p))
    });
    let joiners = match owner_year {
        Some(owner) if owner != stated => rival_lists
            .iter()
            .find(|(_, year, _)| *year == owner)
            .map(|(id, year, list)| owner_joiners(cand, id, *year, list, &stays))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    Ok(Some(TitleSplit {
        folder: cand.series_key.clone(),
        owner_year,
        files,
        joiners,
    }))
}

/// B987 — the unyeared files that stay in a title-split folder join its
/// owner, whose show is not the year the folder's files state. A file whose
/// stored episode disagrees with the owner's same episode (by title, or by an
/// air date a remake's distance away) holds the other show's episode and is
/// rebuilt from the owner. A file the owner lists no episode for is left.
fn owner_joiners(
    cand: &SeriesMatchCandidate,
    owner_id: &str,
    owner_year: u32,
    owner_list: &[EpisodeListing],
    stays: &[(&pharos_core::SeriesEpisodeRow, u32, u32)],
) -> Vec<pharos_core::MediaId> {
    let mut joiners = Vec::new();
    for (row, s, e) in stays {
        let Some(owner) = owner_list
            .iter()
            .find(|l| l.season == *s && l.episode == *e)
        else {
            continue;
        };
        let title_differs = owner.title.as_deref().is_some_and(|t| {
            pharos_core::title_similarity(&row.title, t) < episode_fit::SAME_TITLE
        });
        let aired_differs = aired(row)
            .zip(owner.aired)
            .is_some_and(|(stored, owned)| stored.abs_diff(owned) > RUN_SLACK_SECS.unsigned_abs());
        if !(title_differs || aired_differs) {
            continue;
        }
        tracing::info!(
            series_key = %cand.series_key,
            path = %row.path,
            season = s,
            episode = e,
            stored_title = %row.title,
            stored_aired = aired(row),
            owner_id,
            owner_year,
            owner_title = owner.title.as_deref(),
            owner_aired = owner.aired,
            "series identity repair: file joins the owner's show holding another show's episode"
        );
        joiners.push(row.id);
    }
    joiners
}

/// The show year a show's own episode files state (V893), when they agree on
/// one; else the stored year. The stored column is written by the scan's
/// split pass, which may not have run (or, in a dry run, written) before the
/// survey reaches the show (B980).
fn stated_year(cand: &SeriesMatchCandidate, rows: &[pharos_core::SeriesEpisodeRow]) -> Option<u32> {
    let years: std::collections::BTreeSet<u32> = rows
        .iter()
        .filter_map(|r| {
            pharos_scanner::series_split::stated_show_year(std::path::Path::new(&r.path))
        })
        .collect();
    match years.len() {
        1 => years.into_iter().next(),
        _ => cand.series_year,
    }
}

/// Why a searched show's stored match conflicts with its evidence, if it does.
#[allow(clippy::too_many_arguments)]
async fn show_conflict<Tv, Tm, S>(
    store: &S,
    providers: &SeriesProviders<'_, Tv, Tm>,
    bg_io: &Arc<Semaphore>,
    cfg: &MetadataConfig,
    cand: &SeriesMatchCandidate,
    stated: Option<u32>,
    stored: &SeriesMetadata,
    provider: &str,
) -> DomainResult<Option<(RepairReason, String)>>
where
    Tv: OnlineEnricher,
    Tm: OnlineEnricher,
    S: pharos_core::SeriesIdentityStore,
{
    let mut parts = cand
        .series_key
        .split(pharos_scanner::series_split::SPLIT_KEY_SEPARATOR);
    let folder = parts.next().unwrap_or_default().to_string();
    let split = parts.next().is_some()
        || !store
            .series_overrides(std::slice::from_ref(&folder))
            .await?
            .is_empty();
    if split {
        return Ok(Some((RepairReason::Split, String::new())));
    }
    let premiered = stored.premiere_date.and_then(pharos_store_sqlx::unix_year);
    // B990 — the same slack the re-match reads a candidate's year with, so
    // a show it chose is not re-asked by the next survey.
    if let (Some(stated), Some(premiered)) = (stated, premiered) {
        if stated.abs_diff(premiered) > STATED_YEAR_SLACK {
            return Ok(Some((RepairReason::YearMismatch, String::new())));
        }
    }
    let decision = match provider {
        "tvdb" => match providers.tvdb {
            Some(t) => Some(
                search_decide(t, MediaKind::Episode, &cand.series_name, stated, bg_io, cfg).await,
            ),
            None => None,
        },
        "tmdb" => match providers.tmdb {
            Some(t) => Some(
                search_decide(t, MediaKind::Episode, &cand.series_name, stated, bg_io, cfg).await,
            ),
            None => None,
        },
        _ => None,
    };
    Ok(decision
        .filter(needs_episode_fit)
        .map(|d| (RepairReason::ExactRival, exact_candidates_label(&d))))
}

/// The exact-name candidates a search tied on, as `id (year) score` joined
/// by `, `, so a re-ask line names the rivals it saw.
fn exact_candidates_label(decision: &MatchDecision) -> String {
    exact_candidates(decision)
        .iter()
        .map(|c| match c.year {
            Some(year) => format!("{} ({year}) {:.2}", c.id, c.score),
            None => format!("{} (no year) {:.2}", c.id, c.score),
        })
        .collect::<Vec<_>>()
        .join(", ")
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
    // B990 — the year the show's own files state, as the survey read it
    // (B980): the stored column is empty for a bare folder until the scan's
    // split pass writes it, and a search without it hands a remake's files
    // to the original's exact name.
    let rows = match store.series_episodes(&cand.series_key).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                error = %e,
                series_key = %cand.series_key,
                "T9-series metadata backfill: local episodes unreadable; searching with the stored year"
            );
            Vec::new()
        }
    };
    let year = stated_year(&cand, &rows);
    let stored = match store
        .series_metadata_by_keys(std::slice::from_ref(&cand.series_key))
        .await
    {
        Ok(mut found) => found.remove(&cand.series_key),
        Err(e) => {
            tracing::warn!(
                error = %e,
                series_key = %cand.series_key,
                "T9-series metadata backfill: stored match unreadable; not checking its run"
            );
            None
        }
    };
    let short_pin = too_short_pin(providers, bg_io, stored.as_ref(), &rows).await;
    if let Some((provider, id)) = &short_pin {
        tracing::info!(
            series_key = %cand.series_key,
            provider,
            match_id = id.as_str(),
            seasons = regular_seasons(&rows),
            "series match: the stored match's run is too short for the show's seasons, leaving it out"
        );
        record_rematch(RematchStep::PinExcluded);
    }
    let evidence = SeriesEvidence {
        year,
        rows: &rows,
        short_pin,
    };
    let mut outcome = None;
    // A provider BLIP is not a provider saying no. If any attempt was
    // transient we leave the row untouched for the next pass rather than
    // freezing the show as `none` for the whole TTL.
    let mut transient = None;
    if let Some(tv) = providers.tvdb {
        match resolve_series(
            store, tv, "tvdb", bg_io, cache, cfg, &cand, &evidence, lists,
        )
        .await
        {
            SeriesAttempt::Hit(h) => outcome = Some(h),
            SeriesAttempt::Transient(d) => transient = Some(d),
            SeriesAttempt::NoMatch => {}
        }
    }
    let mut fell_back = false;
    if outcome.is_none() {
        if let Some(tm) = providers.tmdb {
            match resolve_series(
                store, tm, "tmdb", bg_io, cache, cfg, &cand, &evidence, lists,
            )
            .await
            {
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
        // B990 — a show whose stored match ran too short for its seasons, and
        // whose every other candidate did too (or none was left), keeps its
        // match flagged for Manage → Show identity rather than be pinned to
        // it again or lose it.
        if let (Some((provider, id)), Some(mut row)) = (&evidence.short_pin, stored) {
            tracing::info!(
                series_key = %cand.series_key,
                provider,
                match_id = id.as_str(),
                seasons = regular_seasons(&rows),
                "series identity repair: re-ask left the show flagged: only too-short candidates"
            );
            record_rematch(RematchStep::FlaggedTooShort);
            row.match_flag = Some(AMBIGUOUS_FLAG.to_string());
            row.metadata_refreshed_at = Some(now);
            store.upsert_series_metadata(row).await?;
            return Ok(false);
        }
        // Every configured provider searched and found nothing over the
        // confidence floor → record `none` so the show isn't re-searched until
        // the TTL re-admits it. Attributed to whichever provider ran last.
        let provider = if providers.tmdb.is_some() {
            "tmdb"
        } else {
            "tvdb"
        };
        mark_if_match_changed(store, &cand.series_key, None, now).await?;
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
    if year != cand.series_year {
        tracing::info!(
            series_key = %cand.series_key,
            provider = hit.provider,
            year,
            stored_year = cand.series_year,
            chosen = hit.external_id.as_str(),
            decided_by = hit
                .decision
                .decided_by
                .map_or("none", pharos_core::MatchDecidedBy::label),
            "series match: matched with the year the show's files state"
        );
        record_rematch(RematchStep::StatedYear);
    }
    if fell_back {
        tracing::info!(
            series = %cand.series_name,
            provider = hit.provider,
            "T9-series metadata backfill: matched by the fallback provider"
        );
    }

    let flagged = hit.decision.ambiguous;
    mark_if_match_changed(
        store,
        &cand.series_key,
        Some((hit.provider, hit.external_id.as_str())),
        now,
    )
    .await?;
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
            locked: Default::default(),
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
    evidence: &SeriesEvidence<'_>,
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
    let year = evidence.year;
    let mut excluded: Vec<String> = evidence
        .short_pin
        .iter()
        .filter(|(p, _)| *p == provider)
        .map(|(_, id)| id.clone())
        .collect();
    let mut checked = 0usize;
    let (decision, fit_runner_up) = loop {
        let mut decision = series_search_decide(
            enricher,
            &cand.series_key,
            &cand.series_name,
            year,
            &excluded,
            bg_io,
            cfg,
        )
        .await;
        let fit_runner_up = if needs_episode_fit(&decision) {
            apply_episode_fit(store, enricher, provider, bg_io, cand, &mut decision, lists).await
        } else {
            None
        };
        // B990 — once the stored match ran too short, so may the next
        // choice: each is checked, and one too short is left out too.
        let short = match &decision.chosen {
            Some(c) if evidence.short_pin.is_some() => {
                is_too_short(enricher, bg_io, &c.id, evidence.rows).await
            }
            _ => false,
        };
        if !short {
            break (decision, fit_runner_up);
        }
        let Some(chosen) = decision.chosen.take() else {
            break (decision, fit_runner_up);
        };
        tracing::info!(
            provider,
            series_key = %cand.series_key,
            candidate = chosen.id.as_str(),
            "series match: candidate's run is too short for the show's seasons, leaving it out"
        );
        excluded.push(chosen.id);
        checked += 1;
        if checked >= MAX_SHORT_RUN_CHECKS {
            log_series_match(
                provider,
                &cand.series_key,
                &cand.series_name,
                year,
                &decision,
            );
            return SeriesAttempt::NoMatch;
        }
    };
    log_series_match(
        provider,
        &cand.series_key,
        &cand.series_name,
        year,
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

/// What a show's own files say about which show it is, for its match.
struct SeriesEvidence<'a> {
    /// The year its files state, else the stored year (B990).
    year: Option<u32>,
    rows: &'a [pharos_core::SeriesEpisodeRow],
    /// B990 — the stored search match, by provider and id, when its whole
    /// run is too short for the show's local seasons (B986).
    short_pin: Option<(&'static str, String)>,
}

/// Candidates whose runs a re-match checks before it stops: each costs a
/// request, and a show with more too-short candidates than this is flagged.
const MAX_SHORT_RUN_CHECKS: usize = 3;

/// B990 — the stored search match, when its whole run is too short for the
/// show's two or more local seasons (B986's rule). One request to the
/// provider that holds it.
async fn too_short_pin<Tv, Tm>(
    providers: &SeriesProviders<'_, Tv, Tm>,
    bg_io: &Arc<Semaphore>,
    stored: Option<&SeriesMetadata>,
    rows: &[pharos_core::SeriesEpisodeRow],
) -> Option<(&'static str, String)>
where
    Tv: OnlineEnricher,
    Tm: OnlineEnricher,
{
    let m = stored?;
    if m.match_source.as_deref() != Some("search") || regular_seasons(rows) < 2 {
        return None;
    }
    let (provider, id) = stored_match(m)?;
    let short = match provider {
        "tvdb" => is_too_short(providers.tvdb?, bg_io, id, rows).await,
        "tmdb" => is_too_short(providers.tmdb?, bg_io, id, rows).await,
        _ => false,
    };
    let provider = if provider == "tvdb" { "tvdb" } else { "tmdb" };
    short.then(|| (provider, id.to_string()))
}

/// Whether show `id`'s whole run is too short for `rows`' seasons (B986).
async fn is_too_short<E: OnlineEnricher>(
    enricher: &E,
    bg_io: &Arc<Semaphore>,
    id: &str,
    rows: &[pharos_core::SeriesEpisodeRow],
) -> bool {
    let run = {
        let _permit = BgPermit::acquire(bg_io).await;
        enricher.series_run(id).await
    };
    run_window(std::iter::empty(), run).is_some_and(|w| run_too_short(w, rows))
}

/// Spec 033 (V891) — `series_metadata.match_flag` of a close call.
pub(crate) const AMBIGUOUS_FLAG: &str = "ambiguous";

/// Episode lists fetched this pass, by provider, show id and the seasons
/// asked: shows that share candidates (two same-named folders) ask
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
/// first (TVDB lists a remake as "Show B (1996)" with the bare
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
    use pharos_core::SeriesIdentityStore as _;
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
        /// How many searches were made.
        searched: Arc<std::sync::Mutex<usize>>,
        /// Series runs by show id; any other id has none.
        runs: HashMap<String, crate::online_enrich::SeriesRun>,
        /// `begin_pass` / `end_pass` calls, in order.
        passes: Arc<std::sync::Mutex<Vec<&'static str>>>,
        /// What a search narrowed by a year answers, when set; else `search`.
        year_search: Option<Vec<SearchCandidate>>,
        /// The year each search was narrowed by, in order.
        years: Arc<std::sync::Mutex<Vec<Option<u32>>>>,
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
                searched: Default::default(),
                runs: HashMap::new(),
                passes: Default::default(),
                year_search: None,
                years: Default::default(),
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
                searched: Default::default(),
                runs: HashMap::new(),
                passes: Default::default(),
                year_search: None,
                years: Default::default(),
            }
        }

        fn with_search(mut self, cands: Vec<(&str, &str, Option<u32>)>) -> Self {
            self.search = fake_candidates(cands);
            self
        }

        /// A provider whose year filter is strict: a search narrowed by a
        /// year answers `cands`, one without answers `with_search`'s.
        fn with_year_search(mut self, cands: Vec<(&str, &str, Option<u32>)>) -> Self {
            self.year_search = Some(fake_candidates(cands));
            self
        }

        fn years(&self) -> Vec<Option<u32>> {
            self.years.lock().unwrap().clone()
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

        fn with_dated_list(mut self, id: &str, episodes: &[(u32, u32, &str, i64)]) -> Self {
            let list = episodes
                .iter()
                .map(|&(season, episode, title, aired)| EpisodeListing {
                    season,
                    episode,
                    title: Some(title.to_string()),
                    aired: Some(aired),
                })
                .collect();
            self.lists.insert(id.to_string(), list);
            self
        }

        fn with_aired_list(mut self, id: &str, episodes: &[(u32, u32, i64)]) -> Self {
            let list = episodes
                .iter()
                .map(|&(season, episode, aired)| EpisodeListing {
                    season,
                    episode,
                    title: None,
                    aired: Some(aired),
                })
                .collect();
            self.lists.insert(id.to_string(), list);
            self
        }

        fn with_run(mut self, id: &str, run: crate::online_enrich::SeriesRun) -> Self {
            self.runs.insert(id.to_string(), run);
            self
        }

        fn searched(&self) -> usize {
            *self.searched.lock().unwrap()
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

    fn fake_candidates(cands: Vec<(&str, &str, Option<u32>)>) -> Vec<SearchCandidate> {
        cands
            .into_iter()
            .map(|(id, title, year)| SearchCandidate {
                id: id.to_string(),
                title: title.to_string(),
                year,
                aliases: Vec::new(),
            })
            .collect()
    }

    impl OnlineEnricher for FakeEnricher {
        fn provider(&self) -> &'static str {
            self.provider
        }
        fn begin_pass(&self) {
            self.passes.lock().expect("lock").push("begin");
        }
        fn end_pass(&self) {
            self.passes.lock().expect("lock").push("end");
        }
        fn supports(&self, _kind: MediaKind) -> bool {
            true
        }
        async fn search(
            &self,
            _kind: MediaKind,
            _title: &str,
            year: Option<u32>,
        ) -> Vec<SearchCandidate> {
            *self.searched.lock().unwrap() += 1;
            self.years.lock().unwrap().push(year);
            match (&self.year_search, year) {
                (Some(narrowed), Some(_)) => narrowed.clone(),
                _ => self.search.clone(),
            }
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
        async fn series_run(&self, id: &str) -> Option<crate::online_enrich::SeriesRun> {
            self.runs.get(id).copied()
        }
    }

    fn enriched_overview(overview: &str) -> EnrichedMetadata {
        EnrichedMetadata {
            overview: Some(overview.to_string()),
            ..EnrichedMetadata::default()
        }
    }

    /// The version gate with no provider configured, as a dry run.
    async fn readmit(s: &SqliteStore) {
        let providers = SeriesProviders::<FakeEnricher, FakeEnricher> {
            tvdb: None,
            tmdb: None,
        };
        readmit_stale_tv_matches(s, &providers, &sem(1), &MetadataConfig::default(), NOW)
            .await
            .unwrap();
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

    #[tokio::test(flavor = "current_thread")]
    async fn a_no_confident_match_logs_the_search_and_the_nearest_miss() {
        let (logs, _log) = log_capture();
        let s = store().await;
        put_movie(&s, 900_103, "a film").await;
        let (_td, cache) = cache();
        // One candidate, under the floor: it is the nearest miss the log names.
        let tmdb = FakeEnricher::tmdb().with_search(vec![("5", "Something Else", None)]);

        run(
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
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("no confident match"))
            .unwrap_or_default();
        assert!(line.contains("media.id=900103"), "{line}");
        assert!(line.contains("provider=\"tmdb\""), "{line}");
        assert!(line.contains("kind=\"movie\""), "{line}");
        assert!(line.contains("searched_title=\"a film\""), "{line}");
        assert!(line.contains("best_title=\"Something Else\""), "{line}");
        assert!(line.contains("best_score="), "{line}");
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
        // B1181 — a local row only blocks online art while its file exists.
        let curated = tempfile::tempdir().unwrap();
        let poster = curated.path().join("poster.jpg");
        std::fs::write(&poster, [0xFF, 0xD8, 0xFF]).unwrap();
        let poster_locator = poster.to_string_lossy().into_owned();
        s.set_artwork(900_301, "Primary", "local", &poster_locator)
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
        assert_eq!(primary.2, poster_locator);
        let backdrop = art.iter().find(|(role, _, _)| role == "Backdrop");
        assert!(
            backdrop.is_some(),
            "backdrop should still be added: {art:?}"
        );
        assert_eq!(backdrop.unwrap().1, "tmdb");
    }

    #[tokio::test]
    async fn enrich_one_fills_a_role_whose_local_sidecar_is_gone() {
        // B1181 / V1141 — a local Primary whose file has gone is no curated
        // art. The provider's Primary is cached and replaces the dead row.
        let s = store().await;
        let media_dir = tempfile::tempdir().unwrap();
        let media = media_dir.path().join("a film.mkv");
        std::fs::write(&media, b"video").unwrap();
        s.put(MediaItem {
            id: 900_302,
            path: media.clone(),
            title: "a film".to_string(),
            kind: MediaKind::Movie,
            ..MediaItem::default()
        })
        .await
        .unwrap();
        let dead = media_dir.path().join("folder.jpg");
        s.set_artwork(900_302, "Primary", "local", &dead.to_string_lossy())
            .await
            .unwrap();
        let (_td, cache) = cache();
        let tmdb = FakeEnricher::tmdb()
            .with_search(vec![("438632", "a film", Some(2021))])
            .with_detail(EnrichedMetadata {
                artwork: vec![RemoteArt {
                    role: ArtworkRole::Primary,
                    url: "https://image.tmdb.org/t/p/original/p2.jpg".into(),
                }],
                ..EnrichedMetadata::default()
            })
            .with_image_bytes(vec![0xFF, 0xD8, 0xFF]);

        let item = s.get(900_302).await.unwrap();
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

        let art = s.artwork_for(900_302).await.unwrap();
        let primary = art
            .iter()
            .find(|(role, _, _)| role == "Primary")
            .expect("primary row present");
        assert_eq!(
            primary.1, "tmdb",
            "provider art replaces the dead local row: {art:?}"
        );
    }

    #[tokio::test]
    async fn enrich_one_keeps_a_missing_sidecar_while_the_media_is_unreachable_too() {
        // B1181 / V1141 — with the media file gone too, the share is probably
        // down: the missing sidecar proves nothing and must not be replaced.
        let s = store().await;
        let media_dir = tempfile::tempdir().unwrap();
        let media = media_dir.path().join("a film.mkv");
        std::fs::write(&media, b"video").unwrap();
        s.put(MediaItem {
            id: 900_302,
            path: media.clone(),
            title: "a film".to_string(),
            kind: MediaKind::Movie,
            ..MediaItem::default()
        })
        .await
        .unwrap();
        let dead = media_dir.path().join("folder.jpg");
        s.set_artwork(900_302, "Primary", "local", &dead.to_string_lossy())
            .await
            .unwrap();
        let (_td, cache) = cache();
        let tmdb = FakeEnricher::tmdb()
            .with_search(vec![("438632", "a film", Some(2021))])
            .with_detail(EnrichedMetadata {
                artwork: vec![RemoteArt {
                    role: ArtworkRole::Primary,
                    url: "https://image.tmdb.org/t/p/original/p2.jpg".into(),
                }],
                ..EnrichedMetadata::default()
            })
            .with_image_bytes(vec![0xFF, 0xD8, 0xFF]);

        std::fs::remove_file(&media).unwrap();
        let item = s.get(900_302).await.unwrap();
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

        let art = s.artwork_for(900_302).await.unwrap();
        let primary = art
            .iter()
            .find(|(role, _, _)| role == "Primary")
            .expect("primary row present");
        assert_eq!(primary.1, "local", "the sidecar row stays: {art:?}");
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
        // B1181 — the show's curated Primary blocks online art only while its
        // file exists, so this test's sidecar must be a real file.
        let show = tempfile::tempdir().unwrap();
        let folder = show.path().join("folder.jpg");
        std::fs::write(&folder, [0xFF, 0xD8, 0xFF]).unwrap();
        s.set_artwork(900_570, "Primary", "local", &folder.to_string_lossy())
            .await
            .unwrap();
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

        readmit(&s).await;

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
        readmit(&s).await;
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
        readmit(&s).await;
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
        readmit(&s).await;
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

    /// B989: a provider's per-pass state is opened and closed once around a
    /// whole pass, and the pass scope closes it on an early exit too.
    #[tokio::test]
    async fn a_pass_opens_and_closes_the_providers_state_once() {
        let s = store().await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb();
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
        assert_eq!(*tvdb.passes.lock().unwrap(), ["begin", "end"]);

        let scoped = FakeEnricher::tvdb();
        {
            let _scope = PassScope::open(Some(&scoped));
            assert_eq!(*scoped.passes.lock().unwrap(), ["begin"]);
        }
        assert_eq!(*scoped.passes.lock().unwrap(), ["begin", "end"]);
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

    fn gauge_value(
        snap: &pharos_test_metrics::CumulativeSnapshotter,
        name: &str,
        labels: &[(&str, &str)],
    ) -> Option<f64> {
        use metrics_util::debugging::DebugValue;
        snap.snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(k, _, _, v)| {
                let hit = k.key().name() == name
                    && labels.iter().all(|(n, want)| {
                        k.key()
                            .labels()
                            .any(|l| l.key() == *n && l.value() == *want)
                    });
                match v {
                    DebugValue::Gauge(g) if hit => Some(g.into_inner()),
                    _ => None,
                }
            })
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

    /// Spec 033 B930 — a bare `Show B` folder searches with no
    /// year, two shows carry the name, and the provider's order picks the
    /// 1971 one. The decision is counted as ambiguous, decided by provider
    /// order, and logged with both candidates.
    #[tokio::test(flavor = "current_thread")]
    async fn a_same_named_show_decided_by_provider_order_is_counted_and_logged() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();

        let s = store().await;
        put_bare_episode(&s, 1, "Show B", "/tv/Show B").await;
        // No year in the file's name either: a stated year would tell the
        // two shows apart (B990).
        let mut item = s.get(1).await.unwrap();
        item.path = item.path.to_string_lossy().replace(" (2003)", "").into();
        s.put(item).await.unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![
                ("900001", "Show B", Some(1971)),
                ("900002", "Show B", Some(1996)),
            ])
            .with_detail(enriched_overview("A scattered crew."));

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
            "query=\"Show B\"",
            "chosen=\"900001\"",
            "runner_up=\"900002\"",
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

    fn show_b_search(tvdb: FakeEnricher) -> FakeEnricher {
        tvdb.with_search(vec![
            ("900001", "Show B", Some(1971)),
            ("900002", "Show B", Some(1996)),
        ])
        .with_detail(enriched_overview("A scattered crew."))
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

    /// Spec 033 V891 / B930 — two shows named exactly "Show B"
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
            (1, 1, "Opening"),
            (2, 2, "Second Light"),
            (3, 4, "The Long Wait"),
        ] {
            put_titled_episode(&s, id, "/tv/Show B", (season, 1), title).await;
            put_titled_episode(&s, id + 10, "/tv2/Show B", (season, 1), title).await;
        }
        let (_td, cache) = cache();
        let tvdb = show_b_search(FakeEnricher::tvdb())
            .with_list(
                "900001",
                &[(1, 1, "The Old Second"), (1, 2, "The Lost Road")],
            )
            .with_list(
                "900002",
                &[
                    (1, 1, "Opening"),
                    (2, 1, "Second Light"),
                    (3, 1, "The Third Door"),
                    (4, 1, "The Long Wait"),
                ],
            );

        series_pass(&s, &cache, &tvdb).await;

        for key in ["/tv/Show B", "/tv2/Show B"] {
            let row = series_row(&s, key).await;
            assert_eq!(row.match_external_id.as_deref(), Some("900002"), "{key}");
            assert_eq!(row.match_flag, None, "a clear fit is not flagged");
            assert_eq!(row.match_runner_up_id, None);
        }
        let mut listed = tvdb.listed();
        listed.sort();
        assert_eq!(listed, ["900001", "900002"], "one list per show per pass");
        assert!(!tvdb.fetched().contains(&"900001".to_string()));
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
            "chosen=\"900002\"",
            "runner_up=\"900001\"",
            "local_episodes=3",
            "titled=3",
            "flagged=false",
            "decided_by=\"episode_fit\"",
            "900002:1.000",
            "900001:0.167",
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

    /// B930 — TVDB titles the remake "Show B (1996)" with the
    /// bare name as an alias, so the original wins on name position alone.
    /// The episodes still decide.
    #[tokio::test(flavor = "current_thread")]
    async fn a_remake_matching_by_its_alias_is_decided_by_its_episodes() {
        let s = store().await;
        put_titled_episode(&s, 1, "/tv/Show B", (4, 1), "The Long Wait").await;
        let (_td, cache) = cache();
        let mut tvdb = FakeEnricher::tvdb()
            .with_detail(enriched_overview("A scattered crew."))
            .with_list("900001", &[(1, 1, "The Old Second")])
            .with_list("900002", &[(4, 1, "The Long Wait")]);
        tvdb.search = vec![
            SearchCandidate {
                id: "900001".into(),
                title: "Show B".into(),
                year: Some(1971),
                aliases: Vec::new(),
            },
            SearchCandidate {
                id: "900002".into(),
                title: "Show B (1996)".into(),
                year: Some(1996),
                aliases: vec!["Show B".into()],
            },
        ];

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, "/tv/Show B").await;
        assert_eq!(row.match_external_id.as_deref(), Some("900002"));
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
        put_titled_episode(&s, 1, "/tv/Show B", (1, 1), "Pilot").await;
        let (_td, cache) = cache();
        let tvdb = show_b_search(FakeEnricher::tvdb())
            .with_list("900001", &[(1, 1, "Pilot")])
            .with_list("900002", &[(1, 1, "Pilot")]);

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, "/tv/Show B").await;
        assert_eq!(row.match_external_id.as_deref(), Some("900001"));
        assert_eq!(row.match_flag.as_deref(), Some(AMBIGUOUS_FLAG));
        assert_eq!(row.match_runner_up_id.as_deref(), Some("900002"));
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
        let folder = show.path().join("Show B");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("tvshow.nfo"),
            r#"<tvshow><title>Show B</title><uniqueid type="tvdb">900002</uniqueid></tvshow>"#,
        )
        .unwrap();
        let folder = folder.to_string_lossy().into_owned();
        let s = store().await;
        put_titled_episode(&s, 1, &folder, (1, 1), "Pilot").await;
        let (_td, cache) = cache();
        let tvdb = show_b_search(FakeEnricher::tvdb())
            .with_list("900001", &[(1, 1, "Pilot")])
            .with_list("900002", &[(1, 1, "Pilot")]);
        let (logs, _log) = log_capture();

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, &folder).await;
        assert_eq!(row.match_external_id.as_deref(), Some("900002"));
        assert_eq!(row.match_flag.as_deref(), Some(AMBIGUOUS_FLAG));
        assert_eq!(row.match_runner_up_id.as_deref(), Some("900001"));
        let text = logs.text();
        assert!(
            text.lines().any(|l| l.contains("series episode fit")
                && l.contains("nfo_vote=\"900002\"")
                && l.contains("decided_by=\"nfo_vote\"")),
            "{text}"
        );
    }

    /// V891 — more same-named shows than were scored: the best of the scored
    /// three is used and flagged, since an unscored one may fit better.
    #[tokio::test(flavor = "current_thread")]
    async fn more_same_named_shows_than_scored_is_flagged() {
        let s = store().await;
        put_titled_episode(&s, 1, "/tv/Show C", (1, 1), "Harbour").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![
                ("1", "Show C", None),
                ("2", "Show C", None),
                ("3", "Show C", None),
                ("4", "Show C", None),
            ])
            .with_detail(enriched_overview("Space."))
            .with_list("1", &[(2, 1, "x")])
            .with_list("2", &[(1, 1, "Harbour")])
            .with_list("3", &[(3, 1, "y")])
            .with_list("4", &[(1, 1, "Harbour")]);

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, "/tv/Show C").await;
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
        put_titled_episode(&s, 1, "/tv/Show B", (1, 1), "Opening").await;
        let (_td, cache) = cache();
        let tvdb = show_b_search(FakeEnricher::tvdb()).with_list("900002", &[(1, 1, "Opening")]);

        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, "/tv/Show B").await;
        assert_eq!(row.match_external_id.as_deref(), Some("900001"));
        assert_eq!(row.match_flag.as_deref(), Some(AMBIGUOUS_FLAG));
        assert_eq!(row.match_runner_up_id.as_deref(), Some("900002"));
    }

    const SHOW_B: &str = "/tv/Show B";

    fn apply_cfg() -> MetadataConfig {
        MetadataConfig {
            series_identity_repair: RepairMode::Apply,
            ..MetadataConfig::default()
        }
    }

    async fn pin(s: &SqliteStore, key: &str, id: &str, refreshed_at: i64) {
        s.upsert_series_metadata(SeriesMetadata {
            series_key: key.into(),
            series_name: key.rsplit('/').next().unwrap_or(key).into(),
            match_provider: Some("tvdb".into()),
            match_external_id: Some(id.into()),
            match_source: Some("search".into()),
            metadata_refreshed_at: Some(refreshed_at),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    async fn pending_ids(s: &SqliteStore) -> Vec<u64> {
        s.series_reconcile_pending(100)
            .await
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// V940 — a show re-asked onto another match marks its episodes for
    /// reconciliation before the new match is written. An episode's own
    /// manual match is not marked.
    #[tokio::test(flavor = "current_thread")]
    async fn a_changed_show_match_marks_its_episodes() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let s = store().await;
        put_titled_episode(&s, 1, SHOW_B, (1, 1), "Opening").await;
        put_titled_episode(&s, 2, SHOW_B, (2, 1), "Second Light").await;
        s.set_item_match(2, "tvdb", "1", "manual", None, NOW)
            .await
            .unwrap();
        // Stored long ago, so the TTL re-asks it.
        pin(&s, SHOW_B, "900001", 0).await;
        let (_td, cache) = cache();
        let tvdb = show_b_search(FakeEnricher::tvdb())
            .with_list("900001", &[(1, 1, "The Old Second")])
            .with_list("900002", &[(1, 1, "Opening"), (2, 1, "Second Light")]);

        series_pass(&s, &cache, &tvdb).await;

        assert_eq!(
            series_row(&s, SHOW_B).await.match_external_id.as_deref(),
            Some("900002")
        );
        assert_eq!(pending_ids(&s).await, [1]);
        assert_eq!(
            counter_value(
                &snap,
                "pharos_series_reconcile_marked_total",
                &[("reason", "match_changed")]
            ),
            1
        );

        // The same answer again marks nothing more.
        s.clear_series_reconcile(1, NOW).await.unwrap();
        pin(&s, SHOW_B, "900002", 0).await;
        series_pass(&s, &cache, &tvdb).await;
        assert!(pending_ids(&s).await.is_empty());
    }

    /// V940 — under `apply`, a marked episode is rebuilt: what the old show's
    /// record wrote is reset, its stored translations deleted, and it is read
    /// from its show's current match. The mark then clears.
    #[tokio::test(flavor = "current_thread")]
    async fn a_marked_episode_is_rebuilt_against_its_shows_match() {
        let s = store().await;
        put_titled_episode(&s, 1, SHOW_B, (1, 1), "Opening").await;
        let mut item = s.get(1).await.unwrap();
        item.title = "The Old Second".into();
        item.metadata.overview = Some("The old pilot.".into());
        item.metadata.premiere_date = Some(54_259_200);
        item.metadata.provider_ids.tvdb = Some("900001".into());
        s.put(item).await.unwrap();
        s.set_item_match(1, "tvdb", "900001", "search", None, NOW)
            .await
            .unwrap();
        s.upsert_translations(
            &TranslationOwner::Item(1),
            &[LocalizedText {
                language: "deu".into(),
                title: Some("Das Alte Zweite".into()),
                ..LocalizedText::default()
            }],
            NOW,
        )
        .await
        .unwrap();
        pin(&s, SHOW_B, "900002", NOW).await;
        s.mark_series_reconcile(SHOW_B, NOW).await.unwrap();
        let (_td, cache) = cache();
        let record = EnrichedMetadata {
            title: Some("Opening".into()),
            overview: Some("The crew moves on every 33 minutes.".into()),
            premiere_date: Some(876_787_200),
            ..EnrichedMetadata::default()
        };
        let tvdb = FakeEnricher::tvdb().with_detail(record);

        let (n, pending) = reconcile_pass(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &apply_cfg(),
            NOW + 10,
        )
        .await
        .unwrap();

        assert_eq!((n, pending), (1, true));
        assert_eq!(tvdb.fetched(), ["900002"], "read from the pinned show");
        let got = s.get(1).await.unwrap();
        assert_ne!(got.title, "The Old Second");
        assert_eq!(
            got.metadata.overview.as_deref(),
            Some("The crew moves on every 33 minutes.")
        );
        assert_eq!(got.metadata.premiere_date, Some(876_787_200));
        assert_eq!(got.metadata.provider_ids.tvdb.as_deref(), Some("900002"));
        assert!(s
            .translations_for(&[TranslationOwner::Item(1)], &["deu".into()], false)
            .await
            .unwrap()
            .is_empty());
        assert!(pending_ids(&s).await.is_empty());
    }

    /// V940 — a rebuild drops the stills the old show's record cached (the
    /// row and the file), so the episode no longer serves the old show's
    /// still; the new match's still replaces it when its record has one, and
    /// a local sidecar stays.
    #[tokio::test(flavor = "current_thread")]
    async fn a_rebuilt_episode_drops_the_old_shows_still() {
        let s = store().await;
        let (_td, cache) = cache();
        let mut old_paths = Vec::new();
        for id in [1, 2] {
            put_titled_episode(&s, id, SHOW_B, (1, id as u32), "Opening").await;
            let old = cache
                .upload(
                    id,
                    ImageRole::Primary,
                    MediaKind::Episode,
                    0,
                    b"remake still",
                )
                .await
                .unwrap();
            s.set_artwork(id, "Primary", "tvdb", &old.to_string_lossy())
                .await
                .unwrap();
            old_paths.push(old);
            s.set_artwork(id, "Backdrop", "local", "/tv/Show B/fanart.jpg")
                .await
                .unwrap();
            s.mark_reconcile_items(&[id], NOW).await.unwrap();
        }
        pin(&s, SHOW_B, "900002", NOW).await;
        let before = s.get(1).await.unwrap().art_version;
        let bare = FakeEnricher::tvdb().with_detail(EnrichedMetadata {
            title: Some("Opening".into()),
            ..EnrichedMetadata::default()
        });

        reconcile_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&bare),
            &apply_cfg(),
            1,
            NOW,
            NOW + 1,
        )
        .await
        .unwrap();

        let rows = s.artwork_for(1).await.unwrap();
        assert!(
            rows.iter().all(|(_, source, _)| source == "local"),
            "old show's still row kept: {rows:?}"
        );
        assert_eq!(rows.len(), 1, "the local sidecar stays");
        assert!(!old_paths[0].exists(), "old show's cached still kept");
        assert!(s.get(1).await.unwrap().art_version > before);

        let mut with_still = FakeEnricher::tvdb().with_detail(EnrichedMetadata {
            title: Some("Wellspring".into()),
            artwork: vec![RemoteArt {
                role: ArtworkRole::Primary,
                url: "https://tvdb.example/new-still.jpg".into(),
            }],
            ..EnrichedMetadata::default()
        });
        with_still.image_bytes = Some(b"new still".to_vec());
        reconcile_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&with_still),
            &apply_cfg(),
            2,
            NOW,
            NOW + 1,
        )
        .await
        .unwrap();
        let (_, source, locator) = s
            .artwork_for(2)
            .await
            .unwrap()
            .into_iter()
            .find(|(role, _, _)| role == "Primary")
            .expect("new still recorded");
        assert_eq!(source, "tvdb");
        assert_eq!(std::fs::read(locator).unwrap(), b"new still");
    }

    /// A dry run rebuilds nothing and keeps the mark; a transient miss under
    /// `apply` keeps it too, moved behind the rest.
    #[tokio::test(flavor = "current_thread")]
    async fn a_marked_episode_waits_in_a_dry_run_and_after_a_transient_miss() {
        let s = store().await;
        put_titled_episode(&s, 1, SHOW_B, (1, 1), "Opening").await;
        let mut item = s.get(1).await.unwrap();
        item.metadata.overview = Some("The old pilot.".into());
        s.put(item).await.unwrap();
        pin(&s, SHOW_B, "900002", NOW).await;
        s.mark_series_reconcile(SHOW_B, NOW).await.unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb();
        let pass = |cfg: MetadataConfig, now: i64| {
            let (s, cache, tvdb) = (&s, &cache, &tvdb);
            async move {
                reconcile_pass(
                    s,
                    &sem(4),
                    cache,
                    None::<&FakeEnricher>,
                    Some(tvdb),
                    &cfg,
                    now,
                )
                .await
                .unwrap()
            }
        };

        assert_eq!(pass(MetadataConfig::default(), NOW + 1).await, (0, false));
        assert!(tvdb.fetched().is_empty());
        assert_eq!(
            s.get(1).await.unwrap().metadata.overview.as_deref(),
            Some("The old pilot.")
        );
        assert_eq!(s.series_reconcile_pending(10).await.unwrap(), [(1, NOW)]);

        assert_eq!(pass(apply_cfg(), NOW + 2).await, (0, true));
        assert_eq!(
            s.series_reconcile_pending(10).await.unwrap(),
            [(1, NOW + 2)]
        );
    }

    /// Each reconcile attempt is counted under its outcome, and the labels
    /// are distinct (a dashboard contract).
    #[tokio::test(flavor = "current_thread")]
    async fn reconcile_attempts_are_counted_by_outcome() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let s = store().await;
        put_titled_episode(&s, 1, SHOW_B, (1, 1), "Opening").await;
        put_titled_episode(&s, 2, "/tv/Show C", (1, 1), "Opening").await;
        pin(&s, SHOW_B, "900002", NOW).await;
        s.mark_reconcile_items(&[1, 2], NOW).await.unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb();
        let one = |id: u64| {
            let (s, cache, tvdb) = (&s, &cache, &tvdb);
            async move {
                reconcile_one(
                    s,
                    &sem(4),
                    cache,
                    None::<&FakeEnricher>,
                    Some(tvdb),
                    &apply_cfg(),
                    id,
                    NOW,
                    NOW + 1,
                )
                .await
                .unwrap()
            }
        };

        // The show is not matched yet.
        one(2).await;
        // A miss the provider did not explain.
        one(1).await;
        let tvdb = FakeEnricher::tvdb().with_detail(enriched_overview("Found."));
        s.mark_reconcile_items(&[1], NOW).await.unwrap();
        reconcile_one(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &apply_cfg(),
            1,
            NOW,
            NOW + 2,
        )
        .await
        .unwrap();

        let n = |outcome| {
            counter_value(
                &snap,
                "pharos_series_reconcile_total",
                &[("outcome", outcome)],
            )
        };
        assert_eq!((n("deferred"), n("retry"), n("rebuilt")), (1, 1, 1));
        let labels = [
            ReconcileOutcome::Rebuilt,
            ReconcileOutcome::Retry,
            ReconcileOutcome::Deferred,
        ]
        .map(ReconcileOutcome::label);
        let distinct: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(distinct.len(), labels.len());
    }

    async fn set_version(s: &SqliteStore, key: &str, v: &str) {
        pharos_store_sqlx::ServerConfigStore::set_named_config(s, key, v)
            .await
            .unwrap();
    }

    async fn version(s: &SqliteStore, key: &str) -> Option<String> {
        pharos_store_sqlx::ServerConfigStore::load_named_config(s, key)
            .await
            .unwrap()
    }

    async fn gate(s: &SqliteStore, tvdb: &FakeEnricher, cfg: &MetadataConfig) {
        let providers = SeriesProviders::<FakeEnricher, FakeEnricher> {
            tvdb: Some(tvdb),
            tmdb: None,
        };
        readmit_stale_tv_matches(s, &providers, &sem(4), cfg, NOW)
            .await
            .unwrap();
    }

    /// Spec 033 §6 step 3 (V940) — at version 2 only shows with conflicting
    /// evidence are re-asked, and only under `apply`. A dry run surveys once,
    /// logs the selection, and leaves the version at 1 so `apply` still runs.
    /// Episodes are picked when their own old search found another show, or
    /// when they aired outside their show's run.
    #[tokio::test(flavor = "current_thread")]
    async fn version_two_re_asks_only_shows_with_conflicting_evidence() {
        let (logs, _log) = log_capture();
        let s = store().await;
        // SHOW_B: an exact-name rival. Its episode 1's own search found 900001,
        // its episode 2 aired in 2007, long after the show's run.
        put_titled_episode(&s, 1, SHOW_B, (1, 1), "Opening").await;
        put_titled_episode(&s, 2, SHOW_B, (1, 2), "Wellspring").await;
        s.set_item_match(1, "tvdb", "900001", "search", None, NOW)
            .await
            .unwrap();
        let mut late = s.get(2).await.unwrap();
        late.metadata.premiere_date = Some(1_173_398_400);
        // Another show's episode: its stored title is not the file's (B985).
        late.title = "Stray".into();
        s.put(late).await.unwrap();
        pin(&s, SHOW_B, "900002", NOW).await;
        // Show F: one candidate, the match it has. Left alone.
        put_titled_episode(&s, 3, "/tv/Show F", (1, 1), "Opening Day").await;
        pin(&s, "/tv/Show F", "900010", NOW).await;
        set_version(&s, TV_MATCH_VERSION_KEY, "1").await;
        let tvdb = show_b_search(FakeEnricher::tvdb()).with_aired_list(
            "900002",
            // 1997-10-18 .. 1998-01-24
            &[(1, 1, 877_132_800), (1, 2, 885_600_000)],
        );

        gate(&s, &tvdb, &MetadataConfig::default()).await;

        assert_eq!(
            version(&s, TV_MATCH_VERSION_KEY).await.as_deref(),
            Some("1")
        );
        assert_eq!(
            version(&s, TV_MATCH_SURVEY_DRY_RUN_KEY).await,
            Some(dry_run_marker("2"))
        );
        assert!(s
            .series_needing_match(10, NOW - 1)
            .await
            .unwrap()
            .is_empty());
        assert!(pending_ids(&s).await.is_empty());
        let text = logs.text();
        let survey = text
            .lines()
            .find(|l| l.contains("series identity repair: survey"))
            .unwrap_or_else(|| panic!("no survey line in {text}"));
        for want in [
            "mode=\"dry_run\"",
            "shows=1",
            "episodes=2",
            "Show B:exact_rival",
        ] {
            assert!(survey.contains(want), "{want} missing from {survey}");
        }
        let reask = text
            .lines()
            .find(|l| l.contains("series identity repair: re-ask"))
            .unwrap_or_else(|| panic!("no re-ask line in {text}"));
        assert!(
            reask.contains("candidates=900001 (1971) 0.85, 900002 (1996) 0.85"),
            "exact-name candidates missing from {reask}"
        );
        let searches = tvdb.searched();
        gate(&s, &tvdb, &MetadataConfig::default()).await;
        assert_eq!(
            tvdb.searched(),
            searches,
            "a dry run surveys once per version"
        );

        gate(&s, &tvdb, &apply_cfg()).await;

        assert_eq!(
            version(&s, TV_MATCH_VERSION_KEY).await.as_deref(),
            Some("2")
        );
        let reask: Vec<String> = s
            .series_needing_match(10, NOW - 1)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.series_key)
            .collect();
        assert_eq!(reask, [SHOW_B]);
        let mut pending = pending_ids(&s).await;
        pending.sort_unstable();
        assert_eq!(pending, [1, 2]);
    }

    /// B980: a bare folder whose files state `Show B (1996)`
    /// but whose rows hold no stored year yet (the scan's split pass has not
    /// written it) is still re-asked when its match premiered in 1971.
    #[tokio::test]
    async fn the_survey_reads_a_shows_year_from_its_files_not_the_stored_column() {
        let (logs, _log) = log_capture();
        let s = store().await;
        for (id, ep) in [(1u64, 1u32), (2, 2)] {
            put_titled_episode(&s, id, SHOW_B, (1, ep), "Part").await;
            let mut item = s.get(id).await.unwrap();
            item.path =
                format!("{SHOW_B}/Season 1/Show B (1996) - S01E{ep:02} - Part {ep}.mkv").into();
            s.put(item).await.unwrap();
        }
        assert_eq!(
            s.series_needing_match(10, i64::MAX).await.unwrap()[0].series_year,
            None,
            "the stored column is empty"
        );
        pin(&s, SHOW_B, "900001", NOW).await;
        let mut row = series_row(&s, SHOW_B).await;
        // 1971-09-17
        row.premiere_date = Some(53_913_600);
        s.upsert_series_metadata(row).await.unwrap();
        set_version(&s, TV_MATCH_VERSION_KEY, "1").await;
        // No exact-name rival: the 1996 show is titled with its year.
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![
                ("900001", "Show B", Some(1971)),
                ("900002", "Show B (1996)", Some(1996)),
            ])
            .with_detail(enriched_overview("A scattered crew."));

        gate(&s, &tvdb, &MetadataConfig::default()).await;

        let text = logs.text();
        let survey = text
            .lines()
            .find(|l| l.contains("series identity repair: survey"))
            .unwrap_or_else(|| panic!("no survey line in {text}"));
        assert!(survey.contains("Show B:year_mismatch"), "{survey}");
    }

    /// B982 — the run window is the show's whole run, not the span of the
    /// seasons the provider listed: an absolute-numbered episode of a show
    /// still airing is no `outside_run`, while an ended show's run still
    /// bounds a late episode.
    #[tokio::test(flavor = "current_thread")]
    async fn the_run_window_covers_a_shows_whole_run() {
        use crate::online_enrich::SeriesRun;
        let s = store().await;
        const SHOW_D: &str = "/tv/Show D";
        const SHOW_C: &str = "/tv/Show C";
        put_titled_episode(&s, 1, SHOW_D, (1, 1), "The First Step").await;
        put_titled_episode(&s, 2, SHOW_D, (1, 1050), "The Last Step").await;
        put_titled_episode(&s, 3, SHOW_C, (1, 1), "The Old Second").await;
        put_titled_episode(&s, 4, SHOW_C, (1, 2), "The Quiet Hours").await;
        for (id, aired) in [
            (1, 719_539_200),   // 1992-10-20
            (2, 1_452_211_200), // 2016-01-08
            (3, 118_108_800),   // 1973-09-28
            (4, 1_173_398_400), // 2007-03-09
        ] {
            let mut item = s.get(id).await.unwrap();
            item.metadata.premiere_date = Some(aired);
            if id == 4 {
                // The 2007 remake's episode on a 1973 file (B931, B985).
                item.title = "The Late Hour".into();
            }
            s.put(item).await.unwrap();
        }
        pin(&s, SHOW_D, "900014", NOW).await;
        pin(&s, SHOW_C, "900006", NOW).await;
        let tvdb = FakeEnricher::tvdb()
            // Season 1 as the provider numbers it: 1992-10-20 .. 1993-03-15.
            .with_aired_list("900014", &[(1, 1, 719_539_200), (1, 8, 732_153_600)])
            .with_run(
                "900014",
                SeriesRun {
                    first: Some(719_539_200),
                    last: Some(1_541_116_800),
                    ongoing: true,
                },
            )
            .with_aired_list("900006", &[(1, 1, 118_108_800)])
            .with_run(
                "900006",
                SeriesRun {
                    first: Some(118_108_800),
                    last: Some(125_280_000), // 1973-12-21
                    ongoing: false,
                },
            );
        let providers = SeriesProviders::<FakeEnricher, FakeEnricher> {
            tvdb: Some(&tvdb),
            tmdb: None,
        };

        let survey = survey_series_identity(&s, &providers, &sem(4), &MetadataConfig::default())
            .await
            .unwrap();

        assert_eq!(survey.episodes, [(4, RepairReason::OutsideRun)]);
    }

    /// B984 — an episode stored with air date 0 (an NFO's `1970-01-01`
    /// placeholder) is no evidence of airing outside its show's run.
    #[tokio::test(flavor = "current_thread")]
    async fn an_air_date_of_zero_is_not_outside_the_run() {
        use crate::online_enrich::SeriesRun;
        let s = store().await;
        const SHOW_D: &str = "/tv/Show D";
        put_titled_episode(&s, 1, SHOW_D, (10, 1), "Sudden Notice").await;
        put_titled_episode(&s, 2, SHOW_D, (1, 1), "The First Step").await;
        for (id, aired) in [(1, 0), (2, 719_539_200)] {
            let mut item = s.get(id).await.unwrap();
            item.metadata.premiere_date = Some(aired);
            s.put(item).await.unwrap();
        }
        pin(&s, SHOW_D, "900014", NOW).await;
        let tvdb = FakeEnricher::tvdb()
            .with_aired_list("900014", &[(1, 1, 719_539_200)])
            .with_run(
                "900014",
                SeriesRun {
                    first: Some(719_539_200),
                    last: Some(1_541_116_800),
                    ongoing: true,
                },
            );
        let providers = SeriesProviders::<FakeEnricher, FakeEnricher> {
            tvdb: Some(&tvdb),
            tmdb: None,
        };

        let survey = survey_series_identity(&s, &providers, &sem(4), &MetadataConfig::default())
            .await
            .unwrap();

        assert_eq!(survey.episodes, []);
    }

    /// B985 — an episode dated outside its show's run whose file names the
    /// title stored for it is the right episode with an odd date (a later
    /// release, a re-air); only a disagreeing or untitled file is marked.
    #[tokio::test(flavor = "current_thread")]
    async fn an_outside_run_episode_whose_title_agrees_is_not_marked() {
        use crate::online_enrich::SeriesRun;
        let (logs, _log) = log_capture();
        let s = store().await;
        const GT: &str = "/tv/Show E";
        put_titled_episode(&s, 1, GT, (2, 1), "The Gambit").await;
        put_titled_episode(&s, 2, GT, (2, 2), "Quiet Strength").await;
        put_titled_episode(&s, 3, GT, (2, 3), "untitled").await;
        put_titled_episode(&s, 4, GT, (1, 1), "The Long Wish").await;
        put_titled_episode(&s, 5, GT, (2, 5), "The Open Secret").await;
        for (id, aired) in [
            (1, 1_113_091_200), // 2005-04-10
            (2, 1_113_091_200),
            (3, 1_113_091_200),
            (4, 602_812_800), // 1989-02-07
            (5, 1_113_091_200),
        ] {
            let mut item = s.get(id).await.unwrap();
            item.metadata.premiere_date = Some(aired);
            match id {
                2 => item.title = "Terms of Trade".into(),
                3 => item.path = format!("{GT}/Season 2/Show E - S02E03.mkv").into(),
                // The local resolver's title, taken from the file name.
                5 => item.title = "Show E - S02E05 - The Open Secret".into(),
                _ => {}
            }
            s.put(item).await.unwrap();
        }
        pin(&s, GT, "900011", NOW).await;
        let tvdb = FakeEnricher::tvdb()
            .with_aired_list("900011", &[(1, 1, 602_812_800)])
            .with_run(
                "900011",
                SeriesRun {
                    first: Some(602_812_800),
                    last: Some(658_972_800), // 1990-11-19
                    ongoing: false,
                },
            );
        let providers = SeriesProviders::<FakeEnricher, FakeEnricher> {
            tvdb: Some(&tvdb),
            tmdb: None,
        };

        let survey = survey_series_identity(&s, &providers, &sem(4), &MetadataConfig::default())
            .await
            .unwrap();

        let mut marked: Vec<u64> = survey.episodes.iter().map(|(id, _)| *id).collect();
        marked.sort_unstable();
        assert_eq!(marked, [2, 3, 5]);
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("outside the run, titles agree"))
            .unwrap_or_else(|| panic!("no titles-agree line in {text}"));
        assert!(line.contains("episodes=1"), "{line}");
    }

    /// B986 — a four-season show matched to its pilot miniseries (a run of
    /// two days) is re-asked as `run_too_short`, and its episodes are not
    /// selected against the miniseries meanwhile; a one-season show on a
    /// short run is left alone.
    #[tokio::test(flavor = "current_thread")]
    async fn a_series_matched_to_a_miniseries_is_re_asked() {
        use crate::online_enrich::SeriesRun;
        let s = store().await;
        const SHOW_B_REMAKE: &str = "/tv/Show B (1996)";
        const MINI: &str = "/tv/Show H";
        let mut id = 0;
        for season in 1..=4 {
            id += 1;
            put_titled_episode(&s, id, SHOW_B_REMAKE, (season, 1), "Slow Start").await;
            let mut item = s.get(id).await.unwrap();
            item.title = "Night 1".into();
            // 1998-01-14 onwards, a season a year.
            item.metadata.premiere_date = Some(884_736_000 + i64::from(season) * 31_536_000);
            s.put(item).await.unwrap();
        }
        put_titled_episode(&s, 9, MINI, (1, 1), "Step One").await;
        pin(&s, SHOW_B_REMAKE, "900003", NOW).await;
        pin(&s, MINI, "900012", NOW).await;
        let night = |first: i64, last: i64| SeriesRun {
            first: Some(first),
            last: Some(last),
            ongoing: false,
        };
        let tvdb = FakeEnricher::tvdb()
            // 1996-12-08 .. 1996-12-09
            .with_aired_list("900003", &[(1, 1, 850_003_200), (1, 2, 850_089_600)])
            .with_run("900003", night(850_003_200, 850_089_600))
            .with_aired_list("900012", &[(1, 1, 779_673_600)])
            .with_run("900012", night(779_673_600, 782_697_600));
        let providers = SeriesProviders::<FakeEnricher, FakeEnricher> {
            tvdb: Some(&tvdb),
            tmdb: None,
        };

        let survey = survey_series_identity(&s, &providers, &sem(4), &MetadataConfig::default())
            .await
            .unwrap();

        assert_eq!(
            survey.reask.get(SHOW_B_REMAKE),
            Some(&RepairReason::RunTooShort),
            "{:?}",
            survey.reask
        );
        assert!(!survey.reask.contains_key(MINI), "{:?}", survey.reask);
        assert_eq!(survey.episodes, []);
    }

    /// A dry run surveys again when the survey revision moved under the same
    /// matcher version (a marker from before revisions, or another revision),
    /// and not while the marker is current.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dry_run_surveys_again_after_a_survey_revision_bump() {
        let s = store().await;
        put_titled_episode(&s, 1, SHOW_B, (1, 1), "Opening").await;
        pin(&s, SHOW_B, "900002", NOW).await;
        set_version(&s, TV_MATCH_VERSION_KEY, "1").await;
        let tvdb = show_b_search(FakeEnricher::tvdb());
        let current = dry_run_marker(&TV_MATCH_VERSION.to_string());

        for stale in ["2", "2.r1"] {
            set_version(&s, TV_MATCH_SURVEY_DRY_RUN_KEY, stale).await;
            let searches = tvdb.searched();
            gate(&s, &tvdb, &MetadataConfig::default()).await;
            assert!(
                tvdb.searched() > searches,
                "marker {stale} was not re-surveyed"
            );
            assert_eq!(
                version(&s, TV_MATCH_SURVEY_DRY_RUN_KEY).await,
                Some(current.clone())
            );
        }

        let searches = tvdb.searched();
        gate(&s, &tvdb, &MetadataConfig::default()).await;
        assert_eq!(
            tvdb.searched(),
            searches,
            "a current marker surveys nothing"
        );
        assert_eq!(
            version(&s, TV_MATCH_VERSION_KEY).await.as_deref(),
            Some("1"),
            "a dry run never records the matcher version"
        );
    }

    async fn flagged_pin(s: &SqliteStore, key: &str, id: &str) {
        s.upsert_series_metadata(SeriesMetadata {
            series_key: key.into(),
            series_name: key.rsplit('/').next().unwrap_or(key).into(),
            match_provider: Some("tvdb".into()),
            match_external_id: Some(id.into()),
            match_source: Some("search".into()),
            metadata_refreshed_at: Some(NOW),
            match_flag: Some(AMBIGUOUS_FLAG.into()),
            match_runner_up_id: Some("900001".into()),
            match_runner_up_score: Some(0.85),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    async fn hand_pin(
        s: &SqliteStore,
        cache: &ImageCache,
        tvdb: &FakeEnricher,
        pick: Option<(&str, &str)>,
    ) -> Option<HandPin> {
        pin_series_by_hand(
            s,
            &sem(4),
            cache,
            None::<&FakeEnricher>,
            Some(tvdb),
            SHOW_B,
            "Show B",
            pick,
            NOW + 10,
        )
        .await
        .unwrap()
    }

    /// Spec 033 §7 (V896) — confirming a flagged guess keeps the match,
    /// clears the flag and makes it manual, so the refresh TTL never
    /// re-admits it; there is nothing to confirm on an unmatched show.
    #[tokio::test(flavor = "current_thread")]
    async fn confirming_a_flagged_show_makes_its_match_manual() {
        let s = store().await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb();
        assert_eq!(hand_pin(&s, &cache, &tvdb, None).await, None);
        put_titled_episode(&s, 1, SHOW_B, (1, 1), "Opening").await;
        flagged_pin(&s, SHOW_B, "900002").await;

        let pin = hand_pin(&s, &cache, &tvdb, None).await;

        assert_eq!(
            pin,
            Some(HandPin {
                old: Some("tvdb:900002".into()),
                new: "tvdb:900002".into(),
            })
        );
        let row = series_row(&s, SHOW_B).await;
        assert_eq!(row.match_source.as_deref(), Some("manual"));
        assert_eq!(row.match_external_id.as_deref(), Some("900002"));
        assert_eq!(row.match_flag, None);
        assert_eq!(row.match_runner_up_id, None);
        assert!(s
            .series_needing_match(10, i64::MAX)
            .await
            .unwrap()
            .is_empty());
        assert!(tvdb.fetched().is_empty(), "a confirm fetches nothing");
        assert!(pending_ids(&s).await.is_empty(), "the match did not move");
    }

    /// Spec 033 §7 (V896) — picking another show writes its record as a
    /// manual match with the flag cleared and marks the show's episodes,
    /// since their text came from the old show.
    #[tokio::test(flavor = "current_thread")]
    async fn picking_a_show_by_hand_pins_it_and_marks_its_episodes() {
        let s = store().await;
        let (_td, cache) = cache();
        put_titled_episode(&s, 1, SHOW_B, (1, 1), "Opening").await;
        flagged_pin(&s, SHOW_B, "900001").await;
        let tvdb = FakeEnricher::tvdb().with_detail(enriched_overview("A scattered crew."));

        let pin = hand_pin(&s, &cache, &tvdb, Some(("tvdb", "900002"))).await;

        assert_eq!(
            pin,
            Some(HandPin {
                old: Some("tvdb:900001".into()),
                new: "tvdb:900002".into(),
            })
        );
        assert_eq!(tvdb.fetched(), ["900002"]);
        let row = series_row(&s, SHOW_B).await;
        assert_eq!(row.match_source.as_deref(), Some("manual"));
        assert_eq!(row.match_external_id.as_deref(), Some("900002"));
        assert_eq!(row.overview.as_deref(), Some("A scattered crew."));
        assert_eq!(row.match_flag, None);
        assert_eq!(pending_ids(&s).await, [1]);

        let missing = FakeEnricher::tvdb();
        assert_eq!(
            hand_pin(&s, &cache, &missing, Some(("tvdb", "1"))).await,
            None,
            "a record that did not arrive writes nothing"
        );
        assert_eq!(
            series_row(&s, SHOW_B).await.match_external_id.as_deref(),
            Some("900002")
        );
    }

    /// Spec 033 §7 — a hand fix rebuilds its episodes at once, even in a
    /// dry run, where the background pass rebuilds nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hand_fix_rebuilds_its_episodes_in_a_dry_run() {
        let s = store().await;
        let (_td, cache) = cache();
        put_titled_episode(&s, 1, SHOW_B, (1, 1), "Opening").await;
        let mut item = s.get(1).await.unwrap();
        item.metadata.overview = Some("The old pilot.".into());
        s.put(item).await.unwrap();
        pin(&s, SHOW_B, "900002", NOW).await;
        let tvdb = FakeEnricher::tvdb().with_detail(enriched_overview("The new pilot."));

        let rebuilt = rebuild_now(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &MetadataConfig::default(),
            &[1],
            NOW,
        )
        .await
        .unwrap();

        assert_eq!(rebuilt, 1);
        assert_eq!(
            s.get(1).await.unwrap().metadata.overview.as_deref(),
            Some("The new pilot.")
        );
        assert!(pending_ids(&s).await.is_empty());
    }

    const SHOW_A: &str = "/tv/Show A";

    /// Show A folder: an unyeared Season 1 file titled like the 1983 show's
    /// pilot, a Season 3 file stating 2007, and the stored 1983 match.
    async fn show_a_folder(s: &SqliteStore) {
        put_titled_episode(s, 1, SHOW_A, (1, 1), "Pilot").await;
        let mut modern = MediaItem {
            id: 2,
            path: format!("{SHOW_A}/Season 3/Show A (2007) - S03E01 - The Third Way.mkv").into(),
            title: "The Third Way".into(),
            kind: MediaKind::Episode,
            ..MediaItem::default()
        };
        modern.series = Some(pharos_core::SeriesInfo {
            series_name: "Show A".into(),
            season_number: Some(3),
            episode_number: Some(1),
            series_folder: Some(SHOW_A.into()),
            series_year: None,
            series_key: None,
        });
        s.put(modern).await.unwrap();
        s.upsert_series_metadata(SeriesMetadata {
            series_key: SHOW_A.into(),
            series_name: "Show A".into(),
            match_provider: Some("tvdb".into()),
            match_external_id: Some("900004".into()),
            match_source: Some("search".into()),
            premiere_date: Some(432_864_000), // 1983-09-20
            metadata_refreshed_at: Some(NOW),
            ..Default::default()
        })
        .await
        .unwrap();
        set_version(s, TV_MATCH_VERSION_KEY, "1").await;
    }

    fn show_a_tvdb() -> FakeEnricher {
        FakeEnricher::tvdb()
            .with_search(vec![
                ("900005", "Show A", Some(2007)),
                ("900004", "Show A", Some(1983)),
            ])
            .with_list("900005", &[(1, 1, "The New Start")])
            .with_list("900004", &[(1, 1, "Pilot")])
    }

    /// Spec 033 §2 (V893, B932) — the survey finds the unyeared file whose
    /// title is the 1983 show's pilot, not the 2007 show's. A dry run only
    /// logs it; `apply` records the file's `scanner` row with 1983 and the
    /// owner row keeping the folder key for the stored 1983 match, marks the
    /// file, and neither re-asks the folder nor marks the 2007 file before
    /// the scan has split them.
    #[tokio::test(flavor = "current_thread")]
    async fn episode_titles_split_off_a_same_named_shows_files() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();
        let s = store().await;
        show_a_folder(&s).await;
        let tvdb = show_a_tvdb();

        gate(&s, &tvdb, &MetadataConfig::default()).await;
        assert!(s
            .series_overrides(&[SHOW_A.into()])
            .await
            .unwrap()
            .is_empty());
        assert!(pending_ids(&s).await.is_empty());
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("file's episode title names a same-named show"))
            .unwrap_or_else(|| panic!("no title evidence line in {text}"));
        for want in [
            "rival_id=\"900004\"",
            "rival_year=1983",
            "own_title=\"The New Start\"",
        ] {
            assert!(line.contains(want), "{want} missing from {line}");
        }
        assert_eq!(
            gauge_value(
                &snap,
                "pharos_series_identity_repair_selected",
                &[
                    ("kind", "episode"),
                    ("reason", "episode_titles"),
                    ("mode", "dry_run")
                ]
            ),
            Some(1.0)
        );

        gate(&s, &tvdb, &apply_cfg()).await;
        let mut rows = s.series_overrides(&[SHOW_A.into()]).await.unwrap();
        rows.sort_by_key(|o| o.path.is_some());
        let got: Vec<_> = rows
            .iter()
            .map(|o| (o.path.clone(), o.target_year, o.source))
            .collect();
        assert_eq!(
            got,
            [
                (None, Some(1983), pharos_core::OverrideSource::Scanner),
                (
                    Some(format!("{SHOW_A}/Season 1/Show A - S01E01 - Pilot.mkv")),
                    Some(1983),
                    pharos_core::OverrideSource::Scanner
                ),
            ]
        );
        assert_eq!(pending_ids(&s).await, [1]);
        assert!(s
            .series_needing_match(10, NOW - 1)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            version(&s, TV_MATCH_VERSION_KEY).await.as_deref(),
            Some("2")
        );
    }

    /// A bare folder holding a 1991 show and its 2015 remake (B987):
    /// S01E01 "Pilot" names both shows' pilots, so it does not move, but it
    /// joins the 1991 owner while its stored episode may be the 2015 show's.
    async fn remake_folder_with_pilot(s: &SqliteStore, pilot_aired: i64) {
        put_titled_episode(s, 1, SHOW_A, (1, 1), "Pilot").await;
        let mut pilot = s.get(1).await.unwrap();
        pilot.metadata.premiere_date = Some(pilot_aired);
        s.put(pilot).await.unwrap();
        put_titled_episode(s, 3, SHOW_A, (1, 2), "The Old Second").await;
        let mut modern = MediaItem {
            id: 2,
            path: format!("{SHOW_A}/Season 3/Show A (2015) - S03E01 - The New Start.mkv").into(),
            title: "The New Start".into(),
            kind: MediaKind::Episode,
            ..MediaItem::default()
        };
        modern.series = Some(pharos_core::SeriesInfo {
            series_name: "Show A".into(),
            season_number: Some(3),
            episode_number: Some(1),
            series_folder: Some(SHOW_A.into()),
            series_year: None,
            series_key: None,
        });
        s.put(modern).await.unwrap();
        s.upsert_series_metadata(SeriesMetadata {
            series_key: SHOW_A.into(),
            series_name: "Show A".into(),
            match_provider: Some("tvdb".into()),
            match_external_id: Some("900091".into()),
            match_source: Some("search".into()),
            premiere_date: Some(PILOT_1991),
            metadata_refreshed_at: Some(NOW),
            ..Default::default()
        })
        .await
        .unwrap();
        set_version(s, TV_MATCH_VERSION_KEY, "1").await;
    }

    const PILOT_2015: i64 = 1_425_340_800; // 2015-03-03
    const PILOT_1991: i64 = 670_550_400; // 1991-04-02

    fn remake_tvdb_dated() -> FakeEnricher {
        FakeEnricher::tvdb()
            .with_search(vec![
                ("900015", "Show A", Some(2015)),
                ("900091", "Show A", Some(1991)),
            ])
            .with_dated_list(
                "900015",
                &[
                    (1, 1, "Pilot", PILOT_2015),
                    (1, 2, "The New Second", 1_425_945_600),
                ],
            )
            .with_dated_list(
                "900091",
                &[
                    (1, 1, "Pilot", PILOT_1991),
                    (1, 2, "The Old Second", 671_155_200),
                ],
            )
    }

    /// B987 — a file that stays with a title split's owner, whose show is not
    /// the year the folder's files state, holds the other show's episode: it
    /// is marked with the moved files, and the dry run names it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_file_joining_the_owner_with_the_other_shows_episode_is_rebuilt() {
        let (logs, _log) = log_capture();
        let s = store().await;
        remake_folder_with_pilot(&s, PILOT_2015).await;
        let tvdb = remake_tvdb_dated();

        gate(&s, &tvdb, &MetadataConfig::default()).await;
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("joins the owner's show holding another show's episode"))
            .unwrap_or_else(|| panic!("no owner joiner line in {text}"));
        for want in [
            "S01E01 - Pilot.mkv",
            "owner_id=\"900091\"",
            "owner_year=1991",
            &format!("stored_aired={PILOT_2015}"),
            &format!("owner_aired={PILOT_1991}"),
        ] {
            assert!(line.contains(want), "{want} missing from {line}");
        }

        gate(&s, &tvdb, &apply_cfg()).await;
        assert_eq!(pending_ids(&s).await, [1, 3]);
    }

    /// B987 — the same file already holding the owner's episode is left alone.
    #[tokio::test(flavor = "current_thread")]
    async fn a_file_joining_the_owner_with_its_episode_is_not_rebuilt() {
        let (logs, _log) = log_capture();
        let s = store().await;
        remake_folder_with_pilot(&s, PILOT_1991).await;
        let tvdb = remake_tvdb_dated();

        gate(&s, &tvdb, &apply_cfg()).await;
        assert_eq!(pending_ids(&s).await, [3]);
        assert!(!logs.text().contains("joins the owner's show"));
    }

    /// A row the scanner moved to a key with no match row yet is not rebuilt:
    /// it stays marked, behind the rest, until its new show is matched.
    #[tokio::test(flavor = "current_thread")]
    async fn a_moved_episode_waits_for_its_new_shows_match() {
        let s = store().await;
        put_titled_episode(&s, 1, SHOW_A, (3, 1), "The Third Way").await;
        let mut item = s.get(1).await.unwrap();
        item.metadata.overview = Some("new text".into());
        if let Some(series) = item.series.as_mut() {
            series.series_key = Some(format!("{SHOW_A}\u{1f}2007"));
        }
        s.put(item).await.unwrap();
        s.mark_reconcile_items(&[1], NOW).await.unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb();

        let (n, pending) = reconcile_pass(
            &s,
            &sem(4),
            &cache,
            None::<&FakeEnricher>,
            Some(&tvdb),
            &apply_cfg(),
            NOW + 5,
        )
        .await
        .unwrap();

        assert_eq!((n, pending), (0, true));
        assert!(tvdb.fetched().is_empty());
        assert_eq!(
            s.get(1).await.unwrap().metadata.overview.as_deref(),
            Some("new text")
        );
        assert_eq!(
            s.series_reconcile_pending(10).await.unwrap(),
            [(1, NOW + 5)]
        );
    }

    /// The pre-B557 install with no version recorded still re-admits every
    /// searched show, as version 1 did.
    #[tokio::test]
    async fn an_unrecorded_matcher_version_still_re_admits_every_searched_show() {
        let s = store().await;
        put_episode(&s, 1, "Show F", "/tv/Show F").await;
        pin(&s, "/tv/Show F", "900010", NOW).await;
        readmit(&s).await;
        assert_eq!(s.series_needing_match(10, NOW - 1).await.unwrap().len(), 1);
        assert_eq!(
            version(&s, TV_MATCH_VERSION_KEY).await.as_deref(),
            Some("2")
        );
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

    /// A Show C (1973) episode whose NFO id's record belongs to show
    /// `record_series`; the folder is pinned to the 1973 show (900006).
    async fn show_c_episode_with_record_of(
        record_series: &str,
    ) -> (u64, LogBuf, MediaItem, Vec<String>) {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();
        let s = store().await;
        pin_series(&s, "/tv/Show C", "900006").await;
        let mut item = MediaItem {
            id: 7,
            path: "/tv/Show C/Season 01/Show C - S01E01.mkv".into(),
            title: "The New Start".into(),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "Show C".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some("/tv/Show C".into()),
                series_year: None,
                series_key: None,
            }),
            ..MediaItem::default()
        };
        item.metadata.overview = Some("The new premiere.".into());
        // A Jellyfin episode NFO's own TVDB id, of the remake's episode.
        item.metadata.provider_ids.tvdb = Some("900009".into());
        item.metadata.provider_ids.imdb = Some("tt900001".into());
        s.put(item.clone()).await.unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_detail_for(
                "900009",
                EnrichedMetadata {
                    title: Some("The New Start".into()),
                    overview: Some("The new premiere.".into()),
                    series_id: Some(record_series.into()),
                    ..EnrichedMetadata::default()
                },
            )
            .with_detail_for(
                "900006",
                EnrichedMetadata {
                    title: Some("The Old Second".into()),
                    overview: Some("The old premiere.".into()),
                    episode_number: Some((1, 1)),
                    series_id: Some("900006".into()),
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

    /// Spec 033 V892 (B931) — the Show C (1973) folder is matched to the 1973
    /// show, but its episode NFOs carry the 2007 remake's ids. The NFO's
    /// record is set aside with its text and ids, the episode is read from
    /// the pinned show by number, and the rejection is counted and logged.
    #[tokio::test(flavor = "current_thread")]
    async fn an_episode_nfo_of_another_series_is_set_aside() {
        let (n, logs, ep, fetched) = show_c_episode_with_record_of("900007").await;
        assert_eq!(n, 1);
        assert_eq!(fetched, vec!["900009", "900006"]);
        assert_eq!(ep.title, "The Old Second");
        assert_eq!(ep.metadata.overview.as_deref(), Some("The old premiere."));
        assert_eq!(ep.metadata.provider_ids.imdb, None, "the remake's ids go");
        assert_ne!(ep.metadata.provider_ids.tvdb.as_deref(), Some("900009"));
        assert_eq!(ep.match_source.as_deref(), Some("search"));
        assert_eq!(ep.match_external_id.as_deref(), Some("900006"));
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("names another series"))
            .unwrap_or_else(|| panic!("no rejection line in {text}"));
        for want in [
            "record_series_id=\"900007\"",
            "pinned_series_id=900006",
            "nfo_id=900009",
            "reason=\"nfo_other_series\"",
            "Show C - S01E01.mkv",
        ] {
            assert!(line.contains(want), "{want} missing from {line}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_episode_nfo_of_its_own_show_is_used() {
        let (n, _, ep, fetched) = show_c_episode_with_record_of("900006").await;
        assert_eq!(n, 0);
        assert_eq!(fetched, vec!["900009"]);
        assert_eq!(ep.match_source.as_deref(), Some("nfo_id"));
        assert_eq!(ep.metadata.provider_ids.tvdb.as_deref(), Some("900009"));
    }

    /// V892 (B933) — an episode never searches on its own: it is read from
    /// its show's pinned match by number, whatever a search would say.
    #[tokio::test]
    async fn an_episode_is_read_from_its_shows_pinned_match_not_searched() {
        let s = store().await;
        put_bare_episode(&s, 41, "Show I", "/tv/Show I").await;
        pin_series(&s, "/tv/Show I", "900013").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_search(vec![("999", "Show J", None)])
            .with_detail(EnrichedMetadata {
                title: Some("Twice Told".into()),
                series_id: Some("900013".into()),
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
        assert_eq!(tvdb.fetched(), vec!["900013"]);
        let ep = s.get(41).await.unwrap();
        assert_eq!(ep.match_external_id.as_deref(), Some("900013"));
    }

    /// V892 — a show pinned on TMDB (TVDB had no match) has its episodes read
    /// from TMDB even when TVDB is configured.
    #[tokio::test]
    async fn an_episode_uses_its_shows_pinned_provider() {
        let s = store().await;
        put_bare_episode(&s, 42, "Show G", "/tv/Show G").await;
        s.upsert_series_metadata(SeriesMetadata {
            series_key: "/tv/Show G".into(),
            series_name: "Show G".into(),
            match_provider: Some("tmdb".into()),
            match_external_id: Some("900015".into()),
            match_source: Some("search".into()),
            metadata_refreshed_at: Some(NOW),
            ..Default::default()
        })
        .await
        .unwrap();
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb().with_detail(EnrichedMetadata::default());
        let tmdb = FakeEnricher::tmdb().with_detail(EnrichedMetadata {
            title: Some("Renewal".into()),
            series_id: Some("900015".into()),
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
        assert_eq!(tmdb.fetched(), vec!["900015"]);
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
        put_bare_episode(&s, 44, "Show C", "/tv/Show C").await;
        let eligible = |items: Vec<MediaItem>| items.iter().any(|i| i.id == 44);
        assert!(!eligible(s.items_needing_match(100, NOW).await.unwrap()));
        pin_series(&s, "/tv/Show C", "900006").await;
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
            path: "/tv/Show A/Season 01/Show A - S01E01 - Pilot.mkv".into(),
            title: "Pilot".into(),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "Show A".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some("/tv/Show A".into()),
                series_year: None,
                series_key: None,
            }),
            ..MediaItem::default()
        };
        s.put(item).await.unwrap();
        pin_series(&s, "/tv/Show A", "900005").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb().with_detail(EnrichedMetadata {
            title: Some("The New Start".into()),
            neighbour_titles: vec!["Quick Start".into()],
            series_id: Some("900005".into()),
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

    /// One episode file of an invented show, as the scanner keys it: the
    /// folder's `(YYYY)` is its stored year and is not in its name.
    async fn put_show_episode(
        store: &SqliteStore,
        id: u64,
        folder: &str,
        (season, episode): (u32, u32),
        file_show: &str,
    ) {
        let base = folder.rsplit('/').next().unwrap_or(folder);
        let year = pharos_scanner::series_split::stated_show_year(std::path::Path::new(&format!(
            "{folder}/x.mkv"
        )))
        .filter(|_| base.ends_with(')'));
        let name = match year {
            Some(y) => base.trim_end_matches(&format!(" ({y})")).to_string(),
            None => base.to_string(),
        };
        let item = MediaItem {
            id,
            path: format!(
                "{folder}/Season {season}/{file_show} - S{season:02}E{episode:02} - Part {episode}.mkv"
            )
            .into(),
            title: format!("Part {episode}"),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: name,
                season_number: Some(season),
                episode_number: Some(episode),
                series_folder: Some(folder.into()),
                series_year: year,
                series_key: None,
            }),
            ..MediaItem::default()
        };
        store.put(item).await.unwrap();
    }

    /// B990 — a provider's search narrowed by a year keeps only shows it
    /// dates that year, so a show stated as 2003 that the provider dates
    /// 2005 is lost. A year search with no candidate is asked again without
    /// the year, and its candidates ranked by how near their year is.
    #[tokio::test(flavor = "current_thread")]
    async fn a_year_search_with_no_candidate_is_asked_again_without_the_year() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();
        let s = store().await;
        const SHOW: &str = "/tv/Show A (2003)";
        put_show_episode(&s, 1, SHOW, (1, 1), "Show A (2003)").await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_year_search(vec![])
            .with_search(vec![
                ("920001", "Show A", Some(1978)),
                ("920002", "Show A", Some(2009)),
                ("920003", "Show A", Some(2005)),
                ("920004", "Show A", Some(2004)),
            ])
            .with_detail(enriched_overview("A fleet."));

        series_pass(&s, &cache, &tvdb).await;

        assert_eq!(tvdb.years(), [Some(2003), None]);
        let row = series_row(&s, SHOW).await;
        assert_eq!(row.match_source.as_deref(), Some("search"));
        // Both near shows read as 2003 and tie: with no episode list to
        // tell them apart, the nearer year goes first and the call is
        // flagged as close.
        assert_eq!(row.match_external_id.as_deref(), Some("920004"));
        assert_eq!(row.match_flag.as_deref(), Some(AMBIGUOUS_FLAG));
        assert_eq!(
            counter_value(
                &snap,
                "pharos_series_identity_repair_rematch_total",
                &[("step", "year_retry")]
            ),
            1
        );
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("searched again without it"))
            .unwrap_or_else(|| panic!("no retry line in {text}"));
        assert!(line.contains("year=2003"), "{line}");
        assert!(line.contains("920003 (2005)"), "{line}");
    }

    /// B990 — a bare folder whose files state the remake's year, stored on
    /// the original, is re-asked as `year_mismatch`; under `apply` the
    /// re-match searches with the year the files state, so the remake (named
    /// with that year, dated two years later) wins over the original's bare
    /// exact name.
    #[tokio::test(flavor = "current_thread")]
    async fn a_year_mismatch_re_ask_re_matches_with_the_files_year() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();
        let s = store().await;
        const SHOW: &str = "/tv/Show A";
        put_show_episode(&s, 1, SHOW, (1, 1), "Show A (2003)").await;
        put_show_episode(&s, 2, SHOW, (1, 2), "Show A (2003)").await;
        pin(&s, SHOW, "920010", NOW).await;
        let mut row = series_row(&s, SHOW).await;
        // 1978-09-17
        row.premiere_date = Some(274_838_400);
        s.upsert_series_metadata(row).await.unwrap();
        set_version(&s, TV_MATCH_VERSION_KEY, "1").await;
        let tvdb = FakeEnricher::tvdb()
            .with_year_search(vec![])
            .with_search(vec![
                ("920010", "Show A", Some(1978)),
                ("920011", "Show A (2003)", Some(2005)),
                ("920012", "Show A: The Return", Some(1980)),
            ])
            .with_detail(enriched_overview("A fleet."))
            // The remake's record dates it by its 2005 series.
            .with_detail_for(
                "920011",
                EnrichedMetadata {
                    overview: Some("A fleet.".into()),
                    // 2005-01-14
                    premiere_date: Some(1_105_660_800),
                    ..EnrichedMetadata::default()
                },
            );
        let (_td, cache) = cache();

        gate(&s, &tvdb, &apply_cfg()).await;
        assert_eq!(
            s.series_needing_match(10, NOW - 1).await.unwrap()[0].series_key,
            SHOW,
            "re-asked"
        );
        series_pass(&s, &cache, &tvdb).await;

        let row = series_row(&s, SHOW).await;
        assert_eq!(row.match_external_id.as_deref(), Some("920011"));
        assert!(tvdb.years().contains(&Some(2003)), "{:?}", tvdb.years());
        let mut pending = pending_ids(&s).await;
        pending.sort_unstable();
        assert_eq!(pending, [1, 2], "the match change marks its episodes");
        assert_eq!(
            counter_value(
                &snap,
                "pharos_series_identity_repair_rematch_total",
                &[("step", "stated_year")]
            ),
            1
        );
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("with the year the show's files state"))
            .unwrap_or_else(|| panic!("no stated-year line in {text}"));
        for want in ["year=2003", "chosen=\"920011\""] {
            assert!(line.contains(want), "{want} missing from {line}");
        }
        let read = text
            .lines()
            .find(|l| l.contains("candidates read as the stated year"))
            .unwrap_or_else(|| panic!("no read line in {text}"));
        assert!(read.contains("920011 2005->2003"), "{read}");

        // The next survey accepts what the re-match chose: a premiere two
        // years from the stated year is no year mismatch.
        let providers = SeriesProviders::<FakeEnricher, FakeEnricher> {
            tvdb: Some(&tvdb),
            tmdb: None,
        };
        let survey = survey_series_identity(&s, &providers, &sem(4), &apply_cfg())
            .await
            .unwrap();
        assert!(survey.reask.is_empty(), "{:?}", survey.reask);
    }

    const SHORT_SHOW: &str = "/tv/Show A (2003)";
    const MINI_ID: &str = "920020";

    /// A four-season folder stating 2003, pinned by search to a TMDB
    /// miniseries whose whole run is two days, and re-asked (B986).
    async fn short_pinned_show(s: &SqliteStore) {
        let mut id = 0;
        for season in 1..=4 {
            id += 1;
            put_show_episode(s, id, SHORT_SHOW, (season, 1), "Show A (2003)").await;
        }
        s.upsert_series_metadata(SeriesMetadata {
            series_key: SHORT_SHOW.into(),
            series_name: "Show A".into(),
            match_provider: Some("tmdb".into()),
            match_external_id: Some(MINI_ID.into()),
            match_source: Some("search".into()),
            metadata_refreshed_at: Some(NOW),
            overview: Some("The pilot.".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        s.readmit_series(&[SHORT_SHOW.to_string()]).await.unwrap();
    }

    fn two_day_run() -> crate::online_enrich::SeriesRun {
        crate::online_enrich::SeriesRun {
            // 2003-12-08 .. 2003-12-09
            first: Some(1_070_841_600),
            last: Some(1_070_928_000),
            ongoing: false,
        }
    }

    async fn both_pass(
        s: &SqliteStore,
        cache: &ImageCache,
        tvdb: &FakeEnricher,
        tmdb: &FakeEnricher,
    ) {
        enrich_series_pass(
            s,
            &sem(4),
            cache,
            Some(tvdb),
            Some(tmdb),
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
    }

    /// B990 — a `run_too_short` re-ask leaves the too-short pin out of its
    /// re-match. TVDB's year search finds nothing and neither does its
    /// retry; TMDB's year search finds only the miniseries, which is left
    /// out, so TMDB is asked again without the year and its full series (a
    /// year off the stated year) wins.
    #[tokio::test(flavor = "current_thread")]
    async fn a_run_too_short_re_ask_leaves_the_short_pin_out() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let s = store().await;
        short_pinned_show(&s).await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb();
        let tmdb = FakeEnricher::tmdb()
            .with_year_search(vec![(MINI_ID, "Show A", Some(2003))])
            .with_search(vec![
                (MINI_ID, "Show A", Some(2003)),
                ("920021", "Show A", Some(1978)),
                ("920022", "Show A", Some(2004)),
            ])
            .with_run(MINI_ID, two_day_run())
            .with_detail(enriched_overview("The series."));

        both_pass(&s, &cache, &tvdb, &tmdb).await;

        let row = series_row(&s, SHORT_SHOW).await;
        assert_eq!(row.match_external_id.as_deref(), Some("920022"));
        assert_eq!(row.match_flag, None);
        assert_eq!(tmdb.years(), [Some(2003), None]);
        assert_eq!(
            pending_ids(&s).await.len(),
            4,
            "the match change marks them"
        );
        assert_eq!(
            counter_value(
                &snap,
                "pharos_series_identity_repair_rematch_total",
                &[("step", "pin_excluded")]
            ),
            1
        );
    }

    /// B990 — the same re-ask on TVDB: its year search finds nothing, its
    /// retry finds the full series dated a year after the stated year.
    #[tokio::test(flavor = "current_thread")]
    async fn a_run_too_short_re_ask_finds_the_series_by_the_tvdb_retry() {
        let s = store().await;
        short_pinned_show(&s).await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb()
            .with_year_search(vec![])
            .with_search(vec![("920023", "Show A (2003)", Some(2005))])
            .with_detail(enriched_overview("The series."));
        let tmdb = FakeEnricher::tmdb()
            .with_search(vec![(MINI_ID, "Show A", Some(2003))])
            .with_run(MINI_ID, two_day_run());

        both_pass(&s, &cache, &tvdb, &tmdb).await;

        let row = series_row(&s, SHORT_SHOW).await;
        assert_eq!(row.match_provider.as_deref(), Some("tvdb"));
        assert_eq!(row.match_external_id.as_deref(), Some("920023"));
    }

    /// B990 — when the too-short pin is the only candidate left, the re-ask
    /// does not pin it again: the stored match stays, flagged for Manage →
    /// Show identity, its episodes unmarked, and the show is not re-asked
    /// every pass.
    #[tokio::test(flavor = "current_thread")]
    async fn a_run_too_short_re_ask_with_only_the_short_pin_is_flagged() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let (logs, _log) = log_capture();
        let s = store().await;
        short_pinned_show(&s).await;
        let (_td, cache) = cache();
        let tvdb = FakeEnricher::tvdb();
        let tmdb = FakeEnricher::tmdb()
            .with_search(vec![(MINI_ID, "Show A", Some(2003))])
            .with_run(MINI_ID, two_day_run())
            .with_detail(enriched_overview("The pilot again."));

        both_pass(&s, &cache, &tvdb, &tmdb).await;

        let row = series_row(&s, SHORT_SHOW).await;
        assert_eq!(row.match_source.as_deref(), Some("search"));
        assert_eq!(row.match_provider.as_deref(), Some("tmdb"));
        assert_eq!(row.match_external_id.as_deref(), Some(MINI_ID));
        assert_eq!(row.match_flag.as_deref(), Some(AMBIGUOUS_FLAG));
        assert_eq!(
            row.overview.as_deref(),
            Some("The pilot."),
            "not re-written"
        );
        assert_eq!(row.metadata_refreshed_at, Some(NOW));
        assert_eq!(s.flagged_series_keys().await.unwrap(), [SHORT_SHOW]);
        assert_eq!(pending_ids(&s).await, Vec::<u64>::new());
        assert!(tmdb.fetched().is_empty(), "{:?}", tmdb.fetched());
        assert_eq!(
            counter_value(
                &snap,
                "pharos_series_identity_repair_rematch_total",
                &[("step", "flagged_too_short")]
            ),
            1
        );
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("re-ask left the show flagged: only too-short candidates"))
            .unwrap_or_else(|| panic!("no flagged line in {text}"));
        assert!(line.contains(MINI_ID), "{line}");
    }

    /// B990 — `apply` surveys again when the survey revision moved under the
    /// matcher version it already recorded (an install that applied before
    /// the applied marker existed, or under another revision), and not
    /// while the marker is current.
    #[tokio::test(flavor = "current_thread")]
    async fn apply_surveys_again_after_a_survey_revision_bump() {
        let s = store().await;
        const SHOW: &str = "/tv/Show A";
        put_show_episode(&s, 1, SHOW, (1, 1), "Show A (2003)").await;
        async fn stale_pin(s: &SqliteStore) {
            pin(s, SHOW, "920010", NOW).await;
            let mut row = series_row(s, SHOW).await;
            // 1978-09-17
            row.premiere_date = Some(274_838_400);
            s.upsert_series_metadata(row).await.unwrap();
        }
        async fn reasked(s: &SqliteStore) -> bool {
            !s.series_needing_match(10, NOW - 1)
                .await
                .unwrap()
                .is_empty()
        }
        let want = TV_MATCH_VERSION.to_string();
        let current = dry_run_marker(&want);
        let tvdb = FakeEnricher::tvdb();

        for stale in [None, Some("2.r1")] {
            stale_pin(&s).await;
            set_version(&s, TV_MATCH_VERSION_KEY, &want).await;
            if let Some(m) = stale {
                set_version(&s, TV_MATCH_SURVEY_APPLIED_KEY, m).await;
            }
            gate(&s, &tvdb, &apply_cfg()).await;
            assert!(reasked(&s).await, "marker {stale:?} was not re-surveyed");
            assert_eq!(
                version(&s, TV_MATCH_SURVEY_APPLIED_KEY).await,
                Some(current.clone())
            );
        }

        stale_pin(&s).await;
        gate(&s, &tvdb, &apply_cfg()).await;
        assert!(!reasked(&s).await, "a current marker surveys nothing");
        assert_eq!(version(&s, TV_MATCH_VERSION_KEY).await, Some(want));
    }
}
