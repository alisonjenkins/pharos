# Jellyfin → pharos architecture mapping

Goal: extract Jellyfin's load-bearing patterns and translate to idiomatic Rust. Drives trait shape under V12 (domain logic testable without IO). Reference doc for all subsequent T-tasks that touch domain modeling.

## 1. Entity model

| Jellyfin (C#) | pharos (Rust) | Crate | Notes |
|---|---|---|---|
| `BaseItem` (abstract base, ~30 derived types) | `MediaItem` (sum type via enum) | `pharos-core` | Flat sum type, not OO hierarchy. Variants: `Movie`, `Episode`, `Audio`, `Photo`, `Book` (Phase 2). |
| `User` | `User` struct | `pharos-core` | id, name, password_hash, policy. |
| `UserPolicy` | `UserPolicy` struct | `pharos-core` | Access flags, library permissions. Plain data. |
| `Session` (`SessionInfo`) | `Session` (owned by actor) | `pharos-server` | Mutated via mpsc. V18 mandates no `Mutex<Session>`. |
| `Device` | `Device` struct | `pharos-core` | Identified by `device_id` from client hello. |
| `MediaSource` | `MediaSource` struct | `pharos-core` | One file → one source. Carries container, codecs, bitrate. |
| `MediaStream` | `MediaStream` enum | `pharos-core` | `Video`, `Audio`, `Subtitle` variants. |
| `Library` / `CollectionFolder` | `Library` struct | `pharos-core` | Roots + kind + scanner config. |
| `PlaylistItem` / `Playlist` | `Playlist` struct | `pharos-core` | Ordered list of `MediaId`. |
| `SyncPlayGroup` | group actor (`GroupState`) | `pharos-sync` | See §5; durability + multi-replica in ADR-0016. |

Rule: every entity is `Debug + Clone + Send + Sync + Serialize + Deserialize` unless a comment justifies otherwise.

## 2. Provider interfaces → traits

Jellyfin uses C# interfaces injected via Autofac. We use Rust traits with native async fn (per `feedback_async_traits` memory) and compose impls at startup.

| Jellyfin interface | pharos trait | Lives in | Status |
|---|---|---|---|
| `IMetadataProvider<T>` | `MetadataProvider` | `pharos-core` | Pending T6. One impl per source (TMDb, TVDB, MusicBrainz). |
| `IRemoteImageProvider` | `ImageProvider` | `pharos-core` | Pending. |
| `IItemRepository` | `MediaStore` | `pharos-core` ✓ | Implemented in `pharos-store-sqlx`. |
| `ILibraryManager` | `LibraryService` (struct, not trait — single impl) | `pharos-server` | Wraps `MediaStore` + `Scanner`. |
| `IMediaSourceProvider` | `MediaSourceResolver` trait | `pharos-core` | Pending T7. Maps `MediaItem` → playable `MediaSource`s. |
| `IAuthenticationProvider` | `AuthBackend` trait | `pharos-core` | Pending T4. Variants: builtin (Argon2 over local store), future LDAP/OIDC. |
| `IMediaEncoder` | `Transcoder` trait | `pharos-core` | Implemented in `pharos-transcode`: libav worker pool for tiny ops + spawn ffmpeg for segments (ADR-0004). |
| `ISessionManager` | `SessionActor` | `pharos-server` | Actor-only. No trait — one impl. |
| `IDeviceManager` | `DeviceRegistry` | `pharos-server` | Actor-only. |
| `IUserDataManager` | embedded in `MediaStore` | — | Watched/resume position kept in same store under `user_data` table. |
| `IPluginManager` | n/a | — | Replaced by cargo features + compile-time composition. See §4. |

Pattern: trait when ≥2 impls are realistic (auth backends, metadata providers, transcoders). Struct when one impl serves. Avoid premature trait abstraction.

## 3. HTTP surface mapping (informs T5–T7, T11–T14)

Jellyfin organizes controllers under `Jellyfin.Api.Controllers`. Each maps to a `actix_web::Scope` in pharos.

| Jellyfin controller | actix scope | Tasks |
|---|---|---|
| `SystemController` | `/System` | T5 |
| `UserController` | `/Users` | T5 |
| `LibraryController`, `ItemsController` | `/Library`, `/Items` | T6 |
| `VideosController`, `AudioController` | `/Videos`, `/Audio` | T7, T9 |
| `SessionsController`, `PlaystateController` | `/Sessions`, `/PlayState` | T10 |
| `SyncPlayController` | `/SyncPlay` | T17 |
| `ImageController` | `/Items/{id}/Images/*` | T6 (subset) |
| `BrandingController` | `/Branding` | T6 |
| `ConfigurationController` | `/Configuration` | post-parity |

Each scope mounts as `App::new().service(web::scope("/Users").configure(users::routes))`. Module per scope under `pharos-server/src/api/jellyfin/`.

## 4. Plugin model → cargo composition

Jellyfin loads `.dll` plugins at runtime via reflection. pharos cannot — Rust has no stable ABI. Adaptation:

- Each "plugin" is a cargo crate exposing one or more trait impls.
- `pharos-server` declares `Vec<Box<dyn MetadataProvider>>` (or generic equivalent) at startup, populated from config + compile-time crate set.
- Optional features select which crates are built in. Example: `cargo build --features tmdb,musicbrainz`.
- Loss vs Jellyfin: end-users can't drop a `.dll` to extend. Trade-off accepted for type safety + perf. Document as known limitation.

Future: WASM plugin host could restore dynamic extension. Defer past Phase 2.

## 5. SyncPlay → group-sync actor (informs T15–T17)

Jellyfin `SyncPlayController` synchronizes playback across clients in a group. Algorithm:

1. Each client maintains a tick clock synced via `Ping` exchange (median offset from N samples).
2. Leader emits `Play(at: ticks, position: ms)` / `Pause(at: ticks)` / `Seek(...)` commands.
3. Followers schedule local action at `at`, correcting drift via periodic `BufferingReady` / `Resume` exchanges.

pharos translation:

- One tokio task per active group (`SyncSession` actor). Owns members, leader, schedule.
- Inbound: `mpsc::Sender<SyncMsg>` per group. Members send via WebSocket → handler forwards.
- Outbound: per-member WebSocket sink. Actor pushes commands.
- V3 invariant: 500ms p95 sync. Achieved by:
  - Server timestamps every outbound command with monotonic clock.
  - Clients compute offset via repeated `Ping`/`Pong`.
  - Commands include absolute server-clock `at`.
- V18 invariant: no `Mutex<SyncState>`. State lives inside the actor, mutated only by actor's run loop.

## 6. Idiomatic-Rust divergences worth noting

- **No inheritance**: `BaseItem` hierarchy collapses to enums + dispatch via match. Sum types let exhaustiveness checks catch missing branches at compile time (Jellyfin discovers at runtime).
- **No DI container**: dependencies wired at `main` via explicit constructors. Encourages small graphs.
- **No `null`**: `Option<T>` everywhere C# would have nullable refs. Jellyfin's `BaseItem.Path == null` cases become explicit `Option<PathBuf>`.
- **Errors are values**: every IO method returns `Result<_, _>`. No try/catch sprawl.
- **Tasks > threads**: every long-running concern is a tokio task with mpsc inbox. Replaces Jellyfin's `BackgroundService` + `IServerEntryPoint`.
- **Compile-time feature gating**: replaces runtime feature flags for build-shape decisions (backends, optional protocols).
- **String-typed IDs → newtypes**: `MediaId(u64)`, `UserId(Uuid)`, etc. — prevents arg-order bugs.

## 7. Open questions (tracked, not blocking)

- Image cache layout: Jellyfin uses content-addressed sha256 paths. pharos likely same. Confirm at T6.
- Subtitle extraction: Jellyfin runs ffmpeg per request. pharos can cache. Decide at T7/T9.
- Transcode profile selection: Jellyfin uses XML device profiles. pharos may simplify to TOML. Decide at T8.
- Live TV / DLNA / Books: explicit non-goals for Phase 1. Revisit T20.

## References

- jellyfin/jellyfin (C# server) — https://github.com/jellyfin/jellyfin
- jellyfin OpenAPI spec — https://api.jellyfin.org/
- jellyfin SyncPlay docs — https://jellyfin.org/docs/general/clients/syncplay/
