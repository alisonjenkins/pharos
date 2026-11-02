import { test, expect } from "@playwright/test";

// T51 phase 3 — drive the pharos Dioxus UI (served at `/ui/` by
// `[server].ui_dir`) end-to-end through a real Chromium: the native app
// at `/ui/` and legacy management panes at `/ui/manage`.
//
// The native sign-in tests need `[server.native_api].enabled = true`, the
// test origin in `allowed_origins`, and https; with the API off they skip.
//
// Skips cleanly when `/ui/` is not the UI — useful in environments where the
// WASM bundle hasn't been built. Build with:
//   nix develop --command dx build --package pharos-ui --release
// then point `[server].ui_dir` at `target/dx/pharos-ui/release/web/public`.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

test.describe("pharos Dioxus UI", () => {
  test.beforeAll(async ({ request }) => {
    const probe = await request.get(`${PHAROS_URL}/ui/`, {
      headers: { Accept: "text/html" },
    });
    test.skip(
      !(probe.headers()["content-type"] ?? "").includes("text/html"),
      "pharos has no [server].ui_dir configured — build the bundle and re-run",
    );
  });

  test("legacy panes at /ui/manage: login form renders, sign-in button present", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
    page.on("requestfailed", (r) => {
      // Ignore favicon misses + cancellations.
      if (!r.url().includes("favicon")) {
        errors.push(`requestfailed: ${r.url()} — ${r.failure()?.errorText}`);
      }
    });

    await page.goto("/ui/manage");

    // App shell renders.
    await expect(page.locator("header.pharos-banner h1")).toHaveText("pharos");

    // Login form lands without an authed user.
    await expect(page.locator("form")).toBeVisible();
    await expect(page.locator('input[type="text"]')).toBeVisible();
    await expect(page.locator('input[type="password"]')).toBeVisible();
    await expect(page.getByRole("button", { name: "Sign in" })).toBeVisible();

    // Pre-login chrome surfaces the active server URL.
    await expect(page.locator(".pharos-active-server")).toContainText(
      "Connected to:",
    );

    // No console errors so far.
    expect(errors, errors.join("\n")).toEqual([]);
  });

  test("legacy panes at /ui/manage: server-picker toggle reveals the picker form", async ({ page }) => {
    await page.goto("/ui/manage");
    await page.locator(".pharos-switch-server").click();
    await expect(page.locator(".pharos-server-picker")).toBeVisible();
    // Either an empty-state or the saved-server list renders.
    const empty = page.locator(".pharos-server-picker-empty");
    const list = page.locator(".pharos-server-picker-list");
    await expect(empty.or(list)).toBeVisible();
    // Manual-add input pre-fills with the current origin.
    await expect(
      page.locator(".pharos-server-picker-add-input"),
    ).toHaveValue(new RegExp(PHAROS_URL.replace(/^https?:\/\//, "")));
  });

  test("legacy panes at /ui/manage: login with seeded creds reaches the library nav", async ({ page }) => {
    const auth = await page.request.post(
      `${PHAROS_URL}/Users/AuthenticateByName`,
      {
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      },
    );
    test.skip(
      !auth.ok(),
      `pharos has no seeded test user (${TEST_USER}); skipping login flow`,
    );

    await page.goto("/ui/manage");
    await page.locator('input[type="text"]').fill(TEST_USER);
    await page.locator('input[type="password"]').fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();

    // Authenticated chrome lights up — nav buttons rendered.
    await expect(page.locator(".pharos-nav-library")).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.locator(".pharos-nav-search")).toBeVisible();
    await expect(page.locator(".pharos-nav-livetv")).toBeVisible();
    await expect(page.locator(".pharos-nav-prefs")).toBeVisible();
    await expect(page.locator(".pharos-nav-remote")).toBeVisible();
  });

  test.describe("native app at /ui/", () => {
    test("root shows the native app's state", async ({ page }) => {
      const errors: string[] = [];
      page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));

      await page.goto("/ui/");

      await expect(page.locator("header.pharos-banner h1")).toHaveText("pharos");

      const signin = page.locator("form#signin");
      const notEnabled = page.locator("section.pharos-status", {
        hasText: "This interface is not enabled",
      });
      await expect(signin.or(notEnabled)).toBeVisible({ timeout: 10_000 });

      expect(errors, errors.join("\n")).toEqual([]);
    });

    test("the stored theme is applied, and an unknown one falls back saying why", async ({
      page,
    }) => {
      const lines: string[] = [];
      page.on("console", (m) => {
        if (m.text().startsWith("pharos: theme")) lines.push(m.text());
      });
      const rootToken = (name: string) =>
        page.evaluate(
          (n) => getComputedStyle(document.documentElement).getPropertyValue(n).trim(),
          name,
        );

      await page.addInitScript(() => {
        if (!sessionStorage.getItem("theme-step")) {
          sessionStorage.setItem("theme-step", "1");
          localStorage.setItem("pharos.theme.active", "Neon Grid");
        } else {
          localStorage.setItem("pharos.theme.active", "Glitter");
        }
      });

      await page.goto("/ui/");
      await expect.poll(() => rootToken("--px-color-bg")).toBe("#000000");
      await expect
        .poll(() => lines.join("\n"))
        .toMatch(/pharos: theme applied name="Neon Grid" source=shipped/);

      lines.length = 0;
      await page.reload();
      await expect.poll(() => rootToken("--px-color-bg")).toBe("#0e1116");
      await expect
        .poll(() => lines.join("\n"))
        .toMatch(
          /pharos: theme fallback name=Glitter source=shipped reason=unknown_theme .*to="Pharos Night"/,
        );
    });

    test("not-enabled page links to Manage", async ({ page, request }) => {
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 404,
        `native API is on (status ${probe.status()}); skipping not-enabled test`,
      );

      await page.goto("/ui/");
      await page.getByRole("button", { name: "Open Manage" }).click();
      await expect(page).toHaveURL(/\/ui\/manage$/);
      await expect(page.locator("form")).toBeVisible();
      await expect(page.locator('input[type="password"]')).toBeVisible();
    });

    test("native sign-in reaches home", async ({ page, request }) => {
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

      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });
      await expect(
        page.locator(".pharos-shell-nav").getByRole("link", { name: "Manage" }),
      ).toBeVisible();

      await page.getByRole("button", { name: "Sign out" }).click();
      await expect(page.locator("form#signin")).toBeVisible();
    });

    test("a wrong password says so", async ({ page, request }) => {
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 405,
        `native API is not enabled (status ${probe.status()}); skipping native test`,
      );

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill("definitely-wrong");
      await page.getByRole("button", { name: "Sign in" }).click();

      await expect(page.locator('[role="alert"]')).toContainText(
        /Wrong name or password|Too many failed attempts/,
      );
    });

    // Library views, series and seasons are synthesised ids, which the server
    // once refused to describe; the browse screen fetched that description
    // first, so every one of them showed "notFound: item not found".
    test("browse opens a library from home, then a series and its season", async ({
      page,
      request,
    }) => {
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-browse-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      const json = async (path: string) =>
        (await request.get(`${PHAROS_URL}${path}`, { headers: api })).json();
      // A synthesised wire id is the same 63-bit hash in both halves; the
      // native route spells it `s` and that number in decimal.
      const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
      // The seed registers a three-episode show (the startup rescan names it
      // from the file); its episodes carry the synthesised series and season
      // ids.
      const episodes = await json(
        `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Episode&Fields=SeriesId,SeasonId`,
      );
      const episode = episodes.Items?.find(
        (i: { SeriesId?: string; SeasonId?: string }) => i.SeriesId && i.SeasonId,
      );
      expect(episode, "the seed has an episode with a series and season").toBeTruthy();

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });

      await page.locator("section.pharos-rail h2 a").first().click();
      await expect(page).toHaveURL(/\/ui\/browse\/s\d+$/);
      await expect(page.locator("section.pharos-browse h2")).toBeVisible({
        timeout: 15_000,
      });
      await expect(page.locator("ul.pharos-grid li").first()).toBeVisible();
      await expect(page.locator("p.pharos-error")).toHaveCount(0);

      // A series is the show page: its name over the season's episodes.
      await page.goto(`/ui/browse/${nativeSynth(episode.SeriesId)}`);
      await expect(page.locator("section.pharos-show h2")).toHaveText(episode.SeriesName, {
        timeout: 15_000,
      });
      await expect(page.locator("ol.pharos-episodes li").first()).toBeVisible();
      await expect(page.locator("p.pharos-error")).toHaveCount(0);

      // A season opens the same page with that season shown.
      await page.goto(`/ui/browse/${nativeSynth(episode.SeasonId)}`);
      await expect(page.locator("section.pharos-show h2")).toHaveText(episode.SeriesName, {
        timeout: 15_000,
      });
      await expect(
        page.locator("section.pharos-show").getByText(/^Season 1$/).first(),
      ).toBeVisible();
      await expect(page.locator("ol.pharos-episodes li").first()).toBeVisible();
      await expect(page.locator("p.pharos-error")).toHaveCount(0);

      // An episode row plays it.
      await page.locator("ol.pharos-episodes li a.pharos-episode-play").first().click();
      await expect(page).toHaveURL(/\/ui\/watch\/\d+$/);
    });

    // 012 US5: artist, then album, then track list.
    test("browse walks an artist to an album and its tracks", async ({ page, request }) => {
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-music-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      const json = async (path: string) =>
        (await request.get(`${PHAROS_URL}${path}`, { headers: api })).json();
      const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
      // An artist with an album: the first tagged track names both.
      const tracks = await json(
        `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=AlbumArtist,Album`,
      );
      const track = tracks.Items?.find(
        (t: { Album?: string; AlbumArtist?: string }) => t.Album && t.AlbumArtist,
      );
      test.skip(!track, "the seed has no track tagged with an album and artist");
      const artists = await json(`/Artists`);
      const artist = artists.Items?.find((a: { Name: string }) => a.Name === track.AlbumArtist);
      expect(artist, "the track's album artist is listed").toBeTruthy();

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });

      await page.goto(`/ui/browse/${nativeSynth(artist.Id)}`);
      await expect(page.locator("section.pharos-artist h2")).toHaveText(artist.Name, {
        timeout: 15_000,
      });
      await page
        .locator("section.pharos-artist ul.pharos-grid li a", { hasText: track.Album })
        .first()
        .click();
      await expect(page.locator("section.pharos-album h2")).toHaveText(track.Album, {
        timeout: 15_000,
      });
      await expect(page.locator("ol.pharos-tracks li").first()).toBeVisible();
      await expect(page.locator("p.pharos-error")).toHaveCount(0);
    });

    test("unknown route renders a message, not a blank page", async ({
      page,
      request,
    }) => {
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
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });

      await page.goto("/ui/nope");
      await expect(page.getByText("Nothing at /ui/nope.")).toBeVisible({
        timeout: 15_000,
      });
    });

    test("find: a term in the seeded media shows tiles, nonsense says no matches", async ({
      page,
      request,
    }) => {
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
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });

      await page.getByRole("link", { name: "Find" }).click();
      await expect(page).toHaveURL(/\/ui\/find$/);
      const box = page.getByLabel("Search");
      // Every seeded title starts "Playwright …", so the term matches them.
      await box.fill("Playwright");
      await expect(page.locator(".pharos-poster").first()).toBeVisible({
        timeout: 15_000,
      });

      await box.fill("zzqx-no-such-title");
      await expect(page.getByText("No matches")).toBeVisible({
        timeout: 15_000,
      });
    });

    // The page is told it has neither WebM nor native HLS, as Firefox on a
    // WebM-less build would be, so the server remuxes the VP9 fixture to HLS
    // and the page plays it through the vendored hls.js. MediaSource answers
    // are left alone: hls.js asks them itself.
    test("an HLS title plays through hls.js and moves the resume point", async ({
      page,
      request,
    }) => {
      test.setTimeout(120_000);
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-watch-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      // A plain WebM fixture: with WebM hidden it remuxes to HLS. The startup
      // rescan probes it, so wait until the server knows its container.
      const fixture = async () =>
        (
          await (
            await request.get(
              `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path,MediaSources`,
              { headers: api },
            )
          ).json()
        ).Items?.find((i: { Path?: string }) => i.Path?.endsWith("/fixture-1.webm"));
      await expect
        .poll(async () => (await fixture())?.MediaSources?.[0]?.Container, {
          timeout: 30_000,
        })
        .toMatch(/webm/);
      const movie = await fixture();
      test.skip(!movie, "pharos has no seeded movie");
      const nativeId = BigInt(`0x${movie.Id}`).toString();

      await page.addInitScript(() => {
        const canPlayType = HTMLMediaElement.prototype.canPlayType;
        HTMLMediaElement.prototype.canPlayType = function (type: string) {
          return /mpegurl|webm|matroska/i.test(type) ? "" : canPlayType.call(this, type);
        };
      });
      const problems: string[] = [];
      page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));
      page.on("console", (m) => {
        if (m.text().includes("Refused to")) problems.push(`csp: ${m.text()}`);
      });
      const hlsjs: string[] = [];
      page.on("request", (r) => {
        if (r.url().includes("/assets/vendor/hls.min.js")) hlsjs.push(r.url());
      });

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });
      expect(hlsjs, "hls.js is fetched only by the watch page").toEqual([]);

      await page.goto(`/ui/title/${nativeId}`);
      await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
      await expect(page).toHaveURL(new RegExp(`/ui/watch/${nativeId}$`));
      await expect.poll(() => hlsjs.length, { timeout: 15_000 }).toBe(1);
      await expect
        .poll(
          () =>
            page.evaluate(
              () =>
                (document.getElementById("pharos-video") as HTMLVideoElement | null)
                  ?.currentTime ?? 0,
            ),
          { timeout: 60_000 },
        )
        .toBeGreaterThan(3);
      await page.evaluate(() =>
        (document.getElementById("pharos-video") as HTMLVideoElement).pause(),
      );

      // A pause is written at once (progress_writes), so the position lands
      // within a poll or two.
      await expect
        .poll(
          async () => {
            const res = await request.get(
              `${PHAROS_URL}/Users/${user.Id}/Items/${movie.Id}`,
              { headers: api },
            );
            return (await res.json()).UserData?.PlaybackPositionTicks ?? 0;
          },
          { timeout: 20_000 },
        )
        .toBeGreaterThan(3 * 10_000_000);
      expect(problems, problems.join("\n")).toEqual([]);
    });

    // A playlist that never loads is fatal to hls.js with nothing to restart
    // from, so the page must say so rather than show a black box.
    test("an HLS stream that cannot load shows why", async ({ page, request }) => {
      test.setTimeout(60_000);
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-fail-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const items = await (
        await request.get(
          `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path`,
          { headers: { "X-Emby-Token": token } },
        )
      ).json();
      // Probed by the seed, and with Matroska hidden it remuxes to HLS.
      const movie = items.Items?.find((i: { Path?: string }) =>
        i.Path?.endsWith("/multitrack.mkv"),
      );
      test.skip(!movie, "pharos has no seeded multitrack fixture");
      const nativeId = BigInt(`0x${movie.Id}`).toString();

      await page.addInitScript(() => {
        const canPlayType = HTMLMediaElement.prototype.canPlayType;
        HTMLMediaElement.prototype.canPlayType = function (type: string) {
          return /mpegurl|webm|matroska/i.test(type) ? "" : canPlayType.call(this, type);
        };
      });
      await page.route(/\.m3u8(\?|$)/, (route) => route.abort());
      const errors: string[] = [];
      page.on("console", (m) => {
        if (m.text().startsWith("pharos: playback error")) errors.push(m.text());
      });

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });
      await page.goto(`/ui/title/${nativeId}`);
      await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
      await expect(page.getByText(/^Playback failed: the stream failed/)).toBeVisible({
        timeout: 30_000,
      });
      expect(
        errors.some((e) => /engine=hlsjs .*fatal=true action=fail/.test(e)),
        errors.join("\n"),
      ).toBe(true);
    });

    test("the Appearance page applies, keeps, imports, refuses and exports themes", async ({
      page,
      request,
    }) => {
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 405,
        `native API is not enabled (status ${probe.status()}); skipping native test`,
      );
      const lines: string[] = [];
      page.on("console", (m) => {
        if (m.text().startsWith("pharos: theme")) lines.push(m.text());
      });
      const rootToken = (name: string) =>
        page.evaluate(
          (n) => getComputedStyle(document.documentElement).getPropertyValue(n).trim(),
          name,
        );

      await page.goto("/ui/");
      await page.evaluate(() => localStorage.removeItem("pharos.theme.custom"));
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await page.locator(".pharos-shell-nav").getByRole("link", { name: "Appearance" }).click();
      await expect(page).toHaveURL(/\/ui\/settings\/appearance$/);
      await expect(page.locator("li.pharos-theme-card")).toHaveCount(7);

      // Apply switches without a reload, and the pick survives one.
      await page.locator("#theme-apply-kali-dark").click();
      await expect.poll(() => rootToken("--px-color-bg")).toBe("#0b0f19");
      await page.reload();
      await expect.poll(() => rootToken("--px-color-bg")).toBe("#0b0f19");
      await expect
        .poll(() => lines.join("\n"))
        .toMatch(/pharos: theme applied name="Kali Dark"/);

      // Paste a theme: it is saved, listed as custom, and exports byte for byte.
      const pasted =
        '[meta]\nname = "Pasted"\nschema = 1\nextends = "Neon Grid"\n\n[palette]\naccent = "#33ee11"\n';
      const paste = async (text: string) => {
        await page.locator("#theme-import-paste").fill(text);
        await page.locator("#theme-import-paste-go").click();
      };
      await paste(pasted);
      await expect(page.getByRole("status")).toContainText("Imported Pasted");
      await expect(page.locator("#theme-delete-pasted")).toBeVisible();
      const download = page.waitForEvent("download");
      await page.locator("#theme-export-pasted").click();
      const file = await download;
      expect(file.suggestedFilename()).toBe("pasted.pharos-theme.toml");
      const fs = await import("node:fs/promises");
      expect(await fs.readFile((await file.path()) ?? "", "utf8")).toBe(pasted);

      // Hostile input is refused naming the field, and never reaches CSS.
      lines.length = 0;
      await paste(
        '[meta]\nname = "Evil"\nschema = 1\n\n[palette]\naccent = "red; background: url(x)"\n',
      );
      await expect(page.getByRole("alert")).toContainText("palette.accent");
      await expect
        .poll(() => lines.join("\n"))
        .toMatch(/pharos: theme fallback name=Evil source=import reason=bad_value path=palette.accent/);
      await expect(page.locator("#theme-apply-evil")).toHaveCount(0);

      // A theme that fails contrast is refused with the pair and its ratio.
      lines.length = 0;
      await paste(
        '[meta]\nname = "Murky"\nschema = 1\n\n[palette]\nfg = "#101318"\n',
      );
      await expect(page.getByRole("alert")).toContainText("contrast is too low");
      await expect
        .poll(() => lines.join("\n"))
        .toMatch(/pharos: theme rejected name=Murky source=import reason=contrast pair=fg\/bg/);
    });
  });
});

// 013 US7 / ADR-0039: a song opens the now-playing screen with a visualiser.
// The seeded media tree carries 05-carefree.mp3. Headless CI may have no
// WebGL2, so either renderer mode passes; the decision line must be there.
test.describe("pharos Dioxus UI now playing", () => {
  test("a song plays on the listening screen and logs its renderer decision", async ({
    page,
    request,
  }) => {
    test.setTimeout(90_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(
      probe.status() !== 405,
      `native API is not enabled (status ${probe.status()}); skipping native test`,
    );
    const lines: string[] = [];
    page.on("console", (m) => lines.push(m.text()));

    await page.goto("/ui/");
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({
      timeout: 15_000,
    });

    await page.getByRole("link", { name: "Find" }).click();
    await page.getByLabel("Search").fill("carefree");
    const tile = page.locator(".pharos-poster").first();
    await expect(tile).toBeVisible({ timeout: 15_000 });
    await tile.click();
    // The click on Play is the gesture Web Audio needs.
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();

    await expect(page.locator(".pharos-listen")).toBeVisible({
      timeout: 15_000,
    });
    await expect
      .poll(
        () =>
          page.evaluate(
            () =>
              (
                document.getElementById(
                  "pharos-video",
                ) as HTMLMediaElement | null
              )?.currentTime ?? 0,
          ),
        {
          timeout: 30_000,
        },
      )
      .toBeGreaterThan(1);
    await expect
      .poll(
        () =>
          lines.find((l) =>
            /pharos: renderer mode=(webgl2|fallback) /.test(l),
          ) ?? "",
        {
          timeout: 15_000,
        },
      )
      .toMatch(/style=spectrum/);

    await page.getByRole("button", { name: "Halo" }).click();
    await expect(page.getByRole("button", { name: "Halo" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect
      .poll(() =>
        page.evaluate(() => localStorage.getItem("pharos.visualiser")),
      )
      .toBe("halo");
  });
});
