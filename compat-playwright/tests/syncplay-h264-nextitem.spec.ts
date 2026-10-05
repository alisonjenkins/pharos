import { test, expect } from "@playwright/test";
import { requireH264 } from "./lib/h264";
import { VirtualPerson } from "./lib/virtual-person";
import { waitUntilInSync } from "./lib/sync-oracle";

// Repro for jana's episode-swap hang (2026-10-05, Loki). Her Windows Chrome,
// swapped in-page to the next item, fetched `h264cmaf/init.mp4` 159 times in
// ~20 s and never requested a video segment, so it never posted Ready and the
// next-item readiness gate waited on her until the 30 s anti-wedge. The same
// browser loaded the same item fine after a page reload, and Firefox members
// swapped in 2-6 s. The group matrix swaps episodes on VP9 only, and the h264
// smoke swaps audio only, so the h264 next-item path was untested.
//
// Runs on an h264-capable chrome (PHAROS_H264_BROWSER). The failure signature
// is read from the member's own network, not just from the sync oracle, so a
// red run names what the player did instead of timing out.

const FIRST_ITEM = "5";
const SECOND_ITEM = "6";
// Only requests under the SECOND item count toward the swap: after nextItem()
// the old item's hls.js keeps buffering ahead, and counting those would let a
// swap that never loads still pass. Item ids are 32-hex in `/videos/{id}/` URLs.
const SECOND_ITEM_PATH = new RegExp(`/videos/${SECOND_ITEM.padStart(32, "0")}/`, "i");
const INIT_URL = /\/h264cmaf\/init\.mp4/;
const SEGMENT_URL = /\/h264cmaf\/[^/?]+\.m4s/;
const VIDEO_URL = /\/videos\/([0-9a-f]+)\/([^?]+)/;
// Healthy members refetch init a handful of times before the first segment
// (15-21 in prod, Firefox). The wedge was 159.
const INIT_REFETCH_CEILING = 40;
// How long a member gets to request its first video segment of the new item.
// Prod healthy members did it in 2-6 s; the wedge never did in 30 s. The
// GPU-less CI runner transcodes the cold new item in software, so it gets
// longer, the same way sync-oracle's SETTLE_MS does.
const FIRST_SEGMENT_MS = process.env.CI ? 60_000 : 25_000;
// The seeded fixtures 1-7 are unprobed VP9 clips (no bitrate, no size), which a
// real chrome direct-plays, so no HLS happens at all and a bitrate cap gives
// the server nothing to compare. Declaring no direct-play profiles makes
// pharos's real negotiation choose a transcode, and the 6 Mbps cap is jana's
// (`VideoBitrate=6000000` on her init URLs).
const STREAMING_BITRATE_CAP_BPS = 6_000_000;

async function forceTranscode(person: VirtualPerson): Promise<void> {
  await person.page.route(/\/Items\/[0-9a-f]+\/PlaybackInfo/i, async (route) => {
    const url = new URL(route.request().url());
    url.searchParams.set("MaxStreamingBitrate", String(STREAMING_BITRATE_CAP_BPS));
    const body: unknown = route.request().postDataJSON();
    if (typeof body !== "object" || body === null) {
      await route.continue({ url: url.toString() });
      return;
    }
    if ("DeviceProfile" in body) {
      const profile = (body as { DeviceProfile: Record<string, unknown> }).DeviceProfile;
      profile.MaxStreamingBitrate = STREAMING_BITRATE_CAP_BPS;
      profile.DirectPlayProfiles = [];
    }
    await route.continue({ url: url.toString(), postData: JSON.stringify(body) });
  });
}

interface Fetches {
  /** `h264cmaf/init.mp4` requests for the second item. */
  init: number;
  /** `h264cmaf` video segment requests for the second item. */
  segments: number;
  /** Every /videos/{id}/… request of any item, by item and path with segment
   *  numbers folded, so a red run says which rendition the player loaded. */
  byKind: Map<string, number>;
}

function countFetches(person: VirtualPerson): Fetches {
  const counts: Fetches = { init: 0, segments: 0, byKind: new Map() };
  person.page.on("request", (r) => {
    const url = r.url();
    if (SECOND_ITEM_PATH.test(url)) {
      if (INIT_URL.test(url)) counts.init += 1;
      else if (SEGMENT_URL.test(url)) counts.segments += 1;
    }
    const m = VIDEO_URL.exec(url);
    if (m?.[1] && m[2]) {
      const key = `item${m[1].replace(/^0+/, "") || "0"}:${m[2].replace(/\d+\.m4s$/, "N.m4s")}`;
      counts.byKind.set(key, (counts.byKind.get(key) ?? 0) + 1);
    }
  });
  return counts;
}

function summary(f: Fetches): string {
  return [...f.byKind].map(([k, n]) => `${k}×${n}`).join(", ") || "no /videos requests";
}

function reset(f: Fetches): void {
  f.init = 0;
  f.segments = 0;
  f.byKind.clear();
}

test.describe("syncplay h264 next-item swap (real-codec browser)", () => {
  test("a member swapped in-page to the next item loads it instead of refetching init", async ({
    browser,
  }) => {
    test.setTimeout(process.env.CI ? 300_000 : 180_000);
    await requireH264(browser);

    const a = await VirtualPerson.spawn(browser, 0);
    const b = await VirtualPerson.spawn(browser, 1);
    const people = [a, b] as const;
    await Promise.all(people.map(forceTranscode));
    const fetches = people.map(countFetches);

    const g = await a.createGroup("h264-nextitem");
    await b.joinGroup(g);
    await a.setNewQueue([FIRST_ITEM, SECOND_ITEM]);
    await a.unpause();
    // Baseline: the FIRST item is genuinely playing on both. The sync oracle
    // alone passes while a stalled <video> sits at t=0, so require progress.
    for (const p of people) {
      await expect
        .poll(async () => (await p.probe()).currentTime, {
          message: `${p.label} never progressed on the first item`,
          timeout: 30_000,
        })
        .toBeGreaterThan(0.5);
    }
    await waitUntilInSync([a, b]);

    const baseline = fetches.map(summary);
    fetches.forEach(reset);
    await a.nextItem();

    const stuck: string[] = [];
    for (const [i, p] of people.entries()) {
      const f = fetches[i];
      if (!f) continue;
      await expect
        .poll(() => f.segments, { timeout: FIRST_SEGMENT_MS })
        .toBeGreaterThan(0)
        .catch(() => {
          stuck.push(
            `${p.label} requested no h264cmaf video segment of item ${SECOND_ITEM} ` +
              `${FIRST_SEGMENT_MS} ms after the swap (init fetched ${f.init}×). ` +
              `Loaded after: ${summary(f)}. Loaded before: ${baseline[i]}\n${p.diagnostics()}`,
          );
        });
      expect
        .soft(
          f.init,
          `${p.label} refetched h264cmaf/init.mp4 ${f.init}× after the swap ` +
            `(${f.segments} segments)`,
        )
        .toBeLessThanOrEqual(INIT_REFETCH_CEILING);
    }
    expect(stuck, stuck.join("\n\n")).toEqual([]);
    await waitUntilInSync([a, b]);
    for (const p of people) {
      expect((await p.probe()).itemId, `${p.label} is not on item ${SECOND_ITEM}`).toBe(
        SECOND_ITEM,
      );
    }

    await a.close();
    await b.close();
  });
});
