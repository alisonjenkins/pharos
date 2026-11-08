import { test, expect, type APIRequestContext, type Page } from "@playwright/test";

// V722 (B770): a title with a 3D edition is badged and found by the 3D
// filter, and a title with more than one version offers a Version picker
// whose pick is what the watch page plays. V721 (B771): two copies of an
// episode are one episode with two versions. The seed (`just
// compat-dioxus-full`) adds "Stereo Film (2009)" with a `- 3D.HSBS` edition
// and a second copy of Playwright Show S01E02 under `Season 1/`.

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

async function openFromSearch(page: Page, term: string, heading: string): Promise<void> {
  const search = page.locator(".pharos-search input[type=search]");
  await search.fill(term);
  const hit = page
    .locator(".pharos-search [role=listbox] [role=option]")
    .filter({ hasText: term })
    .first()
    .getByRole("link");
  await expect(hit).toBeVisible({ timeout: 15_000 });
  await hit.click();
  await expect(page.locator(".pharos-main")).toContainText(heading, { timeout: 15_000 });
}

test.describe("versions and 3D (spec 022)", () => {
  test("a title with a 3D edition offers it and plays the version picked", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await openFromSearch(page, "Stereo Film", "Stereo Film");

    const picker = page.getByRole("combobox", { name: "Version" });
    await expect(picker).toBeVisible({ timeout: 15_000 });
    const options = picker.locator("option");
    await expect(options).toHaveCount(2);
    await expect(options.nth(1)).toContainText("3D (half side-by-side)");
    await expect(page.locator("dl.pharos-version-facts")).toBeVisible();

    const value = await options.nth(1).getAttribute("value");
    expect(value).toBeTruthy();
    await picker.selectOption(value!);
    await expect(picker).toHaveValue(value!);
    // Kept per title on this device, so it survives a reload.
    await page.reload();
    await expect(page.getByRole("combobox", { name: "Version" })).toHaveValue(value!, {
      timeout: 15_000,
    });

    const streams: string[] = [];
    page.on("request", (r) => {
      if (/\/(videos|Videos)\/[^/]+\/(stream|master|main)/.test(r.url())) streams.push(r.url());
    });
    const chosen = page.waitForEvent("console", {
      predicate: (m) => /pharos: version chosen=/.test(m.text()),
      timeout: 20_000,
    });
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    expect((await chosen).text()).toContain(value!);
    await expect
      .poll(() => streams.find((u) => /MediaSourceId=[^&]*-3d-hsbs/i.test(u)) ?? "", {
        timeout: 20_000,
      })
      .not.toBe("");
  });

  test("the 3D filter keeps a library to its 3D titles and badges them", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await page.locator("section.pharos-rail h2 a", { hasText: /^Stereo$/ }).click();
    const filter = page.getByRole("combobox", { name: "3D" });
    await expect(filter).toBeVisible({ timeout: 15_000 });
    await filter.selectOption({ label: "3D only" });
    const tiles = page.locator("ul.pharos-grid .pharos-poster");
    await expect(tiles).toHaveCount(1, { timeout: 15_000 });
    await expect(tiles.first()).toContainText("Stereo Film");
    await expect(tiles.first().locator(".pharos-badge-3d")).toHaveText("3D");
  });

  test("two copies of an episode are one episode with two versions", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await page.locator("section.pharos-rail h2 a", { hasText: /^TV$/ }).click();
    await page
      .locator("ul.pharos-grid .pharos-poster")
      .filter({ hasText: "Playwright Show" })
      .click();
    await expect(page.locator("ol.pharos-episodes li")).toHaveCount(3, { timeout: 15_000 });
    await page.locator("ol.pharos-episodes li").nth(1).getByRole("link").first().click();
    const picker = page.getByRole("combobox", { name: "Version" });
    await expect(picker).toBeVisible({ timeout: 15_000 });
    await expect(picker.locator("option")).toHaveCount(2);
    await expect(picker.locator("option").nth(1)).toContainText("Season 1");
  });
});
