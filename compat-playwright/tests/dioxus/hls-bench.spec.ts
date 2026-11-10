import { test, expect } from "@playwright/test";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
  PHAROS_URL,
  groupLaunchOptions,
  seededMovie,
  signIn,
} from "../lib/together";

// Spec 031 T031-02: play one stream for a fixed time on a named MSE engine
// and write what it did as JSON, so hls.js and the Rust engine can be
// compared on the same stream, browser and machine (T031-03, T031-17).
//
// Off unless PHAROS_HLS_BENCH is set, so the Dioxus suite skips it:
//   PHAROS_HLS_BENCH=1 just compat-dioxus-full on 8096 playwright.hls-bench.config.ts
//
// Env:
//   PHAROS_BENCH_ENGINES  comma list of hlsjs,rust (default both)
//   PHAROS_BENCH_SECONDS  play time per run, seek included (default 30)
//   PHAROS_BENCH_ITEM     path suffix of the seeded item (default /fixture-1.webm)
//   PHAROS_BENCH_OUT      directory for the JSON (default test-results/hls-bench)
//
// Each file holds one run: time to first frame, the seek's time to playing,
// buffered ranges once a second, long tasks and JS heap. Long tasks and heap
// are Chromium only (Firefox has neither API) and are null there.

const ENGINES = (process.env.PHAROS_BENCH_ENGINES ?? "hlsjs,rust")
  .split(",")
  .map((e) => e.trim())
  .filter(Boolean);
const SECONDS = Number(process.env.PHAROS_BENCH_SECONDS ?? "30");
const ITEM = process.env.PHAROS_BENCH_ITEM ?? "/fixture-1.webm";
const OUT = process.env.PHAROS_BENCH_OUT ?? "test-results/hls-bench";
/** Past the 30 s forward target of both engines, so the seek has to fetch. */
const SEEK_AHEAD_S = 40;

test.use({ launchOptions: groupLaunchOptions });

interface Sample {
  at_ms: number;
  current_s: number;
  buffered: [number, number][];
  ready_state: number;
  paused: boolean;
  heap_bytes: number | null;
}

for (const engine of ENGINES) {
  test(`hls bench: ${engine} plays ${ITEM} for ${SECONDS} s`, async ({
    browser,
    browserName,
    request,
  }) => {
    test.skip(
      !process.env.PHAROS_HLS_BENCH,
      "set PHAROS_HLS_BENCH=1 to run the HLS bench",
    );
    expect(["hlsjs", "rust"], "PHAROS_BENCH_ENGINES").toContain(engine);
    test.setTimeout((SECONDS + 90) * 1000);

    const { api, movie, nativeId } = await seededMovie(request, ITEM);
    // The previous run leaves a resume point; every run starts at 0.
    await request.post(`${PHAROS_URL}/Sessions/Playing/Progress`, {
      headers: api,
      data: {
        ItemId: movie.Id,
        PlaySessionId: `pw-bench-${Date.now()}`,
        PositionTicks: 0,
        IsPaused: true,
      },
    });
    const { context, page, lines } = await signIn(browser, engine);
    await context.addInitScript(() => {
      // No native HLS and no direct play: the stream has to go through MSE.
      const canPlayType = HTMLMediaElement.prototype.canPlayType;
      HTMLMediaElement.prototype.canPlayType = function (type: string) {
        return /mpegurl|webm|matroska/i.test(type)
          ? ""
          : canPlayType.call(this, type);
      };
      const bench = {
        longTasks: null as null | {
          count: number;
          total_ms: number;
          max_ms: number;
        },
        firstFrameMs: null as null | number,
      };
      // The first frame shown, from navigation start (the time origin).
      // Watched from the moment the element exists, so a fast start is not
      // missed between page load and the test asking.
      const watch = (v: HTMLVideoElement) => {
        const done = () => {
          bench.firstFrameMs ??= performance.now();
        };
        if (typeof v.requestVideoFrameCallback === "function")
          v.requestVideoFrameCallback(done);
        else v.addEventListener("playing", done, { once: true });
      };
      new MutationObserver((_, obs) => {
        const v = document.getElementById("pharos-video");
        if (v instanceof HTMLVideoElement) {
          obs.disconnect();
          watch(v);
        }
      }).observe(document, { childList: true, subtree: true });
      (window as unknown as { pwBench: typeof bench }).pwBench = bench;
      try {
        if (PerformanceObserver.supportedEntryTypes?.includes("longtask")) {
          bench.longTasks = { count: 0, total_ms: 0, max_ms: 0 };
          new PerformanceObserver((list) => {
            for (const e of list.getEntries()) {
              const t = bench.longTasks!;
              t.count += 1;
              t.total_ms += e.duration;
              t.max_ms = Math.max(t.max_ms, e.duration);
            }
          }).observe({ type: "longtask", buffered: true });
        }
      } catch {
        bench.longTasks = null;
      }
    });

    try {
      await page.goto(`${PHAROS_URL}/ui/watch/${nativeId}?engine=${engine}`);
      const video = page.locator("#pharos-video");
      await expect(video).toBeAttached({ timeout: 30_000 });
      await expect
        .poll(
          () =>
            page.evaluate(
              () =>
                (
                  window as unknown as {
                    pwBench: { firstFrameMs: number | null };
                  }
                ).pwBench.firstFrameMs,
            ),
          {
            timeout: 60_000,
          },
        )
        .not.toBeNull();
      const firstFrameMs = await page.evaluate(
        () =>
          (window as unknown as { pwBench: { firstFrameMs: number } }).pwBench
            .firstFrameMs,
      );

      const sample = () =>
        page.evaluate((): Omit<Sample, "at_ms"> & { now: number } => {
          const v = document.getElementById("pharos-video") as HTMLVideoElement;
          const buffered: [number, number][] = [];
          for (let i = 0; i < v.buffered.length; i++)
            buffered.push([v.buffered.start(i), v.buffered.end(i)]);
          const memory = (
            performance as unknown as { memory?: { usedJSHeapSize: number } }
          ).memory;
          return {
            now: performance.now(),
            current_s: v.currentTime,
            buffered,
            ready_state: v.readyState,
            paused: v.paused,
            heap_bytes: memory ? memory.usedJSHeapSize : null,
          };
        });

      const samples: Sample[] = [];
      let seek: {
        from_s: number;
        to_s: number;
        to_playing_ms: number | null;
      } | null = null;
      const half = Math.floor(SECONDS / 2);
      for (let s = 0; s < SECONDS; s++) {
        const { now, ...rest } = await sample();
        samples.push({ at_ms: Math.round(now), ...rest });
        if (s === half) {
          seek = await page.evaluate(
            ({ ahead, limitMs }) =>
              new Promise<{
                from_s: number;
                to_s: number;
                to_playing_ms: number | null;
              }>((resolve) => {
                const v = document.getElementById(
                  "pharos-video",
                ) as HTMLVideoElement;
                const from = v.currentTime;
                const to = Math.min(from + ahead, Math.max(0, v.duration - 10));
                const start = performance.now();
                const timer = setTimeout(
                  () =>
                    resolve({ from_s: from, to_s: to, to_playing_ms: null }),
                  limitMs,
                );
                // Playing again: the head moves past the target after the seek.
                const moved = () => {
                  if (!v.seeking && v.currentTime > to + 0.05) {
                    clearTimeout(timer);
                    v.removeEventListener("timeupdate", moved);
                    resolve({
                      from_s: from,
                      to_s: to,
                      to_playing_ms: performance.now() - start,
                    });
                  }
                };
                v.addEventListener("timeupdate", moved);
                v.currentTime = to;
              }),
            { ahead: SEEK_AHEAD_S, limitMs: 30_000 },
          );
        } else {
          await page.waitForTimeout(1_000);
        }
      }
      const longTasks = await page.evaluate(
        () =>
          (window as unknown as { pwBench: { longTasks: unknown } }).pwBench
            .longTasks,
      );

      const fellBack = lines.filter((l) => l.includes("fallback="));
      const chosen =
        lines.find((l) => /pharos: playback engine=\w+ reason=/.test(l)) ??
        null;
      const report = {
        recorded_at: new Date().toISOString(),
        browser: browserName,
        browser_version: browser.version(),
        pharos_url: PHAROS_URL,
        item: ITEM,
        engine,
        engine_line: chosen,
        fallbacks: fellBack,
        seconds: SECONDS,
        first_frame_ms: Math.round(firstFrameMs),
        seek: seek && {
          ...seek,
          to_playing_ms: seek.to_playing_ms && Math.round(seek.to_playing_ms),
        },
        long_tasks: longTasks,
        samples,
        errors: lines.filter((l) => /playback error|pageerror/.test(l)),
      };
      mkdirSync(OUT, { recursive: true });
      const file = join(
        OUT,
        `${browserName}-${engine}-${report.recorded_at.replace(/[:.]/g, "")}.json`,
      );
      writeFileSync(file, `${JSON.stringify(report, null, 2)}\n`);
      console.log(`hls bench: wrote ${file}`);

      // A run that fell back measured the other engine.
      expect(fellBack, "the engine fell back").toEqual([]);
      expect(chosen, lines.join("\n")).toContain(`engine=${engine} `);
      expect(seek?.to_playing_ms, "the seek never played").not.toBeNull();
    } finally {
      await context.close();
    }
  });
}
