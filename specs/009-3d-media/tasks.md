# Tasks: 3D movies and TV shows

**Input**: Design documents from `/specs/009-3d-media/` ([spec.md](file:///home/ali/git/personal/pharos/.claude/worktrees/lively-brewing-eclipse/specs/009-3d-media/spec.md), [plan.md](file:///home/ali/git/personal/pharos/.claude/worktrees/lively-brewing-eclipse/specs/009-3d-media/plan.md))
**IDs**: Appended after baseline `T128` (`T133`..`T144`), `V167` (`V172`..`V175`), and `B224` (`B229`..`B231`).

## Parallel group execution rules

Every task carries an explicit `Parallel Group: G<N>` label. Tasks sharing the same parallel group number touch strictly **disjoint** files and can be implemented concurrently in separate worker subagents without merge conflicts.

| Parallel Group | Tasks | Prerequisite Groups | Disjoint File Guarantee |
|---|---|---|---|
| **G1** (Foundation & migrations) | `T133`, `T134`, `T135` | None | `pharos-core/src/lib.rs` vs `pharos-scanner/src/metadata/filename.rs` vs `pharos-store-sqlx/migrations/*` |
| **G2** (Probers, store, DTOs, transcode filters) | `T136`, `T137`, `T138`, `T139`, `T140`, `T141` | `G1` | Scanner (`ffmpeg.rs`, `fs.rs`) vs Libav probe (`libav/probe.rs`) vs Store (`sqlite.rs`, `postgres.rs`, `migrate.rs`, `media_query.rs`) vs DTO (`dto.rs`) vs Transcode HLS/stream (`lib.rs`, `hls.rs`, `ffmpeg.rs`, `libav/transcode.rs`) vs Transcode images (`image.rs`, `trickplay.rs`) |
| **G3** (Server endpoints, cache, trickplay wiring) | `T142`, `T143` | `G2` | Server playback/HLS (`device_profile.rs`, `items.rs`, `hls.rs`, `stream.rs`, `hls_cache.rs`) vs Server image/trickplay callers (`images.rs`, `trickplay.rs`) |
| **G4** (Verification & baseline docs) | `T144`, `T145` | `G3` | Integration tests (`tests/client_compat.rs`, `tests/jellyfin_playbackinfo_kotlin_strict.rs`, `tests/resolution_filter_audit.rs`) vs Baseline registries (`specs/001-pharos-baseline/*.md`) |

---

## Phase 1: Parallel Group G1 - Core domain, filename parser, and SQL migrations

- [x] **T133** [Parallel Group: G1] [US1] Define `Video3DFormat` enum (`HalfSideBySide`, `FullSideBySide`, `FullTopAndBottom`, `HalfTopAndBottom`, `Mvc` with `#[serde(rename = "MVC")]`), implement `as_str`, `FromStr`, and `Video3DFormat::from_probe_signals` classifier (handling Matroska `stereo_mode`, `AV_PKT_DATA_STEREO3D` side data, H.264 `Stereo High` / `Multiview High` / `mvc` substreams, and coded aspect ratio half/full disambiguation), add `#[serde(default)] pub video_3d_format: Option<Video3DFormat>` to `MediaProbe`, bump `PROBE_SCHEMA_VERSION` from `6` to `7`, and add unit tests in `crates/pharos-core/src/lib.rs`.
  - **Files**: `crates/pharos-core/src/lib.rs`

- [x] **T134** [Parallel Group: G1] [US1] Add 3D filename token recognition (`3d`, `sbs`, `hsbs`, `h-sbs`, `half-sbs`, `fsbs`, `f-sbs`, `full-sbs`, `tab`, `htab`, `h-tab`, `half-tab`, `ftab`, `f-tab`, `full-tab`, `ou`, `hou`, `h-ou`, `half-ou`, `fou`, `f-ou`, `full-ou`, `mvc`, `bd3d`, `bluray3d`, `anaglyph`) to `quality_source_tag` and expose `pub fn parse_video_3d_format(input: &str) -> (Option<Video3DFormat>, Option<&'static str>)` with unit tests for movie and TV episode stems in `crates/pharos-scanner/src/metadata/filename.rs`.
  - **Files**: `crates/pharos-scanner/src/metadata/filename.rs`

- [x] **T135** [Parallel Group: G1] [US1] Create migration `0057_video_3d_format.sql` for both SQLite and PostgreSQL adding `ALTER TABLE media_items ADD COLUMN video_3d_format TEXT;` and partial index `idx_media_items_video_3d_format`.
  - **Files**: `crates/pharos-store-sqlx/migrations/sqlite/0057_video_3d_format.sql`, `crates/pharos-store-sqlx/migrations/postgres/0057_video_3d_format.sql`

---

## Phase 2: Parallel Group G2 - Probers, store persistence, Jellyfin DTOs, and transcode filters

- [x] **T136** [Parallel Group: G2] [US1] Wire `FfmpegProber` (`tags.stereo_mode`, `side_data_list` `Stereo 3D`, MVC profile/substream) in `crates/pharos-scanner/src/ffmpeg.rs`, merge probe and filename 3D detection inside `FsScanner::probe_one` for both `MediaKind::Movie` and `MediaKind::Episode` in `crates/pharos-scanner/src/fs.rs`, and emit ODD counter `pharos_3d_detect_total{format,source,media_kind}` and `tracing::info!(target: "pharos::scanner::3d", ...)` (`V173`).
  - **Files**: `crates/pharos-scanner/src/ffmpeg.rs`, `crates/pharos-scanner/src/fs.rs`

- [x] **T137** [Parallel Group: G2] [US1] Wire `LibavProber::extract_video` to read stream metadata `stereo_mode`, `AV_PKT_DATA_STEREO3D` (`AVStereo3D`) coded side data, and H.264 MVC profile names via `Video3DFormat::from_probe_signals` so `ffmpeg-lib` and `ffmpeg-spawn` produce identical `MediaProbe.video_3d_format` results (`V152`, `V173`).
  - **Files**: `crates/pharos-transcode/src/libav/probe.rs`

- [x] **T138** [Parallel Group: G2] [US1] Persist `probe.video_3d_format` in `MEDIA_COLUMNS`, `MediaStore::put`, `MediaRow`, and `MediaRow::into_domain` across SQLite and PostgreSQL, include `video_3d_format` in `crates/pharos-store-sqlx/src/migrate.rs`, replace the `1 = 0` stub in `crates/pharos-store-sqlx/src/media_query.rs` (`L555-561`) with `video_3d_format IS NOT NULL` / `IS NULL`, and update `crates/pharos-store-sqlx/tests/media_query.rs`.
  - **Files**: `crates/pharos-store-sqlx/src/sqlite.rs`, `crates/pharos-store-sqlx/src/postgres.rs`, `crates/pharos-store-sqlx/src/migrate.rs`, `crates/pharos-store-sqlx/src/media_query.rs`, `crates/pharos-store-sqlx/tests/media_query.rs`

- [x] **T139** [Parallel Group: G2] [US1, US2] Add `#[serde(rename = "Video3DFormat", skip_serializing_if = "Option::is_none")] pub video_3d_format: Option<Video3DFormat>` to `BaseItemDto`, `MediaSourceLiteDto`, `MediaSourceInfoDto`, and `MediaStreamDto` (populated on the `Video` stream entry in `media_streams_from_probe` and on movie/episode items in `BaseItemDto::from_item`), plus unit tests asserting exact `"Video3DFormat"` JSON key casing (`V172`, `B229`).
  - **Files**: `crates/pharos-jellyfin-api/src/dto.rs`

- [ ] **T140** [Parallel Group: G2] [US2, US3] Define `StereoOutput::{Preserve, Mono2D}` and add `video_3d_format: Option<Video3DFormat>` and `stereo_output: StereoOutput` to `TranscodeOptions` (`crates/pharos-transcode/src/lib.rs`) and `SegmentOpts` (`crates/pharos-transcode/src/hls.rs`). Implement `stereo_mono_filter(Video3DFormat)` in `hls.rs`, force `Mono2D` when burning subtitles onto packed SBS/TAB (`B230`), omit `-hwaccel_output_format cuda` when `mono_vf` is active (`B231`), prepend `mono_vf` before `hwupload` and subtitle filters in `hls.rs`, `ffmpeg.rs`, and `libav/transcode.rs`, and strip output `stereo_mode` metadata when `Mono2D` is active (`V175`).
  - **Files**: `crates/pharos-transcode/src/lib.rs`, `crates/pharos-transcode/src/hls.rs`, `crates/pharos-transcode/src/ffmpeg.rs`, `crates/pharos-transcode/src/libav/transcode.rs`

- [ ] **T141** [Parallel Group: G2] [US3] Update `extract_video_frame` in `crates/pharos-transcode/src/image.rs` and `generate_sprites` in `crates/pharos-transcode/src/trickplay.rs` to accept `video_3d_format: Option<Video3DFormat>` and prepend the left-eye crop/scale filter before `scale={width}:-2` so thumbnails and trickplay tiles never render squished double images (`V175`).
  - **Files**: `crates/pharos-transcode/src/image.rs`, `crates/pharos-transcode/src/trickplay.rs`

---

## Phase 3: Parallel Group G3 - Server playback negotiation, HLS cache identity, and image/trickplay callers

- [ ] **T142** [Parallel Group: G3] [US2, US3] Extend `SourceMedia` and `evaluate_condition` in `crates/pharos-server/src/api/jellyfin/device_profile.rs` to support `Is3D` and `Video3DFormat` `ProfileCondition` rules while keeping DirectPlay passthrough as the default; wire `StereoMode` (`preserve` | `mono`) query parameter and `StereoOutput` through `items.rs` (`playback_info`), `hls.rs` (`master_playlist`, `main_playlist`, `segment`), `stream.rs`, and `hls_cache.rs` (`RenditionKey` and `SegmentIdentity`, appending `_mono2d` only on `Mono2D`, `V174`); emit ODD counter `pharos_playback_3d_decision_total` and `tracing::info!(target: "pharos::playback::3d", ...)`.
  - **Files**: `crates/pharos-server/src/api/jellyfin/device_profile.rs`, `crates/pharos-server/src/api/jellyfin/items.rs`, `crates/pharos-server/src/api/jellyfin/hls.rs`, `crates/pharos-server/src/api/jellyfin/stream.rs`, `crates/pharos-server/src/hls_cache.rs`

- [ ] **T143** [Parallel Group: G3] [US3] Pass `item.probe.video_3d_format` into `extract_video_frame` from `crates/pharos-server/src/api/jellyfin/images.rs` and into `generate_sprites` from `crates/pharos-server/src/trickplay.rs`.
  - **Files**: `crates/pharos-server/src/api/jellyfin/images.rs`, `crates/pharos-server/src/trickplay.rs`

---

## Phase 4: Parallel Group G4 - Client compatibility tests and baseline registry updates

- [ ] **T144** [Parallel Group: G4] [US1, US2, US3] Add end-to-end 3D movie and TV episode tests in `crates/pharos-server/tests/client_compat.rs` (verifying `Video3DFormat` on `BaseItemDto`, `MediaSourceInfo`, and `MediaStream`, default `DirectPlay` across client profiles, `ProfileCondition` `Is3D=false` forcing `Transcode` with `StereoMode=mono`, and `SegmentIdentity` isolation), `crates/pharos-server/tests/jellyfin_playbackinfo_kotlin_strict.rs` (verifying strict Kotlin SDK deserialization of `Video3DFormat`), and update `crates/pharos-server/tests/resolution_filter_audit.rs` (`L128`) to assert `/Items?Is3D=true` and `/Items?Is3D=false`.
  - **Files**: `crates/pharos-server/tests/client_compat.rs`, `crates/pharos-server/tests/jellyfin_playbackinfo_kotlin_strict.rs`, `crates/pharos-server/tests/resolution_filter_audit.rs`

- [ ] **T145** [Parallel Group: G4] [US1, US2, US3] Append `T133`–`T144` to `specs/001-pharos-baseline/tasks.md`, `V172`–`V175` to `specs/001-pharos-baseline/invariants.md`, and `B229`–`B231` to `specs/001-pharos-baseline/bugs.md`.
  - **Files**: `specs/001-pharos-baseline/tasks.md`, `specs/001-pharos-baseline/invariants.md`, `specs/001-pharos-baseline/bugs.md`
