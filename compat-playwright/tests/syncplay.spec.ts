import { test, expect } from "@playwright/test";

// T49 — SyncPlay multi-context proof. Two browser contexts open
// independent `/socket` WebSockets, exchange Jellyfin-shaped messages,
// and assert both members receive the same `SyncPlayCommand` after the
// leader issues Play.
//
// We don't drive jellyfin-web's SyncPlay UI — it's not always rendered
// (depends on cast plugin + Sessions broadcast) and the test would
// become a regression sieve for unrelated DOM churn. Instead we open
// raw `WebSocket`s from inside each `page.evaluate`, which still uses
// the real browser stack (TLS / Origin / Sec-WebSocket-Protocol).

const PHAROS_URL = process.env.PHAROS_URL ?? "http://127.0.0.1:8096";
const SEED_USER = process.env.PHAROS_TEST_USER ?? "playwright";
const SEED_PASS = process.env.PHAROS_TEST_PASS ?? "playwright-test-pw";

const WS_URL = PHAROS_URL.replace(/^http/, "ws");

interface PharosWsMessage {
  MessageType: string;
  MessageId?: string;
  Data?: any;
}

/// Drive a single `/socket` connection from inside a page. The returned
/// promise resolves once `predicate` matches a received message.
async function authenticate(
  page: import("@playwright/test").Page,
): Promise<string> {
  const token = await page.evaluate(
    async ({ base, user, pass }) => {
      const res = await fetch(`${base}/Users/AuthenticateByName`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Emby-Authorization": `MediaBrowser Client="playwright", Device="pw", DeviceId="pw-${Date.now()}", Version="0"`,
        },
        body: JSON.stringify({ Username: user, Pw: pass }),
      });
      if (!res.ok) throw new Error(`auth failed ${res.status}`);
      const body = await res.json();
      return body.AccessToken as string;
    },
    { base: PHAROS_URL, user: SEED_USER, pass: SEED_PASS },
  );
  if (!token) throw new Error("no AccessToken");
  return token;
}

/// Open a WS connection and collect messages into a global buffer the
/// outer test can pull from with `getMessages`. The WS reference is
/// stashed on `window.__pharos_ws` so subsequent `send` calls can reach
/// it. Returns once the WS reports OPEN.
async function openSocket(
  page: import("@playwright/test").Page,
  wsUrl: string,
  token: string,
): Promise<void> {
  await page.evaluate(
    async ({ wsUrl, token }) => {
      (window as any).__pharos_msgs = [];
      const ws = new WebSocket(`${wsUrl}/socket?api_key=${encodeURIComponent(token)}`);
      (window as any).__pharos_ws = ws;
      ws.addEventListener("message", (ev) => {
        try {
          (window as any).__pharos_msgs.push(JSON.parse(ev.data));
        } catch {
          /* ignore non-JSON */
        }
      });
      await new Promise<void>((resolve, reject) => {
        ws.addEventListener("open", () => resolve());
        ws.addEventListener("error", () => reject(new Error("ws error")));
      });
    },
    { wsUrl, token },
  );
}

async function send(
  page: import("@playwright/test").Page,
  msg: PharosWsMessage,
): Promise<void> {
  await page.evaluate((msg) => {
    const ws = (window as any).__pharos_ws as WebSocket;
    ws.send(JSON.stringify(msg));
  }, msg as any);
}

async function waitFor(
  page: import("@playwright/test").Page,
  predicate: (m: PharosWsMessage) => boolean,
  timeoutMs = 5000,
): Promise<PharosWsMessage> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const found = await page.evaluate((src) => {
      const msgs = (window as any).__pharos_msgs as PharosWsMessage[];
      const fn = new Function("m", `return (${src})(m);`);
      return msgs.find((m) => fn(m)) ?? null;
    }, predicate.toString());
    if (found) return found as PharosWsMessage;
    await page.waitForTimeout(50);
  }
  throw new Error(`predicate did not match within ${timeoutMs}ms`);
}

test.describe("syncplay multi-context", () => {
  test("two browser contexts join group, leader play reaches both", async ({
    browser,
  }) => {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    const pageA = await ctxA.newPage();
    const pageB = await ctxB.newPage();
    // Navigate to PHAROS_URL so subsequent fetch / WS calls run with
    // the pharos origin — about:blank can't make cross-origin fetches.
    // `/` returns "pharos" (router root); any path that 200s is fine.
    await pageA.goto(`${PHAROS_URL}/`);
    await pageB.goto(`${PHAROS_URL}/`);

    const tokenA = await authenticate(pageA);
    const tokenB = await authenticate(pageB);

    await openSocket(pageA, WS_URL, tokenA);
    await openSocket(pageB, WS_URL, tokenB);

    // A creates a group; capture the GroupId from the GroupJoined update.
    await send(pageA, { MessageType: "SyncPlayCreateGroup", Data: {} });
    const joinedA = await waitFor(
      pageA,
      (m) =>
        m.MessageType === "SyncPlayGroupUpdate" &&
        m?.Data?.Type === "GroupJoined",
    );
    const groupId = joinedA.Data!.GroupId as string;
    expect(groupId).toBeTruthy();

    // B joins the same group.
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

    // A is leader (first joiner; lowest MemberId election ensures it).
    // Send Play and assert BOTH contexts receive a SyncPlayCommand
    // Unpause.
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

    await ctxA.close();
    await ctxB.close();
  });
});
