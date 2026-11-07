import {
  test,
  expect,
  APIRequestContext,
  Browser,
  BrowserContext,
  Page,
} from "@playwright/test";
import { Probeable, VideoProbeLike } from "./sync-oracle";

// Shared by the pharos Dioxus UI watch-together specs (014): signing in a
// browser, starting a group from a title and bringing a second browser in by
// its invite link, and reading a member's `<video>` for `sync-oracle.ts`.

export const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
export const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
export const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

/** A user to sign a browser in as. */
export interface Account {
  name: string;
  password: string;
}

/** The seeded user, opted out of diagnostics (the default). */
export const SEEDED: Account = { name: TEST_USER, password: TEST_PASS };

/** A member's `<video>` as the sync oracle reads it. */
export class Member implements Probeable {
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
export async function listedGroups(
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
export async function toggleUntil(member: Member, paused: boolean) {
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

export async function signIn(browser: Browser, label: string) {
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
export async function seededMovie(request: APIRequestContext, file: string) {
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
export async function startGroup(
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
export async function failure(e: unknown, a: { page: Page }, b: { page: Page }, logs: () => string) {
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
export async function pickUntil(
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
