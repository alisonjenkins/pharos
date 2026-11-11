import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { tvBridgeInitScript } from "../lib/tv-bridge";

// B1332 / V1303: the TV app with its remote, off the TV. Each case walks a
// fix that once came back after it was made, with the TV's own remote
// script (`BRIDGE_JS`, read from android_page.rs by ../lib/tv-bridge.ts)
// installed in the page at the TV's CSS viewport and user agent, and only
// the keys a remote has: the arrows, OK (Enter) and Back.
//
// What this cannot reach: code behind `cfg!(target_os = "android")` (the
// remote scrub's own steps, the banner's Search button, the palette handing
// Down and Up to the D-pad so OK picks a result, B1331) and the WebView
// itself (its on-screen keyboard, config changes). Those are on the on-TV
// checklist, docs/tv-smoke-checklist.md. Nor does a case belong here when
// the web build passes it with the fix reverted: Back to a filtered library
// (B1276) failed in the TV's Rust route parse, which the web build never
// runs on Back, so pharos-ui's route tests guard it (V1239).

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

// The TCL Google TV's WebView, whose CSS viewport is 960 x 540.
test.use({
  viewport: { width: 960, height: 540 },
  userAgent:
    "Mozilla/5.0 (Linux; Android 12; Smart TV Pro Build/STT2.221228.001; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/156.0.8078.47 Mobile Safari/537.36 GoogleTV",
});

type Api = {
  user: { Id: string };
  json: (path: string) => Promise<any>;
  post: (path: string, data: unknown) => Promise<void>;
};

const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
const nativeId = (wireId: string) => BigInt(`0x${wireId}`).toString();

async function signIn(page: Page, request: APIRequestContext): Promise<Api> {
  const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
    headers: { Accept: "application/json" },
  });
  test.skip(
    probe.status() !== 405,
    `native API is not enabled (status ${probe.status()}); skipping native test`,
  );
  const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
    headers: {
      "Content-Type": "application/json",
      "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-tv-remote-${Date.now()}", Version="0"`,
    },
    data: { Username: TEST_USER, Pw: TEST_PASS },
  });
  // With the native API on, the seed is the suite's own (`just
  // compat-dioxus-full`): a guard that skipped on a missing seed would pass
  // CI without running, so a missing seed fails instead.
  expect(auth.ok(), `the seeded test user ${TEST_USER} signs in`).toBe(true);
  const { AccessToken: token, User: user } = await auth.json();
  const json = async (path: string) =>
    (await request.get(`${PHAROS_URL}${path}`, { headers: { "X-Emby-Token": token } })).json();
  const post = async (path: string, data: unknown) => {
    const r = await request.post(`${PHAROS_URL}${path}`, { headers: { "X-Emby-Token": token }, data });
    expect(r.ok(), `POST ${path}: ${r.status()}`).toBe(true);
  };

  const problems: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error" && m.text().startsWith("tv-bridge:")) problems.push(m.text());
  });
  await page.addInitScript(tvBridgeInitScript());
  await page.goto("/ui/");
  await page.locator("#signin-name").fill(TEST_USER);
  await page.locator("#signin-password").fill(TEST_PASS);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
  await expect(page.locator("html")).toHaveAttribute("data-device", "tv");
  // The remote script is in: it told "Rust" the viewport, as on the TV.
  await expect
    .poll(() => page.evaluate(() => (window as any).__tvBridgeSent?.some((m: any) => m.t === "size")))
    .toBe(true);
  expect(problems, problems.join("\n")).toEqual([]);
  return { user, json, post };
}

/** What has the focus: its id, tag, classes and text, for a readable failure. */
const focused = (page: Page) =>
  page.evaluate(() => {
    const at = document.activeElement as HTMLElement | null;
    if (!at || at === document.body) return "(nothing)";
    return `${at.tagName.toLowerCase()}#${at.id}.${[...at.classList].join(".")} "${(at.textContent ?? "").trim().slice(0, 40)}"`;
  });
const focusMatches = (page: Page, selector: string) =>
  page.evaluate((s) => !!document.activeElement?.matches(s), selector);

/** Press `key` until the focus matches `selector`, at most `max` times. */
async function walkTo(page: Page, key: string, selector: string, max = 8): Promise<void> {
  for (let i = 0; i < max && !(await focusMatches(page, selector)); i++) {
    await page.keyboard.press(key);
  }
  expect(await focusMatches(page, selector), `${key} never reached ${selector}; at ${await focused(page)}`).toBe(
    true,
  );
}

/** A Bulk film (a few seconds long), its native id. */
async function aFilm(api: Api): Promise<string> {
  return (await aFilmIds(api)).native;
}

/** A Bulk film (a few seconds long), its wire and native ids. */
async function aFilmIds(api: Api): Promise<{ wire: string; native: string }> {
  const views = await api.json(`/Users/${api.user.Id}/Views`);
  const bulk = views.Items?.find((i: { Name: string }) => i.Name === "Bulk");
  expect(bulk, "the seed has a Bulk library").toBeTruthy();
  let film: { Id: string } | undefined;
  await expect
    .poll(
      async () => {
        film = (await api.json(`/Users/${api.user.Id}/Items?ParentId=${bulk.Id}&Recursive=true&IncludeItemTypes=Movie&Limit=1`))
          .Items?.[0];
        return !!film;
      },
      { timeout: 60_000 },
    )
    .toBe(true);
  return { wire: film!.Id, native: nativeId(film!.Id) };
}

/** Open a film's player, paused, with the remote's focus on the picture. */
async function openPlayer(page: Page, api: Api): Promise<void> {
  await page.goto(`/ui/watch/${await aFilm(api)}`);
  const player = page.locator("#pharos-player");
  await expect(page.locator(".pharos-controls")).toBeAttached({ timeout: 30_000 });
  await expect
    .poll(() => page.evaluate(() => (document.getElementById("pharos-video") as HTMLVideoElement)?.duration || 0), {
      timeout: 30_000,
    })
    .toBeGreaterThan(0);
  await page.evaluate(() => (document.getElementById("pharos-video") as HTMLVideoElement).pause());
  await player.focus();
}

test.describe("Android TV remote (B1332)", () => {
  // B1330 / V1300: the seek bar is a D-pad stop. Up from the control row
  // lands on it; Left and Right on it scrub without moving the focus off
  // it; Down goes back to the controls. #1219 had made it tabindex -1 on the
  // TV, and the remote script stepped only between buttons, selects and
  // links, so Up skipped it and no remote could seek from the bar.
  test("Up from the control row reaches the seek bar, and Left and Right scrub on it (B1330)", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const api = await signIn(page, request);
    await openPlayer(page, api);

    await page.keyboard.press("ArrowDown");
    expect(await focusMatches(page, ".pharos-control-row button"), `Down from the picture: ${await focused(page)}`).toBe(
      true,
    );
    await page.keyboard.press("ArrowUp");
    expect(await focusMatches(page, "#pharos-seek"), `Up from the controls: ${await focused(page)}`).toBe(true);

    const seek = page.locator("#pharos-seek");
    const before = Number(await seek.getAttribute("aria-valuenow"));
    await page.keyboard.press("ArrowRight");
    await expect
      .poll(async () => Number(await seek.getAttribute("aria-valuenow")), { message: "Right on the bar scrubs" })
      .toBeGreaterThan(before);
    expect(await focusMatches(page, "#pharos-seek"), `after Right: ${await focused(page)}`).toBe(true);
    await page.keyboard.press("ArrowLeft");
    expect(await focusMatches(page, "#pharos-seek"), `after Left: ${await focused(page)}`).toBe(true);

    // Down goes back to the control row: the bar is a stop on the way, not
    // a trap.
    await page.keyboard.press("ArrowDown");
    expect(await focusMatches(page, ".pharos-control-row button"), `Down from the bar: ${await focused(page)}`).toBe(
      true,
    );
  });

  // B1277 / V1240: the trick-play preview stays inside the seek bar at
  // either end, so at a film's start or end it is whole on the TV's screen.
  test("the seek preview stays inside the bar at both ends (B1277)", async ({ page, request }) => {
    test.setTimeout(120_000);
    const api = await signIn(page, request);
    await openPlayer(page, api);
    const seek = page.locator("#pharos-seek");
    const preview = page.locator(".pharos-seek-preview");
    const bar = (await seek.boundingBox())!;
    expect(bar.x, "the bar keeps to the 48px title-safe inset").toBeGreaterThanOrEqual(47.5);
    expect(bar.x + bar.width).toBeLessThanOrEqual(960 - 47.5);
    for (const [end, x] of [
      ["start", bar.x + 1],
      ["end", bar.x + bar.width - 1],
    ] as const) {
      await page.mouse.move(x, bar.y + bar.height / 2);
      await expect(preview, `a preview at the ${end}`).toBeVisible();
      const box = (await preview.boundingBox())!;
      expect(box.x, `the preview's left edge at the ${end}`).toBeGreaterThanOrEqual(bar.x - 0.5);
      expect(box.x + box.width, `the preview's right edge at the ${end}`).toBeLessThanOrEqual(
        bar.x + bar.width + 0.5,
      );
    }
  });

  // B1322 / V1293: on an artist page the remote reaches an album's tile and
  // OK opens the album. The card's Play next and Add to queue buttons sat
  // inside the tile's corner and scored nearer under the remote's nearest-
  // control rule, so Down landed on one and OK queued the album instead.
  test("Down reaches an album tile and OK opens the album, not the queue (B1322)", async ({
    page,
    request,
  }) => {
    test.setTimeout(90_000);
    const api = await signIn(page, request);
    type Song = { Album?: string; AlbumArtist?: string; Artists?: string[] };
    const by = (t: Song) => t.AlbumArtist ?? t.Artists?.[0];
    const tracks = await api.json(
      `/Users/${api.user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=AlbumArtist,Artists,Album`,
    );
    const track: Song | undefined = tracks.Items?.find((t: Song) => t.Album && by(t));
    expect(track, "the seed has a track tagged with an album and artist").toBeTruthy();
    const artist = (await api.json(`/Artists`)).Items?.find((a: { Name: string }) => a.Name === by(track!));
    expect(artist, "the track's album artist is listed").toBeTruthy();

    await page.goto(`/ui/browse/${nativeSynth(artist.Id)}`);
    await expect(page.locator("section.pharos-artist h2")).toHaveText(artist.Name, { timeout: 15_000 });
    await expect(page.locator("section.pharos-artist ul.pharos-grid li").first()).toBeVisible({
      timeout: 15_000,
    });
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    const inGrid = "section.pharos-artist ul.pharos-grid li *";
    await walkTo(page, "ArrowDown", inGrid);
    expect(
      await focusMatches(page, "a[href]"),
      `the first stop in the album grid is the tile's link, not a card button: ${await focused(page)}`,
    ).toBe(true);
    await page.keyboard.press("Enter");
    await expect(page.locator("section.pharos-album h2"), "OK opened the album").toBeVisible({
      timeout: 15_000,
    });
  });

  // B1340 / V1311: a Continue Watching tile draws no info control (the
  // remote's arrows never reached one inside the tile). OK on the tile opens
  // its title page; OK held plays it, through the tile's undrawn play link.
  test("OK on a Continue Watching tile opens its title and a held OK plays it (B1340)", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const api = await signIn(page, request);
    const { wire, native: film } = await aFilmIds(api);
    await api.post(`/Sessions/Playing/Progress`, {
      ItemId: wire,
      PlaySessionId: `pw-tile-hold-${Date.now()}`,
      PositionTicks: 25_000_000,
      IsPaused: true,
    });
    const tile = page.locator(`section.pharos-rail a.pharos-poster[href$="/ui/title/${film}"]`).first();
    const home = async () => {
      await page.goto("/ui/");
      await expect(tile, "the film is in Continue watching").toBeAttached({ timeout: 30_000 });
      await page.keyboard.press("ArrowDown");
      await tile.focus();
    };

    await home();
    expect(await page.locator(".pharos-poster-info").count(), "no info control").toBe(0);
    await page.keyboard.press("Enter");
    await expect(page, "OK opened the title page").toHaveURL(new RegExp(`/ui/title/${film}$`), { timeout: 15_000 });

    await home();
    // Hold OK until the hold plays, then let go: no fixed wait.
    await page.keyboard.down("Enter");
    await expect(page, "a held OK played").toHaveURL(new RegExp(`/ui/watch/${film}$`), { timeout: 15_000 });
    await page.keyboard.up("Enter");
  });
});
