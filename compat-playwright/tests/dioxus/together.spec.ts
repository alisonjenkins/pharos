import { test, expect, BrowserContext, Page } from "@playwright/test";
import { assertPausedAll, assertSeekConverged, waitUntilInSync } from "../lib/sync-oracle";
import {
  Member,
  PHAROS_URL,
  TEST_USER,
  failure,
  pickUntil,
  seededMovie,
  signIn,
  startGroup,
  toggleUntil,
} from "../lib/together";

// 014 T026 - watch together in the pharos Dioxus UI, in two browsers.
//
// A starts a group from a title's "Watch together"; B opens the invite link
// `/ui/together/{group}` and joins. The group's play, pause and seek then
// reach both `<video>`s, held to `lib/sync-oracle.ts`'s tolerances.
//
// Needs the same server as the rest of this suite: `[server.native_api]`
// enabled, the seeded user and the seeded `fixture-1.webm` (see
// `just compat-dioxus-full`).

// Joining is a click, which is the gesture playback needs; headless Chromium
// still wants autoplay allowed for the play the group schedules later.
test.use({
  launchOptions: { args: ["--autoplay-policy=no-user-gesture-required"] },
});

test.describe("pharos Dioxus UI watch together", () => {
  // Each try's browsers, so afterEach leaves the group even when the test
  // fails. A group left behind would show in the next try's list (retries
  // run in the same server).
  let open: { context: BrowserContext; page: Page }[] = [];

  test.afterEach(async () => {
    for (const { context, page } of open) {
      // The overlay hides when the pointer rests; moving wakes it.
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

  test("two browsers play, pause and seek in step", async ({ browser, request }) => {
    test.setTimeout(150_000);
    const { a, b, people, logs } = await startGroup(browser, request, open, "/fixture-1.webm");
    try {
      const [byA] = people;

      // The group starts by itself once both members are ready.
      await waitUntilInSync(people);

      // A pauses; B pauses.
      await toggleUntil(byA, true);
      await assertPausedAll(people);

      // A plays again; the group resumes both together.
      await toggleUntil(byA, false);
      await waitUntilInSync(people);
      await toggleUntil(byA, true);
      await assertPausedAll(people);

      // A seeks with the bar; B converges on the same position.
      const bar = a.page.locator("#pharos-seek");
      const box = await bar.boundingBox();
      expect(box, "the seek bar is drawn").not.toBeNull();
      await bar.click({
        position: { x: (box?.width ?? 0) * 0.25, y: (box?.height ?? 2) / 2 },
      });
      await expect
        .poll(() => people[0].probe().then((p) => p.currentTime), { timeout: 15_000 })
        .toBeGreaterThan(5);
      const target = (await people[0].probe()).currentTime * 1000;
      await assertSeekConverged(people, target);
    } catch (e) {
      throw await failure(e, a, b, logs);
    }
  });

  // V479: A is already watching when they start the group from the player.
  // The group starts where A's video is and A's video plays on; the invite
  // link the player shows brings B in at the group's place.
  test("a group started mid-watch keeps A's place and its invite link brings B in", async ({
    browser,
    request,
  }) => {
    test.setTimeout(150_000);
    const { nativeId } = await seededMovie(request, "/fixture-1.webm");
    const a = await signIn(browser, "A");
    open.push(a);
    const b = await signIn(browser, "B");
    open.push(b);
    const logs = () => [...a.lines, ...b.lines].join("\n");
    const byA = new Member("A", a.page);
    const byB = new Member("B", b.page);
    try {
      // A watches alone for a few seconds.
      await a.page.goto(`${PHAROS_URL}/ui/watch/${nativeId}`);
      await a.page.evaluate(() => {
        const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
        if (v) v.muted = true;
      });
      await expect
        .poll(() => byA.probe().then((p) => (p.paused ? 0 : p.currentTime)), {
          message: "A's video plays",
          timeout: 30_000,
        })
        .toBeGreaterThan(3);
      const before = (await byA.probe()).currentTime;
      await expect(a.page.locator(".pharos-members")).toHaveCount(0);

      // A starts the group from the player's top bar.
      await a.page.mouse.move(200, 200);
      await a.page.getByRole("button", { name: "Watch together" }).click();
      await expect(a.page.locator(".pharos-members")).toBeVisible({ timeout: 15_000 });
      await expect(a.page).toHaveURL(new RegExp(`/ui/watch/${nativeId}$`));
      await expect(a.page.getByRole("button", { name: "Watch together" })).toHaveCount(0);

      // The invite panel opens with the group's link.
      const link = a.page.getByRole("textbox", { name: "Invite link" });
      await expect(link).toBeVisible({ timeout: 10_000 });
      const url = await link.inputValue();
      expect(url).toMatch(new RegExp(`^${PHAROS_URL}/ui/together/[A-Za-z0-9_-]+$`));
      await a.page.getByRole("button", { name: "Copy link" }).click();
      await expect(a.page.locator(".pharos-invite-note")).toHaveText(
        /^(Link copied|Press Ctrl\+C to copy the link)$/,
      );

      // A's video stayed where it was and plays on in the group.
      await expect
        .poll(() => byA.probe().then((p) => (p.paused ? 0 : p.currentTime)), {
          message: "A plays on from where they were",
          timeout: 15_000,
        })
        .toBeGreaterThan(before);
      await a.page.getByRole("button", { name: "Done" }).click();
      await expect(link).toHaveCount(0);

      // B follows the link and joins at the group's place.
      await b.page.goto(url);
      await b.page.getByRole("button", { name: "Join" }).click();
      await expect(b.page).toHaveURL(new RegExp(`/ui/watch/${nativeId}$`));
      for (const p of [a.page, b.page]) {
        await expect(p.locator(".pharos-member-list li")).toHaveCount(2, { timeout: 15_000 });
      }
      await waitUntilInSync([byA, byB]);
      expect((await byB.probe()).currentTime).toBeGreaterThan(before);
    } catch (e) {
      throw await failure(e, a, b, logs);
    }
  });

  // B590/V508: a member who opens a song while their group stays on a film
  // plays it on their own, with the queue's Mix button.
  test("a song opened while in a group on a film plays on its own, with Mix", async ({
    browser,
    request,
  }) => {
    test.setTimeout(120_000);
    const { nativeId } = await seededMovie(request, "/fixture-1.webm");
    const a = await signIn(browser, "A");
    open.push(a);
    const byA = new Member("A", a.page);
    try {
      await a.page.goto(`${PHAROS_URL}/ui/watch/${nativeId}`);
      await expect
        .poll(() => byA.probe().then((p) => (p.paused ? 0 : p.currentTime)), {
          message: "A's video plays",
          timeout: 30_000,
        })
        .toBeGreaterThan(1);
      await a.page.mouse.move(200, 200);
      await a.page.getByRole("button", { name: "Watch together" }).click();
      await expect(a.page.locator(".pharos-members")).toBeVisible({ timeout: 15_000 });
      await a.page.getByRole("button", { name: "Done" }).click();

      // A leaves the film in the app, still in the group, and finds a song.
      await a.page.mouse.move(220, 220);
      await a.page.getByRole("button", { name: "Back" }).click();
      await a.page.getByRole("combobox", { name: "Search all libraries" }).fill("carefree");
      const hit = a.page
        .getByRole("option")
        .filter({ hasText: /carefree/i })
        .first()
        .getByRole("link");
      await expect(hit).toBeVisible({ timeout: 15_000 });
      await hit.click();
      await a.page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
      // The film was shrunk, so the song may start in the small player.
      await expect(a.page.locator(".pharos-listen")).toHaveCount(1, { timeout: 15_000 });
      const expand = a.page.locator(".pharos-stage").getByRole("button", { name: "Expand" });
      if (await expand.isVisible()) {
        await expand.click();
      }
      await expect(a.page.locator(".pharos-listen")).toBeVisible({ timeout: 15_000 });

      await a.page.locator(".pharos-queue-toggle").click();
      const panel = a.page.getByRole("complementary", { name: "Queue" });
      await expect(panel).toBeVisible();
      await expect(panel.getByRole("button", { name: "Mix", exact: true })).toBeVisible();
    } catch (e) {
      throw await failure(e, a, a, () => a.lines.join("\n"));
    }
  });

  // 014 US7 and US8 (T042). 07-test-subtitles.webm carries one audio track and
  // one English WebVTT subtitle; fixture-1.webm has neither a second audio nor
  // any subtitle, so the group's pick would have nothing to land on there.
  test("the group's activity and subtitle pick reach B, unless B is on their own tracks", async ({
    browser,
    request,
  }) => {
    test.setTimeout(150_000);
    const { a, b, people, logs } = await startGroup(
      browser,
      request,
      open,
      "/07-test-subtitles.webm",
      { subtitled: true },
    );
    try {
      const [byA] = people;
      await waitUntilInSync(people);

      // A pauses; B's page says so, naming the member (both browsers sign in
      // as the same user, so the name is that user's).
      await toggleUntil(byA, true);
      await expect(b.page.locator(".pharos-activity-line").last()).toHaveText(
        new RegExp(`^${TEST_USER} paused at \\d+:\\d\\d$`),
        { timeout: 10_000 },
      );

      // A picks the subtitle; the group moves B's picker with it.
      const subtitles = (p: Page) => p.getByRole("combobox", { name: "Subtitles", exact: true });
      const track = await subtitles(a.page).locator("option").nth(1).getAttribute("value");
      expect(track, "a subtitle track is offered").toMatch(/^\d+$/);
      await expect(subtitles(b.page)).toHaveValue("off");
      await pickUntil(a.page, "Subtitles", track!, () =>
        expect(subtitles(b.page)).toHaveValue(track!, { timeout: 5_000 }),
      );
      await expect(subtitles(a.page)).toHaveValue(track!);
      await expect(b.page.locator(".pharos-activity-line").last()).toContainText(
        "turned subtitles to",
      );

      // B takes "Just me"; A's strip marks B.
      await pickUntil(b.page, "Tracks", "me", () =>
        expect(a.page.locator(".pharos-activity-line").last()).toHaveText(
          /is on their own tracks$/,
          { timeout: 5_000 },
        ),
      );
      await expect(b.page.getByRole("combobox", { name: "Tracks", exact: true })).toHaveValue("me");

      // A turns subtitles off. B hears it (its page says so) and keeps its own.
      await pickUntil(a.page, "Subtitles", "off", () =>
        expect(b.page.locator(".pharos-activity-line").last()).toHaveText(
          /turned subtitles off$/,
          { timeout: 5_000 },
        ),
      );
      await expect(subtitles(a.page)).toHaveValue("off");
      await expect(subtitles(b.page)).toHaveValue(track!);

      // Back on "Follow group": B takes the group's pick (off) again.
      await pickUntil(b.page, "Tracks", "follow", () =>
        expect(subtitles(b.page)).toHaveValue("off", { timeout: 5_000 }),
      );
    } catch (e) {
      throw await failure(e, a, b, logs);
    }
  });
});
