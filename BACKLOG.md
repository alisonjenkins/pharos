# BACKLOG — pharos open work

Snapshot 2026-07-15. Working handoff for a fresh context. SPEC.md §T/§B is the
canonical tracker; this file surfaces the *in-flight* + *recurring* items that
are easy to lose across context resets. Cross-refs SPEC where a §T row already
exists.

Live now: `main-1784121423-7097d22a2735` (B73). Single replica, 1/1.
Pending deploy: B75 + bit-depth + logDir (commits `685ddd9`, `9d91fb5`, `5e4ef45`).

---

## P0 — in-flight, must close soon

### 0. Native-TV playback — FIXED via B75 (pending deploy + on-device confirm)
- FULL root-cause chain (adb logcat on the TCL "Smart TV Pro", Android 12,
  jellyfin-android-tv 0.19.9):
  - The "crash on seek" was **not** a crash: the app is **SIGKILL'd (signal 9)**
    by TCL's `TGuardMemoryManager` under memory pressure (2.4 GB TV), then
    relaunched + killed again = the "3 crashes". No Java exception, no segfault.
  - Amplifier was B73 itself: it force-transcoded a source the TV can DIRECT-PLAY
    (HEVC 1080p, "Alien", 16 Mbps + 55 external sub tracks). The needless
    HEVC→H.264 transcode + per-seek HLS teardown/realloc tipped the tight RAM
    over → OOM kill.
- Why B73 existed: native apps send the `/videos/{id}/stream?static=true` request
  with ZERO credentials — no header, no cookie, no api_key (B72 audit + reading
  jellyfin-sdk-kotlin: the ExoPlayer OkHttp data-source has no auth interceptor;
  `getVideoStreamUrl` never adds api_key). Real Jellyfin's stream route is
  anonymous (random-GUID ids); pharos ids are low-entropy so anonymous = enumerable.
- **B75 fix** (`9d91fb5`): authenticate the native stream via a capability token
  instead of forcing a transcode. `getVideoStreamUrl(tag = mediaSource.eTag)`
  forwards ETag verbatim as `?tag=`, so pharos stamps `ETag = PlaySessionId`
  (random uuid, registered in transcode_sessions against the media id, direct-play
  sessions now registered too). `stream::authorize_media` accepts a normal token
  OR a `tag`/`PlaySessionId` bound to the requested item — unguessable,
  single-item, time-limited. B73 override DELETED → trust `negotiate()`.
  Guard: tests/jellyfin_playbackinfo_native_directplay.rs (native keeps DirectPlay;
  tokenless `?tag=` authorizes end-to-end; wrong tag rejected).
- **Bit-depth hardening** (`685ddd9`): `negotiate()` now enforces the
  `VideoBitDepth` CodecProfile condition (was silently permissive) so an
  8-bit-only decoder transcodes a 10-bit source instead of direct-playing garbage.
- ON-DEVICE CONFIRM STILL NEEDED once deployed: play the "Alien"-style HEVC item,
  seek → must NOT crash (now direct-plays, in-place seek, zero transcode).

### 0a. FOLLOW-UP — sub-track sideload memory on tight TVs (parity gap, not a bug)
- Even with direct-play, jellyfin-android-tv `VideoManager.setMediaStreamInfo`
  attaches EVERY `DeliveryMethod=External` subtitle (55 for "Alien") into one
  ExoPlayer MergingMediaSource **upfront**. Real Jellyfin does the same (no cap),
  so this is upstream-parity, but it's real RAM on a 2.4 GB TV. If OOM persists
  after B75, consider capping advertised External subs for memory-constrained
  native clients (diverges from Jellyfin — do only if proven necessary).

### 1. REVERT debug logging (once B75 confirmed on-device)
- Debug request-logging STILL ON in prod. Turn OFF once playback confirmed:
  - `charts/pharos/values.yaml` obs.logAllRequests → `false`
  - `crates/pharos-server/src/api/jellyfin/items.rs:~2213` — remove the B70
    PlaybackInfo-response-body log block (`PHAROS_LOG_ALL_REQUESTS == "1"`).
  - B72 header-audit block in `auth_extractor.rs` can also go now (root cause found).
  - bump `charts/pharos/Chart.yaml` version (Flux reconcileStrategy=ChartVersion).
  - obs.rs `log_all_requests()` gate can stay (harmless, opt-in).
- Only turn off AFTER the user confirms a clean play + seek.
- NOTE: `logDir` now set to the cache PVC (`5e4ef45`) so client-log / manual
  "Submit logs" uploads persist — but android-tv crashes go to tracepot (external
  ACRA), never the server, so use adb logcat for native crashes.

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
