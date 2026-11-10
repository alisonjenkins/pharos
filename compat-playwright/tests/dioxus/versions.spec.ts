import { test, expect, type APIRequestContext, type Page } from "@playwright/test";

// V722 (B770): a title with a 3D edition is badged and found by the 3D
// filter, and a title with more than one version offers a Version picker
// whose pick is what the watch page plays. V721 (B771): two copies of an
// episode are one episode with two versions. The seed (`just
// compat-dioxus-full`) adds "Stereo Film (2009)" with a `- 3D.HSBS` edition,
// a lone full side-by-side "Stereo Wide (2011)" (V920) and a second copy of
// Playwright Show S01E02 under `Season 1/`.

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
    // The half side-by-side edition plays as one eye (V920).
    await expect(page.locator(".pharos-video-frame")).toHaveAttribute("data-packing", "hsbs", {
      timeout: 20_000,
    });
  });

  test("the 3D filter keeps a library to its 3D titles and badges them", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await page.locator("section.pharos-rail h2 a", { hasText: /^Stereo$/ }).click();
    // The 3D choice is a pill in the library's sort and filter sheet (B1311).
    await page.getByRole("button", { name: /^Filters/ }).click({ timeout: 15_000 });
    const sheet = page.getByRole("dialog", { name: "Sort and filter" });
    await sheet
      .getByRole("group", { name: "3D", exact: true })
      .getByRole("button", { name: "3D only", exact: true })
      .click();
    await sheet.getByRole("button", { name: "Done", exact: true }).click();
    const tiles = page.locator("ul.pharos-grid .pharos-poster");
    await expect(tiles).toHaveCount(2, { timeout: 15_000 });
    await expect(tiles.filter({ hasText: "Stereo Film" })).toHaveCount(1);
    await expect(tiles.locator(".pharos-badge-3d")).toHaveText(["3D", "3D"]);
  });

  // V920 (B960): a packed version on a flat screen shows one eye at the eye's
  // shape, until "3D: both eyes" is picked.
  test("a full side-by-side film plays as one eye", async ({ page, request }) => {
    await signIn(page, request);
    await openFromSearch(page, "Stereo Wide", "Stereo Wide");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    const frame = page.locator(".pharos-video-frame");
    await expect(frame).toHaveAttribute("data-packing", "fsbs", { timeout: 20_000 });
    const video = page.locator("#pharos-video");
    await expect.poll(() => video.evaluate((v: HTMLVideoElement) => v.videoWidth), {
      timeout: 20_000,
    }).toBeGreaterThan(0);
    const box = () =>
      video.evaluate((v: HTMLVideoElement) => ({
        laidW: v.offsetWidth,
        laidH: v.offsetHeight,
        drawnW: v.getBoundingClientRect().width,
        frameW: v.videoWidth,
        frameH: v.videoHeight,
      }));
    // Laid out at one eye's shape, drawn twice as wide and clipped to half.
    await expect.poll(async () => {
      const b = await box();
      // A full packing's eye is half the frame's width.
      const eye = b.frameW / 2 / b.frameH;
      return Math.abs(b.laidW / b.laidH - eye) < 0.02 && Math.abs(b.drawnW - 2 * b.laidW) < 2;
    }, { timeout: 10_000 }).toBe(true);

    await page.locator(".pharos-player").hover();
    // V923: headless Chromium has no immersive VR, so no VR button.
    await expect(page.getByRole("combobox", { name: "3D view" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Watch in VR" })).toHaveCount(0);
    await page.getByRole("combobox", { name: "3D view" }).selectOption("both-eyes");
    await expect(frame).not.toHaveAttribute("data-packing", /.*/);
    // Kept on this device only (V922).
    expect(
      await page.evaluate(() => localStorage.getItem("pharos.player.stereo")),
      ).toBe("both-eyes");
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
