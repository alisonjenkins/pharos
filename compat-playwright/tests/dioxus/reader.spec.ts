import {
  test,
  expect,
  type APIRequestContext,
  type Browser,
  type BrowserContext,
  type Page,
} from "@playwright/test";
import * as fs from "fs";
import * as path from "path";

// Spec 023 (B410): the native book reader. The seed (`just
// compat-dioxus-full`) writes "Reader Fixture.epub", three chapters of
// unique word tokens ("zq1x4x17"), and "Reader Locked.azw3", a Kindle file
// the scanner cannot convert (the DRM case, V353), and "Reader Paged.pdf", five
// pages with a different drawing each (US5).
//
// "On screen" is geometry, not visibility: the pages are CSS columns, so text
// on another page is in the DOM and `toBeVisible` but clipped away. The
// helpers below ask which words lie inside the page frame.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";
/** Where to leave the review screenshots, when set. */
const SHOTS = process.env.READER_SHOTS;

async function newReaderContext(
  browser: Browser,
  viewport: { width: number; height: number },
  deviceScaleFactor = 1,
): Promise<BrowserContext> {
  return browser.newContext({
    baseURL: PHAROS_URL,
    viewport,
    deviceScaleFactor,
    // The UI service worker (ADR-0110) fetches assets itself, and
    // page.route never sees a request a service worker serves, so the
    // tests that block a reader module would not block anything.
    serviceWorkers: "block",
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
}

/** Echoes the reader's own log lines, which say what was saved and restored. */
function echoReaderLog(page: Page, who: string): void {
  page.on("console", (m) => {
    if (m.text().startsWith("pharos: reader")) console.log(`[${who}] ${m.text()}`);
  });
}

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

/** The words of the book lying inside the page frame, in reading order. */
async function wordsOnPage(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const host = document.getElementById("pharos-reader-host");
    const root = host?.shadowRoot;
    const frame = root?.querySelector(".quire-frame");
    const strip = root?.querySelector(".quire-strip");
    if (!frame || !strip) return [];
    const box = frame.getBoundingClientRect();
    const words: string[] = [];
    const walker = document.createTreeWalker(strip, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      const text = n.nodeValue ?? "";
      for (const m of text.matchAll(/zq\d+x\d+x\d+/g)) {
        const range = document.createRange();
        range.setStart(n, m.index ?? 0);
        range.setEnd(n, (m.index ?? 0) + m[0].length);
        const r = range.getClientRects()[0];
        if (r && r.left >= box.left && r.right <= box.right + 1 && r.top >= box.top && r.bottom <= box.bottom + 1) {
          words.push(m[0]);
        }
      }
    }
    return words;
  });
}

/** Waits for the first page to be laid out and returns its first word. */
async function firstWord(page: Page): Promise<string> {
  await expect.poll(async () => (await wordsOnPage(page)).length, { timeout: 20_000 }).toBeGreaterThan(5);
  return (await wordsOnPage(page))[0];
}

async function turnForward(page: Page): Promise<string> {
  const before = await firstWord(page);
  await page.keyboard.press("ArrowRight");
  await expect.poll(async () => (await wordsOnPage(page))[0], { timeout: 10_000 }).not.toBe(before);
  return (await wordsOnPage(page))[0];
}

/** Where the PDF reader's canvas stands: its pixel size, CSS width and a hash of what it shows. */
async function pdfCanvas(
  page: Page,
): Promise<{ width: number; height: number; cssWidth: number; hostWidth: number; hash: number }> {
  return page.evaluate(() => {
    const host = document.getElementById("pharos-reader-host") as HTMLElement;
    const c = host.querySelector("canvas") as HTMLCanvasElement | null;
    if (!c) return { width: 0, height: 0, cssWidth: 0, hostWidth: host.clientWidth, hash: 0 };
    let hash = 7;
    const url = c.toDataURL();
    for (let i = 0; i < url.length; i++) hash = (hash * 31 + url.charCodeAt(i)) >>> 0;
    return {
      width: c.width,
      height: c.height,
      cssWidth: c.getBoundingClientRect().width,
      hostWidth: host.clientWidth,
      hash,
    };
  });
}

/** Waits for the PDF reader to say it is on page `n` of 5 and for a page to be drawn. */
async function onPdfPage(page: Page, n: number): Promise<void> {
  await expect(page.locator(".pharos-reader-progress-label")).toHaveText(
    new RegExp(`^Page ${n} of 5 · \\d+%$`),
    { timeout: 20_000 },
  );
  await expect.poll(async () => (await pdfCanvas(page)).height, { timeout: 20_000 }).toBeGreaterThan(0);
}

test.describe("book reader (spec 023)", () => {
  test("opens, turns, resizes and resumes in another browser at another size", async ({
    browser,
    request,
  }) => {
    const first = await newReaderContext(browser, { width: 1280, height: 800 }, 2);
    const page = await first.newPage();
    echoReaderLog(page, "first");
    await signIn(page, request);
    await openFromSearch(page, "Reader Fixture", "Reader Fixture");

    // Never opened: the button says Read, and the book starts at its first word.
    const read = page.locator("#pharos-main-action");
    await expect(read).toHaveText("Read");
    await read.click();
    await expect(page.locator(".pharos-reader")).toBeVisible({ timeout: 15_000 });
    expect(await firstWord(page)).toBe("zq0x0x0");

    // Page turns: keys, then the right third of the page.
    const afterKey = await turnForward(page);
    expect(afterKey).not.toBe("zq0x0x0");
    const frame = page.locator("#pharos-reader-host");
    const box = (await frame.boundingBox())!;
    await page.mouse.click(box.x + box.width - 20, box.y + box.height / 2);
    await expect.poll(async () => (await wordsOnPage(page))[0]).not.toBe(afterKey);
    await page.keyboard.press("PageUp");
    await expect.poll(async () => (await wordsOnPage(page))[0]).toBe(afterKey);
    await expect(page.locator(".pharos-reader-progress-label")).not.toHaveText("0%");

    if (SHOTS) {
      fs.mkdirSync(SHOTS, { recursive: true });
      await page.screenshot({ path: path.join(SHOTS, "reader-desktop-1280x800.png") });
    }

    // The table of contents jumps to a chapter.
    await page.getByRole("button", { name: "Contents" }).click();
    await page.getByRole("button", { name: "Chapter 2" }).click();
    await expect.poll(async () => (await wordsOnPage(page))[0]).toBe("zq1x0x0");

    // Raising the font size keeps the first word of the page on screen (V352).
    await turnForward(page);
    const placed = await firstWord(page);
    await page.getByRole("button", { name: "Text settings" }).click();
    await page.getByRole("button", { name: "Larger text" }).click();
    await page.getByRole("button", { name: "Larger text" }).click();
    await expect(page.locator(".pharos-reader-size")).toHaveText("22");
    await expect.poll(async () => await wordsOnPage(page)).toContain(placed);

    if (SHOTS) {
      await page.setViewportSize({ width: 1920, height: 1080 });
      await expect.poll(async () => await wordsOnPage(page)).toContain(placed);
      await page.screenshot({ path: path.join(SHOTS, "reader-settings-1920x1080.png") });
      await page.setViewportSize({ width: 1280, height: 800 });
    }

    // Leaving saves; the title page then offers to continue.
    await page.getByRole("button", { name: "Text settings" }).click();
    await expect.poll(async () => await wordsOnPage(page)).toContain(placed);
    await page.getByRole("button", { name: "Back" }).click();
    const resume = page.locator("#pharos-main-action");
    await expect(resume).toBeVisible({ timeout: 15_000 });
    await expect
      .poll(
        async () => {
          if (/^Continue reading · \d+%$/.test((await resume.textContent()) ?? "")) return true;
          await page.reload();
          await expect(page.locator("#pharos-main-action")).toBeVisible({ timeout: 15_000 });
          return false;
        },
        { timeout: 30_000 },
      )
      .toBe(true);

    // Another browser, another window size and default text size: the same
    // passage (SC-004). The place is a character, not a page, so the page
    // there may start earlier or later than here; the word that was first
    // on the page when the reader last turned it must be on it.
    const second = await newReaderContext(browser, { width: 900, height: 1000 });
    const other = await second.newPage();
    echoReaderLog(other, "second");
    await signIn(other, request);
    await openFromSearch(other, "Reader Fixture", "Reader Fixture");
    await expect(other.locator("#pharos-main-action")).toHaveText(/^Continue reading · \d+%$/);
    await other.locator("#pharos-main-action").click();
    await expect(other.locator(".pharos-reader")).toBeVisible({ timeout: 15_000 });
    await firstWord(other);
    await expect.poll(async () => await wordsOnPage(other)).toContain(placed);

    // Home lists the part-read book under Continue reading.
    await other.goto("/ui/");
    const rail = other.locator("section.pharos-rail", {
      has: other.getByRole("heading", { name: "Continue reading" }),
    });
    await expect(rail).toBeVisible({ timeout: 15_000 });
    // Its own tile, by title: other books read in this suite share the rail.
    await expect(rail.locator('a[href^="/ui/read/"]').filter({ hasText: "Reader Fixture" })).toHaveCount(1);

    await first.close();
    await second.close();
  });

  test("a place read offline is kept on the device and sent when the connection returns (spec 036)", async ({
    browser,
    request,
  }) => {
    test.setTimeout(150_000);
    const first = await newReaderContext(browser, { width: 1280, height: 800 });
    const page = await first.newPage();
    echoReaderLog(page, "offline");
    // `setOffline` stops new requests but leaves an open socket up, so the
    // RPC socket is held by the test and cut with it.
    let reachable = true;
    const sockets: { close(): void }[] = [];
    await page.routeWebSocket(/\/pharos\/rpc/, (ws) => {
      if (!reachable) {
        ws.close();
        return;
      }
      ws.connectToServer();
      sockets.push(ws);
    });
    await signIn(page, request);
    await openFromSearch(page, "Reader Fixture", "Reader Fixture");
    await page.locator("#pharos-main-action").click();
    await expect(page.locator(".pharos-reader")).toBeVisible({ timeout: 15_000 });
    await firstWord(page);
    const pending = () =>
      page.evaluate(() => JSON.parse(localStorage.getItem("pharos.reader.pending") ?? "{}"));
    const note = page.locator(".pharos-reader-offline");
    await expect(note).toHaveCount(0);

    // The tunnel: the connection goes, pages keep turning.
    reachable = false;
    await first.setOffline(true);
    for (const s of sockets) s.close();
    const readAtLeast = Date.now();
    let placed = "";
    for (let i = 0; i < 3; i++) placed = await turnForward(page);
    await expect(note).toHaveText("Offline \u00b7 your place is saved on this device", {
      timeout: 15_000,
    });
    await expect.poll(async () => Object.keys(await pending()).length).toBe(1);
    const queued = Object.values(await pending())[0] as { read_at_ms: number };
    expect(queued.read_at_ms).toBeGreaterThanOrEqual(readAtLeast);
    expect(queued.read_at_ms).toBeLessThanOrEqual(Date.now());

    // The way back: the place is sent without anything from the reader.
    reachable = true;
    await first.setOffline(false);
    await expect(note).toHaveCount(0, { timeout: 90_000 });
    await expect.poll(async () => Object.keys(await pending()).length).toBe(0);

    // Another browser opens at the place read in the tunnel.
    const second = await newReaderContext(browser, { width: 1280, height: 800 });
    const other = await second.newPage();
    echoReaderLog(other, "after");
    await signIn(other, request);
    await openFromSearch(other, "Reader Fixture", "Reader Fixture");
    await other.locator("#pharos-main-action").click();
    await expect(other.locator(".pharos-reader")).toBeVisible({ timeout: 15_000 });
    await firstWord(other);
    await expect.poll(async () => await wordsOnPage(other)).toContain(placed);

    await first.close();
    await second.close();
  });

  test("a place queued offline is sent from the home page once the connection returns (spec 036)", async ({
    browser,
    request,
  }) => {
    test.setTimeout(150_000);
    const first = await newReaderContext(browser, { width: 1280, height: 800 });
    const page = await first.newPage();
    echoReaderLog(page, "leaver");
    let reachable = true;
    const sockets: { close(): void }[] = [];
    await page.routeWebSocket(/\/pharos\/rpc/, (ws) => {
      if (!reachable) {
        ws.close();
        return;
      }
      ws.connectToServer();
      sockets.push(ws);
    });
    await signIn(page, request);
    await openFromSearch(page, "Reader Fixture", "Reader Fixture");
    await page.locator("#pharos-main-action").click();
    await expect(page.locator(".pharos-reader")).toBeVisible({ timeout: 15_000 });
    await firstWord(page);
    const pending = () =>
      page.evaluate(() => JSON.parse(localStorage.getItem("pharos.reader.pending") ?? "{}"));

    reachable = false;
    await first.setOffline(true);
    for (const s of sockets) s.close();
    let placed = "";
    for (let i = 0; i < 4; i++) placed = await turnForward(page);
    await expect.poll(async () => Object.keys(await pending()).length).toBe(1);

    // Out of the book and onto home: no reader is open to send it.
    await page.getByRole("button", { name: "Back" }).click();
    await page.locator(".pharos-brand").click();
    await expect(page).toHaveURL(/\/ui\/?$/);
    expect(Object.keys(await pending()).length).toBe(1);

    reachable = true;
    await first.setOffline(false);
    await expect.poll(async () => Object.keys(await pending()).length, { timeout: 90_000 }).toBe(0);

    const second = await newReaderContext(browser, { width: 1280, height: 800 });
    const other = await second.newPage();
    echoReaderLog(other, "after-leave");
    await signIn(other, request);
    await openFromSearch(other, "Reader Fixture", "Reader Fixture");
    await other.locator("#pharos-main-action").click();
    await expect(other.locator(".pharos-reader")).toBeVisible({ timeout: 15_000 });
    await firstWord(other);
    await expect.poll(async () => await wordsOnPage(other)).toContain(placed);

    await first.close();
    await second.close();
  });

  test("a locked Kindle book has no Read button and offers the download (V353)", async ({
    page,
    request,
  }) => {
    await signIn(page, request);
    await openFromSearch(page, "Reader Locked", "Reader Locked");
    await expect(page.locator(".pharos-main")).toContainText("locked to a Kindle device");
    await expect(page.getByRole("link", { name: /^(Read|Continue reading · \d+%)$/ })).toHaveCount(0);
    await expect(page.getByRole("link", { name: "Download" })).toHaveAttribute(
      "href",
      /\/Items\/\d+\/Download$/,
    );
  });

  test("a PDF opens from a module fetched only then, turns pages and resumes (US5)", async ({
    browser,
    request,
  }) => {
    const first = await newReaderContext(browser, { width: 1280, height: 800 }, 2);
    const page = await first.newPage();
    echoReaderLog(page, "pdf first");
    const moduleRequests: string[] = [];
    page.on("request", (r) => {
      const u = new URL(r.url());
      if (u.pathname.startsWith("/ui/assets/pdf/")) moduleRequests.push(u.pathname.split("/").pop() ?? "");
    });
    await signIn(page, request);
    await openFromSearch(page, "Reader Paged", "Reader Paged");

    // The main app and the title page have not touched the PDF engine.
    expect(moduleRequests).toEqual([]);
    const read = page.locator("#pharos-main-action");
    await expect(read).toHaveText("Read");
    await read.click();
    await expect(page.locator(".pharos-reader")).toBeVisible({ timeout: 15_000 });
    await onPdfPage(page, 1);
    expect(moduleRequests).toEqual(expect.arrayContaining(["loader.js", "quire_pdf.js", "quire_pdf_bg.wasm"]));

    // One canvas, a letter page at the window's pixel ratio under the 4096 px cap.
    const one = await pdfCanvas(page);
    expect(one.height).toBeLessThanOrEqual(4096);
    expect(one.width / one.height).toBeCloseTo(612 / 792, 2);
    expect(one.height).toBeGreaterThan(800); // 2x on a 1280x800 window
    await expect(page.locator("#pharos-reader-host canvas")).toHaveCount(1);

    // Keys, then the right third of the page, then back.
    await page.keyboard.press("ArrowRight");
    await onPdfPage(page, 2);
    const two = await pdfCanvas(page);
    expect(two.hash).not.toBe(one.hash);
    const box = (await page.locator("#pharos-reader-host").boundingBox())!;
    await page.mouse.click(box.x + box.width - 20, box.y + box.height / 2);
    await onPdfPage(page, 3);
    const three = await pdfCanvas(page);
    expect(three.hash).not.toBe(two.hash);
    await page.keyboard.press("PageUp");
    await onPdfPage(page, 2);
    expect((await pdfCanvas(page)).hash).toBe(two.hash);
    await page.keyboard.press("ArrowRight");
    await onPdfPage(page, 3);
    expect((await pdfCanvas(page)).hash).toBe(three.hash);

    if (SHOTS) {
      fs.mkdirSync(SHOTS, { recursive: true });
      await page.screenshot({ path: path.join(SHOTS, "reader-pdf-1280x800-dsf2.png") });
    }

    // Fit width fills the host's width, and fit page restores the whole page.
    const fit = page.getByRole("button", { name: "Fit width" });
    await fit.click();
    await expect(fit).toHaveAttribute("aria-pressed", "true");
    await expect
      .poll(async () => {
        const c = await pdfCanvas(page);
        return Math.abs(c.cssWidth - c.hostWidth) < 2;
      })
      .toBe(true);
    await fit.click();
    await expect
      .poll(async () => (await pdfCanvas(page)).cssWidth)
      .toBeLessThan(box.width / 2);
    expect((await pdfCanvas(page)).hash).toBe(three.hash);

    // Leaving saves page 3; the title page offers to continue at 60%.
    await page.getByRole("button", { name: "Back" }).click();
    const resume = page.locator("#pharos-main-action");
    await expect(resume).toBeVisible({ timeout: 15_000 });
    await expect
      .poll(
        async () => {
          if ((await resume.textContent()) === "Continue reading · 60%") return true;
          await page.reload();
          await expect(page.locator("#pharos-main-action")).toBeVisible({ timeout: 15_000 });
          return false;
        },
        { timeout: 30_000 },
      )
      .toBe(true);

    // Another browser at the same size lands on the same page.
    const second = await newReaderContext(browser, { width: 1280, height: 800 }, 2);
    const other = await second.newPage();
    echoReaderLog(other, "pdf second");
    await signIn(other, request);
    await openFromSearch(other, "Reader Paged", "Reader Paged");
    await expect(other.locator("#pharos-main-action")).toHaveText("Continue reading · 60%");
    await other.locator("#pharos-main-action").click();
    await onPdfPage(other, 3);
    expect((await pdfCanvas(other)).hash).toBe(three.hash);

    await first.close();
    await second.close();
  });

  test("a PDF module that traps is replaced on the next open", async ({ browser, request }) => {
    const ctx = await newReaderContext(browser, { width: 1280, height: 800 });
    const page = await ctx.newPage();
    echoReaderLog(page, "pdf trap");
    const loaders: string[] = [];
    page.on("request", (r) => {
      if (new URL(r.url()).pathname === "/ui/assets/pdf/loader.js") loaders.push(new URL(r.url()).search);
    });
    await signIn(page, request);
    await openFromSearch(page, "Reader Paged", "Reader Paged");
    await page.locator("#pharos-main-action").click();
    await expect(page.locator(".pharos-reader-progress-label")).toHaveText(/^Page \d of 5 · \d+%$/, {
      timeout: 20_000,
    });
    expect(loaders).toHaveLength(1);

    // The engine "panics": every call into the module now traps. The next
    // draw (the page drawn ahead) is enough to break the reader.
    await page.evaluate(() => {
      const w = window as unknown as { QuirePdfDocument: { prototype: { render: () => never } } };
      w.QuirePdfDocument.prototype.render = () => {
        throw new WebAssembly.RuntimeError("unreachable");
      };
    });
    await page.keyboard.press("ArrowLeft");
    await page.keyboard.press("ArrowRight");
    const note = page.locator(".pharos-reader-note[role=alert]");
    await expect(note).toContainText("stopped", { timeout: 10_000 });
    await expect(note.getByRole("link", { name: "Download" })).toBeVisible();

    // Opening again loads a fresh module instead of reusing the broken one.
    await page.getByRole("button", { name: "Back" }).click();
    await page.locator("#pharos-main-action").click();
    await expect(page.locator(".pharos-reader-progress-label")).toHaveText(/^Page \d of 5 · \d+%$/, {
      timeout: 20_000,
    });
    expect(await pdfCanvas(page).then((c) => c.height)).toBeGreaterThan(0);
    expect(loaders).toHaveLength(2);
    expect(new Set(loaders).size).toBe(2);
    await ctx.close();
  });

  test("a PDF module that fails to load is tried again on the next open", async ({
    browser,
    request,
  }) => {
    const ctx = await newReaderContext(browser, { width: 1280, height: 800 });
    const page = await ctx.newPage();
    echoReaderLog(page, "pdf retry");
    let blocked = true;
    await page.route("**/ui/assets/pdf/quire_pdf_bg.wasm", (route) =>
      blocked ? route.abort() : route.continue(),
    );
    await signIn(page, request);
    await openFromSearch(page, "Reader Paged", "Reader Paged");
    await page.locator("#pharos-main-action").click();
    const note = page.locator(".pharos-reader-note[role=alert]");
    await expect(note).toBeVisible({ timeout: 20_000 });
    await expect(note.getByRole("link", { name: "Download" })).toBeVisible();

    blocked = false;
    await page.getByRole("button", { name: "Back" }).click();
    await page.locator("#pharos-main-action").click();
    await expect(page.locator(".pharos-reader-progress-label")).toHaveText(/^Page \d of 5 · \d+%$/, {
      timeout: 20_000,
    });
    await expect(page.locator("script[data-quire-pdf]")).toHaveCount(1);
    await ctx.close();
  });
});
