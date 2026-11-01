// V189 / V191 — item images and subtitles require a signed-in user, and
// unmodified jellyfin-web still renders them: its `<img>` and `<track>`
// fetches send no header and authenticate with the `JellyfinAuth` cookie
// set at sign-in. An anonymous fetch of the same URLs answers 401.

import { test, expect, Page, request } from "@playwright/test";

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const SEED_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const SEED_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

// Seed id 8: VP9 + two Opus + two WebVTT tracks (streams 3 and 4).
const SUBBED = (8).toString(16).padStart(32, "0");
const SUBTITLE_PATH = `/Videos/${SUBBED}/${SUBBED}/Subtitles/3/Stream.vtt`;
const IMAGE_PATH = `/Items/${SUBBED}/Images/Primary`;
const POSTER_PATH = `/Items/${(1).toString(16).padStart(32, "0")}/Images/Primary?maxWidth=300`;

async function signIn(page: Page) {
  await page.goto("/", { waitUntil: "networkidle" });
  const addServer = page.getByText(/add server/i).first();
  if (await addServer.isVisible({ timeout: 5_000 }).catch(() => false)) {
    await addServer.click();
    await page.locator("#txtServerHost").fill(PHAROS_URL);
    await page.getByRole("button", { name: /^connect$/i }).click();
    await page.waitForURL(/#\/login/, { timeout: 20_000 });
  }
  await page.locator("#txtManualName").fill(SEED_USER);
  await page.locator("#txtManualPassword").fill(SEED_PASS);
  await page.getByRole("button", { name: /^sign in$/i }).click();
  await page.waitForURL(/#\/home/, { timeout: 20_000 });
}

test("signed-in jellyfin-web loads posters and a subtitle track", async ({ page }) => {
  const IMAGE = /\/Items\/[0-9a-f]{32}\/Images\//i;
  const SUBTITLE = /\/Videos\/[0-9a-f]{32}\/[0-9a-f]{32}\/(Subtitles|Attachments)\//i;
  const images: string[] = [];
  page.on("response", (resp) => {
    if (IMAGE.test(resp.url()) || SUBTITLE.test(resp.url())) {
      images.push(`${resp.status()} ${resp.url()}`);
    }
  });
  await signIn(page);
  // Cards fetch their artwork lazily after the home view settles.
  await page.waitForResponse((r) => IMAGE.test(r.url()), { timeout: 30_000 });
  await page.waitForLoadState("networkidle");

  const cookies = await page.context().cookies();
  const auth = cookies.find((c) => c.name === "JellyfinAuth");
  expect(auth, "JellyfinAuth set at sign-in").toBeTruthy();
  expect(auth!.httpOnly).toBe(true);
  expect(auth!.sameSite).toBe("Strict");

  // Browse the views that render artwork, including the display-preferences
  // preview, then require that no image or subtitle URL answered 401.
  for (const route of [
    "#/home",
    `#/details?id=${SUBBED}`,
    `#/details?id=${(1).toString(16).padStart(32, "0")}`,
    "#/mypreferencesmenu",
    "#/mypreferencesdisplay",
    "#/home",
  ]) {
    await page.goto(`/${route}`);
    await page.waitForLoadState("networkidle");
  }
  expect(images.length, "browse fetched no artwork").toBeGreaterThan(0);
  expect(images.filter((l) => l.startsWith("401 ")), images.join("\n")).toEqual([]);

  // An <img> sends no header either: seed movie 1's poster renders on the
  // cookie alone.
  const width = await page.evaluate(async (src) => {
    const img = new Image();
    img.src = src;
    await img.decode().catch(() => undefined);
    return img.naturalWidth;
  }, POSTER_PATH);
  expect(width, POSTER_PATH).toBeGreaterThan(0);

  // A <track> element sends no header: only the cookie can authenticate it.
  const cues = await page.evaluate(async (src) => {
    const video = document.createElement("video");
    const track = document.createElement("track");
    track.kind = "subtitles";
    track.default = true;
    track.src = src;
    video.appendChild(track);
    document.body.appendChild(video);
    track.track.mode = "showing";
    const outcome = await new Promise<string>((resolve) => {
      track.addEventListener("load", () => resolve("load"));
      track.addEventListener("error", () => resolve("error"));
      setTimeout(() => resolve("timeout"), 10_000);
    });
    const n = track.track.cues?.length ?? 0;
    video.remove();
    return `${outcome}:${n}`;
  }, SUBTITLE_PATH);
  expect(cues).toMatch(/^load:[1-9]/);
});

test("anonymous requests for an image or a subtitle answer 401", async () => {
  const anon = await request.newContext({ baseURL: PHAROS_URL });
  for (const path of [IMAGE_PATH, SUBTITLE_PATH]) {
    const resp = await anon.get(path);
    expect(resp.status(), path).toBe(401);
  }
  await anon.dispose();
});
