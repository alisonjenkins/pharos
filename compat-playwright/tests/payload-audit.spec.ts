import { test, expect, Page } from "@playwright/test";

// A walk across jellyfin-web's views that records the SIZE of every response,
// so oversized payloads surface as data rather than as somebody noticing a slow
// page. B153 (an album page pulling 51.6 MB of JSON) and B162 (an uncompressed
// 1 MB stylesheet) were both found by hand from a single curl; this makes the
// same question answerable in one run, across every view.
//
// Authenticates through jellyfin-web's own `ApiClient.setAuthenticationInfo`
// rather than driving the login form, so it can point at a REAL server with an
// access token and no password. Seeding `jellyfin_credentials` in localStorage
// does NOT work — the app re-validates and bounces to /login.
//
//   AUDIT_BASE   origin serving jellyfin-web (its angie proxies the API)
//   AUDIT_TOKEN  an access token for that server
//   AUDIT_USER   the user id the token belongs to
//
// Reports rather than asserts a size budget: what counts as "too big" depends
// on the library, and a threshold tuned here would either be noise on a small
// library or silence on a large one. The failure it DOES guard is a view that
// fetched nothing, which is how a broken walk masquerades as a clean result.

const BASE = process.env.AUDIT_BASE ?? "http://127.0.0.1:8096";
const TOKEN = process.env.AUDIT_TOKEN ?? "";
const USER = process.env.AUDIT_USER ?? "";

type Entry = {
  url: string;
  /** Bytes on the wire, including headers; 0 for a cache hit. */
  transfer: number;
  /** Bytes after decompression — what the page actually parses. */
  decoded: number;
  view: string;
};

async function authenticate(page: Page) {
  await page.goto(`${BASE}/web/index.html`, { waitUntil: "domcontentloaded" });
  // The app boots, then redirects to /login; ApiClient is attached during that
  // boot, at a time that varies. `waitForFunction` binds to a frame the
  // redirect replaces, so poll from here instead.
  for (let i = 0; i < 40; i++) {
    const ready = await page
      .evaluate(() => Boolean((window as unknown as Record<string, unknown>).ApiClient))
      .catch(() => false);
    if (ready) break;
    await page.waitForTimeout(1000);
  }
  await page.evaluate(
    ({ token, user }) => {
      const w = window as unknown as Record<string, any>;
      w.ApiClient.setAuthenticationInfo(token, user);
    },
    { token: TOKEN, user: USER },
  );
}

/** Navigate to a hash route and return everything the browser fetched for it. */
async function walk(page: Page, view: string, hash: string): Promise<Entry[]> {
  await page.evaluate(() => performance.clearResourceTimings());
  await page.goto(`${BASE}/web/index.html${hash}`, {
    waitUntil: "domcontentloaded",
  });
  // Views fetch their data after the route resolves. `networkidle` is flaky
  // here because the /socket connection never idles, so wait a fixed beat.
  await page.waitForTimeout(7000);
  const raw = await page.evaluate(() =>
    performance.getEntriesByType("resource").map((e) => {
      const r = e as PerformanceResourceTiming;
      return { url: r.name, transfer: r.transferSize, decoded: r.decodedBodySize };
    }),
  );
  return raw.map((r) => ({ ...r, view }));
}

const VIEWS: Array<[string, string]> = [
  ["home", "#/home.html"],
  ["movies", "#/movies.html?topParentId="],
  ["tv", "#/tv.html?topParentId="],
  ["music", "#/music.html?topParentId="],
  ["music-albums", "#/music.html?tab=1&topParentId="],
  ["music-artists", "#/music.html?tab=2&topParentId="],
  ["music-genres", "#/music.html?tab=4&topParentId="],
  ["search", "#/search.html"],
];

test("payload audit: record every response size across the main views", async ({
  page,
}) => {
  test.skip(!TOKEN || !USER, "needs AUDIT_TOKEN and AUDIT_USER");
  test.setTimeout(600_000);

  await authenticate(page);

  const all: Entry[] = [];
  for (const [view, hash] of VIEWS) {
    all.push(...(await walk(page, view, hash)));
  }

  // Detail pages, reached by the ids the client itself renders — a hand-picked
  // id would not prove the view's own links resolve.
  for (const [view, listHash] of [
    ["album-detail", "#/music.html?tab=1&topParentId="],
    ["movie-detail", "#/movies.html?topParentId="],
    ["series-detail", "#/tv.html?topParentId="],
  ] as Array<[string, string]>) {
    await page.goto(`${BASE}/web/index.html${listHash}`, {
      waitUntil: "domcontentloaded",
    });
    await page.waitForTimeout(6000);
    const card = page.locator("a[href*='#/details?id=']").first();
    if (!(await card.count())) continue;
    const href = await card.getAttribute("href");
    if (!href) continue;
    all.push(...(await walk(page, view, href.startsWith("#") ? href : `#${href}`)));
  }

  const byView = new Map<string, number>();
  for (const e of all) byView.set(e.view, (byView.get(e.view) ?? 0) + e.decoded);

  // eslint-disable-next-line no-console
  console.log("\n=== total DECODED bytes per view ===");
  for (const [view, bytes] of [...byView].sort((a, b) => b[1] - a[1])) {
    // eslint-disable-next-line no-console
    console.log(`${(bytes / 1e6).toFixed(2)} MB  ${view}`);
  }

  const seen = new Set<string>();
  const big: Entry[] = [];
  for (const e of all.sort((x, y) => y.decoded - x.decoded)) {
    if (e.decoded <= 200_000) continue;
    const k = `${e.view}|${e.url}`;
    if (seen.has(k)) continue;
    seen.add(k);
    big.push(e);
  }
  // eslint-disable-next-line no-console
  console.log("\n=== responses over 200 KB ===");
  for (const e of big) {
    const ratio =
      e.transfer > 0 ? `x${(e.decoded / e.transfer).toFixed(1)}` : "cached";
    // eslint-disable-next-line no-console
    console.log(
      `${(e.decoded / 1e6).toFixed(2)} MB decoded / ${(e.transfer / 1e6).toFixed(2)} MB wire (${ratio})  [${e.view}]  ${e.url.replace(BASE, "")}`,
    );
  }

  // A view that fetched nothing means the walk broke, not that the app is
  // lean — fail loudly rather than report a reassuring zero.
  for (const [view] of VIEWS) {
    expect(byView.get(view) ?? 0, `${view} fetched nothing`).toBeGreaterThan(
      10_000,
    );
  }
});
