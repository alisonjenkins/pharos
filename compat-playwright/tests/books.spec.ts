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
/// The seeded `.cbz` (id 10) — two PNG pages of DIFFERENT sizes plus a
/// ComicInfo.xml. Read by `comicsPlayer`, a different reader with a different
/// unpacker (libarchive.js) behind the same `MediaType: "Book"` gate.
/// HEX, not decimal — wire ids are `{id:032x}`, so internal id 10 is `…000a`.
const COMIC_WIRE_ID = "0000000000000000000000000000000a";
/// The seeded `.pdf` (id 11 → `…000b`). Read by `pdfPlayer`, the third reader
/// behind the same `Book` gate and the only one that rasterises into a canvas.
const PDF_WIRE_ID = "0000000000000000000000000000000b";

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

  // T057 — the SECOND reader. `comicsPlayer` sits behind the same
  // `MediaType: "Book"` gate as `bookPlayer` but tests a different extension
  // set (`.cbr`/`.cbt`/`.cbz`/`.cb7`, case-sensitive) and unpacks with
  // libarchive.js instead of epub.js. Nothing about the epub passing implies
  // this one does: playbackManager picks between the two readers purely on
  // `canPlayItem`, i.e. purely on `Path`.
  test("comicsPlayer opens the cbz and turns a page", async ({ page }) => {
    test.setTimeout(150_000);

    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    const sid = await serverId(page);

    // Gate check first, so a failure below localises: wrong MediaType/Path is a
    // different bug from libarchive failing to unpack.
    const item = await page.evaluate(async (id) => {
      const api = (window as any).ApiClient;
      return await api.getItem(api.getCurrentUserId(), id);
    }, COMIC_WIRE_ID);
    expect(item.Type).toBe("Book");
    expect(String(item.MediaType).toLowerCase()).toBe("book");
    expect(item.Path, "no Path means every reader declines silently").toBeTruthy();
    expect(
      item.Path.endsWith(".cbz"),
      `comicsPlayer's compare is case-sensitive and includes the dot, got ${item.Path}`,
    ).toBe(true);

    await page.goto(`/#/details?id=${COMIC_WIRE_ID}&serverId=${sid}`);
    const playBtn = page.locator("button.btnPlay").first();
    await playBtn.waitFor({ timeout: 20_000 });
    await playBtn.click({ force: true });

    // comicsPlayer builds `#comicsPlayer` and fills a Swiper with one
    // `.swiper-slide` per image, each holding an `img.swiper-slide-img` whose
    // src is a blob URL of the unpacked page.
    const reader = page.locator("#comicsPlayer");
    await reader.waitFor({ timeout: 60_000 });
    const slides = page.locator("#comicsPlayer .swiper-slide");
    await expect(slides).toHaveCount(2, { timeout: 60_000 });

    // The images must actually DECODE. A slide exists whether or not its blob
    // is a real image, so an index-only assertion would pass on two broken
    // pages — which is precisely the silent-failure shape this spec exists for.
    const dims = async () =>
      await page.evaluate(() =>
        Array.from(
          document.querySelectorAll<HTMLImageElement>("#comicsPlayer img.swiper-slide-img"),
        ).map((i) => [i.naturalWidth, i.naturalHeight]),
      );
    await expect
      .poll(async () => (await dims()).filter(([w]) => w > 0).length, { timeout: 60_000 })
      .toBe(2);
    expect(
      await dims(),
      "the fixture's pages are 64x64 and 128x32; equal sizes here would mean the " +
        "two slides are showing the same image",
    ).toEqual([
      [64, 64],
      [128, 32],
    ]);

    // TURN THE PAGE. Swiper's own `activeIndex` is the reader's notion of which
    // page is showing, so it is read rather than inferred from the DOM.
    const activeIndex = async () =>
      await page.evaluate(() => {
        const el = document.querySelector(".slideshowSwiperContainer") as any;
        return el?.swiper?.activeIndex ?? -1;
      });
    expect(await activeIndex()).toBe(0);

    await reader.click({ position: { x: 5, y: 5 }, force: true });
    let turned = false;
    for (let i = 0; i < 8 && !turned; i++) {
      await page.keyboard.press("ArrowRight");
      await page.waitForTimeout(600);
      turned = (await activeIndex()) === 1;
    }
    expect(turned, "8 page turns never reached page two").toBe(true);
  });

  // T074 — the THIRD reader. `pdfPlayer` is pdf.js, and unlike the other two
  // it RASTERISES: it renders page one into a `<canvas>`. So the assertion is
  // that the canvas has non-zero pixels drawn into it, not merely that a
  // canvas element appeared — an empty canvas is what a failed render leaves
  // behind, and it looks identical to a successful one in the DOM.
  test("pdfPlayer opens the pdf and renders page one", async ({ page }) => {
    test.setTimeout(150_000);

    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    const sid = await serverId(page);

    const item = await page.evaluate(async (id) => {
      const api = (window as any).ApiClient;
      return await api.getItem(api.getCurrentUserId(), id);
    }, PDF_WIRE_ID);
    expect(item.Type).toBe("Book");
    expect(String(item.MediaType).toLowerCase()).toBe("book");
    expect(item.Path, "no Path means every reader declines silently").toBeTruthy();
    // pdfPlayer lowercases before comparing, unlike bookPlayer — asserted as
    // pdfPlayer actually spells it rather than as the other reader does.
    expect(String(item.Path).toLowerCase().endsWith("pdf")).toBe(true);

    await page.goto(`/#/details?id=${PDF_WIRE_ID}&serverId=${sid}`);
    const playBtn = page.locator("button.btnPlay").first();
    await playBtn.waitFor({ timeout: 20_000 });
    await playBtn.click({ force: true });

    await page.locator("#pdfPlayer").waitFor({ timeout: 60_000 });
    const canvas = page.locator("#pdfPlayer canvas#canvas");
    await canvas.waitFor({ timeout: 60_000 });

    // pdf.js sizes the canvas from the page's MediaBox before it draws, so a
    // non-zero size proves the DOCUMENT parsed — the xref table in the
    // hand-assembled fixture is right.
    const size = await canvas.evaluate((c: HTMLCanvasElement) => [c.width, c.height]);
    expect(size[0], "a zero-width canvas means the PDF never parsed").toBeGreaterThan(0);
    expect(size[1]).toBeGreaterThan(0);

    // And something was actually PAINTED. The fixture draws text, because a
    // blank page renders to a blank canvas and a blank canvas is
    // indistinguishable from a render that silently failed.
    await expect
      .poll(
        async () =>
          await canvas.evaluate((c: HTMLCanvasElement) => {
            const ctx = c.getContext("2d");
            if (!ctx) return -1;
            const { data } = ctx.getImageData(0, 0, c.width, c.height);
            // Count pixels that are neither transparent nor pure white.
            let painted = 0;
            for (let i = 0; i < data.length; i += 4) {
              if (data[i + 3] !== 0 && !(data[i] === 255 && data[i + 1] === 255 && data[i + 2] === 255)) {
                painted++;
              }
            }
            return painted;
          }),
        { timeout: 60_000 },
      )
      .toBeGreaterThan(0);
  });

  // T077 — the read position survives, and the UI puts the book where a book
  // goes.
  //
  // Two different outcomes hide behind "a book has no runtime": the client can
  // present it as a book (right) or try to draw it as a part-watched video
  // (wrong — a full or NaN-width bar reads as corrupted state rather than as an
  // absent duration). Only one is acceptable and they are indistinguishable
  // from the server, so this is observed in the DOM.
  //
  // jellyfin-web has a "Continue Reading" row distinct from "Continue
  // Watching", and which one an item lands in is decided by what PHAROS says it
  // is. The movie is the counterweight: without it, a page that rendered no
  // resume rows at all would report "the book is not in Continue Watching" and
  // pass while proving nothing.
  test("a part-read book resumes into Continue Reading, not Continue Watching", async ({
    page,
  }) => {
    test.setTimeout(150_000);

    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);

    // Report a position for the book AND for a movie, through the client's own
    // API so the request shape is the one a reader actually sends.
    const MOVIE_WIRE_ID = "00000000000000000000000000000001";
    await page.evaluate(
      async ([bookId, movieId]) => {
        const api = (window as any).ApiClient;
        const report = (id: string, ticks: number) =>
          api.ajax({
            type: "POST",
            url: api.getUrl("Sessions/Playing/Progress"),
            data: JSON.stringify({
              ItemId: id,
              PlaySessionId: `spec-${id}`,
              PositionTicks: ticks,
              IsPaused: true,
            }),
            contentType: "application/json",
          });
        await report(bookId, 1_234_000);
        // ~2.5s into the ~5s fixture clip: unambiguously part-watched.
        await report(movieId, 25_000_000);
      },
      [BOOK_WIRE_ID, MOVIE_WIRE_ID],
    );

    // The position came back — this is what the reader reads on reopen.
    const item = await page.evaluate(async (id) => {
      const api = (window as any).ApiClient;
      return await api.getItem(api.getCurrentUserId(), id);
    }, BOOK_WIRE_ID);
    expect(item.UserData.PlaybackPositionTicks).toBe(1_234_000);
    expect(item.RunTimeTicks, "a book still has no time axis").toBe(0);
    expect(
      item.UserData.PlayedPercentage,
      "must be a number: a null percentage is what a NaN serialises to, and a " +
        "client draws that as a full or broken bar",
    ).toBe(0);

    // Now LOOK at it.
    await page.reload({ waitUntil: "networkidle" });
    const sections = async () =>
      await page.evaluate(() => {
        const out: Record<string, { ids: string[]; widths: string[] }> = {};
        for (const h of Array.from(document.querySelectorAll("h2"))) {
          const sec = h.closest(".verticalSection") ?? h.parentElement;
          out[(h.textContent ?? "").trim()] = {
            ids: Array.from(
              new Set(
                Array.from(sec?.querySelectorAll<HTMLElement>("[data-id]") ?? []).map(
                  (e) => e.dataset.id ?? "",
                ),
              ),
            ),
            widths: Array.from(
              sec?.querySelectorAll<HTMLElement>("[class*=itemProgressBarForeground]") ?? [],
            ).map((e) => e.style.width),
          };
        }
        return out;
      });

    await expect
      .poll(async () => (await sections())["Continue Watching"]?.ids ?? [], { timeout: 30_000 })
      .toContain(MOVIE_WIRE_ID);

    const rows = await sections();
    expect(
      rows["Continue Reading"]?.ids ?? [],
      "a part-read book belongs in Continue Reading — which it only reaches if " +
        "pharos reported it as a Book with a resume position",
    ).toContain(BOOK_WIRE_ID);
    expect(
      rows["Continue Watching"]?.ids ?? [],
      "and NOT in Continue Watching, where it would be presented as a video",
    ).not.toContain(BOOK_WIRE_ID);

    // Whatever bars the row does draw, none of the book's may be NaN-width or
    // claim the book is fully read.
    for (const w of rows["Continue Reading"]?.widths ?? []) {
      expect(w, `a book's progress width must not be NaN, got ${w}`).not.toContain("NaN");
      expect(parseFloat(w) || 0, `a book must not render as fully read, got ${w}`).toBeLessThan(1);
    }
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
