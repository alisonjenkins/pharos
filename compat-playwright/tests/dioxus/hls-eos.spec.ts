import { test, expect } from "@playwright/test";
import { PHAROS_URL, groupLaunchOptions, seededMovie, signIn } from "../lib/together";

// The Rust HLS engine plays a clip to its end and fires `ended`, with no
// stall on the way (B1493, B1501). Two generated clips: 90 s leaves a short
// sliver as the last HLS segment, 87 s ends on a whole one.
//
// Off unless PHAROS_HLS_EOS is set, which also makes the recipe generate the
// clips:
//   PHAROS_HLS_EOS=1 just compat-dioxus-full on 8096 playwright.hls-bench.config.ts
const CLIPS = ["/eos-audit-90.webm", "/eos-audit-87.webm"];
// Far enough from the end that the back buffer is trimmed after the engine
// has ended the stream, which is what used to reopen it.
const BEFORE_END_S = 45;

test.use({ launchOptions: groupLaunchOptions });

interface Probe {
  ended: number;
}

for (const clip of CLIPS) {
  test(`rust engine plays ${clip} to the end`, async ({ browser, request }) => {
    test.skip(!process.env.PHAROS_HLS_EOS, "set PHAROS_HLS_EOS=1 to run the play-to-the-end test");
    test.setTimeout(180_000);
    const { nativeId } = await seededMovie(request, clip);
    const { context, page, lines } = await signIn(browser, "rust");
    await context.addInitScript(() => {
      // Force HLS: without this the browser plays the WebM directly.
      const canPlayType = HTMLMediaElement.prototype.canPlayType;
      HTMLMediaElement.prototype.canPlayType = function (type: string) {
        return /mpegurl|webm|matroska/i.test(type) ? "" : canPlayType.call(this, type);
      };
      const probe: Probe = { ended: 0 };
      (window as unknown as { pwEos: Probe }).pwEos = probe;
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
      const video = () => page.evaluate(() => {
        const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
        return { at: v?.currentTime ?? 0, duration: v?.duration ?? 0 };
      });
      await expect.poll(async () => (await video()).at, { timeout: 60_000 }).toBeGreaterThan(0.5);
      await expect.poll(async () => (await video()).duration, { timeout: 30_000 }).toBeGreaterThan(60);
      await page.evaluate((before) => {
        const v = document.getElementById("pharos-video") as HTMLVideoElement;
        v.currentTime = Math.max(0, v.duration - before);
      }, BEFORE_END_S);

      await expect
        .poll(() => page.evaluate(() => (window as unknown as { pwEos: Probe }).pwEos.ended), {
          timeout: (BEFORE_END_S + 30) * 1_000,
          message: lines.join("\n"),
        })
        .toBe(1);
      expect(lines.some((l) => /pharos: playback engine=rust start /.test(l)), "played on the Rust engine").toBe(true);
      expect(lines.filter((l) => /action=(starved|nudge|jump|give_up)/.test(l))).toEqual([]);
    } finally {
      await context.close();
    }
  });
}
