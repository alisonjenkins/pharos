# Online metadata & artwork enrichment (TMDB) — design

- **Date:** 2026-07-22
- **Status:** approved (design), pending implementation plan
- **Scope:** Phase 1 — Movies + TV via TMDB. Music (MusicBrainz / Cover Art
  Archive / fanart.tv) is explicitly **Phase 2**, out of scope here.

## Problem

pharos resolves metadata **local-first only**: `MetadataResolver` merges NFO,
sidecar, filename, and embedded-container providers, but has no *online*
source for movie/TV metadata or artwork. Items without a curated NFO show a
filename-derived title, no overview/genres/cast/ratings, and no poster/backdrop
unless a sidecar image happens to sit next to the file. The one online path
that exists (`crates/pharos-server/src/tmdb.rs`, T81) resolves **cast portraits
only**, by name → TMDB CDN URL (302 redirect, no download).

Goal: fill those gaps for Movies and TV from TMDB — posters, backdrops, logos,
overview, genres, ratings, cast, and full TV depth (series + season + episode)
— while (a) never overriding curated local data, (b) never slowing scans, and
(c) never contending with live playback.

## Decisions (locked with the user)

1. **First target:** Movies + TV via TMDB. Music is Phase 2.
2. **Matching:** exact id (NFO) → parsed title+year TMDB search → no confident
   hit leaves filename metadata. Store the matched id + confidence; expose a
   manual Identify/override path to fix wrong matches.
3. **Artwork:** **download + cache locally** at enrichment time; serve bytes
   through the existing image cache/resize pipeline. Offline + private.
   (Deliberately different from T81 person portraits, which 302 to the CDN.)
4. **TV depth:** full — series poster/backdrop/logo/overview/genres/cast,
   per-season posters, per-episode stills/titles/overviews/air-dates matched
   via parsed `SxxExx`.
5. **Orchestration:** **hybrid (Approach C)** — cheap exact-id lookups inline
   during scan; expensive search + episode resolution + artwork download in a
   paced background sweep gated by the shared `BG_IO` semaphore.

## Architecture

One new `MetadataProvider` plus one paced background task, both behind an
extended `TmdbClient`.

| Unit | Responsibility | Depends on | Testable via |
|------|----------------|------------|--------------|
| `TmdbClient` (extend existing) | TMDB HTTP + JSON shapes (search, movie, tv, season, episode, image download) | `reqwest`, API key | live (manual) / trait fake |
| `TmdbProvider` (new, in `pharos-scanner/src/metadata/`) | known id → `MetadataResult` (gap-fill). Pure over a client trait. | `MetadataProvider` trait, client trait | fake client, offline |
| `match_candidate` (new, pure fn) | parsed title/year/`SxxExx` + search results → id + confidence | nothing (pure) | unit tests, offline |
| `metadata_backfill` (new task, `pharos-server`) | pick candidates, match, download art, persist. **Sole writer.** | `Stores`, `BG_IO`, client | fake client + in-memory store |

### Responsibility split (the hybrid)

- **Inline, during scan** — `TmdbProvider` sits in the `MetadataResolver`
  priority chain **below** NFO/sidecar. It acts **only** when the item already
  carries an exact id (`ProviderIds.tmdb`/`imdb`/`tvdb`, typically from an NFO):
  one cheap GET → fills `None` gaps. **No search, no image download inline.**
  Zero added scan latency when no id is present.
- **Background, after scan** — `metadata_backfill` walks unmatched/stale items
  and does the expensive work: title+year **search**, `SxxExx` episode
  resolution, and **artwork download** into the image cache. Each remote call
  draws a `BG_IO` permit (same gate as scans / subtitle-warm / person-image
  backfill) so it paces against live playback, plus a `REQUEST_SPACING`
  courtesy delay under TMDB's rate ceiling (mirrors T81).

### Gating

The whole subsystem no-ops unless `[tmdb].api_key` / `PHAROS_TMDB_API_KEY` is
set — identical to the T81 person-image gate. No key → behaviour is exactly as
today.

## Data model

### Match record (new columns on `media_items`)

| Column | Type | Meaning |
|--------|------|---------|
| `tmdb_id` | INTEGER NULL | resolved TMDB id (movie / series / episode as appropriate) |
| `match_source` | TEXT NULL | `nfo_id` \| `search` \| `manual` \| `none` |
| `match_confidence` | REAL NULL | 0..1 for `search`; 1.0 for `nfo_id` / `manual` |
| `metadata_refreshed_at` | TIMESTAMP NULL | when the enricher last wrote; drives staleness/TTL |

Placement rationale: one-to-one with the item, read on every backfill sweep,
mirrors the just-shipped `has_primary_art` denormalization. `ProviderIds.tmdb`
still carries the id in the metadata blob for provenance, but these columns are
the authoritative match state the sweep reads/writes. New Postgres + SQLite
migrations (`0043_metadata_match`), columns nullable with no backfill needed
(NULL = "never matched" = eligible).

### Override protection (load-bearing invariant)

The backfill sweep **skips any item with `match_source IN ('manual','nfo_id')`**.
A wrong auto-match the user corrected (`manual`) is never clobbered by a later
rescan; a local NFO id (`nfo_id`) is ground truth. Only `search` / `none` /
NULL rows are (re)matched — and those only when `metadata_refreshed_at` is
NULL or older than a configurable TTL, so already-matched items aren't
re-fetched every sweep.

## Matching (`match_candidate`, pure function)

1. Exact id present (from NFO) → `nfo_id`, confidence 1.0. (This is the inline
   `TmdbProvider` path; the id is recorded so the sweep skips it.)
2. Else parse title + year (existing filename parser) → `GET /search/movie` or
   `/search/tv`.
3. Score each result: title similarity (normalized Levenshtein) × year match
   (exact = full weight, ±1 = partial, none = penalty). Take the best.
4. Best score **≥ 0.7** → `search`, store id + confidence. Below → `none`,
   leave filename-derived metadata, and surface the item in a "needs identify"
   list.

The 0.7 threshold is an initial value, tunable; edge cases (ambiguous
title, missing year, remakes sharing a title) are covered by unit tests.

## TV depth

- **Series** matched once via `/search/tv` → series id.
- **Season:** `GET /tv/{id}/season/{n}` → per-season poster + overview.
- **Episode:** parsed `SxxExx` → `GET /tv/{id}/season/{n}/episode/{e}` →
  still / title / overview / air-date. The episode row stores its own
  `tmdb_id` (the episode id) with `match_source = search` at the series'
  confidence.
- **Synthetic Series/Season items** resolve artwork through the existing
  representative-episode path, extended to prefer the series/season TMDB
  artwork when present.

## Precedence (unchanged)

`MetadataResolver` merges scalars first-`Some`-wins, so a local NFO field
always beats TMDB; TMDB fills only `None` gaps. `Vec` fields (genres / cast /
studios) union + dedup keeping priority order, exactly as today. `TmdbProvider`
is registered at a priority **below** NFO/sidecar/filename.

## Artwork: download, cache, serve

### Download & cache

After a confident match, `metadata_backfill` pulls artwork URLs from the TMDB
response (poster / backdrop / logo for movie + series; poster per season;
still per episode). Each is fetched **once** via `TmdbClient` and written into
the **existing image cache** keyed by `(item_id, role)`, reusing the
resize/webp/avif pipeline untouched. Downloads are `BG_IO`-gated and capped
per pass; any images dropped past the cap are `log`-ged (no silent
truncation).

### Serving

Today `local_artwork_path` serves only `source='local'`. Downloaded TMDB
artwork is registered in the `artwork` table via `set_artwork` (already the
table's sole writer + maintainer of `has_primary_art`) with a new
`source='tmdb'`. A downloaded poster is then a cached local file the existing
`/Items/{id}/Images/...` route serves like any sidecar — `ArtworkSource::Url`
stays in the metadata blob for provenance, but served bytes are always local
(offline + private).

### `has_primary_art` interplay

`set_artwork(role=Primary, source='tmdb')` means bytes are genuinely on disk,
so it must flip `has_primary_art = true`. Adjust the just-shipped rule from
`source == 'local'` to `source IN ('local','tmdb')` (both mean "bytes on
disk"). Coverless audio stays `false`; a TMDB-postered movie/series correctly
advertises Primary.

## Manual Identify / override

- `GET /Items/{id}/RemoteSearch/{Movie|Series}` (Jellyfin-shape) → TMDB search
  candidates for a client / pharos-ui pick-list.
- `POST /Items/{id}/RemoteSearch/Apply` — body carries the chosen `tmdb_id`.
  Handler: set `tmdb_id`, `match_source = 'manual'`, confidence 1.0, and
  enqueue an **immediate targeted backfill** for just that item (re-fetch
  metadata + artwork).

jellyfin-web's existing Identify dialog drives these same routes, so the UI is
free.

## Trigger cadence

`metadata_backfill` runs:

- **(a)** once at boot after the scan settles (mirrors T81 `spawn`);
- **(b)** after each incremental scan, for newly-added / `none` / NULL items;
- **(c)** on-demand for a manual apply.

Idempotent: `manual` / `nfo_id` skipped; `metadata_refreshed_at` + TTL prevents
constant re-fetching of already-matched items; a resolved image excludes its
row from the "needs artwork" query so a restart safely retries only unresolved
work (same self-terminating shape as T81).

## Error handling / isolation (V6)

- A provider `Err` is already logged-and-skipped by `MetadataResolver`; the
  inline `TmdbProvider` inherits that — a TMDB blip never fails a scan.
- The background sweep is fire-and-forget: a failure aborts only that sweep
  (logged `warn`), never the server, and the next trigger retries.
- All remote calls are best-effort with transport/decode errors → skip that
  item, leaving prior (filename/NFO) metadata intact.

## Config

Extend `[tmdb]` (no new secret plumbing — reuses `PHAROS_TMDB_API_KEY`):

| Key | Default | Meaning |
|-----|---------|---------|
| `[tmdb].api_key` | none | already exists; gates the whole subsystem |
| `[tmdb].match_min_confidence` | 0.7 | search-match acceptance threshold |
| `[tmdb].refresh_ttl_days` | e.g. 30 | staleness window for re-matching `search`/`none` rows |
| `[tmdb].max_per_pass` | e.g. 5000 | bound on one sweep (mirrors T81 `MAX_PER_PASS`) |

## Testing

- `match_candidate` — pure unit tests: title/year scoring, threshold edges,
  `SxxExx` parsing, remakes/ambiguous titles.
- `TmdbProvider` — fake client (id → result mapping), asserts gap-fill only,
  never overrides a higher-priority provider.
- `metadata_backfill` — fake client + in-memory store: idempotency (`manual`
  never clobbered), no re-download within TTL, self-termination.
- `has_primary_art` — a store test that `set_artwork(Primary, 'tmdb')` flips
  the flag true; a wire/golden test that a TMDB-postered item advertises
  Primary and serves cached bytes.
- Migrations — round-trip test on both Postgres and SQLite (remember the
  `--features postgres --all-targets` build so postgres-gated literals compile).

## Out of scope (this spec)

- Music metadata/artwork (Phase 2: MusicBrainz + Cover Art Archive / fanart.tv).
- Hardware/HW-agnostic image transforms beyond the existing pipeline.
- Bulk re-identify UI beyond the per-item Identify dialog.
- Collections/box-set enrichment (can follow once movie matching is proven).
