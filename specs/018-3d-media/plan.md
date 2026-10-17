# Implementation plan: 3D movies and TV shows

**Branch**: `018-3d-media` | **Date**: 2026-10-18T00:25:00Z | **Spec**: [spec.md](file:///home/ali/git/personal/pharos/.claude/worktrees/lively-brewing-eclipse/specs/018-3d-media/spec.md)

---

## Summary

Add end-to-end 3D movie and TV episode support across six workspace crates:
1. **`pharos-core`**: Define `Video3DFormat`, add `video_3d_format: Option<Video3DFormat>` to `MediaProbe`, and bump `PROBE_SCHEMA_VERSION` from `6` to `7`.
2. **`pharos-scanner` & `pharos-transcode` (probing)**: Parse Matroska/WebM `stereo_mode` tags, MP4/MOV `AV_PKT_DATA_STEREO3D` side data, and H.264 MVC (`Stereo High` / `Multiview High`) profiles in both `FfmpegProber` (`ffmpeg.rs`) and `LibavProber` (`libav/probe.rs`). Parse filename 3D tokens (`SBS`, `HSBS`, `FSBS`, `TAB`, `HTAB`, `FTAB`, `OU`, `HOU`, `FOU`, `MVC`, `3D`, `anaglyph`) in `filename.rs` and apply fallback/disambiguation in `FsScanner::probe_one` (`fs.rs`) for both movies and TV episodes.
3. **`pharos-store-sqlx`**: Add migration `0057_video_3d_format.sql` for SQLite and PostgreSQL, wire `video_3d_format` through `sqlite.rs`, `postgres.rs`, and `migrate.rs`, and implement `MediaFilters::is_3d` in `media_query.rs`.
4. **`pharos-jellyfin-api`**: Expose `Video3DFormat` with explicit `#[serde(rename = "Video3DFormat")]` on `BaseItemDto`, `MediaSourceLiteDto`, `MediaSourceInfoDto`, and `MediaStreamDto` (`V180` / `B238`).
5. **`pharos-transcode` & `pharos-server` (playback, transcode, thumbnails, trickplay)**: Default to `DirectPlay` passthrough; evaluate `Is3D` and `Video3DFormat` `ProfileCondition` rules in `device_profile.rs`; support `StereoOutput::{Preserve, Mono2D}` in `TranscodeOptions`, `SegmentOpts`, `RenditionKey`, and `SegmentIdentity` (`V182`); crop to the left eye in `Mono2D` mode, when burning subtitles onto packed 3D (`B239`), and when extracting thumbnails (`image.rs`) or trickplay sprites (`trickplay.rs`).

---

## Technical context

- **Language/Version**: Rust 1.85+ (2024 edition)
- **Primary Dependencies**: `axum` 0.8, `sqlx` 0.8 (SQLite + PostgreSQL), `serde` 1.0, `tokio` 1.x, `metrics` 0.24, `tracing` 0.1, `ffmpeg-sys-next` (optional `ffmpeg-lib` feature)
- **Storage**: SQLite and PostgreSQL via `pharos-store-sqlx` (migration `0057_video_3d_format.sql`)
- **Testing**: `cargo nextest run` unit and integration suites across `pharos-core`, `pharos-scanner`, `pharos-transcode`, `pharos-store-sqlx`, `pharos-jellyfin-api`, and `pharos-server` (`tests/client_compat.rs`, `tests/jellyfin_playbackinfo_kotlin_strict.rs`, `tests/resolution_filter_audit.rs`)
- **Constraints**:
  - Wire compatibility with Jellyfin 10.9/10.10 OpenAPI (`Video3DFormat` exact casing) and `jellyfin-sdk-kotlin` strict model decoding.
  - Zero regression on 2D HLS segment cache keys (`SegmentIdentity::cache_file_name` unchanged when `StereoOutput::Preserve`).
  - Identical probe behaviour between `ffmpeg-spawn` and `ffmpeg-lib` backends (`V152`, `V181`).

---

## Constitution check

| Principle | Status | Notes |
|---|---|---|
| **I. Spec-First & Baseline Authority** | Pass | Appends `T133`–`T144`, `V180`–`V183`, and `B238`–`B240` after baseline `T128` / `V167` / `B224` without renumbering. |
| **II. Verification Gates & Client Compatibility** | Pass | Adds 3D coverage to `tests/client_compat.rs` and `tests/jellyfin_playbackinfo_kotlin_strict.rs` plus SQLite/PostgreSQL parity tests. |
| **III. Privacy, Safe Logging & Wire Discipline** | Pass | Explicit `#[serde(rename = "Video3DFormat")]` prevents `PascalCase` `Video3dFormat` drift (`B238`); bounded metric labels contain no file paths or titles. |
| **IV. Resource Bounds & Concurrency Safety** | Pass | `SegmentIdentity` and `RenditionKey` include `StereoOutput` so concurrent 2D and 3D transcodes never collide in `hls_cache`. |
| **V. Observability-Driven Development (ODD)** | Pass | Defines `pharos_3d_detect_total` and `pharos_playback_3d_decision_total` counters and `pharos::scanner::3d` / `pharos::playback::3d` structured tracing spans before business logic. |

---

## Project structure and touched files

| Crate / Area | Files touched | Change summary |
|---|---|---|
| `pharos-core` | `crates/pharos-core/src/lib.rs` | Define `Video3DFormat` enum (`HalfSideBySide`, `FullSideBySide`, `FullTopAndBottom`, `HalfTopAndBottom`, `Mvc`), add `video_3d_format: Option<Video3DFormat>` to `MediaProbe`, bump `PROBE_SCHEMA_VERSION` `6 -> 7`, update `MediaFilters::is_3d` doc comment. |
| `pharos-scanner` (filename) | `crates/pharos-scanner/src/metadata/filename.rs` | Add `parse_video_3d_format(&str) -> (Option<Video3DFormat>, Option<&'static str>)`, add 3D tokens (`3d`, `sbs`, `hsbs`, `fsbs`, `tab`, `htab`, `ftab`, `ou`, `hou`, `fou`, `mvc`, `bd3d`, `anaglyph`) to `quality_source_tag`. |
| `pharos-scanner` (ffprobe + fs) | `crates/pharos-scanner/src/ffmpeg.rs`, `crates/pharos-scanner/src/fs.rs` | Parse `tags.stereo_mode`, `side_data_list` (`Stereo 3D`), and MVC profiles/streams in `FfmpegProber`; merge probe + filename 3D detection in `FsScanner::probe_one` for movies and TV episodes; emit `pharos_3d_detect_total`. |
| `pharos-transcode` (libav probe) | `crates/pharos-transcode/src/libav/probe.rs` | Read `stereo_mode` stream metadata tag, `AV_PKT_DATA_STEREO3D` (`AVStereo3D`) coded side data, and H.264 `Stereo High` / `Multiview High` profile in `LibavProber::extract_video`. |
| `pharos-store-sqlx` | `crates/pharos-store-sqlx/migrations/sqlite/0057_video_3d_format.sql`, `crates/pharos-store-sqlx/migrations/postgres/0057_video_3d_format.sql`, `crates/pharos-store-sqlx/src/sqlite.rs`, `crates/pharos-store-sqlx/src/postgres.rs`, `crates/pharos-store-sqlx/src/migrate.rs`, `crates/pharos-store-sqlx/src/media_query.rs`, `crates/pharos-store-sqlx/tests/media_query.rs` | Add `video_3d_format TEXT` column + partial index; map in `MEDIA_COLUMNS`, `put`, `MediaRow::into_domain`, and `migrate.rs`; implement `is_3d` SQL filter (`IS NOT NULL` / `IS NULL`). |
| `pharos-jellyfin-api` | `crates/pharos-jellyfin-api/src/dto.rs` | Add `#[serde(rename = "Video3DFormat", skip_serializing_if = "Option::is_none")] pub video_3d_format: Option<Video3DFormat>` to `BaseItemDto`, `MediaSourceLiteDto`, `MediaSourceInfoDto`, and `MediaStreamDto` (Video stream only). |
| `pharos-transcode` (filters & media) | `crates/pharos-transcode/src/lib.rs`, `crates/pharos-transcode/src/hls.rs`, `crates/pharos-transcode/src/ffmpeg.rs`, `crates/pharos-transcode/src/libav/transcode.rs`, `crates/pharos-transcode/src/image.rs`, `crates/pharos-transcode/src/trickplay.rs` | Add `StereoOutput::{Preserve, Mono2D}` and `stereo_mono_filter(Video3DFormat)`; prepend left-eye crop/scale on `Mono2D`, on packed-3D subtitle burn-in (`B239`), and on thumbnail/trickplay extraction; drop `-hwaccel_output_format cuda` when CPU `mono_vf` runs (`B240`); clear `stereo_mode` output tag on `Mono2D`. |
| `pharos-server` | `crates/pharos-server/src/api/jellyfin/device_profile.rs`, `crates/pharos-server/src/api/jellyfin/items.rs`, `crates/pharos-server/src/api/jellyfin/hls.rs`, `crates/pharos-server/src/api/jellyfin/stream.rs`, `crates/pharos-server/src/hls_cache.rs`, `crates/pharos-server/src/trickplay.rs`, `crates/pharos-server/src/api/jellyfin/images.rs` | Evaluate `Is3D` and `Video3DFormat` in `ProfileCondition`; default to `DirectPlay`; wire `StereoMode` query param and `StereoOutput` into `RenditionKey` and `SegmentIdentity`; pass `item.probe.video_3d_format` into image and trickplay generators; emit `pharos_playback_3d_decision_total`. |
| Integration tests | `crates/pharos-server/tests/client_compat.rs`, `crates/pharos-server/tests/jellyfin_playbackinfo_kotlin_strict.rs`, `crates/pharos-server/tests/resolution_filter_audit.rs` | Verify 3D `PlaybackInfo`, DirectPlay passthrough, `Is3D` profile condition transcode fallback, Kotlin SDK strict parsing of `Video3DFormat`, and `/Items?Is3D=true` filter audit. |

---

## Detailed design by component

### 1. Domain model (`crates/pharos-core/src/lib.rs`)

1. Bump `pub const PROBE_SCHEMA_VERSION: u32 = 7;` at line 2253.
2. Define `Video3DFormat` with `as_str(&self) -> &'static str` and `FromStr` parsing `"HalfSideBySide"`, `"FullSideBySide"`, `"FullTopAndBottom"`, `"HalfTopAndBottom"`, and `"MVC"`.
3. Add `#[serde(default)] pub video_3d_format: Option<Video3DFormat>` to `MediaProbe` (`L1726`).
4. Provide a shared pure helper `Video3DFormat::from_probe_signals(stereo_mode_tag: Option<&str>, side_data_type: Option<&str>, video_codec: Option<&str>, video_profile: Option<&str>, has_mvc_substream: bool, width: Option<u32>, height: Option<u32>) -> (Option<Video3DFormat>, Option<&'static str>)` in `pharos-core` so `FfmpegProber` (`pharos-scanner`) and `LibavProber` (`pharos-transcode`) share one single-source-of-truth classifier (`V181`).

### 2. Prober and filename detection (`pharos-scanner` & `pharos-transcode`)

1. **`FfmpegProber` (`crates/pharos-scanner/src/ffmpeg.rs`)**:
   - In `FfprobeStream` (`L96`): deserialize `side_data_list: Option<Vec<FfprobeSideData>>` where `FfprobeSideData` reads `side_data_type: Option<String>` and `r#type: Option<String>`.
   - In `FfprobeStreamTags` (`L145`): add `stereo_mode: Option<String>` (plus case-insensitive fallback for `"STEREO_MODE"`).
   - Check if any video stream in `parsed.streams` has `codec_name == Some("mvc")` or `profile` in `["Stereo High", "Multiview High"]`.
   - Call `Video3DFormat::from_probe_signals(...)` to populate `MediaProbe.video_3d_format`.
2. **`LibavProber` (`crates/pharos-transcode/src/libav/probe.rs`)**:
   - In `extract_video(par, st)` (`L209`): read `dict_get(st.metadata, "stereo_mode")`.
   - Inspect `par.coded_side_data` (`0..par.nb_coded_side_data`) for `AVPacketSideDataType::AV_PKT_DATA_STEREO3D`; cast `sd.data as *const AVStereo3D` and map `(*stereo).type_` (`AV_STEREO3D_SIDEBYSIDE` -> `"side by side"`, `AV_STEREO3D_TOPBOTTOM` -> `"top and bottom"`, `AV_STEREO3D_FRAMESEQUENCE` -> `"frame alternate"`).
   - Call `Video3DFormat::from_probe_signals(...)` so `ffmpeg-lib` and `ffmpeg-spawn` always agree (`V152`, `V181`).
3. **Filename parser (`crates/pharos-scanner/src/metadata/filename.rs` & `fs.rs`)**:
   - Add `pub fn parse_video_3d_format(stem: &str) -> (Option<Video3DFormat>, Option<&'static str>)` returning the resolved format and a metric label (`Some("anaglyph")` or `Some("unspecified_3d")` when a non-layout 3D tag is seen).
   - Update `quality_source_tag` (`filename.rs:255`) to return `true` for `"3d"`, `"sbs"`, `"hsbs"`, `"fsbs"`, `"tab"`, `"htab"`, `"ftab"`, `"ou"`, `"hou"`, `"fou"`, `"mvc"`, `"bd3d"`, `"bluray3d"`, `"anaglyph"`.
   - In `FsScanner::probe_one` (`crates/pharos-scanner/src/fs.rs:1254`): combine `probe.video_3d_format` with `parse_video_3d_format(&file_stem)`, record `metrics::counter!("pharos_3d_detect_total", "format" => ..., "source" => ..., "media_kind" => ...).increment(1)`, and emit `tracing::info!(target: "pharos::scanner::3d", ...)`.

### 3. Database migration and queries (`pharos-store-sqlx`)

1. Create `crates/pharos-store-sqlx/migrations/sqlite/0057_video_3d_format.sql` and `crates/pharos-store-sqlx/migrations/postgres/0057_video_3d_format.sql`:
   ```sql
   ALTER TABLE media_items ADD COLUMN video_3d_format TEXT;
   CREATE INDEX IF NOT EXISTS idx_media_items_video_3d_format
       ON media_items (video_3d_format)
       WHERE video_3d_format IS NOT NULL;
   ```
2. Update `MEDIA_COLUMNS`, `put` (`INSERT ... ON CONFLICT DO UPDATE`), `MediaRow`, and `MediaRow::into_domain` in `sqlite.rs` and `postgres.rs`, and `MEDIA_MIGRATE_COLUMNS` / `MediaItemRow` / `copy_media_items` in `migrate.rs`.
3. Update `media_query.rs:555-561` so `filters.is_3d` generates `AND video_3d_format IS NOT NULL` for `Some(true)` and `AND video_3d_format IS NULL` for `Some(false)`.

### 4. Jellyfin DTOs and capability negotiation (`pharos-jellyfin-api` & `pharos-server`)

1. In `crates/pharos-jellyfin-api/src/dto.rs`, re-export `pharos_core::Video3DFormat` and add `#[serde(rename = "Video3DFormat", skip_serializing_if = "Option::is_none")] pub video_3d_format: Option<Video3DFormat>` to `BaseItemDto`, `MediaSourceLiteDto`, `MediaSourceInfoDto`, and `MediaStreamDto`.
2. In `crates/pharos-server/src/api/jellyfin/device_profile.rs`:
   - Add `pub video_3d_format: Option<Video3DFormat>` to `SourceMedia`.
   - Add `TranscodeReason::Video3DFormatNotSupported` (or map to string `"Video3DFormatNotSupported"` in `TranscodeReasons` query param while keeping strict enum compatibility).
   - In `evaluate_condition`: handle `"Is3D"` and `"Video3DFormat"`.
   - When `decide_playback` runs on a 3D item:
     - If `DirectPlay` / `DirectStream` succeeds, `stereo_output` is `Preserve` (client receives bitstream).
     - If `Is3D` or `Video3DFormat` condition fails, reject `DirectPlay` and `DirectStream` and set `stereo_output = StereoOutput::Mono2D`.
   - Emit `pharos_playback_3d_decision_total` and `tracing::info!(target: "pharos::playback::3d", ...)` in `items.rs::playback_info`.

### 5. Transcode, subtitles, thumbnails, trickplay, and HLS cache (`pharos-transcode` & `pharos-server`)

1. Add `pub fn stereo_mono_filter(format: Video3DFormat) -> Option<&'static str>` in `crates/pharos-transcode/src/hls.rs`.
2. In `crates/pharos-transcode/src/hls.rs`:
   - Determine effective `StereoOutput`: `Mono2D` if `opts.stereo_output == StereoOutput::Mono2D` OR (`opts.burn_sub.is_some()` && `opts.video_3d_format` is SBS/TAB, per `B239`).
   - When effective `StereoOutput` is `Mono2D` and `opts.video_3d_format.and_then(stereo_mono_filter)` returns `Some(mono_vf)`:
     - Disable `-hwaccel_output_format cuda` on NVENC decode so frames enter the CPU `crop`/`scale` filter cleanly (`B240`).
     - Prepend `mono_vf` to the `-vf` chain before subtitle burn-in and before `format=nv12,hwupload`.
     - Append `-metadata:s:v:0 stereo_mode=` to clear stereo metadata on the 2D output.
3. Apply the same `stereo_mono_filter` logic in `crates/pharos-transcode/src/ffmpeg.rs` (progressive stream transcode), `crates/pharos-transcode/src/image.rs` (`extract_video_frame`), and `crates/pharos-transcode/src/trickplay.rs` (`generate_sprites`).
4. In `crates/pharos-server/src/hls_cache.rs`: include `stereo_output: StereoOutput` in `RenditionKey` and `SegmentIdentity`, appending `_mono2d` to `SegmentIdentity::cache_file_name` and `segment_etag` when `stereo_output == StereoOutput::Mono2D` (`V182`).
