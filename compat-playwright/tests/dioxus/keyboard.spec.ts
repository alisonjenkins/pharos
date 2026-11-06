import { test, expect, type APIRequestContext, type Page } from "@playwright/test";

// Spec 029: keyboard modes. A title or show page reached from the keyboard
// has its Play focused, Enter plays, the Vim and arrow keys move between
// the page's controls and episode rows, the page keys press its actions,
// and `g` / `?` open the which-key help.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

async function signIn(page: Page, request: APIRequestContext): Promise<void> {
  const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
    headers: { Accept: "application/json" },
  });
  test.skip(
    probe.status() !== 405,
    `native API is not enabled (status ${probe.status()}); skipping native test`,
  );
  await page.goto("/ui/");
  await page.locator("#signin-name").fill(TEST_USER);
  await page.locator("#signin-password").fill(TEST_PASS);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
}

async function openShow(page: Page): Promise<void> {
  await page.locator("section.pharos-rail h2 a", { hasText: /^TV$/ }).click();
  await page
    .locator("ul.pharos-grid .pharos-poster")
    .filter({ hasText: "Playwright Show" })
    .click();
  await expect(page.locator("section.pharos-show h2")).toHaveText("Playwright Show", {
    timeout: 15_000,
  });
  await expect(page.locator("ol.pharos-episodes li")).toHaveCount(3);
}

const focused = (page: Page) =>
  page.evaluate(() => {
    const el = document.activeElement;
    return {
      id: el?.id ?? "",
      text: el?.textContent?.trim() ?? "",
      row: el?.closest("[data-key-row]") ? [...el.closest("ol")!.children].indexOf(el.closest("li")!) : -1,
      action: el?.getAttribute("data-key-action") ?? "",
    };
  });

test.describe("keyboard modes (spec 029)", () => {
  test("a film found from the search box opens with Play focused, and Enter plays", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    const search = page.locator(".pharos-search input[type=search]");
    await search.fill("AA Bulk 026");
    await expect(page.locator(".pharos-search [role=listbox] [role=option]").first()).toBeVisible({
      timeout: 15_000,
    });
    await search.press("ArrowDown");
    await search.press("Enter");
    await expect(page.locator("article.pharos-title h2")).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("#pharos-main-action")).toBeFocused();
    await expect(page.locator("#pharos-main-action")).toHaveText(/^(Play|Resume)$/);
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/ui\/watch\//);
  });

  test("a show opens on its next episode's Play, and the keys walk its episodes", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await openShow(page);
    const play = page.locator("#pharos-main-action");
    await expect(play).toBeFocused();
    await expect(play).toHaveText(/^(Play|Resume) S\d+ · E\d+$/);

    // j moves down until the focus is in the episode rows, then keeps the
    // column from row to row.
    for (let i = 0; i < 12 && (await focused(page)).row < 0; i++) {
      await page.keyboard.press("j");
    }
    const first = await focused(page);
    expect(first.row).toBeGreaterThanOrEqual(0);
    await page.keyboard.press("j");
    const second = await focused(page);
    expect(second.row).toBe(first.row + 1);
    expect(second.action).toBe(first.action);
    await page.keyboard.press("ArrowUp");
    expect((await focused(page)).row).toBe(first.row);

    // Along the row to its Play, which Enter starts.
    for (let i = 0; i < 4 && (await focused(page)).action !== "play"; i++) {
      await page.keyboard.press("l");
    }
    expect((await focused(page)).action).toBe("play");
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/ui\/watch\//);
  });

  test("w marks the focused episode, and again unmarks it", async ({ page, request }) => {
    await signIn(page, request);
    await openShow(page);
    const mark = page.locator("ol.pharos-episodes li").last().locator(".pharos-episode-mark");
    await mark.focus();
    const before = await mark.getAttribute("aria-label");
    await page.keyboard.press("w");
    await expect(mark).not.toHaveAttribute("aria-label", before ?? "");
    await page.keyboard.press("w");
    await expect(mark).toHaveAttribute("aria-label", before ?? "");
    // The mark keeps the focus through the reload it causes.
    await expect(mark).toBeFocused();
  });

  test("g waits and lists where it goes; g h goes home", async ({ page, request }) => {
    await signIn(page, request);
    await openShow(page);
    await page.keyboard.press("g");
    const help = page.locator(".pharos-key-help");
    await expect(help).toBeVisible();
    await expect(help).toContainText("Home");
    await expect(help).toContainText("Settings");
    await page.keyboard.press("h");
    await expect(help).toHaveCount(0);
    await expect(page).toHaveURL(/\/ui\/?$/);
  });

  test("? lists the page's keys and Escape closes the list", async ({ page, request }) => {
    await signIn(page, request);
    await openShow(page);
    await page.keyboard.press("?");
    const help = page.locator(".pharos-key-help");
    await expect(help).toBeVisible();
    await expect(help.locator("h2")).toContainText("Title page");
    await expect(help).toContainText("Mark watched or unwatched");
    await page.keyboard.press("Escape");
    await expect(help).toHaveCount(0);
    await expect(page.locator("section.pharos-show h2")).toHaveText("Playwright Show");
  });

  test("? lists only the keys the page can use", async ({ page, request }) => {
    await signIn(page, request);
    const help = page.locator(".pharos-key-help");
    // Nothing is playing, so there is no player to open or resize.
    await page.keyboard.press("?");
    await expect(help).toBeVisible();
    await expect(help).not.toContainText("Open the full player");
    await expect(help).not.toContainText("Mini bar or floating player");
    await expect(help).not.toContainText("Now playing");
    await page.keyboard.press("Escape");

    // A show lists the season keys exactly when it has season tabs (the
    // seeded show has one season, so none).
    await openShow(page);
    await page.keyboard.press("?");
    await expect(help).toContainText("Mark watched or unwatched");
    if ((await page.locator("[data-key-tabs]").count()) > 0) {
      await expect(help).toContainText("Next season");
    } else {
      await expect(help).not.toContainText("Next season");
    }
    await page.keyboard.press("Escape");

    // A film has none.
    const search = page.locator(".pharos-search input[type=search]");
    await search.fill("AA Bulk 026");
    await expect(page.locator(".pharos-search [role=listbox] [role=option]").first()).toBeVisible({
      timeout: 15_000,
    });
    await search.press("ArrowDown");
    await search.press("Enter");
    await expect(page.locator("#pharos-main-action")).toBeFocused({ timeout: 15_000 });
    await page.keyboard.press("?");
    await expect(help).toContainText("Play or resume");
    await expect(help).not.toContainText("Next season");
    await expect(help).not.toContainText("Previous season");
  });

  test("typing in the search box is never taken by the keys", async ({ page, request }) => {
    await signIn(page, request);
    await openShow(page);
    const search = page.locator(".pharos-search input[type=search]");
    await search.focus();
    await page.keyboard.type("jkgh?");
    await expect(search).toHaveValue("jkgh?");
    await expect(page.locator(".pharos-key-help")).toHaveCount(0);
    await expect(page.locator("section.pharos-show h2")).toHaveText("Playwright Show");
  });
});

test.describe("player keys (spec 029)", () => {
  async function playFilm(page: Page): Promise<void> {
    const search = page.locator(".pharos-search input[type=search]");
    await search.fill("AA Bulk 026");
    await expect(page.locator(".pharos-search [role=listbox] [role=option]").first()).toBeVisible({
      timeout: 15_000,
    });
    await search.press("ArrowDown");
    await search.press("Enter");
    await expect(page.locator("#pharos-main-action")).toBeFocused({ timeout: 15_000 });
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/ui\/watch\//);
    await expect(page.locator(".pharos-controls")).toBeAttached({ timeout: 30_000 });
  }
  const muted = (page: Page) =>
    page.evaluate(() => (document.getElementById("pharos-video") as HTMLVideoElement).muted);

  test("the player's keys work with the focus anywhere on the page (B618)", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await playFilm(page);
    // Play was pressed and is gone, so the focus is on the page, not the player.
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    expect(await muted(page)).toBe(false);
    await page.keyboard.press("m");
    await expect.poll(() => muted(page)).toBe(true);
    await expect(page.locator("#pharos-player")).toBeFocused();
  });

  test("g h in the player goes home rather than seeking back (B617)", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await playFilm(page);
    await page.locator("#pharos-player").focus();
    await page.keyboard.press("g");
    await page.keyboard.press("h");
    await expect(page).toHaveURL(/\/ui\/?$/);
  });
});
