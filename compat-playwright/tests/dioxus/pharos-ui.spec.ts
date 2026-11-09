import { test, expect, type APIRequestContext, type Locator, type Page } from "@playwright/test";

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
// A screen that failed to load: the bare error line, or the panel with a
// retry that Home, Browse and Title show since B823.
const LOAD_ERRORS = "p.pharos-error, .pharos-load-failure";

/** A song as the Jellyfin item list returns it. */
type Song = { Album?: string; AlbumArtist?: string; Artists?: string[] };
/** Who a song is by: its album artist, or else its first track artist. */
const songBy = (t: Song) => t.AlbumArtist ?? t.Artists?.[0];

// Opens the first title the top bar's search suggests for `term`. The
// suggestions are the first hits of the same search the palette runs, in
// its order (the bar replaced the old Find page). Only a hit naming the
// term counts, so a list still showing an earlier search is never clicked,
// and the helper returns once the page has moved to that hit and shows it.
// The shows, seasons, artists and albums listed ahead of titles (V518) are
// passed over.
async function openFirstHit(page: Page, term: string): Promise<void> {
  await page.getByRole("combobox", { name: "Search all libraries" }).fill(term);
  const group = page.locator(".pharos-muted", {
    hasText: /^(Series|Season|Album|Artist)( ·|$)/,
  });
  const hit = page
    .getByRole("option")
    .filter({ hasText: new RegExp(term, "i") })
    .filter({ hasNot: group })
    .first()
    .getByRole("link");
  await expect(hit).toBeVisible({ timeout: 15_000 });
  const href = await hit.getAttribute("href");
  const name = (await hit.locator(".pharos-search-name").textContent())?.trim();
  await hit.click();
  if (href) {
    await expect(page).toHaveURL((url) => url.pathname === href, { timeout: 15_000 });
  }
  // The page shows the hit, not the title it was opened from (B555). An
  // episode's hit reads "Show - S01E01" where its page heads "Show".
  const heading = name?.split(" - ")[0];
  if (heading) {
    await expect(page.locator(".pharos-main")).toContainText(heading, { timeout: 15_000 });
  }
}

// Signs in on a small window and answers a home rail that overflows
// sideways, skipping when the server has no native API or seeded user.
async function overflowingRail(page: Page, request: APIRequestContext): Promise<Locator> {
  const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
    headers: { Accept: "application/json" },
  });
  test.skip(
    probe.status() !== 405,
    `native API is not enabled (status ${probe.status()}); skipping native test`,
  );
  // Part-watch a few videos, so the home screen holds the same shelves
  // whichever tests ran before this one.
  const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
    headers: {
      "Content-Type": "application/json",
      "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-wheel-${Date.now()}", Version="0"`,
    },
    data: { Username: TEST_USER, Pw: TEST_PASS },
  });
  test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
  const { AccessToken: token, User: user } = await auth.json();
  const api = { "X-Emby-Token": token };
  const videos = await (
    await request.get(
      `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Episode,Movie&Limit=4`,
      { headers: api },
    )
  ).json();
  for (const v of videos.Items ?? []) {
    await request.post(`${PHAROS_URL}/Sessions/Playing/Progress`, {
      headers: api,
      data: { ItemId: v.Id, PlaySessionId: `pw-wheel-${v.Id}`, PositionTicks: 25_000_000, IsPaused: true },
    });
  }
  // Narrow and short, so the rails overflow and the page scrolls.
  await page.setViewportSize({ width: 320, height: 360 });
  await page.goto("/ui/");
  await page.locator("#signin-name").fill(TEST_USER);
  await page.locator("#signin-password").fill(TEST_PASS);
  await page.getByRole("button", { name: "Sign in" }).click();
  const rails = page.locator("ul.pharos-rail-items");
  await expect(rails.first().locator("li").first()).toBeVisible({ timeout: 15_000 });
  // Every shelf loaded, so the layout no longer moves under the pointer.
  await page.waitForLoadState("networkidle");
  const at = await rails.evaluateAll((els) =>
    els.findIndex((el) => el.scrollWidth > el.clientWidth),
  );
  expect(at, "some rail overflows sideways").toBeGreaterThanOrEqual(0);
  return rails.nth(at);
}

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
      // The seeded user is an admin, so the account menu offers Manage.
      const account = page.getByRole("button", { name: "Account", exact: true });
      await expect(account).toHaveText(new RegExp(`^${TEST_USER.charAt(0).toUpperCase()}`));
      await expect(account).toHaveAttribute("aria-expanded", "false");
      await account.click();
      await expect(account).toHaveAttribute("aria-expanded", "true");
      await expect(page.getByRole("menuitem", { name: "Manage" })).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(page.getByRole("menu")).toHaveCount(0);

      await account.click();
      await page.getByRole("menuitem", { name: "Sign out" }).click();
      await expect(page.locator("form#signin")).toBeVisible();
    });

    test("a user changes their own password from the account menu", async ({ page, request }) => {
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 405,
        `native API is not enabled (status ${probe.status()}); skipping native test`,
      );
      // A user of its own, so the seeded user's password never changes.
      const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-passwd-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token } = await auth.json();
      const name = `pw-passwd-${Date.now()}`;
      const created = await request.post(`${PHAROS_URL}/Users/New`, {
        headers: { "X-Emby-Token": token, "Content-Type": "application/json" },
        data: { Name: name, Password: "first-pass" },
      });
      expect(created.ok()).toBeTruthy();

      const signIn = async (password: string) => {
        await page.locator("#signin-name").fill(name);
        await page.locator("#signin-password").fill(password);
        await page.getByRole("button", { name: "Sign in" }).click();
      };
      await page.goto("/ui/");
      await signIn("first-pass");
      const account = page.getByRole("button", { name: /^Account/ });
      await account.click();
      // Not an admin: no Manage.
      await expect(page.getByRole("menuitem", { name: "Manage" })).toHaveCount(0);
      await page.getByRole("menuitem", { name: "Settings" }).click();
      await page.getByRole("link", { name: "Account", exact: true }).click();
      await expect(page).toHaveURL(/\/ui\/settings\/account$/);

      const form = page.locator("form#change-password");
      await form.locator("#password-current").fill("wrong");
      await form.locator("#password-new").fill("second-pass");
      await form.locator("#password-confirm").fill("second-pass");
      await form.getByRole("button", { name: "Change password" }).click();
      await expect(form.getByRole("alert")).toContainText("current password is not right");

      await form.locator("#password-current").fill("first-pass");
      await form.locator("#password-new").fill("second-pass");
      await form.locator("#password-confirm").fill("second-pass");
      await form.getByRole("button", { name: "Change password" }).click();
      await expect(form.getByRole("status")).toContainText("Password changed");

      await account.click();
      await page.getByRole("menuitem", { name: "Sign out" }).click();
      await signIn("first-pass");
      await expect(page.locator(".pharos-error")).toBeVisible();
      await signIn("second-pass");
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
    });

    test("a user approves a Quick Connect code from the account menu", async ({ page, request }) => {
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 405,
        `native API is not enabled (status ${probe.status()}); skipping native test`,
      );
      // A user of its own, so the seeded user's password never changes.
      const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-qc-auth-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token } = await auth.json();
      const name = `pw-qc-${Date.now()}`;
      const created = await request.post(`${PHAROS_URL}/Users/New`, {
        headers: { "X-Emby-Token": token, "Content-Type": "application/json" },
        data: { Name: name, Password: "test-pass" },
      });
      expect(created.ok()).toBeTruthy();

      const rand = Math.floor(Math.random() * 1_000_000);
      const initiated = await request.post(`${PHAROS_URL}/QuickConnect/Initiate`, {
        headers: {
          Authorization: `MediaBrowser Client="pw", Device="tv", DeviceId="pw-qc-${rand}", Version="1"`,
        },
      });
      expect(initiated.ok()).toBeTruthy();
      const { Code, Secret } = await initiated.json();

      const signIn = async (password: string) => {
        await page.locator("#signin-name").fill(name);
        await page.locator("#signin-password").fill(password);
        await page.getByRole("button", { name: "Sign in" }).click();
      };
      await page.goto("/ui/");
      await signIn("test-pass");
      const account = page.getByRole("button", { name: /^Account/ });
      await account.click();
      await page.getByRole("menuitem", { name: "Approve a device" }).click();
      await expect(page).toHaveURL(/\/ui\/settings\/approve-device$/);

      const form = page.locator("form#approve-device");
      await form.locator("input").fill("000000");
      await form.getByRole("button", { name: "Approve" }).click();
      await expect(form.getByRole("alert")).toContainText("not right");

      await form.locator("input").fill(Code);
      await form.getByRole("button", { name: "Approve" }).click();
      await expect(form.getByRole("status")).toContainText("Device approved");

      const connect = await request.get(`${PHAROS_URL}/QuickConnect/Connect?secret=${Secret}`);
      expect(connect.ok()).toBeTruthy();
      const connectJson = await connect.json();
      expect(connectJson.Authenticated).toBe(true);

      // The device's own last step: the secret buys a token for the approver.
      const finish = await request.post(`${PHAROS_URL}/Users/AuthenticateWithQuickConnect`, {
        headers: {
          "Content-Type": "application/json",
          Authorization: `MediaBrowser Client="pw", Device="tv", DeviceId="pw-qc-${rand}", Version="1"`,
        },
        data: { Secret },
      });
      expect(finish.ok()).toBeTruthy();
      const finished = await finish.json();
      expect(finished.User.Name).toBe(name);
      expect(typeof finished.AccessToken).toBe("string");
      const me = await request.get(`${PHAROS_URL}/Users/Me`, {
        headers: { "X-Emby-Token": finished.AccessToken },
      });
      expect(me.ok()).toBeTruthy();
      expect((await me.json()).Name).toBe(name);
    });

    test("a user sets and removes their avatar from the Account page", async ({ page, request }) => {
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-avatar-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token } = await auth.json();
      const name = `pw-avatar-${Date.now()}`;
      const created = await request.post(`${PHAROS_URL}/Users/New`, {
        headers: { "X-Emby-Token": token, "Content-Type": "application/json" },
        data: { Name: name, Password: "avatar-pass" },
      });
      expect(created.ok()).toBeTruthy();

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(name);
      await page.locator("#signin-password").fill("avatar-pass");
      await page.getByRole("button", { name: "Sign in" }).click();
      const account = page.getByRole("button", { name: "Account", exact: true });
      await expect(account.locator("img")).toHaveCount(0);
      await account.click();
      await page.getByRole("menuitem", { name: "Settings" }).click();
      await page.getByRole("link", { name: "Account", exact: true }).click();

      // A noisy picture well over the 768 KiB a message may carry, so the
      // browser has to scale it down before it sends it.
      const big = await page.evaluate(async () => {
        const c = document.createElement("canvas");
        c.width = c.height = 900;
        const ctx = c.getContext("2d")!;
        const data = ctx.createImageData(900, 900);
        for (let i = 0; i < data.data.length; i += 65536) {
          crypto.getRandomValues(data.data.subarray(i, i + 65536));
        }
        for (let i = 3; i < data.data.length; i += 4) data.data[i] = 255;
        ctx.putImageData(data, 0, 0);
        const blob: Blob = await new Promise((r) => c.toBlob((b) => r(b!), "image/png"));
        const bytes = new Uint8Array(await blob.arrayBuffer());
        let bin = "";
        for (let i = 0; i < bytes.length; i += 8192) {
          bin += String.fromCharCode(...bytes.subarray(i, i + 8192));
        }
        return btoa(bin);
      });
      expect(Buffer.from(big, "base64").length).toBeGreaterThan(768 * 1024);
      const chooser = page.waitForEvent("filechooser");
      await page.locator("#avatar-choose").click();
      await (await chooser).setFiles({
        name: "noise.png",
        mimeType: "image/png",
        buffer: Buffer.from(big, "base64"),
      });

      // The menu button and the page now show the server's picture.
      const picture = account.locator("img");
      await expect(picture).toHaveAttribute("src", /^\/UserImage\?userId=[0-9a-f]{32}&tag=[0-9a-f]{16}$/);
      await expect
        .poll(() => picture.evaluate((img: HTMLImageElement) => img.naturalWidth))
        .toBe(512);
      await expect(page.locator("#avatar-remove")).toBeVisible();

      await page.locator("#avatar-remove").click();
      await expect(account.locator("img")).toHaveCount(0);
      await expect(page.locator("#avatar-remove")).toHaveCount(0);
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
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);

      // A series is the show page: its name over the season's episodes.
      await page.goto(`/ui/browse/${nativeSynth(episode.SeriesId)}`);
      await expect(page.locator("section.pharos-show h2")).toHaveText(episode.SeriesName, {
        timeout: 15_000,
      });
      await expect(page.locator("ol.pharos-episodes li").first()).toBeVisible();
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);

      // A season opens the same page with that season shown.
      await page.goto(`/ui/browse/${nativeSynth(episode.SeasonId)}`);
      await expect(page.locator("section.pharos-show h2")).toHaveText(episode.SeriesName, {
        timeout: 15_000,
      });
      await expect(
        page.locator("section.pharos-show").getByText(/^Season 1$/).first(),
      ).toBeVisible();
      await expect(page.locator("ol.pharos-episodes li").first()).toBeVisible();
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);

      // An episode row plays it.
      await page.locator("ol.pharos-episodes li a.pharos-episode-play").first().click();
      await expect(page).toHaveURL(/\/ui\/watch\/\d+$/);
    });

    // A library larger than one browse page (100) loads the rest as the grid
    // is scrolled (V237); the seed's "Bulk" library holds 120 titles.
    test("browse scrolls a library past one page and every title loads", async ({
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-scroll-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      const json = async (path: string) =>
        (await request.get(`${PHAROS_URL}${path}`, { headers: api })).json();
      const views = await json(`/Users/${user.Id}/Views`);
      const bulk = views.Items?.find((i: { Name: string }) => i.Name === "Bulk");
      test.skip(!bulk, "the seed has no Bulk library");
      // The startup scan may still be indexing the titles.
      let total = 0;
      await expect
        .poll(
          async () => {
            const listed = await json(
              `/Users/${user.Id}/Items?ParentId=${bulk.Id}&Recursive=true&Limit=1`,
            );
            total = listed.TotalRecordCount ?? 0;
            return total;
          },
          { timeout: 60_000 },
        )
        .toBeGreaterThan(100);

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });
      await page.locator("section.pharos-rail h2 a", { hasText: /^Bulk$/ }).click();
      const tiles = page.locator("section.pharos-browse ul.pharos-grid li");
      await expect(tiles.first()).toBeVisible({ timeout: 15_000 });
      expect(await tiles.count(), "the first page is 100 tiles").toBe(100);

      // Scroll to the bottom until the tile count stops growing.
      let seen = 0;
      let stable = 0;
      for (let round = 0; round < 40 && stable < 3; round++) {
        await page.evaluate(() =>
          window.scrollTo(0, document.documentElement.scrollHeight),
        );
        await page.waitForTimeout(500);
        const now = await tiles.count();
        stable = now === seen ? stable + 1 : 0;
        seen = now;
      }
      expect(seen, "every title loads once the grid is scrolled").toBe(total);
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);
      await expect(page.getByRole("button", { name: "Show more" })).toHaveCount(0);
    });

    // A mouse wheel over a rail scrolls the page, not the rail: turning it
    // sideways trapped the page under the shelves on the home screen.
    // Sideways input still scrolls the rail.
    test("a vertical wheel over a rail scrolls the page, a sideways one the rail", async ({ page, request }) => {
      const rail = await overflowingRail(page, request);

      // Settle the page and the pointer before the wheel: a wheel sent while
      // the page is still scrolling to the rail can land on stale layout.
      const frames = () =>
        page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
      await rail.scrollIntoViewIfNeeded();
      await frames();
      const box = await rail.boundingBox();
      expect(box, "the rail is on the page").toBeTruthy();
      await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
      await frames();
      const top = await page.evaluate(() => window.scrollY);
      await page.mouse.wheel(0, 200);

      await expect.poll(() => page.evaluate(() => window.scrollY)).toBeGreaterThan(top);
      expect(await rail.evaluate((el) => el.scrollLeft)).toBe(0);

      // Sideways input still moves the rail and leaves the page: a
      // horizontal wheel or trackpad swipe, then Shift+wheel. The page moved,
      // so point at the rail again.
      const moved = await rail.boundingBox();
      await page.mouse.move(moved!.x + moved!.width / 2, moved!.y + moved!.height / 2);
      await frames();
      const y = await page.evaluate(() => window.scrollY);
      await page.mouse.wheel(120, 0);
      await expect.poll(() => rail.evaluate((el) => el.scrollLeft)).toBeGreaterThan(0);
      await rail.evaluate((el) => el.scrollTo({ left: 0, behavior: "instant" }));
      await frames();
      await page.keyboard.down("Shift");
      await page.mouse.wheel(0, 120);
      await page.keyboard.up("Shift");
      await expect.poll(() => rail.evaluate((el) => el.scrollLeft)).toBeGreaterThan(0);
      expect(await page.evaluate(() => window.scrollY)).toBe(y);
    });

    // A notch of sideways wheel moves a rail a few posters, not under one,
    // and a mouse drag scrolls it without opening the poster it started on;
    // a plain click still does (B315).
    test("a rail moves further per sideways notch and scrolls with a mouse drag", async ({ page, request }) => {
      const rail = await overflowingRail(page, request);
      const frames = () =>
        page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
      const left = () => rail.evaluate((el) => el.scrollLeft);
      const reset = async () => {
        await rail.evaluate((el) => el.scrollTo({ left: 0, behavior: "instant" }));
        await frames();
      };
      await rail.scrollIntoViewIfNeeded();
      await frames();
      const box = (await rail.boundingBox())!;
      const midY = box.y + box.height / 2;
      await page.mouse.move(box.x + box.width / 2, midY);
      await frames();

      const max = await rail.evaluate((el) => el.scrollWidth - el.clientWidth);
      await page.mouse.wheel(100, 0);
      await expect.poll(left).toBeGreaterThan(Math.min(250, max - 1));
      await reset();

      const url = page.url();
      await page.mouse.move(box.x + box.width - 20, midY);
      await page.mouse.down();
      await page.mouse.move(box.x + box.width / 2, midY, { steps: 8 });
      await page.mouse.move(box.x + 20, midY, { steps: 8 });
      await expect(rail).toHaveClass(/is-dragging/);
      await page.mouse.up();
      await expect(rail).not.toHaveClass(/is-dragging/);
      expect(await left()).toBeGreaterThan(Math.min(100, max - 1));
      await frames();
      expect(page.url(), "the drag opened no poster").toBe(url);

      await rail.locator("a.pharos-poster").first().click();
      await expect(page).not.toHaveURL(url);
    });

    // A library jumps by first letter (V252): the rail on the right goes
    // straight to one, reading ahead past the first page; Shift+wheel steps
    // between letters; a date order hides the rail. The seed's "Bulk" names
    // run AA, BB, ... ZZ, so Z starts past the first 100 tiles.
    test("browse jumps a library by letter, and by year in a date order", async ({
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-letters-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      const json = async (path: string) =>
        (await request.get(`${PHAROS_URL}${path}`, { headers: api })).json();
      const views = await json(`/Users/${user.Id}/Views`);
      const bulk = views.Items?.find((i: { Name: string }) => i.Name === "Bulk");
      test.skip(!bulk, "the seed has no Bulk library");
      await expect
        .poll(
          async () =>
            (
              await json(
                `/Users/${user.Id}/Items?ParentId=${bulk.Id}&Recursive=true&Limit=1`,
              )
            ).TotalRecordCount ?? 0,
          { timeout: 60_000 },
        )
        .toBeGreaterThan(100);

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
      await page.locator("section.pharos-rail h2 a", { hasText: /^Bulk$/ }).click();
      const tiles = page.locator("section.pharos-browse ul.pharos-grid li");
      await expect(tiles.first()).toBeVisible({ timeout: 15_000 });
      const rail = page.getByRole("navigation", { name: "Jump to letter" });
      await expect(rail).toBeVisible({ timeout: 15_000 });
      await expect(rail.getByRole("button", { name: "Numbers and symbols" })).toBeDisabled();

      // Z lies past the first page: the jump reads ahead to it.
      expect(await tiles.count()).toBe(100);
      await rail.getByRole("button", { name: "Z", exact: true }).click();
      const firstZ = tiles.filter({ hasText: /^ZZ Bulk/ }).first();
      await expect(firstZ).toBeInViewport({ timeout: 15_000 });
      expect(await tiles.count()).toBeGreaterThan(100);
      await expect(rail.getByRole("button", { name: "Z", exact: true })).toHaveAttribute(
        "aria-current",
        "true",
      );

      // Shift+wheel up steps back one letter, to Y, and leaves the page
      // where the jump put it otherwise.
      await page.mouse.move(400, 400);
      await page.keyboard.down("Shift");
      await page.mouse.wheel(0, -120);
      await page.keyboard.up("Shift");
      await expect(tiles.filter({ hasText: /^YY Bulk/ }).first()).toBeInViewport();
      await expect(rail.getByRole("button", { name: "Y", exact: true })).toHaveAttribute(
        "aria-current",
        "true",
      );

      // A plain wheel still scrolls the page (B295).
      const before = await page.evaluate(() => window.scrollY);
      await page.mouse.wheel(0, -300);
      await expect.poll(() => page.evaluate(() => window.scrollY)).toBeLessThan(before);

      // A filter narrows the listing but keeps it in name order: the rail
      // stays and still jumps (V252).
      await page.getByLabel("Watched", { exact: true }).selectOption("unwatched");
      await expect(rail).toBeVisible({ timeout: 15_000 });
      await rail.getByRole("button", { name: "Z", exact: true }).click();
      await expect(tiles.filter({ hasText: /^ZZ Bulk/ }).first()).toBeInViewport({
        timeout: 15_000,
      });
      await page.getByLabel("Watched", { exact: true }).selectOption({ index: 0 });
      await page.getByLabel("Genre", { exact: true }).selectOption("Drama");
      await expect(tiles).toHaveCount(60, { timeout: 15_000 });
      await expect(rail).toBeVisible({ timeout: 15_000 });
      // The Dramas are the even titles, which run A, C, E ... Y.
      await expect(rail.getByRole("button", { name: "Z", exact: true })).toBeDisabled();
      await rail.getByRole("button", { name: "Y", exact: true }).click();
      await expect(tiles.filter({ hasText: /^YY Bulk/ }).first()).toBeInViewport({
        timeout: 15_000,
      });
      await page.getByLabel("Genre", { exact: true }).selectOption({ index: 0 });

      // A date order jumps by year instead (V256). The Dramas span 40 years,
      // more than the rail shows at once, so it shows every few.
      await page.getByLabel("Sort", { exact: true }).selectOption("released");
      const years = page.getByRole("navigation", { name: "Jump to year" });
      await expect(years).toBeVisible({ timeout: 15_000 });
      await expect(rail).toHaveCount(0);
      await expect(years.getByRole("button")).toHaveCount(27);
      await expect(years.getByRole("button").first()).toHaveText("2019");
      // The last is the undated odd titles, after the 60 dated ones.
      const undated = years.getByRole("button", { name: "No date" });
      await expect(years.getByRole("button").last()).toHaveText("#");
      await page.evaluate(() => window.scrollTo(0, 0));
      await undated.click();
      await expect(undated).toHaveAttribute("aria-current", "true");
      await expect.poll(() => page.evaluate(() => window.scrollY)).toBeGreaterThan(0);

      // Date added, newest first: everything arrived this year.
      await page.getByLabel("Sort", { exact: true }).selectOption("added");
      await expect(page.getByRole("button", { name: /Sort direction: Newest first/ })).toBeVisible();
      await expect(years.getByRole("button")).toHaveCount(1, { timeout: 15_000 });
      await expect(years.getByRole("button").first()).toHaveText(
        String(new Date().getUTCFullYear()),
      );
      await expect(tiles.first()).toBeVisible();
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);
      await page.evaluate(() => window.scrollTo(0, 0));
    });

    // B560: a library keeps its filters in the URL and for the tab, and
    // comes back where it was left, by Back or by the breadcrumb.
    test("browse keeps a library's filters and place across a title", async ({
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-libstate-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      const json = async (path: string) =>
        (await request.get(`${PHAROS_URL}${path}`, { headers: api })).json();
      const views = await json(`/Users/${user.Id}/Views`);
      const bulk = views.Items?.find((i: { Name: string }) => i.Name === "Bulk");
      test.skip(!bulk, "the seed has no Bulk library");
      await expect
        .poll(
          async () =>
            (
              await json(
                `/Users/${user.Id}/Items?ParentId=${bulk.Id}&Recursive=true&Limit=1`,
              )
            ).TotalRecordCount ?? 0,
          { timeout: 60_000 },
        )
        .toBeGreaterThan(100);

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
      await page.locator("section.pharos-rail h2 a", { hasText: /^Bulk$/ }).click();
      const tiles = page.locator("section.pharos-browse ul.pharos-grid li");
      await expect(tiles.first()).toBeVisible({ timeout: 15_000 });
      const reset = page.getByRole("button", { name: "Reset filters" });
      await expect(reset).toHaveCount(0);
      const bare = new URL(page.url());
      expect(bare.search).toBe("");

      // Z to A, unwatched only: both land in the URL.
      await page.getByRole("button", { name: /^Sort direction/ }).click();
      await page.getByLabel("Watched", { exact: true }).selectOption("unwatched");
      await expect(page).toHaveURL(/\?dir=desc&watched=unwatched$/);
      await expect(reset).toBeVisible();

      // Deep into the grid, past its first page, the tile at the top a
      // little scrolled out.
      const rail = page.getByRole("navigation", { name: "Jump to letter" });
      await expect(rail).toBeVisible({ timeout: 15_000 });
      await rail.getByRole("button", { name: "C", exact: true }).click();
      await expect(tiles.filter({ hasText: /^CC Bulk/ }).first()).toBeInViewport({
        timeout: 15_000,
      });
      await page.mouse.move(400, 400);
      await page.mouse.wheel(0, 150);
      const place = async () =>
        page.evaluate(() => {
          const items = [...document.querySelectorAll("section.pharos-browse ul.pharos-grid li")];
          const at = items.findIndex((li) => li.getBoundingClientRect().bottom > 0);
          return { at, top: Math.round(items[at].getBoundingClientRect().top) };
        });
      await expect.poll(async () => (await place()).at).toBeGreaterThan(100);
      // Past the settle delay, so the place is kept.
      await page.waitForTimeout(500);
      let left = await place();
      let name = (await tiles.nth(left.at).innerText()).trim();

      const backTo = async (how: string) => {
        await expect(page).toHaveURL(/\?dir=desc&watched=unwatched$/, { timeout: 15_000 });
        await expect(page.getByLabel("Watched", { exact: true })).toHaveValue("unwatched");
        await expect(page.getByRole("button", { name: /^Sort direction/ })).toHaveText(
          "Z to A",
        );
        // `name` came from `innerText`, so compare against the same.
        await expect(tiles.nth(left.at)).toHaveText(name, {
          timeout: 15_000,
          useInnerText: true,
        });
        await expect
          .poll(async () => (await place()).top, { message: how, timeout: 15_000 })
          .toBe(left.top);
        expect((await place()).at, how).toBe(left.at);
      };

      // Opens the tile at `left.at` where it sits. Playwright's own click
      // first scrolls a partly hidden link fully into view, which would
      // move the place being kept (the tile's top to 0) before the click.
      const open = async () =>
        tiles
          .nth(left.at)
          .locator("a")
          .first()
          .evaluate((a: HTMLElement) => a.click());

      // Back from a title.
      await open();
      await expect(page.locator("article.pharos-title")).toBeVisible({ timeout: 15_000 });
      await page.goBack();
      await backTo("back");

      // A title opened straight after scrolling, inside the settle delay,
      // still comes back to that scroll. All in one page task, so the click
      // lands two frames after the scroll event (well inside 150 ms) and on
      // a link already in view, with nothing scrolling it there first.
      const quick = await page.evaluate(
        () =>
          new Promise<{ at: number; top: number; name: string; ms: number }>((done) => {
            const started = performance.now();
            window.scrollBy(0, 120);
            requestAnimationFrame(() =>
              requestAnimationFrame(() => {
                const items = [
                  ...document.querySelectorAll("section.pharos-browse ul.pharos-grid li"),
                ];
                const at = items.findIndex((li) => li.getBoundingClientRect().bottom > 0);
                const top = Math.round(items[at].getBoundingClientRect().top);
                const name = (items[at] as HTMLElement).innerText.trim();
                const ms = performance.now() - started;
                items[at].querySelector("a")?.click();
                done({ at, top, name, ms });
              }),
            );
          }),
      );
      expect(quick.ms, "the click came inside the settle delay").toBeLessThan(150);
      expect(quick.top).not.toBe(left.top);
      left = { at: quick.at, top: quick.top };
      name = quick.name;
      await expect(page.locator("article.pharos-title")).toBeVisible({ timeout: 15_000 });
      await page.goBack();
      await backTo("back inside the settle delay");

      // The breadcrumb's bare library link: the tab's last choice and place.
      await open();
      await expect(page.locator("article.pharos-title")).toBeVisible({ timeout: 15_000 });
      await page.locator("nav.pharos-crumbs a", { hasText: /^Bulk$/ }).click();
      await backTo("breadcrumb");

      // Reset returns to the bare listing, at the top.
      await reset.click();
      await expect(page).toHaveURL(new RegExp(`${bare.pathname}$`));
      await expect(reset).toHaveCount(0);
      await expect(page.getByRole("button", { name: /^Sort direction/ })).toHaveText("A to Z");
      await expect(tiles.filter({ hasText: /^AA Bulk/ }).first()).toBeInViewport({
        timeout: 15_000,
      });
    });

    // A TV library lists its series by name and a series opens the show page
    // with its episodes; every step is a click, not a URL.
    // B599: a show opened from a library grid scrolled down opens at its
    // top, where its name and overview are, not part way down its episodes.
    test("a show opened from a scrolled grid opens at its top", async ({ page, request }) => {
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 405,
        `native API is not enabled (status ${probe.status()}); skipping native test`,
      );
      await page.setViewportSize({ width: 420, height: 360 });
      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
      await page.locator("section.pharos-rail h2 a", { hasText: /^TV$/ }).click();
      const tile = page
        .locator("ul.pharos-grid .pharos-poster")
        .filter({ hasText: "Playwright Show" });
      await expect(tile).toBeVisible({ timeout: 15_000 });
      await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
      await expect.poll(() => page.evaluate(() => window.scrollY)).toBeGreaterThan(100);
      await tile.click();
      await expect(page.locator("section.pharos-show h2")).toHaveText("Playwright Show", {
        timeout: 15_000,
      });
      await expect(page.locator("ol.pharos-episodes li")).toHaveCount(3);
      await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0);
    });

    test("browse walks TV: library, series, episode, title", async ({ page, request }) => {
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-tv-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });

      // Home -> the TV library, whose tiles are series sorted by name.
      await page.locator("section.pharos-rail h2 a", { hasText: /^TV$/ }).click();
      await expect(page.locator("section.pharos-browse h2")).toHaveText("TV", {
        timeout: 15_000,
      });
      const tiles = page.locator("ul.pharos-grid .pharos-poster-name");
      await expect(tiles).toHaveText(["Another Show", "Playwright Show"]);
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);

      // Series -> the show page with its three episodes.
      await tiles.filter({ hasText: "Playwright Show" }).click();
      await expect(page.locator("section.pharos-show h2")).toHaveText("Playwright Show", {
        timeout: 15_000,
      });
      const episodes = page.locator("ol.pharos-episodes li");
      await expect(episodes).toHaveCount(3);
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);

      // Episode -> its title page, named as the row was.
      const row = (await episodes.first().locator(".pharos-episode-name").innerText()).trim();
      const name = row.replace(/^\d+\.\s+/, "");
      await episodes.first().locator("a.pharos-episode-link").click();
      await expect(page).toHaveURL(/\/ui\/title\//);
      await expect(page.locator("article.pharos-title h2")).toHaveText(name, {
        timeout: 15_000,
      });
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);
    });

    // B318: the library buttons and the breadcrumb trail.
    test("library buttons and breadcrumbs lead back up from an episode", async ({
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-crumbs-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });

      // The TV library's button comes before the films', and opens it.
      // The list loads after the bar shows, so wait for both buttons first.
      const libraries = page.locator("nav.pharos-library-nav a");
      await expect(libraries.filter({ hasText: /^Bulk$/ })).toBeVisible({ timeout: 15_000 });
      await expect(libraries.filter({ hasText: /^TV$/ })).toBeVisible();
      const names = await libraries.allInnerTexts();
      expect(names.indexOf("TV")).toBeGreaterThanOrEqual(0);
      expect(names.indexOf("TV")).toBeLessThan(names.indexOf("Bulk"));
      await libraries.filter({ hasText: /^TV$/ }).click();
      await expect(page.locator("section.pharos-browse h2")).toHaveText("TV", {
        timeout: 15_000,
      });
      await expect(libraries.filter({ hasText: /^TV$/ })).toHaveAttribute("aria-current", "page");

      // Down to an episode: the trail runs library, show, season, episode.
      await page
        .locator("ul.pharos-grid .pharos-poster-name")
        .filter({ hasText: "Playwright Show" })
        .click();
      const episodes = page.locator("ol.pharos-episodes li");
      await expect(episodes).toHaveCount(3, { timeout: 15_000 });
      await episodes.first().locator("a.pharos-episode-link").click();
      const title = (await page.locator("article.pharos-title h2").innerText()).trim();
      const crumbs = page.locator("nav.pharos-crumbs li");
      await expect(crumbs).toHaveCount(4, { timeout: 15_000 });
      await expect(crumbs.nth(0)).toHaveText("TV");
      await expect(crumbs.nth(1)).toHaveText("Playwright Show");
      await expect(crumbs.nth(3)).toHaveText(title);
      await expect(crumbs.nth(3).locator("[aria-current=page]")).toHaveCount(1);

      // Each step back up is a link.
      await crumbs.nth(1).locator("a").click();
      await expect(page.locator("section.pharos-show h2")).toHaveText("Playwright Show", {
        timeout: 15_000,
      });
      await page.locator("nav.pharos-crumbs a", { hasText: /^TV$/ }).click();
      await expect(page.locator("section.pharos-browse h2")).toHaveText("TV", {
        timeout: 15_000,
      });
      await libraries.filter({ hasText: /^Bulk$/ }).click();
      await expect(page.locator("section.pharos-browse h2")).toHaveText("Bulk", {
        timeout: 15_000,
      });
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);
    });

    // B500: an episode row's tick, the season and the show each mark
    // watched and unwatched, and a part-watched episode can play from the
    // start instead of resuming.
    test("the tab names the page, then Pharos", async ({ page, request }) => {
      test.setTimeout(90_000);
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-title-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
      const episodes = await (
        await request.get(
          `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Episode&Fields=SeriesId`,
          { headers: { "X-Emby-Token": token } },
        )
      ).json();
      const episode = (episodes.Items ?? []).find(
        (i: { SeriesName?: string }) => i.SeriesName === "Another Show",
      );
      test.skip(!episode, "pharos has no seeded Another Show episode");

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
      await expect(page).toHaveTitle("Pharos");

      await page.goto(`/ui/browse/${nativeSynth(episode.SeriesId)}`);
      await expect(page).toHaveTitle("Another Show · Pharos", { timeout: 15_000 });
      const nativeId = BigInt(`0x${episode.Id}`).toString();
      await page.goto(`/ui/title/${nativeId}`);
      await expect(page).toHaveTitle(/^.+ · Another Show · Pharos$/, { timeout: 15_000 });
      const named = await page.title();
      await page.goto(`/ui/watch/${nativeId}`);
      await expect(page).toHaveTitle(named, { timeout: 15_000 });
      await page.goto("/ui/settings/appearance");
      await expect(page).toHaveTitle("Appearance · Pharos", { timeout: 15_000 });
    });

    test("an episode, its season and its show mark watched, and play from start", async ({
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
      const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-mark-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
      const episodes = await (
        await request.get(
          `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Episode&Fields=SeriesId`,
          { headers: api },
        )
      ).json();
      const episode = (episodes.Items ?? []).find(
        (i: { SeriesName?: string }) => i.SeriesName === "Another Show",
      );
      test.skip(!episode, "pharos has no seeded Another Show episode");
      const played = async () =>
        (
          await (
            await request.get(`${PHAROS_URL}/Users/${user.Id}/Items/${episode.Id}`, {
              headers: api,
            })
          ).json()
        ).UserData?.Played;
      await request.delete(`${PHAROS_URL}/Users/${user.Id}/PlayedItems/${episode.Id}`, {
        headers: api,
      });

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });

      await page.goto(`/ui/browse/${nativeSynth(episode.SeriesId)}`);
      const show = page.locator("section.pharos-show");
      await expect(show.locator("h2")).toHaveText("Another Show", { timeout: 15_000 });
      const tick = show.locator("button.pharos-episode-mark").first();
      const season = show.locator(".pharos-season-bar button");
      const whole = show.getByRole("button", { name: /^Mark show (un)?watched$/ });
      await expect(season).toHaveText("Mark season watched");
      await expect(whole).toHaveText("Mark show watched");

      // The row's tick marks the one episode, so its season and show are
      // watched too.
      await tick.click();
      await expect(tick).toHaveClass(/is-watched/);
      await expect(season).toHaveText("Mark season unwatched");
      await expect(whole).toHaveText("Mark show unwatched");
      expect(await played()).toBe(true);

      await whole.click();
      await expect(tick).not.toHaveClass(/is-watched/);
      await expect(season).toHaveText("Mark season watched");
      expect(await played()).toBe(false);

      await season.click();
      await expect(tick).toHaveClass(/is-watched/);
      await season.click();
      await expect(tick).not.toHaveClass(/is-watched/);
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);

      // Part way through: the title offers Resume and Play from start, and
      // the latter starts at zero.
      await request.post(`${PHAROS_URL}/Sessions/Playing/Progress`, {
        headers: api,
        data: {
          ItemId: episode.Id,
          PlaySessionId: `pw-start-${episode.Id}`,
          PositionTicks: 25_000_000,
          IsPaused: true,
        },
      });
      const nativeId = BigInt(`0x${episode.Id}`).toString();
      await page.goto(`/ui/title/${nativeId}`);
      await expect(page.getByRole("link", { name: "Resume" })).toBeVisible({ timeout: 15_000 });
      await page.getByRole("button", { name: "Mark watched" }).click();
      await expect(page.getByRole("button", { name: "Mark unwatched" })).toBeVisible();
      await expect(page.getByRole("button", { name: "Play from start" })).toHaveCount(0);
      await page.getByRole("button", { name: "Mark unwatched" }).click();
      await expect(page.getByRole("link", { name: /^Play$/ })).toBeVisible();

      await request.post(`${PHAROS_URL}/Sessions/Playing/Progress`, {
        headers: api,
        data: {
          ItemId: episode.Id,
          PlaySessionId: `pw-start2-${episode.Id}`,
          PositionTicks: 25_000_000,
          IsPaused: true,
        },
      });
      await page.reload();
      await page.getByRole("button", { name: "Play from start" }).click({ timeout: 15_000 });
      await expect(page).toHaveURL(new RegExp(`/ui/watch/${nativeId}$`));
      const start = await page.evaluate(
        () =>
          new Promise<number>((resolve) => {
            const look = () => {
              const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
              if (v && v.readyState >= 1) resolve(v.currentTime);
              else setTimeout(look, 50);
            };
            look();
          }),
      );
      expect(start).toBeLessThan(1.5);

      // Leave the player first, so its stop report lands before the reset and
      // no later test finds this episode part watched.
      await page.goto("/ui/");
      await expect
        .poll(
          async () => {
            await request.delete(`${PHAROS_URL}/Users/${user.Id}/PlayedItems/${episode.Id}`, {
              headers: api,
            });
            const data = (
              await (
                await request.get(`${PHAROS_URL}/Users/${user.Id}/Items/${episode.Id}`, {
                  headers: api,
                })
              ).json()
            ).UserData;
            return [data?.Played, data?.PlaybackPositionTicks];
          },
          { timeout: 15_000 },
        )
        .toEqual([false, 0]);
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
        `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=AlbumArtist,Artists,Album`,
      );
      const track = tracks.Items?.find((t: Song) => t.Album && songBy(t));
      test.skip(!track, "the seed has no track tagged with an album and artist");
      const artists = await json(`/Artists`);
      const artist = artists.Items?.find((a: { Name: string }) => a.Name === songBy(track));
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
      await expect(page.locator(LOAD_ERRORS)).toHaveCount(0);
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

    test("search: the bar lists matches as you type, Enter opens them all, nonsense says no matches", async ({
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

      // The bar's box lists the first matches as you type; every seeded
      // title starts "Playwright …", so the term matches them.
      const bar = page.getByRole("combobox", { name: "Search all libraries" });
      await bar.fill("Playwright");
      const options = page.getByRole("option");
      await expect(options.first()).toBeVisible({ timeout: 15_000 });
      await bar.press("ArrowDown");
      await expect(options.first()).toHaveAttribute("aria-selected", "true");
      await bar.press("Escape");
      await expect(options).toHaveCount(0);

      // Enter with no match picked opens the palette on the same term,
      // with every match under Titles.
      await bar.press("Enter");
      const palette = page.getByRole("dialog", { name: "Go to or search" });
      await expect(palette).toBeVisible();
      const box = palette.getByRole("combobox");
      await expect(box).toHaveValue("Playwright");
      await expect(box).toBeFocused();
      // The term it opens on is selected, so typing replaces it (B623).
      await expect
        .poll(() =>
          box.evaluate((el: HTMLInputElement) => [el.selectionStart, el.selectionEnd]),
        )
        .toEqual([0, "Playwright".length]);
      await expect(palette.getByRole("group", { name: "Titles" })).toBeVisible({
        timeout: 15_000,
      });

      await box.fill("zzqx-no-such-title");
      await expect(palette.getByText("No matches")).toBeVisible({
        timeout: 15_000,
      });
      await box.press("Escape");
      await expect(palette).toHaveCount(0);
    });

    test("palette: Ctrl+K goes to a page by name, and an old /find link opens it", async ({
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

      const palette = page.getByRole("dialog", { name: "Go to or search" });
      await page.keyboard.press("Control+k");
      await expect(palette).toBeVisible();
      // Before anything is typed it lists the libraries to go to.
      await expect(palette.getByRole("option", { name: /^TV/ })).toBeVisible({
        timeout: 15_000,
      });
      const box = palette.getByRole("combobox");
      await expect(box).toBeFocused();
      await box.fill("appear");
      await box.press("Enter");
      await expect(page).toHaveURL(/\/ui\/settings\/appearance$/);
      await expect(palette).toHaveCount(0);

      // Cmd+K, as on macOS, toggles it too.
      await page.keyboard.press("Meta+k");
      await expect(palette).toBeVisible();
      await page.keyboard.press("Meta+k");
      await expect(palette).toHaveCount(0);

      await page.goto("/ui/find");
      await expect(page).toHaveURL(/\/ui\/$/, { timeout: 15_000 });
      await expect(palette).toBeVisible();
    });

    // V518: a show's name lists the show first, `s01` opens its season and
    // `s01e02` that episode; the arrows keep the picked row in sight.
    test("palette: a show, its season and its episode by name, and the pick stays in sight", async ({
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

      const palette = page.getByRole("dialog", { name: "Go to or search" });
      const box = palette.getByRole("combobox");
      const titles = palette.getByRole("group", { name: "Titles" }).getByRole("option");
      const open = async (text: string) => {
        await page.keyboard.press("Control+k");
        await expect(palette).toBeVisible();
        await box.fill(text);
      };

      await open("playwright show");
      await expect(titles.first()).toContainText("Playwright Show", { timeout: 15_000 });
      await expect(titles.first()).toContainText("Series");
      await box.press("Escape");

      await open("playwright show s1");
      await expect(titles.first()).toContainText("Season 1", { timeout: 15_000 });
      await expect(titles.first()).toContainText("Season · Playwright Show");
      await box.press("Enter");
      await expect(page).toHaveURL(/\/ui\/browse\//);
      await expect(page.locator("section.pharos-show h2")).toHaveText("Playwright Show", {
        timeout: 15_000,
      });

      await open("playwright show s01e2");
      await expect(titles.first()).toContainText("Episode", { timeout: 15_000 });
      await expect(titles.first()).toContainText("E2");
      await box.press("Enter");
      await expect(page).toHaveURL(/\/ui\/title\//);

      // Ctrl+K opens empty, offering the searches made, newest first; picking
      // one searches it again in place (V551).
      await page.keyboard.press("Control+k");
      await expect(box).toHaveValue("");
      const again = palette.getByRole("group", { name: "Recent searches" }).getByRole("option");
      await expect(again.first()).toHaveText(/playwright show s01e2/);
      await again.nth(1).click();
      await expect(box).toHaveValue("playwright show s1");
      await expect(titles.first()).toContainText("Season 1", { timeout: 15_000 });
      await box.press("Escape");

      // The Bulk titles overflow the list; walking down keeps the pick shown.
      await open("bulk");
      await expect(titles.nth(20)).toBeAttached({ timeout: 15_000 });
      for (let i = 0; i < 20; i++) await box.press("ArrowDown");
      const picked = palette.locator('[role=option][aria-selected="true"]');
      await expect(picked).toHaveCount(1);
      await expect(picked).toBeInViewport();
      await box.press("Escape");
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

    // V630: `?engine=rust` picks the Pharos engine for one page load. Until it
    // is built (spec 031 slice 2) the watch page says so and plays through
    // hls.js, so the override can never leave a viewer without playback.
    test("the Rust engine override plays and seeks without hls.js", async ({
      page,
      request,
    }) => {
      test.setTimeout(120_000);
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
      const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-engine-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
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
      const logs: string[] = [];
      page.on("console", (m) => logs.push(m.text()));
      const hlsjs: string[] = [];
      page.on("request", (r) => {
        if (r.url().includes("/assets/vendor/hls.min.js")) hlsjs.push(r.url());
      });

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });

      await page.goto(`/ui/watch/${nativeId}?engine=rust`);
      const verdict = "pharos: playback engine=rust reason=override";
      await expect
        .poll(() => logs.some((l) => l.includes(verdict)), { timeout: 15_000 })
        .toBe(true);
      await expect
        .poll(() => logs.some((l) => l.includes("pharos: playback engine=rust start")), {
          timeout: 30_000,
        })
        .toBe(true);
      const video = () =>
        page.evaluate(() => {
          const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
          return { t: v?.currentTime ?? 0, d: v?.duration ?? 0 };
        });
      await expect.poll(async () => (await video()).t, { timeout: 60_000 }).toBeGreaterThan(1);

      const { d } = await video();
      const target = Math.floor(d / 2);
      await page.evaluate((to) => {
        const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
        if (v) v.currentTime = to;
      }, target);
      await expect
        .poll(async () => (await video()).t, { timeout: 60_000 })
        .toBeGreaterThan(target + 1);

      expect(logs.filter((l) => l.includes("engine=rust fallback="))).toEqual([]);
      expect(logs.filter((l) => l.includes("playback error engine=rust"))).toEqual([]);
      expect(hlsjs).toEqual([]);
    });

    // V637 (spec 031 slice 3): a segment the server fails is retried after a
    // doubling backoff and playback carries on, each failure logged with its
    // cause, attempt and delay.
    test("the Rust engine retries a failed segment with backoff and keeps playing", async ({
      page,
      request,
    }) => {
      test.setTimeout(120_000);
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 405,
        `native API is not enabled (status ${probe.status()})`,
      );
      const auth = await request.post(
        `${PHAROS_URL}/Users/AuthenticateByName`,
        {
          headers: {
            "Content-Type": "application/json",
            "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-retry-${Date.now()}", Version="0"`,
          },
          data: { Username: TEST_USER, Pw: TEST_PASS },
        },
      );
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      const fixture = async () =>
        (
          await (
            await request.get(
              `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path,MediaSources`,
              { headers: api },
            )
          ).json()
        ).Items?.find((i: { Path?: string }) =>
          i.Path?.endsWith("/fixture-1.webm"),
        );
      await expect
        .poll(async () => (await fixture())?.MediaSources?.[0]?.Container, {
          timeout: 30_000,
        })
        .toMatch(/webm/);
      const movie = await fixture();
      test.skip(!movie, "pharos has no seeded movie");
      const nativeId = BigInt(`0x${movie.Id}`).toString();
      // An earlier test's seek leaves a resume point; start from the top.
      await request.post(`${PHAROS_URL}/Sessions/Playing/Progress`, {
        headers: api,
        data: {
          ItemId: movie.Id,
          PlaySessionId: `pw-reset-${Date.now()}`,
          PositionTicks: 0,
          IsPaused: true,
        },
      });

      await page.addInitScript(() => {
        const canPlayType = HTMLMediaElement.prototype.canPlayType;
        HTMLMediaElement.prototype.canPlayType = function (type: string) {
          return /mpegurl|webm|matroska/i.test(type)
            ? ""
            : canPlayType.call(this, type);
        };
      });
      const logs: string[] = [];
      page.on("console", (m) => logs.push(m.text()));
      let refused = 0;
      await page.route(
        (url) => /\/h264cmaf\/(?:[^/]+\/)?\d+\.m4s$/.test(url.pathname),
        async (route) => {
          if (refused < 2) {
            refused += 1;
            await route.fulfill({ status: 503, body: "busy" });
          } else {
            await route.continue();
          }
        },
      );

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });

      await page.goto(`/ui/watch/${nativeId}?engine=rust`);
      await expect
        .poll(
          () =>
            logs.some((l) =>
              /pharos: playback engine=rust start .* device=desktop first_byte_ms=10000 load_ms=60000 /.test(
                l,
              ),
            ),
          { timeout: 30_000 },
        )
        .toBe(true);
      const failures = () =>
        logs.filter((l) => l.includes("playback error engine=rust"));
      await expect.poll(() => failures().length, { timeout: 30_000 }).toBe(2);
      expect(failures()[0]).toMatch(
        /track=video index=\d+ cause=http status=503 deadline_ms=none rung=\S+ action=retry attempt=1 after_ms=1000 give_up=none /,
      );
      expect(failures()[1]).toMatch(
        /index=\d+ cause=http status=503 .*action=retry attempt=2 after_ms=2000 /,
      );

      const video = () =>
        page.evaluate(() => {
          const v = document.getElementById(
            "pharos-video",
          ) as HTMLVideoElement | null;
          return v?.currentTime ?? 0;
        });
      await expect.poll(video, { timeout: 60_000 }).toBeGreaterThan(13);
      expect(refused).toBe(2);
      expect(failures()).toHaveLength(2);
      expect(logs.filter((l) => l.includes("action=fail"))).toEqual([]);
      expect(logs.filter((l) => l.includes("engine=rust fallback="))).toEqual(
        [],
      );
    });

    // B733: after the back buffer is trimmed, a seek to just before where the
    // remaining data starts lands in a segment that is only partly buffered.
    // The Rust engine must fetch it again, not wait on data it never asks for.
    test("the Rust engine plays on after a seek back to the back buffer's edge", async ({
      page,
      request,
    }) => {
      test.setTimeout(180_000);
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 405,
        `native API is not enabled (status ${probe.status()})`,
      );
      const auth = await request.post(
        `${PHAROS_URL}/Users/AuthenticateByName`,
        {
          headers: {
            "Content-Type": "application/json",
            "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-seekback-${Date.now()}", Version="0"`,
          },
          data: { Username: TEST_USER, Pw: TEST_PASS },
        },
      );
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      const fixture = async () =>
        (
          await (
            await request.get(
              `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path,MediaSources`,
              { headers: api },
            )
          ).json()
        ).Items?.find((i: { Path?: string }) =>
          i.Path?.endsWith("/fixture-1.webm"),
        );
      await expect
        .poll(async () => (await fixture())?.MediaSources?.[0]?.Container, {
          timeout: 30_000,
        })
        .toMatch(/webm/);
      const movie = await fixture();
      test.skip(!movie, "pharos has no seeded movie");
      const nativeId = BigInt(`0x${movie.Id}`).toString();
      // An earlier test's seek leaves a resume point; start from the top.
      await request.post(`${PHAROS_URL}/Sessions/Playing/Progress`, {
        headers: api,
        data: {
          ItemId: movie.Id,
          PlaySessionId: `pw-reset-${Date.now()}`,
          PositionTicks: 0,
          IsPaused: true,
        },
      });

      await page.addInitScript(() => {
        const canPlayType = HTMLMediaElement.prototype.canPlayType;
        HTMLMediaElement.prototype.canPlayType = function (type: string) {
          return /mpegurl|webm|matroska/i.test(type)
            ? ""
            : canPlayType.call(this, type);
        };
      });
      const logs: string[] = [];
      page.on("console", (m) => logs.push(m.text()));
      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });

      await page.goto(`/ui/watch/${nativeId}?engine=rust`);
      const now = () =>
        page.evaluate(
          () =>
            (document.getElementById("pharos-video") as HTMLVideoElement | null)
              ?.currentTime ?? 0,
        );
      // At 34 s the 15 s back buffer's cutoff (19 s) sits in the first half of
      // the 18-24 s segment, so the audio track keeps that segment's second half.
      await expect
        .poll(now, { timeout: 90_000, intervals: [50] })
        .toBeGreaterThan(34);
      const target = (await now()) - 15.6;
      await page.evaluate((to) => {
        const v = document.getElementById(
          "pharos-video",
        ) as HTMLVideoElement | null;
        if (v) v.currentTime = to;
      }, target);
      await expect.poll(now, { timeout: 15_000 }).toBeGreaterThan(target + 1);

      expect(logs.filter((l) => l.includes("engine=rust fallback="))).toEqual(
        [],
      );
      expect(
        logs.filter((l) => l.includes("playback error engine=rust")),
      ).toEqual([]);
    });

    // V701/V702 (spec 031 slice 3, T031-15): a small hole in the buffer stalls
    // the browser; the Rust engine's watchdog jumps it and logs the jump. The
    // test cuts 0.4 s of audio out of a segment ahead of the play head. Opus
    // frames are all keyframes, so the cut is exact, and the segment's
    // midpoint stays buffered, so the planner does not fetch it again.
    test("the Rust engine jumps a small hole in the buffer", async ({
      page,
      request,
      browserName,
    }) => {
      test.setTimeout(120_000);
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
      const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-gap-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
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
      // An earlier test's seek leaves a resume point; start from the top.
      await request.post(`${PHAROS_URL}/Sessions/Playing/Progress`, {
        headers: api,
        data: {
          ItemId: movie.Id,
          PlaySessionId: `pw-reset-${Date.now()}`,
          PositionTicks: 0,
          IsPaused: true,
        },
      });

      await page.addInitScript(() => {
        const canPlayType = HTMLMediaElement.prototype.canPlayType;
        HTMLMediaElement.prototype.canPlayType = function (type: string) {
          return /mpegurl|webm|matroska/i.test(type) ? "" : canPlayType.call(this, type);
        };
        const add = MediaSource.prototype.addSourceBuffer;
        MediaSource.prototype.addSourceBuffer = function (mime: string) {
          const sb = add.call(this, mime);
          if (/^audio\//.test(mime)) (window as unknown as { pwAudio: SourceBuffer }).pwAudio = sb;
          return sb;
        };
      });
      const logs: string[] = [];
      page.on("console", (m) => logs.push(m.text()));

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });

      await page.goto(`/ui/watch/${nativeId}?engine=rust`);
      const video = () =>
        page.evaluate(() => {
          const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
          const a = (window as unknown as { pwAudio?: SourceBuffer }).pwAudio;
          const r = a?.buffered;
          return {
            t: v?.currentTime ?? 0,
            audioEnd: r && r.length > 0 ? r.end(r.length - 1) : 0,
            updating: a?.updating ?? true,
          };
        });
      await expect.poll(async () => (await video()).t, { timeout: 60_000 }).toBeGreaterThan(1);

      // The next segment boundary past the play head, with that whole segment buffered.
      const hole = Math.ceil(((await video()).t + 1) / 6) * 6 + 1;
      await expect
        .poll(
          async () => {
            const v = await video();
            return v.audioEnd > hole + 3 && !v.updating;
          },
          { timeout: 60_000 },
        )
        .toBe(true);
      await page.evaluate((from) => {
        (window as unknown as { pwAudio: SourceBuffer }).pwAudio.remove(from, from + 0.4);
      }, hole);

      const stalls = () => logs.filter((l) => l.includes("pharos: playback stall")).join("\n");
      await expect
        .poll(async () => (await video()).t, { timeout: 30_000 })
        .toBeGreaterThan(hole + 1);
      // Firefox plays through a short audio hole by itself; Chromium stalls
      // at it, so there the watchdog has to jump it.
      if (browserName === "chromium") {
        expect(
          logs.some((l) => /pharos: playback stall engine=rust action=jump .*gap_ms=\d+/.test(l)),
          stalls(),
        ).toBe(true);
        await expect
          .poll(() => logs.some((l) => l.includes("playback stall engine=rust action=recovered")), {
            timeout: 5_000,
          })
          .toBe(true);
      }
      expect(logs.filter((l) => l.includes("action=give_up"))).toEqual([]);
      expect(logs.filter((l) => l.includes("engine=rust fallback="))).toEqual([]);
      expect(logs.filter((l) => l.includes("playback error engine=rust"))).toEqual([]);
    });

    // V648/V649 (spec 031 slice 4): on a link that slows mid-play the Rust
    // engine steps down a rung at a segment boundary, with no reload and no
    // jump in currentTime, and logs the switch with what it decided on. The
    // link is slowed by a local proxy that trickles each segment's body, so
    // the engine's first-to-last-byte estimate sees a slow link in Chromium
    // and Firefox alike. The seeded clip is too small for the server to list
    // a ladder (its listing is covered by the server's handler tests), so the
    // test adds the 360p rung, which the server serves for any item, to the
    // abr=1 master and gives the source rung a bandwidth above the link's.
    test("the Rust engine steps down a quality rung when the link slows", async ({
      page,
      request,
    }) => {
      test.setTimeout(180_000);
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
      const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-abr-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
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
      // An earlier test's seek leaves a resume point; start from the top.
      await request.post(`${PHAROS_URL}/Sessions/Playing/Progress`, {
        headers: api,
        data: {
          ItemId: movie.Id,
          PlaySessionId: `pw-reset-${Date.now()}`,
          PositionTicks: 0,
          IsPaused: true,
        },
      });

      // Bytes per second each segment body is trickled at; 0 is full speed.
      let rateBps = 0;
      const http = await import("node:http");
      const proxy = http.createServer(async (req, res) => {
        try {
          // The page signs its requests with its session cookie.
          const upstream = await fetch(`${PHAROS_URL}${req.url}`, {
            headers: req.headers.cookie ? { cookie: req.headers.cookie } : {},
          });
          const body = Buffer.from(await upstream.arrayBuffer());
          res.writeHead(upstream.status, {
            "content-type": upstream.headers.get("content-type") ?? "video/mp4",
            "access-control-allow-origin": "*",
          });
          const chunk = 16_384;
          for (let at = 0; at < body.length; at += chunk) {
            res.write(body.subarray(at, at + chunk));
            if (rateBps > 0) {
              await new Promise((r) => setTimeout(r, (chunk / rateBps) * 1000));
            }
          }
          res.end();
        } catch (e) {
          res.destroy(e as Error);
        }
      });
      await new Promise<void>((r) => proxy.listen(0, "127.0.0.1", r));
      const port = (proxy.address() as { port: number }).port;
      const segments: string[] = [];
      await page.route(/\/h264cmaf\/.*\.m4s/, (route) => {
        const u = new URL(route.request().url());
        segments.push(u.pathname);
        return route.continue({ url: `http://127.0.0.1:${port}${u.pathname}${u.search}` });
      });

      let ladder = "";
      await page.route(/\/master\.m3u8\?.*abr=1/, async (route) => {
        const res = await route.fetch();
        const lines = (await res.text()).split("\n");
        const at = lines.findIndex((l) => l.startsWith("#EXT-X-STREAM-INF:"));
        const uri = lines[at + 1] ?? "";
        if (at < 0 || !uri.includes("/h264cmaf/main.m3u8")) {
          ladder = `no h264cmaf rung: ${lines.join(" | ")}`;
          return route.fulfill({ response: res, body: lines.join("\n") });
        }
        const inf = lines[at];
        lines[at] = inf.replace(/BANDWIDTH=\d+/, "BANDWIDTH=4000000");
        lines.splice(
          at + 2,
          0,
          inf.replace(/BANDWIDTH=\d+/, "BANDWIDTH=500000").replace(/,RESOLUTION=\S+/, "") +
            ",RESOLUTION=640x360",
          uri.replace("/h264cmaf/main.m3u8", "/h264cmaf/360p/main.m3u8"),
        );
        ladder = "ok";
        return route.fulfill({ response: res, body: lines.join("\n") });
      });

      try {
        await page.addInitScript(() => {
          const canPlayType = HTMLMediaElement.prototype.canPlayType;
          HTMLMediaElement.prototype.canPlayType = function (type: string) {
            return /mpegurl|webm|matroska/i.test(type) ? "" : canPlayType.call(this, type);
          };
        });
        const logs: string[] = [];
        page.on("console", (m) => logs.push(m.text()));

        await page.goto("/ui/");
        await page.locator("#signin-name").fill(TEST_USER);
        await page.locator("#signin-password").fill(TEST_PASS);
        await page.getByRole("button", { name: "Sign in" }).click();
        await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });

        await page.goto(`/ui/watch/${nativeId}?engine=rust`);
        await expect
          .poll(() => logs.find((l) => l.includes("pharos: abr o")), { timeout: 60_000 })
          .toBeTruthy();
        expect(ladder).toBe("ok");
        const abrLine = logs.find((l) => l.includes("pharos: abr o")) ?? "";
        expect(abrLine, logs.filter((l) => l.includes("abr")).join("\n")).toContain(
          "pharos: abr on rungs=360p,240p current=240p",
        );

        const video = () =>
          page.evaluate(() => {
            const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
            return v?.currentTime ?? 0;
          });
        await expect.poll(video, { timeout: 60_000 }).toBeGreaterThan(1);

        // The link drops to 800 kbit/s: room for the 360p rung, not the source.
        rateBps = 100_000;
        const switched = () => logs.find((l) => l.includes("pharos: abr switch"));
        await expect.poll(switched, { timeout: 120_000 }).toBeTruthy();
        const line = switched() ?? "";
        expect(line).toMatch(
          /pharos: abr switch from=\S+ to=\S+ reason=(bandwidth|buffer) est_kbps=\d+ buffer_ms=\d+ cold_ms=\d+ applied=true/,
        );
        const to = /to=(\S+)/.exec(line)?.[1] ?? "";

        // The next segments come from the new rung, without a reload, and
        // the play head keeps going from where it was.
        const before = await video();
        await expect
          .poll(() => segments.some((p) => p.includes(`/h264cmaf/${to}/`)), {
            timeout: 60_000,
          })
          .toBe(true);
        await expect.poll(video, { timeout: 60_000 }).toBeGreaterThan(before + 1);
        expect(logs.filter((l) => l.includes("pharos: playback engine=rust start"))).toHaveLength(
          1,
        );
        expect(logs.filter((l) => l.includes("engine=rust fallback="))).toEqual([]);
        expect(logs.filter((l) => l.includes("playback error engine=rust"))).toEqual([]);
      } finally {
        rateBps = 0;
        await page.unrouteAll({ behavior: "ignoreErrors" });
        await new Promise((r) => proxy.close(r));
      }
    });

    // B551: a seek that waits on the server shows a spinner over the video,
    // outside the chrome that hides on idle, and it goes once data arrives.
    test("a seek that waits on the server shows a loading spinner", async ({ page, request }) => {
      test.setTimeout(120_000);
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
      const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-spin-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
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
        .poll(async () => (await fixture())?.MediaSources?.[0]?.Container, { timeout: 30_000 })
        .toMatch(/webm/);
      const movie = await fixture();
      test.skip(!movie, "pharos has no seeded movie");
      const nativeId = BigInt(`0x${movie.Id}`).toString();
      // Force the HLS path, as the playback test above does.
      await page.addInitScript(() => {
        const canPlayType = HTMLMediaElement.prototype.canPlayType;
        HTMLMediaElement.prototype.canPlayType = function (type: string) {
          return /mpegurl|webm|matroska/i.test(type) ? "" : canPlayType.call(this, type);
        };
      });
      // Hold every segment after the first until released: the seek below
      // then has to wait on the "server".
      let release: () => void = () => {};
      const released = new Promise<void>((r) => (release = r));
      const segment = /\/videos\/[^?]*\/a?(\d+)\.(?:ts|m4s)(?:\?|$)/;
      await page.route(segment, async (route) => {
        const seg = Number(segment.exec(route.request().url())?.[1] ?? 0);
        if (seg > 0) await released;
        await route.continue();
      });

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
      await page.goto(`/ui/watch/${nativeId}`);
      const video = () =>
        page.evaluate(() => {
          const v = document.getElementById("pharos-video") as HTMLVideoElement | null;
          return { duration: v?.duration || 0, time: v?.currentTime ?? 0 };
        });
      await expect.poll(async () => (await video()).duration, { timeout: 30_000 }).toBeGreaterThan(12);
      const spinner = page.locator(".pharos-watch-loading");

      const { duration } = await video();
      await page.evaluate((to) => {
        (document.getElementById("pharos-video") as HTMLVideoElement).currentTime = to;
      }, Math.floor(duration / 2));
      await expect(spinner).toBeVisible({ timeout: 5_000 });
      await expect(spinner).toHaveAttribute("role", "status");
      // Still there once the idle chrome has hidden.
      await page.waitForTimeout(3_500);
      await expect(spinner).toBeVisible();

      release();
      await expect(spinner).toHaveCount(0, { timeout: 30_000 });
    });

    // 013 US2/FR-003: the watch page draws its own controls. A text subtitle
    // switches in place as a WebVTT track the server converts; volume, mute,
    // play/pause and the seek bar drive the <video>.
    test("the watch controls switch subtitles in place and drive the video", async ({
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-controls-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const api = { "X-Emby-Token": token };
      const subtitled = async () =>
        (
          await (
            await request.get(
              `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path`,
              { headers: api },
            )
          ).json()
        ).Items?.find((i: { Path?: string }) => i.Path?.endsWith("/07-test-subtitles.webm"));
      // The startup rescan probes the fixture; its streams appear once it has.
      const subtitleStreams = async () => {
        const item = await subtitled();
        if (!item) return [];
        const info = await request.post(`${PHAROS_URL}/Items/${item.Id}/PlaybackInfo`, {
          headers: api,
          data: {},
        });
        return ((await info.json()).MediaSources?.[0]?.MediaStreams ?? []).filter(
          (s: { Type: string }) => s.Type === "Subtitle",
        );
      };
      await expect
        .poll(async () => (await subtitleStreams()).length, { timeout: 60_000 })
        .toBeGreaterThan(0);
      const movie = await subtitled();
      const nativeId = BigInt(`0x${movie.Id}`).toString();
      const problems: string[] = [];
      page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
      await page.goto(`/ui/watch/${nativeId}`);

      const video = page.locator("#pharos-video");
      await expect(page.locator(".pharos-controls")).toBeAttached({ timeout: 30_000 });
      await expect(video).not.toHaveAttribute("controls", /.*/);
      const media = () =>
        page.evaluate(() => {
          const v = document.getElementById("pharos-video") as HTMLVideoElement;
          return {
            time: v.currentTime,
            duration: v.duration,
            paused: v.paused,
            volume: v.volume,
            muted: v.muted,
            // Loaded tracks: "showing" is the browser drawing the cues; "hidden"
            // is pharos-subs drawing them on a canvas (spec 026), which falls
            // back to "showing" where WebGL2 is missing.
            active: Array.from(v.textTracks)
              .filter((t) => t.mode !== "disabled")
              .map((t) => ({ id: t.id, mode: t.mode, cues: t.cues?.length ?? 0 })),
          };
        });
      await expect.poll(async () => (await media()).duration, { timeout: 30_000 }).toBeGreaterThan(0);

      // Subtitles: off at first (the fixture's track is not default), then on
      // in place, with cues fetched from the server's WebVTT route.
      const picker = page.getByRole("combobox", { name: "Subtitles" });
      const option = await picker.locator("option").nth(1).getAttribute("value");
      expect(option, "a subtitle track is offered").toMatch(/^\d+$/);
      const before = page.url();
      await picker.selectOption(option!);
      await expect
        .poll(async () => (await media()).active, { timeout: 15_000 })
        .toEqual([expect.objectContaining({ id: `pharos-sub-${option}` })]);
      await expect
        .poll(async () => (await media()).active[0]?.cues ?? 0, { timeout: 15_000 })
        .toBeGreaterThan(0);
      expect(page.url(), "a text subtitle does not leave the page").toBe(before);
      // Drawn by pharos-subs: the track is hidden and a canvas follows the
      // video. Otherwise the renderer fell back and the track shows.
      const drawnByRust = (await media()).active[0]?.mode === "hidden";
      if (drawnByRust) {
        await expect(page.locator("canvas.pharos-rust-subs")).toBeAttached();
      } else {
        await expect(page.locator("canvas.pharos-rust-subs")).toHaveCount(0);
      }
      // In a phone-wide player a cue that wraps ends a line above the
      // picture's edge too: lifted past -2 (V455).
      const viewport = page.viewportSize();
      await page.setViewportSize({ width: 360, height: 640 });
      const lifted = drawnByRust
        ? null
        : await page.evaluate(async (id) => {
        const v = document.getElementById("pharos-video") as HTMLVideoElement;
        const track = Array.from(v.textTracks).find((t) => t.id === id)!;
        const cue = new VTTCue(
          v.currentTime,
          v.currentTime + 60,
          "a subtitle line long enough to wrap in a phone-sized player",
        );
        track.addCue(cue);
        for (let i = 0; i < 50 && cue.line === "auto"; i++) {
          await new Promise((r) => setTimeout(r, 100));
        }
        const line = cue.line;
        track.removeCue(cue);
        return line;
      }, `pharos-sub-${option}`);
      if (!drawnByRust) expect(lifted).toBeLessThanOrEqual(-3);
      if (viewport) await page.setViewportSize(viewport);
      await picker.selectOption("off");
      await expect.poll(async () => (await media()).active).toEqual([]);
      await expect(page.locator("canvas.pharos-rust-subs")).toHaveCount(0);

      // The chrome fades soon after the last activity while playing, even
      // with a track just picked or a control focused by a key press (B430).
      const chromeOpacity = () =>
        page.evaluate(
          () => getComputedStyle(document.querySelector(".pharos-controls")!).opacity,
        );
      if ((await media()).paused) {
        await page.locator(".pharos-control-row .pharos-control").first().click();
      }
      await expect.poll(async () => (await media()).paused).toBe(false);
      await page.mouse.move(20, 20);
      await expect.poll(chromeOpacity, { message: "a pointer move shows the chrome" }).toBe("1");
      await picker.selectOption(option!);
      await expect.poll(chromeOpacity, { timeout: 4_000 }).toBe("0");
      await picker.selectOption("off");
      // Tab moves focus by keyboard, so the control it lands on is
      // :focus-visible; a modifier alone would not make it so.
      await page.getByRole("button", { name: "Mute" }).focus();
      await page.keyboard.press("Tab");
      await expect
        .poll(() =>
          page.evaluate(() => document.activeElement?.matches(".pharos-controls :focus-visible")),
        )
        .toBe(true);
      await expect.poll(chromeOpacity, { message: "a key press shows the chrome" }).toBe("1");
      await expect.poll(chromeOpacity, { timeout: 4_000 }).toBe("0");
      await page.mouse.move(40, 40);
      await expect.poll(chromeOpacity).toBe("1");

      // Volume and mute.
      await page.locator(".pharos-volume").fill("0.5");
      await expect.poll(async () => (await media()).volume).toBeCloseTo(0.5, 2);
      await page.getByRole("button", { name: "Mute" }).click();
      await expect.poll(async () => (await media()).muted).toBe(true);
      await page.getByRole("button", { name: "Unmute" }).click();
      await expect.poll(async () => (await media()).muted).toBe(false);

      // Play/pause through the control, not the element.
      const toggle = page.locator(".pharos-control-row .pharos-control").first();
      if (!(await media()).paused) {
        await toggle.click();
      }
      await expect.poll(async () => (await media()).paused).toBe(true);

      // The seek bar: hovering previews a time, a click seeks there.
      const seek = page.locator("#pharos-seek");
      const box = (await seek.boundingBox())!;
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await expect(page.locator(".pharos-seek-time")).toBeVisible();
      await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
      await expect
        .poll(async () => {
          const m = await media();
          return Math.abs(m.time - m.duration / 2);
        })
        .toBeLessThan(1.5);
      expect(problems, problems.join("\n")).toEqual([]);
    });

    // A track the <video> cannot switch itself (another audio track, an image
    // subtitle to burn) negotiates a new playback. The player must load the
    // new session's stream, not keep playing the old one.
    test("picking another audio track loads the new playback's stream", async ({
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-renegotiate-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const dualAudio = async () =>
        (
          await (
            await request.get(
              `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path`,
              { headers: { "X-Emby-Token": token } },
            )
          ).json()
        ).Items?.find((i: { Path?: string }) => i.Path?.endsWith("/08-test-dual-audio.webm"));
      await expect.poll(dualAudio, { timeout: 60_000 }).toBeTruthy();
      const movie = await dualAudio();
      const nativeId = BigInt(`0x${movie.Id}`).toString();
      const problems: string[] = [];
      page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));
      // Every stream URL names its play session: `tag=` on direct play,
      // `PlaySessionId=` on a transcode.
      const sessions = new Set<string>();
      page.on("request", (r) => {
        const url = new URL(r.url());
        if (!url.pathname.toLowerCase().startsWith(`/videos/${movie.Id}/`)) return;
        const session = url.searchParams.get("tag") ?? url.searchParams.get("PlaySessionId");
        if (session) sessions.add(session);
      });

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
      await page.goto(`/ui/watch/${nativeId}`);
      await expect(page.locator(".pharos-controls")).toBeAttached({ timeout: 30_000 });
      await expect.poll(() => sessions.size, { timeout: 30_000 }).toBe(1);

      const picker = page.getByRole("combobox", { name: "Audio" });
      const second = await picker.locator("option").nth(1).getAttribute("value");
      expect(second, "a second audio track is offered").toMatch(/^\d+$/);
      await picker.selectOption(second!);
      await expect
        .poll(() => sessions.size, {
          message: "the player loads the renegotiated session's stream",
          timeout: 30_000,
        })
        .toBe(2);
      await expect(picker).toHaveValue(second!);
      expect(problems, problems.join("\n")).toEqual([]);
    });

    // The title page offers the tracks before playback, named by language
    // rather than by code, and the player starts on the one picked there.
    test("the title page picks the audio track the player starts on", async ({
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-preplay-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const dualAudio = async () =>
        (
          await (
            await request.get(
              `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path`,
              { headers: { "X-Emby-Token": token } },
            )
          ).json()
        ).Items?.find((i: { Path?: string }) => i.Path?.endsWith("/08-test-dual-audio.webm"));
      await expect.poll(dualAudio, { timeout: 60_000 }).toBeTruthy();
      const movie = await dualAudio();
      const nativeId = BigInt(`0x${movie.Id}`).toString();
      const problems: string[] = [];
      page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));
      // Nothing plays aloud: every media element is muted before it starts.
      await page.addInitScript(() => {
        const play = HTMLMediaElement.prototype.play;
        HTMLMediaElement.prototype.play = function (this: HTMLMediaElement) {
          this.muted = true;
          return play.call(this);
        };
      });
      const sessions = new Set<string>();
      page.on("request", (r) => {
        const url = new URL(r.url());
        if (!url.pathname.toLowerCase().startsWith(`/videos/${movie.Id}/`)) return;
        const session = url.searchParams.get("tag") ?? url.searchParams.get("PlaySessionId");
        if (session) sessions.add(session);
      });

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
      await page.goto(`/ui/title/${nativeId}`);
      const before = page.locator(".pharos-title-tracks").getByRole("combobox", { name: "Audio" });
      await expect(before).toBeVisible({ timeout: 30_000 });
      const labels = (await before.locator("option").allTextContents()).join(" | ");
      expect(labels).toMatch(/English/);
      expect(labels).toMatch(/Japanese/);
      expect(labels, "tracks are named by language, not by code").not.toMatch(/\b(eng|jpn)\b/i);
      const japanese = await before
        .locator("option", { hasText: "Japanese" })
        .getAttribute("value");
      await before.selectOption(japanese!);
      await page.locator(".pharos-title .pharos-actions a.pharos-button").first().click();

      await expect(page.locator(".pharos-controls")).toBeAttached({ timeout: 30_000 });
      await expect.poll(() => sessions.size, { timeout: 30_000 }).toBe(1);
      const picker = page.getByRole("combobox", { name: "Audio" });
      await expect(picker, "the player starts on the track picked before it").toHaveValue(
        japanese!,
      );
      expect(problems, problems.join("\n")).toEqual([]);
    });

    // Spec 019: an image subtitle (DVD here) is drawn by the player from the
    // server's PNGs, so turning it on keeps the playback it has: no new
    // session, no burn. Disarm check: with `subtitle_switch` renegotiating
    // for an image track, picking it reloads the player and the cue is gone.
    test("an image subtitle is drawn over the same playback", async ({ page, request }) => {
      test.setTimeout(180_000);
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
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-image-subs-${Date.now()}", Version="0"`,
        },
        data: { Username: TEST_USER, Pw: TEST_PASS },
      });
      test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
      const { AccessToken: token, User: user } = await auth.json();
      const dvdSubs = async () =>
        (
          await (
            await request.get(
              `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Movie&Fields=Path`,
              { headers: { "X-Emby-Token": token } },
            )
          ).json()
        ).Items?.find((i: { Path?: string }) => i.Path?.endsWith("/09-test-dvd-subtitle.mkv"));
      await expect.poll(dvdSubs, { timeout: 60_000 }).toBeTruthy();
      const movie = await dvdSubs();
      const nativeId = BigInt(`0x${movie.Id}`).toString();
      const problems: string[] = [];
      page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));
      const sessions = new Set<string>();
      page.on("request", (r) => {
        const url = new URL(r.url());
        if (!url.pathname.toLowerCase().startsWith(`/videos/${movie.Id}/`)) return;
        const session = url.searchParams.get("tag") ?? url.searchParams.get("PlaySessionId");
        if (session) sessions.add(session);
      });

      await page.goto("/ui/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
      await page.goto(`/ui/watch/${nativeId}`);
      await expect(page.locator(".pharos-controls")).toBeAttached({ timeout: 30_000 });
      await expect.poll(() => sessions.size, { timeout: 30_000 }).toBe(1);

      // The chrome takes no pointer events while idle.
      await page.locator("#pharos-player").hover();
      const picker = page.getByRole("combobox", { name: "Subtitles" });
      const dvd = await picker.locator("option").nth(1).getAttribute("value");
      expect(dvd, "the DVD subtitle is offered").toMatch(/^\d+$/);
      await picker.selectOption(dvd!);
      await expect(picker).toHaveValue(dvd!);

      const at = (seconds: number) =>
        page.evaluate((s) => {
          const v = document.getElementById("pharos-video") as HTMLVideoElement;
          v.pause();
          v.currentTime = s;
        }, seconds);
      const shown = page.locator(".pharos-image-sub");
      // The first cue is on from 0.5 s to 2.5 s, the second from 4 s to 6 s.
      await at(1);
      await expect(shown, "the first cue is drawn").toHaveCount(1, { timeout: 60_000 });
      await expect(shown).toHaveAttribute("src", new RegExp(`/subtitles/${dvd}/images/0\\.png\\?v=\\d+-\\d+$`));
      await expect
        .poll(() => shown.evaluate((img: HTMLImageElement) => img.naturalWidth), {
          message: "the cue's PNG loads",
        })
        .toBeGreaterThan(0);
      await at(3);
      await expect(shown, "nothing between cues").toHaveCount(0);
      await at(5);
      await expect(shown).toHaveAttribute("src", /\/images\/1\.png\?v=\d+-\d+$/);
      expect(sessions.size, "turning the subtitle on kept the playback").toBe(1);
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
      // B806: the failure panel names what happened in plain words and keeps
      // the raw reason as its details line.
      await expect(page.getByText(/This video stopped playing\./).first()).toBeVisible({
        timeout: 30_000,
      });
      await expect(page.getByText(/the stream failed/).first()).toBeVisible();
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
      await page.getByRole("button", { name: "Account", exact: true }).click();
      await page.getByRole("menuitem", { name: "Settings" }).click();
      await expect(page.getByRole("menu")).toHaveCount(0);
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

    await page
      .getByRole("combobox", { name: "Search all libraries" })
      .fill("carefree");
    const hit = page.getByRole("option").first().getByRole("link");
    await expect(hit).toBeVisible({ timeout: 15_000 });
    await hit.click();
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

    // The screen's own controls drive the <audio>; the browser's are off.
    const audio = page.locator("#pharos-video");
    await expect(audio).not.toHaveAttribute("controls", /.*/);
    await page.getByRole("button", { name: "Pause" }).click();
    await expect
      .poll(() => audio.evaluate((a) => (a as HTMLMediaElement).paused))
      .toBe(true);
    await expect(page.getByRole("button", { name: "Play" })).toBeVisible();
    await page.getByRole("button", { name: "Play" }).click();
    await expect(page.getByRole("button", { name: "Pause" })).toBeVisible();

    // One button in the middle of the top strip opens a grid of every
    // style; each style's shader compiles (B482/V426).
    const trigger = page.locator("#pharos-style-trigger");
    const box = await trigger.boundingBox();
    const width = page.viewportSize()?.width ?? 0;
    expect(Math.abs((box?.x ?? 0) + (box?.width ?? 0) / 2 - width / 2)).toBeLessThan(2);
    await trigger.click();
    const picker = page.getByRole("group", { name: "Visualiser" });
    await expect(picker).toBeVisible();
    const styles = await picker.getByRole("button").allTextContents();
    expect(styles.length).toBeGreaterThanOrEqual(9);
    // Spectrum, the default, logged its decision above.
    for (const name of styles.filter((s) => s !== "Off" && s !== "Spectrum")) {
      lines.length = 0;
      await picker.getByRole("button", { name, exact: true }).click();
      const decided = new RegExp(`pharos: renderer mode=\\S+ reason=\\S+ style=${name.toLowerCase()} `);
      await expect
        .poll(() => lines.find((l) => decided.test(l)) ?? "", { timeout: 15_000 })
        .toMatch(decided);
      expect(lines.find((l) => decided.test(l))).not.toMatch(/reason=shader/);
    }
    await picker.getByRole("button", { name: "Halo", exact: true }).click();
    await expect(
      picker.getByRole("button", { name: "Halo", exact: true }),
    ).toHaveAttribute("aria-pressed", "true");
    await expect
      .poll(() =>
        page.evaluate(() => localStorage.getItem("pharos.visualiser")),
      )
      .toBe("halo");
    // Escape closes the grid and leaves the song playing.
    await page.keyboard.press("Escape");
    await expect(picker).toBeHidden();
    await expect(trigger).toContainText("Halo");
    await expect(page.getByRole("button", { name: "Pause" })).toBeVisible();
  });
});

// B734 / V682: a song is routed through its analyser once it plays, with no
// visualiser drawing, so its tap's running context keeps the output awake
// through a pause (V683). B735: the resume names the item, how long it was
// paused, the wake and the browser.
test.describe("pharos Dioxus UI music resume", () => {
  test("a song with the visualiser off is tapped, and its resume says so", async ({
    page,
    request,
  }) => {
    test.setTimeout(90_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    const lines: string[] = [];
    page.on("console", (m) => lines.push(m.text()));

    await page.goto("/ui/");
    // Off draws no visualiser, the state a shrunk player is in.
    await page.evaluate(() => localStorage.setItem("pharos.visualiser", "off"));
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
    await openFirstHit(page, "carefree");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-listen")).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("canvas.pharos-visualiser")).toHaveCount(0);
    await expect
      .poll(() => lines.some((l) => l === "pharos: visualiser tap=connected"), {
        timeout: 15_000,
      })
      .toBe(true);

    const audio = page.locator("#pharos-video");
    await page.getByRole("button", { name: "Pause" }).click();
    await expect.poll(() => audio.evaluate((a) => (a as HTMLMediaElement).paused)).toBe(true);
    await expect
      .poll(() => lines.find((l) => l.startsWith("pharos: audio output warm ")) ?? "")
      .toMatch(/reason=paused context_state=running rest_after_ms=300000/);
    await page.waitForTimeout(1_000);
    await page.getByRole("button", { name: "Play" }).click();
    const resumed = () => lines.find((l) => l.startsWith("pharos: resume click_to_playing_ms=")) ?? "";
    await expect.poll(resumed, { timeout: 15_000 }).toMatch(/item=music/);
    const line = resumed();
    expect(line).toMatch(/ context_state=running /);
    expect(line).not.toMatch(/playing_to_audible_ms=untapped/);
    expect(line).toMatch(/ context_wake=none /);
    expect(line).toMatch(/ user_agent=Mozilla\//);
    const paused = Number(/ paused_ms=(\d+) /.exec(line)?.[1] ?? "0");
    expect(paused).toBeGreaterThanOrEqual(900);
  });
});

// B450 / V390: a volume turned down stays down on the next player, which
// mounts a new <audio> element, and B478 / V422 its slider shows it from the
// first frame. B451 / V391: a lost WebGL context comes back
// without the screen dropping its visualiser, and attribute 0 is an array.
test.describe("pharos Dioxus UI now playing survives remounts", () => {
  test("the volume carries to the next player and a lost context is restored", async ({
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
    const volume = () =>
      page.evaluate(
        () =>
          (document.getElementById("pharos-video") as HTMLMediaElement | null)
            ?.volume ?? -1,
      );
    const play = async () => {
      await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
      await expect(page.locator(".pharos-listen")).toBeVisible({
        timeout: 15_000,
      });
    };

    await page.goto("/ui/");
    await page.evaluate(() => localStorage.removeItem("pharos.volume"));
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({
      timeout: 15_000,
    });
    await openFirstHit(page, "carefree");
    await play();

    await page.getByLabel("Volume").fill("0.3");
    await expect.poll(volume).toBeCloseTo(0.3, 2);
    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("pharos.volume")))
      .toBe("0.300");

    // Back only shrinks the player now (spec 020); Close ends it, so the
    // next Play mounts a fresh one.
    await page.getByRole("button", { name: "Back", exact: true }).click();
    await page.locator(".pharos-stage").getByRole("button", { name: "Close" }).click();
    await expect(page.locator(".pharos-stage")).toHaveCount(0);
    // B478 / V422: the new player's slider never draws full volume first.
    await page.evaluate(() => {
      const w = window as unknown as { seenVolumes: string[]; stopSampling: boolean };
      w.seenVolumes = [];
      w.stopSampling = false;
      const sample = () => {
        const slider = document.querySelector<HTMLInputElement>(".pharos-listen-volume input");
        if (slider) w.seenVolumes.push(slider.value);
        if (!w.stopSampling) requestAnimationFrame(sample);
      };
      requestAnimationFrame(sample);
    });
    await play();
    await expect.poll(volume).toBeCloseTo(0.3, 2);
    // At least one frame must have been sampled before sampling stops.
    await expect
      .poll(() =>
        page.evaluate(() => (window as unknown as { seenVolumes: string[] }).seenVolumes.length),
      )
      .toBeGreaterThan(0);
    const seen = await page.evaluate(() => {
      const w = window as unknown as { seenVolumes: string[]; stopSampling: boolean };
      w.stopSampling = true;
      return w.seenVolumes;
    });
    expect(seen.length).toBeGreaterThan(0);
    expect(seen.filter((v) => Math.abs(Number(v) - 0.3) > 0.01)).toEqual([]);

    // V393: a volume localStorage refuses still reaches the next player,
    // from this page load's memory rather than the stored 0.300.
    await page.evaluate(() => {
      const write = Storage.prototype.setItem;
      Storage.prototype.setItem = function (key: string, value: string) {
        if (key === "pharos.volume") throw new DOMException("full", "QuotaExceededError");
        return write.call(this, key, value);
      };
    });
    await page.getByLabel("Volume").fill("0.5");
    await expect.poll(volume).toBeCloseTo(0.5, 2);
    await expect
      .poll(() => lines.join("\n"))
      .toMatch(/pharos: volume storage write failed/);
    await page.getByRole("button", { name: "Back", exact: true }).click();
    await page.locator(".pharos-stage").getByRole("button", { name: "Close" }).click();
    await expect(page.locator(".pharos-stage")).toHaveCount(0);
    await play();
    await expect.poll(volume).toBeCloseTo(0.5, 2);
    expect(lines.join("\n")).toMatch(/pharos: volume restored volume=0\.500 muted=false from=page/);
    expect(lines.join("\n")).toMatch(/pharos: volume restored volume=0\.300 muted=false/);

    // The rest needs a GPU context; headless CI may have none.
    await expect
      .poll(() => [...lines].reverse().find((l) => /pharos: renderer mode=/.test(l)) ?? "", {
        timeout: 15_000,
      })
      .toMatch(/mode=(webgl2|fallback)/);
    const mode = [...lines].reverse().find((l) => /pharos: renderer mode=/.test(l)) ?? "";
    test.skip(!/mode=webgl2/.test(mode), `no WebGL2 here: ${mode}`);

    await expect
      .poll(() =>
        page.evaluate(() => {
          const canvas = document.querySelector("canvas.pharos-visualiser");
          const gl = (canvas as HTMLCanvasElement | null)?.getContext("webgl2");
          return gl ? gl.getVertexAttrib(0, gl.VERTEX_ATTRIB_ARRAY_ENABLED) : null;
        }),
      )
      .toBe(true);

    await page.evaluate(() => {
      const canvas = document.querySelector("canvas.pharos-visualiser") as HTMLCanvasElement;
      const ext = canvas.getContext("webgl2")?.getExtension("WEBGL_lose_context");
      ext?.loseContext();
      setTimeout(() => ext?.restoreContext(), 300);
    });
    await expect
      .poll(() => lines.join("\n"), { timeout: 10_000 })
      .toMatch(/pharos: webgl context restored program=\w+/);
    await expect(page.locator("canvas.pharos-visualiser")).toBeVisible();
    expect(lines.join("\n")).not.toMatch(/reason=context_lost/);
    await expect.poll(volume).toBeCloseTo(0.5, 2);
  });
});

// B452 / V392: a dropped connection reconnects without rebuilding the
// player, so the song plays on from where it was in the same element.
test.describe("pharos Dioxus UI now playing survives a reconnect", () => {
  test("a dropped connection keeps the same audio element playing", async ({
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
    await page.addInitScript(() => {
      const sockets: WebSocket[] = [];
      (window as unknown as { __sockets: WebSocket[] }).__sockets = sockets;
      const Native = window.WebSocket;
      window.WebSocket = class extends Native {
        constructor(url: string | URL, protocols?: string | string[]) {
          super(url, protocols);
          sockets.push(this);
        }
      } as typeof WebSocket;
    });
    const at = () =>
      page.evaluate(
        () =>
          (document.getElementById("pharos-video") as HTMLMediaElement | null)
            ?.currentTime ?? 0,
      );

    await page.goto("/ui/");
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({
      timeout: 15_000,
    });
    await openFirstHit(page, "carefree");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-listen")).toBeVisible({
      timeout: 15_000,
    });
    await expect.poll(at, { timeout: 30_000 }).toBeGreaterThan(2);

    await page.evaluate(() => {
      (document.getElementById("pharos-video") as HTMLMediaElement & { __probe?: number })
        .__probe = 1;
      for (const s of (window as unknown as { __sockets: WebSocket[] }).__sockets) {
        if (s.readyState === WebSocket.OPEN) s.close();
      }
    });
    const before = await at();
    // Reconnected: a newer socket is open and the screens settled.
    await expect
      .poll(
        () =>
          page.evaluate(() => {
            const all = (window as unknown as { __sockets: WebSocket[] }).__sockets;
            return all.filter((s) => s.readyState === WebSocket.OPEN).length;
          }),
        { timeout: 15_000 },
      )
      .toBeGreaterThan(0);
    await expect(page.getByText("Connection lost. Reconnecting…")).toHaveCount(0, {
      timeout: 15_000,
    });
    // Renewal retries after 1, 2 and 4 s, so wait for its line.
    await expect
      .poll(() => lines.join("\n"), { timeout: 15_000 })
      .toMatch(/pharos: playback grant renewed/);

    const kept = await page.evaluate(
      () =>
        (document.getElementById("pharos-video") as HTMLMediaElement & { __probe?: number })
          ?.__probe ?? 0,
    );
    expect(kept, "the reconnect rebuilt the <audio>").toBe(1);
    await expect.poll(at).toBeGreaterThan(before + 0.5);

    // The stream is still served after the reconnect: a seek past what is
    // buffered plays on without an error.
    const target = await page.evaluate(() => {
      const a = document.getElementById("pharos-video") as HTMLMediaElement;
      a.currentTime = Math.max(a.currentTime, a.duration - 5);
      return a.currentTime;
    });
    // Only playback past the target shows the range was fetched and played.
    await expect.poll(at, { timeout: 15_000 }).toBeGreaterThan(target + 1);
    expect(
      await page.evaluate(
        () => (document.getElementById("pharos-video") as HTMLMediaElement).error?.code ?? 0,
      ),
    ).toBe(0);
  });
});

// V300: the seeded song has a synced .lrc sidecar (a line every 2 s, written
// by `just compat-dioxus-full`). Its lines light up in time, a line seeks
// when clicked, and the Lyrics chip hides the panel.
test.describe("pharos Dioxus UI lyrics", () => {
  test("synced lyrics follow the song and seek when a line is chosen", async ({
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
    await page.goto("/ui/");
    await page.evaluate(() => localStorage.removeItem("pharos.listen.lyrics"));
    await page.evaluate(() => localStorage.removeItem("pharos.listen.karaoke"));
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({
      timeout: 15_000,
    });
    await openFirstHit(page, "carefree");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();

    const panel = page.getByRole("region", { name: "Lyrics" });
    await expect(panel).toBeVisible({ timeout: 15_000 });
    await expect(panel).toHaveAttribute("data-synced", "true");
    const audio = page.locator("#pharos-video");
    const current = panel.locator('[data-state="current"]');
    // Restart from the top so the lit line is predictable.
    await audio.evaluate((a) => ((a as HTMLMediaElement).currentTime = 0));
    await expect(current).toHaveText(/Carefree line [01]$/, { timeout: 15_000 });
    await expect(current).toHaveText(/Carefree line [2-9]$/, { timeout: 15_000 });

    // A later line seeks there, lights up, and is scrolled to the middle.
    await panel.getByRole("button", { name: "Carefree line 15" }).click();
    await expect
      .poll(() => audio.evaluate((a) => (a as HTMLMediaElement).currentTime))
      .toBeGreaterThanOrEqual(29.5);
    await expect(current).toHaveText(/Carefree line 1[5-9]$/);
    await expect
      .poll(() =>
        panel.evaluate((p) => {
          const lit = p.querySelector('[data-state="current"]') as HTMLElement;
          const box = p.getBoundingClientRect();
          const line = lit.getBoundingClientRect();
          return Math.abs(line.top + line.height / 2 - (box.top + box.height / 2));
        }),
      )
      .toBeLessThan(40);

    // Karaoke (B483/V427): off to start; on, the sung line fills in as it
    // is sung, and the choice is kept on the device.
    await expect(panel).toHaveAttribute("data-karaoke", "false");
    const karaoke = page.getByRole("button", { name: "Karaoke", exact: true });
    await expect(karaoke).toHaveAttribute("aria-pressed", "false");
    await karaoke.click();
    await expect(panel).toHaveAttribute("data-karaoke", "true");
    const words = panel.locator('[data-state="current"] .pharos-lyrics-words');
    await expect(words).toHaveCount(1);
    const wipe = () =>
      words.evaluate((w) => parseFloat(getComputedStyle(w).getPropertyValue("--pharos-wipe")));
    const first = await wipe();
    await expect.poll(wipe, { timeout: 5_000 }).not.toBe(first);
    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("pharos.listen.karaoke")))
      .toBe("on");
    await karaoke.click();
    await expect(panel).toHaveAttribute("data-karaoke", "false");

    await page.getByRole("button", { name: "Lyrics", exact: true }).click();
    await expect(panel).toBeHidden();
    await expect
      .poll(() =>
        page.evaluate(() => localStorage.getItem("pharos.listen.lyrics")),
      )
      .toBe("hidden");
    await page.getByRole("button", { name: "Lyrics", exact: true }).click();
    await expect(panel).toBeVisible();
  });
});

// V375/V376: an album page queues songs, the player shows them in a
// collapsible queue, and a dragged row moves.
test.describe("pharos Dioxus UI play queue", () => {
  test("queue an album, open the queue, drag, shuffle and clear", async ({
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
    const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
      headers: {
        "Content-Type": "application/json",
        "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-queue-${Date.now()}", Version="0"`,
      },
      data: { Username: TEST_USER, Pw: TEST_PASS },
    });
    test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
    const { AccessToken: token, User: user } = await auth.json();
    const api = { "X-Emby-Token": token };
    const json = async (path: string) =>
      (await request.get(`${PHAROS_URL}${path}`, { headers: api })).json();
    const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
    const tracks = await json(
      `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=AlbumArtist,Artists,Album`,
    );
    // The seeded song (05-carefree.mp3) has an album and a track artist but
    // no album artist tag, so the artist stands in for it (songBy).
    const track = tracks.Items?.find((t: Song) => t.Album && songBy(t));
    test.skip(!track, "the seed has no track tagged with an album and artist");
    const artists = await json(`/Artists`);
    const artist = artists.Items?.find((a: { Name: string }) => a.Name === songBy(track));
    expect(artist, "the track's artist is listed").toBeTruthy();

    await page.goto("/ui/");
    await page.evaluate(() => localStorage.removeItem("pharos.listen.queue"));
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });

    // The album card offers the queue buttons too.
    await page.goto(`/ui/browse/${nativeSynth(artist.Id)}`);
    const card = page
      .locator("section.pharos-artist ul.pharos-grid li", { hasText: track.Album })
      .first();
    await expect(card).toBeVisible({ timeout: 15_000 });
    await card.hover();
    await expect(
      card.getByRole("button", { name: `Play ${track.Album} next` }),
    ).toBeVisible();
    await card.locator("a").first().click();
    await expect(page.locator("section.pharos-album h2")).toHaveText(track.Album, {
      timeout: 15_000,
    });

    // Three copies of the album, so the queue holds at least two rows once
    // the first song starts and takes its own first copy out.
    const addAlbum = page.getByRole("button", { name: `Add ${track.Album} to the queue` });
    for (let i = 0; i < 3; i++) {
      await addAlbum.click();
      await expect(page.locator(".pharos-queue-note").first()).toHaveText(/added to the queue/);
    }
    const rows = page.locator("ol.pharos-tracks li");
    await expect(rows.first().locator(".pharos-track-queued")).toHaveText("In queue");
    await rows.first().locator("a").click();

    const toggle = page.locator(".pharos-queue-toggle");
    await expect(toggle).toBeVisible({ timeout: 15_000 });
    await expect(toggle).toHaveAttribute("aria-pressed", "false");
    await expect(page.locator(".pharos-lyrics-toggle")).toBeVisible();
    const panel = page.getByRole("complementary", { name: "Queue" });
    await expect(panel).toBeHidden();
    await toggle.click();
    await expect(panel).toBeVisible();
    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("pharos.listen.queue")))
      .toBe("shown");

    const entries = panel.locator("li.pharos-queue-entry");
    await expect.poll(() => entries.count()).toBeGreaterThanOrEqual(2);
    const count = await entries.count();
    const last = await entries.nth(count - 1).getAttribute("data-key");
    await entries.nth(count - 1).dragTo(entries.first());
    await expect(entries.first()).toHaveAttribute("data-key", last ?? "");

    const shuffle = panel.getByRole("button", { name: "Shuffle" });
    await shuffle.click();
    await expect(shuffle).toHaveAttribute("aria-pressed", "true");
    await expect(entries).toHaveCount(count);

    // Clear asks first (spec 020 US3): Cancel and Escape keep the queue.
    const ask = panel.getByRole("alertdialog", { name: "Clear the queue?" });
    await panel.getByRole("button", { name: "Clear", exact: true }).click();
    await expect(ask).toContainText(`This removes ${count} songs.`);
    await expect(ask.getByRole("button", { name: "Cancel" })).toBeFocused();
    await ask.getByRole("button", { name: "Cancel" }).click();
    await expect(ask).toBeHidden();
    await expect(entries).toHaveCount(count);
    await expect(panel.getByRole("button", { name: "Clear", exact: true })).toBeFocused();
    await panel.getByRole("button", { name: "Clear", exact: true }).click();
    await page.keyboard.press("Escape");
    await expect(ask).toBeHidden();
    await expect(entries).toHaveCount(count);
    await expect(page.locator(".pharos-listen")).toBeVisible();
    // Escape still cancels once focus has left the panel.
    await panel.getByRole("button", { name: "Clear", exact: true }).click();
    await expect(ask).toBeVisible();
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await page.keyboard.press("Escape");
    await expect(ask).toBeHidden();
    await expect(entries).toHaveCount(count);
    await panel.getByRole("button", { name: "Clear", exact: true }).click();
    await ask.getByRole("button", { name: "Clear queue" }).click();
    await expect(entries).toHaveCount(0);
    await expect(panel).toContainText("Nothing queued.");
    // The dialog and Clear are gone; focus lands on Shuffle, not the body.
    await expect(panel.getByRole("button", { name: "Shuffle" })).toBeFocused();
    await toggle.click();
    await expect(panel).toBeHidden();
  });

  // Spec 029 (V512): the album page, the music player and its queue work
  // from the keyboard alone.
  test("an album plays, queues and shrinks from the keyboard", async ({ page, request }) => {
    test.setTimeout(90_000);
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
        "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-keys-${Date.now()}", Version="0"`,
      },
      data: { Username: TEST_USER, Pw: TEST_PASS },
    });
    test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
    const { AccessToken: token, User: user } = await auth.json();
    const api = { "X-Emby-Token": token };
    const json = async (path: string) =>
      (await request.get(`${PHAROS_URL}${path}`, { headers: api })).json();
    const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
    const tracks = await json(
      `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=AlbumArtist,Artists,Album`,
    );
    const track = tracks.Items?.find((t: Song) => t.Album && songBy(t));
    test.skip(!track, "the seed has no track tagged with an album and artist");
    const artists = await json(`/Artists`);
    const artist = artists.Items?.find((a: { Name: string }) => a.Name === songBy(track));
    expect(artist, "the track's artist is listed").toBeTruthy();

    await page.goto("/ui/");
    await page.evaluate(() => localStorage.removeItem("pharos.listen.queue"));
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
    await page.goto(`/ui/browse/${nativeSynth(artist.Id)}`);
    const card = page
      .locator("section.pharos-artist ul.pharos-grid li", { hasText: track.Album })
      .first();
    await expect(card).toBeVisible({ timeout: 15_000 });
    const albumPath = await card.locator("a").first().getAttribute("href");
    await page.goto(albumPath ?? "");

    // Opened fresh, the first track holds the focus, so Enter plays it.
    const first = page.locator("#pharos-first-track a.pharos-track");
    await expect(first).toBeFocused({ timeout: 15_000 });
    for (let i = 0; i < 3; i++) {
      await page.keyboard.press("a");
      await expect(page.locator("#pharos-first-track .pharos-queue-note")).toHaveText(
        /added to the queue/,
      );
    }
    await page.keyboard.press("Enter");
    await expect(page.locator(".pharos-stage.is-full .pharos-listen")).toBeVisible({
      timeout: 15_000,
    });

    // q opens the queue and puts the focus in it; G, k and d work its rows.
    const panel = page.getByRole("complementary", { name: "Queue" });
    const entries = panel.locator("li.pharos-queue-entry");
    await page.locator("body").focus();
    await page.keyboard.press("q");
    await expect(panel).toBeVisible();
    await expect(panel.getByRole("button", { name: "Shuffle" })).toBeFocused();
    await page.keyboard.press("s");
    await expect(panel.getByRole("button", { name: "Shuffle" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect.poll(() => entries.count()).toBeGreaterThanOrEqual(2);
    const count = await entries.count();
    await page.keyboard.press("G");
    const focusedRow = panel.locator("li.pharos-queue-entry:has(:focus)");
    await expect(focusedRow).toHaveCount(1);
    await expect(focusedRow).toHaveAttribute("data-key", (await entries.last().getAttribute("data-key")) ?? "");
    await page.keyboard.press("k");
    await expect(focusedRow).toHaveCount(1);
    await page.keyboard.press("d");
    await expect(entries).toHaveCount(count - 1);

    // Out of the queue, Escape takes the song to the small player and
    // Shift+F brings the full player back.
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await page.keyboard.press("Escape");
    await expect(page.locator(".pharos-stage.is-full")).toHaveCount(0);
    await expect(page.locator(".pharos-stage")).toBeVisible();
    await page.keyboard.press("Shift+F");
    await expect(page.locator(".pharos-stage.is-full .pharos-listen")).toBeVisible({
      timeout: 15_000,
    });
    // And Shift+M makes the full player small again (B623).
    await page.keyboard.press("Shift+M");
    await expect(page.locator(".pharos-stage.is-full")).toHaveCount(0);
    await expect(page.locator(".pharos-stage")).toBeVisible();
  });
});

// Spec 020: one player for the session. A song keeps playing through 20
// route changes in its floating window (013 SC-004), the window drags
// anywhere and stays in view, and swaps to the mini bar and back to full,
// all on the one stream the first play negotiated.
test.describe("pharos Dioxus UI stage", () => {
  const signIn = async (page: Page) => {
    await page.goto("/ui/");
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
  };
  const at = (page: Page) =>
    page.evaluate(
      () => (document.getElementById("pharos-video") as HTMLMediaElement | null)?.currentTime ?? 0,
    );
  const plays = (lines: string[]) => lines.filter((l) => /pharos: playback method=/.test(l)).length;

  test("a song plays on through route changes, floating, mini and full again", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    const lines: string[] = [];
    page.on("console", (m) => lines.push(m.text()));
    await page.setViewportSize({ width: 1200, height: 800 });
    await signIn(page);

    await openFirstHit(page, "carefree");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-listen")).toBeVisible({
      timeout: 15_000,
    });
    await expect.poll(() => at(page), { timeout: 30_000 }).toBeGreaterThan(0.5);
    expect(plays(lines)).toBe(1);

    // Back leaves the screen and the song floats.
    await page.locator(".pharos-listen").getByRole("button", { name: "Back", exact: true }).click();
    const stage = page.locator(".pharos-stage.is-floating");
    await expect(stage).toBeVisible();
    await expect(page.locator(".pharos-listen")).toBeHidden();

    let last = await at(page);
    for (let i = 0; i < 20; i++) {
      if (i % 3 === 0) {
        await page.locator(".pharos-brand").click();
      } else if (i % 3 === 1) {
        await page.locator(".pharos-library-tab").first().click();
      } else {
        await page.getByRole("link", { name: "Together" }).first().click();
      }
    }
    await expect.poll(() => at(page), { timeout: 10_000 }).toBeGreaterThan(last);
    expect(
      await page.evaluate(() => (document.getElementById("pharos-video") as HTMLMediaElement).paused),
    ).toBe(false);
    expect(plays(lines), "no second play for a route change").toBe(1);

    // Drag the window by its strip to the top-left corner.
    const strip = stage.locator(".pharos-stage-strip");
    const box = (await strip.boundingBox())!;
    await page.mouse.move(box.x + 60, box.y + box.height / 2);
    await page.mouse.down();
    await page.mouse.move(box.x - 2000, box.y - 2000, { steps: 8 });
    await page.mouse.up();
    await expect.poll(async () => (await stage.boundingBox())!.x).toBeLessThan(1);
    expect((await stage.boundingBox())!.y).toBeLessThan(1);
    await expect
      .poll(() => page.evaluate(() => sessionStorage.getItem("pharos.stage.spot")))
      .toBe("0.0000,0.0000");

    // From the bottom-right, a smaller window keeps it wholly in view.
    await strip.focus();
    for (let i = 0; i < 100; i++) await page.keyboard.press("ArrowRight");
    for (let i = 0; i < 100; i++) await page.keyboard.press("ArrowDown");
    await page.setViewportSize({ width: 400, height: 600 });
    const small = (await stage.boundingBox())!;
    expect(small.x).toBeGreaterThanOrEqual(0);
    expect(small.y).toBeGreaterThanOrEqual(0);
    expect(small.x + small.width).toBeLessThanOrEqual(400.5);
    expect(small.y + small.height).toBeLessThanOrEqual(600.5);
    await page.setViewportSize({ width: 1200, height: 800 });

    await stage.getByRole("button", { name: "Mini bar" }).click();
    const bar = page.locator(".pharos-stage.is-mini");
    await expect(bar).toBeVisible();
    await expect(bar.getByRole("button", { name: "Pause" })).toBeVisible();

    last = await at(page);
    await bar.getByRole("button", { name: "Expand" }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-listen")).toBeVisible();
    await expect(page).toHaveURL(/\/ui\/watch\//);
    await expect.poll(() => at(page)).toBeGreaterThan(last);
    expect(plays(lines), "the full view keeps the stream").toBe(1);

    // The shrink button swaps without Back; it remembers the bar.
    await page.locator(".pharos-listen").getByRole("button", { name: "Shrink" }).click();
    await expect(bar).toBeVisible();
    await bar.getByRole("button", { name: "Close" }).click();
    await expect(page.locator(".pharos-stage")).toHaveCount(0);
    expect(lines.some((l) => /pharos: stage view=closed/.test(l))).toBe(true);
    // Full → shrink → Close ends on a page, never a blank watch route.
    await expect(page).not.toHaveURL(/\/ui\/watch\//);
    await expect(page.locator(".pharos-main > *").first()).toBeVisible();
  });

  test("a queued song plays next in the floating window without opening it", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
      headers: {
        "Content-Type": "application/json",
        "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-stage-${Date.now()}", Version="0"`,
      },
      data: { Username: TEST_USER, Pw: TEST_PASS },
    });
    test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
    const { AccessToken: token, User: user } = await auth.json();
    const json = async (path: string) =>
      (await request.get(`${PHAROS_URL}${path}`, { headers: { "X-Emby-Token": token } })).json();
    const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
    const tracks = await json(
      `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=AlbumArtist,Artists,Album`,
    );
    const track = tracks.Items?.find((t: Song) => t.Album && songBy(t));
    test.skip(!track, "the seed has no track tagged with an album and artist");
    const artists = await json(`/Artists`);
    const artist = artists.Items?.find((a: { Name: string }) => a.Name === songBy(track));
    expect(artist, "the track's album artist is listed").toBeTruthy();

    const lines: string[] = [];
    page.on("console", (m) => lines.push(m.text()));
    await signIn(page);
    await page.goto(`/ui/browse/${nativeSynth(artist.Id)}`);
    const card = page
      .locator("section.pharos-artist ul.pharos-grid li", { hasText: track.Album })
      .first();
    await expect(card).toBeVisible({ timeout: 15_000 });
    await card.locator("a").first().click();
    await expect(page.locator("section.pharos-album h2")).toHaveText(track.Album, {
      timeout: 15_000,
    });
    // The whole album again behind the first song, so something follows it.
    // Twice: starting a queued song takes its copy off the queue, and the
    // seed's album may hold only that one song.
    const add = page.getByRole("button", { name: `Add ${track.Album} to the queue` });
    await add.click();
    await add.click();
    await page.locator("ol.pharos-tracks li").first().locator("a").click();
    await expect(page.locator(".pharos-stage.is-full .pharos-listen")).toBeVisible({
      timeout: 15_000,
    });
    await expect.poll(() => at(page), { timeout: 30_000 }).toBeGreaterThan(0.5);
    await page.locator(".pharos-listen").getByRole("button", { name: "Shrink" }).click();
    await expect(page.locator(".pharos-stage.is-floating")).toBeVisible();
    await expect(page.locator("section.pharos-album")).toBeVisible();
    const route = page.url();
    const before = lines.length;

    await page.evaluate(() => {
      const a = document.getElementById("pharos-video") as HTMLMediaElement;
      a.currentTime = Math.max(0, a.duration - 0.5);
    });
    // The next song is a new playback, or the same song again in place.
    await expect
      .poll(
        () =>
          lines
            .slice(before)
            .some((l) => /pharos: playback method=|pharos: queue replay/.test(l)),
        { timeout: 20_000 },
      )
      .toBe(true);
    await expect(page.locator(".pharos-stage.is-floating")).toBeVisible();
    await expect.poll(() => at(page), { timeout: 15_000 }).toBeGreaterThan(0.2);
    expect(page.url(), "the page stays where the listener is").toBe(route);
    expect(lines.slice(before).some((l) => /ui_action|stage view=full/.test(l))).toBe(false);
  });

  test("a Play while the player floats plays there and leaves the page alone", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    const lines: string[] = [];
    page.on("console", (m) => lines.push(m.text()));
    await signIn(page);

    await openFirstHit(page, "carefree");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect.poll(() => at(page), { timeout: 30_000 }).toBeGreaterThan(0.5);
    await page.locator(".pharos-listen").getByRole("button", { name: "Shrink" }).click();
    await expect(page.locator(".pharos-stage.is-floating")).toBeVisible();

    // A film from its title page: the page stays, the window plays it.
    await openFirstHit(page, "Playwright");
    await expect(page).toHaveURL(/\/ui\/title\//);
    const title = page.url();
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    const video = page.locator(".pharos-stage.is-floating .pharos-watch video");
    // On a miss, say what the stage became and what the page logged.
    await expect(video, "the floating stage plays the film")
      .toBeVisible({ timeout: 15_000 })
      .catch(async (e) => {
        const stage = await page
          .locator(".pharos-stage")
          .evaluateAll((els) => els.map((el) => `${el.className}: ${el.innerHTML.slice(0, 600)}`));
        throw new Error(
          `${e}\nurl=${page.url()}\nstage=${JSON.stringify(stage)}\nlog:\n${lines.slice(-40).join("\n")}`,
        );
      });
    await expect.poll(() => at(page), { timeout: 30_000 }).toBeGreaterThan(0.2);
    expect(page.url(), "the title page stays").toBe(title);
    expect(plays(lines)).toBe(2);
    expect(lines.some((l) => /pharos: stage view=floating item=\S+ reason=play/.test(l))).toBe(
      true,
    );

    // Expand shows the film full, on its own watch route.
    await page.locator(".pharos-stage").getByRole("button", { name: "Expand" }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch")).toBeVisible();
    await expect(page).toHaveURL(/\/ui\/watch\//);
    expect(plays(lines), "expanding keeps the stream").toBe(2);
  });

  // A video opens full-window, inside the browser window rather than in
  // browser fullscreen (B903), and Esc shrinks it to the compact player so
  // the library can be browsed while it plays on (V852).
  test("a video opens full-window, and Esc shrinks it to the compact player", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    const fullscreen = () => page.evaluate(() => document.fullscreenElement !== null);
    const fillsWindow = () =>
      page.evaluate(() => {
        const stage = document.querySelector(".pharos-stage.is-full .pharos-watch");
        if (!stage) return false;
        const r = stage.getBoundingClientRect();
        return (
          Math.round(r.width) === window.innerWidth && Math.round(r.height) === window.innerHeight
        );
      });
    await signIn(page);

    await openFirstHit(page, "Playwright");
    await expect(page).toHaveURL(/\/ui\/title\//);
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await expect.poll(fillsWindow, { message: "a video fills the window" }).toBe(true);
    expect(await fullscreen(), "a video is not in browser fullscreen").toBe(false);

    // Esc shrinks it; the library shows with the film still playing.
    await page.keyboard.press("Escape");
    await expect(page.locator(".pharos-stage.is-floating, .pharos-stage.is-mini")).toBeVisible();
    await expect(page).not.toHaveURL(/\/ui\/watch\//);
    await expect(page.locator(".pharos-shell")).toBeVisible();
    await expect
      .poll(() =>
        page.evaluate(() => {
          const v = document.querySelector<HTMLVideoElement>(".pharos-stage video");
          return v !== null && !v.ended && v.readyState > 0;
        }),
      )
      .toBe(true);
    await page.locator(".pharos-stage-bar").getByRole("button", { name: "Close" }).click();
    await expect(page.locator(".pharos-stage")).toHaveCount(0);

    // An episode's Play on its show page opens full-window too.
    await page.locator(".pharos-brand").click();
    await page.locator("section.pharos-rail h2 a", { hasText: /^TV$/ }).click();
    await page.locator("ul.pharos-grid .pharos-poster-name", { hasText: "Playwright Show" }).click();
    await page.locator("ol.pharos-episodes li a.pharos-episode-play").first().click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await expect.poll(fillsWindow, { message: "an episode fills the window" }).toBe(true);
    expect(await fullscreen(), "an episode is not in browser fullscreen").toBe(false);

    // A menu claims Esc first: the open key help closes, the player stays.
    await page.keyboard.press("?");
    await expect(page.locator(".pharos-key-help")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator(".pharos-key-help")).toHaveCount(0);
    await expect(page.locator(".pharos-stage.is-full .pharos-watch")).toBeVisible();

    // In browser fullscreen the first Esc only leaves it, the next shrinks.
    await page.keyboard.press("f");
    await expect.poll(fullscreen, { message: "f enters fullscreen" }).toBe(true);
    await page.keyboard.press("Escape");
    await expect.poll(fullscreen, { message: "Esc leaves fullscreen" }).toBe(false);
    await expect(page.locator(".pharos-stage.is-full .pharos-watch")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator(".pharos-stage.is-floating, .pharos-stage.is-mini")).toBeVisible();
  });

  // Films and episodes have a queue of their own (V853): a film added from
  // its page waits in the player's queue and Next plays it before the next
  // episode, taking it out of the queue.
  test("a queued film plays next from the video player's queue", async ({ page, request }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    await signIn(page);

    await openFirstHit(page, "Bulk 001");
    await expect(page).toHaveURL(/\/ui\/title\//);
    const film = page.url().match(/\/title\/([^/?#]+)/)![1];
    await page.getByRole("button", { name: /^Add .*Bulk 001 to the queue$/ }).click();
    await expect(page.locator(".pharos-queue-note")).toHaveText(/added to the queue/);

    await openFirstHit(page, "Playwright");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await page.mouse.move(20, 20);
    const toggle = page.locator(".pharos-watch-top").getByRole("button", { name: "Queue, 1 video" });
    await toggle.click();
    const panel = page.locator(".pharos-watch #pharos-queue");
    await expect(panel).toBeVisible();
    await expect(panel.locator(".pharos-queue-entry")).toHaveText(/Bulk 001/);

    // Esc in the queue closes it, not the player.
    await panel.getByRole("button", { name: /Bulk 001/ }).first().focus();
    await page.keyboard.press("Escape");
    await expect(panel).toHaveCount(0);
    await expect(page.locator(".pharos-stage.is-full .pharos-watch")).toBeVisible();

    await page.mouse.move(40, 40);
    await page.getByRole("button", { name: "Next in the queue" }).click();
    await expect(page).toHaveURL(new RegExp(`/ui/watch/${film}`));
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await page.mouse.move(20, 20);
    await expect(
      page.locator(".pharos-watch-top").getByRole("button", { name: "Queue", exact: true }),
    ).toBeVisible();
    await expect(page.locator(".pharos-watch #pharos-queue .pharos-queue-entry")).toHaveCount(0);
  });

  // The floating window resizes from a top corner, away from the edge it
  // sits nearest, keeping the opposite bottom corner put (B583).
  test("the floating window resizes by its corner handle and the arrow keys", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    await page.setViewportSize({ width: 1200, height: 800 });
    await signIn(page);
    await openFirstHit(page, "Playwright");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await page.mouse.move(20, 20);
    await page.locator(".pharos-watch-top").getByRole("button", { name: "Shrink" }).click();
    const stage = page.locator(".pharos-stage.is-floating");
    await expect(stage).toBeVisible();
    const grip = stage.getByRole("button", { name: "Resize the player with the arrow keys" });
    await expect(grip).toHaveAttribute("data-grip", "top-left");

    const before = (await stage.boundingBox())!;
    const handle = (await grip.boundingBox())!;
    await page.mouse.move(handle.x + 4, handle.y + 4);
    await page.mouse.down();
    await page.mouse.move(handle.x + 4 - 160, handle.y + 4, { steps: 8 });
    await page.mouse.up();
    const after = (await stage.boundingBox())!;
    expect(after.width).toBeCloseTo(before.width + 160, 0);
    expect(after.height, "the video keeps its shape").toBeCloseTo(
      before.height + (160 * 9) / 16,
      0,
    );
    expect(after.x + after.width, "the right edge stays").toBeCloseTo(
      before.x + before.width,
      0,
    );
    expect(after.y + after.height, "the bottom edge stays").toBeCloseTo(
      before.y + before.height,
      0,
    );
    await expect
      .poll(() => page.evaluate(() => sessionStorage.getItem("pharos.stage.width")))
      .toBe(String(Math.round(after.width)));

    // Right moves the left-hand handle inwards: one step narrower.
    await grip.focus();
    await page.keyboard.press("ArrowRight");
    await expect.poll(async () => (await stage.boundingBox())!.width).toBeCloseTo(
      after.width - 16,
      0,
    );

    // The width outlives a reload of the tab.
    const width = (await stage.boundingBox())!.width;
    await page.locator(".pharos-stage-bar").getByRole("button", { name: "Close" }).click();
    await openFirstHit(page, "Playwright");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await page.mouse.move(20, 20);
    await page.locator(".pharos-watch-top").getByRole("button", { name: "Shrink" }).click();
    await expect.poll(async () => (await stage.boundingBox())!.width).toBeCloseTo(width, 0);
    await page.locator(".pharos-stage-bar").getByRole("button", { name: "Close" }).click();

    // A song's window is the strip alone: nothing to resize.
    await openFirstHit(page, "carefree");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await page.locator(".pharos-listen").getByRole("button", { name: "Shrink" }).click();
    await expect(stage).toBeVisible();
    await expect(grip).toBeHidden();
  });

  test("the compact player keeps every control in view at 320px", async ({ page, request }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    await signIn(page);
    await openFirstHit(page, "Playwright");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await page.mouse.move(20, 20);
    await page.locator(".pharos-watch-top").getByRole("button", { name: "Shrink" }).click();
    await page.setViewportSize({ width: 320, height: 640 });
    // Every button of the bar lies wholly inside the stage, which clips.
    const clipped = () =>
      page.evaluate(() => {
        const stage = document.querySelector(".pharos-stage")!.getBoundingClientRect();
        return [...document.querySelectorAll<HTMLElement>(".pharos-stage-bar button")]
          .filter((b) => b.offsetParent !== null)
          .filter((b) => {
            const r = b.getBoundingClientRect();
            return (
              r.left < stage.left - 0.5 ||
              r.right > stage.right + 0.5 ||
              r.right > document.documentElement.clientWidth + 0.5
            );
          })
          .map((b) => b.getAttribute("aria-label"));
      });
    await expect(page.locator(".pharos-stage.is-floating")).toBeVisible();
    const close = page.locator(".pharos-stage-bar").getByRole("button", { name: "Close" });
    await expect(close).toBeVisible();
    expect(await clipped(), "floating at 320px").toEqual([]);
    await page.locator(".pharos-stage").getByRole("button", { name: "Mini bar" }).click();
    await expect(page.locator(".pharos-stage.is-mini")).toBeVisible();
    expect(await clipped(), "mini bar at 320px").toEqual([]);
  });

  test("the floating player keeps its controls in view on a short viewport", async ({ page, request }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    await signIn(page);
    await openFirstHit(page, "Playwright");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await page.mouse.move(20, 20);
    await page.locator(".pharos-watch-top").getByRole("button", { name: "Shrink" }).click();
    await expect(page.locator(".pharos-stage.is-floating")).toBeVisible();
    await page.setViewportSize({ width: 320, height: 180 });
    // Every visible button of the bar lies inside the viewport's height.
    const below = await page.evaluate(() =>
      [...document.querySelectorAll<HTMLElement>(".pharos-stage-bar button")]
        .filter((b) => b.offsetParent !== null)
        .filter((b) => b.getBoundingClientRect().bottom > document.documentElement.clientHeight + 0.5)
        .map((b) => b.getAttribute("aria-label")),
    );
    expect(below, "floating at 320x180").toEqual([]);
    await expect(
      page.locator(".pharos-stage-bar").getByRole("button", { name: "Close" }),
    ).toBeInViewport();
  });

  test("picture in picture hands the film to the browser and back", async ({ page, request }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    const lines: string[] = [];
    page.on("console", (m) => lines.push(m.text()));
    await signIn(page);
    const pipId = () =>
      page.evaluate(
        () =>
          (document as Document & { pictureInPictureElement: Element | null })
            .pictureInPictureElement?.id ?? null,
      );
    const supported = await page.evaluate(
      () =>
        (document as Document & { pictureInPictureEnabled?: boolean }).pictureInPictureEnabled ===
        true,
    );
    test.skip(!supported, "this browser has no picture-in-picture");

    await openFirstHit(page, "Playwright");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await expect.poll(() => at(page), { timeout: 30_000 }).toBeGreaterThan(0.2);
    const watchPip = page.locator(".pharos-controls").getByRole("button", {
      name: "Picture in picture",
    });
    const stagePip = page.locator(".pharos-stage-bar").getByRole("button", {
      name: "Picture in picture",
    });

    // From the watch page: out to the browser's window and back.
    await page.mouse.move(20, 20);
    await watchPip.click();
    await expect.poll(pipId).toBe("pharos-video");
    await page.mouse.move(40, 40);
    await watchPip.click();
    await expect.poll(pipId).toBeNull();

    // From the floating window, which keeps playing behind it.
    await page.mouse.move(20, 20);
    await page.locator(".pharos-watch-top").getByRole("button", { name: "Shrink" }).click();
    await expect(page.locator(".pharos-stage.is-floating")).toBeVisible();
    await stagePip.click();
    await expect.poll(pipId).toBe("pharos-video");
    const from = await at(page);
    await expect.poll(() => at(page), { timeout: 10_000 }).toBeGreaterThan(from);
    await stagePip.click();
    await expect.poll(pipId).toBeNull();
    expect(plays(lines), "the browser's window keeps the stream").toBe(1);
    expect(lines.filter((l) => /pharos: pip action=open id=pharos-video/.test(l))).toHaveLength(2);
    expect(lines.filter((l) => /pharos: pip action=close id=pharos-video/.test(l))).toHaveLength(2);
    expect(lines.filter((l) => /pharos: pip .*refused/.test(l))).toEqual([]);
  });

  test("picture in picture is absent where the browser has none", async ({ page, request }) => {
    test.setTimeout(120_000);
    const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
      headers: { Accept: "application/json" },
    });
    test.skip(probe.status() !== 405, `native API is not enabled (status ${probe.status()})`);
    // Firefox has no `pictureInPictureEnabled`; stand in for it here.
    await page.addInitScript(() =>
      Object.defineProperty(Document.prototype, "pictureInPictureEnabled", {
        get: () => false,
      }),
    );
    await signIn(page);
    await openFirstHit(page, "Playwright");
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect(page.locator(".pharos-stage.is-full .pharos-watch video")).toBeVisible({
      timeout: 15_000,
    });
    await expect(page.getByRole("button", { name: "Fullscreen" })).toHaveCount(1);
    await expect(page.getByRole("button", { name: "Picture in picture" })).toHaveCount(0);
    await page.mouse.move(20, 20);
    await page.locator(".pharos-watch-top").getByRole("button", { name: "Shrink" }).click();
    await expect(page.locator(".pharos-stage.is-floating")).toBeVisible();
    await expect(page.getByRole("button", { name: "Picture in picture" })).toHaveCount(0);
  });
});

// V408/V409: a song hands the stage over to the next without a Loading
// screen in between, and with a crossfade chosen the two overlap, the old
// one fading out under the new one (ADR-0053).
test.describe("pharos Dioxus UI song handover", () => {
  // Signs in, queues Carefree's album and plays the short seeded song (6 s)
  // before it, with `crossfade` seconds chosen on the Playback page.
  async function playIntoQueuedSong(page: Page, request: APIRequestContext, crossfade: number) {
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
        "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-handover-${Date.now()}", Version="0"`,
      },
      data: { Username: TEST_USER, Pw: TEST_PASS },
    });
    test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
    const { AccessToken: token, User: user } = await auth.json();
    const tracks = await (
      await request.get(
        `${PHAROS_URL}/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=Album`,
        { headers: { "X-Emby-Token": token } },
      )
    ).json();
    type Track = { Id: string; Name: string; Album?: string; AlbumId?: string; RunTimeTicks?: number };
    const songs: Track[] = tracks.Items ?? [];
    const queued = songs.find((t) => t.AlbumId);
    const first = songs.find((t) => !t.AlbumId && (t.RunTimeTicks ?? 0) < 200_000_000);
    test.skip(!queued || !first, "the seed has no album song and a short song apart from it");
    // Earlier cases leave both part or wholly played; start each from 0.
    for (const t of [queued!, first!]) {
      await request.delete(`${PHAROS_URL}/Users/${user.Id}/PlayedItems/${t.Id}`, {
        headers: { "X-Emby-Token": token },
      });
    }
    const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
    const native = (t: Track) => BigInt(`0x${t.Id}`).toString();

    await page.goto("/ui/");
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
    if (crossfade > 0) {
      // Off until the listener turns it on.
      await page.goto("/ui/settings/playback");
      await expect(page.locator("#crossfade-0")).toHaveAttribute("aria-pressed", "true");
      await page.locator(`#crossfade-${crossfade}`).click();
      await expect(page.locator(`#crossfade-${crossfade}`)).toHaveAttribute("aria-pressed", "true");
    }
    await page.goto(`/ui/browse/${nativeSynth(queued!.AlbumId!)}`);
    await expect(page.locator("section.pharos-album h2")).toHaveText(queued!.Album!, {
      timeout: 30_000,
    });
    await page.getByRole("button", { name: `Add ${queued!.Album} to the queue` }).click();
    await expect(page.locator(".pharos-queue-note").first()).toHaveText(/added to the queue/);
    // The queue lives in the page, so go on without a reload; then click
    // Play, so the play is a gesture and the page may autoplay.
    await page.evaluate((path) => {
      history.pushState(null, "", path);
      dispatchEvent(new PopStateEvent("popstate"));
    }, `/ui/title/${native(first!)}`);
    await page.locator(`a[href$="/ui/watch/${native(first!)}"]`).first().click();
    const audio = page.locator("#pharos-video");
    await expect
      .poll(() => audio.evaluate((a: HTMLMediaElement) => a.readyState), { timeout: 15_000 })
      .toBeGreaterThan(0);
    return { audio, next: `/ui/watch/${native(queued!)}` };
  }

  test("the next song takes over without a loading screen", async ({ page, request }) => {
    test.setTimeout(90_000);
    // Anything drawn as Loading on the stage from here on is the flash.
    await page.addInitScript(() => {
      const w = window as unknown as { pharosLoading: number };
      w.pharosLoading = 0;
      new MutationObserver(() => {
        const loading = document.querySelector("#pharos-stage .pharos-loading");
        if (loading && loading.getClientRects().length > 0) w.pharosLoading++;
      }).observe(document, { childList: true, subtree: true, attributes: true });
    });
    const { audio, next } = await playIntoQueuedSong(page, request, 0);
    await expect.poll(() => audio.evaluate((a: HTMLMediaElement) => !a.paused)).toBe(true);
    const before = await page.evaluate(
      () => (window as unknown as { pharosLoading: number }).pharosLoading,
    );
    // The short song plays out and the queued one follows it.
    await expect(page).toHaveURL(new RegExp(`${next}$`), { timeout: 20_000 });
    await expect(page.locator("#pharos-video-out")).toHaveCount(0, { timeout: 10_000 });
    await expect(page.locator(".pharos-listen")).toHaveCount(1);
    await expect.poll(() => audio.evaluate((a: HTMLMediaElement) => !a.paused)).toBe(true);
    expect(
      await page.evaluate(() => (window as unknown as { pharosLoading: number }).pharosLoading),
    ).toBe(before);
  });

  // V472: Shift+N / Shift+P skip from anywhere in the page, not only with
  // the player focused, and the media session's nexttrack / previoustrack
  // (the media keys and the system's media controls) do the same; a skip
  // with nowhere to go is not offered to the system.
  test("Shift+N and the media session skip tracks from anywhere in the page", async ({
    page,
    request,
  }) => {
    test.setTimeout(90_000);
    await page.addInitScript(() => {
      const handlers: Record<string, unknown> = {};
      (window as unknown as { skipHandlers: typeof handlers }).skipHandlers = handlers;
      const session = navigator.mediaSession;
      const set = session.setActionHandler.bind(session);
      session.setActionHandler = (action, handler) => {
        handlers[action] = handler;
        set(action, handler);
      };
    });
    // Every skip a player took, `to=none` included, so a count sees an extra
    // dispatch that found nowhere to go; `to` is checked separately.
    const skips: string[] = [];
    const destinations: string[] = [];
    page.on("console", (m) => {
      const skip = /^pharos: track skip (dir=\w+ source=\w+) item=\S+ to=(\S+)/.exec(m.text());
      if (skip) {
        skips.push(skip[1]);
        destinations.push(skip[2]);
      }
    });
    const handler = (action: string) =>
      page.evaluate(
        (action) =>
          typeof (window as unknown as { skipHandlers: Record<string, unknown> }).skipHandlers[
            action
          ],
        action,
      );
    const { next } = await playIntoQueuedSong(page, request, 0);
    const first = new URL(page.url()).pathname;
    await expect.poll(() => handler("nexttrack")).toBe("function");

    // Focus on the page, not the player.
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await page.keyboard.press("Shift+N");
    await expect(page).toHaveURL(new RegExp(`${next}$`), { timeout: 15_000 });
    expect(skips).toContain("dir=next source=keyboard");
    // The system offers a next track exactly when the Next button would play one.
    const nextButton = page.getByRole("button", { name: "Next track" });
    await expect(nextButton).toBeVisible({ timeout: 15_000 });
    await expect
      .poll(async () => (await handler("nexttrack")) === "function")
      .toBe(await nextButton.isEnabled());

    await expect
      .poll(() => page.locator("#pharos-video").evaluate((a: HTMLMediaElement) => a.readyState), {
        timeout: 15_000,
      })
      .toBeGreaterThan(0);
    // Past the restart threshold Previous restarts the song rather than going
    // back, so start from the top before asking for the previous track.
    await page.locator("#pharos-video").evaluate((a: HTMLMediaElement) => {
      a.pause();
      a.currentTime = 0;
    });
    // The slider shows the position the player itself acts on.
    await expect
      .poll(async () => Number(await page.getByRole("slider", { name: "Seek" }).inputValue()))
      .toBeLessThan(1_000);
    await page.evaluate(() =>
      (
        (window as unknown as { skipHandlers: Record<string, () => void> }).skipHandlers
      ).previoustrack(),
    );
    await expect.poll(() => skips.at(-1)).toBe("dir=previous source=media_session");
    // Back on the first song, the system's next goes to the queued one again
    // once that song's player is up and has offered it.
    await expect(page).toHaveURL(new RegExp(`${first}$`), { timeout: 15_000 });
    await expect
      .poll(() => page.locator("#pharos-video").evaluate((a: HTMLMediaElement) => a.readyState), {
        timeout: 15_000,
      })
      .toBeGreaterThan(0);
    await expect.poll(() => handler("nexttrack")).toBe("function");
    await page.evaluate(() =>
      (
        (window as unknown as { skipHandlers: Record<string, () => void> }).skipHandlers
      ).nexttrack(),
    );
    await expect(page).toHaveURL(new RegExp(`${next}$`), { timeout: 15_000 });
    await expect.poll(() => skips.at(-1)).toBe("dir=next source=media_session");
    await expect
      .poll(() => page.locator("#pharos-video").evaluate((a: HTMLMediaElement) => a.readyState), {
        timeout: 15_000,
      })
      .toBeGreaterThan(0);

    // With the focus on a slider, whose own keydown stops propagating, the
    // key still skips, and only once.
    const seek = page.getByRole("slider", { name: "Seek" });
    await expect(seek).toBeEnabled({ timeout: 15_000 });
    await seek.focus();
    const before = skips.length;
    await page.keyboard.press("Shift+P");
    await expect.poll(() => skips.at(-1)).toBe("dir=previous source=keyboard");
    await page.waitForTimeout(500);
    expect(skips.slice(before)).toEqual(["dir=previous source=keyboard"]);
    expect(destinations).not.toContain("none");
  });

  test("a crossfade plays the next song under the end of this one", async ({ page, request }) => {
    test.setTimeout(90_000);
    // A 6 s song caps the fade at a third of it, about 2 s.
    const { audio, next } = await playIntoQueuedSong(page, request, 4);
    const sample = () =>
      page.evaluate(() => {
        const at = (id: string) => document.getElementById(id) as HTMLMediaElement | null;
        const [o, i] = [at("pharos-video-out"), at("pharos-video")];
        return {
          both: !!o && !!i && !o.paused && !i.paused,
          out: o?.volume ?? -1,
          in: i?.volume ?? -1,
        };
      });
    await expect.poll(async () => (await sample()).both, { timeout: 15_000, intervals: [50] }).toBe(
      true,
    );
    await expect(page).toHaveURL(new RegExp(`${next}$`));
    const first = await sample();
    await page.waitForTimeout(500);
    const later = await sample();
    expect(later.both).toBe(true);
    expect(later.out).toBeLessThan(first.out);
    expect(later.in).toBeGreaterThan(first.in);
    // Faded out, the old song is gone and only the new one plays, at the
    // listener's volume.
    await expect(page.locator("#pharos-video-out")).toHaveCount(0, { timeout: 10_000 });
    await expect(page.locator(".pharos-listen")).toHaveCount(1);
    await expect.poll(() => audio.evaluate((a: HTMLMediaElement) => a.volume)).toBeGreaterThan(0.99);
  });
});


// V406: the now-playing bar has previous and next track buttons either side
// of the seek buttons. Next plays the queue's first song; Previous past the
// opening seconds starts the song again, and within them steps back to the
// song before (or starts it again when there is none).
test.describe("pharos Dioxus UI previous and next track", () => {
  test("next plays what is queued and previous restarts a song", async ({
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
    const auth = await request.post(`${PHAROS_URL}/Users/AuthenticateByName`, {
      headers: {
        "Content-Type": "application/json",
        "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-skip-${Date.now()}", Version="0"`,
      },
      data: { Username: TEST_USER, Pw: TEST_PASS },
    });
    test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
    const { AccessToken: token, User: user } = await auth.json();
    const api = { "X-Emby-Token": token };
    const json = async (path: string) =>
      (await request.get(`${PHAROS_URL}${path}`, { headers: api })).json();
    const nativeSynth = (wireId: string) => `s${BigInt(`0x${wireId.slice(16)}`)}`;
    const tracks = await json(
      `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=Album`,
    );
    // Long enough to seek past the restart point and back.
    const track = tracks.Items?.find(
      (t: { AlbumId?: string; RunTimeTicks?: number }) =>
        t.AlbumId && (t.RunTimeTicks ?? 0) > 200_000_000,
    );
    test.skip(!track, "the seed has no track of over 20 s on an album");
    // Two copies of a one-song album queue the same song, so Next is known.
    const siblings = await json(
      `/Users/${user.Id}/Items?ParentId=${track.AlbumId}&IncludeItemTypes=Audio`,
    );
    test.skip(
      (siblings.Items?.length ?? 0) !== 1,
      "the queue steps in this test assume a one-song album",
    );

    await page.goto("/ui/");
    await page.locator("#signin-name").fill(TEST_USER);
    await page.locator("#signin-password").fill(TEST_PASS);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
    await page.goto(`/ui/browse/${nativeSynth(track.AlbumId)}`);
    // A cold server reads the album's art and tracks on first visit.
    await expect(page.locator("section.pharos-album h2")).toHaveText(track.Album, {
      timeout: 30_000,
    });
    // Twice, so a song is still queued once the first copy starts.
    const addAlbum = page.getByRole("button", { name: `Add ${track.Album} to the queue` });
    for (let i = 0; i < 2; i++) {
      await addAlbum.click();
      await expect(page.locator(".pharos-queue-note").first()).toHaveText(/added to the queue/);
    }
    await page
      .locator("ol.pharos-tracks li", { hasText: track.Name })
      .first()
      .locator("a")
      .click();

    // The branch each Previous press took, from the UI's decision log.
    const previousReasons: string[] = [];
    page.on("console", (m) => {
      const reason = /^pharos: previous .*reason=(\w+)/.exec(m.text())?.[1];
      if (reason) previousReasons.push(reason);
    });
    const audio = page.locator("#pharos-video");
    const previous = page.getByRole("button", { name: "Previous track" });
    const next = page.getByRole("button", { name: "Next track" });
    await expect(previous).toBeVisible({ timeout: 15_000 });
    await expect(previous).toBeEnabled();
    await expect(next).toBeEnabled({ timeout: 15_000 });
    const at = () => audio.evaluate((a: HTMLMediaElement) => a.currentTime);
    const seekTo = async (s: number) => {
      await audio.evaluate((a: HTMLMediaElement, s: number) => {
        a.currentTime = s;
      }, s);
      await expect.poll(at).toBeGreaterThan(s - 1);
    };
    await expect
      .poll(() => audio.evaluate((a: HTMLMediaElement) => a.readyState), { timeout: 15_000 })
      .toBeGreaterThan(0);

    // Next plays the queue's first song, the second copy, from its start.
    await seekTo(12);
    await next.click();
    await expect.poll(at).toBeLessThan(5);
    await expect(page.locator("h2.pharos-listen-title")).toHaveText(track.Name);
    await expect(next).toBeDisabled();

    // Past the opening seconds, Previous starts the same song again.
    await expect
      .poll(() => audio.evaluate((a: HTMLMediaElement) => a.duration))
      .toBeGreaterThan(20);
    await seekTo(15);
    const song = page.url();
    await previous.click();
    await expect.poll(at).toBeLessThan(5);
    expect(page.url()).toBe(song);
    await expect.poll(() => previousReasons.at(-1)).toBe("past_start");
    // A restart steps nowhere, so nothing is queued yet.
    await expect(next).toBeDisabled();

    // In the opening seconds Previous steps back instead. Both copies are one
    // song, so history has nothing before it (the step back to a different
    // song is covered in queue.rs); the album has no track before either, so
    // the song starts again, by the early branch rather than the restart one.
    // Paused at 1 s, so only the press can bring the position to zero.
    await seekTo(1);
    await audio.evaluate((a: HTMLMediaElement) => a.pause());
    await expect.poll(at).toBeLessThan(2.5);
    expect(await at()).toBeGreaterThan(0.5);
    // Let a timeupdate carry the new position to the UI before pressing.
    await page.waitForTimeout(500);
    await previous.click();
    await expect.poll(() => previousReasons.at(-1)).toBe("nothing_before");
    await expect.poll(at).toBeLessThan(0.5);
    expect(page.url()).toBe(song);
    await expect(page.locator("h2.pharos-listen-title")).toHaveText(track.Name);
  });
});
