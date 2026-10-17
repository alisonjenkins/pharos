# Feature specification: 3D movies and TV shows

**Feature Branch**: `018-3d-media`
**Created**: 2026-10-18T00:25:00Z
**Status**: Ready
**Scope**: Stereoscopic 3D detection from probe metadata and filename tags across movies and TV episodes, `Video3DFormat` wire exposure on Jellyfin DTOs, SQLite and PostgreSQL schema migration `0057` with `Is3D` filtering, DirectPlay passthrough by default, stereo-preserving and 2D left-eye conversion transcode pipelines (including thumbnail, trickplay, and subtitle burn-in safety), observability signals, and client compatibility verification.

**Note (2026-11-01)**: landed after 3D edition grouping, which already added `media_items.video_3d_format` and `has_3d` in migration `0058_video_3d`. The planned `0057_video_3d_format` migration and its partial index were dropped, `Is3D` follows `has_3d` (primary file or any grouped edition), and the stream-detection re-probe is `PROBE_SCHEMA_VERSION = 8`.

---

## Problem statement

Pharos currently discards stereoscopic 3D layout metadata during probing and filename parsing, leaving `MediaFilters::is_3d` hardcoded to match zero items (`1 = 0`), omitting `Video3DFormat` from `BaseItemDto`, `MediaSourceInfo`, and `MediaStreamDto`, and causing image extraction, trickplay generation, subtitle burn-in, and 2D client playback to mishandle packed side-by-side (SBS) and top-and-bottom (TAB) frames. Users with 3D movies or 3D TV episodes need clients that support 3D (Kodi, Infuse, VR players, 3D TVs/projectors) to receive `Video3DFormat` and DirectPlay the bitstream unmodified, while 2D-only clients and server-generated thumbnails/trickplay tiles need clean single-eye (left-eye) 2D output without squished double-image artifacts or cache collisions.

---

## User scenarios and testing

### User story 1 - Scan, persist, and browse 3D movies and TV episodes (Priority: P1)

A library contains 3D movies (for example `Avatar (2009) [1080p.HSBS].mkv`, `Gravity (2013) 3D.MVC.mkv`) and 3D TV episodes (`Doctor Who - S07E14 - The Day of the Doctor [1080p.HTAB].mkv`). When the scanner runs, Pharos inspects both container/stream metadata (`stereo_mode` tags, `AV_PKT_DATA_STEREO3D` side data, H.264 `Stereo High` / `Multiview High` MVC profiles) and filename tokens (`HSBS`, `SBS`, `FSBS`, `HTAB`, `TAB`, `FTAB`, `OU`, `HOU`, `MVC`, `3D`, `anaglyph`). Upgraded libraries automatically re-probe existing files via `PROBE_SCHEMA_VERSION = 7`. Clients querying `/Items`, `/Users/{userId}/Items`, `/Shows/{id}/Episodes`, or `/Items/{id}/PlaybackInfo` receive `Video3DFormat` on the item, media source, and video stream, and can filter by `Is3D=true` or `Is3D=false`.

**Independent test**: Scan fixture movies and TV episodes covering Matroska `stereo_mode`, MP4 `Stereo 3D` side data, H.264 MVC profile, and filename-only tags into both SQLite and PostgreSQL stores; assert `BaseItemDto.Video3DFormat`, `MediaSourceInfo.Video3DFormat`, `MediaStreamDto.Video3DFormat`, and `/Items?Is3D=true` / `/Items?Is3D=false`.

**Acceptance scenarios**:

1. **Given** an MKV with `tags.stereo_mode = "left_right"` at `1920x1080` and no filename tag, **When** scanned, **Then** `probe.video_3d_format` is `Some(Video3DFormat::HalfSideBySide)` and `/Items?Is3D=true` returns the item.
2. **Given** an MKV with `tags.stereo_mode = "left_right"` at `3840x1080` (or filename tag `FSBS` / `Full-SBS`), **When** scanned, **Then** `probe.video_3d_format` is `Some(Video3DFormat::FullSideBySide)`.
3. **Given** a TV episode file `Show.S01E02.1080p.HTAB.mkv` with no container `stereo_mode` tag, **When** scanned, **Then** `FsScanner` applies filename 3D detection to `MediaKind::Episode`, sets `Video3DFormat::HalfTopAndBottom`, and strips `HTAB` from the parsed episode title fallback.
4. **Given** an existing database at `PROBE_SCHEMA_VERSION = 6`, **When** upgraded to migration `0057` and `PROBE_SCHEMA_VERSION = 7`, **Then** incremental scan re-probes existing items and populates `video_3d_format`.

---

### User story 2 - DirectPlay passthrough by default and capability negotiation (Priority: P1)

When a client requests `/Items/{id}/PlaybackInfo` for a 3D item, Pharos defaults to `DirectPlay` (or `DirectStream` remux) whenever the container, codec, audio, and bitrate match the client's `DeviceProfile`, because packed SBS/TAB frames are standard H.264/HEVC video frames and MVC MKVs carry a backwards-compatible H.264 base view. If a client declares a `CodecProfile` condition on `Is3D` (`Equals: "false"`) or `Video3DFormat` restricting supported 3D layouts, or passes `?StereoMode=mono` (or `AllowVideoStreamCopy=false` with a 2D-only profile), Pharos routes playback to transcode with 2D left-eye conversion (`StereoOutput::Mono2D`).

**Independent test**: POST `/Items/{id}/PlaybackInfo` with each canonical client fixture from `crates/pharos-server/tests/client_compat.rs` (Jellyfin Web, Swiftfin, Findroid, Infuse, Kodi, Roku, Jellyfin Android TV) against `HalfSideBySide` and `Mvc` items; assert `DirectPlay` by default and `Transcode` with `StereoOutput::Mono2D` when constrained by `ProfileCondition` or query parameter.

**Acceptance scenarios**:

1. **Given** a `1080p` H.264 `HalfSideBySide` MKV and a client profile supporting MKV + H.264 with no `Is3D` restriction, **When** `POST /Items/{id}/PlaybackInfo` is called, **Then** `SupportsDirectPlay = true` and `Video3DFormat = "HalfSideBySide"` is present on `MediaSourceInfo` and the video `MediaStream`.
2. **Given** a client `DeviceProfile` with a `VideoCodec` `CodecProfile` containing `ProfileCondition { Condition: Equals, Property: Is3D, Value: "false", IsRequired: true }`, **When** `POST /Items/{id}/PlaybackInfo` is called for a `HalfSideBySide` item, **Then** `SupportsDirectPlay = false`, `SupportsDirectStream = false`, `TranscodeReasons` includes `Video3DFormatNotSupported` (mapped to `VideoCodecNotSupported` on strict Jellyfin wire enums where required), and `TranscodingUrl` includes `StereoMode=mono`.
3. **Given** a `HalfSideBySide` item and a client requesting `StereoMode=preserve` (default when transcoding for bitrate/codec reasons on a 3D-capable client), **When** HLS transcoding runs, **Then** the video filter chain preserves the packed stereo frame without cropping a single eye.

---

### User story 3 - 2D conversion, subtitle burn-in safety, and single-eye thumbnails/trickplay (Priority: P1)

When Pharos extracts a poster/backdrop thumbnail (`image.rs`), generates a trickplay sprite sheet (`trickplay.rs`), burns subtitles into a packed 3D video (`hls.rs`), or transcodes for a 2D client (`StereoOutput::Mono2D`), scaling the raw packed frame directly produces a unusable side-by-side double image or splits burned-in subtitles across both eyes. Pharos crops the left eye (`crop=iw/2:ih:0:0` for SBS, `crop=iw:ih/2:0:0` for TAB) and scales half-resolution layouts back to square-pixel display dimensions (`scale=iw*2:ih,setsar=1` for `HalfSideBySide`, `scale=iw:ih*2,setsar=1` for `HalfTopAndBottom`, `setsar=1` for Full SBS/TAB, no crop for `Mvc`) before downstream scaling, hardware upload, or subtitle burn-in.

**Independent test**: Unit test filter-graph construction across software, VAAPI, QSV, NVENC, and VideoToolbox encoders for all five `Video3DFormat` variants under `StereoOutput::Preserve` and `StereoOutput::Mono2D`, plus thumbnail and trickplay command builders.

**Acceptance scenarios**:

1. **Given** a `HalfSideBySide` (`1920x1080`) item and `StereoOutput::Mono2D`, **When** building the ffmpeg `-vf` chain, **Then** `crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1` is prepended before hardware upload (`hwupload`) or software formatting, producing a `1920x1080` single-eye output.
2. **Given** a `HalfSideBySide` item with burned-in image subtitles (`PGS`/`VobSub`), **When** transcoding runs, **Then** Pharos forces `StereoOutput::Mono2D` left-eye conversion before overlaying the subtitle plane so the burned subtitle is centered in a 2D frame rather than stretched across both eyes.
3. **Given** two concurrent clients watching the same 3D item via HLS transcode, one with `StereoOutput::Preserve` and one with `StereoOutput::Mono2D`, **When** segments are cached in `hls_cache.rs`, **Then** `SegmentIdentity` and `RenditionKey` include `stereo_output` so the two clients never share cached segments (`V132`, `V182`).
4. **Given** a `HalfSideBySide` or `FullTopAndBottom` item, **When** `extract_video_frame` or `generate_sprites` runs, **Then** the filter chain crops to the left eye before `scale={width}:-2`.

---

## Detection architecture

### Canonical `Video3DFormat` enum

`crates/pharos-core/src/lib.rs` defines the domain enum matching Jellyfin's OpenAPI `Video3DFormat` schema:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Video3DFormat {
    HalfSideBySide,
    FullSideBySide,
    FullTopAndBottom,
    HalfTopAndBottom,
    #[serde(rename = "MVC")]
    Mvc,
}
```

Database column `media_items.video_3d_format` stores the canonical Jellyfin wire string (`"HalfSideBySide"`, `"FullSideBySide"`, `"FullTopAndBottom"`, `"HalfTopAndBottom"`, `"MVC"`).

### Probe metadata extraction (`ffmpeg.rs` and `libav/probe.rs`)

Both `FfmpegProber` (`crates/pharos-scanner/src/ffmpeg.rs`) and `LibavProber` (`crates/pharos-transcode/src/libav/probe.rs`) inspect the primary video stream using identical precedence rules:

1. **H.264 MVC codec/profile**:
   - `codec_name` equals `"mvc"` / `"h264_mvc"`, or `video_profile` is `"Stereo High"` or `"Multiview High"` (FFmpeg profile IDs `128` / `118`), or a secondary video stream in the container has codec `"mvc"`.
   - Maps to `Video3DFormat::Mvc`.
2. **Container tag `stereo_mode` (Matroska / WebM) and MP4/MOV `Stereo 3D` (`AV_PKT_DATA_STEREO3D`) side data**:
   - Side-by-side indicators: Matroska `stereo_mode` in `["left_right", "right_left", "1", "2", "11"]` or `AVStereo3D.type` == `"side by side"` (`AV_STEREO3D_SIDEBYSIDE` / `AV_STEREO3D_SIDEBYSIDE_QUINCUNX`).
   - Top-and-bottom indicators: Matroska `stereo_mode` in `["bottom_top", "top_bottom", "3", "4"]` or `AVStereo3D.type` == `"top and bottom"` (`AV_STEREO3D_TOPBOTTOM`).
   - Frame-sequential / alternate (`"block_lr"`, `"block_rl"`, `"frame alternate"`, `AV_STEREO3D_FRAMESEQUENCE`): maps to `Video3DFormat::Mvc`.
   - Anaglyph (`"anaglyph_cyan_red"`, `"anaglyph_green_magenta"`): logged and counted in `pharos_3d_detect_total{format="anaglyph",source="probe"}`, with `video_3d_format = None` because Jellyfin's `Video3DFormat` enum has no anaglyph variant and anaglyph frames are already pre-mixed for standard 2D displays (see Open Questions §Q1).
3. **Half vs Full aspect disambiguation for probe-detected SBS/TAB**:
   - For SBS: if coded aspect ratio `width / height >= 3.0` (for example `3840x1080`, ratio `3.55`) or filename explicitly contains `FSBS` / `Full-SBS` / `Full.SBS`, classify as `FullSideBySide`; otherwise classify as `HalfSideBySide` (for example `1920x1080`).
   - For TAB: if coded aspect ratio `width / height <= 1.0` (for example `1920x2160`, ratio `0.89`) or filename explicitly contains `FTAB` / `Full-TAB` / `Full.OU`, classify as `FullTopAndBottom`; otherwise classify as `HalfTopAndBottom` (for example `1920x1080`).

### Filename tag extraction (`filename.rs` and `fs.rs`)

`crates/pharos-scanner/src/metadata/filename.rs` exposes a pure function `pub fn parse_video_3d_format(input: &str) -> Option<Video3DFormat>` and updates `quality_source_tag` so 3D tokens terminate title extraction cleanly for both movies and TV episodes.

Token matching is case-insensitive on word/separator boundaries (`.`, `_`, `-`, ` `, `[`, `]`, `(`, `)`):

| Token pattern | Resolved `Video3DFormat` |
|---|---|
| `hsbs`, `h-sbs`, `half-sbs`, `half.sbs`, `sbs` (without `full`/`fsbs`) | `HalfSideBySide` |
| `fsbs`, `f-sbs`, `full-sbs`, `full.sbs` | `FullSideBySide` |
| `htab`, `h-tab`, `half-tab`, `half.tab`, `hou`, `h-ou`, `half-ou`, `tab`, `ou` (without `full`/`ftab`/`fou`) | `HalfTopAndBottom` |
| `ftab`, `f-tab`, `full-tab`, `full.tab`, `fou`, `f-ou`, `full-ou` | `FullTopAndBottom` |
| `mvc`, `3d.mvc`, `bluray3d`, `bd3d` | `Mvc` |
| Bare `3d` without layout token | Does not guess a layout on its own (`None`), unless paired with probe metadata; emits `pharos_3d_detect_total{format="unspecified_3d",source="filename"}` |
| `anaglyph` | Emits `pharos_3d_detect_total{format="anaglyph",source="filename"}`, returns `None` |

**Precedence in `FsScanner::probe_one` (`crates/pharos-scanner/src/fs.rs`)**:
1. If probe detected a specific `Video3DFormat`, keep the probe format, EXCEPT when probe saw generic SBS/TAB at standard `1920x1080` and the filename explicitly specifies `FSBS` or `FTAB` (or vice versa), where the explicit half/full filename qualifier refines the half/full distinction.
2. If `probe.video_3d_format` is `None`, fallback to `parse_video_3d_format(&file_stem_or_path)`.
3. This step runs inside `FsScanner::probe_one` before `MediaItem` assembly so both `MediaKind::Movie` and `MediaKind::Episode` receive `probe.video_3d_format`.

---

## Wire contract and DTO mapping

### `BaseItemDto`, `MediaSourceLiteDto`, `MediaSourceInfoDto`, and `MediaStreamDto`

All four structs in `crates/pharos-jellyfin-api/src/dto.rs` use `#[serde(rename_all = "PascalCase")]`. In `serde`, `PascalCase` converts `video_3d_format` into `"Video3dFormat"` (lowercase `d`), which violates Jellyfin's OpenAPI specification and breaks `jellyfin-sdk-kotlin` strict deserialization (`V1`, `V7`, `V38`). Every field MUST carry an explicit `#[serde(rename = "Video3DFormat", skip_serializing_if = "Option::is_none")]`:

- `BaseItemDto::video_3d_format: Option<Video3DFormat>` (populated in `BaseItemDto::from_item` from `item.probe.video_3d_format` for both `Movie` and `Episode` items).
- `MediaSourceLiteDto::video_3d_format: Option<Video3DFormat>` (populated in `media_sources_from_item`).
- `MediaSourceInfoDto::video_3d_format: Option<Video3DFormat>` (populated in `MediaSourceInfoDto::from_item` and `from_item_for_decision`).
- `MediaStreamDto::video_3d_format: Option<Video3DFormat>` (populated on the `Type: "Video"` stream entry in `media_streams_from_probe`, `None` on Audio/Subtitle/EmbeddedImage streams).

### `/Items` `Is3D` query filter

- `crates/pharos-store-sqlx/src/media_query.rs`: replace the stub `Some(true) => sql.push_str(" AND 1 = 0")` at lines 555-561 with:
  - `Some(true) => sql.push_str(" AND video_3d_format IS NOT NULL")`
  - `Some(false) => sql.push_str(" AND video_3d_format IS NULL")`
- Add partial index `CREATE INDEX IF NOT EXISTS idx_media_items_video_3d_format ON media_items (video_3d_format) WHERE video_3d_format IS NOT NULL;` in migration `0057` for both SQLite and PostgreSQL.

---

## Playback decision, capability negotiation, and transcode pipeline

### DirectPlay passthrough default and `ProfileCondition` support

1. **Default DirectPlay**: In `crates/pharos-server/src/api/jellyfin/device_profile.rs`, a 3D item (`source.video_3d_format.is_some()`) is eligible for `DirectPlay` and `DirectStream` by default.
2. **Condition evaluation**:
   - Extend `SourceMedia` (`device_profile.rs:115`) with `pub video_3d_format: Option<Video3DFormat>`.
   - Extend `evaluate_condition` (`device_profile.rs:257`) to recognise property names:
     - `"Is3D"`: compares `"true"` (when `source.video_3d_format.is_some()`) or `"false"` (when `None`) against `Condition` (`Equals`, `NotEquals`).
     - `"Video3DFormat"`: compares the canonical string (`"HalfSideBySide"`, etc.) against `Condition` (`Equals`, `NotEquals`, `EqualsAny`).
   - When a `CodecProfile` condition on `"Is3D"` or `"Video3DFormat"` fails, `decide_playback` rejects `DirectPlay` and `DirectStream`, sets transcode reason `Video3DFormatNotSupported`, and selects `StereoOutput::Mono2D`.
3. **Explicit client override (`StereoMode` query param)**:
   - `/Items/{id}/PlaybackInfo`, `/Videos/{id}/master.m3u8`, `/Videos/{id}/main.m3u8`, `/Videos/{id}/hls/{segment}`, and `/Videos/{id}/stream` accept an optional query parameter `StereoMode` (`"preserve"` | `"mono"`).
   - If `StereoMode=mono` is passed OR `decide_playback` failed an `Is3D`/`Video3DFormat` condition OR subtitles are being burned into a packed SBS/TAB stream (`burn_sub.is_some()` and `video_3d_format` is SBS/TAB), the transcode uses `StereoOutput::Mono2D`.
   - Otherwise, transcoding uses `StereoOutput::Preserve`.

### Transcode filter chain (`StereoOutput`)

Define `StereoOutput` in `crates/pharos-transcode/src/lib.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum StereoOutput {
    #[default]
    Preserve,
    Mono2D,
}
```

Add `pub video_3d_format: Option<Video3DFormat>` and `pub stereo_output: StereoOutput` to `TranscodeOptions` (`crates/pharos-transcode/src/lib.rs:278`) and `SegmentOpts` (`crates/pharos-transcode/src/hls.rs:110`).

#### 1. `StereoOutput::Preserve` (default for 3D-capable clients when transcoding bitrate/codec)
- Do NOT crop or rescale to a single eye.
- Keep the full frame dimensions and sample aspect ratio (`SAR`).
- Because Pharos HLS variant rungs (`crates/pharos-server/src/api/jellyfin/hls.rs:1090-1098`) vary `video_bitrate_bps` without scaling dimensions, packed SBS/TAB frame layout remains intact across all bitrate rungs. If any future spatial downscale is applied under `Preserve`, dimensions must remain even-aligned per eye (divisible by 4 horizontally for SBS, vertically for TAB).

#### 2. `StereoOutput::Mono2D` (left-eye extraction for 2D clients and subtitle burn-in)
Helper `stereo_mono_filter(format: Video3DFormat) -> Option<&'static str>` in `crates/pharos-transcode/src/hls.rs`:

| `Video3DFormat` | CPU pre-filter string | Output geometry from `1920x1080` / `3840x1080` / `1920x2160` |
|---|---|---|
| `HalfSideBySide` | `"crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1"` | Left half `960x1080` scaled to `1920x1080`, `SAR 1:1` |
| `FullSideBySide` | `"crop=iw/2:ih:0:0,setsar=1"` | Left half `1920x1080`, `SAR 1:1` |
| `HalfTopAndBottom` | `"crop=iw:ih/2:0:0,scale=iw:ih*2,setsar=1"` | Top half `1920x540` scaled to `1920x1080`, `SAR 1:1` |
| `FullTopAndBottom` | `"crop=iw:ih/2:0:0,setsar=1"` | Top half `1920x1080`, `SAR 1:1` |
| `Mvc` | `None` | Base H.264 view is already `1920x1080` 2D left eye |

**Hardware acceleration and filter ordering (`hls.rs`, `ffmpeg.rs`, `libav/transcode.rs`)**:
- In `hls.rs::append_video_Codec_args`: when `stereo_mono_filter` returns `Some(mono_vf)`:
  - For CPU decode + VAAPI/QSV hardware encode (`hwupload` path at `hls.rs:884-891`): prepend `mono_vf` in software BEFORE `format=nv12,hwupload` (`{mono_vf},format=nv12,hwupload`).
  - For CUDA hardware decode (`-hwaccel cuda -hwaccel_output_format cuda` at `hls.rs:334-340`): when `mono_vf` is active (analogous to `burn_image` at `hls.rs:766`), omit `-hwaccel_output_format cuda` so decoded frames stay in system memory for the CPU `crop`/`scale` filter before `h264_nvenc` encode.
  - For software encode and VideoToolbox: prepend `mono_vf` at the start of the `-vf` chain (before subtitle `overlay` or `subtitles=` burn-in so burned subtitles are rendered onto the 2D single-eye frame!).
  - Clear container/stream `stereo_mode` metadata on output via `-metadata:s:v:0 stereo_mode=` when `StereoOutput::Mono2D` is active so players do not re-split an already-cropped 2D stream.

#### 3. Thumbnails (`image.rs`) and trickplay (`trickplay.rs`)
- `extract_video_frame` (`crates/pharos-transcode/src/image.rs`) and `generate_sprites` (`crates/pharos-transcode/src/trickplay.rs`) accept `video_3d_format: Option<Video3DFormat>`.
- When `video_3d_format` is `Some(fmt)` and `stereo_mono_filter(fmt)` is `Some(mono_vf)`, prepend `mono_vf` before `scale={width}:-2` (for example `crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1,scale=320:-2`).

### HLS cache isolation (`V132`, `V182`)

In `crates/pharos-server/src/hls_cache.rs`:
- Add `pub stereo_output: pharos_transcode::StereoOutput` to `RenditionKey` (`L128`) and `SegmentIdentity` (`L1152`).
- In `SegmentIdentity::cache_file_name` (`L1178`) and `segment_etag` (`hls.rs:285`): when `stereo_output == StereoOutput::Preserve` (default), emit no extra suffix so existing 2D and default 3D cache keys remain byte-for-byte unchanged; when `stereo_output == StereoOutput::Mono2D`, append `_mono2d` before the extension.

---

## Invariants and bug-class guards

Appended after baseline `V167` and `B224`:

- **V180** (`pharos-jellyfin-api`): Every `Video3DFormat` field on `BaseItemDto`, `MediaSourceLiteDto`, `MediaSourceInfoDto`, and `MediaStreamDto` carries `#[serde(rename = "Video3DFormat", skip_serializing_if = "Option::is_none")]` and serializes to exact PascalCase `"Video3DFormat"` (capital `3D`), never `"Video3dFormat"`.
- **V181** (`pharos-scanner`, `pharos-store-sqlx`): `PROBE_SCHEMA_VERSION` is `7`; both `FfmpegProber` and `LibavProber` produce identical `Option<Video3DFormat>` for Matroska `stereo_mode`, MP4 `Stereo 3D` side data, and H.264 MVC profiles, and `FsScanner::probe_one` applies filename fallback for both `MediaKind::Movie` and `MediaKind::Episode`.
- **V182** (`pharos-server`, `pharos-transcode`): `SegmentIdentity` and `RenditionKey` include `StereoOutput`; a `StereoOutput::Preserve` transcode and a `StereoOutput::Mono2D` transcode of the same item never share cached HLS segments or ETags (`V132` extension).
- **V183** (`pharos-transcode`): Whenever `StereoOutput::Mono2D`, subtitle burn-in on packed 3D, thumbnail extraction (`image.rs`), or trickplay sprite generation (`trickplay.rs`) runs on a `HalfSideBySide`, `FullSideBySide`, `HalfTopAndBottom`, or `FullTopAndBottom` source, the left-eye crop filter runs before any spatial scale, subtitle overlay, or hardware surface upload.
- **B238**: `#[serde(rename_all = "PascalCase")]` silently renames `video_3d_format` to `"Video3dFormat"`, causing `jellyfin-sdk-kotlin` and Jellyfin Web to ignore the 3D format. Guarded by `V180` and `jellyfin_playbackinfo_kotlin_strict.rs`.
- **B239**: Burning subtitles (`PGS`/`ASS`/`SRT`) onto a packed SBS/TAB stream without first cropping to 2D splits the subtitle across the left and right eye halves. Guarded by `V183` (forcing `StereoOutput::Mono2D` when burning subtitles on packed 3D).
- **B240**: Using `-hwaccel_output_format cuda` when `StereoOutput::Mono2D` is active passes GPU-resident `cuda` frames to the CPU `crop`/`scale` filter and fails ffmpeg startup. Guarded by dropping `-hwaccel_output_format cuda` whenever `stereo_mono_filter` is `Some(_)`, matching the existing `burn_image` guard at `hls.rs:766`.

---

## Observability (ODD per `CLAUDE.md`)

All metrics use bounded label sets (`format` in `{"HalfSideBySide","FullSideBySide","HalfTopAndBottom","FullTopAndBottom","MVC","anaglyph","unspecified_3d"}`, `source` in `{"probe_tag","probe_sidedata","probe_mvc","filename"}`, `stereo_output` in `{"preserve","mono2d","direct"}`).

| Signal | Type | Labels / Fields | Purpose |
|---|---|---|---|
| `pharos_3d_detect_total` | Counter | `format`, `source`, `media_kind` (`movie`\|`episode`) | Counts 3D items detected during library scan by format and detection source |
| `pharos_playback_3d_decision_total` | Counter | `format`, `decision` (`direct_play`\|`direct_stream`\|`transcode`), `stereo_output` (`preserve`\|`mono2d`), `reason` | Tracks how 3D media is served across clients |
| `tracing::info!(target: "pharos::scanner::3d", ...)` | Structured log | `item_id`, `video_3d_format`, `detect_source`, `media_kind` | Emitted when a 3D item is detected during scan |
| `tracing::info!(target: "pharos::playback::3d", ...)` | Structured log | `item_id`, `video_3d_format`, `decision`, `stereo_output`, `reason` | Emitted on `PlaybackInfo` and HLS stream startup for 3D items |

**Verification queries**:
- PromQL detection rate by source: `sum by (format, source) (rate(pharos_3d_detect_total[5m]))`
- PromQL 3D playback breakdown: `sum by (format, decision, stereo_output) (rate(pharos_playback_3d_decision_total[5m]))`
- LogQL scan audit: `{app="pharos"} | json | target="pharos::scanner::3d"`
- LogQL 2D fallback audit: `{app="pharos"} | json | target="pharos::playback::3d" | stereo_output="mono2d"`

---

## Risks and open questions (with assumed answers)

1. **Q1: How should Pharos represent `anaglyph` 3D files given Jellyfin's `Video3DFormat` enum only has `HalfSideBySide`, `FullSideBySide`, `FullTopAndBottom`, `HalfTopAndBottom`, and `MVC`?**
   - *Risk*: Adding a non-standard `"Anaglyph"` enum variant breaks `jellyfin-sdk-kotlin` strict enum deserialization (`V38`/`V39`). Moreover, anaglyph video is already colour-multiplexed into a single 2D frame and cannot be spatially cropped into a single eye without severe colour distortion.
   - *Assumed answer*: Keep `Video3DFormat` strictly limited to Jellyfin's 5 wire variants. Detect `anaglyph` in probe/filename to strip the token from parsed titles and increment `pharos_3d_detect_total{format="anaglyph",source=...}`, while storing `video_3d_format = None` so clients play the pre-mixed frame as standard 2D video without spatial cropping.

2. **Q2: How should a bare `3D` filename token (e.g. `Movie (2012) 3D 1080p.mkv`) be handled when the container has no `stereo_mode` tag, no `Stereo 3D` side data, and no `MVC` profile?**
   - *Risk*: Guessing `HalfSideBySide` when the file is actually `HalfTopAndBottom` or `MVC` would cause `StereoOutput::Mono2D` and thumbnail extraction to crop the wrong half of the frame.
   - *Assumed answer*: Strip `3D` as a quality boundary token in `filename.rs` so title parsing stays clean, and record `pharos_3d_detect_total{format="unspecified_3d",source="filename"}`, but only set `video_3d_format: Some(...)` from filename alone when an explicit layout token (`SBS`, `HSBS`, `FSBS`, `TAB`, `HTAB`, `FTAB`, `OU`, `HOU`, `FOU`, `MVC`, `BD3D`) is present.

3. **Q3: What happens when subtitles must be burned into a 3D video during transcoding?**
   - *Risk*: Burning a 2D subtitle overlay across a packed `HalfSideBySide` or `HalfTopAndBottom` frame puts the left half of each sentence in the left eye and the right half in the right eye (`B239`). Rendering stereoscopic dual-eye subtitles requires per-eye subtitle duplication not supported by standard `subtitles`/`overlay` filters.
   - *Assumed answer*: When subtitle burn-in is required on a packed SBS/TAB item (`burn_sub.is_some()`), automatically switch `stereo_output` to `StereoOutput::Mono2D` so the video is cropped to the left eye first and the burned subtitle renders cleanly on a 2D frame; log `reason="subtitle_burn_mono_fallback"` on `pharos::playback::3d`. (Text subtitles delivered out-of-band as WebVTT/SRT remain `StereoOutput::Preserve` or `DirectPlay` so 3D players can render them natively.)

4. **Q4: How do we distinguish `HalfSideBySide` vs `FullSideBySide` (and `HalfTopAndBottom` vs `FullTopAndBottom`) when Matroska `stereo_mode` only says `"left_right"` or `"top_bottom"`?**
   - *Risk*: Cropping `FullSideBySide` (`3840x1080`) with `scale=iw*2:ih` would stretch the output to `3840x1080` (`32:9`).
   - *Assumed answer*: Use the coded frame aspect ratio (`width / height >= 3.0` -> `FullSideBySide`; `width / height <= 1.0` -> `FullTopAndBottom`; standard `~16:9` -> `Half*`), overridden by explicit filename tokens (`FSBS`/`Full-SBS` vs `HSBS`/`Half-SBS`, `FTAB`/`Full-TAB` vs `HTAB`/`Half-TAB`).
