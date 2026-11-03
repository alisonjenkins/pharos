# Feature specification: image subtitles drawn by the native player

**Feature Branch**: `feat/ui-image-subtitles`
**Created**: 2026-11-03T18:50:00Z
**Status**: Ready
**Scope**: The native web player (`/ui/`, 013/017) draws DVD (`dvd_subtitle`), Blu-ray (`hdmv_pgs_subtitle`) and DVB (`dvb_subtitle`) image subtitles itself, over a direct-played video, instead of the server burning them into a transcode. jellyfin-web and every Jellyfin-wire client keep today's burn-in; their `SubtitleProfiles` handling is unchanged.

---

## Problem statement

013 FR-003 left image subtitles to burn-in. Turning one on in the native player therefore renegotiates the whole playback into an HLS transcode (`downgrade=subtitle_burn`): the picture is re-encoded, the stream restarts, quality drops to the transcode ceiling, and the server spends a GPU/CPU encode for the whole session. Turning it off renegotiates again. Fringe S01E01 (2026-11-03, B293) is the case that surfaced it: its only subtitles are `dvd_subtitle`.

## User story 1 - Turn on an image subtitle without leaving direct play (Priority: P1)

A viewer watching a direct-played film with DVD or Blu-ray subtitles picks one in the Subtitles menu. The subtitle appears over the video at its authored place and size, timed to the picture, and the video keeps playing the same stream. Picking "off" or another track takes effect at once.

**Independent test**: the Dioxus e2e on a fixture with a `dvd_subtitle` track: picking it keeps the original play session (no new stream request), and the overlay shows an `<img>` for the event under the play head.

**Acceptance scenarios**:

1. **Given** a native client that declares image-subtitle drawing, **When** it negotiates an item whose selected subtitle is an image codec, **Then** the decision is not downgraded for the subtitle (`downgrade` is not `subtitle_burn`), and the track's delivery is `image` with a manifest URL.
2. **Given** the player shows an image track, **When** the play head is inside an event, **Then** that event's bitmap is drawn over the video's content box, scaled from the subtitle canvas to the displayed picture (letterboxing and anamorphic display aspect included); outside every event nothing is drawn.
3. **Given** a client that does not declare image-subtitle drawing (jellyfin-web, an older native build), **When** it negotiates the same item, **Then** behaviour is unchanged: burn-in.

## User story 2 - Cold extraction does not block playback (Priority: P1)

The bitmaps come from one whole-file demux and decode of the subtitle stream (about 30 s cold over NFS). Extraction runs on demand, background-I/O gated, once per file and stream; its result is cached on disk. The manifest is served as extraction progresses (`complete: false`, `through_ms`), so cues near the start show within seconds and the player re-polls until complete.

**Acceptance scenarios**:

1. **Given** a cold cache, **When** the manifest is first requested, **Then** extraction starts once (concurrent requests share it) and the response is a partial manifest, never an error; the player polls it while `complete` is false.
2. **Given** a completed extraction, **When** the file is unchanged, **Then** later requests serve from the cache with no demux; a changed mtime or a bumped `IMAGE_SUB_GEN_VERSION` re-extracts.
3. **Given** a library scan or startup warm, **Then** no image-subtitle extraction runs (B49/B52).

## Requirements

- **FR-001** `Capabilities` (native API) gains `imageSubtitles :List(Text)`: the image codecs the player draws. The web player declares `dvd_subtitle`, `hdmv_pgs_subtitle`, `dvb_subtitle`.
- **FR-002** Negotiation treats a declared image codec as externally delivered; text subtitles keep External as today.
- **FR-003** `SubtitleDelivery` gains `image @2`; `Track.url` is then the manifest URL `/videos/{id}/subtitles/{index}/images.json`.
- **FR-004** The manifest is JSON: `{ "width", "height", "complete", "through_ms", "events": [{ "start_ms", "end_ms", "x", "y", "width", "height", "forced", "url" }] }`. `width`/`height` are the subtitle canvas (the source's coded size for DVD, the PGS presentation size). Bitmaps are PNG (RGBA, palette applied) at `/videos/{id}/subtitles/{index}/images/{n}.png`.
- **FR-005** An event's end is its decoded `end_display_time` when non-zero, else the next event's start (PGS clears with an empty event), capped at 15 s for a final event without one.
- **FR-006** Routes require item access like the WebVTT route; a bitmap not yet extracted answers 404.
- **FR-007** Signals: `pharos_subtitle_image_extract_total{outcome}` (`complete`, `failed`), the extraction log line with events, duration and bytes, and `subtitle.delivery=Image` in `playbackinfo subtitle selection`. Live check: `{namespace="pharos"} |= "playbackinfo subtitle selection" |= "dvd_subtitle"` shows `Some(Image)` and the following `native playback negotiated` shows `downgrade="none"`.

## Out of scope

jellyfin-web client-side PGS (its own `pgssub` path is untouched); OCR to text; styling image subtitles (they are pictures).
