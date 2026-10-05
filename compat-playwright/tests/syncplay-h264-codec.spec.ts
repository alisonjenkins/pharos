import { test, expect } from "@playwright/test";
import { requireH264 } from "./lib/h264";
import { VirtualPerson } from "./lib/virtual-person";
import { waitUntilInSync } from "./lib/sync-oracle";

// Guards the PR#70 class: the FOSS Playwright chromium cannot decode h264, so
// the VP9 matrix is structurally blind to the demuxed-CMAF path real
// Firefox/Zen clients use. This smoke runs on an h264-capable chromium
// (PHAROS_H264_BROWSER): jellyfin-web then advertises h264 → pharos serves the
// demuxed CMAF. We assert it actually decodes + plays, and that an audio swap
// reuses the cached video without desyncing the group.

test.describe("syncplay h264 codec smoke (real-codec browser)", () => {
  test("h264 demuxed-CMAF actually decodes, and an audio swap keeps sync", async ({
    browser,
  }) => {
    test.setTimeout(150_000);

    // The smoke is meaningless unless this browser truly decodes h264.
    await requireH264(browser);

    const a = await VirtualPerson.spawn(browser, 0);
    const g = await a.createGroup("h264-smoke");
    const b = await VirtualPerson.spawn(browser, 1);
    await b.joinGroup(g);
    await a.setNewQueue(["8"]); // multitrack; browser advertises h264 → demuxed CMAF
    await a.unpause();
    await waitUntilInSync([a, b]); // proves h264 CMAF decoded + rolling

    for (const p of [a, b]) {
      const pr = await p.probe();
      expect(pr.errorCode, `${p.label} video.error`).toBeNull();
    }

    await b.swapAudio(); // the PR#70 class: swap must reuse cached video
    await waitUntilInSync([a, b]);
    await a.close();
    await b.close();
  });
});
