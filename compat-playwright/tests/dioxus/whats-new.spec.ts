import { test, expect, type APIRequestContext, type Page } from "@playwright/test";

// Spec 027: after sign-in, a new account is walked through setup, and an
// existing one through the settings added since it last looked, each with a
// live demo, then new features and fixes. The cases sign in as fresh
// accounts, so they see setup; the what's-new pages are covered by the
// view's SSR tests. It opens by itself only right after the page loads or the
// viewer signs in, and never over playback; otherwise the top bar's badge
// opens it (V495). Each case signs in as a
// fresh account, whose `whats_new.seen` pref is unset.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const TEST_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const TEST_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";
const PASSWORD = "whats-new-pass";

// The suite's config turns what's new off on this device; these cases are
// the ones that want it.
test.use({ storageState: { cookies: [], origins: [] } });

type Admin = { token: string; userId: string };

async function admin(request: APIRequestContext): Promise<Admin> {
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
      "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-whatsnew-${Date.now()}", Version="0"`,
    },
    data: { Username: TEST_USER, Pw: TEST_PASS },
  });
  test.skip(!auth.ok(), `pharos has no seeded test user (${TEST_USER})`);
  const { AccessToken: token, User: user } = await auth.json();
  return { token, userId: user.Id };
}

async function newUser(request: APIRequestContext, a: Admin, tag: string): Promise<string> {
  const name = `pw-whatsnew-${tag}-${Date.now()}`;
  const created = await request.post(`${PHAROS_URL}/Users/New`, {
    headers: { "X-Emby-Token": a.token, "Content-Type": "application/json" },
    data: { Name: name, Password: PASSWORD },
  });
  expect(created.ok()).toBeTruthy();
  return name;
}

async function signIn(page: Page, name: string): Promise<void> {
  await page.locator("#signin-name").fill(name);
  await page.locator("#signin-password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
}

const dialog = (page: Page) => page.locator(".pharos-whats-new[role=dialog]");
const badge = (page: Page) =>
  page.getByRole("button", { name: /^(What's new: \d+ new thing|Finish setting up Pharos)/ });

test("a new account is walked through setup with live demos, then never again", async ({
  page,
  request,
}) => {
  const name = await newUser(request, await admin(request), "done");
  await page.goto("/ui/");
  await signIn(page, name);
  const box = dialog(page);
  await expect(box).toBeVisible({ timeout: 15_000 });
  await expect(box).toHaveAttribute("data-flow", "setup");
  await expect(box.getByRole("heading", { name: "Welcome to Pharos" })).toBeVisible();
  // Focus lands inside, so a keyboard or TV remote can act on it.
  await expect(box.locator("#pharos-whats-new-next")).toBeFocused();
  await box.getByRole("button", { name: "Next" }).click();

  // A theme applies to the page as it is picked.
  await expect(box.getByRole("heading", { name: "Pick a theme" })).toBeVisible();
  const other = box
    .locator(".pharos-whats-new-theme[aria-checked=false]")
    .first();
  const theme = await other.getAttribute("data-choice");
  await other.click();
  await expect(box.locator(`[data-choice="${theme}"]`)).toHaveAttribute("aria-checked", "true");
  await box.getByRole("button", { name: "Next" }).click();

  await expect(box.getByRole("heading", { name: "Subtitle size" })).toBeVisible();
  await box.getByRole("radio", { name: "Large", exact: true }).click();
  await expect(box.locator(".pharos-whats-new-subs")).toHaveAttribute(
    "data-subtitle-size",
    "large",
  );
  await expect(page.locator("html")).toHaveAttribute("data-subtitle-size", "large");
  await box.getByRole("button", { name: "Next" }).click();

  await expect(box.getByRole("heading", { name: "Library card transitions" })).toBeVisible();
  await box.getByRole("radio", { name: "Pop" }).click();
  await expect(box.getByRole("radio", { name: "Pop" })).toHaveAttribute("aria-checked", "true");
  await expect(box.locator(".pharos-whats-new-demo")).toHaveAttribute(
    "data-library-transition",
    "pop",
  );
  // The demo's cards run the chosen arrival.
  await expect
    .poll(() =>
      box.locator(".pharos-whats-new-demo .pharos-arrive").first().evaluate(
        (el) => getComputedStyle(el).animationName,
      ),
    )
    .toBe("pharos-arrive-pop");
  expect(await page.evaluate(() => localStorage.getItem("pharos.library.transition"))).toBe("pop");
  await expect(page.locator("html")).toHaveAttribute("data-library-transition", "pop");
  await box.getByRole("button", { name: "Next" }).click();

  // The controls effect plays on a stand-in player that hides its chrome
  // and brings it back on a loop.
  await expect(box.getByRole("heading", { name: "Controls effect" })).toBeVisible();
  await box.getByRole("radio", { name: "Burn" }).click();
  await expect(page.locator("html")).toHaveAttribute("data-overlay-fx", "burn");
  expect(await page.evaluate(() => localStorage.getItem("pharos.overlay.effect"))).toBe("burn");
  const player = box.locator(".pharos-whats-new-player");
  await expect(player).toHaveClass(/is-idle/, { timeout: 5_000 });
  await expect(player).not.toHaveClass(/is-idle/, { timeout: 5_000 });

  await box.getByRole("button", { name: "Start watching" }).click();
  await expect(box).toHaveCount(0);
  await expect(badge(page)).toHaveCount(0);

  // Seen is kept on the server: a reload signs in again and shows nothing.
  await page.reload();
  await expect(page.locator(".pharos-shell-bar")).toBeVisible({ timeout: 15_000 });
  await page.waitForTimeout(2_000);
  await expect(dialog(page)).toHaveCount(0);
  await expect(badge(page)).toHaveCount(0);
});

test("the arrow keys stay inside the dialog and never walk out to the page behind (B711)", async ({
  page,
  request,
}) => {
  const name = await newUser(request, await admin(request), "keys");
  await page.goto("/ui/");
  await signIn(page, name);
  const box = dialog(page);
  await expect(box.locator("#pharos-whats-new-next")).toBeFocused({ timeout: 15_000 });
  const inside = () =>
    page.evaluate(() => !!document.activeElement?.closest(".pharos-whats-new[role=dialog]"));
  for (const key of ["ArrowDown", "ArrowDown", "ArrowRight", "ArrowLeft", "ArrowUp", "ArrowUp"]) {
    await page.keyboard.press(key);
    expect(await inside(), `focus left the dialog after ${key}`).toBe(true);
  }
  // Left from Next reaches its neighbour, so the dialog is still walkable.
  await box.locator("#pharos-whats-new-next").focus();
  await page.keyboard.press("ArrowLeft");
  await expect(box.getByRole("button", { name: "Remind me later" })).toBeFocused();
});

test("Remind me later leaves the badge, and a reload splashes again", async ({
  page,
  request,
}) => {
  const name = await newUser(request, await admin(request), "later");
  await page.goto("/ui/");
  await signIn(page, name);
  const box = dialog(page);
  await expect(box).toBeVisible({ timeout: 15_000 });
  await box.getByRole("button", { name: "Remind me later" }).click();
  await expect(box).toHaveCount(0);
  await badge(page).click();
  await expect(box).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(box).toHaveCount(0);
  await expect(badge(page)).toBeVisible();
  await page.reload();
  await expect(dialog(page)).toBeVisible({ timeout: 15_000 });
});

test("a page loaded on the watch page never splashes; the badge opens it", async ({
  page,
  request,
}) => {
  const a = await admin(request);
  const name = await newUser(request, a, "watch");
  const videos = await (
    await request.get(
      `${PHAROS_URL}/Users/${a.userId}/Items?Recursive=true&IncludeItemTypes=Movie&Limit=1`,
      { headers: { "X-Emby-Token": a.token } },
    )
  ).json();
  const movie = (videos.Items ?? [])[0];
  test.skip(!movie, "pharos has no seeded movie");
  const nativeId = BigInt(`0x${movie.Id}`).toString();

  // A page opened (or reloaded after a deploy) on the watch page.
  await page.goto(`/ui/watch/${nativeId}`);
  await signIn(page, name);
  await expect(page.locator("video")).toBeVisible({ timeout: 20_000 });
  await page.waitForTimeout(3_000);
  await expect(dialog(page)).toHaveCount(0);

  // Back to browsing in the app, the video still playing in its window:
  // the badge offers what's new, and nothing opens by itself.
  await page.evaluate(() => {
    history.pushState(null, "", "/ui/");
    dispatchEvent(new PopStateEvent("popstate"));
  });
  await expect(badge(page)).toBeVisible({ timeout: 15_000 });
  await page.waitForTimeout(2_000);
  await expect(dialog(page)).toHaveCount(0);
  await badge(page).click();
  await expect(dialog(page)).toBeVisible();
});
