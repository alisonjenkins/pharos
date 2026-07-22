# Online Metadata & Artwork Enrichment (TMDB + TVDB) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enrich Movies + TV with online metadata and artwork — TVDB authoritative for TV series/season/episode structure, TMDB for all Movies plus TV artwork and gap-fill — without overriding curated local data, slowing scans, or contending with live playback.

**Architecture:** All online work runs in a **background enrichment task** (`metadata_backfill`, pharos-server) gated by the shared `bg_io` semaphore — mirroring the existing T81 person-image backfill. Two API clients (`TmdbClient` extended, `TvdbClient` new) sit behind a provider-agnostic `OnlineEnricher` trait. A pure `match_best` function (pharos-core) resolves a parsed title+year against provider search results. Matched metadata is folded onto the stored `MediaItem` with **fill-if-absent** semantics (local always wins) and persisted through the same store methods the scanner uses (`put` + `link_item_*` + `set_artwork`). Downloaded artwork is written into the existing image cache and served through the existing `/Items/{id}/Images` route.

> **Deviation from the approved spec (Architecture §):** the spec placed exact-id lookups *inline in the scan resolver*. During planning this proved unworkable — a `MetadataProvider` in the resolver chain receives only a path and cannot see the NFO-resolved tmdb/tvdb id (providers don't observe each other's merge output; the id only exists after the item is stored). Exact-id lookups therefore run in the **same background pass** as search, prioritised first and cheap (one GET, no search). This is a strict improvement: scans stay 100% network-free. External behavior, data model, and the hybrid cheap-vs-expensive split are unchanged. Update the spec's Architecture section to match when convenient.

**Tech Stack:** Rust (workspace), actix-web, sqlx (sqlite + postgres), tokio, reqwest, serde_json, tracing. Runs inside the Nix devShell.

## Global Constraints

- **Always** run cargo/clippy/nextest via `nix develop --command <cmd>`; never the host toolchain.
- `CARGO_BUILD_JOBS=4` for builds in this environment.
- Clippy is `-D warnings` with `clippy::unwrap_used` / `expect_used` **denied** in non-test code — no `.unwrap()`/`.expect()` outside `#[cfg(test)]`. Use `?`, `let else`, `ok()?`, `map_err`.
- Tests run with `cargo nextest run --workspace`; doctests separately with `cargo test --doc --workspace`.
- **Any schema/column change ships a migration in BOTH `migrations/sqlite/` and `migrations/postgres/` with the same numeric prefix.** After adding a `MediaItem` field, also compile with `--features postgres --all-targets` (a postgres-gated test literal won't be built by the default sqlite target and will fail CI otherwise — see `migrate_roundtrip.rs`).
- Atomic commits: one logical change per commit; reverting a commit alone must leave the project compiling. Never squash.
- Commit message trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
- Errors propagate their underlying cause — never collapse to a bare class string.
- Run `just hakari-regen` only if a crate's `Cargo.toml` dependencies change (adds/removes a dep). No dep changes are expected in this plan (reqwest/serde_json already present in pharos-server; pharos-core adds none).
- Times as ISO-8601 UTC.

---

## File Structure

**pharos-core** (`crates/pharos-core/src/lib.rs`):
- `MediaItem` gains 5 match-state fields (`match_provider`, `match_external_id`, `match_source`, `match_confidence`, `metadata_refreshed_at`).
- New pure matching module content: `SearchCandidate` struct + `match_best(...)` fn + `title_similarity(...)` helper.
- `MediaStore` trait gains `items_needing_match`, `set_item_match`, `item_entity_counts` methods.

**pharos-store-sqlx**:
- `migrations/{sqlite,postgres}/0043_metadata_match.sql` (new, both).
- `src/sqlite.rs` + `src/postgres.rs`: column list, `MediaRow` fields + mapping, `put` upsert (leave match columns to `set_item_match`), `set_artwork` Primary-branch predicate widened, new store methods.

**pharos-server**:
- `src/config.rs`: `TvdbConfig`, `MetadataConfig`, `PHAROS_TVDB_API_KEY` env override.
- `src/tmdb.rs`: extend with movie/tv search + detail + image-bytes fetch + parse fns; `MetadataProvider`-agnostic `EnrichedMetadata`/`RemoteArt` types live in new `src/online_enrich.rs`.
- `src/tvdb.rs` (new): `TvdbClient` (v4 login→JWT, cached, 401 re-login) + search/detail/parse fns.
- `src/online_enrich.rs` (new): `OnlineEnricher` trait, `EnrichedMetadata`, `RemoteArt`, `TmdbEnricher`/`TvdbEnricher` impls, `apply_enrichment` merge.
- `src/metadata_backfill.rs` (new): `spawn`/`run` orchestrator + artwork download/cache.
- `src/api/jellyfin/item_ops.rs`: `GET /items/{id}/remotesearch/{Movie|Series}` + `POST /items/{id}/remotesearch/apply` routes.
- `src/api/jellyfin/images.rs`: `local_artwork_path` source predicate widened.
- `src/main.rs`: spawn `metadata_backfill` gated on keys; thread clients.

---

## Task 1: Match-state columns on `media_items`

**Files:**
- Create: `crates/pharos-store-sqlx/migrations/sqlite/0043_metadata_match.sql`
- Create: `crates/pharos-store-sqlx/migrations/postgres/0043_metadata_match.sql`
- Modify: `crates/pharos-core/src/lib.rs` (`MediaItem` struct, ~line 32-70)
- Modify: `crates/pharos-store-sqlx/src/sqlite.rs` (column list ~:23, `MediaRow` ~:2781, mapping ~:2866)
- Modify: `crates/pharos-store-sqlx/src/postgres.rs` (column list ~:39, `MediaRow` ~:2949, mapping ~:3030)
- Test: `crates/pharos-store-sqlx/tests/sqlite_store.rs`

**Interfaces:**
- Produces: `MediaItem { match_provider: Option<String>, match_external_id: Option<String>, match_source: Option<String>, match_confidence: Option<f32>, metadata_refreshed_at: Option<i64> }` (all `Default = None`). `match_source` values: `"nfo_id" | "search" | "manual" | "none"`.

- [ ] **Step 1: Write the migrations**

`crates/pharos-store-sqlx/migrations/sqlite/0043_metadata_match.sql`:
```sql
-- 0043 — online-metadata match state (provider-agnostic). NULL match_source
-- = never matched = eligible for the background enricher. `manual`/`nfo_id`
-- are skipped by the enricher so a user override / local id is never clobbered.
ALTER TABLE media_items ADD COLUMN match_provider TEXT;
ALTER TABLE media_items ADD COLUMN match_external_id TEXT;
ALTER TABLE media_items ADD COLUMN match_source TEXT;
ALTER TABLE media_items ADD COLUMN match_confidence REAL;
ALTER TABLE media_items ADD COLUMN metadata_refreshed_at INTEGER;
```

`crates/pharos-store-sqlx/migrations/postgres/0043_metadata_match.sql`:
```sql
-- 0043 — online-metadata match state (provider-agnostic). See sqlite/0043.
ALTER TABLE media_items ADD COLUMN match_provider TEXT;
ALTER TABLE media_items ADD COLUMN match_external_id TEXT;
ALTER TABLE media_items ADD COLUMN match_source TEXT;
ALTER TABLE media_items ADD COLUMN match_confidence REAL;
ALTER TABLE media_items ADD COLUMN metadata_refreshed_at BIGINT;
```

- [ ] **Step 2: Add the fields to `MediaItem`**

In `crates/pharos-core/src/lib.rs`, after `pub has_primary_art: bool,` (line ~69), before the closing `}`:
```rust
    /// Online-enrichment match state. Which provider (`"tmdb"`/`"tvdb"`) and
    /// id authoritatively identified this item, and how (`match_source`:
    /// `"nfo_id"`/`"search"`/`"manual"`/`"none"`). `None` = never matched
    /// (eligible for the background enricher). `manual`/`nfo_id` are never
    /// re-matched, so a user override / local id survives rescans.
    pub match_provider: Option<String>,
    pub match_external_id: Option<String>,
    pub match_source: Option<String>,
    /// 0..1 search-match score; 1.0 for `nfo_id`/`manual`. `None` when unmatched.
    pub match_confidence: Option<f32>,
    /// Unix-seconds of the last successful enrichment write; drives the TTL
    /// that stops already-matched items being re-fetched every pass.
    pub metadata_refreshed_at: Option<i64>,
```

- [ ] **Step 3: Build fails — fix every explicit `MediaItem { .. }` literal**

Run: `nix develop --command bash -lc 'CARGO_BUILD_JOBS=4 cargo build -p pharos-core --all-targets'`
Expected: FAIL — `missing fields match_provider, ... in initializer of MediaItem`.

Add `match_provider: None, match_external_id: None, match_source: None, match_confidence: None, metadata_refreshed_at: None,` to each failing literal. The scanner's constructor in `crates/pharos-scanner/src/fs.rs:1032` is one (put it after `has_primary_art: false,`). Then repeat the build across the workspace and both feature sets:
```
nix develop --command bash -lc 'CARGO_BUILD_JOBS=4 cargo build --workspace --all-targets'
nix develop --command bash -lc 'CARGO_BUILD_JOBS=4 cargo build -p pharos-store-sqlx --features postgres --all-targets'
```
Fix each `missing fields` error the same way until both are clean. (Delegate this mechanical sweep to a subagent if many files are touched; the change is identical everywhere.)

- [ ] **Step 4: Thread the columns through the sqlite store**

In `crates/pharos-store-sqlx/src/sqlite.rs`, append the 5 columns to the `SELECT` column-list constant (~:23, after `has_primary_art`):
```
, match_provider, match_external_id, match_source, match_confidence, metadata_refreshed_at
```
Add to the `MediaRow` struct (~:2781, after `has_primary_art: bool,`):
```rust
    match_provider: Option<String>,
    match_external_id: Option<String>,
    match_source: Option<String>,
    match_confidence: Option<f32>,
    metadata_refreshed_at: Option<i64>,
```
Add to `into_domain` mapping (~:2866, after `has_primary_art: self.has_primary_art,`):
```rust
    match_provider: self.match_provider,
    match_external_id: self.match_external_id,
    match_source: self.match_source,
    match_confidence: self.match_confidence,
    metadata_refreshed_at: self.metadata_refreshed_at,
```
Leave `put`'s INSERT/upsert untouched — like `has_primary_art`, these columns default NULL on insert, are preserved by `ON CONFLICT`, and are written only by `set_item_match` (Task 2). Confirm `put`'s column list does NOT include them.

- [ ] **Step 5: Thread the columns through the postgres store**

Mirror Step 4 in `crates/pharos-store-sqlx/src/postgres.rs` (column list ~:39, `MediaRow` ~:2949, mapping ~:3030). Postgres `REAL` maps to `f32` and `BIGINT` to `i64` — same Rust types as sqlite here.

- [ ] **Step 6: Write the round-trip test**

Add to `crates/pharos-store-sqlx/tests/sqlite_store.rs`:
```rust
#[tokio::test]
async fn match_columns_default_null_and_roundtrip() {
    let store = new_test_store().await; // existing helper in this file
    let mut item = sample_movie("Blade Runner"); // existing helper
    item.id = 900001;
    store.put(item.clone()).await.unwrap();
    let got = store.get(900001).await.unwrap();
    assert_eq!(got.match_provider, None);
    assert_eq!(got.match_source, None);
    assert_eq!(got.match_confidence, None);
    assert_eq!(got.metadata_refreshed_at, None);
}
```
(If `new_test_store` / `sample_movie` have different names in this file, use the existing equivalents — grep the top of the file for the setup helpers.)

- [ ] **Step 7: Run tests**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-store-sqlx match_columns_default_null_and_roundtrip'`
Expected: PASS. Also run the existing `migrate_roundtrip` test if present: `cargo nextest run -p pharos-store-sqlx migrate`.

- [ ] **Step 8: Commit**
```bash
git add crates/pharos-store-sqlx/migrations crates/pharos-core/src/lib.rs crates/pharos-store-sqlx/src crates/pharos-scanner/src/fs.rs crates/pharos-store-sqlx/tests/sqlite_store.rs
git commit -m "feat(store): add provider-agnostic online-match columns to media_items

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Store methods `set_item_match`, `items_needing_match`, `item_entity_counts`

**Files:**
- Modify: `crates/pharos-core/src/lib.rs` (`MediaStore` trait, ~:2170-2342)
- Modify: `crates/pharos-store-sqlx/src/sqlite.rs` (impl block)
- Modify: `crates/pharos-store-sqlx/src/postgres.rs` (impl block)
- Test: `crates/pharos-store-sqlx/tests/sqlite_store.rs`

**Interfaces:**
- Produces (on `MediaStore`):
  - `set_item_match(id: MediaId, provider: &str, external_id: &str, source: &str, confidence: Option<f32>, refreshed_at: i64) -> DomainResult<()>`
  - `items_needing_match(limit: i64, ttl_cutoff: i64) -> DomainResult<Vec<MediaItem>>` — rows where `match_source IS NULL OR match_source IN ('search','none')`, AND `(metadata_refreshed_at IS NULL OR metadata_refreshed_at < ttl_cutoff)`, `kind IN ('movie','episode')`, ordered by id, capped at `limit`. **Excludes** `manual`/`nfo_id`.
  - `item_entity_counts(id: MediaId) -> DomainResult<EntityCounts>` where `EntityCounts { genres: u32, people: u32, studios: u32 }` — lets the enricher know which joins are already populated locally (fill-if-empty).

- [ ] **Step 1: Write failing tests**

Add to `crates/pharos-store-sqlx/tests/sqlite_store.rs`:
```rust
#[tokio::test]
async fn set_item_match_persists_and_excludes_from_needing() {
    let store = new_test_store().await;
    let mut item = sample_movie("Dune"); item.id = 900010;
    store.put(item.clone()).await.unwrap();

    // Before matching, it is eligible.
    let need = store.items_needing_match(10, i64::MAX).await.unwrap();
    assert!(need.iter().any(|i| i.id == 900010));

    store.set_item_match(900010, "tmdb", "438631", "search", Some(0.92), 1_700_000_000).await.unwrap();
    let got = store.get(900010).await.unwrap();
    assert_eq!(got.match_provider.as_deref(), Some("tmdb"));
    assert_eq!(got.match_external_id.as_deref(), Some("438631"));
    assert_eq!(got.match_source.as_deref(), Some("search"));
    assert_eq!(got.metadata_refreshed_at, Some(1_700_000_000));

    // A fresh (recent) search-match is excluded by a cutoff below its timestamp.
    let need2 = store.items_needing_match(10, 1_699_999_999).await.unwrap();
    assert!(!need2.iter().any(|i| i.id == 900010));
    // But a manual match is excluded regardless of ttl.
    store.set_item_match(900010, "tmdb", "1", "manual", None, 1).await.unwrap();
    let need3 = store.items_needing_match(10, i64::MAX).await.unwrap();
    assert!(!need3.iter().any(|i| i.id == 900010));
}
```

- [ ] **Step 2: Run — verify it fails to compile (methods absent)**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-store-sqlx set_item_match_persists'`
Expected: FAIL — `no method named set_item_match`.

- [ ] **Step 3: Declare the trait methods + `EntityCounts`**

In `crates/pharos-core/src/lib.rs`, add near `MediaStore` (after `artwork_for`, ~:2341):
```rust
    /// Write the 5 online-match columns for `id` in one UPDATE. `confidence`
    /// is `None` for `nfo_id`/`manual`. No-op (zero rows) if the id is absent.
    fn set_item_match(
        &self, item_id: MediaId, provider: &str, external_id: &str,
        source: &str, confidence: Option<f32>, refreshed_at: i64,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// Items eligible for online enrichment: `match_source` NULL or in
    /// (`search`,`none`), not refreshed since `ttl_cutoff`, kind movie/episode,
    /// ascending id, capped at `limit`. Excludes `manual`/`nfo_id`.
    fn items_needing_match(
        &self, limit: i64, ttl_cutoff: i64,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaItem>>> + Send;

    /// Count of linked genres/people/studios for `id` (fill-if-empty gate).
    fn item_entity_counts(
        &self, item_id: MediaId,
    ) -> impl std::future::Future<Output = DomainResult<EntityCounts>> + Send;
```
And a top-level type near the other store DTOs:
```rust
/// Linked-entity population counts for one item (online-enrich fill-if-empty gate).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntityCounts { pub genres: u32, pub people: u32, pub studios: u32 }
```

- [ ] **Step 4: Implement in the sqlite store**

In `crates/pharos-store-sqlx/src/sqlite.rs` impl block (use the file's existing `MEDIA_COLUMNS`/select constant + `map_err` helpers; `?`-propagate, no unwrap):
```rust
async fn set_item_match(
    &self, item_id: MediaId, provider: &str, external_id: &str,
    source: &str, confidence: Option<f32>, refreshed_at: i64,
) -> DomainResult<()> {
    let id_i64 = i64::try_from(item_id).map_err(|e| domain_err(e))?;
    sqlx::query(
        "UPDATE media_items SET match_provider = ?, match_external_id = ?, \
         match_source = ?, match_confidence = ?, metadata_refreshed_at = ? WHERE id = ?",
    )
    .bind(provider).bind(external_id).bind(source).bind(confidence).bind(refreshed_at).bind(id_i64)
    .execute(&self.pool).await.map_err(|e| domain_err(e))?;
    Ok(())
}

async fn items_needing_match(&self, limit: i64, ttl_cutoff: i64) -> DomainResult<Vec<MediaItem>> {
    let rows = sqlx::query_as::<_, MediaRow>(
        &format!(
            "SELECT {COLS} FROM media_items \
             WHERE (match_source IS NULL OR match_source IN ('search','none')) \
               AND (metadata_refreshed_at IS NULL OR metadata_refreshed_at < ?) \
               AND kind IN ('movie','episode') \
             ORDER BY id ASC LIMIT ?",
            COLS = MEDIA_COLUMNS,
        ),
    )
    .bind(ttl_cutoff).bind(limit)
    .fetch_all(&self.pool).await.map_err(|e| domain_err(e))?;
    rows.into_iter().map(MediaRow::into_domain).collect()
}

async fn item_entity_counts(&self, item_id: MediaId) -> DomainResult<EntityCounts> {
    let id_i64 = i64::try_from(item_id).map_err(|e| domain_err(e))?;
    let (g,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM item_genres WHERE item_id = ?")
        .bind(id_i64).fetch_one(&self.pool).await.map_err(|e| domain_err(e))?;
    let (p,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM item_people WHERE item_id = ?")
        .bind(id_i64).fetch_one(&self.pool).await.map_err(|e| domain_err(e))?;
    let (s,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM item_studios WHERE item_id = ?")
        .bind(id_i64).fetch_one(&self.pool).await.map_err(|e| domain_err(e))?;
    Ok(EntityCounts { genres: g as u32, people: p as u32, studios: s as u32 })
}
```
Use the exact `MEDIA_COLUMNS` constant name and `map_err` closure this file already uses (grep for `into_domain` and an existing method to copy the error-mapping idiom). Verify the join table names (`item_genres`/`item_people`/`item_studios`) against the file's other queries; correct them if they differ.

- [ ] **Step 5: Implement in the postgres store**

Mirror Step 4 in `crates/pharos-store-sqlx/src/postgres.rs` with `$1..$n` placeholders and `fetch_one::<(i64,)>` counts. `EntityCounts` fields cast `i64 as u32` identically.

- [ ] **Step 6: Run tests**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-store-sqlx set_item_match_persists_and_excludes_from_needing'`
Expected: PASS.
Then confirm postgres still compiles: `nix develop --command bash -lc 'CARGO_BUILD_JOBS=4 cargo build -p pharos-store-sqlx --features postgres --all-targets'`.

- [ ] **Step 7: Commit**
```bash
git add crates/pharos-core/src/lib.rs crates/pharos-store-sqlx/src crates/pharos-store-sqlx/tests/sqlite_store.rs
git commit -m "feat(store): set_item_match + items_needing_match + item_entity_counts

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Config — `[tvdb]` + `[metadata]` + `PHAROS_TVDB_API_KEY`

**Files:**
- Modify: `crates/pharos-server/src/config.rs`
- Test: `crates/pharos-server/src/config.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `Config.tvdb: TvdbConfig { api_key: Option<String> }`, `Config.metadata: MetadataConfig { refresh_ttl_days: u32 (default 30), max_per_pass: i64 (default 5000), match_min_confidence: f32 (default 0.7) }`. Env `PHAROS_TVDB_API_KEY` → `tvdb.api_key`.

- [ ] **Step 1: Write failing test**

Add to the `#[cfg(test)]` module in `config.rs`:
```rust
#[test]
fn metadata_defaults_and_tvdb_env_override() {
    let mut cfg = Config::minimal_for_test(); // use whatever the file's test-ctor / TOML-parse helper is
    assert_eq!(cfg.metadata.refresh_ttl_days, 30);
    assert_eq!(cfg.metadata.max_per_pass, 5000);
    assert!((cfg.metadata.match_min_confidence - 0.7).abs() < f32::EPSILON);
    std::env::set_var("PHAROS_TVDB_API_KEY", "  abc123  ");
    cfg.apply_env();
    assert_eq!(cfg.tvdb.api_key.as_deref(), Some("abc123"));
    std::env::remove_var("PHAROS_TVDB_API_KEY");
}
```
(If the file builds `Config` from a TOML string in tests instead of a ctor, parse a minimal TOML omitting `[tvdb]`/`[metadata]` so the `#[serde(default)]` paths are exercised.)

- [ ] **Step 2: Run — fails**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-server metadata_defaults_and_tvdb_env_override'`
Expected: FAIL — `no field metadata on Config`.

- [ ] **Step 3: Add the config structs**

In `config.rs`, mirror `TmdbConfig` (~:26):
```rust
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct TvdbConfig {
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MetadataConfig {
    #[serde(default = "default_refresh_ttl_days")]
    pub refresh_ttl_days: u32,
    #[serde(default = "default_max_per_pass")]
    pub max_per_pass: i64,
    #[serde(default = "default_match_min_confidence")]
    pub match_min_confidence: f32,
}
fn default_refresh_ttl_days() -> u32 { 30 }
fn default_max_per_pass() -> i64 { 5000 }
fn default_match_min_confidence() -> f32 { 0.7 }
impl Default for MetadataConfig {
    fn default() -> Self {
        Self { refresh_ttl_days: default_refresh_ttl_days(),
                max_per_pass: default_max_per_pass(),
                match_min_confidence: default_match_min_confidence() }
    }
}
```
Add to `Config` (after `pub tmdb: TmdbConfig,` ~:16):
```rust
    #[serde(default)] pub tvdb: TvdbConfig,
    #[serde(default)] pub metadata: MetadataConfig,
```

- [ ] **Step 4: Add the env override**

In `Config::apply_env` (~:467), right after the TMDB block, mirror the trim + non-empty pattern:
```rust
    if let Ok(v) = std::env::var("PHAROS_TVDB_API_KEY") {
        let v = v.trim();
        if !v.is_empty() {
            self.tvdb.api_key = Some(v.to_string());
        }
    }
```
Add `PHAROS_TVDB_API_KEY` to the recognised-vars doc comment (~:445).

- [ ] **Step 5: Run test**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-server metadata_defaults_and_tvdb_env_override'`
Expected: PASS.

- [ ] **Step 6: Commit**
```bash
git add crates/pharos-server/src/config.rs
git commit -m "feat(config): [tvdb] api_key + [metadata] enrichment settings + PHAROS_TVDB_API_KEY

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Pure matching — `match_best` (pharos-core)

**Files:**
- Modify: `crates/pharos-core/src/lib.rs` (new pure fns + types + `#[cfg(test)]`)

**Interfaces:**
- Produces:
  - `pub struct SearchCandidate { pub id: String, pub title: String, pub year: Option<u32> }`
  - `pub struct MatchOutcome { pub id: String, pub confidence: f32 }`
  - `pub fn match_best(query_title: &str, query_year: Option<u32>, candidates: &[SearchCandidate], min_confidence: f32) -> Option<MatchOutcome>` — best score `= title_similarity × year_factor`; returns the best candidate iff its score ≥ `min_confidence`.
  - `pub fn title_similarity(a: &str, b: &str) -> f32` — normalized (0..1), case/whitespace/punctuation-insensitive.

- [ ] **Step 1: Write failing tests**
```rust
#[test]
fn match_best_prefers_exact_title_and_year() {
    let cands = vec![
        SearchCandidate { id: "1".into(), title: "The Thing".into(), year: Some(2011) },
        SearchCandidate { id: "2".into(), title: "The Thing".into(), year: Some(1982) },
    ];
    let m = match_best("The Thing", Some(1982), &cands, 0.7).unwrap();
    assert_eq!(m.id, "2");
    assert!(m.confidence > 0.9);
}
#[test]
fn match_best_rejects_below_threshold() {
    let cands = vec![SearchCandidate { id: "9".into(), title: "Completely Different".into(), year: None }];
    assert!(match_best("The Thing", Some(1982), &cands, 0.7).is_none());
}
#[test]
fn match_best_year_off_by_one_is_partial_not_zero() {
    let cands = vec![SearchCandidate { id: "3".into(), title: "Blade Runner".into(), year: Some(1983) }];
    // title exact, year ±1 -> still above threshold
    assert!(match_best("Blade Runner", Some(1982), &cands, 0.7).is_some());
}
#[test]
fn title_similarity_ignores_case_and_punctuation() {
    assert!(title_similarity("WALL·E", "wall e") > 0.85);
    assert!(title_similarity("Se7en", "Seven") < 0.9); // not falsely perfect
}
```

- [ ] **Step 2: Run — fails**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-core match_best'`
Expected: FAIL — `cannot find function match_best`.

- [ ] **Step 3: Implement**

Add to `lib.rs` (pure, no deps beyond std):
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCandidate { pub id: String, pub title: String, pub year: Option<u32> }
#[derive(Debug, Clone, PartialEq)]
pub struct MatchOutcome { pub id: String, pub confidence: f32 }

/// Lowercase, keep only alphanumerics + single spaces (collapse the rest).
fn normalize_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() { out.push(l); }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim().to_string()
}

/// Normalized Levenshtein similarity in 0..1 over normalized titles.
pub fn title_similarity(a: &str, b: &str) -> f32 {
    let (a, b) = (normalize_title(a), normalize_title(b));
    if a.is_empty() && b.is_empty() { return 1.0; }
    let (av, bv): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (n, m) = (av.len(), bv.len());
    if n == 0 || m == 0 { return 0.0; }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if av[i - 1] == bv[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let dist = prev[m] as f32;
    let maxlen = n.max(m) as f32;
    1.0 - dist / maxlen
}

/// Year agreement multiplier: exact = 1.0, ±1 = 0.9, else 0.6; unknown = 0.85.
fn year_factor(q: Option<u32>, c: Option<u32>) -> f32 {
    match (q, c) {
        (Some(q), Some(c)) => {
            let d = q.abs_diff(c);
            if d == 0 { 1.0 } else if d == 1 { 0.9 } else { 0.6 }
        }
        _ => 0.85,
    }
}

/// Best-scoring candidate over `min_confidence`, else None. Score = title
/// similarity × year factor. Ties resolve to the earliest candidate.
pub fn match_best(
    query_title: &str, query_year: Option<u32>,
    candidates: &[SearchCandidate], min_confidence: f32,
) -> Option<MatchOutcome> {
    let mut best: Option<MatchOutcome> = None;
    for c in candidates {
        let score = title_similarity(query_title, &c.title) * year_factor(query_year, c.year);
        if best.as_ref().map(|b| score > b.confidence).unwrap_or(true) {
            best = Some(MatchOutcome { id: c.id.clone(), confidence: score });
        }
    }
    best.filter(|b| b.confidence >= min_confidence)
}
```

- [ ] **Step 4: Run tests**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-core match_best title_similarity'`
Expected: PASS. If `title_similarity_ignores_case_and_punctuation`'s `WALL·E` case falls below 0.85, that is acceptable — adjust the assertion threshold to the actual value rather than weakening the algorithm, but keep the `Se7en`/`Seven` inequality.

- [ ] **Step 5: Commit**
```bash
git add crates/pharos-core/src/lib.rs
git commit -m "feat(core): pure match_best + title_similarity for online provider matching

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `online_enrich` types + TMDB enricher

**Files:**
- Create: `crates/pharos-server/src/online_enrich.rs`
- Modify: `crates/pharos-server/src/tmdb.rs` (add search/detail/parse fns + image fetch)
- Modify: `crates/pharos-server/src/lib.rs` (`pub mod online_enrich;`)
- Test: inline `#[cfg(test)]` in both files

**Interfaces:**
- Produces (`online_enrich.rs`):
```rust
pub struct RemoteArt { pub role: pharos_core::ArtworkRole, pub url: String }
#[derive(Default)]
pub struct EnrichedMetadata {
    pub title: Option<String>, pub overview: Option<String>, pub tagline: Option<String>,
    pub production_year: Option<u32>, pub premiere_date: Option<i64>,
    pub community_rating: Option<f32>, pub official_rating: Option<String>,
    pub genres: Vec<String>, pub people: Vec<pharos_core::PersonRef>,
    pub provider_id: Option<String>,          // the matched id on THIS provider
    pub also_tmdb_id: Option<String>,         // cross-provider bridge (tvdb->tmdb) for artwork
    pub artwork: Vec<RemoteArt>,
}
pub trait OnlineEnricher: Send + Sync {
    fn provider(&self) -> &'static str;                 // "tmdb" | "tvdb"
    fn supports(&self, kind: pharos_core::MediaKind) -> bool;
    fn search(&self, kind: MediaKind, title: &str, year: Option<u32>)
        -> impl Future<Output = Vec<pharos_core::SearchCandidate>> + Send;
    fn fetch(&self, kind: MediaKind, id: &str, season: Option<u32>, episode: Option<u32>)
        -> impl Future<Output = Option<EnrichedMetadata>> + Send;
    fn fetch_image_bytes(&self, url: &str)
        -> impl Future<Output = Option<Vec<u8>>> + Send;
}
```
- `TmdbEnricher(TmdbClient)` impl: `supports` = all (movies + TV artwork/gap-fill); `search` hits `/search/movie` or `/search/tv`; `fetch` hits `/movie/{id}` or `/tv/{id}` (+ `/season/{s}/episode/{e}` when both given); artwork URLs use `https://image.tmdb.org/t/p/original` + `poster_path`/`backdrop_path`.

- [ ] **Step 1: Write parse-fn tests in `tmdb.rs`**

TMDB parsing is pure over `serde_json::Value` (like the existing `parse_profile_path`). Add fixtures + tests:
```rust
#[test]
fn tmdb_parse_search_results_yields_candidates() {
    let body = r#"{"results":[
        {"id":438631,"title":"Dune","release_date":"2021-10-01"},
        {"id":438632,"title":"Dune Part Two","release_date":"2024-03-01"}]}"#;
    let c = super::parse_movie_search(body);
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].id, "438631");
    assert_eq!(c[0].year, Some(2021));
}
#[test]
fn tmdb_parse_movie_detail_extracts_overview_genres_art() {
    let body = r#"{"id":438631,"overview":"A duke's son...","release_date":"2021-10-01",
        "vote_average":7.8,"genres":[{"name":"Science Fiction"},{"name":"Adventure"}],
        "poster_path":"/p.jpg","backdrop_path":"/b.jpg"}"#;
    let e = super::parse_movie_detail(body).unwrap();
    assert_eq!(e.overview.as_deref(), Some("A duke's son..."));
    assert_eq!(e.genres, vec!["Science Fiction","Adventure"]);
    assert_eq!(e.community_rating, Some(7.8));
    assert!(e.artwork.iter().any(|a| a.role == pharos_core::ArtworkRole::Primary
        && a.url.ends_with("/p.jpg")));
    assert!(e.artwork.iter().any(|a| a.role == pharos_core::ArtworkRole::Backdrop));
}
```

- [ ] **Step 2: Run — fails**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-server tmdb_parse'`
Expected: FAIL — `cannot find function parse_movie_search`.

- [ ] **Step 3: Implement the enricher types + TMDB parse/fetch**

Create `online_enrich.rs` with the types/trait above (add `use std::future::Future; use pharos_core::MediaKind;`). In `tmdb.rs` add the pure parse fns + client methods. Sketch (fill the TV-detail + episode parse mirroring the movie parse; use `original` image base for downloads):
```rust
const IMAGE_BASE_ORIGINAL: &str = "https://image.tmdb.org/t/p/original";

pub(crate) fn parse_movie_search(body: &str) -> Vec<pharos_core::SearchCandidate> {
    let v: serde_json::Value = match serde_json::from_str(body) { Ok(v) => v, Err(_) => return vec![] };
    v.get("results").and_then(|r| r.as_array()).map(|arr| arr.iter().filter_map(|r| {
        let id = r.get("id")?.as_i64()?.to_string();
        let title = r.get("title").or_else(|| r.get("name"))?.as_str()?.to_string();
        let year = r.get("release_date").or_else(|| r.get("first_air_date"))
            .and_then(|d| d.as_str()).and_then(year_of);
        Some(pharos_core::SearchCandidate { id, title, year })
    }).collect()).unwrap_or_default()
}
fn year_of(date: &str) -> Option<u32> { date.get(0..4)?.parse().ok() }

pub(crate) fn parse_movie_detail(body: &str) -> Option<crate::online_enrich::EnrichedMetadata> {
    use crate::online_enrich::{EnrichedMetadata, RemoteArt};
    use pharos_core::ArtworkRole;
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut art = vec![];
    if let Some(p) = v.get("poster_path").and_then(|x| x.as_str()) {
        art.push(RemoteArt { role: ArtworkRole::Primary, url: format!("{IMAGE_BASE_ORIGINAL}{p}") });
    }
    if let Some(b) = v.get("backdrop_path").and_then(|x| x.as_str()) {
        art.push(RemoteArt { role: ArtworkRole::Backdrop, url: format!("{IMAGE_BASE_ORIGINAL}{b}") });
    }
    Some(EnrichedMetadata {
        overview: v.get("overview").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string),
        production_year: v.get("release_date").and_then(|x| x.as_str()).and_then(year_of),
        community_rating: v.get("vote_average").and_then(|x| x.as_f64()).map(|f| f as f32),
        genres: v.get("genres").and_then(|g| g.as_array()).map(|a| a.iter()
            .filter_map(|g| g.get("name")?.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        provider_id: v.get("id").and_then(|x| x.as_i64()).map(|i| i.to_string()),
        artwork: art,
        ..EnrichedMetadata::default()
    })
}
```
Then add `TmdbClient` async methods (`get`+`text` like the existing `search_person_image`, api_key query param) that fetch the body and hand it to these parse fns, plus:
```rust
pub async fn fetch_image_bytes(&self, url: &str) -> Option<Vec<u8>> {
    let resp = self.http.get(url).send().await.ok()?;
    if !resp.status().is_success() { return None; }
    resp.bytes().await.ok().map(|b| b.to_vec())
}
```
Finally implement `OnlineEnricher for TmdbEnricher` (a thin newtype wrapping `TmdbClient`), routing `kind`/`season`/`episode` to the right endpoint + parse fn. Add `pub mod online_enrich;` to `lib.rs`.

- [ ] **Step 4: Run tests**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-server tmdb_parse'`
Expected: PASS. Then clippy the crate: `nix develop --command bash -lc 'cargo clippy -p pharos-server --all-targets -- -D warnings'`.

- [ ] **Step 5: Commit**
```bash
git add crates/pharos-server/src/online_enrich.rs crates/pharos-server/src/tmdb.rs crates/pharos-server/src/lib.rs
git commit -m "feat(tmdb): OnlineEnricher trait + TMDB search/detail parse + image fetch

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: TVDB client + enricher (v4 login/JWT)

**Files:**
- Create: `crates/pharos-server/src/tvdb.rs`
- Modify: `crates/pharos-server/src/lib.rs` (`pub mod tvdb;`)
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Produces: `TvdbClient::new(api_key: String)`; async `search_series`/`get_series`/`get_episode` returning parsed data; `TvdbEnricher(TvdbClient): OnlineEnricher` with `provider()=="tvdb"`, `supports(kind)= kind==Episode` (TV only). JWT cached in `Arc<tokio::sync::RwLock<Option<String>>>`; a 401 clears it and re-logins once.
- Base `https://api4.thetvdb.com/v4`; login `POST /login {"apikey": <key>}` → `.data.token`.

- [ ] **Step 1: Write auth + parse tests**

Auth is the risky part — test it against a trait-abstracted transport so no network is needed:
```rust
// A fake transport returns queued responses and counts login calls.
#[tokio::test]
async fn tvdb_jwt_cached_and_relogin_on_401() {
    let t = FakeTransport::new()
        .push_login("jwt-1")
        .push_ok(r#"{"data":{"id":121361,"name":"Game of Thrones"}}"#) // first call ok
        .push_401()                                                     // token expired
        .push_login("jwt-2")
        .push_ok(r#"{"data":{"id":121361,"name":"Game of Thrones"}}"#); // retried ok
    let c = TvdbClient::with_transport("key".into(), t.clone());
    assert!(c.get_series("121361").await.is_some());
    assert!(c.get_series("121361").await.is_some());
    assert_eq!(t.login_count(), 2); // logged in once, re-logged once after 401
}
#[test]
fn tvdb_parse_series_search_yields_candidates() {
    let body = r#"{"data":[{"tvdb_id":"121361","name":"Game of Thrones","year":"2011"}]}"#;
    let c = super::parse_series_search(body);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].id, "121361");
    assert_eq!(c[0].year, Some(2011));
}
```
Define a small `TvdbTransport` trait (`async fn get(&self, path, bearer) -> (u16, String)` and `async fn login(&self, apikey) -> Option<String>`); `TvdbClient` holds `Box<dyn TvdbTransport>`; the real impl wraps `reqwest`. This keeps the auth state-machine unit-testable.

- [ ] **Step 2: Run — fails**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-server tvdb_'`
Expected: FAIL — module missing.

- [ ] **Step 3: Implement**

Create `tvdb.rs`: the `TvdbTransport` trait + a `ReqwestTransport` impl (real HTTP), the `TvdbClient` holding `api_key`, `transport`, and `token: Arc<RwLock<Option<String>>>`. Core state-machine:
```rust
async fn authed_get(&self, path: &str) -> Option<String> {
    let mut token = self.ensure_token().await?;
    let (status, body) = self.transport.get(path, &token).await;
    if status == 401 {
        { *self.token.write().await = None; }        // clear + re-login once
        token = self.ensure_token().await?;
        let (s2, b2) = self.transport.get(path, &token).await;
        if s2 != 200 { return None; }
        return Some(b2);
    }
    if status != 200 { return None; }
    Some(body)
}
async fn ensure_token(&self) -> Option<String> {
    if let Some(t) = self.token.read().await.clone() { return Some(t); }
    let t = self.transport.login(&self.api_key).await?;
    *self.token.write().await = Some(t.clone());
    Some(t)
}
```
Add `parse_series_search`, `parse_series_detail`, `parse_episode_detail` (pure, over `serde_json::Value`, mirroring the TMDB parse fns but reading TVDB's `.data` envelope; artwork under `image`/`artworks`). Implement `OnlineEnricher for TvdbEnricher`: `supports` = `kind == MediaKind::Episode`; `search` → `parse_series_search`; `fetch(Episode, series_id, Some(s), Some(e))` → series+episode fetch, filling `also_tmdb_id` from TVDB's `remoteIds` (imdb/tmdb) when present. `fetch_image_bytes` downloads the absolute TVDB CDN URL. Add `pub mod tvdb;` to `lib.rs`.

- [ ] **Step 4: Run tests**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-server tvdb_'`
Expected: PASS. Clippy: `nix develop --command bash -lc 'cargo clippy -p pharos-server --all-targets -- -D warnings'`.

- [ ] **Step 5: Commit**
```bash
git add crates/pharos-server/src/tvdb.rs crates/pharos-server/src/lib.rs
git commit -m "feat(tvdb): v4 client with cached JWT + re-login, series/episode enricher

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Enrichment merge — `apply_enrichment` (fill-if-absent)

**Files:**
- Modify: `crates/pharos-server/src/online_enrich.rs`
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Produces: `pub struct AppliedEnrichment { pub genres: Vec<String>, pub people: Vec<pharos_core::PersonRef> }` and
  `pub fn apply_enrichment(item: &mut pharos_core::MediaItem, counts: pharos_core::EntityCounts, e: &EnrichedMetadata) -> AppliedEnrichment`
  — scalars fill ONLY when the item's field is `None` (local wins); genres/people returned for linking ONLY when `counts` shows the item currently has none of that kind (fill-if-empty); `provider_ids.tmdb`/`tvdb` fill-if-none.

- [ ] **Step 1: Write failing tests**
```rust
#[test]
fn apply_enrichment_fills_only_missing_scalars() {
    let mut item = bare_movie(); // helper: all metadata None
    item.metadata.overview = Some("local overview".into());
    let e = EnrichedMetadata {
        overview: Some("online overview".into()),
        production_year: Some(1999),
        genres: vec!["Sci-Fi".into()],
        provider_id: Some("603".into()),
        ..EnrichedMetadata::default()
    };
    let applied = apply_enrichment(&mut item, pharos_core::EntityCounts::default(), &e);
    assert_eq!(item.metadata.overview.as_deref(), Some("local overview")); // local kept
    assert_eq!(item.metadata.production_year, Some(1999));                  // gap filled
    assert_eq!(applied.genres, vec!["Sci-Fi"]);                            // item had 0 genres
}
#[test]
fn apply_enrichment_skips_joins_when_already_populated() {
    let mut item = bare_movie();
    let e = EnrichedMetadata { genres: vec!["Sci-Fi".into()], ..EnrichedMetadata::default() };
    let counts = pharos_core::EntityCounts { genres: 3, people: 0, studios: 0 };
    let applied = apply_enrichment(&mut item, counts, &e);
    assert!(applied.genres.is_empty()); // local NFO genres present -> online genres not linked
}
```

- [ ] **Step 2: Run — fails**, then **Step 3: implement**:
```rust
fn fill<T>(slot: &mut Option<T>, v: Option<T>) { if slot.is_none() { if let Some(v) = v { *slot = Some(v); } } }

pub struct AppliedEnrichment { pub genres: Vec<String>, pub people: Vec<pharos_core::PersonRef> }

pub fn apply_enrichment(
    item: &mut pharos_core::MediaItem, counts: pharos_core::EntityCounts, e: &EnrichedMetadata,
) -> AppliedEnrichment {
    let md = &mut item.metadata;
    fill(&mut md.overview, e.overview.clone());
    fill(&mut md.tagline, e.tagline.clone());
    fill(&mut md.production_year, e.production_year);
    fill(&mut md.premiere_date, e.premiere_date);
    fill(&mut md.community_rating, e.community_rating);
    fill(&mut md.official_rating, e.official_rating.clone());
    AppliedEnrichment {
        genres: if counts.genres == 0 { e.genres.clone() } else { vec![] },
        people: if counts.people == 0 { e.people.clone() } else { vec![] },
    }
}
```
(`provider_ids` slots are filled by the backfill after it knows which provider matched — Task 8 — so they aren't set here.)

- [ ] **Step 4: Run tests** — `cargo nextest run -p pharos-server apply_enrichment` → PASS.
- [ ] **Step 5: Commit**
```bash
git add crates/pharos-server/src/online_enrich.rs
git commit -m "feat(enrich): apply_enrichment fill-if-absent merge (local always wins)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Artwork download + cache + serve non-`local` sources

**Files:**
- Modify: `crates/pharos-store-sqlx/src/sqlite.rs` (`set_artwork` Primary predicate ~:1103)
- Modify: `crates/pharos-store-sqlx/src/postgres.rs` (`set_artwork` Primary predicate ~:1016)
- Modify: `crates/pharos-server/src/api/jellyfin/images.rs` (`local_artwork_path` source filter ~:656)
- Modify: `crates/pharos-server/src/metadata_backfill.rs` (created in Task 9 — but the download helper is defined here; if implementing strictly in order, place the helper in `online_enrich.rs` instead and call it from Task 9)
- Test: `crates/pharos-store-sqlx/tests/sqlite_store.rs` + inline

**Interfaces:**
- Produces: artwork rows with `source` in `{"local","tmdb","tvdb"}` are all servable local files; `has_primary_art` flips true for any of them; `local_artwork_path` serves any of them.
- `download_and_cache_art(cache: &ImageCache, store: &S, item: &MediaItem, provider: &str, art: &RemoteArt, bytes: Vec<u8>) -> DomainResult<()>` — writes bytes via `ImageCache::upload(item.id, role, item.kind, 0, &bytes)` then `store.set_artwork(item.id, role.as_str(), provider, cache_path)`.

- [ ] **Step 1: Widen the `has_primary_art` predicate + write test**

In `sqlite.rs` `set_artwork` Primary branch, change the bind from `source.eq_ignore_ascii_case("local")` to a servable-source check:
```rust
let servable = matches!(source.to_ascii_lowercase().as_str(), "local" | "tmdb" | "tvdb");
sqlx::query("UPDATE media_items SET has_primary_art = ? WHERE id = ?")
    .bind(servable).bind(id_i64)...
```
Mirror in `postgres.rs`. Add a store test:
```rust
#[tokio::test]
async fn tmdb_primary_artwork_flips_has_primary_art() {
    let store = new_test_store().await;
    let mut item = sample_movie("Arrival"); item.id = 900020;
    store.put(item.clone()).await.unwrap();
    store.set_artwork(900020, "Primary", "tmdb", "/cache/primary/movie/900020.jpg").await.unwrap();
    assert!(store.get(900020).await.unwrap().has_primary_art);
}
```

- [ ] **Step 2: Widen `local_artwork_path`'s source filter**

In `images.rs:656`, change the `.find(|(r, source, _)| r.eq_ignore_ascii_case(token) && source == "local")` predicate to accept downloaded providers:
```rust
.find(|(r, source, _)| r.eq_ignore_ascii_case(token)
    && matches!(source.to_ascii_lowercase().as_str(), "local" | "tmdb" | "tvdb"))
```
(A downloaded provider's `locator` is an absolute cache path, so the existing `try_exists` check serves it exactly like a sidecar.)

- [ ] **Step 3: Implement `download_and_cache_art`**

In `online_enrich.rs` (so Task 9 can call it), using `pharos_cache::image_cache::{ImageCache, ImageRole}` and mapping `ArtworkRole -> ImageRole` (they share names — write a `to_cache_role(ArtworkRole) -> ImageRole` match):
```rust
pub async fn download_and_cache_art<S: pharos_core::MediaStore>(
    cache: &pharos_cache::image_cache::ImageCache, store: &S,
    item: &pharos_core::MediaItem, provider: &str, art: &RemoteArt, bytes: Vec<u8>,
) -> pharos_core::DomainResult<()> {
    let role = to_cache_role(art.role);
    let path = cache.upload(item.id, role, item.kind, 0, &bytes).await
        .map_err(|e| pharos_core::DomainError::msg(format!("cache upload: {e}")))?;
    store.set_artwork(item.id, art.role.as_str(), provider, &path.to_string_lossy()).await
}
```
(Use the crate's actual `DomainError` constructor — grep `pharos-core` for how other server code builds a `DomainError` from a string; match that idiom, do not invent `DomainError::msg` if it doesn't exist.)

- [ ] **Step 4: Run tests + clippy**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-store-sqlx tmdb_primary_artwork_flips_has_primary_art && cargo build -p pharos-store-sqlx --features postgres --all-targets'`
Expected: PASS + clean postgres build. Clippy pharos-server.

- [ ] **Step 5: Commit**
```bash
git add crates/pharos-store-sqlx/src crates/pharos-server/src/api/jellyfin/images.rs crates/pharos-server/src/online_enrich.rs crates/pharos-store-sqlx/tests/sqlite_store.rs
git commit -m "feat(artwork): serve + count downloaded tmdb/tvdb artwork as on-disk local images

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: `metadata_backfill` orchestrator

**Files:**
- Create: `crates/pharos-server/src/metadata_backfill.rs`
- Modify: `crates/pharos-server/src/lib.rs` (`pub mod metadata_backfill;`)
- Test: inline `#[cfg(test)]` with fake enrichers + in-memory store

**Interfaces:**
- Produces:
  - `pub fn spawn(stores: Stores, bg_io: Arc<Semaphore>, cache: Arc<ImageCache>, tmdb: Option<TmdbEnricher>, tvdb: Option<TvdbEnricher>, cfg: MetadataConfig)` — fire-and-forget, mirrors T81.
  - `pub(crate) async fn run(...) -> DomainResult<usize>` — returns number of items enriched.
  - `pub(crate) async fn enrich_one(...)` — resolve one item (used by the manual-apply endpoint, Task 11).
- Consumes: `items_needing_match` (Task 2), `match_best` (Task 4), `OnlineEnricher` (Tasks 5/6), `apply_enrichment` + `download_and_cache_art` (Tasks 7/8), `set_item_match` (Task 2), `link_item_genres`/`link_item_people` + `put` (existing).

**Per-item algorithm (encode exactly):**
1. Choose provider by kind: `Episode` → prefer TVDB (fallback TMDB); `Movie` → TMDB.
2. Determine external id + source:
   - If `item.metadata.provider_ids.{tvdb|tmdb|imdb}` already set (from NFO) → `source="nfo_id"`, id known, **skip search**.
   - Else parse `(title, year)` via `pharos_scanner::metadata::filename::parse_stem(stem, item.kind==Movie)` and, for episodes, `(season, episode)` via `pharos_scanner`'s `SeriesInfo` on the item (`item.series`). Call `enricher.search(kind, title, year)` → `match_best(title, year, &cands, cfg.match_min_confidence)`. Some → `source="search"`, id = outcome.id, confidence = outcome.confidence. None → `set_item_match(id, provider, "", "none", None, now)` and return (leaves filename metadata).
3. `enricher.fetch(kind, &id, season, episode)` → `EnrichedMetadata` (None → treat as transient, do NOT mark; next pass retries).
4. `counts = store.item_entity_counts(item.id)`; `applied = apply_enrichment(&mut item, counts, &e)`; set `item.metadata.provider_ids.{provider} = Some(id)` (fill-if-none); `store.put(item)`.
5. If `!applied.genres.is_empty()` → `store.link_item_genres(id, &applied.genres)`; same for `people`.
6. **Artwork:** for TV prefer TMDB art (fallback TVDB): if the matched provider is TVDB and `e.also_tmdb_id` is set, additionally `tmdb.fetch(Episode-or-series, also_tmdb_id, ..)` for its artwork; choose, per role, TMDB first then TVDB. For each chosen `RemoteArt`: draw a `BgPermit`, `enricher.fetch_image_bytes(url)`, then `download_and_cache_art(...)`. Cap at the poster+backdrop+logo roles; `log` any skipped.
7. `store.set_item_match(id, provider, &id, source, confidence, now_secs)`.
8. `tokio::time::sleep(REQUEST_SPACING)` between items (mirror T81's 120ms).

- [ ] **Step 1: Write the idempotency + match tests (fakes)**
```rust
#[tokio::test]
async fn backfill_matches_by_search_and_persists_match_state() {
    let store = InMemoryStores::new();
    store.put_movie(900100, "Dune (2021)").await;               // no NFO id
    let tmdb = FakeEnricher::tmdb()
        .with_search("Dune", vec![("438631","Dune",Some(2021))])
        .with_detail("438631", enriched_overview("A duke's son..."));
    let cfg = MetadataConfig::default();
    let n = run(&store, &sem(4), &cache(), &Some(tmdb), &None, &cfg).await.unwrap();
    assert_eq!(n, 1);
    let got = store.get(900100).await.unwrap();
    assert_eq!(got.match_provider.as_deref(), Some("tmdb"));
    assert_eq!(got.match_source.as_deref(), Some("search"));
    assert_eq!(got.metadata.overview.as_deref(), Some("A duke's son..."));
}
#[tokio::test]
async fn backfill_never_reprocesses_manual_override() {
    let store = InMemoryStores::new();
    store.put_movie(900101, "Whatever").await;
    store.set_item_match(900101, "tmdb", "1", "manual", None, 1).await.unwrap();
    let tmdb = FakeEnricher::tmdb().with_search("Whatever", vec![("2","Whatever",None)]);
    let n = run(&store, &sem(4), &cache(), &Some(tmdb), &None, &MetadataConfig::default()).await.unwrap();
    assert_eq!(n, 0); // items_needing_match excludes manual
    assert_eq!(store.get(900101).await.unwrap().match_external_id.as_deref(), Some("1"));
}
#[tokio::test]
async fn backfill_no_confident_hit_marks_none() {
    let store = InMemoryStores::new();
    store.put_movie(900102, "Obscure Home Video").await;
    let tmdb = FakeEnricher::tmdb().with_search("Obscure Home Video", vec![("5","Something Else",None)]);
    run(&store, &sem(4), &cache(), &Some(tmdb), &None, &MetadataConfig::default()).await.unwrap();
    assert_eq!(store.get(900102).await.unwrap().match_source.as_deref(), Some("none"));
}
```
Define the fakes (`FakeEnricher` implementing `OnlineEnricher`, `InMemoryStores` implementing the `MediaStore` subset used) in the test module. `now_secs` must be injectable (pass a `now: i64` param to `run`/`enrich_one` rather than calling the clock) so tests are deterministic — the real `spawn` passes `now_secs()`.

- [ ] **Step 2: Run — fails**, then **Step 3: implement** `run`/`enrich_one`/`spawn` per the algorithm. Draw a `BgPermit::acquire(bg_io)` around each network call (search, fetch, image bytes) exactly like T81. Gate provider availability: if the kind's preferred enricher is `None`, fall back; if none available, skip the item.

- [ ] **Step 4: Run tests + clippy**

Run: `nix develop --command bash -lc 'cargo nextest run -p pharos-server backfill_'`
Expected: PASS. Clippy the crate `-D warnings`.

- [ ] **Step 5: Commit**
```bash
git add crates/pharos-server/src/metadata_backfill.rs crates/pharos-server/src/lib.rs
git commit -m "feat(enrich): metadata_backfill orchestrator (match, fetch, persist, art)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Wire the backfill into server startup

**Files:**
- Modify: `crates/pharos-server/src/main.rs` (near the T81 spawn ~:749)

**Interfaces:**
- Consumes: `metadata_backfill::spawn`, `TmdbEnricher`, `TvdbEnricher`, `cfg.tmdb`/`cfg.tvdb`/`cfg.metadata`, `state.stores`, `state.bg_io`, `state.image_cache` (grep `state.rs` for the cache field name).

- [ ] **Step 1: Add the spawn (gated on keys)**

After the T81 person-image spawn block in `main.rs`:
```rust
{
    let tmdb = match cfg.tmdb.api_key.as_deref() {
        Some(k) if !k.is_empty() => Some(pharos_server::online_enrich::TmdbEnricher::new(
            pharos_server::tmdb::TmdbClient::new(k.to_string()))),
        _ => None,
    };
    let tvdb = match cfg.tvdb.api_key.as_deref() {
        Some(k) if !k.is_empty() => Some(pharos_server::online_enrich::TvdbEnricher::new(
            pharos_server::tvdb::TvdbClient::new(k.to_string()))),
        _ => None,
    };
    if tmdb.is_some() || tvdb.is_some() {
        pharos_server::metadata_backfill::spawn(
            state.stores.clone(), state.bg_io.clone(), state.image_cache.clone(),
            tmdb, tvdb, cfg.metadata.clone());
    } else {
        tracing::info!("online metadata enrichment disabled (no [tmdb]/[tvdb] api_key)");
    }
}
```
(Match the real field names on `state`/`AppState`; if the cache is `state.cache` not `state.image_cache`, use that.)

- [ ] **Step 2: Build + smoke**

Run: `nix develop --command bash -lc 'CARGO_BUILD_JOBS=4 cargo build -p pharos-server'`
Expected: clean. With no keys set, startup logs the "disabled" line (no behavior change).

- [ ] **Step 3: Commit**
```bash
git add crates/pharos-server/src/main.rs
git commit -m "feat(server): spawn metadata_backfill at startup when a provider key is set

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Manual Identify endpoints

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/item_ops.rs` (`register` ~:20 + handlers)
- Test: `crates/pharos-server/tests/` (add an integration test or inline handler test mirroring the crate's existing route tests)

**Interfaces:**
- `GET /items/{id}/remotesearch/movie` and `/items/{id}/remotesearch/series` → JSON array of candidates `[{Name, ProductionYear, ProviderIds:{Tmdb|Tvdb}, ImageUrl?}]`.
- `POST /items/{id}/remotesearch/apply` body `{"Provider":"tmdb"|"tvdb","Id":"438631"}` → sets `match_source="manual"`, then runs `enrich_one` immediately for that item; returns 204.

- [ ] **Step 1: Write a handler test**

Mirror the pattern of an existing item_ops route test (grep the crate's `tests/` for how a route is driven — likely via the `client_compat` harness or an actix `test::TestRequest`). Assert: POST apply on a known item flips `match_source` to `manual` and (with a fake enricher wired into test state) writes the overview.

- [ ] **Step 2: Run — fails**, then **Step 3: implement** the three handlers + register:
```rust
// in register(cfg):
.route("/items/{id}/remotesearch/movie", web::get().to(remote_search_movie))
.route("/items/{id}/remotesearch/series", web::get().to(remote_search_series))
.route("/items/{id}/remotesearch/apply", web::post().to(remote_search_apply))
```
Handlers resolve the item via `state.stores.get(id)`, call the appropriate enricher's `search`, and serialize candidates with the crate's `wire::json`. `remote_search_apply` deserializes the body (a `#[serde(rename_all="PascalCase")]` DTO), calls `store.set_item_match(id, provider, &body.id, "manual", None, now)`, then `metadata_backfill::enrich_one(...)` for immediate refresh, and returns `HttpResponse::NoContent()`. Guard with `AuthUser` like the sibling handlers. If enrichers aren't available in `AppState`, add them to `AppState` at construction (Task 10 already builds them — store `Option<TmdbEnricher>`/`Option<TvdbEnricher>` on `AppState` so both startup-spawn and the endpoint share them).

- [ ] **Step 4: Run tests + clippy** → PASS.
- [ ] **Step 5: Commit**
```bash
git add crates/pharos-server/src/api/jellyfin/item_ops.rs crates/pharos-server/src/state.rs crates/pharos-server/tests
git commit -m "feat(api): RemoteSearch identify + apply endpoints for manual match override

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: Full-suite gate + spec sync + memory

**Files:**
- Modify: `docs/superpowers/specs/2026-07-22-online-metadata-artwork-tmdb-tvdb-design.md` (Architecture note: online work runs in the background pass, not the scan resolver)
- Modify: memory (`project_*` pointer)

- [ ] **Step 1: Full workspace test**

Run:
```
nix develop --command bash -lc 'just test'
nix develop --command bash -lc 'cargo test --doc --workspace'
nix develop --command bash -lc 'cargo clippy --workspace --all-targets -- -D warnings'
nix develop --command bash -lc 'cargo build --workspace --features postgres --all-targets'
```
Expected: all green. Fix any fallout before committing.

- [ ] **Step 2: Sync the spec's Architecture section** to state exact-id lookups run in the background pass (not inline in the scan resolver), matching the Deviation note at the top of this plan.

- [ ] **Step 3: Update memory** — add a `project_online_metadata_enrichment.md` pointer summarizing: TMDB+TVDB enrichment shipped; background `metadata_backfill` gated on keys; provider-agnostic match columns; manual Identify via RemoteSearch; artwork downloaded+cached+served as on-disk; secret `pharos-metadata-keys` in home-cluster.

- [ ] **Step 4: Commit**
```bash
git add docs/superpowers/specs /home/ali/.claude/projects/-home-ali-git-personal-pharos/memory
git commit -m "docs: sync metadata spec to background-pass architecture + memory pointer

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Deployment (after merge)

Not a code task — noted for the operator. The `pharos-metadata-keys` Secret is already live in the cluster (committed to home-cluster). To activate enrichment in production, add to `helmreleases/pharos.yaml` `extraEnv`:
```yaml
- name: PHAROS_TMDB_API_KEY
  valueFrom: { secretKeyRef: { name: pharos-metadata-keys, key: tmdb-api-key } }
- name: PHAROS_TVDB_API_KEY
  valueFrom: { secretKeyRef: { name: pharos-metadata-keys, key: tvdb-api-key } }
```
Bump the pharos image tag to the merged SHA. Wiring `PHAROS_TMDB_API_KEY` also activates the existing T81 person-portrait backfill.

---

## Self-Review

**Spec coverage:**
- TMDB movies+TV artwork+gap-fill → Tasks 5, 9. TVDB TV structure → Tasks 6, 9. ✓
- Provider precedence / local-wins → Task 7 (fill-if-absent) + fill-if-empty joins. ✓
- Matching (NFO id → search → none, confidence, threshold) → Tasks 4, 9. ✓
- Provider-agnostic match columns + override protection → Tasks 1, 2 (`items_needing_match` excludes manual/nfo_id). ✓
- Artwork download+cache+serve, `has_primary_art` widen → Task 8. ✓
- Full TV depth (series+season+episode) → Task 6 fetch + Task 9 season/episode routing. ✓
- Manual Identify → Task 11. ✓
- Background pass (BG_IO-gated, self-terminating, TTL) → Task 9 + `items_needing_match` ttl_cutoff. ✓
- Config + gating → Task 3, 10. ✓
- Migrations both backends → Task 1. ✓
- Gap surfaced: TVDB→TMDB artwork bridge relies on `also_tmdb_id` — Task 6 fills it from TVDB `remoteIds`; if a series lacks a TMDB remote id, TV artwork falls back to TVDB's own images (Task 9 step 6 "fallback TVDB"). Acceptable and covered.

**Placeholder scan:** No "TBD"/"handle errors"/"similar to". Each code step carries real code. Two steps explicitly instruct verifying a real symbol name against the codebase (`DomainError` constructor, `state` cache field) rather than inventing one — these are correctness guards, not placeholders.

**Type consistency:** `match_best`→`MatchOutcome{id,confidence}` used consistently (Task 4→9). `EnrichedMetadata`/`RemoteArt`/`OnlineEnricher` signatures identical across Tasks 5/6/7/9. `set_item_match(provider, external_id, source, confidence, refreshed_at)` argument order identical in Tasks 2, 9, 11. `EntityCounts{genres,people,studios}` consistent Tasks 2, 7. `download_and_cache_art` signature consistent Tasks 8, 9.
