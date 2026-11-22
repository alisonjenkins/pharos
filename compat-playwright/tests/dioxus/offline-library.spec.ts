import { test, expect, type Browser, type Page } from "@playwright/test";

// Spec 049 (V1483, V1484): with no server the app keeps its usual screens
// and lists only what is on the device, under an offline line; when the
// server is back the library fills in again. Uses a kept book (spec 036),
// which needs no transcode, as the thing on the device.
//
// Run against a real bundle and a pharos seeded with
// `pharos admin seed-playwright-user` and the reader books
// (tools/make-reader-books.cjs), as books-offline.spec.ts.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_OFFLINE_USER ?? "together-b";
const TEST_PASS = process.env.PHAROS_OFFLINE_PASS ?? "together-b-test-pw";

test.use({ baseURL: PHAROS_URL, serviceWorkers: "allow" });

async function signedIn(browser: Browser): Promise<{ page: Page; logs: string[] }> {
  const context = await browser.newContext({
    baseURL: PHAROS_URL,
    serviceWorkers: "allow",
    storageState: {
      cookies: [],
      origins: [
        {
          origin: new URL(PHAROS_URL).origin,
          localStorage: [{ name: "pharos.whats_new", value: "off" }],
        },
      ],
    },
  });
  const page = await context.newPage();
  const logs: string[] = [];
  page.on("console", (m) => {
    if (m.text().startsWith("pharos: offline")) logs.push(m.text());
  });
  await page.goto("/ui/");
  await page.evaluate(() => navigator.serviceWorker.ready);
  await page.waitForFunction(() => navigator.serviceWorker.controller !== null);
  await page.locator("#signin-name").fill(TEST_USER);
  await page.locator("#signin-password").fill(TEST_PASS);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
  return { page, logs };
}

async function keepBook(page: Page, name: string): Promise<void> {
  const search = page.locator(".pharos-search input[type=search]");
  await search.fill(name);
  const hit = page
    .locator(".pharos-search [role=listbox] [role=option]")
    .filter({ hasText: name })
    .first()
    .getByRole("link");
  await expect(hit).toBeVisible({ timeout: 15_000 });
  await hit.click();
  await expect(page.locator(".pharos-main")).toContainText(name, { timeout: 15_000 });
  await page.locator("#pharos-keep-offline").click();
  await expect(page.locator("#pharos-keep-offline")).toHaveText("Remove from device", {
    timeout: 20_000,
  });
}

const main = (page: Page) => page.locator(".pharos-main");

test.describe("the library offline (spec 049)", () => {
  test.beforeEach(async ({ request }) => {
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
  });

  test("offline, Home lists only what is on the device and fills in again when back", async ({
    browser,
  }) => {
    const { page, logs } = await signedIn(browser);
    await keepBook(page, "Reader Fixture");

    // Online, Home lists a book that is not kept.
    await page.goto("/ui/");
    await expect(main(page)).toContainText("Reader Second", { timeout: 20_000 });

    // A cold start with no network: the usual shell and Home, the offline
    // line, the kept book, and nothing that is not on the device.
    await page.context().setOffline(true);
    await page.reload();
    await expect(page.locator(".pharos-offline-note")).toContainText(
      "You're offline. Showing what's on this device.",
      { timeout: 30_000 },
    );
    await expect(page.locator(".pharos-shell-bar")).toBeVisible();
    await expect(main(page)).toContainText("Reader Fixture", { timeout: 15_000 });
    await expect(main(page)).not.toContainText("Reader Second");
    expect(logs.some((l) => /^pharos: offline library verdict=device reason=no_network items=\d+/.test(l))).toBe(
      true,
    );
    await page.screenshot({ path: "test-results/offline-home.png" });

    // Its title page offers Read and nothing that needs the server.
    await main(page).getByRole("link", { name: /Reader Fixture/ }).first().click();
    await expect(page.locator("#pharos-main-action")).toBeVisible({ timeout: 15_000 });
    await expect(main(page)).not.toContainText("Download file");
    await page.screenshot({ path: "test-results/offline-title.png" });

    // Back online: "Back online", and Home lists the whole library again.
    await page.context().setOffline(false);
    await page.goto("/ui/");
    await expect(main(page)).toContainText("Reader Second", { timeout: 30_000 });
    await expect(page.locator(".pharos-offline-note:not(.pharos-offline-note--back)")).toHaveCount(0);
    await page.context().close();
  });

  test("a session that loses its connection switches to the device's library, then back", async ({
    browser,
  }) => {
    const { page, logs } = await signedIn(browser);
    await keepBook(page, "Reader Fixture");
    // The socket passes through a route the test can cut, as a server
    // that goes away (the browser stays online, so only the lost session
    // tells the app).
    let cut = false;
    const open: { close: () => Promise<void> }[] = [];
    await page.routeWebSocket(/\/pharos\/rpc/, (ws) => {
      if (cut) {
        void ws.close();
        return;
      }
      ws.connectToServer();
      open.push(ws);
    });
    // The app asks over HTTP why a socket would not open; a server that is
    // gone does not answer that either.
    await page.route(/\/pharos\/session/, (route) => (cut ? route.abort() : route.continue()));
    await page.goto("/ui/");
    await expect(main(page)).toContainText("Reader Second", { timeout: 20_000 });

    // Cut: the page waits out the 5 s grace before it switches, timed
    // from the moment the app shows it saw the loss.
    cut = true;
    for (const ws of open.splice(0)) await ws.close();
    await expect(page.locator(".pharos-connection-shell")).toBeVisible({ timeout: 20_000 });
    const lostAt = Date.now();
    const note = page.locator(".pharos-offline-note");
    // Well inside the grace: still the server's screens.
    await page.waitForTimeout(4_000);
    await expect(note).toHaveCount(0);
    await expect(note).toBeVisible({ timeout: 20_000 });
    // The loss is seen up to one poll late, so allow that much under 5 s.
    expect(Date.now() - lostAt).toBeGreaterThanOrEqual(4_800);
    await expect(main(page)).toContainText("Reader Fixture");
    await expect(main(page)).not.toContainText("Reader Second", { timeout: 10_000 });
    expect(logs.some((l) => /^pharos: offline library verdict=device reason=connection_lost /.test(l))).toBe(true);

    cut = false;
    await expect(page.locator(".pharos-offline-note--back")).toContainText("Back online.", {
      timeout: 60_000,
    });
    await page.screenshot({ path: "test-results/back-online.png" });
    await expect(main(page)).toContainText("Reader Second", { timeout: 30_000 });
    expect(logs.some((l) => /^pharos: offline library verdict=server reason=connected was_device=true/.test(l))).toBe(
      true,
    );
    await page.context().close();
  });
});
