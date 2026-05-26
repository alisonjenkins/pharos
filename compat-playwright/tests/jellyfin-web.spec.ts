import { test, expect, Page } from "@playwright/test";

// T29 phase 3 — drive unmodified jellyfin-web pointed at pharos.
//
// Pre-reqs:
//   * Nix devShell active → JELLYFIN_WEB_DIR set.
//   * pharos running on PHAROS_URL with the playwright seed user:
//       admin seed-playwright-user
//     (creates user "playwright" / pw "playwright-test-pw" + 4 items).
//
// Selectors are tuned for jellyfin-web 10.11.x; bumps to that version
// may break them. Prefer id selectors that map to upstream's IDs
// (txtServerHost / txtManualName / txtManualPassword / btnManual).

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const SEED_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const SEED_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

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
  // After Connect, jellyfin-web routes to #/login?serverid=… when
  // /users/public is empty (no tile picker). Wait for the form, then
  // fill + submit. No "Manual Login" intermediate click needed — that
  // only shows when the tile picker is populated.
  await page.waitForURL(/#\/login/, { timeout: 20_000 });
  await page.locator("#txtManualName").waitFor({ timeout: 10_000 });
  await page.locator("#txtManualName").fill(username);
  await page.locator("#txtManualPassword").fill(password);
  await page.getByRole("button", { name: /^sign in$/i }).click();
}

test.describe("jellyfin-web compat", () => {
  test("connect → manual login → land on home", async ({ page }) => {
    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    await page.waitForURL(/#\/home/, { timeout: 25_000 });
    await expect(page).toHaveURL(/#\/home/);
  });

  test("library has at least one card on home", async ({ page }) => {
    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    await page.waitForURL(/#\/home/, { timeout: 25_000 });
    const tile = page.locator(".card, .listItem").first();
    await tile.waitFor({ timeout: 20_000 });
    await expect(tile).toBeVisible();
  });

  test("wrong password surfaces an error", async ({ page }) => {
    await connectToServer(page);
    await login(page, SEED_USER, "definitely-not-the-pw");
    const err = page
      .getByText(/(invalid|incorrect).*(user|password)|sign in.*failed/i)
      .first();
    await err.waitFor({ timeout: 15_000 });
    await expect(err).toBeVisible();
  });

  test("real media plays through pharos in a real browser", async ({ page }) => {
    // End-to-end media-playback proof: a logged-in browser fetches the
    // seeded MP4 from pharos's direct-play endpoint, decodes it, and
    // advances currentTime past zero.
    //
    // We synthesize the <video> element directly instead of driving
    // jellyfin-web's Play button. The button now renders + clicks (see
    // commit log — needed MediaSources + MediaStreams on the item DTO),
    // but jellyfin-web's playbackManager has further endpoint expectations
    // (PlaybackInfo POST with deviceProfile, intros endpoint, etc.) that
    // are still open work. Pharos's *playback path* — auth via api_key
    // query, /Videos/{id}/stream serving real H.264/AAC bytes with the
    // right Content-Type and CORS — is what this test actually validates.
    test.setTimeout(60_000);
    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    await page.waitForURL(/#\/home/, { timeout: 25_000 });
    const token = await page.evaluate(() => {
      try {
        const raw = window.localStorage.getItem("jellyfin_credentials");
        if (!raw) return null;
        const data = JSON.parse(raw);
        return data?.Servers?.[0]?.AccessToken ?? null;
      } catch (_e) {
        return null;
      }
    });
    if (!token) {
      throw new Error("could not read AccessToken from jellyfin-web localStorage");
    }
    // Mount a hidden <video> pointing at /Videos/1/stream. Browser will
    // perform the GET (with CORS + Range), decode the fixture, and
    // expose currentTime via the standard media element API.
    await page.evaluate(
      ({ pharosBase, token }) => {
        const v = document.createElement("video");
        v.id = "pharos-playback-probe";
        v.crossOrigin = "anonymous";
        v.muted = true; // autoplay policies require muted.
        v.autoplay = true;
        v.controls = false;
        v.style.position = "fixed";
        v.style.left = "-9999px"; // off-screen but rendered.
        v.src = `${pharosBase}/Videos/1/stream?api_key=${token}&static=true`;
        document.body.appendChild(v);
        v.play().catch(() => {
          // Even if play() rejects we still measure currentTime
          // advancement via the timeupdate event.
        });
      },
      { pharosBase: PHAROS_URL, token },
    );
    const video = page.locator("#pharos-playback-probe");
    await video.waitFor({ state: "attached", timeout: 5_000 });
    await page.waitForFunction(
      () => {
        const v = document.getElementById("pharos-playback-probe") as
          | HTMLVideoElement
          | null;
        return !!v && v.currentTime > 0;
      },
      undefined,
      { timeout: 30_000 },
    );
  });
});
