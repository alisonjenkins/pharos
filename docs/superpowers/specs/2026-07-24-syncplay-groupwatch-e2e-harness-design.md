# SyncPlay Group-Watch E2E Harness — Design

**Date:** 2026-07-24
**Status:** Approved (brainstorming) — pending implementation plan
**Author:** Alison + Claude

## Problem

SyncPlay (group watch) is *the* key feature for Alison and friends, yet
regressions keep reaching production and only surface during live group
watches ("she joined but the video never popped up", "audio swap wedged the
group"). The existing automated coverage cannot catch these because:

- `syncplay.spec.ts` drives the **raw WebSocket** command path — it never
  loads jellyfin-web's real SyncPlay Manager, so it stays green while the
  browser is wedged.
- `syncplay-realclient.spec.ts` loads the real Manager but is **single
  browser** — it cannot test multi-member join / sync / propagation.
- `syncplay-two-users.spec.ts` uses two contexts but again drives **raw
  WebSocket**, asserting command *delivery*, not that a `<video>` actually
  plays or stays in sync.
- All bundled Playwright browsers are **FOSS builds with no h264 decode**
  (`DEMUXER_ERROR_NO_SUPPORTED_STREAMS`). jellyfin-web on them advertises
  VP9, so pharos serves the VP9 rung. Alison + friends run **Firefox/Zen**,
  which advertise h264, so pharos serves the newer **demuxed-h264 CMAF**
  path. No existing test exercises that path in a real browser — which is
  exactly the class that wedged the group in PR#70.

## Goal

A harness that spins up **up to 3 "virtual people"** — each a *real*
jellyfin-web instance with the real SyncPlay Manager and a real `<video>`
element — drives them through every group-watch scenario, and asserts with
confidence whether a **real browser actually starts playing and stays in
sync**.

## Scope: scenario matrix

Each scenario is a distinct test on a fresh group. All members are real
jellyfin-web contexts.

1. **Solo-play then late join.** A creates group, starts playback with no
   one else present, then B (and C) join. Joiner must promptly reach the
   same item and be *playing* — no forced pause, no wedge at `currentTime 0`.
2. **Empty group, join, then pick playback.** Group created empty. B joins.
   Then an item is chosen to start. Playback must start on *all* members.
3. **Next / previous episode.** In a group playing a series episode, the
   Next (then Previous) control must promptly start playback of the new
   episode on *all* members.
4. **Pause propagation.** Pause pauses everyone.
5. **Resume in sync.** Play resumes everyone, re-converged to the same
   point.
6. **Seek propagation.** Seek moves everyone to the same point.
7. **Audio / subtitle swap without desync.** Switching audio track or
   subtitle track on one member must not push any member out of sync.

## Non-goals

- Native (Android TV / kotlin) clients — covered separately by the DTO /
  socket conformance suites.
- Replacing the existing raw-socket conformance specs — they stay as the
  fast protocol gate. This harness is the *browser-truth* layer above them.
- SyncPlay across multiple pharos replicas (single-replica assumption holds
  as elsewhere).

## Architecture

### Topology — N virtual people

One Playwright process, **N browser contexts** (default **3**). Each context
has isolated storage, so it logs in as a distinct seeded user
(`playwright`, `playwright2`, `playwright3`) with a distinct deviceId, and is
therefore a distinct SyncPlay member. Each context runs the unmodified
jellyfin-web bundle → the real SyncPlay Manager → a real `<video>`. Browser
*contexts* (not full browser launches) keep 3 members cheap.

### `VirtualPerson` helper — `compat-playwright/tests/lib/virtual-person.ts`

A class wrapping one context+page with the operations the matrix needs.
Control goes through the app's **own** entry points via `page.evaluate` so
the real Manager runs and selectors do not rot:

- `connect()` / `login(user, pass)` — extracted from the existing specs.
- `createGroup(name)` / `joinGroup(groupId)` — real
  `ApiClient.createSyncPlayGroup` / join.
- `play(itemId)` — via the real `playbackManager` (group-routed once in a
  group). One scenario additionally clicks the real `button.btnPlay` to
  prove the UI button path.
- `pause()`, `unpause()`, `seek(positionTicks)`, `nextTrack()`,
  `prevTrack()`, `setAudio(index)`, `setSubtitle(index)` — via
  `playbackManager` / SyncPlay APIs.
- `probe()` → `{ itemId, currentTime, paused, currentSrc, readyState,
  audioIndex, textIndex, errorCode }` read off the live `<video>` + Manager.
- Per-person capture of console lines and `/SyncPlay|/PlaybackInfo|/Playing`
  requests, surfaced in assertion failure messages for diagnosis.

### Sync oracle — `assertInSync` / `waitUntilInSync`

Samples all N `probe()`s in one tight `Promise.all` (minimises skew), then
asserts, with **condition-based polling** (no flat sleeps):

- all members report the **same `itemId`**;
- **paused-state agreement** across members;
- when playing, **pairwise `|Δ currentTime| ≤ 1.5 s`** after a settle window
  (poll up to 8 s for convergence);
- after **seek**, all members within **1.0 s** of the target within 5 s;
- after **pause**, all `paused === true` within 2 s;
- after **resume**, all playing and re-converged to ≤ 1.5 s.

Tolerances are named constants at the top of the spec so they are tunable in
one place. Rationale: 1.5 s absorbs transcode-segment + time-sync jitter
while still catching the multi-second drift/wedge that the real bugs produce.

### VP9 matrix vs h264 smoke (codec fidelity)

Chosen approach: **both**, split by concern.

- **`syncplay-group-matrix.spec.ts` — bundled chromium (VP9).** Runs the
  full 7-scenario matrix. Fast, deterministic, fully offline. Proves the
  SyncPlay *orchestration* (join, propagation, sync) with real `<video>`
  playback. Structurally VP9 because the bundled browser cannot decode h264.
- **`syncplay-h264-codec.spec.ts` — real-codec browser (h264).** A single
  focused smoke: a proprietary-codec Chromium (h264-capable) drives 2
  members on an h264 source, asserting the **demuxed-CMAF** path actually
  decodes (`currentTime > 0`, no `video.error`) and that an **audio swap
  reuses the cached video** with no desync. This is the guard for the PR#70
  class of bug that the VP9 matrix cannot see. Software libx264 encode is
  fine here (CI VM has no GPU); the point is browser-side decode + swap, not
  encoder path.

The real-codec browser is pulled from nix (a proprietary-codec Chromium /
`google-chrome`, resolved during planning). It stands in for Firefox/Zen on
the *codec* axis — an offline Firefox lacks the runtime-downloaded OpenH264,
so a codec-capable Chromium is the practical h264 real-browser.

### Seed extensions — `seed_playwright_user` (`crates/pharos-server/src/main.rs`)

The current seed produces four single-track VP9/Opus items with no series
structure. Add:

- **A 3-episode linked series** (Series → Season → 3 ordered Episodes) so the
  Next/Previous scenario has real adjacency to navigate. VP9/Opus so the
  chromium matrix decodes it.
- **A dual-audio + 2-subtitle clip** (2 audio tracks + 2 embedded text
  subtitle tracks) for the audio/subtitle-swap scenario. VP9/Opus video.
- Two additional users (`playwright2` already exists for B53; add
  `playwright3`) so 3 distinct members are available.

Existing items are preserved so the current crawl / console specs are
unaffected.

### Wiring

- **Local manual recipe:** `just compat-syncplay` — extends the
  `compat-playwright-full` bootstrap (build `transcode-worker`, write the
  temp config, seed the user + media + 3 users, serve the nix-pinned
  jellyfin-web bundle via `http-server --proxy`), then runs *only* the two
  syncplay specs (matrix + h264 smoke). Manual because it is heavy (3 real
  browsers + live transcode).
- **Path-filtered CI:** a **new** workflow `.github/workflows/syncplay-e2e.yml`
  (separate from `ci.yml` because GitHub `on.pull_request.paths` filters the
  whole workflow, and `ci.yml` must run on every PR). Triggers:
  - `pull_request` with a `paths:` filter over the group-play blast radius;
  - `workflow_dispatch` for on-demand manual runs.

  It reuses the runner label `pharos-nix-builder-amd64` and
  `alisonjenkins/setup-host-nix@v1`, mirrors the existing `compat-playwright`
  job's browser handling (nix-pinned, no `npx playwright install`), and
  uploads the Playwright report on failure. Not a required check, so a
  `paths`-skipped run never blocks merges.

  **Path filter (group-play blast radius):**
  - `crates/pharos-sync/**`
  - `crates/pharos-server/src/api/jellyfin/syncplay.rs`
  - `crates/pharos-server/src/api/jellyfin/hls.rs`
  - `crates/pharos-server/src/sessions.rs`
  - `crates/pharos-transcode/**`
  - `crates/pharos-cache/src/hls_cache.rs`
  - `crates/pharos-server/src/main.rs` (the seed)
  - `compat-playwright/tests/syncplay-*.spec.ts`
  - `compat-playwright/tests/lib/**`
  - `.github/workflows/syncplay-e2e.yml`
  - `justfile`

## Testing strategy

- The harness *is* the test. Its own correctness is validated by: each
  scenario failing loudly (rich diagnostics) when the corresponding server
  behaviour is broken, and passing against current `main`.
- The `VirtualPerson` helper and `assertInSync` oracle are exercised by
  every scenario; no separate unit layer is warranted (YAGNI).
- Seed changes are covered by the specs consuming them (a missing series /
  dual-audio item makes the relevant scenario fail at setup with a clear
  message).

## Risks / mitigations

- **Flakiness from real transcode + 3 browsers.** Mitigate with
  condition-based polling (never flat sleeps), generous but bounded settle
  windows, and per-person diagnostic capture. CI `retries: 2` as elsewhere.
- **VP9 matrix blind to h264.** Explicitly mitigated by the h264 smoke.
- **Real-codec browser availability offline.** Resolved by pulling a
  codec-capable Chromium from nix during planning; falls back to documenting
  the requirement if nix cannot provide one, but the h264 smoke is the whole
  point of this design so it is not optional.
- **CI runner has no GPU.** Software encode is acceptable — the harness
  tests browser decode + orchestration, not the encoder.

## Open questions

None blocking. Exact nix attribute for the codec-capable Chromium is an
implementation detail resolved in the plan.
