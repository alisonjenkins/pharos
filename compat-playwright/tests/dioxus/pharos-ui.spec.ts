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
      await page.getByRole("menuitem", { name: "Account" }).click();
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
      await page.getByRole("menuitem", { name: "Account" }).click();

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
      await expect(page.locator("p.pharos-error")).toHaveCount(0);
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
    test("browse jumps a library by letter and sorts it by date added", async ({
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

      // Date added, newest first: no letters to jump by.
      await page.getByLabel("Sort", { exact: true }).selectOption("added");
      await expect(rail).toHaveCount(0);
      await expect(page.getByRole("button", { name: /Sort direction: Newest first/ })).toBeVisible();
      await expect(tiles.first()).toBeVisible();
      await expect(page.locator("p.pharos-error")).toHaveCount(0);
      await page.evaluate(() => window.scrollTo(0, 0));
    });

    // A TV library lists its series by name and a series opens the show page
    // with its episodes; every step is a click, not a URL.
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
      await expect(page.locator("p.pharos-error")).toHaveCount(0);

      // Series -> the show page with its three episodes.
      await tiles.filter({ hasText: "Playwright Show" }).click();
      await expect(page.locator("section.pharos-show h2")).toHaveText("Playwright Show", {
        timeout: 15_000,
      });
      const episodes = page.locator("ol.pharos-episodes li");
      await expect(episodes).toHaveCount(3);
      await expect(page.locator("p.pharos-error")).toHaveCount(0);

      // Episode -> its title page, named as the row was.
      const row = (await episodes.first().locator(".pharos-episode-name").innerText()).trim();
      const name = row.replace(/^\d+\.\s+/, "");
      await episodes.first().locator("a.pharos-episode-link").click();
      await expect(page).toHaveURL(/\/ui\/title\//);
      await expect(page.locator("article.pharos-title h2")).toHaveText(name, {
        timeout: 15_000,
      });
      await expect(page.locator("p.pharos-error")).toHaveCount(0);
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
      await expect(page.locator("p.pharos-error")).toHaveCount(0);
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
            showing: Array.from(v.textTracks)
              .filter((t) => t.mode === "showing")
              .map((t) => ({ id: t.id, cues: t.cues?.length ?? 0 })),
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
        .poll(async () => (await media()).showing, { timeout: 15_000 })
        .toEqual([expect.objectContaining({ id: `pharos-sub-${option}` })]);
      await expect
        .poll(async () => (await media()).showing[0]?.cues ?? 0, { timeout: 15_000 })
        .toBeGreaterThan(0);
      expect(page.url(), "a text subtitle does not leave the page").toBe(before);
      await picker.selectOption("off");
      await expect.poll(async () => (await media()).showing).toEqual([]);

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
      await page.getByRole("button", { name: "Account", exact: true }).click();
      await page.getByRole("menuitem", { name: "Appearance" }).click();
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

// B450 / V390: a volume turned down stays down on the next player, which
// mounts a new <audio> element. B451 / V391: a lost WebGL context comes back
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
    await page.getByRole("link", { name: "Find" }).click();
    await page.getByLabel("Search").fill("carefree");
    const tile = page.locator(".pharos-poster").first();
    await expect(tile).toBeVisible({ timeout: 15_000 });
    await tile.click();
    await play();

    await page.getByLabel("Volume").fill("0.3");
    await expect.poll(volume).toBeCloseTo(0.3, 2);
    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("pharos.volume")))
      .toBe("0.300");

    await page.getByRole("button", { name: "Back", exact: true }).click();
    await play();
    await expect.poll(volume).toBeCloseTo(0.3, 2);
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
    await expect.poll(volume).toBeCloseTo(0.3, 2);
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
    await page.getByRole("link", { name: "Find" }).click();
    await page.getByLabel("Search").fill("carefree");
    const tile = page.locator(".pharos-poster").first();
    await expect(tile).toBeVisible({ timeout: 15_000 });
    await tile.click();
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
    await page.waitForTimeout(3_000);

    const kept = await page.evaluate(
      () =>
        (document.getElementById("pharos-video") as HTMLMediaElement & { __probe?: number })
          ?.__probe ?? 0,
    );
    expect(kept, "the reconnect rebuilt the <audio>").toBe(1);
    await expect.poll(at).toBeGreaterThan(before + 0.5);
    expect(lines.join("\n")).toMatch(/pharos: playback grant renewed/);

    // The stream is still served after the reconnect: a seek past what is
    // buffered plays on without an error.
    await page.evaluate(() => {
      const a = document.getElementById("pharos-video") as HTMLMediaElement;
      a.currentTime = Math.max(a.currentTime, a.duration - 5);
    });
    await expect
      .poll(() =>
        page.evaluate(
          () => (document.getElementById("pharos-video") as HTMLMediaElement).error?.code ?? 0,
        ),
      )
      .toBe(0);
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
      `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=AlbumArtist,Album`,
    );
    // The seeded song (05-carefree.mp3) has an album and a track artist but
    // no album artist tag, so the artist stands in for it.
    type Song = { Album?: string; AlbumArtist?: string; Artists?: string[] };
    const by = (t: Song) => t.AlbumArtist ?? t.Artists?.[0];
    const track = tracks.Items?.find((t: Song) => t.Album && by(t));
    test.skip(!track, "the seed has no track tagged with an album and artist");
    const artists = await json(`/Artists`);
    const artist = artists.Items?.find((a: { Name: string }) => a.Name === by(track));
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
    await expect(page.getByRole("button", { name: "Lyrics", exact: true })).toBeVisible();
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
    await panel.getByRole("button", { name: "Clear", exact: true }).click();
    await ask.getByRole("button", { name: "Clear queue" }).click();
    await expect(entries).toHaveCount(0);
    await expect(panel).toContainText("Nothing queued.");
    await toggle.click();
    await expect(panel).toBeHidden();
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

    await page.getByRole("link", { name: "Find" }).click();
    await page.getByLabel("Search").fill("carefree");
    const tile = page.locator(".pharos-poster").first();
    await expect(tile).toBeVisible({ timeout: 15_000 });
    await tile.click();
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
    const stops = ["Find", "Together"];
    for (let i = 0; i < 20; i++) {
      if (i % 3 === 0) {
        await page.locator(".pharos-brand").click();
      } else {
        await page.getByRole("link", { name: stops[i % 3 - 1] }).first().click();
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
      `/Users/${user.Id}/Items?Recursive=true&IncludeItemTypes=Audio&Fields=AlbumArtist,Album`,
    );
    const track = tracks.Items?.find(
      (t: { Album?: string; AlbumArtist?: string }) => t.Album && t.AlbumArtist,
    );
    test.skip(!track, "the seed has no track tagged with an album and artist");
    const artists = await json(`/Artists`);
    const artist = artists.Items?.find((a: { Name: string }) => a.Name === track.AlbumArtist);
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
    await page.getByRole("button", { name: `Add ${track.Album} to the queue` }).click();
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

    await page.getByRole("link", { name: "Find" }).click();
    await page.getByLabel("Search").fill("carefree");
    await page.locator(".pharos-poster").first().click();
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    await expect.poll(() => at(page), { timeout: 30_000 }).toBeGreaterThan(0.5);
    await page.locator(".pharos-listen").getByRole("button", { name: "Shrink" }).click();
    await expect(page.locator(".pharos-stage.is-floating")).toBeVisible();

    // A film from its title page: the page stays, the window plays it.
    await page.getByRole("link", { name: "Find" }).click();
    await page.getByLabel("Search").fill("Playwright");
    await page.locator(".pharos-poster").first().click();
    await expect(page).toHaveURL(/\/ui\/title\//);
    const title = page.url();
    await page.getByRole("link", { name: /^(Play|Resume)$/ }).click();
    const video = page.locator(".pharos-stage.is-floating .pharos-watch video");
    await expect(video).toBeVisible({ timeout: 15_000 });
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
});
