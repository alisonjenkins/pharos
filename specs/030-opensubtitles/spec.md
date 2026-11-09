# Feature specification: OpenSubtitles subtitle search and automatic download

**Feature Branch**: `feat/opensubtitles-provider`
**Created**: 2026-11-07T00:00:00Z
**Status**: Implemented — slices 1 and 2 landed (#925); T030-01..10 all done (audited 2026-11-09)
**Scope**: Search OpenSubtitles.com for subtitles from the Jellyfin "Search for subtitles" dialog, download the chosen one to the cache volume, and (slice 2) fetch missing subtitles in the background after a scan.
**Decision record**: ADR-0069. **Task**: T76 (the two RemoteSearch endpoints, today stubs). **Verification**: V562-V571.
**Depends on**: `SubtitleCache` and the cache PVC conventions (`[server].subtitle_cache_dir`), `discover_sidecars` (external subtitle streams), the TMDB/TVDB secret handling in `config.rs`.
**Audit rows closed when slice 1 lands**: `docs/jellyfin-parity-audit.md` (RemoteSearch/Subtitles), `docs/jellyfin-web-feature-matrix.md`, `docs/ui-gap-assessment.md` D18, `docs/library-backlog.md` LIB-E8.

---

## Problem statement

`GET /Items/{id}/RemoteSearch/Subtitles/{lang}` returns `[]` and `POST /Items/{id}/RemoteSearch/Subtitles/{subtitleId}` does not exist. A viewer whose file has no subtitle in their language can open Jellyfin's "Search for subtitles" dialog and finds nothing. Nobody downloads subtitles for them in the background either, so a new episode with no usable track stays that way until someone fetches a file by hand and drops it beside the media. The library may be read-only NFS, so "beside the media" is not even an option for a server-side download.

## OpenSubtitles.com API facts this spec relies on

Sources: OpenSubtitles REST API docs (<https://opensubtitles.stoplight.io/docs/opensubtitles-api>). Those pages are rendered client-side and could not be fetched whole, so the figures below were cross-checked against the OpenSubtitles forum, the vendor's help centre and the Jellyfin OpenSubtitles plugin source (`jellyfin/jellyfin-plugin-opensubtitles`). Figures marked (reported) are second-hand. The client never hard-codes quotas: it reads the live values from responses.

| Fact | Value | Source |
|---|---|---|
| Base URL | `https://api.opensubtitles.com/api/v1`; after `/login` the response carries a `base_url` host to use for later calls (VIP accounts) | API docs; Jellyfin plugin |
| Every request | `Api-Key: <key>` and a descriptive `User-Agent: <app> v<version>` (a library default is refused with 403) | API docs "Best practices"; forum |
| User calls | `Authorization: Bearer <JWT>` from `POST /login {username,password}`; the token is valid 24 h; login is limited to 1 request per second per IP | API docs; forum |
| Request rate | 5 requests per second per IP (50 per second on paid plans); excess answers `429`, sometimes with `Retry-After` | API docs |
| Search | `GET /subtitles?moviehash=&imdb_id=&tmdb_id=&parent_imdb_id=&parent_tmdb_id=&season_number=&episode_number=&languages=&type=` (ids numeric, no `tt` prefix) | API docs |
| Result | `data[].attributes`: `subtitle_id`, `language`, `release`, `download_count`, `ratings`, `from_trusted`, `hearing_impaired`, `ai_translated`, `machine_translated`, `foreign_parts_only`, `fps`, `uploader.name`, `upload_date`, `files[].file_id`, `moviehash_match` | API docs |
| Download | `POST /download {file_id, sub_format:"srt"}` with a bearer token returns `{link, file_name, requests, remaining, message, reset_time, reset_time_utc}`; `link` is a short-lived URL (3 h at most; do not store it) | API docs; Jellyfin plugin |
| Download quota | Per 24 h and per account level, from single digits for a key alone, to low tens for a free account (reported: help centre "from 10 for regular users to 1000 for VIP"), to 1000 for VIP. Exhaustion answers `406`, and the body gives `remaining` and `reset_time_utc` | help centre; API docs; Jellyfin plugin issue #28 |
| File-host limits | The temporary download link is served by a caching layer with its own limit (reported `x-ratelimit-limit-second: 6`, `-minute: 240`), separate from the API plan | forum thread "Download rate limit?" |

## User story 1 - Search and download a subtitle by hand (Priority: P1)

An admin opens an item's menu, picks "Search for subtitles", chooses English, and sees results ranked with hash matches first. They pick one. The track appears in the subtitle menu of the item, marked External, without any change in the media folder.

**Independent test**: handler tests with a fake provider (`SubtitleProvider`). Search returns a typed `RemoteSubtitleInfo[]`; POSTing a result's `Id` stores the subtitle under the cache directory and the item's PlaybackInfo then lists one more External subtitle stream with that language.

**Acceptance scenarios**:

1. **Given** an API key is configured and the item's file is at least 128 KiB, **When** the search is made, **Then** the provider is asked with the file's OpenSubtitles hash, the item's IMDb or TMDB id (parent ids and season/episode for an episode) and the requested language, and results with a hash match sort first (V562, V566).
2. **Given** no API key is configured, **When** either endpoint is called, **Then** search returns `[]` with 200 and download answers 400, and no network request is made (V563).
3. **Given** a result is downloaded, **Then** the file is written under the subtitle cache directory, never beside the media (V564), and appears as an External subtitle stream with the right language in `MediaSources` (V565).
4. **Given** the provider answers 406 or 429 to a download, **Then** the endpoint answers 429 with a message naming the reset time, and no further download is attempted until then (V567).

## User story 2 - Missing subtitles arrive on their own (Priority: P2, slice 2)

After a scan, for each item with no subtitle (embedded or external) in any configured language, a background job asks OpenSubtitles for the best hash-matched or trusted result, downloads it within the day's quota, and stores it. A miss is remembered so the next pass does not ask again until the memo expires.

**Acceptance scenarios**:

1. **Given** a library of items lacking English subtitles and a quota of N downloads left, **Then** at most N downloads run in a day, each at least one second apart, and the job stops for the day on the first 406 or 429 (V567, V568).
2. **Given** an item for which the provider returned no result, **Then** it is not searched again for 7 days or until its file changes (V569).
3. **Given** the job runs, **Then** each decision (searched, hit, miss, quota stop, skipped) is counted by `pharos_opensubtitles_total{op,outcome}` and logged with the item id and language (V571).

## Requirements

- **FR-001** The feature is off unless `[opensubtitles].api_key` or `PHAROS_OPENSUBTITLES_API_KEY` is set (blank is unset). `username` and `password` (or `PHAROS_OPENSUBTITLES_USERNAME` / `_PASSWORD`) are optional and raise the download quota. Secrets are never logged and never appear in a metric label (V570).
- **FR-002** `/download` is called with a bearer token when a username and password are configured, and with the API key alone otherwise. The JWT is cached in memory until 5 minutes before expiry and refreshed once on a 401.
- **FR-003** The OpenSubtitles hash is the file size plus the sum of the first 64 KiB and last 64 KiB read as little-endian u64 words, wrapping, printed as 16 lowercase hex digits. Files under 128 KiB have no hash (V562).
- **FR-004** Downloaded subtitles live at `<subtitle cache dir>/opensubtitles/<media id>/os<file id>.<lang>.srt`. When no subtitle cache directory is configured the feature is off with a startup warning (V564).
- **FR-005** `discover_sidecars` also lists the files in that directory, sorted by path together with the sidecars beside the media, so the stream index is stable between PlaybackInfo and `Stream.vtt` (V565). The sidecar memo is invalidated by that directory's mtime.
- **FR-006** The client sends `User-Agent: Pharos v<crate version>` and paces itself to at most 4 requests per second. A 429 is reported with its `Retry-After` and not retried inside the call. A 406 on `/download` is quota exhaustion: it closes the shared gate until the `reset_time_utc` the API names (24 h when absent), as does a successful download that leaves `remaining` at 0 (V567).
- **FR-007** The job (slice 2) is bounded: one item at a time, a `[opensubtitles].max_downloads_per_day` ceiling (default 20) below the account's quota, a pause between items, a persisted memo of misses (V569), and it yields to playback (`bg_io`).
- **FR-008** The languages the job searches come from `[opensubtitles].languages` (default `["en"]`).
- **FR-009** The Jellyfin `RemoteSubtitleInfo.Id` is `<file id>-<language>` (for example `4242-en`), because the download POST carries nothing else and the stored file needs its language. Search and download require an admin.

## Verification

| Id | Statement | Guard |
|---|---|---|
| V562 | The moviehash of a file is the OpenSubtitles algorithm, and files under 128 KiB have none | unit tests in `opensubtitles::hash` |
| V563 | With no API key the search endpoint returns `[]`, the download endpoint 400, and the provider is never called | handler tests |
| V564 | A downloaded subtitle is written only under the subtitle cache directory | handler test asserts the media directory is unchanged |
| V565 | A downloaded subtitle is an External subtitle stream with its language in MediaSources | handler test via PlaybackInfo |
| V566 | A search carries the hash, the numeric IMDb/TMDB id (parent ids and season/episode for episodes) and the language, and hash matches sort first | client request-building tests, handler ordering test |
| V567 | A 406 or 429 on download stops downloads until the reported reset time, in the client and in the job | client tests against a local responder; job test |
| V568 | The background job downloads at most `max_downloads_per_day` subtitles a day, one at a time, spaced by the rate limit | job test with a fake clock |
| V569 | A search that found nothing is not repeated until the memo entry expires or the file changes | memo unit tests |
| V570 | The API key, password and JWT never appear in logs, errors or metric labels | redaction test on `Debug` of the config and client |
| V571 | Every search, download and job decision is counted by `pharos_opensubtitles_total{op,outcome}` and logged with its inputs | metric test with a disarm check |
