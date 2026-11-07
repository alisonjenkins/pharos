# Tasks: OpenSubtitles subtitle search and automatic download (030)

**Input**: [spec.md](spec.md), [plan.md](plan.md), ADR-0069. Ids are local to this spec (`T030-NN`). Every task follows TDD: a failing test first, then the code.

## Slice 1 - Hand search and download (V562-V566, V570)

- [ ] T030-01 `opensubtitles/hash.rs`: `moviehash_from_parts` and `moviehash`. Tests: hand-computed vectors (zero-filled file, counting bytes), under 128 KiB gives none, a file read from disk equals the pure function (V562).
- [ ] T030-02 `opensubtitles/mod.rs`: the `SubtitleProvider` trait, query/hit types, `ProviderError`, `QuotaGate`.
- [ ] T030-03 `opensubtitles/client.rs`: the reqwest client. Tests against a local responder: headers (`Api-Key`, `User-Agent`), query string, login then bearer reuse, 401 re-login once, 406 and 429 map to quota errors with the reset time (V566, V567).
- [ ] T030-04 `[opensubtitles]` config and env overrides, `AppState` wiring in `main.rs`, startup log naming whether the feature is on. A `Debug` redaction test (V570).
- [ ] T030-05 `opensubtitles/store.rs` and the `discover_sidecars` extension. Tests: a saved file shows up as a sidecar with its language, the media directory is untouched (V564, V565).
- [ ] T030-06 `RemoteSubtitleInfoDto` in `pharos-jellyfin-api`, the search handler and the new POST handler, metrics. Tests with a fake provider: off without a key (V563), ordering, download then PlaybackInfo lists the stream (V565), quota answers 429 (V567).
- [ ] T030-07 Close the audit rows (`jellyfin-parity-audit.md`, `jellyfin-web-feature-matrix.md`, `ui-gap-assessment.md` D18, `library-backlog.md` LIB-E8), append V562-V571 to `invariants.md`, set the ADR to Accepted.

## Slice 2 - Background job (V567, V568, V569, V571)

- [ ] T030-08 `opensubtitles/memo.rs`: miss memo beside the cache, keyed by item id and language, signature `(mtime, size)` plus a 7-day expiry and a schema version.
- [ ] T030-09 `opensubtitles/job.rs`: after-scan pass over items with no subtitle in the configured languages. One at a time, `max_downloads_per_day`, 1 s spacing, `bg_io` permit, stops for the day on a quota error. Tests with a fake provider and clock.
- [ ] T030-10 Spawn the job from `main.rs` behind the key, and add the `pharos_opensubtitles_total` outcomes for the job with a disarm check.
