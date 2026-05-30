# Library + Import Backlog — toward a Plex/Jellyfin superset

> Status: backlog (not yet scheduled into `SPEC.md §T`). Generated from a library +
> import audit of the scanner, domain model, DB, and Jellyfin API.

## Vision

Pharos is a **superset** of Plex and Jellyfin. The Jellyfin API we expose today
(and a future Plex API) are **lossy-but-compatible projections** for *their*
clients; pharos's own clients (`pharos-ui`) expose the full, richer feature set.
This backlog grows the canonical `pharos-core` model into that superset and makes
the import pipeline state-of-the-art.

## Architecture principles (hold across every item)

- **Canonical-superset, lossy projection.** Enrich `pharos-core`; the Jellyfin DTO
  mapping (`pharos-jellyfin-api`) projects it down lossily. Fields Jellyfin can't
  represent simply don't appear in that projection but are served in full to pharos
  clients. A future Plex API is another projection of the same canonical model.
- **Invariants preserved**: V5 (walk in `spawn_blocking`), V6 (prober crash never
  crashes the server — extended to malformed NFO + network failures), V10 (per-put
  atomicity), V12 (`pharos-core` is IO-free traits/structs only). The path-id
  (`xxh3(path)`) and the 32-hex synth-ids (`pharos-jellyfin-api/src/dto.rs:706-734`)
  stay as the Jellyfin wire identity; new identity/entities are **added alongside**.
- **Migrations are additive** (`ALTER ... ADD` nullable / `CREATE TABLE`), matching
  the 0005–0015 pattern. Dual-backend (sqlite + postgres) mirrored.

## Decisions

- **Metadata: local-first, online opt-in.** Default provider = NFO + sidecar art +
  embedded tags (offline, deterministic, no keys). Online providers
  (TMDB/TVDB/MusicBrainz/fanart.tv) are pluggable + opt-in behind config keys; a
  `MetadataProvider` priority-merge lets local NFO edits win.
- **Sequencing: leverage order** — import robustness → scale → canonical entities →
  local metadata → online providers → superset discovery/UX → subtitle correctness.

## Audited gaps (what's ABSENT vs Jellyfin/Plex)

- **Import robustness**: full-walk-every-time (no mtime/size/`last_scanned`
  incremental), no deletion cleanup (orphans persist forever), no move/rename
  tracking (moved file = new path-id = lost watch history), no FS watcher,
  sequential single-probe.
- **Scale**: `/Items` loads the whole library and filters in memory (won't scale
  past ~10k); substring-only search; no SQL pushdown / FTS.
- **Canonical model**: no People/cast/crew (endpoint returns `[]`), Studios faked
  from `album_artist`, no Collections/box-sets, no Playlists, Tags hardcoded empty,
  no Ratings (community/critic/parental), no year/premiere-date, no overview/tagline,
  no provider IDs (tmdb/tvdb/imdb/mbid), no typed `libraries`.
- **Metadata enrichment**: zero NFO parsing, zero sidecar artwork detection, zero
  filename year/quality parsing, zero external providers, no edit/identify flow.
- **Discovery**: `/Items/{id}/Similar` empty, no recommendations, no
  collections/people/real-library browsing, no playlists.
- **Subtitles**: discovery too narrow (misses `Subs/` folders, language/multi-token
  names, `.ass`); out-of-sync on transcode/seek (timeline not aligned to delivered
  video PTS).

---

## Backlog

Effort: S ≈ <1d, M ≈ 1–3d, L ≈ >3d. **Proj** = needs a Jellyfin-DTO projection
update. IDs are placeholders; they become numbered `SPEC.md §T` tasks when scheduled.

### EPIC A — Import robustness  *(highest leverage, no network)*

| ID | Feature | Effort | Deps |
|----|---------|--------|------|
| LIB-A1 | Scan-state columns + `scan_runs` ledger (mig 0016) | S | — |
| LIB-A2 | Incremental scan — skip unchanged files (mtime+size) in `scan_into` | M | A1 |
| LIB-A3 | Deletion reconciliation — mark-and-sweep orphans, root-scoped | M | A1 |
| LIB-A4 | Broadcast scan deltas on the socket bus (added/removed) | S | A2,A3 |
| LIB-A5 | Parallel probing — bounded worker pool over files (respect V6 isolation) | M | A2 |
| LIB-A6 | Content fingerprint column + `find_by_fp` (mig 0020) | S | A1 |
| LIB-A7 | Move/rename detection — rebind path on same row by fingerprint (keep id + history) | M | A6 |
| LIB-A8 | FS watcher (`notify`, feature `watch`) — native near-real-time updates on local FS | M | A2,A7 |
| LIB-A9 | Tiered change-detection: detect non-watchable roots (NFS/SMB/CIFS/FUSE or watcher-init failure) → graceful fallback to periodic incremental rescan; per-root mode logged + auto-downgrade on watcher error; configurable poll interval | M | A2,A8 |
| LIB-A10 | Scan status/progress + per-root change-detection mode in admin API + UI | S | A1,A9 |

### EPIC B — Scale: SQL pushdown + search

| ID | Feature | Effort | Deps |
|----|---------|--------|------|
| LIB-B1 | `MediaQuery` + `MediaStore::query` (SQL WHERE/ORDER/LIMIT + total) | L | — |
| LIB-B2 | Route `/Items` + `/Users/{u}/Items` through `query` (golden-snapshot parity) | M | B1 |
| LIB-B3 | Covering indexes (title, (kind,created_at), genre/artist/album) | S | B1 |
| LIB-B4 | FTS5 `media_fts` + triggers (mig 0017); `/Search/Hints` uses it | M | B1 |
| LIB-B5 | Faceted search counts (aggregate-before-filter) | M | B4 |

### EPIC C — Canonical superset model + entities

| ID | Feature | Effort | Deps | Proj |
|----|---------|--------|------|------|
| LIB-C1 | `libraries` table — typed roots (movies/tvshows/music/mixed) + options (mig 0019); real `/Library/VirtualFolders` | L | B1 | ✓ |
| LIB-C2 | People/cast/crew entities + `item_people` (role/char/order); real `/Persons`, `/Items?PersonId=` | L | — | ✓ |
| LIB-C3 | Studios entities (replace album_artist overload); real `/Studios` | M | — | ✓ |
| LIB-C4 | Genres as entities + `item_genres`; `/Genres` from rows | M | — | ✓ |
| LIB-C5 | Collections / box sets + `collection_items`; `/Collections` CRUD + browse | L | C1 | ✓ |
| LIB-C6 | Tags + `item_tags`; populate `BaseItemDto.Tags` | S | — | ✓ |
| LIB-C7 | Ratings (community/critic/official-parental) + year/premiere-date columns | M | — | ✓ |
| LIB-C8 | Overview / tagline / studios / genres as first-class DTO fields | S | C3,C4,C7 | ✓ |
| LIB-C9 | Provider-id columns (tmdb/tvdb/imdb/mbid) + DTO `ProviderIds` | S | — | ✓ |
| LIB-C10 | Synth-id ↔ entity `wire_id` mapping (exact ParentId resolution, no breakage) | M | C1–C4 | ✓ |
| LIB-C11 | **Series/season entity keyed on folder path** (+ year + provider-id), not bare name — fixes same-name shows merging/interleaving (*Cosmos* 1980 vs 2014). Episodes group by series folder; year from `Show (YYYY)` folder or `tvshow.nfo` | M | C7,C9,D6 | ✓ |

### EPIC D — Local metadata (default provider)

| ID | Feature | Effort | Deps |
|----|---------|--------|------|
| LIB-D1 | `MetadataProvider` trait + `MetadataResolver` priority-merge (core) | M | C* |
| LIB-D2 | NFO reader (Kodi movie/tvshow/episode/album NFO via quick-xml) | L | D1 |
| LIB-D3 | NFO write-back (persist edits/identify back to disk) | M | D2 |
| LIB-D4 | Sidecar artwork detection (poster/fanart/cover/folder.jpg) + `artwork` table (mig 0021) | M | D1 |
| LIB-D5 | `images.rs` serves local sidecar first, frame-extract fallback; per-season art | M | D4 |
| LIB-D6 | Filename parsing (year/quality/source/edition) folded into edition grouping | M | D1 |
| LIB-D7 | Scanner wires resolver → entity stores in the put transaction | M | D1,C* |
| LIB-D8 | Lyrics (.lrc sidecar + embedded) for music | S | D1 |

### EPIC E — Online providers (opt-in)

| ID | Feature | Effort | Deps |
|----|---------|--------|------|
| LIB-E1 | Provider config surface (keys, enable flags, per-library order) + admin UI | M | D1 |
| LIB-E2 | HTTP cache + per-provider token-bucket rate limiter (in pharos-cache) | M | E1 |
| LIB-E3 | TMDB provider (movies/tv) | L | E2 |
| LIB-E4 | TVDB provider (tv) | L | E2 |
| LIB-E5 | MusicBrainz + cover-art-archive provider (music) | L | E2 |
| LIB-E6 | fanart.tv provider (artwork) | M | E2 |
| LIB-E7 | "Identify / Match" + "Refresh Metadata" admin actions (force re-resolve) | M | E3,E4 |
| LIB-E8 | Subtitle search/download provider (OpenSubtitles) | M | E2 |

### EPIC F — Superset discovery / UX  *(pharos clients)*

| ID | Feature | Effort | Deps | Proj |
|----|---------|--------|------|------|
| LIB-F1 | `/Items/{id}/Similar` — genre + people + studio overlap scoring | M | C2–C4 | ✓ |
| LIB-F2 | Recommendations / "Because you watched" rows | L | F1 | ✓ |
| LIB-F3 | Playlists (user-curated, ordered) CRUD + browse | L | C1 | ✓ |
| LIB-F4 | Smart playlists (rule-based, saved queries over `MediaQuery`) | L | B1,F3 | partial |
| LIB-F5 | Multi-version / edition picker (full surface in pharos-ui) | M | C* | partial |
| LIB-F6 | Parental controls (official-rating gate, per-user max rating) | M | C7 | ✓ |
| LIB-F7 | Per-user library access / hidden libraries | M | C1 | ✓ |
| LIB-F8 | People/genre/studio/collection browsing views in pharos-ui | L | C2–C5 | n/a |
| LIB-F9 | Advanced intro/credit detection (audio fingerprint / silence) beyond chapter heuristic | L | — | ✓ |

### EPIC G — Subtitle correctness & sync

Two failure classes: **missing** subtitles (discovery too narrow) and **out-of-sync**
subtitles. The systemic sync rule: a subtitle's timeline must derive from the **same
media-source timeline as the delivered video** (single PTS source of truth) — never
extracted independently from t=0 and served against a seeked/PTS-rebased video.

| ID | Feature | Effort | Deps | Proj |
|----|---------|--------|------|------|
| LIB-G1 | Robust sidecar discovery: `Subs/`/`Subtitles/` subfolders, language-named + multi-token files (`Movie.en.forced.sdh.srt`), `.ass/.ssa`, correct video↔sidecar basename matching | M | — | ✓ |
| LIB-G2 | **HLS/transcode subtitle timeline alignment** — honor container `start_time`/first-PTS and the transcode seek offset so cues match the delivered video PTS; deliver segmented WebVTT aligned to the HLS media sequence | L | — | ✓ |
| LIB-G3 | Honor container `start_time`/first-PTS on embedded-subtitle extraction (direct-play path) | M | — | ✓ |
| LIB-G4 | Persisted per-`(user,item,stream)` subtitle delay/offset + pharos-ui sync slider; project to the Jellyfin client where representable | M | — | partial |
| LIB-G5 | Import-time sync diagnostics: first-cue, last-cue-vs-duration, `start_time` delta → flag suspect tracks; auto-detect fps/PAL mismatch (23.976↔25) + offer retime | M | A1 | ✓ |
| LIB-G6 | ASS/SSA: extract with styling preserved or burn-in option (today styling is dropped on WebVTT conversion) | M | — | ✓ |
| LIB-G7 | Forced/SDH correctness end-to-end + default-track selection honoring user language preference | S | — | ✓ |

---

## Notable design notes

### Same-name series disambiguation (fixes the *Cosmos* interleave bug — LIB-C11)
Jellyfin keys a series on its display name, so *Cosmos (1980, Carl Sagan)* and
*Cosmos (2014, deGrasse Tyson)* collapse into one show with interleaved episodes —
and pharos has the same flaw today (`series_id_for(series_name)` hashes the name).
The canonical `series` entity is keyed on the **series folder path** (the directory
containing the `Season NN`/episode files), with `(name, year, provider_ids)` as
disambiguating/display attributes. Episodes group by their series *folder*, never by
bare name. Identity precedence: provider-id → folder path → name+year. The
folder-keyed grouping is a small, high-value change that can land in the scanner's
`parse_series_info` early, ahead of the full entity tables.

### Tiered change detection (NFS/SMB don't support watching — LIB-A9)
FS watching is not universally available: SMB/NFS network shares (a common pharos
deployment) don't deliver inotify/FSEvents, and watcher init can fail at runtime.
Each library root independently selects the best mode it can sustain:
1. **Native watch** (`notify`) — local FS, watcher initialises.
2. **Periodic incremental rescan** — fallback when watching is unavailable (probe
   `/proc/mounts` / `statfs` f_type for nfs/cifs/smb/fuse, or catch a watcher-init
   error). Cheap because the incremental scan only re-probes changed files.
3. **Manual** — `/Library/Refresh` (interval = 0) as the floor.
Detected at startup, logged per root, auto-downgraded if a watcher later errors.

### Subtitle sync (LIB-G2/G3)
The out-of-sync class is largely our own: when we transcode/seek, the video PTS is
rebased but the subtitle is extracted independently from t=0. Fix by deriving the
subtitle timeline from the same media-source timeline as the delivered video —
honor container `start_time`/first-PTS and the transcode seek offset, and for HLS
deliver segmented WebVTT aligned to the media sequence.

---

## Verification (per epic)

- **A**: probe-counting `MockProber` — second scan probes 0 unchanged / N changed /
  sweeps deleted; move test (`mv` a played file → same id + `user_data` preserved +
  no orphan); watcher test (feature-gated); **fallback test** (force non-watchable
  root → periodic-incremental picks up new files). Bench: incremental rescan is
  stat-only / O(changed).
- **B**: 20k synthetic rows — `query` pagination + `TotalRecordCount` + ORDER parity;
  byte-identical golden `/Items` snapshot before/after the SQL switch; FTS ≥ old
  substring matches.
- **C**: migration backfill (one library per root); entity round-trips; `/Persons`
  `/Studios` `/Collections` pass `pharos-jellyfin-test-client`. **C11 regression**:
  `Cosmos (1980)/` + `Cosmos (2014)/` each with `S01E01` → two distinct series ids,
  no interleave, per-series year.
- **D**: parse a real Kodi `movie.nfo`; sidecar tmpdir → `ArtworkRef`s; resolver
  merge precedence; end-to-end `.mkv + .nfo + poster.jpg` ⇒ DTO carries
  overview/artwork/people; malformed NFO never aborts a scan.
- **E**: each provider mocked (wiremock) → mapping + rate-limit + cache-hit;
  providers-disabled ⇒ zero outbound requests; enabled-but-offline ⇒ scan completes
  on local data.
- **F**: `/Similar` overlap; playlist CRUD; parental gate hides over-rated items.
- **G**: non-zero `start_time` + transcode seek offset → cue at *t* aligns with the
  frame at *t* (no drift); `Subs/` + `Movie.en.forced.srt` discovered + matched; a
  PAL-shifted SRT flagged; a persisted per-user offset survives reconnect.
- **Global**: `just test` + `just lint` + `just hakari-check` + `crate2nix generate`
  green each phase; the Jellyfin projection stays wire-compatible (jellyfin-web
  playwright suite green).

## Critical files

- `crates/pharos-core/src/lib.rs` — canonical superset structs + IO-free traits.
- `crates/pharos-scanner/src/fs.rs`, new `src/{metadata/*,watcher.rs}` — import.
- `crates/pharos-store-sqlx/migrations/sqlite/0016..0021_*.sql`, `src/{sqlite,postgres}.rs`.
- `crates/pharos-server/src/api/jellyfin/{items,search,images,admin,subtitles}.rs` — projection.
- `crates/pharos-jellyfin-api/src/dto.rs` — lossy Jellyfin projection + synth-ids.
