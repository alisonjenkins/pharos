import { Browser, BrowserContext, Page } from "@playwright/test";
import { PHAROS_URL, SELECTORS, USERS } from "./handles";
import type { Probeable, VideoProbeLike } from "./sync-oracle";

// A single "virtual person": an isolated jellyfin-web browser context running
// the REAL SyncPlay Manager + real <video>. Group create/join go through
// window.ApiClient; group COMMANDS are issued via this member's own /SyncPlay/*
// REST call (window.ApiClient.ajax — identical wire effect to the OSD, so the
// server attributes them to this member and every member's real Manager reacts
// over its socket). Audio/subtitle swaps drive the real OSD menus. All socket
// traffic is captured (WebSocket wrapped before the app loads) so probe() can
// read the group's current item + PlaylistItemId the way the app sees them.

const POSITION_TICKS_PER_MS = 10_000;

export interface VideoProbe extends VideoProbeLike {
  readyState: number;
  currentSrc: string;
}

export class VirtualPerson implements Probeable {
  readonly label: string;
  private consoleLines: string[] = [];
  private netLines: string[] = [];

  private constructor(
    readonly ctx: BrowserContext,
    readonly page: Page,
    label: string,
  ) {
    this.label = label;
  }

  /** New context + login as USERS[index]; the real app opens its socket
   *  (captured) + SyncPlay Manager. */
  static async spawn(browser: Browser, index: number): Promise<VirtualPerson> {
    const creds = USERS[index];
    const ctx = await browser.newContext();
    const page = await ctx.newPage();
    const person = new VirtualPerson(ctx, page, creds.user);

    // Capture ALL socket messages before any app script runs.
    await page.addInitScript(() => {
      (window as any).__pharos_msgs = [];
      const Native = window.WebSocket;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (window as any).WebSocket = function (this: any, url: string, protocols?: any) {
        const ws = new Native(url, protocols);
        // Track the live SyncPlay `/socket` so a test can force-drop it.
        if (/\/socket(\?|$)/i.test(String(url))) {
          (window as any).__pharos_ws = ws;
        }
        ws.addEventListener("message", (ev: MessageEvent) => {
          try {
            (window as any).__pharos_msgs.push(JSON.parse(ev.data));
          } catch {
            /* non-JSON frame */
          }
        });
        return ws;
      } as unknown as typeof WebSocket;
      (window as any).WebSocket.prototype = Native.prototype;
    });

    page.on("console", (m) => person.consoleLines.push(`[${m.type()}] ${m.text()}`));
    page.on("pageerror", (e) => person.consoleLines.push(`[pageerror] ${e.message}`));
    page.on("request", (r) => {
      const u = r.url();
      if (/\/SyncPlay\/|\/PlaybackInfo|\/Playing/i.test(u)) {
        person.netLines.push(`${r.method()} ${new URL(u).pathname}`);
      }
    });

    await person.connectAndLogin(creds.user, creds.pass);
    return person;
  }

  private async connectAndLogin(user: string, pass: string): Promise<void> {
    const p = this.page;
    await p.goto("/", { waitUntil: "networkidle" });
    // Some builds land straight on login if a server is already known.
    const onSelect = await p
      .getByText(SELECTORS.addServer)
      .isVisible()
      .catch(() => false);
    if (onSelect) {
      await p.getByText(SELECTORS.addServer).click();
      const host = p.locator(SELECTORS.serverHost);
      await host.waitFor({ timeout: 10_000 });
      await host.fill(PHAROS_URL);
      await p.getByRole("button", { name: SELECTORS.connect }).click();
    }
    await p.waitForURL(/#\/login/, { timeout: 20_000 });
    await p.locator(SELECTORS.manualName).waitFor({ timeout: 10_000 });
    await p.locator(SELECTORS.manualName).fill(user);
    await p.locator(SELECTORS.manualPassword).fill(pass);
    await p.getByRole("button", { name: SELECTORS.signIn }).click();
    await p.waitForURL(/#\/(home|dashboard)/, { timeout: 25_000 });
    // ApiClient is ready once home renders.
    await p.waitForFunction(
      () => typeof (window as any).ApiClient?.accessToken === "function",
      undefined,
      { timeout: 15_000 },
    );
  }

  // ---- group membership (via the app's real ApiClient) ----

  async createGroup(name: string): Promise<string> {
    await this.page.evaluate(
      async (n) => (window as any).ApiClient.createSyncPlayGroup({ GroupName: n }),
      name,
    );
    return this.waitForGroupId();
  }

  async joinGroup(groupId: string): Promise<void> {
    await this.page.evaluate(
      async (g) => (window as any).ApiClient.joinSyncPlayGroup({ GroupId: g }),
      groupId,
    );
    await this.waitForGroupId();
  }

  private async waitForGroupId(): Promise<string> {
    const id = await this.page.waitForFunction(
      () => {
        const msgs = ((window as any).__pharos_msgs ?? []) as any[];
        const j = [...msgs]
          .reverse()
          .find(
            (m) =>
              m.MessageType === "SyncPlayGroupUpdate" &&
              m?.Data?.Type === "GroupJoined",
          );
        return j?.Data?.GroupId ?? null;
      },
      undefined,
      { timeout: 15_000 },
    );
    return (await id.jsonValue()) as string;
  }

  // ---- group commands (this member's own /SyncPlay/* REST call) ----

  private async cmd(path: string, body: Record<string, unknown>): Promise<void> {
    await this.page.evaluate(
      async ({ path, body }) => {
        const api = (window as any).ApiClient;
        await api.ajax({
          type: "POST",
          url: api.getUrl(`SyncPlay/${path}`),
          data: JSON.stringify(body),
          contentType: "application/json",
        });
      },
      { path, body },
    );
  }

  /** Start/replace the group queue with `itemIds` (decimal wire ids ok). */
  async setNewQueue(itemIds: string[], startMs = 0): Promise<void> {
    await this.cmd("SetNewQueue", {
      PlayingQueue: itemIds,
      PlayingItemPosition: 0,
      StartPositionTicks: startMs * POSITION_TICKS_PER_MS,
    });
  }

  async pause(): Promise<void> {
    await this.cmd("Pause", {});
  }

  async unpause(): Promise<void> {
    await this.cmd("Unpause", {});
  }

  async seek(positionMs: number): Promise<void> {
    await this.cmd("Seek", { PositionTicks: positionMs * POSITION_TICKS_PER_MS });
  }

  async nextItem(): Promise<void> {
    await this.cmd("NextItem", { PlaylistItemId: await this.currentPlaylistItemId() });
  }

  async previousItem(): Promise<void> {
    await this.cmd("PreviousItem", { PlaylistItemId: await this.currentPlaylistItemId() });
  }

  // ---- audio / subtitle swap via the real OSD ----

  private async openOsd(): Promise<void> {
    // The OSD auto-hides; jellyfin-web binds pointermove on `document` and
    // reveals the controls only on REAL movement — a single static move has
    // zero delta and is ignored. Wiggle across the video (steps => genuine
    // intermediate pointermove events) until the bottom bar is visible.
    const osd = this.page.locator(SELECTORS.videoOsd).first();
    for (let i = 0; i < 6; i++) {
      if (await osd.isVisible().catch(() => false)) break;
      await this.page.mouse.move(300 + i * 30, 260);
      await this.page.mouse.move(660, 430, { steps: 12 });
    }
    await osd.waitFor({ state: "visible", timeout: 10_000 });
  }

  // Open a track actionsheet (audio/subtitle) from the OSD.
  private async openTrackMenu(button: string): Promise<void> {
    await this.openOsd();
    await this.page.locator(button).first().click();
    await this.page
      .locator(SELECTORS.actionSheet)
      .waitFor({ state: "visible", timeout: 10_000 });
  }

  // Switch to the LAST menu entry: for the 2-audio fixture that is the
  // alternate track; for subtitles it enables a real track (default is Off).
  // A concrete track index is unstable (subtitle stream indices are 3/4, not
  // 1/2), so we select the alternate by menu position — deterministic switch.
  async swapAudio(): Promise<void> {
    await this.openTrackMenu(SELECTORS.osdAudioButton);
    await this.page
      .locator(`${SELECTORS.actionSheet} .actionSheetMenuItem`)
      .last()
      .click({ timeout: 10_000 });
  }

  async swapSubtitle(): Promise<void> {
    await this.openTrackMenu(SELECTORS.osdSubtitleButton);
    await this.page
      .locator(`${SELECTORS.actionSheet} .actionSheetMenuItem`)
      .last()
      .click({ timeout: 10_000 });
  }

  // ---- observation ----

  /** The playing item's PlaylistItemId, from the newest PlayQueue push. */
  async currentPlaylistItemId(): Promise<string | null> {
    return this.page.evaluate(() => {
      const msgs = ((window as any).__pharos_msgs ?? []) as any[];
      const q = [...msgs]
        .reverse()
        .find(
          (m) =>
            m.MessageType === "SyncPlayGroupUpdate" && m?.Data?.Type === "PlayQueue",
        );
      const d = q?.Data?.Data;
      if (!d?.Playlist?.length) return null;
      return d.Playlist[d.PlayingItemIndex ?? 0]?.PlaylistItemId ?? null;
    });
  }

  async probe(): Promise<VideoProbe> {
    return this.page.evaluate(() => {
      const msgs = ((window as any).__pharos_msgs ?? []) as any[];
      const q = [...msgs]
        .reverse()
        .find(
          (m) =>
            m.MessageType === "SyncPlayGroupUpdate" && m?.Data?.Type === "PlayQueue",
        );
      const d = q?.Data?.Data;
      const itemId: string | null =
        d?.Playlist?.length ? d.Playlist[d.PlayingItemIndex ?? 0]?.ItemId ?? null : null;
      const v = document.querySelector("video") as HTMLVideoElement | null;
      return {
        itemId,
        currentTime: v?.currentTime ?? 0,
        paused: v ? v.paused : true,
        errorCode: v?.error?.code ?? null,
        readyState: v?.readyState ?? 0,
        currentSrc: v?.currentSrc ?? "",
      };
    });
  }

  diagnostics(): string {
    return (
      `--- ${this.label}: /SyncPlay|/PlaybackInfo|/Playing ---\n` +
      this.netLines.slice(-40).join("\n") +
      `\n--- ${this.label}: console (last 40) ---\n` +
      this.consoleLines.slice(-40).join("\n")
    );
  }

  /** Drop this member's `/socket` the way a real connection loss does: block
   *  the WS from reconnecting, then close the live one so the server runs its
   *  disconnect handler (→ MemberSocketLost). HTTP is left working — jellyfin-web
   *  keeps POSTing commands over HTTP with no socket, exactly the observed
   *  prod failure. `setOffline` alone does NOT do this: it blocks HTTP but
   *  leaves the WebSocket up, so the server never sees the disconnect. */
  async goOffline(): Promise<void> {
    await this.page.route("**/socket**", (route) => route.abort());
    await this.page.evaluate(() => {
      const ws = (window as any).__pharos_ws as WebSocket | undefined;
      if (ws && ws.readyState <= 1) ws.close();
    });
  }

  /** Allow the socket to reconnect so jellyfin-web re-opens it and the server
   *  resyncs this member. Nudge an `online` event so its connection manager
   *  reconnects promptly instead of waiting out its retry backoff. */
  async goOnline(): Promise<void> {
    await this.page.unroute("**/socket**");
    await this.page.evaluate(() => window.dispatchEvent(new Event("online")));
  }

  async close(): Promise<void> {
    await this.ctx.close();
  }
}
