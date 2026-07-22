# Online metadata & artwork enrichment (TMDB + TVDB) — design

- **Date:** 2026-07-22
- **Status:** approved (design), pending implementation plan
- **Scope:** Phase 1 — Movies + TV via **TMDB** and **TVDB**. Music
  (MusicBrainz / Cover Art Archive / fanart.tv) is explicitly **Phase 2**,
  out of scope here.

## Problem

pharos resolves metadata **local-first only**: `MetadataResolver` merges NFO,
sidecar, filename, and embedded-container providers, but has no *online*
source for movie/TV metadata or artwork. Items without a curated NFO show a
filename-derived title, no overview/genres/cast/ratings, and no poster/backdrop
unless a sidecar image happens to sit next to the file. The one online path
that exists (`crates/pharos-server/src/tmdb.rs`, T81) resolves **cast portraits
only**, by name → TMDB CDN URL (302 redirect, no download).

Goal: fill those gaps for Movies and TV — posters, backdrops, logos, overview,
genres, ratings, cast, and full TV depth (series + season + episode) — while
(a) never overriding curated local data, (b) never slowing scans, and (c) never
contending with live playback.

## Decisions (locked with the user)

1. **Targets:** Movies + TV. Two online providers:
   - **TMDB** — all Movies (metadata + artwork), and TV artwork (preferred),
     plus TV gap-fill. Also the existing person-portrait path (T81).
   - **TVDB** — authoritative for **TV** series/season/episode *structure* and
     episode metadata (titles, overviews, air-dates, canonical aired ordering).
   Music is Phase 2.
2. **Provider precedence (TV):** local NFO > **TVDB** > **TMDB**. TVDB wins for
   the TV fields it supplies; TMDB fills the remaining gaps and supplies TV
   artwork. All online providers are gap-fill (first-`Some`-wins), so curated
   local data is never overwritten.
3. **TV artwork:** **TMDB preferred, TVDB fallback.** Prefer TMDB
   posters/backdrops/logos (higher-res, cleaner, textless logos); use TVDB
   artwork only where TMDB lacks an image.
4. **Matching:** exact id (NFO `tmdbid`/`tvdbid`/`imdbid`) → parsed title+year
   provider search → no confident hit leaves filename metadata. Store the
   matched provider + id + confidence; expose a manual Identify/override path.
5. **Artwork:** **download + cache locally** at enrichment time; serve bytes
   through the existing image cache/resize pipeline. Offline + private.
   (Deliberately different from T81 person portraits, which 302 to the CDN.)
6. **TV depth:** full — series poster/backdrop/logo/overview/genres/cast,
   per-season posters, per-episode stills/titles/overviews/air-dates matched
   via parsed `SxxExx`.
7. **Orchestration:** **hybrid (Approach C)** — cheap exact-id lookups inline
   during scan; expensive search + episode resolution + artwork download in a
   paced background sweep gated by the shared `BG_IO` semaphore.

## Architecture

Two online clients, two providers, one paced background task.

| Unit | Responsibility | Depends on | Testable via |
|------|----------------|------------|--------------|
| `TmdbClient` (extend existing) | TMDB HTTP + JSON (search, movie, tv, season, episode, image download) | `reqwest`, api key (query param) | live (manual) / trait fake |
| `TvdbClient` (new) | TVDB v4 HTTP + JSON (login→JWT, series, season, episodes, artwork) | `reqwest`, api key (JWT bearer) | live (manual) / trait fake |
| `TmdbProvider` (new) | known id → `MetadataResult` gap-fill (movies + TV) | provider trait, TMDB client trait | fake client, offline |
| `TvdbProvider` (new) | known id → `MetadataResult` gap-fill (**TV only**; returns empty for movies) | provider trait, TVDB client trait | fake client, offline |
| `match_candidate` (pure fn) | parsed title/year/`SxxExx` + search results → provider + id + confidence | nothing (pure) | unit tests, offline |
| `metadata_backfill` (new task, `pharos-server`) | pick candidates, match, download art, persist. **Sole writer.** | `Stores`, `BG_IO`, both clients | fake clients + in-memory store |

Providers live under `crates/pharos-scanner/src/metadata/` next to
`nfo`/`sidecar`/`filename`/`embedded`.

### TVDB v4 client specifics

TVDB v4 differs from TMDB's simple api-key query param: authenticate with
`POST /v4/login {apikey}` → a JWT bearer, valid ~1 month. `TvdbClient` caches
the token in memory and re-logs-in on a 401. All data calls send
`Authorization: Bearer <jwt>`. Base `https://api4.thetvdb.com/v4`; artwork/image
URLs come back as absolute CDN URLs (downloaded, not 302'd).

### Responsibility split (the hybrid)

- **Inline, during scan** — `TmdbProvider` + `TvdbProvider` sit in the
  `MetadataResolver` chain **below** NFO/sidecar and **above** filename, ordered
  TVDB-then-TMDB. Each acts **only** when the item already carries that
  provider's exact id (`ProviderIds.tvdb`/`tmdb`/`imdb`, typically from an NFO):
  one cheap authenticated GET → fills `None` gaps. **No search, no image
  download inline.** Zero added scan latency when no id is present.
- **Background, after scan** — `metadata_backfill` walks unmatched/stale items
  and does the expensive work: title+year **search** (TVDB for TV, TMDB for
  movies), `SxxExx` episode resolution, and **artwork download** into the image
  cache. Each remote call draws a `BG_IO` permit (same gate as scans /
  subtitle-warm / person-image backfill) so it paces against live playback,
  plus a `REQUEST_SPACING` courtesy delay under each provider's rate ceiling
  (mirrors T81).

### Gating

Each provider no-ops unless its key is configured (`[tmdb].api_key` /
`PHAROS_TMDB_API_KEY`, `[tvdb].api_key` / `PHAROS_TVDB_API_KEY`) — same pattern
as the T81 person-image gate. No key → that provider is off; no keys at all →
behaviour is exactly as today. TVDB present but TMDB absent (or vice-versa)
degrades gracefully: whichever provider has a key runs, the other is skipped.

## Data model

### Match record (new columns on `media_items`) — provider-agnostic

| Column | Type | Meaning |
|--------|------|---------|
| `match_provider` | TEXT NULL | `tmdb` \| `tvdb` (extensible: `musicbrainz` in Phase 2) |
| `match_external_id` | TEXT NULL | the provider's id (movie/series/episode as appropriate) |
| `match_source` | TEXT NULL | `nfo_id` \| `search` \| `manual` \| `none` |
| `match_confidence` | REAL NULL | 0..1 for `search`; 1.0 for `nfo_id` / `manual` |
| `metadata_refreshed_at` | TIMESTAMP NULL | when the enricher last wrote; drives staleness/TTL |

The columns record the **primary identity match** — which provider/id resolved
what this item *is*, and how confidently. Per-provider ids also continue to
live in the metadata blob's `ProviderIds` (both a `tmdb` and `tvdb` slot can be
populated for one TV item — e.g. TVDB matched the series, TMDB supplied a
poster; only the authoritative match provider is recorded in `match_provider`).
Placement rationale: one-to-one with the item, read on every backfill sweep,
mirrors the just-shipped `has_primary_art` denormalization. New Postgres +
SQLite migrations (`0043_metadata_match`), columns nullable, no backfill needed
(NULL = "never matched" = eligible).

### Override protection (load-bearing invariant)

The backfill sweep **skips any item with `match_source IN ('manual','nfo_id')`**.
A wrong auto-match the user corrected (`manual`) is never clobbered by a later
rescan; a local NFO id (`nfo_id`) is ground truth. Only `search` / `none` /
NULL rows are (re)matched — and those only when `metadata_refreshed_at` is
NULL or older than a configurable TTL, so already-matched items aren't
re-fetched every sweep.

## Matching (`match_candidate`, pure function)

1. Exact id present (from NFO): `tvdbid` for TV → `match_provider=tvdb`;
   else `tmdbid`/`imdbid` → `match_provider=tmdb`. `match_source=nfo_id`,
   confidence 1.0. (Inline provider path; recorded so the sweep skips it.)
2. Else parse title + year (existing filename parser) and search the
   kind-appropriate provider:
   - **TV** → TVDB series search; if TVDB has no key or no hit, fall back to
     TMDB `/search/tv`.
   - **Movie** → TMDB `/search/movie`.
3. Score each result: title similarity (normalized Levenshtein) × year match
   (exact = full weight, ±1 = partial, none = penalty). Take the best.
4. Best score **≥ 0.7** → `search`, store provider + id + confidence. Below →
   `none`, leave filename-derived metadata, surface the item in a "needs
   identify" list.

The 0.7 threshold is an initial, tunable value; ambiguous-title / missing-year /
remake edge cases are covered by unit tests.

## TV depth

Resolved via **TVDB** (structure/metadata), artwork via **TMDB** (fallback TVDB):

- **Series** matched once (TVDB series search or NFO `tvdbid`) → series id;
  overview/genres/cast from TVDB, poster/backdrop/logo from TMDB (TVDB fallback).
- **Season:** TVDB season record → per-season overview + air structure; season
  poster from TMDB `/tv/{id}/season/{n}` (TVDB fallback).
- **Episode:** parsed `SxxExx` → TVDB episode (aired order) → title / overview /
  air-date; still image from TMDB episode (TVDB fallback). The episode row
  stores its own `match_external_id` (the TVDB episode id) with
  `match_source=search` at the series' confidence.
- **Cross-provider join:** a TV item matched on TVDB also carries its TMDB
  series id (resolved once via TVDB→TMDB `imdbid`/title bridge) so TMDB artwork
  can be fetched; that TMDB id is stored in `ProviderIds.tmdb`, not in the
  authoritative `match_provider` column.
- **Synthetic Series/Season items** resolve artwork through the existing
  representative-episode path, extended to prefer the series/season TMDB
  artwork (TVDB fallback) when present.

## Precedence (unchanged merge semantics)

`MetadataResolver` merges scalars first-`Some`-wins, `Vec` fields union+dedup
keeping priority order. Registration order: NFO/sidecar (highest) > `TvdbProvider`
> `TmdbProvider` > filename (lowest). So for TV, TVDB fields win over TMDB; for
movies, `TvdbProvider` returns empty and TMDB supplies everything; a local NFO
field always beats both.

## Artwork: download, cache, serve

### Download & cache

After a confident match, `metadata_backfill` collects artwork URLs by role
(poster / backdrop / logo for movie + series; poster per season; still per
episode), **preferring TMDB, falling back to TVDB per role**. Each chosen image
is fetched **once** via the owning client and written into the **existing image
cache** keyed by `(item_id, role)`, reusing the resize/webp/avif pipeline
untouched. Downloads are `BG_IO`-gated and capped per pass; any images dropped
past the cap are `log`-ged (no silent truncation).

### Serving

Today `local_artwork_path` serves only `source='local'`. Downloaded artwork is
registered in the `artwork` table via `set_artwork` (already the table's sole
writer + maintainer of `has_primary_art`) with a new `source` value — `tmdb` or
`tvdb` per origin. A downloaded poster is then a cached local file the existing
`/Items/{id}/Images/...` route serves like any sidecar; `ArtworkSource::Url`
stays in the metadata blob for provenance, but served bytes are always local
(offline + private).

### `has_primary_art` interplay

`set_artwork(role=Primary, source IN ('tmdb','tvdb'))` means bytes are genuinely
on disk, so it must flip `has_primary_art = true`. Adjust the just-shipped rule
from `source == 'local'` to `source IN ('local','tmdb','tvdb')` (all mean "bytes
on disk"). Coverless audio stays `false`; a TMDB/TVDB-postered movie/series
correctly advertises Primary.

## Manual Identify / override

- `GET /Items/{id}/RemoteSearch/{Movie|Series}` (Jellyfin-shape) → search
  candidates (TVDB for series, TMDB for movies) for a client / pharos-ui
  pick-list.
- `POST /Items/{id}/RemoteSearch/Apply` — body carries the chosen provider +
  id. Handler: set `match_provider`, `match_external_id`, `match_source=manual`,
  confidence 1.0, and enqueue an **immediate targeted backfill** for just that
  item (re-fetch metadata + artwork).

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
  inline providers inherit that — a TMDB/TVDB blip never fails a scan.
- The background sweep is fire-and-forget: a failure aborts only that sweep
  (logged `warn`), never the server, and the next trigger retries.
- All remote calls are best-effort; transport/decode/auth (expired JWT) errors
  → skip that item, leaving prior (filename/NFO) metadata intact. A TVDB 401
  triggers one re-login before giving up.

## Config

Extend config (secrets injected from k8s Secret env vars — see the GitOps
secret plumbing below):

| Key | Default | Meaning |
|-----|---------|---------|
| `[tmdb].api_key` | none | already exists; gates TMDB (movies + TV artwork + persons) |
| `[tvdb].api_key` | none | **new**; gates TVDB (TV structure/metadata) |
| `[tmdb].match_min_confidence` / `[tvdb].match_min_confidence` | 0.7 | search-match acceptance threshold |
| `[metadata].refresh_ttl_days` | 30 | staleness window for re-matching `search`/`none` rows |
| `[metadata].max_per_pass` | 5000 | bound on one sweep (mirrors T81 `MAX_PER_PASS`) |

Env vars: `PHAROS_TMDB_API_KEY` (exists), `PHAROS_TVDB_API_KEY` (new) — both
injected from a k8s Secret (`pharos-metadata-keys`, SOPS-encrypted in the
home-cluster GitOps repo).

## Testing

- `match_candidate` — pure unit tests: title/year scoring, threshold edges,
  `SxxExx` parsing, TV-vs-movie provider selection, TVDB→TMDB fallback,
  remakes/ambiguous titles.
- `TmdbProvider` / `TvdbProvider` — fake client (id → result mapping), asserts
  gap-fill only, never overrides a higher-priority provider; `TvdbProvider`
  returns empty for movie items.
- `TvdbClient` auth — fake transport: JWT cached, 401 triggers exactly one
  re-login.
- `metadata_backfill` — fake clients + in-memory store: idempotency (`manual`
  never clobbered), no re-download within TTL, self-termination, TMDB-preferred
  artwork with TVDB fallback per role.
- `has_primary_art` — a store test that `set_artwork(Primary, 'tmdb'|'tvdb')`
  flips the flag true; a wire/golden test that an online-postered item
  advertises Primary and serves cached bytes.
- Migrations — round-trip test on both Postgres and SQLite (remember the
  `--features postgres --all-targets` build so postgres-gated literals compile).

## GitOps secret plumbing (home-cluster repo)

- `clusters/home-cluster-1/flux-system/secrets/pharos-metadata-keys.enc.yaml` —
  a SOPS-encrypted `Secret` (ns `pharos`) with keys `tmdb-api-key` +
  `tvdb-api-key`. Encrypted via `op inject | sops` so plaintext is never
  exposed; uses the existing `.sops.yaml` home-cluster age recipient and Flux's
  `decryption: provider: sops` on the `flux-system` Kustomization.
- add it to `secrets/kustomization.yaml`.
- `helmreleases/pharos.yaml` `extraEnv`: `PHAROS_TMDB_API_KEY` +
  `PHAROS_TVDB_API_KEY` from `secretKeyRef: pharos-metadata-keys`.

## Out of scope (this spec)

- Music metadata/artwork (Phase 2: MusicBrainz + Cover Art Archive / fanart.tv).
- Image transforms beyond the existing resize/webp/avif pipeline.
- Bulk re-identify UI beyond the per-item Identify dialog.
- Collections/box-set enrichment (can follow once movie matching is proven).
