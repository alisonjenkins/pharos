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
// the scanner cannot convert (the DRM case, V353).
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
    await expect(rail.locator('a[href^="/ui/read/"]')).toHaveCount(1);

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
});
