import { test, expect, BrowserContext, Page } from "@playwright/test";
import { assertPausedAll, assertSeekConverged, waitUntilInSync } from "../lib/sync-oracle";
import { failure, groupLaunchOptions, startGroup, toggleUntil } from "../lib/together";

// Spec 034: a group where each member picks 2D or 3D. The seed (`just
// compat-dioxus-full`) adds "Stereo Film (2009)" with a `- 3D.HSBS` edition,
// one item with two versions; its primary is the 2D file (ADR-0080).
//
// A starts a group on it and B joins, both on 2D. B switches to 3D from the
// player (US3, V903): B renegotiates on the 3D file at the group's place, stays
// in the group, and the group plays on with A on 2D and B on 3D (US1).

test.use({ launchOptions: groupLaunchOptions });

const STREAM = /\/(videos|Videos)\/[^/]+\/(stream|master|main)/;
const THREE_D = /MediaSourceId=[^&]*-3d-hsbs/i;

test.describe("watch together on different versions (spec 034)", () => {
  let open: { context: BrowserContext; page: Page }[] = [];

  test.afterEach(async () => {
    for (const { context, page } of open) {
      await page.mouse.move(20, 20).catch(() => {});
      await page
        .locator(".pharos-members")
        .getByRole("button", { name: "Leave" })
        .click({ timeout: 5_000 })
        .catch(() => {});
      await context.close();
    }
    open = [];
  });

  test("a member switches to 3D from the player and stays in step on 2D's timeline", async ({
    browser,
    request,
  }) => {
    test.setTimeout(150_000);
    const { a, b, people, logs } = await startGroup(
      browser,
      request,
      open,
      "/Stereo Film (2009).webm",
    );
    try {
      const [byA] = people;
      await waitUntilInSync(people);
      // Paused, so the 10 s clip does not run out across the switch.
      await toggleUntil(byA, true);
      await assertPausedAll(people);
      const at = (await byA.probe()).currentTime * 1000;

      const bStreams: string[] = [];
      b.page.on("request", (r) => {
        if (STREAM.test(r.url())) bStreams.push(r.url());
      });
      const aStreams: string[] = [];
      a.page.on("request", (r) => {
        if (STREAM.test(r.url())) aStreams.push(r.url());
      });
      const switched = b.page.waitForEvent("console", {
        predicate: (m) => /pharos: version switch /.test(m.text()),
        timeout: 20_000,
      });
      await b.page.mouse.move(200, 200);
      const picker = b.page.getByRole("combobox", { name: "Version" });
      await expect(picker).toBeVisible({ timeout: 15_000 });
      await expect(picker.locator("option")).toHaveCount(2);
      const threeD = picker.locator("option", { hasText: "3D (half side-by-side)" });
      await picker.selectOption((await threeD.getAttribute("value"))!);
      const line = (await switched).text();
      expect(line).toContain('from="" ');
      expect(line).toContain("grouped=true");

      // B plays the 3D file now; A stays on 2D.
      await expect
        .poll(() => bStreams.find((u) => THREE_D.test(u)) ?? "", { timeout: 20_000 })
        .not.toBe("");
      expect(aStreams.filter((u) => THREE_D.test(u))).toEqual([]);

      // B is still a member, at the group's place.
      for (const p of [a.page, b.page]) {
        await expect(p.locator(".pharos-member-list li")).toHaveCount(2, { timeout: 15_000 });
      }
      await assertPausedAll(people);
      await assertSeekConverged(people, at);

      // The group plays on with both.
      await toggleUntil(byA, false);
      await waitUntilInSync(people);
      expect(logs()).not.toMatch(/\[B\] pharos: group (left|rejoin)/);
    } catch (e) {
      throw await failure(e, a, b, logs);
    }
  });
});
