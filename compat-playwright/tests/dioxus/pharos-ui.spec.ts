import { test, expect } from "@playwright/test";

// T51 phase 3 — drive the pharos Dioxus UI (served at `/` by
// `[server].ui_dir`) end-to-end through a real Chromium: the native app
// at `/` and legacy management panes at `/manage`.
//
// The native sign-in tests need `[server.native_api].enabled = true`, the
// test origin in `allowed_origins`, and https; with the API off they skip.
//
// Skips cleanly when `/` is not the UI — useful in environments where the
// WASM bundle hasn't been built. Build with:
//   nix develop --command dx build --package pharos-ui --release
// then point `[server].ui_dir` at `target/dx/pharos-ui/release/web/public`.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

test.describe("pharos Dioxus UI", () => {
  test.beforeAll(async ({ request }) => {
    const probe = await request.get(`${PHAROS_URL}/`, {
      headers: { Accept: "text/html" },
    });
    test.skip(
      !(probe.headers()["content-type"] ?? "").includes("text/html"),
      "pharos has no [server].ui_dir configured — build the bundle and re-run",
    );
  });

  test("legacy panes at /manage: login form renders, sign-in button present", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
    page.on("requestfailed", (r) => {
      // Ignore favicon misses + cancellations.
      if (!r.url().includes("favicon")) {
        errors.push(`requestfailed: ${r.url()} — ${r.failure()?.errorText}`);
      }
    });

    await page.goto("/manage");

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

  test("legacy panes at /manage: server-picker toggle reveals the picker form", async ({ page }) => {
    await page.goto("/manage");
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

  test("legacy panes at /manage: login with seeded creds reaches the library nav", async ({ page }) => {
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

    await page.goto("/manage");
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

  test.describe("native app at /", () => {
    test("root shows the native app's state", async ({ page }) => {
      const errors: string[] = [];
      page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));

      await page.goto("/");

      await expect(page.locator("header.pharos-banner h1")).toHaveText("pharos");

      const signin = page.locator("form#signin");
      const notEnabled = page.locator("section.pharos-status", {
        hasText: "This interface is not enabled",
      });
      await expect(signin.or(notEnabled)).toBeVisible({ timeout: 10_000 });

      expect(errors, errors.join("\n")).toEqual([]);
    });

    test("not-enabled page links to Manage", async ({ page, request }) => {
      const probe = await request.get(`${PHAROS_URL}/pharos/session`, {
        headers: { Accept: "application/json" },
      });
      test.skip(
        probe.status() !== 404,
        `native API is on (status ${probe.status()}); skipping not-enabled test`,
      );

      await page.goto("/");
      await page.getByRole("button", { name: "Open Manage" }).click();
      await expect(page).toHaveURL(/\/manage$/);
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

      await page.goto("/");
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

      await page.goto("/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill("definitely-wrong");
      await page.getByRole("button", { name: "Sign in" }).click();

      await expect(page.locator('[role="alert"]')).toContainText(
        /Wrong name or password|Too many failed attempts/,
      );
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

      await page.goto("/");
      await page.locator("#signin-name").fill(TEST_USER);
      await page.locator("#signin-password").fill(TEST_PASS);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.locator(".pharos-shell-bar")).toBeVisible({
        timeout: 15_000,
      });

      await page.goto("/nope");
      await expect(page.getByText("Nothing at /nope.")).toBeVisible({
        timeout: 15_000,
      });
    });
  });
});
