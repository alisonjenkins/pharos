# Edit-Image Picker (RemoteImages) — Design

**Date:** 2026-07-23
**Status:** Design (pending user review)

## Goal

Make jellyfin-web's **Edit Images** dialog functional: for a movie, episode,
or **series container** (e.g. "Dragon Ball", which today shows a bad
auto-extracted tile), the user can browse alternate **Primary (poster)**,
**Backdrop (fanart)**, and **Logo** images provided by TMDB / TVDB and pick
one. The chosen image replaces the current art and survives future background
enrichment passes.

## Problem

Three Jellyfin endpoints back the dialog; pharos currently stubs them:

- `GET /Items/{id}/RemoteImages` — returns a hardcoded empty
  `{Images:[],TotalRecordCount:0,Providers:[]}`
  (`item_ops.rs::remote_images`).
- `GET /Items/{id}/RemoteImages/Providers` — returns `[]`.
- `POST /Items/{id}/RemoteImages/Download` — **does not exist**.

So the dialog opens but lists nothing and cannot download.

Underlying gap: the `OnlineEnricher` trait can `search` / `fetch` (one
best poster + one backdrop) but cannot **enumerate all** candidate images for
an already-matched id — which is exactly what a picker needs.

## Architecture

Reuse the existing "one-off enricher built from keys on `AppState`" pattern
that `RemoteSearch{,/Apply}` already uses (`state.tmdb_api_key` /
`state.tvdb_api_key`). No new long-lived state.

Flow (list): resolve item → find its matched provider id → call a **new**
`list_images(kind, id, role)` on that provider → map to Jellyfin
`RemoteImageInfo` DTOs.

Flow (download): fetch the chosen `ImageUrl` bytes (provider-agnostic — any
enricher's `fetch_image_bytes`, or a plain HTTP GET) → cache → record as the
item's art → **freeze the row** so the background pass won't overwrite it.

Two item shapes, resolved the way `images.rs` already distinguishes them:

| Shape | Provider id source | Art storage | Freeze |
|-------|-------------------|-------------|--------|
| Real item (movie/episode) | `item.metadata.provider_ids` (tmdb/tvdb) | `set_artwork(id, role, provider, path)` → cache dir | `set_item_match(match_source="manual")` |
| Synth **Series** container | `series_metadata` row via `series.series_key()` (`match_provider` + `match_external_id`) | `upload_series_art` → `series_metadata.{poster,backdrop,logo}_locator` | `upsert_series_metadata(match_source="manual")` |

Both `items_needing_match` and `series_needing_match` already exclude
`match_source='manual'`, so freezing is a no-new-mechanism guarantee that a
curated pick is never re-clobbered.

**Tradeoff (accepted):** picking an image pins the item's *identity* to
`manual` as well (the background metadata pass skips it thereafter). This
matches the existing `RemoteSearch/Apply` semantics — a user who curates art
has effectively confirmed the match — and is the simplest correct way to
protect the single `(item, role)` artwork row (which `set_artwork` REPLACES
wholesale) from the next enrichment pass. Documented so it is a choice, not a
surprise.

## Endpoints

### `GET /Items/{id}/RemoteImages`

Query: `Type` (Primary|Backdrop|Logo, optional — absent = all three),
`ProviderName` (optional filter), `IncludeAllLanguages` (ignored; we always
return what the provider gives).

Response (`RemoteImageInfoDto`, PascalCase):
```
{
  "Images": [
    { "ProviderName": "TheMovieDb", "Url": "...", "Type": "Primary",
      "Height": 3000, "Width": 2000, "Language": "en",
      "CommunityRating": 5.4, "VoteCount": 12, "RatingType": "Score" }
  ],
  "TotalRecordCount": 1,
  "Providers": ["TheMovieDb"]
}
```
No key / no match / provider blip → empty, well-shaped, `200` (never `404`/
`500` on a public image route — V6 spirit, same as the current stub).

### `GET /Items/{id}/RemoteImages/Providers`

Returns the matched provider display name(s) as a JSON string array
(`["TheMovieDb"]` / `["TheTVDB"]`), or `[]` when unmatched / no key.

### `POST /Items/{id}/RemoteImages/Download`

Query: `Type` (required), `ImageUrl` (required), `ProviderName` (optional,
echoed for provenance). Fetches bytes → caches → records art → freezes row.
Returns `204 No Content` (Jellyfin's contract). `400` on missing
`Type`/`ImageUrl`; `404` on unknown item; failure to fetch bytes → `400`
with the underlying reason surfaced (never a bare 500).

## New provider capability

Add to `OnlineEnricher` (and both impls):

```rust
/// All candidate images the provider offers for an already-resolved `id`,
/// filtered to `role`. Empty Vec on any error (best-effort, never panics).
fn list_images(
    &self,
    kind: MediaKind,
    id: &str,
    role: ArtworkRole,
) -> impl Future<Output = Vec<RemoteImage>> + Send;
```

```rust
pub struct RemoteImage {
    pub role: ArtworkRole,
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub language: Option<String>,
    pub community_rating: Option<f32>,
    pub vote_count: Option<u32>,
}
```

- **TMDB**: `GET /movie/{id}/images` or `/tv/{id}/images` →
  `posters[]` (Primary), `backdrops[]` (Backdrop), `logos[]` (Logo). Each
  carries `file_path`, `width`, `height`, `iso_639_1`, `vote_average`,
  `vote_count`. URL = `https://image.tmdb.org/t/p/original{file_path}`.
- **TVDB**: series artworks (already present in the `/series/{id}/extended`
  record, or `/series/{id}/artworks`). Map TVDB artwork `type` →
  role: poster→Primary, background→Backdrop, clearlogo→Logo. TVDB carries
  `image` (full URL), `width`/`height`, `language`, `score`. Logo coverage
  is patchy — an empty logo list is honest and expected.

`fetch_image_bytes` already exists on every enricher for the download step.

## Storage

- **Series** needs a **Logo** slot it lacks today: migration **0045** adds
  `logo_locator TEXT` to `series_metadata` (both backends). Extend
  `SeriesMetadata`, `SeriesMetaRow`/`SERIES_META_COLUMNS`, `upsert_series_metadata`,
  and `series_metadata_art_path` (which today serves only Primary/Backdrop) to
  cover Logo. `ImageCache::upload_series_art` already takes a role — extend its
  role→subdir mapping to Logo.
- **Real items**: `set_artwork(id, role, provider, cached_path)`. The cache
  write reuses the existing download+cache helper the enrichment art path uses
  (`metadata_backfill` → `ImageCache`). `local_artwork_path` already serves
  Primary/Backdrop/Logo from tmdb/tvdb sources — no serving change needed.

## Files

- **Modify** `crates/pharos-server/src/api/jellyfin/item_ops.rs` — replace
  `remote_images` + `remote_image_providers` stubs, add `remote_images_download`,
  register the POST route, add DTOs, add the resolve-provider-id + synth-series
  helpers, tests.
- **Modify** `crates/pharos-server/src/online_enrich.rs` — `RemoteImage` struct
  + `list_images` trait method.
- **Modify** `crates/pharos-server/src/tmdb.rs` — `list_images` impl +
  `/…/images` client call + parser + tests.
- **Modify** `crates/pharos-server/src/tvdb.rs` — `list_images` impl + artworks
  parse + tests.
- **Modify** `crates/pharos-server/src/metadata_backfill.rs` — the fake
  enricher in tests gains `list_images` (default empty is fine).
- **Create** `crates/pharos-store-sqlx/migrations/{sqlite,postgres}/0045_series_logo_locator.sql`.
- **Modify** `crates/pharos-core/src/lib.rs` — `SeriesMetadata.logo_locator`.
- **Modify** `crates/pharos-store-sqlx/src/{series_meta_row.rs,sqlite.rs,postgres.rs}` —
  logo column in select/upsert.
- **Modify** `crates/pharos-server/src/api/jellyfin/images.rs` — `series_metadata_art_path`
  serves Logo; tests.
- **Modify** `crates/pharos-cache/src/image_cache.rs` — `upload_series_art` Logo subdir.
- **Modify** `crates/pharos-server/tests/jellyfin_feature_inventory.rs` — assert the
  Download endpoint + non-empty list shape under a fake provider if feasible.

## Edge cases / non-goals

- **Unmatched item** (no provider id): list returns empty `200`; download of a
  supplied URL still works (bytes are fetched regardless), but there is no
  provider id to *list* — that's fine.
- **Delete / reorder / upload-from-disk** image ops: **out of scope**. Only
  browse-and-pick-a-provided-image, which is the stated need.
- **Season** containers: same synth path as Series; a season tile already
  falls back to the series poster. Picking per-season art is out of scope —
  the picker targets the Series container id.
- **postgres INT4/INT8 discipline**: any new aggregate/column added to a
  cross-backend query gets a `backend_conformance` exercise (lesson from the
  series-metadata INT4 bug), not sqlite-only coverage.

## Testing

- Provider parsers: TMDB `/images` JSON → posters/backdrops/logos mapped to
  the right roles with dims/language/rating; TVDB artworks → roles. Malformed
  body → empty Vec (no panic).
- Endpoint handlers (via the `_inner` split, like `remote_search_*`): list
  under a fake enricher returns mapped DTOs; download records art + freezes
  the row (assert `match_source == "manual"` and the art row/locator is set);
  missing key → empty list; unknown id → 404; missing `ImageUrl` → 400.
- Series path: synth Series id resolves to the series_key, download writes
  `series_metadata.poster_locator` and freezes the series row; Logo writes
  `logo_locator` and `series_metadata_art_path` serves it.
- `backend_conformance`: `logo_locator` round-trips on both engines.
- Full `just test` + workspace clippy + both backend builds before commit.
