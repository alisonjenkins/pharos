import { test, expect, type Browser, type Page } from "@playwright/test";

// Spec 036 slice 3 (FR-004 to FR-006, US2, US3): a book kept on the device
// opens with no connection, is listed under Settings, Offline, and goes when
// removed or on sign-out. The service worker (ADR-0110) lets the shell start
// offline; the book itself is read from Cache Storage by the page. The seed
// (`just compat-dioxus-full`) writes "Reader Fixture.epub", three chapters of
// unique word tokens ("zq1x4x17").
//
// Run alone against a real bundle:
//   dx build --platform web --package pharos-ui --bin pharos-ui-web \
//     --features web --release --profile wasm-release --debug-symbols false
// copy target/dx/pharos-ui-web/release/web/public, assets/pharos.css (into
// assets/) and assets/sw.js (at the root) into a ui_dir, point a pharos with
// [server.native_api] at it, run `pharos admin seed-playwright-user`, build the
// books with tools/make-reader-books.cjs, then
// PHAROS_URL=... npx playwright test --config playwright.dioxus.config.ts books-offline.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

test.use({ baseURL: PHAROS_URL, serviceWorkers: "allow" });

/** A fresh signed-in page whose service worker controls it, so a reload can start offline. */
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
    if (m.text().startsWith("pharos: kept") || m.text().startsWith("pharos: reader")) {
      logs.push(m.text());
    }
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

async function openBookPage(page: Page): Promise<void> {
  const search = page.locator(".pharos-search input[type=search]");
  await search.fill("Reader Fixture");
  const hit = page
    .locator(".pharos-search [role=listbox] [role=option]")
    .filter({ hasText: "Reader Fixture" })
    .first()
    .getByRole("link");
  await expect(hit).toBeVisible({ timeout: 15_000 });
  await hit.click();
  await expect(page.locator(".pharos-main")).toContainText("Reader Fixture", { timeout: 15_000 });
}

const bookCaches = (page: Page) =>
  page.evaluate(async () => {
    const out: Record<string, string[]> = {};
    for (const name of await caches.keys()) {
      if (!name.startsWith("pharos-books-")) continue;
      const cache = await caches.open(name);
      out[name] = (await cache.keys()).map((r) => new URL(r.url).pathname);
    }
    return out;
  });

const record = (page: Page) =>
  page.evaluate(() => {
    const text = localStorage.getItem("pharos.kept");
    return text === null ? null : JSON.parse(text);
  });

async function keepFixture(page: Page): Promise<void> {
  await openBookPage(page);
  await page.locator("#pharos-keep-offline").click();
  await expect(page.locator("#pharos-keep-offline")).toHaveText("Remove from device", {
    timeout: 20_000,
  });
}

test.describe("books kept on the device (spec 036)", () => {
  test.beforeEach(async ({ request }) => {
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
  });

  test("a kept book opens offline from a reload, and Remove deletes it", async ({ browser }) => {
    const { page, logs } = await signedIn(browser);
    await keepFixture(page);

    const caches1 = await bookCaches(page);
    const names = Object.keys(caches1);
    expect(names).toHaveLength(1);
    expect(caches1[names[0]]).toHaveLength(1);
    expect(caches1[names[0]][0]).toMatch(/^\/Items\/\d+\/Download$/);
    const kept = await record(page);
    expect(kept.books).toHaveLength(1);
    expect(kept.books[0]).toMatchObject({ title: "Reader Fixture", format: "epub", reason: "hand" });
    expect(kept.books[0].bytes).toBeGreaterThan(1000);
    expect(names[0]).toBe(`pharos-books-${kept.user}`);
    expect(logs.some((l) => /^pharos: kept item=\d+ bytes=\d+ format=EPUB reason=hand/.test(l))).toBe(
      true,
    );
    expect(logs.some((l) => l.startsWith("pharos: kept persist granted="))).toBe(true);

    // Settings, Offline lists it with its size and why.
    await page.goto("/ui/settings/offline");
    const row = page.locator(".pharos-kept-row");
    await expect(row).toHaveCount(1, { timeout: 15_000 });
    await expect(row).toContainText("Reader Fixture");
    await expect(row).toContainText("kept by hand");
    await expect(page.locator("#pharos-kept-total")).toHaveText(/^1 book · /);

    // No connection: the shell starts from the worker, the list from the record.
    await page.context().setOffline(true);
    await page.reload();
    await expect(page.getByRole("heading", { name: "Offline: showing books on this device" })).toBeVisible({
      timeout: 30_000,
    });
    await page.getByRole("link", { name: "Reader Fixture" }).click();
    await expect(page.locator(".pharos-reader")).toBeVisible({ timeout: 20_000 });
    await expect
      .poll(
        () =>
          page.evaluate(() => {
            const root = document.getElementById("pharos-reader-host")?.shadowRoot;
            return root?.querySelector(".quire-strip")?.textContent?.includes("zq0x0x0") ?? false;
          }),
        { timeout: 20_000 },
      )
      .toBe(true);
    expect(logs.some((l) => /^pharos: reader opened item=\d+ format=epub from=cache/.test(l))).toBe(true);

    // Back online: Remove deletes the bytes and the record.
    await page.context().setOffline(false);
    await page.goto("/ui/settings/offline");
    await expect(page.locator(".pharos-kept-row")).toHaveCount(1, { timeout: 30_000 });
    await page.getByRole("button", { name: "Remove Reader Fixture from this device" }).click();
    await expect(page.locator(".pharos-kept-row")).toHaveCount(0);
    await expect(page.locator("#pharos-kept-empty")).toBeVisible();
    expect(await bookCaches(page)).toEqual({});
    expect(await record(page)).toBeNull();
    expect(logs.some((l) => /^pharos: kept removed item=\d+ bytes=\d+/.test(l))).toBe(true);
    await page.context().close();
  });

  test("a kept book opens from the cache even online", async ({ browser }) => {
    const { page, logs } = await signedIn(browser);
    await keepFixture(page);
    await page.locator("#pharos-main-action").click();
    await expect(page.locator(".pharos-reader")).toBeVisible({ timeout: 20_000 });
    await expect
      .poll(() => logs.some((l) => /^pharos: reader opened item=\d+ format=epub from=cache/.test(l)))
      .toBe(true);
    await page.context().close();
  });

  test("signing out deletes every kept book", async ({ browser }) => {
    const { page, logs } = await signedIn(browser);
    await keepFixture(page);
    expect(Object.keys(await bookCaches(page))).toHaveLength(1);

    await page.getByRole("button", { name: "Account", exact: true }).click();
    await page.getByRole("menuitem", { name: "Sign out" }).click();
    await expect(page.locator("form#signin")).toBeVisible();
    await expect.poll(async () => Object.keys(await bookCaches(page)).length).toBe(0);
    expect(await record(page)).toBeNull();
    expect(logs.some((l) => /^pharos: kept cleared why=sign-out books=1 /.test(l))).toBe(true);
    await page.context().close();
  });
});
