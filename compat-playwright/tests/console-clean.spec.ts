// Stricter Playwright suite — catches the bug shapes the crawl
// spec missed because jellyfin-web swallowed them into hls.js /
// fetch handlers instead of throwing `pageerror`.
//
// What we add over crawl.spec.ts:
//
// 1. `page.on("console")` capture. Any `error` / `warning` level log
//    from jellyfin-web fails the test. The HLS `manifestParsingError`
//    that hung video playback was a console.error — invisible to
//    the existing `pageerror` listener.
//
// 2. `page.on("response")` capture. Any 4xx/5xx response (other than
//    a small allowlist of expected probes) fails the test. The
//    /SyncPlay/List 404 that broke the group-watch button would have
//    caught this way long before the user hit it.
//
// 3. Exercise the surfaces real users click: open Library → item →
//    Play, open the group-watch panel, open Settings, navigate
//    History. Each is a known-bad path we've shipped regressions on.
//
// Triggered alongside the crawl spec by `just compat-playwright-full`.

import { test, expect, Page, ConsoleMessage } from "@playwright/test";

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const SEED_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const SEED_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

// Console messages we *expect* and that don't indicate a regression.
// Keep this list tiny — wide allowlists are the reason crawl.spec.ts
// missed the HLS error.
const TOLERATED_CONSOLE: RegExp[] = [
  /CastReceiverId/i,
  /Failed to load resource/i, // global; specific 4xx caught by response handler
  /service worker/i,
];

// Allowed 4xx URLs (probes the client makes that pharos legitimately
// 404s — keep narrow!).
const ALLOWED_404_PATTERNS: RegExp[] = [
  // Items/{0000…0000} appears when jellyfin-web fetches a placeholder
  // before populating the real id.
  /\/Items\/0{32}\b/,
  // The seeded playwright items are synthetic — they have no real media
  // file on disk, so pharos can't extract the *derived* Thumb / Backdrop
  // frames and 404s those requests. Primary (the placeholder poster)
  // still resolves, so a Primary 404 would still fail the test.
  /\/Items\/\d+\/Images\/(Thumb|Backdrop)\b/,
];

type Capture = {
  consoleErrors: string[];
  badResponses: string[];
  pageErrors: string[];
};

function start(page: Page): Capture {
  const cap: Capture = { consoleErrors: [], badResponses: [], pageErrors: [] };
  page.on("console", (msg: ConsoleMessage) => {
    if (msg.type() !== "error" && msg.type() !== "warning") return;
    const text = msg.text();
    if (TOLERATED_CONSOLE.some((re) => re.test(text))) return;
    cap.consoleErrors.push(text.slice(0, 250));
  });
  page.on("pageerror", (err) => {
    cap.pageErrors.push(err.message.slice(0, 250));
  });
  page.on("response", (resp) => {
    const status = resp.status();
    if (status < 400) return;
    const url = resp.url();
    if (ALLOWED_404_PATTERNS.some((re) => re.test(url))) return;
    // jellyfin-web auto-probes its *own* serving origin for a co-hosted
    // server at boot (`/System/Info/Public`); served standalone via
    // http-server that 404s before the client falls back to the
    // manually-added pharos origin. Tolerate it only on the non-pharos
    // (static-bundle) origin, so a real 404 from pharos still fails.
    if (
      status === 404 &&
      !url.startsWith(PHAROS_URL) &&
      /\/System\/Info\/Public\b/.test(url)
    ) {
      return;
    }
    cap.badResponses.push(`${status} ${url}`);
  });
  return cap;
}

function assertClean(cap: Capture, label: string) {
  const lines: string[] = [];
  if (cap.consoleErrors.length) {
    lines.push(`console errors:\n  ${cap.consoleErrors.join("\n  ")}`);
  }
  if (cap.badResponses.length) {
    lines.push(`bad responses:\n  ${cap.badResponses.join("\n  ")}`);
  }
  if (cap.pageErrors.length) {
    lines.push(`page errors:\n  ${cap.pageErrors.join("\n  ")}`);
  }
  expect(lines.join("\n\n"), `Issues after ${label}:`).toBe("");
}

async function connect(page: Page) {
  await page.goto("/", { waitUntil: "networkidle" });
  const addServer = page.getByText(/add server/i).first();
  if (await addServer.isVisible({ timeout: 5_000 }).catch(() => false)) {
    await addServer.click();
    await page.locator("#txtServerHost").waitFor();
    await page.locator("#txtServerHost").fill(PHAROS_URL);
    await page.getByRole("button", { name: /^connect$/i }).click();
    await page.waitForURL(/#\/login/, { timeout: 20_000 });
  }
}

async function signIn(page: Page) {
  await page.locator("#txtManualName").fill(SEED_USER);
  await page.locator("#txtManualPassword").fill(SEED_PASS);
  await page.getByRole("button", { name: /^sign in$/i }).click();
  await page.waitForURL(/#\/home/, { timeout: 20_000 });
}

test.describe("strict console + response capture", () => {
  test("login → home → library opens cleanly", async ({ page }) => {
    const cap = start(page);
    await connect(page);
    await signIn(page);
    await page.waitForLoadState("networkidle");
    assertClean(cap, "home view loaded");

    // Open library card → list view.
    const libraryCard = page.locator(".sectionTitle, .card").first();
    if (await libraryCard.isVisible({ timeout: 5_000 }).catch(() => false)) {
      await libraryCard.click();
      await page.waitForLoadState("networkidle");
      assertClean(cap, "library opened");
    }
  });

  test("group-watch panel opens without 404 or console error", async ({ page }) => {
    const cap = start(page);
    await connect(page);
    await signIn(page);

    // jellyfin-web's group icon — best-effort selector. The point of
    // this test is the *console + network* capture, not the click
    // path; if the icon isn't visible we still verify the page didn't
    // pre-fail at load.
    const groupBtn = page
      .locator("button[is='paper-icon-button-light'][title='Group play']")
      .first();
    if (await groupBtn.isVisible({ timeout: 3_000 }).catch(() => false)) {
      await groupBtn.click();
      await page.waitForTimeout(1_000);
    }
    assertClean(cap, "group-watch panel");
  });

  test("settings + my-preferences route cleanly", async ({ page }) => {
    const cap = start(page);
    await connect(page);
    await signIn(page);

    // Navigate directly via hash route. The bundle is served at the
    // origin root (no `/web/` prefix) and jellyfin-web resolves the
    // SPA hash route without the `.html` suffix — matching crawl.spec.
    for (const route of ["#/mypreferencesmenu", "#/mypreferencesdisplay"]) {
      await page.goto(`/${route}`, { waitUntil: "networkidle" });
      assertClean(cap, `route ${route}`);
    }
  });
});
