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
//! `manual` or `nfo_id`-sourced row is NEVER reprocessed — a user override or
//! a local NFO id survives every pass. A `search`/`none` row is re-admitted
//! only once its `metadata_refreshed_at` predates the TTL cutoff.
//!
//! [`items_needing_match`]: pharos_core::MediaStore::items_needing_match

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::bg_io::BgPermit;
use crate::config::MetadataConfig;
use crate::online_enrich::{
    apply_enrichment, download_and_cache_art, EnrichedMetadata, OnlineEnricher, RemoteArt,
};
use crate::state::Stores;
use crate::tmdb::TmdbEnricher;
use crate::tvdb::{ReqwestTransport, TvdbEnricher};
use pharos_cache::ImageCache;
use pharos_core::{
    match_best, ArtworkRole, DomainResult, GenreStore, MediaItem, MediaKind, MediaStore,
    PersonStore, ProviderIds,
};
use pharos_scanner::FilenameProvider;

/// Courtesy delay between items — well under either provider's published rate
/// ceiling so a full backfill never trips limiting. This is on top of the
/// `bg_io` gate (which throttles against playback, not the remote API). Mirrors
/// T81's 120ms.
const REQUEST_SPACING: Duration = Duration::from_millis(120);

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

/// Spawn the one-pass enrichment sweep on the tokio runtime. Fire-and-forget:
/// a failure aborts only this sweep (logged), never the server. Mirrors
/// [`crate::person_image_backfill::spawn`]. `now` is computed once here and
/// threaded through so every item in the pass shares one timestamp.
pub fn spawn(
    stores: Stores,
    bg_io: Arc<Semaphore>,
    cache: Arc<ImageCache>,
    tmdb: Option<TmdbEnricher>,
    tvdb: Option<TvdbEnricher<ReqwestTransport>>,
    cfg: MetadataConfig,
) {
    tokio::spawn(async move {
        let now = now_secs();
        match run(
            &stores,
            &bg_io,
            cache.as_ref(),
            tmdb.as_ref(),
            tvdb.as_ref(),
            &cfg,
            now,
        )
        .await
        {
            Ok(n) => tracing::info!(enriched = n, "T9 metadata backfill: complete"),
            Err(e) => tracing::warn!(error = %e, "T9 metadata backfill: aborted"),
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
pub(crate) async fn run<Tm, Tv, S>(
    store: &S,
    bg_io: &Arc<Semaphore>,
    cache: &ImageCache,
    tmdb: Option<&Tm>,
    tvdb: Option<&Tv>,
    cfg: &MetadataConfig,
    now: i64,
) -> DomainResult<usize>
where
    Tm: OnlineEnricher,
    Tv: OnlineEnricher,
    S: MediaStore + GenreStore + PersonStore,
{
    // No provider configured → nothing to do (mirrors spawn's key gate).
    if tmdb.is_none() && tvdb.is_none() {
        return Ok(0);
    }
    // Items whose last enrichment predates this cutoff (or never matched) are
    // eligible; `manual`/`nfo_id` rows are excluded by the query itself.
    let ttl_cutoff = now.saturating_sub(i64::from(cfg.refresh_ttl_days) * 86_400);
    let items = store
        .items_needing_match(cfg.max_per_pass, ttl_cutoff)
        .await?;
    let total = items.len();
    if total == 0 {
        return Ok(0);
    }
    tracing::info!(total, "T9 metadata backfill: enriching items");
    let mut enriched = 0usize;
    for item in items {
        // V6 — one bad item (a provider blip, a store hiccup) never aborts the
        // pass; log it and carry on to the next.
        match enrich_one(store, bg_io, cache, tmdb, tvdb, cfg, item, now).await {
            Ok(true) => enriched += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "T9 metadata backfill: item failed"),
        }
        tokio::time::sleep(REQUEST_SPACING).await;
    }
    Ok(enriched)
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
    /// A transient miss (fetch returned nothing for a resolved id). Leave the
    /// row untouched so the next pass retries.
    Transient,
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
        MediaKind::Audio => return Ok(false),
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
            None => return Ok(false),
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
                return Ok(false);
            }
        }
        MediaKind::Audio => return Ok(false),
    };

    let (external_id, source, confidence, enriched) = match resolved {
        Resolved::NoMatch => {
            // No confident hit — record `none` (leaves filename metadata) so
            // the row isn't re-searched until the TTL re-admits it. Guard
            // against a concurrent manual apply that landed while this
            // item's search was in flight (FR1 TOCTOU) — a user override
            // must never be reverted by the sweep's trailing write.
            if is_manual(store, item.id).await {
                tracing::debug!(
                    media.id = item.id,
                    "T9 metadata backfill: skipping none-write, item matched manually mid-flight"
                );
            } else {
                store
                    .set_item_match(item.id, matched_provider, "", "none", None, now)
                    .await?;
            }
            return Ok(false);
        }
        Resolved::Transient => return Ok(false),
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

    for (prov, art) in &chosen {
        if !CACHED_ART_ROLES.contains(&art.role) {
            tracing::debug!(role = ?art.role, item = item.id, "T9 metadata backfill: skipping art role (not cached)");
            continue;
        }
        if local_roles.contains(&art.role.as_str().to_ascii_lowercase()) {
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
        if let Err(e) = download_and_cache_art(cache, store, &item, prov, art, bytes).await {
            tracing::warn!(error = %e, role = ?art.role, "T9 metadata backfill: art cache failed");
        }
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
/// contract (and, incidentally, `items_needing_match` excludes both
/// `"manual"` and `"nfo_id"` either way — see its doc comment).
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
        None => Resolved::Transient,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_core::{MediaItem, SearchCandidate};
    use pharos_store_sqlx::sqlite::SqliteStore;
    use tempfile::TempDir;

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
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 1);

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
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 1);

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
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
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
            &MetadataConfig::default(),
            NOW,
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            s.get(900_102).await.unwrap().match_source.as_deref(),
            Some("none")
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
}
