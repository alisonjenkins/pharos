import { test, expect } from "@playwright/test";
import { PHAROS_URL, groupLaunchOptions, seededMovie, signIn } from "../lib/together";

// The Rust HLS engine plays a film split across two files as one stream
// (V1504): across the join without a stall, a seek into the second part, and
// on to `ended`. Two generated clips in one folder, 40 s and 35 s.
//
// Off unless PHAROS_HLS_EOS is set, which also makes the recipe generate the
// clips:
//   PHAROS_HLS_EOS=1 just compat-dioxus-full on 8096 playwright.hls-bench.config.ts
const FIRST_PART = "/split-audit cd1.webm";
const JOIN_S = 40;
const WHOLE_S = 75;

test.use({ launchOptions: groupLaunchOptions });

interface Probe {
  ended: number;
}

test("rust engine plays a film split across two files", async ({ browser, request }) => {
  test.skip(!process.env.PHAROS_HLS_EOS, "set PHAROS_HLS_EOS=1 to run the split-film test");
  test.setTimeout(360_000);
  const { api, movie, nativeId } = await seededMovie(request, FIRST_PART);
  // The first scan indexes the two files apart; the scan after both are
  // probed makes them one film. Ask for scans until the film runs for both.
  let asked = 0;
  await expect
    .poll(
      async () => {
        if (Date.now() - asked > 10_000) {
          asked = Date.now();
          await request.post(`${PHAROS_URL}/Library/Refresh`, { headers: api });
        }
        const got = await request.get(`${PHAROS_URL}/Items/${movie.Id}`, { headers: api });
        if (!got.ok()) return -got.status();
        return ((await got.json()).RunTimeTicks ?? 0) / 10_000_000;
      },
      { message: "the two files became one film", timeout: 120_000, intervals: [2_000] },
    )
    .toBeGreaterThan(WHOLE_S - 5);
  const { context, page, lines } = await signIn(browser, "rust");
  await context.addInitScript(() => {
    // Force HLS: without this the browser plays the WebM directly.
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function (type: string) {
      return /mpegurl|webm|matroska/i.test(type) ? "" : canPlayType.call(this, type);
    };
    const probe: Probe = { ended: 0 };
    (window as unknown as { pwSplit: Probe }).pwSplit = probe;
    new MutationObserver((_, obs) => {
      const v = document.getElementById("pharos-video");
      if (v instanceof HTMLVideoElement) {
        obs.disconnect();
        v.muted = true;
        v.addEventListener("ended", () => {
          probe.ended += 1;
        });
      }
    }).observe(document, { childList: true, subtree: true });
  });
  try {
    await page.goto(`${PHAROS_URL}/ui/watch/${nativeId}?engine=rust`);
    const video = () =>
      page.evaluate(() => {
        const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
        return { at: v?.currentTime ?? 0, duration: v?.duration ?? 0 };
      });
    const seek = (to: number) =>
      page.evaluate((at) => {
        (document.getElementById("pharos-video") as HTMLVideoElement).currentTime = at;
      }, to);
    const log = () => lines.join("\n");

    await expect.poll(async () => (await video()).at, { timeout: 60_000, message: log() }).toBeGreaterThan(0.5);
    // Both parts are one timeline.
    await expect
      .poll(async () => (await video()).duration, { timeout: 30_000, message: log() })
      .toBeGreaterThan(WHOLE_S - 5);

    // Play across the join.
    await seek(JOIN_S - 6);
    await expect
      .poll(async () => (await video()).at, { timeout: 60_000, message: log() })
      .toBeGreaterThan(JOIN_S + 4);

    // Back into the first part, then straight into the second.
    await seek(5);
    await expect.poll(async () => (await video()).at, { timeout: 30_000, message: log() }).toBeGreaterThan(6);
    await expect.poll(async () => (await video()).at, { timeout: 30_000, message: log() }).toBeLessThan(JOIN_S);
    await seek(WHOLE_S - 12);
    await expect
      .poll(() => page.evaluate(() => (window as unknown as { pwSplit: Probe }).pwSplit.ended), {
        timeout: 60_000,
        message: log(),
      })
      .toBe(1);

    const start = lines.find((l) => /pharos: playback engine=rust start /.test(l));
    expect(start, "played on the Rust engine").toBeTruthy();
    expect(start, "the engine saw two parts").toMatch(/ parts=2 /);
    expect(lines.filter((l) => /fallback=hlsjs/.test(l))).toEqual([]);
    expect(lines.filter((l) => /action=(starved|nudge|jump|give_up)/.test(l))).toEqual([]);
  } finally {
    await context.close();
  }
});
