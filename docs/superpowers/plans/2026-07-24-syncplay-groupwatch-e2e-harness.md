# SyncPlay Group-Watch E2E Harness — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Automated proof that up to 3 real jellyfin-web browsers actually start playing and stay in sync through every SyncPlay group-watch scenario.

**Architecture:** One Playwright process spins N browser *contexts* (default 3), each an isolated jellyfin-web login = a distinct SyncPlay member running the real SyncPlay Manager + real `<video>`. A `VirtualPerson` helper drives group create/join via `window.ApiClient`, issues group commands (pause/seek/next/prev/play-queue) via each person's own authenticated `/SyncPlay/*` REST call (identical wire effect to the OSD), and drives audio/subtitle swaps through the real OSD menus. A `sync-oracle` samples every `<video>.currentTime` in one tick and asserts convergence within tolerance. The 7-scenario matrix runs on the FOSS nix-pinned chromium (VP9 — the codec that browser advertises); a separate h264 smoke runs on `pkgs.chromium` (h264-capable) to guard the demuxed-CMAF path real Firefox/Zen use.

**Tech Stack:** Playwright (`@playwright/test` 1.59.1, nix-pinned browsers), TypeScript, Rust (seed), Nix (devShell), GitHub Actions, `just`.

## Global Constraints

- **Nix devShell only.** Every Rust/ffmpeg/Playwright command runs under `nix develop --command …`. Never invoke host `cargo`/`ffmpeg`/`node`.
- **Playwright browsers come from nix** (`PLAYWRIGHT_BROWSERS_PATH`), never `npx playwright install`. `@playwright/test` stays pinned to `playwright-driver.version`.
- **Condition-based waiting only** — poll for a predicate; never a flat `sleep`/`waitForTimeout` as a synchronisation primitive (a short settle poll interval is fine).
- **Atomic commits** — one logical change per commit; reverting a single commit must leave the tree compiling. Never squash.
- **Clippy is not in the pre-commit hook** — run `nix develop --command cargo clippy --workspace --all-targets -- -D warnings` before any Rust commit. `unwrap_used`/`expect_used` are denied in non-test code (test files may `#![allow(...)]` as the existing specs do).
- **Times ISO8601 UTC** in any docs/logs.
- **Sync tolerances** (single source of truth, `sync-oracle.ts`): playing convergence `SYNC_TOL_MS = 1500`; seek accuracy `SEEK_TOL_MS = 1000`; settle/poll window `SETTLE_MS = 8000`, poll every `POLL_MS = 200`.
- **Members**: seeded users `playwright` / `playwright2` / `playwright3`, each a distinct browser context = distinct deviceId = distinct member.

---

## File Structure

- **Create** `compat-playwright/tests/lib/handles.ts` — verified DOM selectors + JS handles for the pinned jellyfin-web 10.11.8 bundle (spike output).
- **Create** `compat-playwright/tests/lib/virtual-person.ts` — `VirtualPerson` class (connect/login/group/commands/probe/capture).
- **Create** `compat-playwright/tests/lib/sync-oracle.ts` — tolerances + `sampleAll` / `waitUntilInSync` / `assertPausedAll` / `assertItem`.
- **Create** `compat-playwright/tests/syncplay-group-matrix.spec.ts` — 7 scenarios (chromium/VP9).
- **Create** `compat-playwright/tests/syncplay-h264-codec.spec.ts` — h264 decode + audio-swap smoke (real-codec browser).
- **Create** `compat-playwright/playwright.syncplay.config.ts` — two projects: `chromium` (matrix) and `chromium-h264` (smoke, `executablePath` from `PHAROS_H264_BROWSER`).
- **Modify** `crates/pharos-server/src/main.rs` — split media seeding into testable `register_seed_items` + `generate_seed_fixtures`; add a 3-episode series + a dual-audio/2-subtitle clip.
- **Modify** `flake.nix` — add `pkgs.chromium` to the devShell; export `PHAROS_H264_BROWSER`.
- **Modify** `justfile` — `compat-syncplay` recipe (bootstrap + 3 users + run the two specs).
- **Create** `.github/workflows/syncplay-e2e.yml` — path-filtered `pull_request` + `workflow_dispatch`.

---

## Task 1: Seed — testable item registration + series/multitrack fixtures

**Files:**
- Modify: `crates/pharos-server/src/main.rs` (the `seed_playwright_user` fn, ~line 166–290)
- Test: `crates/pharos-server/src/main.rs` (a `#[cfg(test)] mod seed_tests`)

**Interfaces:**
- Produces:
  - `struct SeedPaths { movie: PathBuf, movie2: PathBuf, episode_legacy: PathBuf, audio: PathBuf, series_eps: [PathBuf; 3], multitrack: PathBuf }`
  - `async fn register_seed_items(stores: &Stores, paths: &SeedPaths) -> Result<(), AppError>` — pure DB registration, no ffmpeg. Registers ids 1–4 (unchanged: Movie, Movie, Episode, Audio), ids 5–7 (Episodes of one series), id 8 (multitrack Movie).
  - `async fn generate_seed_fixtures(fixture_dir: &Path, target_dir: &Path) -> Result<SeedPaths, AppError>` — runs ffmpeg to materialise every path; the single-track ones copy the existing 5 s VP9/Opus fixture, id-8 is a fresh multi-track encode.
- Consumes: existing `Stores`, `MediaItem`, `MediaKind`, `SeriesInfo` from `pharos_core`.

- [ ] **Step 1: Write the failing test** (append to `main.rs`)

```rust
#[cfg(test)]
mod seed_tests {
    use super::*;
    use pharos_core::{MediaKind, MediaStore};

    // register_seed_items must persist the 3-episode series (ids 5-7) with
    // SeriesInfo adjacency and the multitrack item (id 8), on top of the
    // legacy 1-4. Pure DB path — no ffmpeg, paths are synthetic.
    #[tokio::test]
    async fn register_seed_items_persists_series_and_multitrack() {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let p = |s: &str| std::path::PathBuf::from(format!("/seed/{s}"));
        let paths = SeedPaths {
            movie: p("fixture-1.webm"),
            movie2: p("fixture-2.webm"),
            episode_legacy: p("fixture-3.webm"),
            audio: p("fixture-4.webm"),
            series_eps: [p("show-s01e01.webm"), p("show-s01e02.webm"), p("show-s01e03.webm")],
            multitrack: p("multitrack.mkv"),
        };
        register_seed_items(&stores, &paths).await.unwrap();

        // Legacy items still present.
        assert!(stores.get(1).await.unwrap().is_some());
        // The three series episodes carry ordered SeriesInfo.
        for (id, ep) in [(5u64, 1u32), (6, 2), (7, 3)] {
            let item = stores.get(id).await.unwrap().expect("series episode present");
            assert_eq!(item.kind, MediaKind::Episode);
            let s = item.series.expect("SeriesInfo set");
            assert_eq!(s.series_name, "Playwright Show");
            assert_eq!(s.season_number, Some(1));
            assert_eq!(s.episode_number, Some(ep));
        }
        // The multitrack clip is registered as a Movie.
        let mt = stores.get(8).await.unwrap().expect("multitrack present");
        assert_eq!(mt.kind, MediaKind::Movie);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `nix develop --command cargo test -p pharos-server --lib -- seed_tests::register_seed_items_persists_series_and_multitrack`
Expected: FAIL — `SeedPaths` / `register_seed_items` not defined.

- [ ] **Step 3: Implement `SeedPaths` + `register_seed_items` + `generate_seed_fixtures`, refactor `seed_playwright_user` to call them**

Replace the media-generation + registration block inside `seed_playwright_user` (the `fixture_path` generation through the `for (i, kind, path)` loop, ~lines 190–281) with calls to the two new functions, and add the functions. Registration:

```rust
struct SeedPaths {
    movie: std::path::PathBuf,
    movie2: std::path::PathBuf,
    episode_legacy: std::path::PathBuf,
    audio: std::path::PathBuf,
    series_eps: [std::path::PathBuf; 3],
    multitrack: std::path::PathBuf,
}

async fn register_seed_items(stores: &Stores, paths: &SeedPaths) -> Result<(), AppError> {
    use pharos_core::{MediaItem, MediaKind, MediaStore, SeriesInfo};

    // Legacy items 1-4 (unchanged shapes; keep existing crawl/console specs green).
    let legacy: [(u64, MediaKind, &std::path::PathBuf); 4] = [
        (1, MediaKind::Movie, &paths.movie),
        (2, MediaKind::Movie, &paths.movie2),
        (3, MediaKind::Episode, &paths.episode_legacy),
        (4, MediaKind::Audio, &paths.audio),
    ];
    for (id, kind, path) in legacy {
        let _ = stores
            .put(MediaItem { id, path: path.clone(), title: format!("Playwright Title {id}"), kind, ..Default::default() })
            .await;
    }

    // 3-episode series (ids 5-7). series_folder gives a stable series_key so
    // pharos synthesises one Series + Season with three ordered episodes.
    let series_folder = paths.series_eps[0].parent().map(|p| p.to_string_lossy().into_owned());
    for (idx, path) in paths.series_eps.iter().enumerate() {
        let ep = (idx + 1) as u32;
        let item = MediaItem {
            id: 5 + idx as u64,
            path: path.clone(),
            title: format!("Playwright Show S01E0{ep}"),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "Playwright Show".into(),
                season_number: Some(1),
                episode_number: Some(ep),
                series_folder: series_folder.clone(),
                series_year: Some(2026),
            }),
            ..Default::default()
        };
        let _ = stores.put(item).await;
    }

    // Dual-audio + 2-subtitle clip (id 8) for the swap scenario.
    let _ = stores
        .put(MediaItem { id: 8, path: paths.multitrack.clone(), title: "Playwright Multitrack".into(), kind: MediaKind::Movie, ..Default::default() })
        .await;
    Ok(())
}
```

Fixture generation (reuses the existing VP9/Opus single-track encode for 1–7; a fresh multi-track encode for id 8):

```rust
async fn generate_seed_fixtures(
    fixture_dir: &std::path::Path,
    target_dir: &std::path::Path,
) -> Result<SeedPaths, AppError> {
    tokio::fs::create_dir_all(fixture_dir).await.map_err(AppError::Io)?;
    tokio::fs::create_dir_all(target_dir).await.map_err(AppError::Io)?;

    // Base 5 s VP9/Opus single-track clip (existing recipe).
    let base = fixture_dir.join("fixture.webm");
    run_ffmpeg(&[
        "-y", "-hide_banner", "-loglevel", "error",
        "-f", "lavfi", "-i", "testsrc=duration=5:size=320x240:rate=15",
        "-f", "lavfi", "-i", "sine=frequency=440:duration=5",
        "-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8",
        "-row-mt", "1", "-pix_fmt", "yuv420p", "-c:a", "libopus", "-shortest",
        base.to_str().unwrap(),
    ]).await?;

    // Copy the base into every single-track path (UNIQUE(path) needs distinct files).
    let copy_base = |name: &str| {
        let dst = target_dir.join(name);
        (base.clone(), dst)
    };
    let singles = [
        ("fixture-1.webm"), ("fixture-2.webm"), ("fixture-3.webm"), ("fixture-4.webm"),
        ("show-s01e01.webm"), ("show-s01e02.webm"), ("show-s01e03.webm"),
    ];
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for name in singles {
        let (src, dst) = copy_base(name);
        tokio::fs::copy(&src, &dst).await.map_err(AppError::Io)?;
        out.push(dst);
    }

    // Multi-track clip: VP9 + two Opus tracks (440 Hz / 880 Hz) + two WebVTT
    // subtitle tracks, muxed into Matroska so pharos probes 2 audio + 2 subs.
    let sub_a = fixture_dir.join("a.vtt");
    let sub_b = fixture_dir.join("b.vtt");
    tokio::fs::write(&sub_a, "WEBVTT\n\n00:00:00.000 --> 00:00:05.000\nTrack A\n").await.map_err(AppError::Io)?;
    tokio::fs::write(&sub_b, "WEBVTT\n\n00:00:00.000 --> 00:00:05.000\nTrack B\n").await.map_err(AppError::Io)?;
    let multitrack = target_dir.join("multitrack.mkv");
    run_ffmpeg(&[
        "-y", "-hide_banner", "-loglevel", "error",
        "-f", "lavfi", "-i", "testsrc=duration=5:size=320x240:rate=15",
        "-f", "lavfi", "-i", "sine=frequency=440:duration=5",
        "-f", "lavfi", "-i", "sine=frequency=880:duration=5",
        "-i", sub_a.to_str().unwrap(),
        "-i", sub_b.to_str().unwrap(),
        "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:s", "-map", "4:s",
        "-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-row-mt", "1",
        "-pix_fmt", "yuv420p", "-c:a", "libopus", "-c:s", "webvtt",
        "-metadata:s:a:0", "language=eng", "-metadata:s:a:1", "language=jpn",
        "-metadata:s:s:0", "language=eng", "-metadata:s:s:1", "language=jpn",
        multitrack.to_str().unwrap(),
    ]).await?;

    Ok(SeedPaths {
        movie: out[0].clone(), movie2: out[1].clone(), episode_legacy: out[2].clone(), audio: out[3].clone(),
        series_eps: [out[4].clone(), out[5].clone(), out[6].clone()],
        multitrack,
    })
}

async fn run_ffmpeg(args: &[&str]) -> Result<(), AppError> {
    let status = tokio::process::Command::new("ffmpeg").args(args).status().await.map_err(AppError::Io)?;
    if !status.success() {
        return Err(AppError::Io(std::io::Error::other("ffmpeg fixture generation failed")));
    }
    Ok(())
}
```

Rewire `seed_playwright_user`'s body to: create the user (unchanged), then
`let target_dir = cfg.media.roots.first().cloned().unwrap_or_else(|| fixture_dir.clone());`
`let paths = generate_seed_fixtures(&fixture_dir, &target_dir).await?;`
`register_seed_items(&stores, &paths).await?;`
and update the final `writeln!` to `"… 8 items (incl. 3-episode series + multitrack), fixture={}"`.

- [ ] **Step 4: Run test to verify it passes**

Run: `nix develop --command cargo test -p pharos-server --lib -- seed_tests::register_seed_items_persists_series_and_multitrack`
Expected: PASS.

- [ ] **Step 5: Clippy + commit**

Run: `nix develop --command cargo clippy -p pharos-server --all-targets -- -D warnings`
```bash
git add crates/pharos-server/src/main.rs
git commit -m "feat(seed): 3-episode series + dual-audio/subtitle clip for syncplay E2E

Split seed media into testable register_seed_items (pure DB) +
generate_seed_fixtures (ffmpeg). Adds ids 5-7 (one series, S01E01-03)
and id 8 (2 audio + 2 subtitle tracks) so the group-watch harness can
drive next/prev-episode and audio/subtitle-swap scenarios."
```

---

## Task 2: devShell — h264-capable browser

**Files:**
- Modify: `flake.nix` (devShell `packages` list ~line 845; `shellHook` ~line 866)

**Interfaces:**
- Produces: `PHAROS_H264_BROWSER` env var → absolute path of an h264-capable chromium binary, present in the devShell.

- [ ] **Step 1: Add `pkgs.chromium` to the devShell packages**

After `pkgs.jellyfin-web` (line 846) add:
```nix
            # h264-capable Chromium for the SyncPlay h264 codec smoke
            # (compat-playwright/syncplay-h264-codec.spec.ts). The nix-pinned
            # playwright-driver chromium is a FOSS build with NO h264 decode,
            # so it can only exercise the VP9 path; pkgs.chromium decodes h264
            # and stands in for the Firefox/Zen demuxed-CMAF path in prod.
            pkgs.chromium
```

- [ ] **Step 2: Export the binary path in the shellHook**

After the `PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1` line (line 878) add:
```sh
            # Absolute path of the h264-capable chromium (see packages list).
            # syncplay-h264-codec.spec.ts launches it via Playwright
            # executablePath; the spec asserts h264 decode and fails loudly
            # with a swap-to-google-chrome hint if this build lacks it.
            export PHAROS_H264_BROWSER=${pkgs.chromium}/bin/chromium
```

- [ ] **Step 3: Verify the env resolves**

Run: `nix develop --command bash -c 'test -x "$PHAROS_H264_BROWSER" && echo OK: $PHAROS_H264_BROWSER'`
Expected: prints `OK: /nix/store/…-chromium-…/bin/chromium`.

- [ ] **Step 4: Commit**

```bash
git add flake.nix
git commit -m "build(devShell): add h264-capable chromium + PHAROS_H264_BROWSER

The nix-pinned playwright chromium is FOSS (no h264 decode); the
SyncPlay h264 codec smoke needs a browser that decodes the demuxed-CMAF
path real Firefox/Zen clients use."
```

---

## Task 3: Control-handle discovery spike → `handles.ts`

**Files:**
- Create: `compat-playwright/tests/lib/handles.ts`
- Create (temporary, deleted in Step 4): `compat-playwright/tests/_spike.spec.ts`

**Interfaces:**
- Produces (`handles.ts` exports):
  - `PHAROS_URL`, `WS_URL`, seeded-user credential tuples `USERS = [{user,pass}, …]` (playwright / playwright2 / playwright3).
  - `SELECTORS` — verified selectors: `serverHost`, `manualName`, `manualPassword`, `signIn`, `playButton`, `videoOsd`, `osdAudioButton`, `osdSubtitleButton`, `trackMenuItem(index)`.
  - Note comment: group create/join uses `window.ApiClient.createSyncPlayGroup`/`joinSyncPlayGroup`; `window.playbackManager` is NOT exposed, so playback control is via REST (see `virtual-person.ts`) and OSD selectors here.

- [ ] **Step 1: Write the spike spec** to observe the live pinned bundle

The bundle is a deterministic nix input, so its handles are stable. Boot one page, log in, open the item-1 OSD, and dump the audio/subtitle control selectors + confirm `window.ApiClient` group methods exist. Create `_spike.spec.ts`:

```ts
import { test } from "@playwright/test";

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";

test("spike: dump syncplay control handles", async ({ page }) => {
  test.setTimeout(90_000);
  await page.goto("/", { waitUntil: "networkidle" });
  // Reuse the existing connect+login flow (copy from syncplay-realclient.spec.ts).
  // … connect, login as playwright …
  const api = await page.evaluate(() => ({
    hasCreate: typeof (window as any).ApiClient?.createSyncPlayGroup,
    hasJoin: typeof (window as any).ApiClient?.joinSyncPlayGroup,
    hasPlaybackManager: typeof (window as any).playbackManager,
  }));
  console.log("API HANDLES", JSON.stringify(api));
  // Start item 1, open OSD, dump audio/subtitle button selectors.
  // … navigate to /#/details?id=1, click play …
  const osd = await page.evaluate(() => {
    const q = (s: string) => !!document.querySelector(s);
    return {
      audioBtn: q("button.btnAudio") || q(".btnAudio"),
      subBtn: q("button.btnSubtitles") || q(".btnSubtitles"),
      osd: q(".videoOsdBottom") || q("#videoOsdPage"),
    };
  });
  console.log("OSD HANDLES", JSON.stringify(osd));
});
```

- [ ] **Step 2: Run the spike against a live stack**

Run: `nix develop --command bash -c 'just compat-syncplay-spike'` — a temporary recipe identical to `compat-playwright-full` bootstrap but running only `_spike.spec.ts` (add it, or run the existing `compat-playwright-full` bootstrap manually and `npx playwright test _spike.spec.ts`).
Expected: console prints `API HANDLES {"hasCreate":"function","hasJoin":"function","hasPlaybackManager":"undefined"}` and the real OSD selectors. Record the observed truthy selector names.

- [ ] **Step 3: Write `handles.ts`** using the observed values

```ts
// Verified against the nix-pinned jellyfin-web 10.11.8 bundle
// (JELLYFIN_WEB_DIR). Handles are deterministic per pinned input; re-run
// tests/_spike (git history) if the jellyfin-web pin bumps.
export const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
export const WS_URL = PHAROS_URL.replace(/^http/, "ws");

export const USERS = [
  { user: process.env.PHAROS_TEST_USER ?? "playwright", pass: process.env.PHAROS_TEST_PASS ?? "playwright-test-pw" },
  { user: process.env.PHAROS_TEST_USER2 ?? "playwright2", pass: process.env.PHAROS_TEST_PASS2 ?? "playwright2-test-pw" },
  { user: process.env.PHAROS_TEST_USER3 ?? "playwright3", pass: process.env.PHAROS_TEST_PASS3 ?? "playwright3-test-pw" },
];

// Fill each value from the spike's observed truthy selector.
export const SELECTORS = {
  serverHost: "#txtServerHost",
  manualName: "#txtManualName",
  manualPassword: "#txtManualPassword",
  signIn: /^sign in$/i,
  playButton: "button.btnPlay",
  osdAudioButton: "<observed>",     // e.g. "button.btnAudio"
  osdSubtitleButton: "<observed>",  // e.g. "button.btnSubtitles"
  trackMenuItem: (index: number) => `.actionSheetMenuItem[data-id="${index}"]`,
};
```

Replace each `<observed>` with the spike's real value (no placeholders remain in the committed file).

- [ ] **Step 4: Delete the spike spec, commit `handles.ts`**

```bash
rm compat-playwright/tests/_spike.spec.ts
git add compat-playwright/tests/lib/handles.ts
git commit -m "test(syncplay): record verified jellyfin-web control handles

window.ApiClient exposes createSyncPlayGroup/joinSyncPlayGroup;
window.playbackManager is NOT exposed, so group commands go via REST and
audio/subtitle swaps via the observed OSD selectors."
```

---

## Task 4: `sync-oracle.ts` — tolerances + convergence assertions

**Files:**
- Create: `compat-playwright/tests/lib/sync-oracle.ts`
- Test: `compat-playwright/tests/lib/sync-oracle.spec.ts` (pure, no browser)

**Interfaces:**
- Consumes: `VideoProbe` shape `{ itemId: string|null, currentTime: number, paused: boolean, ... }` (defined in `virtual-person.ts`, Task 5; for this task define a local structural type to avoid a cycle).
- Produces:
  - `SYNC_TOL_MS`, `SEEK_TOL_MS`, `SETTLE_MS`, `POLL_MS`.
  - `maxPairwiseDeltaMs(times: number[]): number`
  - `withinTol(times: number[], tolMs: number): boolean`
  - `waitUntilInSync(people, {tolMs?}): Promise<void>` — polls `people.map(p=>p.probe())` until all report the same non-null `itemId`, all `paused===false`, and `maxPairwiseDeltaMs ≤ tol`, or throws with a per-person diagnostic after `SETTLE_MS`.
  - `assertPausedAll(people): Promise<void>`, `assertItemAll(people, itemId): Promise<void>`, `assertSeekConverged(people, targetMs): Promise<void>`.

- [ ] **Step 1: Write the failing pure test**

```ts
import { test, expect } from "@playwright/test";
import { maxPairwiseDeltaMs, withinTol, SYNC_TOL_MS } from "./sync-oracle";

test.describe("sync-oracle math", () => {
  test("maxPairwiseDeltaMs is the spread in ms", () => {
    // currentTime is seconds; delta reported in ms.
    expect(maxPairwiseDeltaMs([10.0, 10.4, 10.1])).toBeCloseTo(400, 0);
  });
  test("withinTol respects the tolerance", () => {
    expect(withinTol([10.0, 11.4], SYNC_TOL_MS)).toBe(true);  // 1400ms ≤ 1500
    expect(withinTol([10.0, 11.6], SYNC_TOL_MS)).toBe(false); // 1600ms > 1500
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `nix develop --command bash -c 'cd compat-playwright && npx playwright test tests/lib/sync-oracle.spec.ts'`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement `sync-oracle.ts`**

```ts
export const SYNC_TOL_MS = 1500;
export const SEEK_TOL_MS = 1000;
export const SETTLE_MS = 8000;
export const POLL_MS = 200;

export function maxPairwiseDeltaMs(timesSec: number[]): number {
  if (timesSec.length < 2) return 0;
  return (Math.max(...timesSec) - Math.min(...timesSec)) * 1000;
}
export function withinTol(timesSec: number[], tolMs: number): boolean {
  return maxPairwiseDeltaMs(timesSec) <= tolMs;
}

interface Probeable {
  label: string;
  probe(): Promise<{ itemId: string | null; currentTime: number; paused: boolean; errorCode: number | null }>;
}

async function poll<T>(fn: () => Promise<T | null>, timeoutMs: number, pollMs: number): Promise<T | null> {
  const deadline = Date.now() + timeoutMs;
  // Date.now here is wall-clock for a bounded poll loop — allowed in test code.
  for (;;) {
    const v = await fn();
    if (v !== null) return v;
    if (Date.now() >= deadline) return null;
    await new Promise((r) => setTimeout(r, pollMs));
  }
}

export async function waitUntilInSync(people: Probeable[], opts: { tolMs?: number } = {}): Promise<void> {
  const tol = opts.tolMs ?? SYNC_TOL_MS;
  let last = "";
  const ok = await poll(async () => {
    const probes = await Promise.all(people.map((p) => p.probe()));
    last = probes.map((pr, i) => `${people[i].label}: item=${pr.itemId} t=${pr.currentTime.toFixed(2)} paused=${pr.paused} err=${pr.errorCode}`).join("\n");
    const ids = probes.map((p) => p.itemId);
    const allSameItem = ids.every((id) => id !== null && id === ids[0]);
    const allPlaying = probes.every((p) => !p.paused && p.errorCode === null);
    const converged = withinTol(probes.map((p) => p.currentTime), tol);
    return allSameItem && allPlaying && converged ? true : null;
  }, SETTLE_MS, POLL_MS);
  if (!ok) throw new Error(`members never converged (≤${tol}ms, all playing, same item):\n${last}`);
}

export async function assertItemAll(people: Probeable[], itemId: string): Promise<void> {
  const ok = await poll(async () => {
    const probes = await Promise.all(people.map((p) => p.probe()));
    return probes.every((p) => p.itemId === itemId) ? true : null;
  }, SETTLE_MS, POLL_MS);
  if (!ok) throw new Error(`not all members reached item ${itemId}`);
}

export async function assertPausedAll(people: Probeable[]): Promise<void> {
  const ok = await poll(async () => {
    const probes = await Promise.all(people.map((p) => p.probe()));
    return probes.every((p) => p.paused) ? true : null;
  }, SETTLE_MS, POLL_MS);
  if (!ok) throw new Error("not all members paused");
}

export async function assertSeekConverged(people: Probeable[], targetMs: number): Promise<void> {
  const ok = await poll(async () => {
    const probes = await Promise.all(people.map((p) => p.probe()));
    return probes.every((p) => Math.abs(p.currentTime * 1000 - targetMs) <= SEEK_TOL_MS) ? true : null;
  }, SETTLE_MS, POLL_MS);
  if (!ok) throw new Error(`members did not converge to seek target ${targetMs}ms (±${SEEK_TOL_MS})`);
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `nix develop --command bash -c 'cd compat-playwright && npx playwright test tests/lib/sync-oracle.spec.ts'`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add compat-playwright/tests/lib/sync-oracle.ts compat-playwright/tests/lib/sync-oracle.spec.ts
git commit -m "test(syncplay): sync oracle — currentTime convergence tolerances"
```

---

## Task 5: `VirtualPerson` helper

**Files:**
- Create: `compat-playwright/tests/lib/virtual-person.ts`

**Interfaces:**
- Consumes: `handles.ts` (`SELECTORS`, `PHAROS_URL`, `USERS`).
- Produces: `class VirtualPerson` with `label`, and async methods:
  - static `spawn(browser, index): Promise<VirtualPerson>` — new context+page, connect, login `USERS[index]`, open socket, cache token + deviceId + serverId.
  - `createGroup(name)`, `joinGroup(groupId)`, `leaveGroup()` — via `window.ApiClient`.
  - `setNewQueue(itemIds: string[])`, `pause()`, `unpause()`, `seek(positionMs)`, `nextItem()`, `previousItem()` — via authenticated `POST /SyncPlay/*` (`fetch` inside `page.evaluate`, Emby-Authorization header with the cached token+deviceId). Bodies match the DTOs in `crates/pharos-server/src/api/jellyfin/syncplay.rs` (`SetNewQueue{PlayingQueue,PlayingItemPosition,StartPositionTicks}`, `Seek{PositionTicks}`, `NextItem/PreviousItem{PlaylistItemId}`).
  - `playViaUi(itemId)` — real `SELECTORS.playButton` click (the UI playback path).
  - `swapAudio(index)`, `swapSubtitle(index)` — open the real OSD menu (`SELECTORS.osdAudioButton` / `osdSubtitleButton`) and pick `trackMenuItem(index)`.
  - `probe(): Promise<VideoProbe>` — read the live `<video>` + current SyncPlay item.
  - `currentPlaylistItemId(): Promise<string|null>` — from the last `PlayQueue` push (needed for Next/Previous bodies).
  - `diagnostics(): string` — captured console + `/SyncPlay|/PlaybackInfo|/Playing` requests.
  - `close()`.
- `interface VideoProbe { itemId: string|null; currentTime: number; paused: boolean; readyState: number; currentSrc: string; audioIndex: number|null; textIndex: number|null; errorCode: number|null }`.

- [ ] **Step 1: Implement the helper** (no standalone unit test — exercised by every scenario in Task 6; its correctness is proven when the matrix passes)

Key implementation notes for the implementer:
- Reuse `connectToServer` / `login` from `syncplay-realclient.spec.ts` (copy into the helper).
- Cache `token` from `localStorage.jellyfin_credentials` (or capture the auth response); cache `deviceId` — jellyfin-web stores it in `localStorage` under a `_deviceId2` key; read whatever the app actually sends (confirm via the spike). `serverId` from `jellyfin_credentials.Servers[0].Id`.
- Socket: open `/socket?api_key=…&deviceId=…` (as in `syncplay-two-users.spec.ts`), record messages on `window.__pharos_msgs`; `currentPlaylistItemId` scans them for the newest `PlayQueue`/`GroupUpdate` carrying the playing `PlaylistItemId`.
- REST commands POST to `${PHAROS_URL}/SyncPlay/<Cmd>` with header `X-Emby-Authorization: MediaBrowser Client="pw", Device="pw", DeviceId="${deviceId}", Version="0", Token="${token}"` and JSON body. These are the exact requests jellyfin-web's OSD issues in a group.
- `probe()` runs in `page.evaluate`: read the single `<video>`; `itemId` from the SyncPlay manager's current item if reachable, else from the last `Play`/`PlayQueue` socket message; `audioIndex`/`textIndex` from the app's current stream indices if available else `video.audioTracks`/`textTracks` selected index.

- [ ] **Step 2: Type-check**

Run: `nix develop --command bash -c 'cd compat-playwright && npx tsc --noEmit'`
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add compat-playwright/tests/lib/virtual-person.ts
git commit -m "test(syncplay): VirtualPerson — real jellyfin-web member driver

Group ops via window.ApiClient; group commands via each member's own
/SyncPlay/* REST call (OSD-equivalent wire); audio/subtitle swap via the
real OSD menus; probe() reads the live <video>."
```

---

## Task 6: The 7-scenario matrix spec (chromium/VP9)

**Files:**
- Create: `compat-playwright/tests/syncplay-group-matrix.spec.ts`

**Interfaces:**
- Consumes: `VirtualPerson`, `sync-oracle` (`waitUntilInSync`, `assertPausedAll`, `assertItemAll`, `assertSeekConverged`).

Each test spawns a fresh set of members, builds/joins one group, exercises one
scenario, and asserts via the oracle. Item ids: movie `"1"`; series episodes
`"5","6","7"`. Use `test.setTimeout(120_000)`. On failure, append
`people.map(p=>p.diagnostics())` to the assertion message.

- [ ] **Step 1: Scenario 1 — solo-play then late join**

```ts
import { test, expect } from "@playwright/test";
import { VirtualPerson } from "./lib/virtual-person";
import { waitUntilInSync, assertPausedAll, assertItemAll, assertSeekConverged } from "./lib/sync-oracle";

test.describe("syncplay group-watch matrix (chromium/VP9)", () => {
  test("solo play, then a late joiner promptly joins the stream playing", async ({ browser }) => {
    test.setTimeout(120_000);
    const a = await VirtualPerson.spawn(browser, 0);
    const g = await a.createGroup("m-solo-join");
    await a.setNewQueue(["1"]);         // A starts playback alone
    await a.unpause();
    await waitUntilInSync([a]);          // A is actually playing
    const b = await VirtualPerson.spawn(browser, 1);
    await b.joinGroup(g);                // B joins mid-playback
    // B must reach the same item AND be playing (no forced pause, no wedge).
    await waitUntilInSync([a, b]);
    await a.close(); await b.close();
  });
```

- [ ] **Step 2: Scenario 2 — empty group, join, then pick playback**

```ts
  test("empty group: member joins, then playback starts on everyone", async ({ browser }) => {
    test.setTimeout(120_000);
    const a = await VirtualPerson.spawn(browser, 0);
    const g = await a.createGroup("m-empty-join");
    const b = await VirtualPerson.spawn(browser, 1);
    const c = await VirtualPerson.spawn(browser, 2);
    await b.joinGroup(g); await c.joinGroup(g);   // join while nothing plays
    await a.setNewQueue(["1"]); await a.unpause(); // then pick playback
    await waitUntilInSync([a, b, c]);              // all three roll
    await a.close(); await b.close(); await c.close();
  });
```

- [ ] **Step 3: Scenario 3 — next / previous episode**

```ts
  test("next/previous episode starts playback of the new episode on everyone", async ({ browser }) => {
    test.setTimeout(150_000);
    const [a, b, c] = await Promise.all([0, 1, 2].map((i) => VirtualPerson.spawn(browser, i)));
    const g = await a.createGroup("m-nextprev");
    await b.joinGroup(g); await c.joinGroup(g);
    await a.setNewQueue(["5", "6", "7"]);  // 3-episode queue
    await a.unpause();
    await assertItemAll([a, b, c], "5");
    await waitUntilInSync([a, b, c]);
    await a.nextItem();                     // → episode 6 on all
    await assertItemAll([a, b, c], "6");
    await waitUntilInSync([a, b, c]);
    await a.previousItem();                 // → episode 5 on all
    await assertItemAll([a, b, c], "5");
    await waitUntilInSync([a, b, c]);
    await a.close(); await b.close(); await c.close();
  });
```

- [ ] **Step 4: Scenarios 4+5 — pause all, resume in sync**

```ts
  test("pause pauses everyone; resume plays everyone back in sync", async ({ browser }) => {
    test.setTimeout(150_000);
    const [a, b, c] = await Promise.all([0, 1, 2].map((i) => VirtualPerson.spawn(browser, i)));
    const g = await a.createGroup("m-pause-resume");
    await b.joinGroup(g); await c.joinGroup(g);
    await a.setNewQueue(["1"]); await a.unpause();
    await waitUntilInSync([a, b, c]);
    await b.pause();                        // any member can pause
    await assertPausedAll([a, b, c]);
    await b.unpause();
    await waitUntilInSync([a, b, c]);       // re-converged, all playing
    await a.close(); await b.close(); await c.close();
  });
```

- [ ] **Step 5: Scenario 6 — seek all**

```ts
  test("seek moves everyone to the same point", async ({ browser }) => {
    test.setTimeout(150_000);
    const [a, b, c] = await Promise.all([0, 1, 2].map((i) => VirtualPerson.spawn(browser, i)));
    const g = await a.createGroup("m-seek");
    await b.joinGroup(g); await c.joinGroup(g);
    await a.setNewQueue(["1"]); await a.unpause();
    await waitUntilInSync([a, b, c]);
    await a.seek(3000);                     // seek to 3.0s (ms)
    await assertSeekConverged([a, b, c], 3000);
    await a.close(); await b.close(); await c.close();
  });
```

- [ ] **Step 6: Scenario 7 — audio/subtitle swap keeps everyone in sync**

```ts
  test("audio + subtitle swap on one member does not desync the group", async ({ browser }) => {
    test.setTimeout(150_000);
    const [a, b, c] = await Promise.all([0, 1, 2].map((i) => VirtualPerson.spawn(browser, i)));
    const g = await a.createGroup("m-swap");
    await b.joinGroup(g); await c.joinGroup(g);
    await a.setNewQueue(["8"]); await a.unpause();  // multitrack item
    await waitUntilInSync([a, b, c]);
    await b.swapAudio(1);                            // B switches to 2nd audio
    await waitUntilInSync([a, b, c]);               // B re-converges, group intact
    await b.swapSubtitle(1);                         // B switches subtitle track
    await waitUntilInSync([a, b, c]);
    await a.close(); await b.close(); await c.close();
  });
});
```

- [ ] **Step 7: Type-check + commit** (execution happens in Task 8)

Run: `nix develop --command bash -c 'cd compat-playwright && npx tsc --noEmit'`
Expected: no errors.
```bash
git add compat-playwright/tests/syncplay-group-matrix.spec.ts
git commit -m "test(syncplay): 7-scenario group-watch matrix (3 real members, VP9)"
```

---

## Task 7: h264 codec smoke (real-codec browser) + Playwright config

**Files:**
- Create: `compat-playwright/playwright.syncplay.config.ts`
- Create: `compat-playwright/tests/syncplay-h264-codec.spec.ts`

**Interfaces:**
- Consumes: `PHAROS_H264_BROWSER` (Task 2), `VirtualPerson`, `sync-oracle`.

- [ ] **Step 1: Write the syncplay Playwright config** — two projects, matrix vs h264 smoke

```ts
import { defineConfig, devices } from "@playwright/test";

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const JELLYFIN_WEB_PORT = parseInt(process.env.JELLYFIN_WEB_PORT ?? "8910", 10);
const JELLYFIN_WEB_DIR = process.env.JELLYFIN_WEB_DIR;
if (!JELLYFIN_WEB_DIR) throw new Error("JELLYFIN_WEB_DIR not set — enter the nix devShell.");
const H264_BROWSER = process.env.PHAROS_H264_BROWSER; // absolute chromium path

export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? "github" : "list",
  timeout: 150_000,
  use: {
    baseURL: `http://127.0.0.1:${JELLYFIN_WEB_PORT}`,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
    extraHTTPHeaders: { "X-Pharos-Compat-Suite": "playwright" },
  },
  projects: [
    {
      name: "matrix",
      testMatch: /syncplay-group-matrix\.spec\.ts/,
      use: { ...devices["Desktop Chrome"], channel: "chromium" },
    },
    {
      name: "h264",
      testMatch: /syncplay-h264-codec\.spec\.ts/,
      use: {
        ...devices["Desktop Chrome"],
        launchOptions: H264_BROWSER ? { executablePath: H264_BROWSER } : {},
      },
    },
    // sync-oracle.spec.ts (pure) is run separately; exclude from these projects.
  ],
  webServer: {
    command: `npx http-server ${JELLYFIN_WEB_DIR} -p ${JELLYFIN_WEB_PORT} -s --cors --proxy ${PHAROS_URL}?`,
    port: JELLYFIN_WEB_PORT,
    reuseExistingServer: true,
    timeout: 30_000,
  },
});
```

- [ ] **Step 2: Write the h264 smoke** — assert decode, then audio-swap reuses cached video

```ts
import { test, expect } from "@playwright/test";
import { VirtualPerson } from "./lib/virtual-person";
import { waitUntilInSync } from "./lib/sync-oracle";

test.describe("syncplay h264 codec smoke (real-codec browser)", () => {
  test("h264 demuxed-CMAF actually decodes, and an audio swap keeps sync", async ({ browser }) => {
    test.setTimeout(150_000);
    // Guard: this browser must decode h264, else the smoke is meaningless.
    const canH264 = await (async () => {
      const ctx = await browser.newContext();
      const page = await ctx.newPage();
      await page.goto("about:blank");
      const ok = await page.evaluate(() =>
        (window as any).MediaSource?.isTypeSupported('video/mp4; codecs="avc1.640028"') === true);
      await ctx.close();
      return ok;
    })();
    expect(
      canH264,
      "PHAROS_H264_BROWSER lacks h264 decode — swap flake.nix to google-chrome (allowUnfree) so the demuxed-CMAF path is exercised",
    ).toBe(true);

    const a = await VirtualPerson.spawn(browser, 0);
    const g = await a.createGroup("h264-smoke");
    const b = await VirtualPerson.spawn(browser, 1);
    await b.joinGroup(g);
    await a.setNewQueue(["8"]);  // multitrack; browser advertises h264 → demuxed CMAF
    await a.unpause();
    await waitUntilInSync([a, b]);           // proves h264 CMAF decoded + rolling
    // Assert no decode error on either member.
    for (const p of [a, b]) {
      const pr = await p.probe();
      expect(pr.errorCode, `${p.label} video.error`).toBeNull();
    }
    await b.swapAudio(1);                     // the PR#70 class: swap must reuse video
    await waitUntilInSync([a, b]);
    await a.close(); await b.close();
  });
});
```

- [ ] **Step 3: Type-check + commit**

Run: `nix develop --command bash -c 'cd compat-playwright && npx tsc --noEmit'`
Expected: no errors.
```bash
git add compat-playwright/playwright.syncplay.config.ts compat-playwright/tests/syncplay-h264-codec.spec.ts
git commit -m "test(syncplay): h264 demuxed-CMAF decode + audio-swap smoke (real-codec browser)"
```

---

## Task 8: `just compat-syncplay` recipe + first full run

**Files:**
- Modify: `justfile`

**Interfaces:**
- Consumes: everything above. Bootstraps pharos (like `compat-playwright-full`) + seeds 3 users, then runs the two syncplay specs via `playwright.syncplay.config.ts`.

- [ ] **Step 1: Add the recipe** (mirror `compat-playwright-full`, adding playwright3 + the syncplay config)

```make
# Manual SyncPlay group-watch E2E: 3 real jellyfin-web members through the
# full scenario matrix (VP9) + an h264 demuxed-CMAF smoke. Heavy (3 browsers
# + live transcode) — run before group-watch nights / pre-release, or let the
# path-filtered syncplay-e2e workflow run it when group-play code changes.
compat-syncplay:
    #!/usr/bin/env bash
    set -euo pipefail
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    cat > "$TMP/pharos.toml" <<EOF
    [server]
    bind = "127.0.0.1:8096"
    name = "pharos-syncplay"
    image_cache_dir = "$TMP/images"
    trickplay_cache_dir = "$TMP/trickplay"
    transcode_cache_dir = "$TMP/transcode"
    image_seek_seconds = 1
    [obs]
    log_level = "warn"
    [media]
    roots = ["$TMP/media"]
    [database]
    url = "sqlite://$TMP/pharos.db?mode=rwc"
    EOF
    PHAROS_CONFIG="$TMP/pharos.toml"
    nix develop --command cargo build -q --bin transcode-worker
    nix develop --command cargo run -q --bin pharos -- --config "$PHAROS_CONFIG" admin seed-playwright-user
    nix develop --command cargo run -q --bin pharos -- --config "$PHAROS_CONFIG" admin create-user --name playwright2 --password playwright2-test-pw --admin
    nix develop --command cargo run -q --bin pharos -- --config "$PHAROS_CONFIG" admin create-user --name playwright3 --password playwright3-test-pw --admin
    nix develop --command bash -c "cargo run -q --bin pharos -- --config '$PHAROS_CONFIG' serve" &
    SERVER_PID=$!
    trap 'kill $SERVER_PID 2>/dev/null || true; rm -rf "$TMP"' EXIT
    # Wait for readiness (condition-based, not a flat sleep).
    for i in $(seq 1 60); do curl -sf http://127.0.0.1:8096/System/Info/Public >/dev/null && break || sleep 0.5; done
    nix develop --command bash -c 'cd compat-playwright && npx playwright test --config playwright.syncplay.config.ts'
```

- [ ] **Step 2: Run the whole harness end-to-end**

Run: `nix develop --command just compat-syncplay`
Expected: all matrix scenarios + the h264 smoke PASS. If a scenario fails, read the per-person diagnostics + `compat-playwright/playwright-report`; this is real bug-finding — triage against `crates/pharos-sync` / `syncplay.rs` / `hls.rs` before weakening any assertion.

- [ ] **Step 3: Commit**

```bash
git add justfile
git commit -m "test(syncplay): just compat-syncplay — manual 3-member group-watch harness"
```

---

## Task 9: Path-filtered CI workflow

**Files:**
- Create: `.github/workflows/syncplay-e2e.yml`

**Interfaces:**
- Consumes: `just compat-syncplay`. Runs only on PRs touching group-play code, plus manual dispatch.

- [ ] **Step 1: Write the workflow** (separate file so the `paths:` filter scopes the whole workflow; mirrors `ci.yml`'s `compat-playwright` job)

```yaml
name: syncplay-e2e
on:
  workflow_dispatch:
  pull_request:
    paths:
      - "crates/pharos-sync/**"
      - "crates/pharos-server/src/api/jellyfin/syncplay.rs"
      - "crates/pharos-server/src/api/jellyfin/hls.rs"
      - "crates/pharos-server/src/sessions.rs"
      - "crates/pharos-transcode/**"
      - "crates/pharos-cache/src/hls_cache.rs"
      - "crates/pharos-server/src/main.rs"
      - "compat-playwright/tests/syncplay-*.spec.ts"
      - "compat-playwright/tests/lib/**"
      - "compat-playwright/playwright.syncplay.config.ts"
      - ".github/workflows/syncplay-e2e.yml"
      - "justfile"
concurrency:
  group: syncplay-e2e-${{ github.ref }}
  cancel-in-progress: true
jobs:
  syncplay:
    name: syncplay group-watch E2E
    runs-on: pharos-nix-builder-amd64
    steps:
      - uses: actions/checkout@v4
      - uses: alisonjenkins/setup-host-nix@v1
      - name: npm install
        run: nix develop --command bash -c 'cd compat-playwright && npm install --no-audit --no-fund'
      - name: Run SyncPlay group-watch harness
        run: nix develop --command just compat-syncplay
      - name: Upload Playwright report on failure
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: syncplay-report-${{ github.sha }}
          path: compat-playwright/playwright-report
```

- [ ] **Step 2: Validate YAML + path filter locally**

Run: `nix develop --command bash -c 'command -v yamllint >/dev/null && yamllint .github/workflows/syncplay-e2e.yml || python3 -c "import yaml,sys; yaml.safe_load(open(sys.argv[1]))" .github/workflows/syncplay-e2e.yml && echo VALID'`
Expected: `VALID` (or clean yamllint).

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/syncplay-e2e.yml
git commit -m "ci(syncplay): path-filtered group-watch E2E workflow

Runs the 3-member SyncPlay harness only when group-play code changes
(pharos-sync, syncplay.rs, hls.rs, sessions.rs, transcode, hls_cache,
the seed, or the specs) + on manual dispatch. Not a required check, so a
paths-skipped run never blocks unrelated PRs."
```

---

## Self-Review

**Spec coverage:** All 7 scenarios → Task 6 (1:1 tests) + audio/sub also in Task 7. 3 virtual people → `VirtualPerson` + 3 seeded users (Task 1/5/8). "Actually know a real browser plays" → real jellyfin-web contexts + `<video>.currentTime` oracle (Task 4/5) + h264 real-codec smoke (Task 7). Manual + path-filtered CI → Task 8/9. Seed for next/prev + swap → Task 1.

**Placeholder scan:** `handles.ts` ships `<observed>` values ONLY as spike output to be filled in Task 3 Step 3 before commit — the task explicitly requires replacing them; no committed file keeps a placeholder. No TBD/TODO elsewhere.

**Type consistency:** `VideoProbe` defined in `virtual-person.ts` (Task 5), structurally consumed by `sync-oracle.ts` via a local `Probeable` interface (Task 4) — no import cycle. `SeedPaths`/`register_seed_items`/`generate_seed_fixtures` names consistent across Task 1 steps. `PHAROS_H264_BROWSER` consistent across Task 2/7. SyncPlay command names match the routes in `syncplay.rs` (`setnewqueue`/`pause`/`unpause`/`seek`/`nextitem`/`previousitem`).

**Known risk carried forward:** whether the nix-pinned `chromium` build decodes h264 is asserted at runtime by the h264 smoke's guard (Task 7 Step 2), which fails loudly with the google-chrome/allowUnfree remedy rather than silently skipping — acceptable per the spec's "h264 smoke is not optional".
