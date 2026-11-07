# ADR-0069: OpenSubtitles downloads go to the cache volume, behind a provider trait

- **Status:** Accepted
- **Date:** 2026-11-07T00:00:00Z
- **Deciders:** Ali

## Context

Jellyfin's "Search for subtitles" dialog calls `GET /Items/{id}/RemoteSearch/Subtitles/{lang}` and `POST /Items/{id}/RemoteSearch/Subtitles/{subtitleId}`. Pharos answers `[]` to the first and has no route for the second (T76). The libraries may be read-only NFS, so Jellyfin's habit of saving the file beside the media cannot be copied. Pharos finds external subtitles by scanning the media directory (`discover_sidecars`), and the stream index of an external track is its position in that sorted list, so a track stored elsewhere has to join the same list or the indexes drift between PlaybackInfo and `Stream.vtt`.

OpenSubtitles.com offers a REST API v1 (`api.opensubtitles.com/api/v1`) with search by file hash and by IMDb/TMDB id, a `/download` that returns a short-lived link, and a JWT login. Requests need an `Api-Key` and a descriptive `User-Agent`; the rate is 5 requests per second per IP; download quotas are per 24 h by account level and exhaustion answers 406 (details and sources in spec 030).

## Decision

- We use the OpenSubtitles.com REST API v1, with `reqwest` already in `pharos-server`. The code is a module `pharos-server/src/opensubtitles/`, not a new crate: it has one consumer and no dependency of its own.
- The client sits behind an object-safe `SubtitleProvider` trait so handlers and the job are tested with a fake. No test touches the network.
- The feature is off unless `[opensubtitles].api_key` (or `PHAROS_OPENSUBTITLES_API_KEY`) is set. `username` and `password` are optional and raise the quota. They follow the TMDB/TVDB secret rule: env var first, never in a committed config, never logged.
- Downloaded subtitles are stored at `<subtitle cache dir>/opensubtitles/<media id>/os<file id>.<lang>.srt` (`<media id>` is the hex `stable_id` of the media path) and never beside the media. With no subtitle cache directory the feature stays off.
- `discover_sidecars` also lists that directory, sorted by path with the others, so a downloaded track is an External stream with a stable index. The sidecar cache key includes that directory's mtime. The root is a process-wide setting installed at startup (`OnceLock`), because `discover_sidecars` has a dozen callers that hold only a media path.
- The Jellyfin `RemoteSubtitleInfo.Id` is `<file id>-<language>`, since the download POST carries nothing else and the stored file needs its language.
- Slice 2 adds a bounded background job (one item at a time, a daily ceiling below the account quota, a persisted miss memo beside the cache like `probe_memo`, stop for the day on 406/429).
- The quota is never hard-coded. A 406 on `/download` sets an in-memory gate until the reported reset time; a 429 is reported with its `Retry-After`.

## Consequences

- Read-only libraries work. Subtitles live and die with the cache volume, so losing the volume loses the downloads (and the job fetches them again, quota permitting).
- A global in a library function is a wart, accepted to avoid threading state through a dozen callers; a later change can pass the root explicitly.
- Search costs one API request and a hash read of 128 KiB (two seeks) over NFS, run on the admin's request.
- The operator must create an OpenSubtitles API key (a consumer app) and put it in the cluster secret.

## Alternatives considered

- Save beside the media like Jellyfin: fails on read-only NFS.
- Store in the database: subtitles are files the existing sidecar path already serves; a table would need a store implementation per backend for no gain.
- A separate `pharos-opensubtitles` crate: no second consumer; extract it when there is one.
- The legacy XML-RPC / `.org` API: deprecated, and its 200 per day cap is gone.

## References

- `specs/030-opensubtitles/`
- OpenSubtitles REST API docs: <https://opensubtitles.stoplight.io/docs/opensubtitles-api>
- `jellyfin/jellyfin-plugin-opensubtitles` (request shapes, the `file_id` as subtitle id)
