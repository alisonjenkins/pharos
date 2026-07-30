import { test, expect, Page } from "@playwright/test";

// 004-books (T050) — SC-001, against unmodified jellyfin-web in real Chromium.
//
// pharos implements NO reader. `bookPlayer` is epub.js shipped inside the
// jellyfin-web bundle, and it opens a book only if THREE server-side gates are
// satisfied:
//
//   1. `canPlayMediaType(mediaType)` → `"book" === (e || "").toLowerCase()`
//   2. `canPlayItem(item)`          → `item.Path?.endsWith("epub")`
//   3. bytes                        → `Items/{id}/Download?api_key=…`
//
// All three fail SILENTLY. A missing `Path` makes `canPlayItem` return false and
// jellyfin-web declines with no error, no toast and no network request — nothing
// in a log, nothing in a screenshot. That is precisely why this spec exists and
// why it asserts the reader actually MOUNTED and a page actually TURNED, rather
// than that a click did not throw.
//
// Pre-reqs (same as the other specs): pharos on PHAROS_URL seeded via
// `admin seed-playwright-user`, which writes a real minimal epub — two chapters,
// so a page turn is observable — and registers it as item id 9.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const SEED_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const SEED_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

/** The seeded epub's item id (see `register_seed_items`). */
const BOOK_ID = "9";
/// The 32-hex wire id jellyfin-web addresses items by; `9` is the internal id.
const BOOK_WIRE_ID = "00000000000000000000000000000009";

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
  await page.waitForURL(/#\/login/, { timeout: 20_000 });
  await page.locator("#txtManualName").waitFor({ timeout: 10_000 });
  await page.locator("#txtManualName").fill(username);
  await page.locator("#txtManualPassword").fill(password);
  await page.getByRole("button", { name: /^sign in$/i }).click();
  await page.waitForURL(/#\/home/, { timeout: 25_000 });
}

async function serverId(page: Page): Promise<string> {
  const id = await page.evaluate(() => {
    try {
      return (
        JSON.parse(window.localStorage.getItem("jellyfin_credentials")!)
          .Servers?.[0]?.Id ?? null
      );
    } catch (_e) {
      return null;
    }
  });
  if (!id) throw new Error("could not read serverId from jellyfin-web localStorage");
  return id;
}

test.describe("books: unmodified jellyfin-web opens an epub", () => {
  // Gate 1 + 2, read out of the CLIENT's own API call rather than curl, so the
  // assertion is about what jellyfin-web actually received.
  test("the item pharos serves satisfies both canPlay gates", async ({ page }) => {
    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);

    // NO Fields — deliberately. Captured from a real session, the details page
    // fetches `/Users/{uid}/Items/{id}` with no Fields at all and hands THAT
    // object to playbackManager. So this is the request whose answer decides
    // whether a book opens, and asking for Fields here would test a request the
    // client never makes.
    const item = await page.evaluate(async (id) => {
      const api = (window as any).ApiClient;
      return await api.getItem(api.getCurrentUserId(), id);
    }, BOOK_WIRE_ID);

    expect(item.Type).toBe("Book");
    // Gate 1. `canPlayMediaType` lowercases, so casing is not the risk — absence
    // or "Video" is, and "Video" is what a wildcard match arm produces.
    expect(String(item.MediaType).toLowerCase()).toBe("book");

    // Gate 2. bookPlayer's compare is `item.Path?.endsWith("epub")` and it is
    // CASE-SENSITIVE, so assert the exact suffix the client tests for.
    expect(
      item.Path,
      "Path absent on the UNFIELDED fetch → every reader declines silently, which is " +
        "exactly what Fields-gating Path for books caused",
    ).toBeTruthy();
    expect(item.Path.endsWith("epub")).toBe(true);

    // SC-004 — nothing for a client to try to stream.
    expect(item.MediaSources ?? []).toHaveLength(0);
    expect(item.RunTimeTicks ?? 0).toBe(0);
  });

  // Gate 3, again through the client's own request path so `api_key` query auth
  // is exercised exactly as `getItemDownloadUrl` builds it.
  test("the client can fetch the bytes and they are a valid epub", async ({ page }) => {
    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);

    const result = await page.evaluate(async (id) => {
      const api = (window as any).ApiClient;
      const url = api.getItemDownloadUrl(id);
      const resp = await fetch(url);
      const buf = await resp.arrayBuffer();
      const head = new Uint8Array(buf.slice(0, 4));
      return {
        url,
        status: resp.status,
        contentType: resp.headers.get("content-type"),
        length: buf.byteLength,
        // Every zip — and so every epub — starts "PK\x03\x04".
        isZip: head[0] === 0x50 && head[1] === 0x4b && head[2] === 0x03 && head[3] === 0x04,
      };
    }, BOOK_WIRE_ID);

    // The URL the reader builds carries the token in the QUERY and no header.
    expect(result.url).toContain("/Download");
    expect(result.url).toContain("api_key=");
    expect(result.status, `GET ${result.url} failed`).toBe(200);
    expect(result.contentType).toBe("application/epub+zip");
    expect(result.length).toBeGreaterThan(0);
    expect(result.isZip, "epub.js cannot unzip what is not a zip").toBe(true);
  });

  // The actual acceptance test: the reader mounts, renders chapter one, and a
  // page turn reaches chapter two. Nothing below reads pharos directly — if this
  // passes, a human opening the book sees a book.
  // The acceptance test (SC-001): the reader mounts, renders, and a page turns.
  test("bookPlayer opens the epub and turns a page", async ({ page }) => {
    test.setTimeout(150_000);

    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    const sid = await serverId(page);

    // The details page + Play button is the journey that was BROKEN: its item
    // fetch sends no Fields, so a Fields-gated Path left canPlayItem false and
    // this click did nothing at all — no iframe, no /Download request, no error.
    await page.goto(`/#/details?id=${BOOK_WIRE_ID}&serverId=${sid}`);
    const playBtn = page.locator("button.btnPlay").first();
    await playBtn.waitFor({ timeout: 20_000 });
    await playBtn.click({ force: true });

    // epub.js paints the chapter into a sandboxed `about:srcdoc` iframe, so the
    // assertion goes through the FRAME, not the page DOM. Its presence is what
    // proves the reader mounted rather than that the click was merely accepted.
    const chapterOne = page.frameLocator("iframe").first().locator("#ch1-marker");
    await chapterOne.waitFor({ timeout: 60_000 });
    await expect(chapterOne).toHaveText("PHAROS_CHAPTER_ONE");

    // TURN THE PAGE. The fixture has two chapters precisely so this is
    // observable — SC-001 asks for a page turn, not for a book that renders.
    //
    // epub.js paginates WITHIN a section before moving to the next one, and the
    // page count depends on the viewport, so "one keypress = next chapter" is not
    // a safe assumption. Turn until chapter two appears, bounded, which is what a
    // reader does anyway. The reader is clicked first so the keypress has focus —
    // without it the event goes to the document and nothing moves.
    await page.locator(".bookPlayerContainer, #bookPlayer, .epub-container").first().click({
      position: { x: 5, y: 5 },
      force: true,
    });

    const chapterTwo = page.frameLocator("iframe").first().locator("#ch2-marker");
    let turned = false;
    for (let i = 0; i < 12 && !turned; i++) {
      await page.keyboard.press("ArrowRight");
      await page.waitForTimeout(1200);
      turned = (await chapterTwo.count()) > 0;
    }
    expect(turned, "12 page turns never reached chapter two").toBe(true);
    await expect(chapterTwo).toHaveText("PHAROS_CHAPTER_TWO");
  });

  // Bytes must arrive by the URL the reader itself builds, during a real open.
  test("opening the book fetches its bytes from /Download", async ({ page }) => {
    test.setTimeout(150_000);
    const downloads: string[] = [];
    page.on("request", (r) => {
      const p = new URL(r.url()).pathname;
      if (/\/Download$/i.test(p)) downloads.push(new URL(r.url()).pathname + new URL(r.url()).search);
    });

    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    const sid = await serverId(page);
    await page.goto(`/#/details?id=${BOOK_WIRE_ID}&serverId=${sid}`);
    const playBtn = page.locator("button.btnPlay").first();
    await playBtn.waitFor({ timeout: 20_000 });
    await playBtn.click({ force: true });
    await page.frameLocator("iframe").first().locator("#ch1-marker").waitFor({ timeout: 60_000 });

    expect(downloads.length, "the reader must have fetched the epub").toBeGreaterThan(0);
    // Query auth, no Authorization header — the shape getItemDownloadUrl builds.
    expect(downloads[0]).toContain("api_key=");
  });

  // The negative case that the whole design turns on. Recorded as a test so the
  // failure MODE is documented executably: if a future change drops `Path`, this
  // is what the symptom looks like — no request, no error, nothing.
  test("removing Path from the client's view makes the reader decline silently", async ({
    page,
  }) => {
    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);

    const verdicts = await page.evaluate(async (id) => {
      const api = (window as any).ApiClient;
      const item = await api.getItem(api.getCurrentUserId(), id);
      const withoutPath = { ...item, Path: undefined };

      // Ask the REAL registered players, not a reimplementation of their logic.
      const pm = (window as any).playbackManager;
      if (!pm || typeof pm.getPlayers !== "function") return null;
      const players = pm
        .getPlayers()
        .filter((p: any) => typeof p.canPlayMediaType === "function");
      const readers = players.filter((p: any) => p.canPlayMediaType("Book"));
      return {
        readerCount: readers.length,
        withPath: readers.some((p: any) => !p.canPlayItem || p.canPlayItem(item)),
        withoutPath: readers.some(
          (p: any) => !p.canPlayItem || p.canPlayItem(withoutPath),
        ),
      };
    }, BOOK_ID);

    // playbackManager is not always reachable on `window`; skip rather than
    // assert a false pass.
    test.skip(verdicts === null, "playbackManager not exposed on window");

    expect(verdicts!.readerCount, "the bundle must ship a book reader").toBeGreaterThan(0);
    expect(verdicts!.withPath, "with Path, a reader accepts the item").toBe(true);
    expect(
      verdicts!.withoutPath,
      "without Path every reader declines — silently, which is why Path is asserted server-side too",
    ).toBe(false);
  });
});
