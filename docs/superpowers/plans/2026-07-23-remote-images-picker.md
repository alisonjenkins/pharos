# RemoteImages Edit-Image Picker — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make jellyfin-web's Edit-Images dialog list alternate Primary/Backdrop/Logo art from TMDB/TVDB and let the user download a pick, for movies, episodes, and synth Series containers.

**Architecture:** Add one `OnlineEnricher::list_images(kind, id)` capability (all roles in one provider call), then wire the three Jellyfin RemoteImages endpoints in `item_ops.rs`, reusing the existing "one-off enricher from `AppState` keys" pattern (`RemoteSearch/Apply`). Real items store art via `set_artwork` + freeze via `set_item_match("manual")`; synth Series store via `upload_series_art` + freeze via `upsert_series_metadata(match_source="manual")`. Series Logo serves from the deterministic `series_image_path` (no schema change).

**Tech Stack:** actix-web, reqwest, serde_json, sqlx (no migration needed).

## Global Constraints

- Rust MSRV **1.80** — no `repeat_n`, no APIs newer than 1.80.
- clippy `-D warnings` with `unwrap_used`/`expect_used` **denied** outside `#[cfg(test)]`.
- Public image/list routes NEVER 500 or 404 on a provider blip / missing key — return an empty, well-shaped `200` (V6 spirit). Genuine client errors (missing required query params, unknown item id) DO get `400`/`404`.
- All provider parsers return empty `Vec` (never panic) on malformed bodies.
- Every error surfaced to a client carries the underlying cause, never a bare class.
- Times: Unix seconds via `crate::metadata_backfill::now_secs()`.
- Run inside the Nix devShell (`nix develop --command …`). Full `just test` + workspace clippy + both backend builds before final review.

---

### Task 1: Provider capability — `list_images`

**Files:**
- Modify: `crates/pharos-server/src/online_enrich.rs` (add `RemoteImage` + trait method)
- Modify: `crates/pharos-server/src/tmdb.rs` (impl + client call + parser + tests)
- Modify: `crates/pharos-server/src/tvdb.rs` (impl + client call + parser + tests)
- Modify: `crates/pharos-server/src/metadata_backfill.rs` (FakeEnricher gains the method)

**Interfaces:**
- Produces: `pharos_server::online_enrich::RemoteImage`, `OnlineEnricher::list_images(&self, kind: MediaKind, id: &str) -> impl Future<Output = Vec<RemoteImage>> + Send`.
- Consumes: existing `TmdbClient` (`http`, `api_key`, `API_BASE`, `IMAGE_BASE_ORIGINAL`), `TvdbClient::authed_get`, `ArtworkRole`.

- [ ] **Step 1: Add the `RemoteImage` type + trait method (online_enrich.rs)**

After the `RemoteArt` struct (~line 24) add:

```rust
/// One candidate image a provider offers for an already-resolved id, richer
/// than [`RemoteArt`] (carries dimensions / language / rating so the
/// Edit-Images picker can show and sort them). Downloading is still deferred
/// to [`OnlineEnricher::fetch_image_bytes`] on the chosen [`RemoteImage::url`].
#[derive(Debug, Clone, PartialEq)]
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

Add to the `OnlineEnricher` trait (after `fetch`):

```rust
    /// All candidate images the provider offers for an already-resolved `id`
    /// (every role in one call). Empty `Vec` on any transport/HTTP/decode
    /// error — best-effort, never panics, never fails the caller.
    fn list_images(
        &self,
        kind: MediaKind,
        id: &str,
    ) -> impl Future<Output = Vec<RemoteImage>> + Send;
```

- [ ] **Step 2: Run the build to see every impl break**

Run: `nix develop --command cargo build -p pharos-server`
Expected: FAIL — `not all trait items implemented: list_images` for `TmdbEnricher`, `TvdbEnricher`, `FakeEnricher`.

- [ ] **Step 3: Write the TMDB parser failing test (tmdb.rs `#[cfg(test)]`)**

```rust
#[test]
fn tmdb_parse_images_maps_roles_dims_and_rating() {
    let body = r#"{
      "posters":[{"file_path":"/p.jpg","width":2000,"height":3000,"iso_639_1":"en","vote_average":5.4,"vote_count":12}],
      "backdrops":[{"file_path":"/b.jpg","width":1920,"height":1080,"iso_639_1":null,"vote_average":4.1,"vote_count":3}],
      "logos":[{"file_path":"/l.png","width":800,"height":310,"iso_639_1":"en","vote_average":0.0,"vote_count":0}]
    }"#;
    let imgs = super::parse_tmdb_images(body);
    assert_eq!(imgs.len(), 3);
    let primary = imgs.iter().find(|i| i.role == ArtworkRole::Primary).unwrap();
    assert_eq!(primary.url, "https://image.tmdb.org/t/p/original/p.jpg");
    assert_eq!(primary.width, Some(2000));
    assert_eq!(primary.language.as_deref(), Some("en"));
    assert!((primary.community_rating.unwrap() - 5.4).abs() < 1e-4);
    assert!(imgs.iter().any(|i| i.role == ArtworkRole::Backdrop));
    assert!(imgs.iter().any(|i| i.role == ArtworkRole::Logo));
}

#[test]
fn tmdb_parse_images_empty_on_garbage() {
    assert!(super::parse_tmdb_images("not json").is_empty());
}
```

Run: `nix develop --command cargo test -p pharos-server tmdb_parse_images` → FAIL (no `parse_tmdb_images`).

- [ ] **Step 4: Implement the TMDB parser + client call + impl (tmdb.rs)**

Add near the other free `parse_*` fns:

```rust
use crate::online_enrich::RemoteImage;

/// Parse a TMDB `/movie|tv/{id}/images` body into role-tagged candidates.
/// `posters`→Primary, `backdrops`→Backdrop, `logos`→Logo. Empty on any
/// decode error (best-effort).
pub(crate) fn parse_tmdb_images(body: &str) -> Vec<RemoteImage> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let mut out = vec![];
    for (key, role) in [
        ("posters", ArtworkRole::Primary),
        ("backdrops", ArtworkRole::Backdrop),
        ("logos", ArtworkRole::Logo),
    ] {
        let Some(arr) = v.get(key).and_then(|x| x.as_array()) else {
            continue;
        };
        for img in arr {
            let Some(path) = img.get("file_path").and_then(|x| x.as_str()) else {
                continue;
            };
            out.push(RemoteImage {
                role,
                url: format!("{IMAGE_BASE_ORIGINAL}{path}"),
                width: img.get("width").and_then(|x| x.as_u64()).map(|n| n as u32),
                height: img.get("height").and_then(|x| x.as_u64()).map(|n| n as u32),
                language: img
                    .get("iso_639_1")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                community_rating: img
                    .get("vote_average")
                    .and_then(|x| x.as_f64())
                    .map(|f| f as f32),
                vote_count: img.get("vote_count").and_then(|x| x.as_u64()).map(|n| n as u32),
            });
        }
    }
    out
}
```

Add a client method on `impl TmdbClient` (near `search_tv`):

```rust
    /// List all images TMDB has for a resolved movie/series id. `kind`
    /// selects the endpoint (`/movie/{id}/images` vs `/tv/{id}/images`).
    /// Empty `Vec` on any transport/HTTP error.
    pub(crate) async fn list_images(&self, kind: MediaKind, id: &str) -> Vec<RemoteImage> {
        let path = match kind {
            MediaKind::Movie => format!("{API_BASE}/movie/{id}/images"),
            _ => format!("{API_BASE}/tv/{id}/images"),
        };
        let Ok(resp) = self
            .http
            .get(path)
            .query(&[("api_key", self.api_key.as_str())])
            .send()
            .await
        else {
            return vec![];
        };
        if !resp.status().is_success() {
            return vec![];
        }
        let Ok(body) = resp.text().await else {
            return vec![];
        };
        parse_tmdb_images(&body)
    }
```

Add to `impl OnlineEnricher for TmdbEnricher` (line ~228):

```rust
    async fn list_images(&self, kind: MediaKind, id: &str) -> Vec<RemoteImage> {
        self.0.list_images(kind, id).await
    }
```

(Ensure `RemoteImage` is imported where the impl lives; `use crate::online_enrich::RemoteImage;` at top of file.)

- [ ] **Step 5: Run TMDB parser tests**

Run: `nix develop --command cargo test -p pharos-server tmdb_parse_images` → PASS.

- [ ] **Step 6: Write TVDB parser failing test (tvdb.rs `#[cfg(test)]`)**

```rust
#[test]
fn tvdb_parse_artworks_maps_known_types() {
    let body = r#"{"data":{"artworks":[
      {"image":"https://a/p.jpg","type":2,"language":"eng","score":100,"width":680,"height":1000},
      {"image":"https://a/bg.jpg","type":3,"language":null,"score":50},
      {"image":"https://a/logo.png","type":23,"language":"eng","score":10},
      {"image":"https://a/icon.jpg","type":5,"language":"eng","score":1}
    ]}}"#;
    let imgs = super::parse_tvdb_artworks(body);
    // types 2/3/23 kept (Primary/Backdrop/Logo); type 5 (icon) dropped.
    assert_eq!(imgs.len(), 3);
    assert!(imgs.iter().any(|i| i.role == ArtworkRole::Primary && i.url == "https://a/p.jpg"));
    assert!(imgs.iter().any(|i| i.role == ArtworkRole::Backdrop));
    assert!(imgs.iter().any(|i| i.role == ArtworkRole::Logo));
    let p = imgs.iter().find(|i| i.role == ArtworkRole::Primary).unwrap();
    assert_eq!(p.width, Some(680));
    assert_eq!(p.language.as_deref(), Some("eng"));
}

#[test]
fn tvdb_parse_artworks_empty_on_garbage() {
    assert!(super::parse_tvdb_artworks("nope").is_empty());
}
```

Run: `nix develop --command cargo test -p pharos-server tvdb_parse_artworks` → FAIL.

- [ ] **Step 7: Implement TVDB parser + client call + impl (tvdb.rs)**

```rust
use crate::online_enrich::RemoteImage;

/// TVDB v4 series artwork `type` ids we surface. Others (icon=5, banner=1,
/// season art, …) are dropped — the picker only offers poster/backdrop/logo.
fn tvdb_artwork_role(type_id: i64) -> Option<ArtworkRole> {
    match type_id {
        2 => Some(ArtworkRole::Primary),   // poster
        3 => Some(ArtworkRole::Backdrop),  // background
        23 => Some(ArtworkRole::Logo),     // clearlogo
        _ => None,
    }
}

/// Parse a TVDB `/series/{id}/extended` body's `data.artworks[]` into
/// role-tagged candidates. Empty on any decode error / missing array.
pub(crate) fn parse_tvdb_artworks(body: &str) -> Vec<RemoteImage> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let Some(arr) = v.get("data").and_then(|d| d.get("artworks")).and_then(|a| a.as_array()) else {
        return vec![];
    };
    let mut out = vec![];
    for a in arr {
        let Some(role) = a.get("type").and_then(|x| x.as_i64()).and_then(tvdb_artwork_role) else {
            continue;
        };
        let Some(url) = a.get("image").and_then(|x| x.as_str()) else {
            continue;
        };
        out.push(RemoteImage {
            role,
            url: url.to_string(),
            width: a.get("width").and_then(|x| x.as_u64()).map(|n| n as u32),
            height: a.get("height").and_then(|x| x.as_u64()).map(|n| n as u32),
            language: a
                .get("language")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            community_rating: a.get("score").and_then(|x| x.as_f64()).map(|f| f as f32),
            vote_count: None,
        });
    }
    out
}
```

Client method on `impl<T: TvdbTransport> TvdbClient<T>` (reuses the same
extended endpoint `get_series` uses):

```rust
    /// List a series' artworks from its extended record. Empty on any
    /// auth/transport/decode failure.
    pub async fn list_series_artworks(&self, id: &str) -> Vec<RemoteImage> {
        let Some(body) = self.authed_get(&format!("/series/{id}/extended")).await else {
            return vec![];
        };
        parse_tvdb_artworks(&body)
    }
```

Add to `impl OnlineEnricher for TvdbEnricher<T>` (line ~385). TVDB only knows
series artwork, so any `kind` maps to the series id passed in:

```rust
    async fn list_images(&self, _kind: MediaKind, id: &str) -> Vec<RemoteImage> {
        self.0.list_series_artworks(id).await
    }
```

- [ ] **Step 8: Run TVDB parser tests**

Run: `nix develop --command cargo test -p pharos-server tvdb_parse_artworks` → PASS.

- [ ] **Step 9: Satisfy `FakeEnricher` (metadata_backfill.rs ~line 948)**

Add to the `impl OnlineEnricher for FakeEnricher` block:

```rust
        async fn list_images(
            &self,
            _kind: MediaKind,
            _id: &str,
        ) -> Vec<crate::online_enrich::RemoteImage> {
            vec![]
        }
```

- [ ] **Step 10: Build + full parser tests + clippy**

Run: `nix develop --command cargo build -p pharos-server` → OK.
Run: `nix develop --command cargo test -p pharos-server tmdb_parse_images tvdb_parse_artworks` → PASS.
Run: `nix develop --command cargo clippy -p pharos-server -- -D warnings` → clean.

- [ ] **Step 11: Commit**

```bash
git add crates/pharos-server/src/online_enrich.rs crates/pharos-server/src/tmdb.rs crates/pharos-server/src/tvdb.rs crates/pharos-server/src/metadata_backfill.rs
git commit -m "feat(enrich): OnlineEnricher::list_images — enumerate all provider art (TMDB/TVDB)"
```

---

### Task 2: Serve synth-Series Logo from the deterministic cache path

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/images.rs` (`series_metadata_art_path`)
- Test: same file's `#[cfg(test)]`

**Interfaces:**
- Consumes: `pharos_cache::image_cache::series_image_path(root, ImageRole, &str)` (already `pub`), `state.images` (Option<Arc<ImageCache>> — the cache root), `item.series.series_key()`.
- Produces: `series_metadata_art_path` now returns a path for `ImageRole::Logo`.

Background: `series_metadata_art_path` currently returns `None` for any role
other than Primary/Backdrop (those read `poster_locator`/`backdrop_locator`
from the DB). Series Logo has no DB column; instead serve the deterministic
`series_image_path(root, Logo, series_key)` when the file exists — the exact
path `upload_series_art` writes to in Task 3.

- [ ] **Step 1: Write the failing test**

Add to the images `#[cfg(test)]` module (mirror `synth_series_primary_serves_cached_tvdb_poster`):

```rust
#[actix_web::test]
async fn synth_series_logo_serves_cached_file_without_db_locator() {
    use pharos_cache::image_cache::{series_image_path, ImageRole};
    let state = /* build AppState with an ImageCache rooted at a tempdir — copy the
        harness the poster test uses */;
    let series_key = "Dragon Ball";
    // Write a logo into the deterministic slot.
    let root = /* the cache root PathBuf used by state.images */;
    let logo_path = series_image_path(&root, ImageRole::Logo, series_key);
    tokio::fs::create_dir_all(logo_path.parent().unwrap()).await.unwrap();
    tokio::fs::write(&logo_path, b"PNGBYTES").await.unwrap();

    let item = /* a representative episode MediaItem whose series.series_key() == series_key */;
    let got = super::series_metadata_art_path(&state, &item, ImageRole::Logo).await;
    assert_eq!(got.as_deref(), Some(logo_path.as_path()));
}
```

(Use the same AppState/tempdir/episode construction the adjacent poster test already uses — copy it verbatim rather than inventing a new harness.)

Run: `nix develop --command cargo test -p pharos-server synth_series_logo_serves_cached_file` → FAIL (returns `None`).

- [ ] **Step 2: Add the Logo branch to `series_metadata_art_path`**

Change the early role guard from `Primary | Backdrop` to also allow `Logo`,
and add a Logo arm that resolves the deterministic path instead of a DB
locator:

```rust
    if !matches!(role, ImageRole::Primary | ImageRole::Backdrop | ImageRole::Logo) {
        return None;
    }
    let series = item.series.as_ref()?;
    let key = series.series_key().to_string();

    // Logo has no DB locator column — it lives at the deterministic
    // series-cache path `upload_series_art` writes. Serve it iff present.
    if role == ImageRole::Logo {
        let cache = state.images.as_ref()?;
        let path = pharos_cache::image_cache::series_image_path(cache.root(), ImageRole::Logo, &key);
        return match tokio::fs::try_exists(&path).await {
            Ok(true) => Some(path),
            _ => None,
        };
    }

    // Primary/Backdrop unchanged — DB locator lookup below.
    let map = state.stores.series_metadata_by_keys(std::slice::from_ref(&key)).await.ok()?;
    let meta = map.get(&key)?;
    let locator = match role {
        ImageRole::Primary => meta.poster_locator.clone(),
        ImageRole::Backdrop => meta.backdrop_locator.clone(),
        _ => None,
    }?;
    // …existing try_exists tail…
```

If `ImageCache` has no public `root()` accessor, add one:
`pub fn root(&self) -> &std::path::Path { &self.root }` in `image_cache.rs`.

- [ ] **Step 3: Run the test**

Run: `nix develop --command cargo test -p pharos-server synth_series_logo_serves_cached_file` → PASS.
Run: `nix develop --command cargo test -p pharos-server -- images::` → existing image tests still PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/pharos-server/src/api/jellyfin/images.rs crates/pharos-cache/src/image_cache.rs
git commit -m "feat(images): serve synth-Series Logo from deterministic cache path"
```

---

### Task 3: The three RemoteImages endpoints

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/item_ops.rs` (replace stubs, add download, DTOs, helpers, routes, tests)

**Interfaces:**
- Consumes: `OnlineEnricher::list_images` (Task 1), `series_metadata_art_path` Logo (Task 2), `TmdbEnricher`/`TvdbEnricher`/`TmdbClient`/`TvdbClient`/`ReqwestTransport` (already imported), `download_and_cache_art`, `RemoteArt`, `set_item_match`, `upsert_series_metadata`, `series_metadata_by_keys`, `ImageCache::upload_series_art`, `resolve_synth_image_item` (private in images.rs — see Step 5), `now_secs`.
- Produces: functional `GET /Items/{id}/RemoteImages`, `GET /Items/{id}/RemoteImages/Providers`, `POST /Items/{id}/RemoteImages/Download`.

- [ ] **Step 1: Replace the RemoteImages DTOs (item_ops.rs ~line 244)**

Remove `RemoteImageResultDto` (the empty-stub struct). Add:

```rust
/// One image in a `GET /Items/{id}/RemoteImages` result — Jellyfin's
/// `RemoteImageInfo`. `RatingType` is always `"Score"` (both providers give a
/// numeric score, not a like/dislike).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteImageInfoDto {
    provider_name: String,
    url: String,
    #[serde(rename = "Type")]
    image_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    community_rating: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vote_count: Option<u32>,
    rating_type: &'static str,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteImagesResultDto {
    images: Vec<RemoteImageInfoDto>,
    total_record_count: u32,
    providers: Vec<String>,
}

/// Query for `GET /Items/{id}/RemoteImages`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteImagesQuery {
    #[serde(rename = "Type", default)]
    image_type: Option<String>,
    #[serde(default)]
    provider_name: Option<String>,
}

/// Query for `POST /Items/{id}/RemoteImages/Download`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteImageDownloadQuery {
    #[serde(rename = "Type")]
    image_type: String,
    image_url: String,
    #[serde(default)]
    provider_name: Option<String>,
}
```

- [ ] **Step 2: Add provider-name mapping + resolution helpers**

```rust
/// Jellyfin display name ↔ internal provider token.
fn provider_display(token: &str) -> &'static str {
    match token {
        "tmdb" => "TheMovieDb",
        _ => "TheTVDB",
    }
}
fn provider_token(display: &str) -> Option<&'static str> {
    match display.to_ascii_lowercase().as_str() {
        "themoviedb" | "tmdb" => Some("tmdb"),
        "thetvdb" | "tvdb" => Some("tvdb"),
        _ => None,
    }
}

/// `ArtworkRole` → Jellyfin image-type token (Primary/Backdrop/Logo).
fn role_token(role: pharos_core::ArtworkRole) -> &'static str {
    role.as_str()
}

/// A requested Jellyfin `Type` token → `ArtworkRole`, restricted to the three
/// roles the picker supports. `None` for any other type (→ 400 on download).
/// (`ArtworkRole` has no `from_str_ci`; `ImageRole` does, so parse via it.)
fn artwork_role_from_type(t: &str) -> Option<pharos_core::ArtworkRole> {
    use pharos_cache::image_cache::ImageRole;
    match ImageRole::from_str_ci(t)? {
        ImageRole::Primary => Some(pharos_core::ArtworkRole::Primary),
        ImageRole::Backdrop => Some(pharos_core::ArtworkRole::Backdrop),
        ImageRole::Logo => Some(pharos_core::ArtworkRole::Logo),
        _ => None,
    }
}

/// The provider + id to enumerate images from, for either a real item or a
/// synth Series container. `None` when nothing is matched (→ empty list).
struct ImageMatch {
    provider: &'static str, // "tmdb" | "tvdb"
    external_id: String,
    kind: MediaKind,
}

/// Resolve the (provider, id, kind) to list images for.
/// - Real item: prefer whichever `metadata.provider_ids` is set AND has a key.
/// - Synth Series: `series_metadata` row via `series_key`.
async fn resolve_image_match(state: &AppState, id_str: &str) -> Option<ImageMatch> {
    use pharos_core::SeriesMetadataStore;
    // Real numeric id?
    if let Some(id) = pharos_jellyfin_api::dto::parse_item_id(id_str) {
        if let Ok(item) = state.stores.get(id).await {
            let ids = &item.metadata.provider_ids;
            if let Some(tmdb) = ids.tmdb.as_deref().filter(|s| !s.is_empty()) {
                return Some(ImageMatch { provider: "tmdb", external_id: tmdb.to_string(), kind: item.kind });
            }
            if let Some(tvdb) = ids.tvdb.as_deref().filter(|s| !s.is_empty()) {
                return Some(ImageMatch { provider: "tvdb", external_id: tvdb.to_string(), kind: item.kind });
            }
            return None;
        }
    }
    // Synth Series/Season → representative episode → series_key → series_metadata.
    let item = crate::api::jellyfin::images::resolve_synth_image_item(state, id_str).await?;
    let key = item.series.as_ref()?.series_key().to_string();
    let map = state.stores.series_metadata_by_keys(std::slice::from_ref(&key)).await.ok()?;
    let meta = map.get(&key)?;
    let provider = match meta.match_provider.as_deref()? {
        "tmdb" => "tmdb",
        "tvdb" => "tvdb",
        _ => return None,
    };
    Some(ImageMatch {
        provider,
        external_id: meta.match_external_id.clone()?,
        kind: MediaKind::Episode, // series-level, like the enrichment path
    })
}
```

`resolve_synth_image_item` is currently private to `images.rs`. Make it
`pub(crate)` there (change `async fn resolve_synth_image_item` →
`pub(crate) async fn resolve_synth_image_item`).

- [ ] **Step 3: Implement the list + providers handlers**

```rust
/// Core of `GET /Items/{id}/RemoteImages`. Empty (200) on no key / no match /
/// provider blip. Filters to the requested `Type` when given.
async fn remote_images_inner(
    state: &AppState,
    id_str: &str,
    q: &RemoteImagesQuery,
) -> RemoteImagesResultDto {
    let Some(m) = resolve_image_match(state, id_str).await else {
        return RemoteImagesResultDto { images: vec![], total_record_count: 0, providers: vec![] };
    };
    // Build the matched provider's enricher iff its key is configured.
    let images: Vec<crate::online_enrich::RemoteImage> = match m.provider {
        "tmdb" => match non_empty(state.tmdb_api_key.as_deref()) {
            Some(key) => TmdbEnricher(TmdbClient::new(key.to_string())).list_images(m.kind, &m.external_id).await,
            None => vec![],
        },
        _ => match non_empty(state.tvdb_api_key.as_deref()) {
            Some(key) => TvdbEnricher(TvdbClient::new(key.to_string())).list_images(m.kind, &m.external_id).await,
            None => vec![],
        },
    };
    let want = q.image_type.as_deref().and_then(|t| {
        // normalise the requested type to an ArtworkRole token for compare
        Some(t.to_string())
    });
    let dtos: Vec<RemoteImageInfoDto> = images
        .into_iter()
        .filter(|img| match &want {
            Some(t) => role_token(img.role).eq_ignore_ascii_case(t),
            None => true,
        })
        .map(|img| RemoteImageInfoDto {
            provider_name: provider_display(m.provider).to_string(),
            url: img.url,
            image_type: role_token(img.role).to_string(),
            height: img.height,
            width: img.width,
            language: img.language,
            community_rating: img.community_rating,
            vote_count: img.vote_count,
            rating_type: "Score",
        })
        .collect();
    let providers = if dtos.is_empty() { vec![] } else { vec![provider_display(m.provider).to_string()] };
    RemoteImagesResultDto { total_record_count: dtos.len() as u32, images: dtos, providers }
}

async fn remote_images(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: web::Query<RemoteImagesQuery>,
) -> impl Responder {
    let result = remote_images_inner(&state, &path.into_inner(), &q).await;
    crate::api::jellyfin::wire::json(&result)
}

async fn remote_image_providers(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> impl Responder {
    let providers = match resolve_image_match(&state, &path.into_inner()).await {
        Some(m) => vec![provider_display(m.provider).to_string()],
        None => vec![],
    };
    crate::api::jellyfin::wire::json(&providers)
}
```

(Replace the two old stub handlers entirely. Keep their old signatures'
route registrations — updated in Step 6.)

- [ ] **Step 4: Implement the download handler**

```rust
/// Plain public-CDN GET for the chosen image bytes (both TMDB and TVDB serve
/// art from public CDNs — no auth needed for the download itself). `None`
/// with the reason logged on any transport/HTTP error.
async fn fetch_url_bytes(url: &str) -> Result<Vec<u8>, String> {
    let resp = reqwest::Client::new().get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("image host returned {}", resp.status()));
    }
    resp.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string())
}

/// Core of `POST /Items/{id}/RemoteImages/Download`. Fetches the chosen URL,
/// caches it as the item's art, and freezes the row to `manual` so the
/// background enrichment pass never clobbers the curated pick.
async fn remote_images_download_inner(
    state: &AppState,
    id_str: &str,
    q: RemoteImageDownloadQuery,
) -> Result<(), actix_web::Error> {
    let role = artwork_role_from_type(&q.image_type)
        .ok_or_else(|| error::ErrorBadRequest("unsupported image Type"))?;
    let bytes = fetch_url_bytes(&q.image_url)
        .await
        .map_err(|e| error::ErrorBadRequest(format!("could not fetch image: {e}")))?;
    let now = crate::metadata_backfill::now_secs();
    let provider = q
        .provider_name
        .as_deref()
        .and_then(provider_token)
        .unwrap_or("tmdb");

    // Real numeric id → item art + freeze identity.
    if let Some(id) = pharos_jellyfin_api::dto::parse_item_id(id_str) {
        if let Ok(item) = state.stores.get(id).await {
            let cache = state.images.as_ref().ok_or_else(|| error::ErrorInternalServerError("no image cache configured"))?;
            let art = crate::online_enrich::RemoteArt { role, url: q.image_url.clone() };
            crate::online_enrich::download_and_cache_art(cache, &state.stores, &item, provider, &art, bytes)
                .await
                .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
            // Freeze: keep existing provider id if any, else the download's provider.
            let ext = item.metadata.provider_ids.tmdb.clone()
                .or(item.metadata.provider_ids.tvdb.clone())
                .unwrap_or_default();
            state.stores.set_item_match(id, provider, &ext, "manual", None, now)
                .await
                .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
            return Ok(());
        }
    }

    // Synth Series → series_metadata locator + freeze.
    let item = crate::api::jellyfin::images::resolve_synth_image_item(state, id_str)
        .await
        .ok_or_else(|| error::ErrorNotFound("not found"))?;
    let key = item.series.as_ref().ok_or_else(|| error::ErrorNotFound("not found"))?.series_key().to_string();
    let cache = state.images.as_ref().ok_or_else(|| error::ErrorInternalServerError("no image cache configured"))?;
    let image_role = pharos_cache::image_cache::ImageRole::from_str_ci(&q.image_type)
        .ok_or_else(|| error::ErrorBadRequest("unsupported image Type"))?;
    let path = cache.upload_series_art(&key, image_role, &bytes)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("series art cache: {e}")))?;

    use pharos_core::SeriesMetadataStore;
    let mut meta = state.stores.series_metadata_by_keys(std::slice::from_ref(&key)).await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?
        .remove(&key)
        .unwrap_or_else(|| pharos_core::SeriesMetadata { series_key: key.clone(), series_name: key.clone(), ..Default::default() });
    match role {
        pharos_core::ArtworkRole::Primary => meta.poster_locator = Some(path.to_string_lossy().into_owned()),
        pharos_core::ArtworkRole::Backdrop => meta.backdrop_locator = Some(path.to_string_lossy().into_owned()),
        _ => {} // Logo lives at the deterministic path; no locator column.
    }
    meta.match_source = Some("manual".to_string());
    meta.metadata_refreshed_at = Some(now);
    state.stores.upsert_series_metadata(meta).await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(())
}

async fn remote_images_download(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
    q: web::Query<RemoteImageDownloadQuery>,
) -> Result<impl Responder, actix_web::Error> {
    remote_images_download_inner(&state, &path.into_inner(), q.into_inner()).await?;
    Ok(HttpResponse::NoContent().finish())
}
```

Confirm `pharos_core::SeriesMetadata` derives `Default` (it has
`#[derive(... Default ...)]`? if not, construct all fields explicitly instead
of `..Default::default()`).

- [ ] **Step 5: Confirm `SeriesMetadata: Default`**

Run: `nix develop --command cargo build -p pharos-server` — if it errors on
`Default`, either add `#[derive(Default)]` to `SeriesMetadata` in
`pharos-core` (check no field forbids it — all are `Option`/`Vec`/`String`,
so Default is derivable) or build the struct literal explicitly. Prefer
deriving `Default` (mechanical, safe).

- [ ] **Step 6: Register the POST route + keep the two GETs**

Find the RemoteImages route registration block (near the `remote_search_*`
routes ~line 40) and ensure:

```rust
.service(
    web::resource("/Items/{id}/RemoteImages")
        .route(web::get().to(remote_images)),
)
.service(
    web::resource("/Items/{id}/RemoteImages/Providers")
        .route(web::get().to(remote_image_providers)),
)
.service(
    web::resource("/Items/{id}/RemoteImages/Download")
        .route(web::post().to(remote_images_download)),
)
```

(Match the existing registration style in this file — if routes are wired in a
central router module instead, add the Download route there beside the
existing two.)

- [ ] **Step 7: Handler tests (item_ops.rs `#[cfg(test)]`)**

Drive the `_inner` fns directly (like `remote_search_*` tests):

```rust
#[actix_web::test]
async fn remote_images_empty_without_key_or_match() {
    let state = seed_state().await;
    put_movie(&state, 42, "Dune (2021)").await; // no provider id, no key
    let r = remote_images_inner(&state, "42", &RemoteImagesQuery { image_type: None, provider_name: None }).await;
    assert_eq!(r.total_record_count, 0);
    assert!(r.providers.is_empty());
}

#[actix_web::test]
async fn download_missing_url_is_400() {
    // web::Query rejects a missing ImageUrl before the handler; assert via the
    // parse, or call _inner with an unreachable host and assert BadRequest.
    let state = seed_state().await;
    put_movie(&state, 43, "X").await;
    let err = remote_images_download_inner(&state, "43", RemoteImageDownloadQuery {
        image_type: "Primary".into(),
        image_url: "http://127.0.0.1:1/nope.jpg".into(), // unreachable → fetch error → 400
        provider_name: Some("TheMovieDb".into()),
    }).await.unwrap_err();
    assert_eq!(err.as_response_error().status_code(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[actix_web::test]
async fn download_bad_type_is_400() {
    let state = seed_state().await;
    put_movie(&state, 44, "X").await;
    let err = remote_images_download_inner(&state, "44", RemoteImageDownloadQuery {
        image_type: "Nonsense".into(),
        image_url: "https://example/x.jpg".into(),
        provider_name: None,
    }).await.unwrap_err();
    assert_eq!(err.as_response_error().status_code(), actix_web::http::StatusCode::BAD_REQUEST);
}
```

For a positive download test, serve bytes from a local `httptest`/`wiremock`
mock if one is already a dev-dependency; otherwise assert the freeze path by
calling `set_item_match` semantics indirectly — check the repo for an existing
HTTP-mock helper before adding a dependency. If none exists, keep the positive
path covered by the unreachable-host negative test plus a direct unit test of
`resolve_image_match` returning the right provider for an item carrying a
`provider_ids.tmdb`.

```rust
#[actix_web::test]
async fn resolve_match_reads_item_provider_id() {
    let state = seed_state().await;
    // put a movie with metadata.provider_ids.tmdb = "603"
    let mut item = /* MediaItem id 50, kind Movie */;
    item.metadata.provider_ids.tmdb = Some("603".into());
    state.stores.upsert(item).await.unwrap();
    let m = super::resolve_image_match(&state, "50").await.unwrap();
    assert_eq!(m.provider, "tmdb");
    assert_eq!(m.external_id, "603");
}
```

(Use whatever `upsert`/put helper the existing item_ops tests already use to
seed a `MediaItem`.)

Run: `nix develop --command cargo test -p pharos-server -- item_ops::` → PASS.

- [ ] **Step 8: clippy + commit**

Run: `nix develop --command cargo clippy -p pharos-server -- -D warnings` → clean.

```bash
git add crates/pharos-server/src/api/jellyfin/item_ops.rs crates/pharos-server/src/api/jellyfin/images.rs crates/pharos-core/src/lib.rs
git commit -m "feat(api): RemoteImages list/providers/download endpoints (TMDB/TVDB poster/backdrop/logo)"
```

---

### Task 4: Feature-inventory coverage

**Files:**
- Modify: `crates/pharos-server/tests/jellyfin_feature_inventory.rs`

**Interfaces:**
- Consumes: the live endpoints from Task 3.

- [ ] **Step 1: Add a Download-endpoint smoke assertion**

Near the existing `remote_image_search` test (~line 278), add:

```rust
#[actix_web::test]
async fn remote_image_download_route_exists() {
    let f = seed_rich().await;
    // Unreachable ImageUrl → handler runs → 400 (route wired), NOT 404 (missing route).
    let status = post_status(
        &f,
        &format!(
            "/Items/{}/RemoteImages/Download?Type=Primary&ImageUrl=http://127.0.0.1:1/x.jpg&ProviderName=TheMovieDb",
            f.rich_item_id
        ),
        "",
    )
    .await;
    assert_eq!(status, 400, "download route should be wired (400 on bad fetch), not 404");
}
```

If a `post_status` helper does not exist in this test file, add a minimal one
mirroring `get_status` (POST, empty body). Confirm the existing
`remote_image_search` test still passes (the response shape changed from the
empty stub to the real `RemoteImagesResultDto`, still `200`).

- [ ] **Step 2: Run + commit**

Run: `nix develop --command cargo test -p pharos-server --test jellyfin_feature_inventory` → PASS.

```bash
git add crates/pharos-server/tests/jellyfin_feature_inventory.rs
git commit -m "test(compat): cover RemoteImages/Download route"
```

---

## Final verification (before merge)

- [ ] `nix develop --command just test` — full workspace green.
- [ ] `nix develop --command cargo test --doc --workspace` — doctests green.
- [ ] `nix develop --command cargo clippy --workspace --all-targets -- -D warnings` — clean.
- [ ] `nix develop --command cargo build -p pharos-server --no-default-features --features backend-spawn` — spawn backend builds.
- [ ] If any `Cargo.toml` changed: `just hakari-regen`. (No dep changes expected.)
- [ ] Manual sanity: `resolve_image_match` returns `tvdb` for a synth Dragon Ball id whose `series_metadata.match_provider = "tvdb"`.

## Notes / non-goals (carried from the spec)

- No delete/reorder/upload-from-disk image ops.
- No per-Season art picking (targets the Series container id).
- Series art source = the matched provider only (no TMDB-bridge enrichment of TVDB-matched series). Note as a future enhancement.
- Picking an image pins the item/series identity to `manual` (documented tradeoff — protects the single art row/locator from the next enrichment pass, mirrors `RemoteSearch/Apply`).
