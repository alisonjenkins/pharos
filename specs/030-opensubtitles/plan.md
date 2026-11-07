# Implementation plan: OpenSubtitles subtitle search and automatic download (030)

**Spec**: [spec.md](spec.md) · **ADR**: ADR-0069 · **Created**: 2026-11-07T00:00:00Z

## Technical context

- Language and stack: Rust, actix-web, `reqwest` (already a `pharos-server` dependency, used by `tmdb.rs`), serde. No new third-party dependency, so no `just hakari-regen`.
- No live network in tests. The provider sits behind a trait; handler and job tests use a fake. The HTTP client is tested against a local `actix-web` responder bound to `127.0.0.1:0`, with the base URL injectable.

## Constitution check

- TDD: each task starts with a failing test.
- ODD: `pharos_opensubtitles_total{op,outcome}` with stable labels from a `label()` method, asserted distinct, with a disarm check (V571).
- V17: no `unwrap`/`expect` outside tests.
- Typed Jellyfin wire DTO (`#[derive(Serialize)]` plus `wire::json()`), as for `LyricDto`.

## Module layout

| Where | What |
|---|---|
| `pharos-server/src/opensubtitles/mod.rs` | `SubtitleProvider` trait (boxed futures, object-safe so `AppState` holds `Arc<dyn SubtitleProvider>`), `SearchQuery`, `SubtitleHit`, `Downloaded`, `ProviderError { QuotaExceeded { reset }, RateLimited { retry_after }, Auth, Http { status }, Transport, Parse }` |
| `.../opensubtitles/hash.rs` | `moviehash(path) -> io::Result<Option<String>>` and the pure `moviehash_from_parts(size, head, tail)` |
| `.../opensubtitles/client.rs` | `OpenSubtitlesClient` (reqwest, `Api-Key`, `User-Agent`, lazy login, cached JWT, pacing, `base_url` override), wire structs for `/login`, `/subtitles`, `/download`, and the `/subtitles` query builder |
| `.../opensubtitles/store.rs` | `subtitle_dir(root, item_id)`, `save(root, item_id, lang, file_id, bytes)`; the only writer, always under the root |
| `.../opensubtitles/quota.rs` | `QuotaGate`: in-memory "no downloads until" instant, set from a 406/429 |
| `.../opensubtitles/job.rs` (slice 2) | the background job, modelled on `audio_analysis.rs` |
| `.../opensubtitles/memo.rs` (slice 2) | miss memo, modelled on `pharos_scanner::probe_memo` |
| `config.rs` | `[opensubtitles]` (`api_key`, `username`, `password`, `languages`, `max_downloads_per_day`) and the `PHAROS_OPENSUBTITLES_*` env overrides |
| `state.rs` | `AppState.opensubtitles: Option<Arc<OpenSubtitles>>` where `OpenSubtitles { provider, root, gate }` |
| `api/jellyfin/subtitles.rs` | `discover_sidecars_uncached` also scans the store directory; the cache key includes that directory's mtime |
| `api/jellyfin/item_ops.rs` | `remote_subtitle_search` and the new `remote_subtitle_download` |
| `pharos-jellyfin-api/src/dto.rs` | `RemoteSubtitleInfoDto` |

## Data flow

Search: item -> `SearchQuery { hash, imdb, tmdb, parent ids, season, episode, language }` -> provider -> hits sorted (hash match, trusted, download count) -> `RemoteSubtitleInfoDto[]` with `Id` = the file id.

Download: `Id` -> provider `download(file_id)` (quota gate checked first) -> `store::save` -> the sidecar cache for the item's media path is invalidated by the directory mtime -> 204.

Playback: `discover_sidecars(media_path)` -> sidecars beside the media, then the store directory for `stable_id(media_path)` -> External streams. Index order is path order within the list, as today.

## Slices

1. Hand search and download (T030-01..07): V562-V566, V570, V571 (search/download ops).
2. Background job (T030-08..10): V567 (job side), V568, V569, V571 (job ops).
