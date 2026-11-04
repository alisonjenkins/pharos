import { test, expect, Browser } from "@playwright/test";

// 014 T035 - who in a watch-together group has seen what, in two browsers
// signed in as two users.
//
// V (a second user) starts a group on the seeded movie and A (the seeded
// user) joins. A marks one episode of the seeded show played; V then opens the
// show page inside the app (the group lives in the page, so no reload) and
// reads it under "In this group". The member strip on the watch page answers
// the same question for the item playing.
//
// Needs the same server as the rest of this suite (see
// `just compat-dioxus-full`): `[server.native_api]` enabled, the seeded user,
// `fixture-1.webm` and the seeded three-episode "Playwright Show". Named to
// run after `together.spec.ts`, which takes the only group listed: closing a
// tab leaves a group's seats to the socket-loss grace, so this one's group
// outlives it.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";
const VIEWER = "pw-seen-viewer";
const VIEWER_PASS = "pw-seen-viewer-pw";

test.use({
  launchOptions: { args: ["--autoplay-policy=no-user-gesture-required"] },
});

async function signIn(browser: Browser, name: string, password: string) {
  const context = await browser.newContext();
  const page = await context.newPage();
  const lines: string[] = [];
  page.on("console", (m) => lines.push(`[${name}] ${m.text()}`));
  page.on("pageerror", (e) => lines.push(`[${name}] pageerror: ${e.message}`));
  await page.goto(`${PHAROS_URL}/ui/`);
  await page.locator("#signin-name").fill(name);
  await page.locator("#signin-password").fill(password);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
  return { context, page, lines };
}

test.describe("pharos Dioxus UI watch together: who has seen what", () => {
  test("a member sees what the others in the group have played", async ({
    browser,
    request,
  }) => {
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
        "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-seen-${Date.now()}", Version="0"`,
      },
      data: { Username: TEST_USER, Pw: TEST_PASS },
    });
    test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
    const { AccessToken: token, User: user } = await auth.json();
    const api = { "X-Emby-Token": token };
    const json = async (path: string) =>
      (await request.get(`${PHAROS_URL}${path}`, { headers: api })).json();

    const movies = await json(
      `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path`,
    );
    const movie = movies.Items?.find((i: { Path?: string }) =>
      i.Path?.endsWith("/fixture-1.webm"),
    );
    test.skip(!movie, "pharos has no seeded movie");
    const nativeId = BigInt(`0x${movie.Id}`).toString();
    const episodes = await json(
      `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Episode&Fields=SeriesName`,
    );
    const shown = episodes.Items?.filter(
      (i: { SeriesName?: string }) => i.SeriesName === "Playwright Show",
    );
    expect(shown, "the seed has the three-episode show").toHaveLength(3);

    // The second user, once.
    const made = await request.post(`${PHAROS_URL}/Users/New`, {
      headers: { ...api, "Content-Type": "application/json" },
      data: { Name: VIEWER, Password: VIEWER_PASS },
    });
    expect([200, 400, 409], `creating ${VIEWER}: ${made.status()}`).toContain(made.status());

    const v = await signIn(browser, VIEWER, VIEWER_PASS);
    const a = await signIn(browser, TEST_USER, TEST_PASS);
    const logs = () => [...v.lines, ...a.lines].join("\n");
    try {
      // V starts the group; A joins by the invite link.
      await v.page.goto(`${PHAROS_URL}/ui/title/${nativeId}`);
      await v.page.getByRole("button", { name: "Watch together" }).click();
      await expect(v.page).toHaveURL(new RegExp(`/ui/watch/${nativeId}$`));
      await expect(v.page.locator(".pharos-members")).toBeVisible({ timeout: 15_000 });
      const groups = await json("/SyncPlay/List");
      // Another spec may have left a group behind; V's is the one V is in.
      const mine = groups.find((g: { Participants: string[] }) =>
        g.Participants.includes(VIEWER),
      );
      expect(mine, "V's group is listed").toBeTruthy();
      await a.page.goto(
        `${PHAROS_URL}/ui/together/${String(mine.GroupId).toLowerCase()}`,
      );
      await a.page.getByRole("button", { name: "Join" }).click();
      await expect(a.page).toHaveURL(new RegExp(`/ui/watch/${nativeId}$`));
      await expect(v.page.locator(".pharos-member-list li")).toHaveCount(2, {
        timeout: 15_000,
      });

      // A has played the first episode.
      const first = shown.find((i: { IndexNumber?: number }) => i.IndexNumber === 1) ?? shown[0];
      const played = await request.post(
        `${PHAROS_URL}/Users/${user.Id}/PlayedItems/${first.Id}`,
        { headers: api },
      );
      expect(played.ok(), `marking played: ${played.status()}`).toBeTruthy();

      // V reads the show page in the app, still in the group.
      await v.page.getByRole("button", { name: "Back" }).click();
      await v.page.locator(".pharos-shell-bar a").first().click();
      await v.page.locator("section.pharos-rail h2 a", { hasText: /^TV$/ }).click();
      await v.page
        .locator("ul.pharos-grid .pharos-poster-name", { hasText: "Playwright Show" })
        .click();
      await expect(v.page.locator("ol.pharos-episodes li")).toHaveCount(3, {
        timeout: 15_000,
      });
      const group = v.page.locator("section.pharos-group-seen");
      await expect(group).toContainText("In this group", { timeout: 15_000 });
      await expect(group).toContainText(TEST_USER);
      await expect(group).toContainText("1/3 episodes");
      // On the row: A's dot is filled for the played episode, V's is not.
      await expect(
        v.page.locator("ol.pharos-episodes .pharos-seen-dot.is-seen"),
      ).toHaveCount(1);
      await expect(
        v.page.locator("ol.pharos-episodes .pharos-seen-dot.is-unseen"),
      ).toHaveCount(5);
    } catch (e) {
      const dom = async (p: typeof v.page) =>
        p.locator("body").innerText().catch(() => "(no page)");
      throw new Error(
        `${(e as Error).message}\n--- V page ---\n${await dom(v.page)}\n--- A page ---\n${await dom(a.page)}\n--- browser logs ---\n${logs()}`,
      );
    } finally {
      await v.context.close();
      await a.context.close();
    }
  });
});
