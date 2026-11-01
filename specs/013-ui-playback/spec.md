# Feature Specification: playback and reading in pharos-ui

**Feature directory**: `specs/013-ui-playback/`
**Created**: 2026-11-01
**Status**: draft (`/speckit-specify` stage; next: `/speckit-clarify`)
**Input**: "It must support all the things Jellyfin's UI does and more."
(Alison, 2026-11-01)
**Depends on**: 009 (shell, mini-player slot, websocket), 011 (player keys),
ADR-0026 (capability negotiation), ADR-0027 (unified seek model), 002 (skip
segments), 004/005 (books), ADR-0019 (VR)
**API**: pharos-ui calls the pharos-native API (017, ADR-0028). Jellyfin
endpoints named below identify the existing server behaviour that a native
method wraps through a shared domain service (V172); they are not what the UI
calls. Jellyfin clients keep using them unchanged.
**Supersedes**: T57, T26 phase rows; the "Out of scope" note in 008.

## Problem

`player.rs` is a bare `<video src=…>` with no MSE or hls.js, so desktop Chrome
and Firefox cannot play any transcoded item, cannot switch audio tracks, and
cannot change quality. There is no music player, no queue and no book reader.

## User Stories

### US1 — any video plays in any supported browser (P1)

The player negotiates with `POST Items/{id}/PlaybackInfo` using a DeviceProfile
built from the browser's real capabilities (`MediaSource.isTypeSupported`,
`mediaCapabilities.decodingInfo`, HDR/display probes), then plays DirectPlay,
DirectStream or HLS (fMP4/CMAF and TS) through MSE.

**Independent test**: Playwright on Chromium and Firefox plays a file needing a
transcode (HEVC 10-bit) and one that direct-plays (H.264 MP4); both report
`Sessions/Playing` and progress.

**Decided (Alison, 2026-11-01)**: v1 embeds **hls.js** via wasm-bindgen — the
client the server is already tested against. Long-term goal: replace it with
an **all-Rust/WASM HLS→MSE engine** that improves on hls.js. To keep that
swap cheap, v1 MUST put hls.js behind the `PlaybackEngine` port (009 §Boundaries) (load, seek,
select audio/subtitle/quality, buffer and error events, stats) that the player
UI, queue, SyncPlay (014) and telemetry (016) talk to exclusively; no hls.js
type or event name leaks past the trait. The 016 playback timeline is the
yardstick a future Rust engine must beat (start time, stall rate, seek
latency, ABR switch quality, measured on the same sessions). The Rust engine
gets its own spec when work starts.

### US2 — player controls (P1)

Full-screen-first player with: play/pause, seek bar with chapter ticks and
trickplay thumbnails (GPU-drawn), time/ends-at, volume, speed, audio track,
subtitle track (with off/forced), subtitle style (size, colour, background,
position — from user prefs), quality/bitrate (auto + fixed), aspect/zoom,
picture-in-picture, fullscreen, next/previous, episode list drawer, stats
overlay. Chrome auto-hides; all keys per 011 US4.

### US3 — skip intro / outro / recap / preview (P1)

Detected `MediaSegments` show a "Skip intro" button at the right time; per-user
preference: ask / auto-skip / never, per segment type. Credits trigger an
up-next card with countdown to the next episode.

### US4 — resume and progress (P1)

Resume prompt (resume / start over) honours the user's resume threshold;
progress reported per Jellyfin cadence; mark-played threshold honoured; works
with the unified seek model across DirectPlay and HLS (ADR-0027).

### US5 — live TV (P2)

Play a channel through HLS, channel up/down keys, mini-guide overlay with
now/next, "live" pill and seek-within-buffer where the server allows.

### US6 — read a book (P1)

In-app readers for epub (paginated + scroll, TOC, font/size/line-height/margins
from theme, light/sepia/dark), PDF (page/fit-width/zoom), comics cbz/cbr/cbt/cb7
(single/double page, right-to-left manga mode, fit modes, preloading) and
converted Kindle books (005, delivered as epub). Progress syncs via
`Sessions/Playing/Progress` or user data so Continue Reading works across
clients (004 US5). Keyboard: arrows/space page turn, `t` TOC, `+/-` zoom.

`[NEEDS CLARIFICATION: reuse epub.js / pdf.js / libarchive.js as jellyfin-web
does (fast, proven), or Rust renderers? Recommendation: reuse the JS libraries
via wasm-bindgen, styled by tokens.]`

### US7 — music player and queue (P1)

Persistent mini-player in the shell (009 FR-008) that survives navigation;
expanded now-playing view with art, waveform scrubber (server waveform
endpoint), lyrics (from `.lrc` sidecars when present), queue panel with drag
reorder, shuffle, repeat one/all, play next/add to queue, gapless where the
browser allows, optional GPU visualiser themed by 010.

### US8 — audiobooks and podcasts-like audio (P3)

Chapter list, ±30 s skip, speed, sleep timer, per-item resume.
`[NEEDS CLARIFICATION: are audiobooks in the library today? MediaKind has no
audiobook kind — treat as Audio with chapters?]`

### US9 — VR (P3)

On a WebXR-capable browser (Meta Quest Browser and the Steam Frame browser are
the v1 targets, 009 NFR-005), "Watch in VR" hands the current stream to the
vendored jellyfinvr player (ADR-0019) or an equivalent WebXR view, keeping
playback position.

### US10 — explain playback (P2, pharos-only)

The stats overlay shows the server's decision: DirectPlay/DirectStream/
Transcode, why (the `direct_play_block` / `downgrade` reasons from
PlaybackInfo), the encoder/device chosen, segment cache hit rate, buffer
health, dropped frames. A "Report a playback problem" button posts the overlay
snapshot plus `PlaySessionId` to the client-log endpoint.

## Functional Requirements

- **FR-001** DeviceProfile builder from live browser capability probes; cached
  per device; overridable by user max-bitrate and "prefer direct play".
- **FR-002** MSE playback engine behind the `PlaybackEngine` trait (US1 decision) with ABR, seek, audio-
  track switch (re-negotiate or alternate rendition), error recovery, and
  `PlaySessionId` / `tag` handling as the server expects (B75).
- **FR-003** Subtitles: WebVTT rendered by the DOM layer with token-styled cues;
  ASS/SSA via a JS renderer (libass-wasm/JASSUB) `[NEEDS CLARIFICATION]`;
  image subs (PGS) via burn-in as the server decides today.
- **FR-004** Trickplay sprites fetched from `Videos/{id}/Trickplay/...` and
  uploaded as GPU textures for zero-latency scrubbing.
- **FR-005** MediaSegments client (`GET MediaSegments/{itemId}`), with per-type
  user preference stored with user settings.
- **FR-006** Playback reporting: `Sessions/Playing`, `/Progress`, `/Stopped`,
  plus remote-control commands received over the 009 websocket (014 US3).
- **FR-007** Queue model shared by video and music: play now/next/last, shuffle,
  repeat, persisted per session, exposed to 014 for group queues.
- **FR-008** Readers per US6 with position model = (format-specific locator,
  percentage), mapped to the server's progress field.
- **FR-009** Media Session API integration (011 FR-009) and wake lock during
  video.
- **FR-010** Instrumentation per CLAUDE.md ODD: the client logs every play
  decision it receives and every playback failure with item, session, method,
  error code and the last 10 s of buffer state — failure path as rich as
  success.

## Success criteria

- **SC-001** Playwright: transcode + direct-play matrix passes on Chromium and
  Firefox; audio-track switch and subtitle switch verified by media state.
- **SC-002** Skip intro appears within 0.5 s of segment start.
- **SC-003** Each reader opens the 004 fixtures, turns a page, and resumes at
  the same locator after reload.
- **SC-004** Mini-player survives 20 route changes without an audio gap.
- **SC-005** After deploy, `pharos_playback_decision_total` shows pharos-ui
  sessions and their decision mix (verify by query).

## Edge cases

- Browser claims HEVC but fails decode → fall back to transcode once, remember
  per device (relates to T130).
- Token expiry mid-stream; server restart mid-stream; seek past end.
- 55 external subtitle tracks (BACKLOG 0a) — list lazily, fetch only the chosen one.
- Tab hidden: keep audio, stop GPU work.
