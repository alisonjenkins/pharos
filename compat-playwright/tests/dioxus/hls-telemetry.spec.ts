import { test, expect } from "@playwright/test";
import { delta, describe as line, errorEvents, settledClientEvents } from "../lib/client-events";
import {
  OPTED_IN_A,
  PHAROS_URL,
  groupLaunchOptions,
  seededMovie,
  signIn,
} from "../lib/together";

// Spec 031 / 016 US4 (V691, V692, V694): what the Rust HLS engine tells
// telemetry. A viewer who opted in to diagnostics plays the seeded clip with
// `?engine=rust`, seeks into time it has not fetched, and closes the player.
// The console carries one start, one seek (with the split between download
// and decode) and one closing summary, and the server counts the same three
// kinds, so the path is proven end to end, not only in the browser.
//
// Needs the server `just compat-dioxus-full` starts: `[server.native_api]`
// enabled, the seeded opted-in users and the 120 s `fixture-1.webm`.

// The watch page starts playing with no click behind it, muted.
test.use({ launchOptions: groupLaunchOptions });

/** `key=value` fields of a console line. */
function fields(line: string): Map<string, string> {
  return new Map([...line.matchAll(/(\w+)=(\S*)/g)].map(([, k, v]) => [k, v] as [string, string]));
}

test("the Rust engine reports its start, a seek to playing and a closing summary", async ({
  browser,
  request,
}) => {
  test.setTimeout(150_000);
  const { nativeId } = await seededMovie(request, "/fixture-1.webm");
  const { context, page, lines } = await signIn(browser, "a", OPTED_IN_A);
  // No native HLS and no direct play: the player has to go through MSE.
  await context.addInitScript(() => {
    const canPlayType = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function (type: string) {
      return /mpegurl|webm|matroska/i.test(type) ? "" : canPlayType.call(this, type);
    };
  });
  const logged = (prefix: string) => lines.filter((l) => l.includes(`pharos: playback ${prefix} engine=rust`));

  try {
    const before = await settledClientEvents(request, PHAROS_URL);
    await page.goto(`${PHAROS_URL}/ui/watch/${nativeId}?engine=rust`);
    await expect.poll(() => logged("start").length, { timeout: 60_000 }).toBe(1);
    const start = fields(logged("start")[0]);
    expect(start.get("end"), logged("start")[0]).toBe("playing");
    expect(Number(start.get("startup_ms"))).toBeGreaterThan(0);
    expect(start.get("first_index"), logged("start")[0]).not.toBe("none");

    const video = () =>
      page.evaluate(() => {
        const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
        return { t: v?.currentTime ?? 0, d: v?.duration ?? 0 };
      });
    await expect.poll(async () => (await video()).t, { timeout: 60_000 }).toBeGreaterThan(1);
    const { t: from, d } = await video();
    // Past the 30 s forward target, so the seek has to fetch.
    const target = Math.floor(Math.min(d - 10, from + 60));
    await page.evaluate((to) => {
      const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
      if (v) v.currentTime = to;
    }, target);
    await expect.poll(() => logged("seek").length, { timeout: 60_000 }).toBe(1);
    const seekLine = logged("seek")[0];
    const seek = fields(seekLine);
    expect(seek.get("end"), seekLine).toBe("playing");
    expect(seek.get("in_buffer"), seekLine).toBe("false");
    expect(Math.abs(Number(seek.get("to_ms")) - target * 1000), seekLine).toBeLessThan(1_000);
    expect(Number(seek.get("from_ms")), seekLine).toBeLessThan(target * 1000);
    expect(seek.get("first_index"), seekLine).not.toBe("none");
    expect(Number(seek.get("latency_ms")), seekLine).toBeGreaterThanOrEqual(
      Number(seek.get("first_appended_ms")),
    );

    // Closing the player drops the engine, which sends its last window. Back
    // only shrinks the player (spec 020); Close ends it. A full page load
    // would unload the wasm before anything could be sent.
    // The watch chrome hides after ~2 s idle; a pointer move brings it back.
    await page.mouse.move(600, 300);
    await page.mouse.move(640, 360);
    await page.getByRole("button", { name: "Back", exact: true }).click({ timeout: 10_000 });
    await page.locator(".pharos-stage").getByRole("button", { name: "Close" }).click();
    await expect(page.locator(".pharos-stage")).toHaveCount(0);
    await expect.poll(() => logged("summary").length, { timeout: 15_000 }).toBe(1);
    const summary = fields(logged("summary")[0]);
    expect(summary.get("reason"), logged("summary")[0]).toBe("closed");
    expect(Number(summary.get("video_segments"))).toBeGreaterThan(0);
    expect(summary.get("seeks"), logged("summary")[0]).toBe("1");

    const moved = delta(before, await settledClientEvents(request, PHAROS_URL));
    const count = (kind: string) =>
      [...moved].filter(([k]) => k.startsWith(`${kind}/`)).reduce((n, [, v]) => n + v, 0);
    expect(count("playbackStart"), line(moved)).toBe(1);
    expect(count("seek"), line(moved)).toBe(1);
    expect(count("playbackSummary"), line(moved)).toBe(1);
    expect(errorEvents(moved).size, line(moved)).toBe(0);
  } catch (e) {
    console.log(lines.filter((l) => l.includes("pharos:")).join("\n"));
    throw e;
  } finally {
    await context.close();
  }
});
