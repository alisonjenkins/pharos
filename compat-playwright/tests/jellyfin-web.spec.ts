import { test, expect, Page } from "@playwright/test";

// T29 phase 3 — drive unmodified jellyfin-web pointed at pharos.
//
// Pre-reqs:
//   * Nix devShell active → JELLYFIN_WEB_DIR set.
//   * pharos running on PHAROS_URL with the playwright seed user:
//       admin seed-playwright-user
//     (creates user "playwright" / pw "playwright-test-pw" + 4 items).
//
// Selectors are tuned for jellyfin-web 10.11.x; bumps to that version
// may break them. Prefer id selectors that map to upstream's IDs
// (txtServerHost / txtManualName / txtManualPassword / btnManual).

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const SEED_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const SEED_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

async function connectToServer(page: Page) {
  await page.goto("/", { waitUntil: "networkidle" });
  await page.getByRole("heading", { name: /select server/i }).waitFor({
    timeout: 15_000,
  });
  await page.getByText(/add server/i).click();
  const host = page.locator("#txtServerHost");
  await host.waitFor({ timeout: 10_000 });
  await host.fill(PHAROS_URL);
  await page.getByRole("button", { name: /^connect$/i }).click();
}

async function login(page: Page, username: string, password: string) {
  // After Connect, jellyfin-web routes to #/login?serverid=… when
  // /users/public is empty (no tile picker). Wait for the form, then
  // fill + submit. No "Manual Login" intermediate click needed — that
  // only shows when the tile picker is populated.
  await page.waitForURL(/#\/login/, { timeout: 20_000 });
  await page.locator("#txtManualName").waitFor({ timeout: 10_000 });
  await page.locator("#txtManualName").fill(username);
  await page.locator("#txtManualPassword").fill(password);
  await page.getByRole("button", { name: /^sign in$/i }).click();
}

test.describe("jellyfin-web compat", () => {
  test("connect → manual login → land on home", async ({ page }) => {
    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    await page.waitForURL(/#\/home/, { timeout: 25_000 });
    await expect(page).toHaveURL(/#\/home/);
  });

  test("library has at least one card on home", async ({ page }) => {
    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    await page.waitForURL(/#\/home/, { timeout: 25_000 });
    const tile = page.locator(".card, .listItem").first();
    await tile.waitFor({ timeout: 20_000 });
    await expect(tile).toBeVisible();
  });

  test("wrong password surfaces an error", async ({ page }) => {
    await connectToServer(page);
    await login(page, SEED_USER, "definitely-not-the-pw");
    const err = page
      .getByText(/(invalid|incorrect).*(user|password)|sign in.*failed/i)
      .first();
    await err.waitFor({ timeout: 15_000 });
    await expect(err).toBeVisible();
  });

  test("clicking jellyfin-web Play button plays media end-to-end", async ({ page }) => {
    // Drive jellyfin-web's actual playback path:
    //   login → details page for seeded fixture → click btnPlay →
    //   jellyfin-web's playbackManager → htmlVideoPlayer → <video>
    //   with currentTime > 0.
    //
    // Requires User.Configuration.CastReceiverId (chromecastPlayer
    // plugin reads it during init — without it a synchronous TypeError
    // kills the playback chain). Fixture is VP9/Opus inside WebM so
    // the FOSS chromium Playwright ships can decode without H.264
    // licenses.
    test.setTimeout(90_000);
    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    await page.waitForURL(/#\/home/, { timeout: 25_000 });
    const serverId = await page.evaluate(() => {
      try {
        return JSON.parse(window.localStorage.getItem("jellyfin_credentials")!)
          .Servers?.[0]?.Id ?? null;
      } catch (_e) {
        return null;
      }
    });
    if (!serverId) {
      throw new Error("could not read serverId from jellyfin-web localStorage");
    }
    // Navigate straight to the item details page; click Play.
    await page.goto(`/#/details?id=1&serverId=${serverId}`);
    const playBtn = page.locator("button.btnPlay").first();
    await playBtn.waitFor({ timeout: 20_000 });
    await playBtn.click();
    // jellyfin-web's htmlVideoPlayer mounts a <video> inside its
    // own osd container once playbackManager resolves the source.
    const video = page.locator("video").first();
    await video.waitFor({ state: "attached", timeout: 30_000 });
    await page.waitForFunction(
      () => {
        const v = document.querySelector("video") as HTMLVideoElement | null;
        return !!v && v.currentTime > 0;
      },
      undefined,
      { timeout: 30_000 },
    );
    // Sanity: page URL should have navigated to the player view.
    await expect(page).toHaveURL(/#\/video/);
  });
});
