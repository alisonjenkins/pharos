import { test, expect } from "@playwright/test";

// B53 — two DIFFERENT users on the SAME browser deviceId must be distinct
// SyncPlay members. jellyfin-web derives deviceId from the browser, so
// Alison + Lace + Jana on the same Firefox all send the same deviceId; before
// the fix they collapsed into ONE member and fought over the socket, so the
// group could never hold them all and playback wedged.
//
// This drives the real HTTP + WebSocket path (browser-native WebSocket inside
// page.evaluate), with BOTH users opening `/socket` under the SAME deviceId.
// The proof: A creates a group, B joins, A hits Play, and BOTH receive the
// Unpause command — impossible if they'd collided into one member.

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const WS_URL = PHAROS_URL.replace(/^http/, "ws");
const USER_A = process.env.PHAROS_TEST_USER ?? "playwright";
const PASS_A = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";
const USER_B = process.env.PHAROS_TEST_USER2 ?? "playwright2";
const PASS_B = process.env.PHAROS_TEST_PASS2 ?? "playwright2-test-pw";

// The COLLIDING deviceId — identical for both users (as a shared browser
// build would send). Pre-fix this is the whole bug.
const SHARED_DEVICE_ID = "shared-browser-device-id-b53";

interface Msg {
  MessageType: string;
  Data?: any;
}

async function authenticate(
  page: import("@playwright/test").Page,
  user: string,
  pass: string,
): Promise<string> {
  const token = await page.evaluate(
    async ({ base, user, pass, dev }) => {
      const res = await fetch(`${base}/Users/AuthenticateByName`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="pw", Device="pw", DeviceId="${dev}", Version="0"`,
        },
        body: JSON.stringify({ Username: user, Pw: pass }),
      });
      if (!res.ok) throw new Error(`auth ${user} failed ${res.status}`);
      return (await res.json()).AccessToken as string;
    },
    { base: PHAROS_URL, user, pass, dev: SHARED_DEVICE_ID },
  );
  if (!token) throw new Error(`no token for ${user}`);
  return token;
}

// Open `/socket` with BOTH the token AND the shared deviceId — the collision
// condition. Records messages on window.__pharos_msgs.
async function openSocket(
  page: import("@playwright/test").Page,
  token: string,
): Promise<void> {
  await page.evaluate(
    async ({ wsUrl, token, dev }) => {
      (window as any).__pharos_msgs = [];
      const url = `${wsUrl}/socket?api_key=${encodeURIComponent(
        token,
      )}&deviceId=${encodeURIComponent(dev)}`;
      const ws = new WebSocket(url);
      (window as any).__pharos_ws = ws;
      ws.addEventListener("message", (ev) => {
        try {
          (window as any).__pharos_msgs.push(JSON.parse(ev.data));
        } catch {
          /* ignore */
        }
      });
      await new Promise<void>((resolve, reject) => {
        ws.addEventListener("open", () => resolve());
        ws.addEventListener("error", () => reject(new Error("ws error")));
      });
    },
    { wsUrl: WS_URL, token, dev: SHARED_DEVICE_ID },
  );
}

async function send(
  page: import("@playwright/test").Page,
  msg: Msg,
): Promise<void> {
  await page.evaluate((msg) => {
    (window as any).__pharos_ws.send(JSON.stringify(msg));
  }, msg as any);
}

async function socketOpen(
  page: import("@playwright/test").Page,
): Promise<boolean> {
  return page.evaluate(
    () => (window as any).__pharos_ws?.readyState === 1,
  );
}

async function waitFor(
  page: import("@playwright/test").Page,
  predicate: (m: Msg) => boolean,
  timeoutMs = 8000,
): Promise<Msg> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const found = await page.evaluate((src) => {
      const msgs = (window as any).__pharos_msgs as Msg[];
      const fn = new Function("m", `return (${src})(m);`);
      return msgs.find((m) => fn(m)) ?? null;
    }, predicate.toString());
    if (found) return found as Msg;
    await page.waitForTimeout(50);
  }
  throw new Error(`predicate did not match within ${timeoutMs}ms`);
}

test.describe("syncplay two users, same device (B53)", () => {
  test("distinct members on a shared deviceId — leader Play reaches both", async ({
    browser,
  }) => {
    test.setTimeout(60_000);
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    const pageA = await ctxA.newPage();
    const pageB = await ctxB.newPage();
    await pageA.goto(`${PHAROS_URL}/`);
    await pageB.goto(`${PHAROS_URL}/`);

    const tokenA = await authenticate(pageA, USER_A, PASS_A);
    const tokenB = await authenticate(pageB, USER_B, PASS_B);
    // Sanity: two DIFFERENT users (else the test proves nothing).
    expect(tokenA).not.toBe(tokenB);

    await openSocket(pageA, tokenA);
    await openSocket(pageB, tokenB);

    // A creates a group.
    await send(pageA, { MessageType: "SyncPlayCreateGroup", Data: {} });
    const joinedA = await waitFor(
      pageA,
      (m) =>
        m.MessageType === "SyncPlayGroupUpdate" &&
        m?.Data?.Type === "GroupJoined",
    );
    const groupId = joinedA.Data!.GroupId as string;
    expect(groupId).toBeTruthy();

    // B joins the SAME group under the SAME deviceId.
    await send(pageB, {
      MessageType: "SyncPlayJoinGroup",
      Data: { GroupId: groupId },
    });
    await waitFor(
      pageB,
      (m) =>
        m.MessageType === "SyncPlayGroupUpdate" &&
        m?.Data?.Type === "GroupJoined",
    );

    // Both sockets must still be open — pre-fix, B's connect (same member id)
    // bumped A into the disconnect/reconnect war.
    expect(await socketOpen(pageA)).toBe(true);
    expect(await socketOpen(pageB)).toBe(true);

    // A hits Play. If A and B are the SAME member (the bug), B never gets its
    // own copy of the command. Post-fix both receive Unpause.
    await send(pageA, {
      MessageType: "SyncPlayPlay",
      Data: { PlaybackPositionTicks: 0 },
    });
    const cmdA = await waitFor(
      pageA,
      (m) =>
        m.MessageType === "SyncPlayCommand" && m?.Data?.Command === "Unpause",
    );
    const cmdB = await waitFor(
      pageB,
      (m) =>
        m.MessageType === "SyncPlayCommand" && m?.Data?.Command === "Unpause",
    );
    expect(cmdA.Data!.Command).toBe("Unpause");
    expect(cmdB.Data!.Command).toBe("Unpause");

    // And both are STILL connected after the exchange.
    expect(await socketOpen(pageA)).toBe(true);
    expect(await socketOpen(pageB)).toBe(true);

    await ctxA.close();
    await ctxB.close();
  });
});
