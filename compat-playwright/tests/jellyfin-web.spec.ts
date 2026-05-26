import { test, expect, Page } from "@playwright/test";

// T29 phase 3 — drive unmodified jellyfin-web pointed at pharos.
//
// Expectations:
//   * jellyfin-web build is at compat-playwright/jellyfin-web/ (index.html
//     + assets). See ../README.md for how to produce it.
//   * pharos is running on PHAROS_URL (default 127.0.0.1:8096) with a
//     known user pre-seeded:
//       username = "playwright"
//       password = "playwright-test-pw"
//     and at least one MediaItem in the store.
//
// The test is intentionally lenient: jellyfin-web's UI selectors are not
// stable across releases. We use accessible-name + role queries first;
// `data-testid` fallbacks where Jellyfin doesn't ship one.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const SEED_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const SEED_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

async function configureServerUrl(page: Page) {
  // Jellyfin-web's wizard prompts for a server URL on first run when no
  // saved server is in localStorage. Selectors below match the
  // upstream "Connect Server" form (selectServer.html).
  await page.goto("/");
  // Older + newer builds use slightly different ids; try both.
  const hostInput = page
    .getByLabel(/host(name)? or url/i, { exact: false })
    .or(page.locator("input[name='host']"))
    .or(page.locator("input[type='url']"))
    .first();
  await hostInput.waitFor({ timeout: 15_000 });
  await hostInput.fill(PHAROS_URL);
  await page.getByRole("button", { name: /connect/i }).click();
}

async function login(page: Page) {
  // After connect, the manual-login link → username + password fields.
  // Some versions show the user-tile screen first; handle either.
  const manualLink = page.getByRole("button", { name: /manual login/i });
  if (await manualLink.isVisible().catch(() => false)) {
    await manualLink.click();
  }
  await page
    .getByLabel(/username/i)
    .first()
    .fill(SEED_USER);
  await page
    .getByLabel(/password/i)
    .first()
    .fill(SEED_PASS);
  await page.getByRole("button", { name: /sign in/i }).click();
}

test.describe("jellyfin-web compat", () => {
  test("connect → login → land on home", async ({ page }) => {
    await configureServerUrl(page);
    await login(page);
    // After login, jellyfin-web navigates to #!/home.html. Wait for the
    // header that's only visible to authed users.
    await expect(page).toHaveURL(/home\.html/, { timeout: 20_000 });
    await expect(
      page.getByRole("link", { name: /home/i }).first(),
    ).toBeVisible();
  });

  test("library appears with at least one item tile", async ({ page }) => {
    await configureServerUrl(page);
    await login(page);
    // Wait for the library row. Jellyfin renders "My Media" or library
    // cards on the home page.
    await expect(page).toHaveURL(/home\.html/, { timeout: 20_000 });
    const tiles = page.locator("button.card, a.card");
    await expect(tiles.first()).toBeVisible({ timeout: 15_000 });
    const count = await tiles.count();
    expect(count).toBeGreaterThan(0);
  });

  test("wrong password surfaces an error", async ({ page }) => {
    await configureServerUrl(page);
    const manualLink = page.getByRole("button", { name: /manual login/i });
    if (await manualLink.isVisible().catch(() => false)) {
      await manualLink.click();
    }
    await page.getByLabel(/username/i).first().fill(SEED_USER);
    await page.getByLabel(/password/i).first().fill("definitely-not-the-pw");
    await page.getByRole("button", { name: /sign in/i }).click();
    // jellyfin-web shows a toast "Invalid username or password" or a
    // modal — assert either appears within 10 s.
    const error = page
      .getByText(/invalid (user|password)|incorrect/i)
      .first();
    await expect(error).toBeVisible({ timeout: 10_000 });
  });
});
