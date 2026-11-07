import { test, expect, type APIRequestContext, type Page } from "@playwright/test";

// Spec 025: the UI as an Android TV or Google TV draws it. A TV user agent
// gets the ten-foot layout (title-safe margins, a glowing focus ring), and
// a remote's Back returns the focus to the tile it opened (B710, B712).

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

// The TCL Google TV's WebView, whose CSS viewport is 960 x 540.
test.use({
  viewport: { width: 960, height: 540 },
  userAgent:
    "Mozilla/5.0 (Linux; Android 12; Smart TV Pro Build/STT2.221228.001; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/156.0.8078.47 Mobile Safari/537.36 GoogleTV",
});

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

const focusedText = (page: Page) =>
  page.evaluate(() => document.activeElement?.textContent?.trim() ?? "");
const focusedIsPoster = (page: Page) =>
  page.evaluate(() => !!document.activeElement?.matches(".pharos-poster"));

test.describe("Android TV (spec 025)", () => {
  test("a TV keeps the page inside the title-safe area and rings the focus brightly", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await expect(page.locator("html")).toHaveAttribute("data-device", "tv");
    const brand = await page.locator(".pharos-brand").boundingBox();
    expect(brand!.x).toBeGreaterThanOrEqual(48);
    expect(brand!.y).toBeGreaterThanOrEqual(27);

    // Home's rails arrive after the shell; walk once they are drawn.
    await expect(page.locator("main .pharos-poster").first()).toBeVisible({ timeout: 15_000 });
    for (let i = 0; i < 6 && !(await focusedIsPoster(page)); i++) {
      await page.keyboard.press("ArrowDown");
    }
    expect(await focusedIsPoster(page)).toBe(true);
    const ring = await page.evaluate(() => {
      const art = document.activeElement!.querySelector(".pharos-poster-art")!;
      const s = getComputedStyle(art);
      return { width: s.outlineWidth, style: s.outlineStyle, glow: s.boxShadow };
    });
    expect(ring).toMatchObject({ width: "3px", style: "solid" });
    expect(ring.glow).not.toBe("none");
  });

  test("the first press after a page opens lands on its content, not the header (B712)", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await page.locator("section.pharos-rail h2 a", { hasText: /^TV$/ }).click();
    await expect(page.locator("ul.pharos-grid .pharos-poster").first()).toBeVisible({
      timeout: 15_000,
    });
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await page.keyboard.press("ArrowDown");
    const inMain = await page.evaluate(() => !!document.activeElement?.closest("main"));
    expect(inMain).toBe(true);
  });

  test("Back returns the remote to the tile it opened (B712)", async ({ page, request }) => {
    await signIn(page, request);
    await page.locator("section.pharos-rail h2 a", { hasText: /^TV$/ }).click();
    const grid = page.locator("ul.pharos-grid .pharos-poster");
    await expect(grid.first()).toBeVisible({ timeout: 15_000 });
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    for (let i = 0; i < 6 && !(await focusedIsPoster(page)); i++) {
      await page.keyboard.press("ArrowDown");
    }
    for (let i = 0; i < 6 && !(await focusedText(page)).includes("Playwright Show"); i++) {
      await page.keyboard.press("ArrowRight");
    }
    expect(await focusedText(page)).toContain("Playwright Show");
    await page.keyboard.press("Enter");
    await expect(page.locator("section.pharos-show h2")).toHaveText("Playwright Show", {
      timeout: 15_000,
    });

    await page.goBack();
    await expect(grid.first()).toBeVisible({ timeout: 15_000 });
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await page.keyboard.press("ArrowDown");
    expect(await focusedIsPoster(page)).toBe(true);
    expect(await focusedText(page)).toContain("Playwright Show");
  });
});
