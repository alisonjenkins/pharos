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
    const movie = movies.Items?.find((i: { Path?: string }) =>
      i.Path?.endsWith("/fixture-1.webm"),
    );
    test.skip(!movie, "pharos has no seeded movie");
    const nativeId = BigInt(`0x${movie.Id}`).toString();

    const a = await signIn(browser, "A");
    open.push(a);
    const b = await signIn(browser, "B");
    open.push(b);
    const logs = () => [...a.lines, ...b.lines].join("\n");
    try {
      // Groups another test or an earlier try left are not A's.
      const before = new Set(await listedGroups(request, api));

      // A starts a group on the title and lands on the watch page as its member.
      await a.page.goto(`${PHAROS_URL}/ui/title/${nativeId}`);
      await a.page.getByRole("button", { name: "Watch together" }).click();
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
      const dom = async (p: Page) =>
        p.locator("body").innerText().catch(() => "(no page)");
      throw new Error(
        `${(e as Error).message}\n--- A page ---\n${await dom(a.page)}\n--- B page ---\n${await dom(b.page)}\n--- browser logs ---\n${logs()}`,
      );
    }
  });
});
