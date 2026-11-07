import { test, expect, APIRequestContext, BrowserContext, Page } from "@playwright/test";
import { assertPausedAll, waitUntilInSync } from "../lib/sync-oracle";
import {
  ClientEventCounts,
  clientEvents,
  delta,
  describe as line,
  eventKey,
} from "../lib/client-events";
import {
  Member,
  OPTED_IN_A,
  OPTED_IN_B,
  PHAROS_URL,
  SEEDED,
  failure,
  groupLaunchOptions,
  startGroup,
  toggleUntil,
} from "../lib/together";

// 014 T025 / 016 - watch together in the pharos Dioxus UI, read through the
// client events each member sends (spec 016) as well as its `<video>`.
//
// The members sign in as users who opted in to diagnostics (seeded by
// `admin seed-playwright-user`: together-a, together-b), so their group
// session's `groupWait`, `groupDrift` and `groupHardSeek` reach the server,
// which counts them in `/metrics`. The last case puts the seeded user, who
// has not opted in, in the group and checks that nothing of theirs is sent.
//
// Needs the server `just compat-dioxus-full` starts: `[server.native_api]`
// enabled, the seeded users and the 120 s `fixture-1.webm`.

/** `groupDrift` is sampled every 10 s (`SAMPLE_EVERY_S`) while playing. */
const DRIFT_WITHIN_MS = 30_000;

test.use({ launchOptions: groupLaunchOptions });

/** The counts that moved since `before`, scraped now. */
async function since(request: APIRequestContext, before: ClientEventCounts) {
  return delta(before, await clientEvents(request, PHAROS_URL));
}

/** Polls until `kind/level` has risen by at least `atLeast` since `before`. */
async function expectRise(
  request: APIRequestContext,
  before: ClientEventCounts,
  key: string,
  atLeast: number,
  why: string,
  timeout = 15_000,
) {
  let seen: ClientEventCounts = new Map();
  try {
    await expect
      .poll(
        async () => {
          seen = await since(request, before);
          return seen.get(key) ?? 0;
        },
        { message: why, timeout },
      )
      .toBeGreaterThanOrEqual(atLeast);
  } catch (e) {
    // The poll's message is fixed when it starts; name what was counted by the end.
    throw new Error(`${(e as Error).message}\ncounted since the step: ${line(seen)}`);
  }
}

/**
 * Runs `member`'s own `<video>` at double speed, ahead of the group's clock,
 * as a player whose clock runs off would, until the member's drift check
 * seeks it back. The rate is held each frame, so the check's rate nudge
 * cannot win and the drift outgrows `SEEK_ABOVE_MS` (1.5 s).
 *
 * Neither a jump of `currentTime` nor a faster rate (4x) will do: both run
 * the video out of data, it fires `waiting`, and a buffering member holds the
 * group instead of being corrected. A `waiting` here ends the run, so a
 * failure says which path the member took. Resolves with whether the
 * correcting seek came within `timeoutMs`, whether the video waited first,
 * and how far ahead it got.
 */
async function runAhead(member: Member, timeoutMs = 10_000) {
  return member.page.evaluate(
    (timeout) =>
      new Promise<{ seeked: boolean; waited: boolean; aheadS: number }>((resolve) => {
        const v = document.getElementById("pharos-video") as HTMLVideoElement;
        const from = v.currentTime;
        let peak = from;
        let frame = 0;
        const hold = () => {
          peak = Math.max(peak, v.currentTime);
          v.playbackRate = 2;
          frame = requestAnimationFrame(hold);
        };
        const done = (seeked: boolean, waited: boolean) => {
          cancelAnimationFrame(frame);
          clearTimeout(timer);
          v.removeEventListener("seeking", onSeek);
          v.removeEventListener("waiting", onWait);
          v.playbackRate = 1;
          resolve({ seeked, waited, aheadS: peak - from });
        };
        const onSeek = () => done(true, false);
        const onWait = () => done(false, true);
        const timer = setTimeout(() => done(false, false), timeout);
        v.addEventListener("seeking", onSeek);
        v.addEventListener("waiting", onWait);
        hold();
      }),
    timeoutMs,
  );
}

/** Waits until `member`'s video is back near `peer`'s, after running ahead. */
async function caughtUp(member: Member, peer: Member) {
  await expect
    .poll(
      async () => {
        const [m, p] = await Promise.all([member.probe(), peer.probe()]);
        return Math.abs(m.currentTime - p.currentTime);
      },
      { message: `${member.label} is seeked back to the group's position`, timeout: 20_000 },
    )
    .toBeLessThan(1.5);
}

test.describe("pharos Dioxus UI watch together: client events", () => {
  // Each try's browsers, so afterEach leaves the group even when the test
  // fails (retries run in the same server).
  let open: { context: BrowserContext; page: Page }[] = [];

  test.afterEach(async () => {
    for (const { context, page } of open) {
      // The overlay hides when the pointer rests; moving wakes it.
      await page.mouse.move(20, 20).catch(() => {});
      await page
        .locator(".pharos-members")
        .getByRole("button", { name: "Leave" })
        .click({ timeout: 5_000 })
        .catch(() => {});
      await context.close();
    }
    open = [];
  });

  test("opted-in members report the group's wait, drift and a hard seek", async ({
    browser,
    request,
  }) => {
    test.setTimeout(180_000);
    const start = await clientEvents(request, PHAROS_URL);
    const { a, b, people, logs } = await startGroup(browser, request, open, "/fixture-1.webm", {
      accounts: [OPTED_IN_A, OPTED_IN_B],
    });
    const [byA, byB] = people;
    try {
      // Both members ready, so the group plays: each reports how long it
      // waited for the start (`groupWait`).
      await waitUntilInSync(people);
      await expectRise(
        request,
        start,
        eventKey("groupWait", "info"),
        2,
        "each member reports its wait for the group's start",
      );

      // Playing, each samples its drift against the group's clock.
      const playing = await clientEvents(request, PHAROS_URL);
      await expectRise(
        request,
        playing,
        eventKey("groupDrift", "info"),
        2,
        "each member samples its drift while the group plays",
        DRIFT_WITHIN_MS,
      );

      // B runs ahead; B's own drift check seeks it back to the group (a
      // hard seek, reported as `groupHardSeek`), and A carries on.
      const drifting = await clientEvents(request, PHAROS_URL);
      const ran = await runAhead(byB);
      expect(ran, `B runs ahead and is seeked back`).toMatchObject({ seeked: true, waited: false });
      await expectRise(
        request,
        drifting,
        eventKey("groupHardSeek", "warn"),
        1,
        "B reports its hard seek back to the group",
      );
      await caughtUp(byB, byA);
      await waitUntilInSync(people);

      // A group command from A still reaches both after the correction.
      await toggleUntil(byA, true);
      await assertPausedAll(people);

      const all = await since(request, start);
      expect(all.get("dropped/rate_limited") ?? 0, `no event was refused: ${line(all)}`).toBe(0);
      expect(all.get("dropped/not_opted_in") ?? 0, `no event was refused: ${line(all)}`).toBe(0);
    } catch (e) {
      throw await failure(e, a, b, logs);
    }
  });

  test("telemetry default off: a member who has not opted in sends no events", async ({
    browser,
    request,
  }) => {
    test.setTimeout(180_000);
    // A has opted in, B (the seeded user) has not.
    const { a, b, people, logs } = await startGroup(browser, request, open, "/fixture-1.webm", {
      accounts: [OPTED_IN_A, SEEDED],
    });
    const [byA, byB] = people;
    try {
      await waitUntilInSync(people);

      // The control: A runs ahead, is seeked back and reports it, so a hard
      // seek does reach the server from a member who opted in.
      const control = await clientEvents(request, PHAROS_URL);
      const ranA = await runAhead(byA);
      expect(ranA, `A runs ahead and is seeked back`).toMatchObject({ seeked: true, waited: false });
      await expectRise(
        request,
        control,
        eventKey("groupHardSeek", "warn"),
        1,
        "A, opted in, reports its hard seek",
      );
      await caughtUp(byA, byB);
      await waitUntilInSync(people);

      // B runs ahead and is seeked back the same way, but says nothing. A's
      // drift samples in the same window show events still reach the server.
      const before = await clientEvents(request, PHAROS_URL);
      const ranB = await runAhead(byB);
      expect(ranB, `B runs ahead and is seeked back`).toMatchObject({ seeked: true, waited: false });
      await caughtUp(byB, byA);
      await expectRise(
        request,
        before,
        eventKey("groupDrift", "info"),
        1,
        "the opted-in member's drift samples still arrive",
        DRIFT_WITHIN_MS,
      );
      const after = await since(request, before);
      expect(
        after.get(eventKey("groupHardSeek", "warn")) ?? 0,
        `B's hard seek is not reported: ${line(after)}`,
      ).toBe(0);
      // Not sent at all, rather than sent and refused by the server.
      expect(
        after.get("dropped/not_opted_in") ?? 0,
        `B's client sends nothing for the server to refuse: ${line(after)}`,
      ).toBe(0);
    } catch (e) {
      throw await failure(e, a, b, logs);
    }
  });
});
