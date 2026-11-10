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
// A seeded user the other specs do not read with: these cases read the book
// and send the place to the server, and reader.spec.ts expects the
// "playwright" user's copy never to have been opened.
const TEST_USER = process.env.PHAROS_KEPT_USER ?? "together-a";
const TEST_PASS = process.env.PHAROS_KEPT_PASS ?? "together-a-test-pw";

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

async function openBookPage(page: Page, name = "Reader Fixture"): Promise<void> {
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

/** The first fixture word lying inside the page frame; "" until the page is laid out. */
function firstWord(page: Page): Promise<string> {
  return page.evaluate(() => {
    const root = document.getElementById("pharos-reader-host")?.shadowRoot;
    const frame = root?.querySelector(".quire-frame");
    const strip = root?.querySelector(".quire-strip");
    if (!frame || !strip) return "";
    const box = frame.getBoundingClientRect();
    const walker = document.createTreeWalker(strip, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      const text = n.nodeValue ?? "";
      for (const m of text.matchAll(/zq\d+x\d+x\d+/g)) {
        const range = document.createRange();
        range.setStart(n, m.index ?? 0);
        range.setEnd(n, (m.index ?? 0) + m[0].length);
        const r = range.getClientRects()[0];
        if (r && r.left >= box.left && r.right <= box.right + 1 && r.top >= box.top && r.bottom <= box.bottom + 1) {
          return m[0];
        }
      }
    }
    return "";
  });
}

/** A second device of the same user reads the book online to a later place and leaves; returns once the server holds it. */
async function anotherDeviceReadsOn(
  browser: Browser,
  id: string,
): Promise<{ page: Page; word: string }> {
  const b = await signedIn(browser);
  await b.page.goto(`/ui/read/${id}`);
  await expect(b.page.locator(".pharos-reader")).toBeVisible({ timeout: 20_000 });
  await expect.poll(() => firstWord(b.page), { timeout: 20_000 }).not.toBe("");
  let word = await firstWord(b.page);
  for (let turns = 0; turns < 3; turns++) {
    const before = word;
    await b.page.keyboard.press("ArrowRight");
    await expect.poll(() => firstWord(b.page), { timeout: 10_000 }).not.toBe(before);
    word = await firstWord(b.page);
  }
  expect(word).not.toBe("zq0x0x0");
  await b.page.getByRole("button", { name: "Back" }).click();
  await expect(b.page.locator("#pharos-main-action")).toBeVisible({ timeout: 15_000 });

  // The server has it once a fresh online open of the book lands there.
  await expect
    .poll(
      async () => {
        await b.page.goto(`/ui/read/${id}`);
        await expect(b.page.locator(".pharos-reader")).toBeVisible({ timeout: 20_000 });
        await expect.poll(() => firstWord(b.page), { timeout: 20_000 }).not.toBe("");
        return firstWord(b.page);
      },
      { timeout: 40_000 },
    )
    .toBe(word);
  return { page: b.page, word };
}

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

  test("a kept book opens in a tunnel, where the browser says it is online", async ({ browser }) => {
    const { page, logs } = await signedIn(browser);
    await keepFixture(page);
    const id = (await record(page)).books[0].id as string;

    // The socket opens and never answers, as in a tunnel; onLine stays true.
    await page.context().routeWebSocket(/\/pharos\/rpc/, () => {});
    expect(await page.evaluate(() => navigator.onLine)).toBe(true);
    const began = Date.now();
    await page.goto(`/ui/read/${id}`);
    await expect(page.locator(".pharos-reader")).toBeVisible({ timeout: 10_000 });
    await expect
      .poll(
        () =>
          page.evaluate(() => {
            const root = document.getElementById("pharos-reader-host")?.shadowRoot;
            return root?.querySelector(".quire-strip")?.textContent?.includes("zq0x0x0") ?? false;
          }),
        { timeout: 10_000 },
      )
      .toBe(true);
    console.log(`tunnel open took ${Date.now() - began} ms`);
    expect(logs.some((l) => /^pharos: reader opened item=\d+ format=epub from=cache/.test(l))).toBe(true);
    await page.context().close();
  });

  test("a place read offline is kept, resumed, and sent by the one save queue", async ({
    browser,
  }) => {
    const { page, logs } = await signedIn(browser);
    await keepFixture(page);
    const id = (await record(page)).books[0].id as string;

    await page.context().setOffline(true);
    await page.goto(`/ui/read/${id}`);
    await expect(page.locator(".pharos-reader")).toBeVisible({ timeout: 30_000 });
    await expect(page.locator(".quire-strip")).toContainText("zq0x0x0");
    await page.keyboard.press("ArrowRight");
    // The place is in the kept record and in the save queue, and only there.
    await expect
      .poll(async () => (await record(page)).books[0].locator, { timeout: 10_000 })
      .not.toBeNull();
    const placed = (await record(page)).books[0].locator as string;
    const queued = await page.evaluate(() => JSON.parse(localStorage.getItem("pharos.reader.pending") ?? "{}"));
    expect(Object.keys(queued)).toEqual([id]);
    expect(queued[id].locator).toBe(placed);
    expect(queued[id].read_at_ms).toBeGreaterThan(0);

    // A reload offline resumes at that place.
    await page.reload();
    await expect(page.locator(".pharos-reader")).toBeVisible({ timeout: 30_000 });
    await expect
      .poll(() => logs.filter((l) => /from=cache bytes=\d+ ms=\d+ resumed=true/.test(l)).length)
      .toBeGreaterThan(0);

    // Back online, the app-level flusher sends it and empties the queue.
    await page.context().setOffline(false);
    await expect
      .poll(
        () => page.evaluate(() => Object.keys(JSON.parse(localStorage.getItem("pharos.reader.pending") ?? "{}")).length),
        { timeout: 60_000 },
      )
      .toBe(0);
    await page.context().close();
  });

  test("a kept book opens at the place another device read since", async ({ browser }) => {
    // Device A keeps the book, with whatever place the server holds for it.
    const a = await signedIn(browser);
    await keepFixture(a.page);
    const id = (await record(a.page)).books[0].id as string;
    const placeBefore = (await record(a.page)).books[0].locator as string | null;

    // Device B, the same user online, reads on and leaves, which saves the place.
    const { page: bPage, word } = await anotherDeviceReadsOn(browser, id);

    // A opens its kept copy: from the cache, then at B's place.
    await a.page.goto(`/ui/read/${id}`);
    await expect(a.page.locator(".pharos-reader")).toBeVisible({ timeout: 20_000 });
    await expect.poll(() => firstWord(a.page), { timeout: 20_000 }).toBe(word);
    expect(a.logs.some((l) => /^pharos: reader opened item=\d+ format=epub from=cache/.test(l))).toBe(true);
    expect(a.logs.some((l) => /^pharos: reader moved to the server place item=\d+ /.test(l))).toBe(true);
    // The record keeps the place too, for the next open with no server.
    await expect.poll(async () => (await record(a.page)).books[0].locator).not.toBe(placeBefore);
    await a.page.context().close();
    await bPage.context().close();
  });

  test("a kept book opened and left without a turn leaves the server's newer place alone", async ({
    browser,
  }) => {
    const a = await signedIn(browser);
    await keepFixture(a.page);
    const id = (await record(a.page)).books[0].id as string;
    const { page: bPage, word } = await anotherDeviceReadsOn(browser, id);

    // A opens its kept copy and leaves at once, turning nothing.
    await a.page.goto(`/ui/read/${id}`);
    await expect(a.page.locator(".pharos-reader")).toBeVisible({ timeout: 20_000 });
    await a.page.getByRole("button", { name: "Back" }).click();
    await expect(a.page.locator("#pharos-main-action")).toBeVisible({ timeout: 15_000 });
    await a.page.waitForTimeout(2_000);
    expect(a.logs.some((l) => /^pharos: reader save item=/.test(l))).toBe(false);

    // A device that opens the book now lands at B's place.
    const c = await signedIn(browser);
    await c.page.goto(`/ui/read/${id}`);
    await expect(c.page.locator(".pharos-reader")).toBeVisible({ timeout: 20_000 });
    await expect.poll(() => firstWord(c.page), { timeout: 20_000 }).toBe(word);
    for (const p of [a.page, bPage, c.page]) await p.context().close();
  });

  test("keeping a PDF also stores the PDF module for the offline reader", async ({ browser }) => {
    const { page } = await signedIn(browser);
    await openBookPage(page, "Reader Paged");
    await page.locator("#pharos-keep-offline").click();
    await expect(page.locator("#pharos-keep-offline")).toHaveText("Remove from device", {
      timeout: 30_000,
    });
    expect((await record(page)).books[0]).toMatchObject({ format: "pdf" });
    const shell = await page.evaluate(async () => {
      const out: string[] = [];
      for (const name of await caches.keys()) {
        if (!name.startsWith("pharos-shell-")) continue;
        const cache = await caches.open(name);
        for (const r of await cache.keys()) out.push(new URL(r.url).pathname);
      }
      return out;
    });
    expect(shell).toContain("/ui/assets/pdf/quire_pdf_bg.wasm");
    expect(shell).toContain("/ui/assets/pdf/quire_pdf.js");
    expect(shell).toContain("/ui/assets/pdf/loader.js");
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
