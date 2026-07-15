# BACKLOG — pharos open work

Snapshot 2026-07-15. Working handoff for a fresh context. SPEC.md §T/§B is the
canonical tracker; this file surfaces the *in-flight* + *recurring* items that
are easy to lose across context resets. Cross-refs SPEC where a §T row already
exists.

Live now: `main-1784114711-ed0b88e2d496` (B71). Single replica, 1/1.

---

## P0 — in-flight, must close soon

### 1. Confirm B71 fixed native-TV playback, then REVERT debug logging
- B71 (`ed0b88e`) is deployed. It made sidecar subtitle stream indices
  contiguous (`sidecar_base_index(probe)`) instead of the `1_000_000` sentinel
  that crashed the positional kotlin player on PlaybackInfo. **Not yet
  confirmed on-device** — user must press Play on the Android TV.
- Debug request-logging is STILL ON in prod. Turn OFF once playback confirmed:
  - `charts/pharos/values.yaml:106` → `logAllRequests: false`
  - `crates/pharos-server/src/api/jellyfin/items.rs:~2213` — remove the B70
    PlaybackInfo-response-body log block (`PHAROS_LOG_ALL_REQUESTS == "1"`).
  - bump `charts/pharos/Chart.yaml` version (Flux reconcileStrategy=ChartVersion).
  - obs.rs `log_all_requests()` gate can stay (harmless, opt-in).
- This is the last step of the QC/kotlin crash cascade (B61–B71). Only turn it
  off AFTER the user confirms a clean play; the body log is the diagnostic.

---

## P1 — recurring bug classes (the ones that keep biting)

### 2. json! → typed DTO sweep  (T94 — not yet in SPEC §T; add it)
- **133** `json!` calls remain in `crates/pharos-server/src/`. Only ~3 converted
  (commit `8457d1a` added `wire::json()` + `SimdJson<T>` extractor).
- Worst offenders: `items.rs` (62), `system.rs` (16), `socket.rs` (13),
  `live_tv.rs` (9), `admin.rs` (8), `stubs.rs` (6).
- Why it matters: every ad-hoc `json!` is a place a kotlin-required field can be
  omitted / mistyped with no compile check → native-client crash (B13/B14
  class). Typed DTOs serialized via `wire::json()` make the shape checkable.
- Rule: new response bodies use a `#[derive(Serialize)]` DTO in
  `pharos-jellyfin-api/src/dto.rs` + `wire::json()`; inbound bodies use
  `SimdJson<T>`. sonic-rs (SIMD) is the serializer both directions.

### 3. camelCase query-param binding  (B11/B13 open note — the recurring one)
- **27** `web::Query` structs still `rename_all = "PascalCase"` across
  users/system/item_ops/syncplay/search/sessions/admin/items.rs.
- kotlin/native SDKs send **camelCase** query params; ASP.NET binds
  case-insensitively so real Jellyfin accepts both. pharos's PascalCase-only
  binding SILENTLY ignores them (default value) or 400s (required) → native
  browse filters/paging/sort wrong, no error logged.
- Fixed so far only where it broke a specific flow (QC `secret`/`code` via
  `query_param_ci`). Systemic fix wanted: a case-insensitive `web::Query`
  deserialize (custom deserializer or a CI wrapper) applied to the whole
  Jellyfin surface, with a guard test per struct family.
- This is the thread behind "continue to fix the dtos based off our kotlin
  audit". Method: for each struct, diff pharos fields vs the kotlin request
  model (`gh api .../jellyfin-sdk-kotlin/.../<Name>.kt`), add missing
  aliases/fields, make binding case-insensitive.

### 4. Type-system: make invalid states unrepresentable  (SPEC T88)
- Ongoing per user mandate. Done: WireId, Segment* newtypes, SegmentKey,
  sidecar index contiguity (B71). Still open in T88:
  - (a) migrate handler sigs off raw id strings onto `WireId` (Real/Synth arms);
    42 `parse_item_id` sites conform by convention only.
  - (b) `Ticks(u64)`/`Seconds(f64)`/`Millis(u64)` newtypes — `TICKS_PER_SECOND`
    duplicated 6×; `position_ms` sits beside tick-valued u64s.
  - (c) `PlaySessionId` + `DeviceId` newtypes; unify wire `group_id: String`
    with pharos-sync `GroupId(Uuid)`.
  - (d) `ItemType` + `SortField` enums — two duplicated "Movie"/"Episode" match
    tables (search.rs, items.rs); raw sort-field strings = typo silently
    no-matches.

---

## P2 — Jellyfin parity backlog (already in SPEC §T, listed for visibility)

Open `.` tasks in SPEC §T, grouped:

- **Metadata/policy/library**: T67 (People/Studios/Tags on list responses +
  ExternalUrls/RemoteTrailers/ProductionLocations + MetadataEditor), T68
  (full UserPolicy field set + ENFORCE disable/EnabledFolders/MaxParentalRating),
  T69 (LibraryOptions/VirtualFolders CRUD + DirectoryContents picker).
- **Controllers/stubs**: T70 (/Playlists CRUD), T72 (named-configuration
  persistence — currently read-only toml no-ops), T73 (activity log), T74
  (scheduled tasks), T75 (plugin/package install), T76 (item ops:
  MergeVersions/ContentType/RemoteImages/RemoteSearch/lyrics/InstantMix).
- **Artwork**: T78 (art-aware audio image tags — cosmetic 404 churn), T79
  (cast/person PrimaryImageTag), T81 (real person images via TMDB at scan time).
- **Media segments / skip intro**: T86 (detect intro/outro spans +
  /MediaSegments/{itemId} typed segments → jellyfin-web native Skip buttons).
- **SyncPlay deferred** (from 2026-07-14 deep audit — single-replica steady
  state, so low unless multi-replica or native client): T84 (next-episode
  real-client APPLY verification gap + NextItem-during-load reconciliation),
  T89 (/SyncPlay/Queue + Move/Remove handlers — 404s silently today), T90
  (join fidelity: phantom-group on stale GroupId; real group_name/state in
  GroupJoined), T91 (multi-replica hardening: advisory-lock heartbeat, NOTIFY
  oversize fallback, SIGTERM persist drain, sync_recovery ORDER BY), T92
  (native /sync/v1/ws Leader* gate parity).
- **Test infra**: T80 (Postgres test harness — store trait suite vs real PG in
  CI; class guard for B19 pg-only divergences), T85 (trickplay sweep
  multi-replica dedup via bg-leader election).
- **Plex surface / far future**: T11–T14 (plex-api), T20.

---

## Notes / process reminders
- Deploy: push main → CI (self-hosted ARC runner) → GHCR → Flux (~15-20 min).
  `kubectl --context default -n pharos`. flux CLI unusable (API skew); Flux
  auto-rolls.
- After any Cargo.toml dep change: `just hakari-regen`.
- Always `just test` (full workspace) before commit — targeted runs missed
  codified-buggy-value tests twice (B64, B69).
- kotlin-required field check:
  `gh api "repos/jellyfin/jellyfin-sdk-kotlin/contents/jellyfin-model/src/commonMain/kotlin-generated/org/jellyfin/sdk/model/api/<Name>.kt" --jq '.content' | base64 -d | grep "public val"`
  (required = no `?` and no `= default`).
- obs.rs logs only error statuses by default; 2xx invisible unless
  `PHAROS_LOG_ALL_REQUESTS=1` (temporarily on — see P0.1).
