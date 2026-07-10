import { test, expect, Page } from "@playwright/test";

// SyncPlay real-client engagement proof.
//
// The sibling `syncplay.spec.ts` drives the *WebSocket* command path with
// raw sockets — it never loads jellyfin-web's own SyncPlay Manager. That is
// exactly why it stays green while the browser is broken: stock jellyfin-web
// creates groups over **HTTP** (`POST /SyncPlay/New`) and then, on the
// `GroupJoined` push, runs `enableSyncPlay` → `bindToPlayer` →
// `timeSyncCore.forceUpdate()` → `GET /GetUTCTime`. Only once that time-sync
// round-trip completes does `syncPlayReady` flip true and queued commands
// (Play/Pause/Seek) actually apply. If any step before `forceUpdate` throws,
// the group looks "joined" but never syncs and never plays — the reported bug.
//
// This test boots the REAL bundle (login → live socket → live SyncPlay
// Manager), then creates a group the way the app does, and asserts the client
// reaches the time-sync round-trip. GetUtcTime firing is the smoking gun that
// `enableSyncPlay` ran to completion.

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
  await page.waitForURL(/#\/login/, { timeout: 20_000 });
  await page.locator("#txtManualName").waitFor({ timeout: 10_000 });
  await page.locator("#txtManualName").fill(username);
  await page.locator("#txtManualPassword").fill(password);
  await page.getByRole("button", { name: /^sign in$/i }).click();
}

test.describe("syncplay real-client engagement", () => {
  test("creating a group drives the client into time-sync (GetUtcTime)", async ({
    page,
  }) => {
    test.setTimeout(90_000);

    // Collect every console line + network request so a single failing run
    // shows exactly where enableSyncPlay dies.
    const consoleLines: string[] = [];
    page.on("console", (m) => consoleLines.push(`[${m.type()}] ${m.text()}`));
    page.on("pageerror", (e) => consoleLines.push(`[pageerror] ${e.message}`));

    let getUtcTimeHits = 0;
    const syncPlayReqs: string[] = [];
    page.on("request", (r) => {
      const u = r.url();
      if (/\/GetUTCTime/i.test(u)) getUtcTimeHits += 1;
      if (/\/SyncPlay\//i.test(u)) syncPlayReqs.push(`${r.method()} ${u}`);
    });

    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    await page.waitForURL(/#\/home/, { timeout: 25_000 });

    // The socket + SyncPlay Manager are up now. Create a group exactly as the
    // "New Group" menu item does: apiClient.createSyncPlayGroup → POST
    // /SyncPlay/New with the app's own token + deviceId, so the GroupJoined
    // push comes back down THIS app's socket into its real SyncPlay Manager.
    const created = await page.evaluate(async () => {
      const api = (window as any).ApiClient;
      if (!api || typeof api.createSyncPlayGroup !== "function") {
        return { ok: false, reason: "no ApiClient.createSyncPlayGroup" };
      }
      try {
        await api.createSyncPlayGroup({ GroupName: "pw-syncplay" });
        return { ok: true };
      } catch (e: any) {
        return { ok: false, reason: String(e?.message ?? e) };
      }
    });
    expect(created.ok, `createSyncPlayGroup failed: ${created.reason}`).toBe(
      true,
    );

    // Give the GroupJoined push + enableSyncPlay + forceUpdate time to run.
    // Poll for the GetUtcTime round-trip rather than a flat sleep.
    const deadline = Date.now() + 15_000;
    while (getUtcTimeHits === 0 && Date.now() < deadline) {
      await page.waitForTimeout(200);
    }

    const syncPlayLog = consoleLines.filter((l) => /SyncPlay/i.test(l));
    const errorLog = consoleLines.filter((l) =>
      /\[(error|pageerror)\]/i.test(l),
    );

    // Diagnostics surfaced on failure.
    const diag =
      `\n--- SyncPlay /SyncPlay/* requests ---\n${syncPlayReqs.join("\n")}` +
      `\n--- SyncPlay console ---\n${syncPlayLog.join("\n")}` +
      `\n--- errors ---\n${errorLog.join("\n")}`;

    expect(
      getUtcTimeHits,
      `client never called GET /GetUTCTime after group create — ` +
        `enableSyncPlay aborted before time-sync, so syncPlayReady stays false ` +
        `and no command ever applies.${diag}`,
    ).toBeGreaterThan(0);
  });

  test("playing a video while in a group actually starts playback", async ({
    page,
    browserName,
  }) => {
    // pharos force-transcodes Firefox to VP9, and Playwright's bundled Firefox
    // can't decode VP9 (manifestIncompatibleCodecsError) — a test-env codec
    // gap, not a pharos bug (real Firefox/Zen decodes VP9 fine). Playback
    // assertions therefore run on chromium, which decodes VP9. The engagement
    // test above confirms the SyncPlay control path itself works on Firefox.
    test.skip(
      browserName === "firefox",
      "Playwright Firefox lacks VP9 decode; playback path verified on chromium",
    );
    // The real bug surface: engagement (above) works, but when the user
    // presses Play *while in a group*, jellyfin-web routes playback through
    // SyncPlay — it POSTs SetNewQueue, then WAITS for the group's Play command
    // (readiness gate) before letting the <video> roll. If the server's gate
    // never releases (e.g. it waits for a MemberReady the client never sends,
    // or the client waits for a Play the server never emits) the video wedges
    // at currentTime 0 forever: "the video never plays for us in the group".
    test.setTimeout(90_000);

    const consoleLines: string[] = [];
    page.on("console", (m) => consoleLines.push(`[${m.type()}] ${m.text()}`));
    page.on("pageerror", (e) => consoleLines.push(`[pageerror] ${e.message}`));
    const syncPlayReqs: string[] = [];
    page.on("request", (r) => {
      const p = new URL(r.url()).pathname;
      if (/\/SyncPlay\/|\/Items|\/PlaybackInfo|\/Playing/i.test(r.url())) {
        syncPlayReqs.push(`${r.method()} ${p}${new URL(r.url()).search}`);
      }
    });

    await connectToServer(page);
    await login(page, SEED_USER, SEED_PASS);
    await page.waitForURL(/#\/home/, { timeout: 25_000 });

    const serverId = await page.evaluate(() => {
      try {
        return (
          JSON.parse(window.localStorage.getItem("jellyfin_credentials")!)
            .Servers?.[0]?.Id ?? null
        );
      } catch (_e) {
        return null;
      }
    });
    expect(serverId, "serverId from localStorage").toBeTruthy();

    // Enter a SyncPlay group first, so the subsequent Play is group-routed.
    const created = await page.evaluate(async () => {
      const api = (window as any).ApiClient;
      try {
        await api.createSyncPlayGroup({ GroupName: "pw-playback" });
        return { ok: true };
      } catch (e: any) {
        return { ok: false, reason: String(e?.message ?? e) };
      }
    });
    expect(created.ok, `createSyncPlayGroup: ${created.reason}`).toBe(true);
    // Let enableSyncPlay + time-sync settle so syncPlayReady is true.
    await page.waitForTimeout(2000);

    // Now play the seeded fixture (item id=1) the way the UI does.
    await page.goto(`/#/details?id=1&serverId=${serverId}`);
    const playBtn = page.locator("button.btnPlay").first();
    await playBtn.waitFor({ timeout: 20_000 });
    await playBtn.click();

    const video = page.locator("video").first();
    await video.waitFor({ state: "attached", timeout: 30_000 });

    let advanced = false;
    try {
      await page.waitForFunction(
        () => {
          const v = document.querySelector("video") as HTMLVideoElement | null;
          return !!v && v.currentTime > 0.1;
        },
        undefined,
        { timeout: 25_000 },
      );
      advanced = true;
    } catch (_e) {
      advanced = false;
    }

    const videoState = await page.evaluate(() => {
      const v = document.querySelector("video") as HTMLVideoElement | null;
      if (!v) return "no <video> element";
      return `src=${v.currentSrc || v.src} readyState=${v.readyState} paused=${v.paused} currentTime=${v.currentTime} error=${v.error?.code ?? "none"}`;
    });
    const diag =
      `\n--- media/sync requests ---\n${syncPlayReqs.join("\n")}` +
      `\n--- video ---\n${videoState}` +
      `\n--- full console (last 70) ---\n${consoleLines.slice(-70).join("\n")}`;

    expect(
      advanced,
      `in-group playback never advanced past 0 — the readiness gate wedged ` +
        `the video.${diag}`,
    ).toBe(true);
  });
});
