import { test, expect } from "@playwright/test";

// T51 phase 3 — drive the pharos Dioxus UI (served at `/ui/*` by
// `[server].ui_dir`) end-to-end through a real Chromium.
//
// Skips cleanly when `/ui/` 404s — useful in environments where the
// WASM bundle hasn't been built. Build with:
//   nix develop --command dx build --package pharos-ui --release
// then point `[server].ui_dir` at `target/dx/pharos-ui/release/web/public`.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

test.describe("pharos Dioxus UI", () => {
  test.beforeAll(async ({ request }) => {
    const probe = await request.get(`${PHAROS_URL}/ui/`);
    test.skip(
      probe.status() === 404,
      "pharos has no [server].ui_dir configured — build the bundle and re-run",
    );
  });

  test("login form renders, sign-in button present", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
    page.on("requestfailed", (r) => {
      // Ignore favicon misses + cancellations.
      if (!r.url().includes("favicon")) {
        errors.push(`requestfailed: ${r.url()} — ${r.failure()?.errorText}`);
      }
    });

    await page.goto("/ui/");

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

  test("server-picker toggle reveals the picker form", async ({ page }) => {
    await page.goto("/ui/");
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

  test("login with seeded creds reaches the library nav", async ({ page }) => {
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

    await page.goto("/ui/");
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
});
