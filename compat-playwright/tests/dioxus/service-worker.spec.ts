import { test, expect, type Page } from "@playwright/test";

// Spec 036 FR-003 (ADR-0110): the service worker caches the app shell, so
// /ui/ opens with no connection, and owns only `pharos-shell-*` caches. The
// worker is the server's rendered /ui/sw.js; nothing here signs in, because
// the shell is public.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";

/** Loads /ui/ and waits until the worker is active and controls the page. */
async function openControlled(page: Page): Promise<void> {
  await page.goto("/ui/");
  await page.evaluate(() => navigator.serviceWorker.ready);
  // The first load is not controlled until the worker claims it.
  await page.waitForFunction(() => navigator.serviceWorker.controller !== null);
}

const cacheNames = (page: Page) => page.evaluate(() => caches.keys());

test.use({
  baseURL: PHAROS_URL,
  // Service workers are blocked by Playwright's default only when asked to.
  serviceWorkers: "allow",
});

test("the shell opens offline from the worker's cache", async ({ page, context }) => {
  await openControlled(page);
  const names = (await cacheNames(page)).filter((n) => n.startsWith("pharos-shell-"));
  expect(names).toHaveLength(1);
  const cached = await page.evaluate(async (name) => {
    const cache = await caches.open(name);
    return (await cache.keys()).map((r) => new URL(r.url).pathname);
  }, names[0]);
  expect(cached).toContain("/ui/");
  expect(cached.some((p) => p.endsWith(".wasm"))).toBe(true);
  expect(cached.some((p) => p.includes("/assets/pdf/"))).toBe(false);

  await context.setOffline(true);
  await page.reload();
  await expect(page).toHaveTitle("Pharos");
  // The app mounted: dx's #main has content, not the browser's error page.
  await expect(page.locator("#main > *").first()).toBeAttached({ timeout: 30_000 });
  await context.setOffline(false);
});

test("the worker leaves the API alone", async ({ page }) => {
  await openControlled(page);
  await page.evaluate(() =>
    fetch("/pharos/session", { headers: { Accept: "application/json" } }).catch(() => null),
  );
  const urls = await page.evaluate(async () => {
    const out: string[] = [];
    for (const name of await caches.keys()) {
      const cache = await caches.open(name);
      for (const r of await cache.keys()) out.push(new URL(r.url).pathname);
    }
    return out;
  });
  expect(urls.length).toBeGreaterThan(0);
  for (const p of urls) expect(p.startsWith("/ui/"), p).toBe(true);
});

test("a new worker deletes old shell caches and keeps the book caches", async ({ page }) => {
  await openControlled(page);
  await page.evaluate(async () => {
    await caches.open("pharos-shell-old");
    await caches.open("pharos-books-kept");
  });
  // A new script URL under the same scope is a new worker that installs and
  // activates, as a deploy's does (its bytes would differ).
  await page.evaluate(async () => {
    await navigator.serviceWorker.register("/ui/sw.js?next");
    await navigator.serviceWorker.ready;
  });
  await expect
    .poll(async () => (await cacheNames(page)).includes("pharos-shell-old"), { timeout: 15_000 })
    .toBe(false);
  const names = await cacheNames(page);
  expect(names).toContain("pharos-books-kept");
  expect(names.filter((n) => n.startsWith("pharos-shell-"))).toHaveLength(1);
});
