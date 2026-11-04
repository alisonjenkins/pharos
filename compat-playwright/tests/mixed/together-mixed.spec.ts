import { test, expect, Browser, Page } from "@playwright/test";
import { VirtualPerson } from "../lib/virtual-person";
import {
  Probeable,
  VideoProbeLike,
  assertPausedAll,
  assertSeekConverged,
  waitUntilInSync,
} from "../lib/sync-oracle";

// 014 T028 - a pharos Dioxus UI browser and an unmodified jellyfin-web browser
// in ONE SyncPlay group, held to `lib/sync-oracle.ts`'s tolerances through
// play, pause and seek. Both orders run: jellyfin-web creates, then the
// Dioxus UI creates. Commands originate on both sides in each order.
//
// Needs `just compat-dioxus-full on 8096 playwright.mixed.config.ts`.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

/** Item ids are decimal on the Dioxus side and may be 32-hex in jellyfin-web's
 *  PlayQueue; the oracle compares them for equality, so make both decimal. */
function canonicalItemId(id: string | null): string | null {
  if (id === null) return null;
  try {
    // All-digit 32-hex ids (00…01) look decimal, so key on the length.
    return /^[0-9a-f]{32}$/i.test(id) ? BigInt(`0x${id}`).toString() : id;
  } catch {
    return id;
  }
}

class JellyfinWebMember implements Probeable {
  constructor(readonly person: VirtualPerson) {}
  get label(): string {
    return `jellyfin-web(${this.person.label})`;
  }
  async probe(): Promise<VideoProbeLike> {
    const p = await this.person.probe();
    return { ...p, itemId: canonicalItemId(p.itemId) };
  }
  diagnostics(): string {
    return this.person.diagnostics();
  }
}

class DioxusMember implements Probeable {
  readonly label = "pharos-ui";
  constructor(readonly page: Page) {}
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
}

async function dioxusSignIn(browser: Browser) {
  const context = await browser.newContext();
  const page = await context.newPage();
  const lines: string[] = [];
  page.on("console", (m) => lines.push(`[pharos-ui] ${m.text()}`));
  page.on("pageerror", (e) => lines.push(`[pharos-ui] pageerror: ${e.message}`));
  await page.goto(`${PHAROS_URL}/ui/`);
  await page.locator("#signin-name").fill(TEST_USER);
  await page.locator("#signin-password").fill(TEST_PASS);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
  return { context, page, lines };
}

async function seekBar(page: Page, fraction: number) {
  const bar = page.locator("#pharos-seek");
  const box = await bar.boundingBox();
  expect(box, "the seek bar is drawn").not.toBeNull();
  await bar.click({
    position: { x: (box?.width ?? 0) * fraction, y: (box?.height ?? 2) / 2 },
  });
}

test.describe("mixed pharos-ui + jellyfin-web watch together", () => {
  for (const creator of ["jellyfin-web", "pharos-ui"] as const) {
    test(`${creator} creates the group; play, pause, seek stay in sync`, async ({
      browser,
      request,
    }) => {
      test.setTimeout(180_000);
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 405,
        `native API is not enabled (status ${probe.status()})`,
      );
      const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-mixed-${Date.now()}", Version="0"`,
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

      const dx = await dioxusSignIn(browser);
      const jf = await VirtualPerson.spawn(browser, 0);
      const dxMember = new DioxusMember(dx.page);
      const people: Probeable[] = [new JellyfinWebMember(jf), dxMember];
      try {
        const onWatchPage = async () => {
          await expect(dx.page).toHaveURL(new RegExp(`/ui/watch/${nativeId}$`));
          await expect(dx.page.locator(".pharos-members")).toBeVisible({
            timeout: 15_000,
          });
        };

        if (creator === "jellyfin-web") {
          const groupId = await jf.createGroup("mixed-jf");
          await jf.setNewQueue([nativeId]);
          await dx.page.goto(`${PHAROS_URL}/ui/together/${groupId.toLowerCase()}`);
          await dx.page.getByRole("button", { name: "Join" }).click();
          await onWatchPage();
        } else {
          // The first order's group may linger; tell ours apart by id.
          const listGroups = async (): Promise<{ GroupId: string }[]> =>
            (await request.get(`${PHAROS_URL}/SyncPlay/List`, { headers: api })).json();
          const before = new Set((await listGroups()).map((g) => g.GroupId));
          await dx.page.goto(`${PHAROS_URL}/ui/title/${nativeId}`);
          await dx.page.getByRole("button", { name: "Watch together" }).click();
          await onWatchPage();
          const fresh = (await listGroups()).filter((g) => !before.has(g.GroupId));
          expect(fresh, "the Dioxus group is listed").toHaveLength(1);
          await jf.joinGroup(fresh[0].GroupId);
        }
        await expect(dx.page.locator(".pharos-member-list li")).toHaveCount(2, {
          timeout: 15_000,
        });

        // Both start by themselves once ready.
        await waitUntilInSync(people);

        // jellyfin-web pauses; both pause.
        await jf.pause();
        await assertPausedAll(people);

        // pharos-ui plays; both resume in step.
        await dx.page.locator("#pharos-video").click();
        await waitUntilInSync(people);

        // pharos-ui pauses; both pause.
        await dx.page.locator("#pharos-video").click();
        await assertPausedAll(people);

        // jellyfin-web plays; both resume in step.
        await jf.unpause();
        await waitUntilInSync(people);

        // Seek from pharos-ui's bar (group paused first so the target holds still).
        await jf.pause();
        await assertPausedAll(people);
        await seekBar(dx.page, 0.25);
        await expect
          .poll(() => dxMember.probe().then((p) => p.currentTime), { timeout: 15_000 })
          .toBeGreaterThan(5);
        const dxTarget = (await dxMember.probe()).currentTime * 1000;
        await assertSeekConverged(people, dxTarget);

        // Seek from jellyfin-web; both converge, then play on together.
        await jf.seek(12_000);
        await assertSeekConverged(people, 12_000);
        await jf.unpause();
        await waitUntilInSync(people);
      } catch (e) {
        const dom = (p: Page) => p.locator("body").innerText().catch(() => "(no page)");
        throw new Error(
          `${(e as Error).message}\n--- pharos-ui page ---\n${await dom(dx.page)}\n--- logs ---\n${dx.lines.slice(-60).join("\n")}\n${jf.diagnostics()}`,
        );
      } finally {
        await dx.context.close();
        await jf.close();
      }
    });
  }
});
