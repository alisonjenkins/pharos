import {
  test,
  expect,
  APIRequestContext,
  Browser,
  BrowserContext,
  Page,
} from "@playwright/test";
import {
  Probeable,
  VideoProbeLike,
  assertPausedAll,
  assertSeekConverged,
  waitUntilInSync,
} from "../lib/sync-oracle";

// 014 T026 - watch together in the pharos Dioxus UI, in two browsers.
//
// A starts a group from a title's "Watch together"; B opens the invite link
// `/ui/together/{group}` and joins. The group's play, pause and seek then
// reach both `<video>`s, held to `lib/sync-oracle.ts`'s tolerances.
//
// Needs the same server as the rest of this suite: `[server.native_api]`
// enabled, the seeded user and the seeded `fixture-1.webm` (see
// `just compat-dioxus-full`).

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

// Joining is a click, which is the gesture playback needs; headless Chromium
// still wants autoplay allowed for the play the group schedules later.
test.use({
  launchOptions: { args: ["--autoplay-policy=no-user-gesture-required"] },
});

/** A member's `<video>` as the sync oracle reads it. */
class Member implements Probeable {
  constructor(
    readonly label: string,
    readonly page: Page,
  ) {}

  async probe(): Promise<VideoProbeLike> {
    return this.page.evaluate(() => {
      const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
      return {
        itemId: v?.getAttribute("data-item-id") ?? null,
        currentTime: v?.currentTime ?? 0,
        paused: v?.paused ?? true,
        errorCode: v?.error?.code ?? null,
      };
    });
  }

  diagnostics(): string {
    return this.label;
  }
}

/** The group ids `SyncPlay/List` shows the user. */
async function listedGroups(
  request: APIRequestContext,
  api: Record<string, string>,
): Promise<string[]> {
  const res = await request.get(`${PHAROS_URL}/SyncPlay/List`, { headers: api });
  const groups: { GroupId: string }[] = await res.json();
  return groups.map((g) => g.GroupId);
}

/**
 * Click A's `<video>` until A's own player reaches `paused`, then return.
 * The click is a group command, so the player changes only when the server's
 * answer comes back; a click that lands before the page has wired its player
 * to the group is lost, so it is sent again rather than waited on forever.
 * A click while A still shows the old state asks for the same new state, so
 * a repeat never undoes one in flight.
 */
async function toggleUntil(member: Member, paused: boolean) {
  for (let attempt = 0; attempt < 3; attempt++) {
    await member.page.locator("#pharos-video").click();
    const took = await expect
      .poll(() => member.probe().then((p) => p.paused), { timeout: 5_000 })
      .toBe(paused)
      .then(() => true)
      .catch(() => false);
    if (took) {
      // A repeat is worth seeing: it means a click went unheard.
      if (attempt > 0) {
        test.info().annotations.push({
          type: "lost-click",
          description: `${member.label} paused=${paused} took ${attempt + 1} clicks`,
        });
      }
      return;
    }
  }
  throw new Error(`${member.label}'s player never became paused=${paused}`);
}

async function signIn(browser: Browser, label: string) {
  const context = await browser.newContext();
  // A context of its own skips the config's storage: keep what's new off.
  await context.addInitScript(() => localStorage.setItem("pharos.whats_new", "off"));
  const page = await context.newPage();
  const lines: string[] = [];
  page.on("console", (m) => lines.push(`[${label}] ${m.text()}`));
  page.on("pageerror", (e) => lines.push(`[${label}] pageerror: ${e.message}`));
  await page.goto(`${PHAROS_URL}/ui/`);
  await page.locator("#signin-name").fill(TEST_USER);
  await page.locator("#signin-password").fill(TEST_PASS);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
  return { context, page, lines };
}

/** The seeded movie `file`: its native id, and an API token for the user. */
async function seededMovie(request: APIRequestContext, file: string) {
  const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
    headers: { Accept: "application/json" },
  });
  test.skip(
    probe.status() !== 405,
    `native API is not enabled (status ${probe.status()}); skipping native test`,
  );
  const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
    headers: {
      "Content-Type": "application/json",
      "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-together-${Date.now()}", Version="0"`,
    },
    data: { Username: TEST_USER, Pw: TEST_PASS },
  });
  test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
  const { AccessToken: token, User: user } = await auth.json();
  const api = { "X-Emby-Token": token };
  const movies = await (
    await request.get(
      `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path`,
      { headers: api },
    )
  ).json();
  const movie = movies.Items?.find((i: { Path?: string }) => i.Path?.endsWith(file));
  test.skip(!movie, `pharos has no seeded ${file}`);
  const nativeId = BigInt(`0x${movie.Id}`).toString();
  return { api, movie, nativeId };
}

/** Two signed-in browsers in one group on the seeded movie `file`. */
async function startGroup(
  browser: Browser,
  request: APIRequestContext,
  open: { context: BrowserContext; page: Page }[],
  file: string,
  opts: { subtitled?: boolean } = {},
) {
  const { api, movie, nativeId } = await seededMovie(request, file);
  if (opts.subtitled) {
    // The startup rescan probes the fixture; its streams appear once it has.
    await expect
      .poll(
        async () => {
          const info = await request.post(`${PHAROS_URL}/Items/${movie.Id}/PlaybackInfo`, {
            headers: api,
            data: {},
          });
          return ((await info.json()).MediaSources?.[0]?.MediaStreams ?? []).filter(
            (s: { Type: string }) => s.Type === "Subtitle",
          ).length;
        },
        { message: `${file} has a probed subtitle stream`, timeout: 60_000 },
      )
      .toBeGreaterThan(0);
  }

  const a = await signIn(browser, "A");
  open.push(a);
  const b = await signIn(browser, "B");
  open.push(b);
  const logs = () => [...a.lines, ...b.lines].join("\n");
  // Groups another test or an earlier try left are not A's.
  const before = new Set(await listedGroups(request, api));

  // A starts a group on the title and lands on the watch page as its member.
  await a.page.goto(`${PHAROS_URL}/ui/title/${nativeId}`);
  await a.page.getByRole("button", { name: /^Watch together( from start)?$/ }).click();
  await expect(a.page).toHaveURL(new RegExp(`/ui/watch/${nativeId}$`));
  await expect(a.page.locator(".pharos-members")).toBeVisible({ timeout: 15_000 });

  // B follows the invite link and joins.
  let groupId = "";
  await expect
    .poll(
      async () => {
        const made = (await listedGroups(request, api)).filter((g) => !before.has(g));
        groupId = made[0] ?? "";
        return made.length;
      },
      { message: "A's group is listed", timeout: 10_000 },
    )
    .toBe(1);
  await b.page.goto(`${PHAROS_URL}/ui/together/${groupId.toLowerCase()}`);
  await b.page.getByRole("button", { name: "Join" }).click();
  await expect(b.page).toHaveURL(new RegExp(`/ui/watch/${nativeId}$`));
  await expect(b.page.locator(".pharos-members")).toBeVisible({ timeout: 15_000 });
  // Both see both members.
  for (const p of [a.page, b.page]) {
    await expect(p.locator(".pharos-member-list li")).toHaveCount(2, {
      timeout: 15_000,
    });
  }
  const people = [new Member("A", a.page), new Member("B", b.page)];
  return { a, b, people, logs };
}

/** A failed step's error: its message, both pages' text and the browser logs. */
async function failure(e: unknown, a: { page: Page }, b: { page: Page }, logs: () => string) {
  const dom = async (p: Page) => p.locator("body").innerText().catch(() => "(no page)");
  return new Error(
    `${(e as Error).message}\n--- A page ---\n${await dom(a.page)}\n--- B page ---\n${await dom(b.page)}\n--- browser logs ---\n${logs()}`,
  );
}

/**
 * Pick `value` in `page`'s select named `name` until `landed` holds. A pick
 * is a group command, lost if it is sent before the page has wired its player
 * to the group, so it is sent again rather than waited on forever.
 */
async function pickUntil(
  page: Page,
  name: string,
  value: string,
  landed: () => Promise<unknown>,
) {
  for (let attempt = 0; attempt < 4; attempt++) {
    await page.getByRole("combobox", { name, exact: true }).selectOption(value);
    const took = await landed().then(
      () => true,
      () => false,
    );
    if (took) return;
  }
  throw new Error(`"${name}" never took ${value}`);
}

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
