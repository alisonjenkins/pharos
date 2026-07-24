import { test } from "@playwright/test";
import { VirtualPerson } from "./lib/virtual-person";
import {
  waitUntilInSync,
  assertPausedAll,
  assertItemAll,
  assertSeekConverged,
} from "./lib/sync-oracle";

// The SyncPlay group-watch matrix: up to 3 real jellyfin-web members (each a
// real SyncPlay Manager + <video>) through every group-watch scenario. Runs on
// the FOSS nix chromium → VP9 (the codec that browser advertises). Group
// playback is started via SetNewQueue (the group-routed equivalent of the Play
// button); each member's Manager reacts over its own socket and opens its real
// player. Seed items: movie "1"; series episodes "5","6","7"; multitrack "8".

test.describe("syncplay group-watch matrix (chromium/VP9)", () => {
  test("solo play, then a late joiner promptly joins the stream playing", async ({
    browser,
  }) => {
    test.setTimeout(120_000);
    const a = await VirtualPerson.spawn(browser, 0);
    const g = await a.createGroup("m-solo-join");
    await a.setNewQueue(["1"]); // A starts playback alone
    await a.unpause();
    await waitUntilInSync([a]); // A is actually playing
    const b = await VirtualPerson.spawn(browser, 1);
    await b.joinGroup(g); // B joins mid-playback
    // B must reach the same item AND be playing — no forced pause, no wedge.
    await waitUntilInSync([a, b]);
    await a.close();
    await b.close();
  });

  test("empty group: member joins, then playback starts on everyone", async ({
    browser,
  }) => {
    test.setTimeout(120_000);
    const a = await VirtualPerson.spawn(browser, 0);
    const g = await a.createGroup("m-empty-join");
    const b = await VirtualPerson.spawn(browser, 1);
    const c = await VirtualPerson.spawn(browser, 2);
    await b.joinGroup(g);
    await c.joinGroup(g); // join while nothing plays
    await a.setNewQueue(["1"]);
    await a.unpause(); // then pick playback
    await waitUntilInSync([a, b, c]); // all three roll
    await a.close();
    await b.close();
    await c.close();
  });

  test("next/previous episode starts playback of the new episode on everyone", async ({
    browser,
  }) => {
    test.setTimeout(150_000);
    const [a, b, c] = await Promise.all(
      [0, 1, 2].map((i) => VirtualPerson.spawn(browser, i)),
    );
    const g = await a.createGroup("m-nextprev");
    await b.joinGroup(g);
    await c.joinGroup(g);
    await a.setNewQueue(["5", "6", "7"]); // 3-episode queue
    await a.unpause();
    await assertItemAll([a, b, c], "5");
    await waitUntilInSync([a, b, c]);
    await a.nextItem(); // → episode 6 on all
    await assertItemAll([a, b, c], "6");
    await waitUntilInSync([a, b, c]);
    await a.previousItem(); // → episode 5 on all
    await assertItemAll([a, b, c], "5");
    await waitUntilInSync([a, b, c]);
    await a.close();
    await b.close();
    await c.close();
  });

  test("pause pauses everyone; resume plays everyone back in sync", async ({
    browser,
  }) => {
    test.setTimeout(150_000);
    const [a, b, c] = await Promise.all(
      [0, 1, 2].map((i) => VirtualPerson.spawn(browser, i)),
    );
    const g = await a.createGroup("m-pause-resume");
    await b.joinGroup(g);
    await c.joinGroup(g);
    await a.setNewQueue(["1"]);
    await a.unpause();
    await waitUntilInSync([a, b, c]);
    await b.pause(); // any member can pause
    await assertPausedAll([a, b, c]);
    await b.unpause();
    await waitUntilInSync([a, b, c]); // re-converged, all playing
    await a.close();
    await b.close();
    await c.close();
  });

  test("seek moves everyone to the same point", async ({ browser }) => {
    test.setTimeout(150_000);
    const [a, b, c] = await Promise.all(
      [0, 1, 2].map((i) => VirtualPerson.spawn(browser, i)),
    );
    const g = await a.createGroup("m-seek");
    await b.joinGroup(g);
    await c.joinGroup(g);
    await a.setNewQueue(["1"]);
    await a.unpause();
    await waitUntilInSync([a, b, c]);
    await a.seek(3000); // seek to 3.0s (ms)
    await assertSeekConverged([a, b, c], 3000);
    await a.close();
    await b.close();
    await c.close();
  });

  test("a member losing its connection does not freeze the rest of the group", async ({
    browser,
  }) => {
    // Regression guard for the socket-drop wedge (fix: gates only wait on
    // members with a live socket). Before it, a member whose /socket dropped
    // stayed in the readiness gate's pending set for the 20s reconnect grace,
    // so a command that opened a gate froze everyone until the 30s anti-wedge.
    test.setTimeout(150_000);
    const [a, b, c] = await Promise.all(
      [0, 1, 2].map((i) => VirtualPerson.spawn(browser, i)),
    );
    const g = await a.createGroup("m-socketdrop");
    await b.joinGroup(g);
    await c.joinGroup(g);
    await a.setNewQueue(["1"]);
    await a.unpause();
    await waitUntilInSync([a, b, c]);

    // b's connection dies — the server sees its /socket drop (→ MemberSocketLost).
    await b.goOffline();
    // a opens a gate (seek). The still-connected a + c must converge AND keep
    // playing, NOT freeze for 30s waiting on the disconnected b — this is the
    // regression guard for the socket-drop wedge. (A member reconnecting and
    // resyncing is covered by the reconnect_restores_a_member_to_the_gate unit
    // test; asserting it here is fragile against the 5s fixture ending.)
    await a.seek(1000);
    await assertSeekConverged([a, c], 1000);
    await waitUntilInSync([a, c]);
    await a.close();
    await b.close();
    await c.close();
  });

  test("audio + subtitle swap on one member does not desync the group", async ({
    browser,
  }) => {
    test.setTimeout(150_000);
    const [a, b, c] = await Promise.all(
      [0, 1, 2].map((i) => VirtualPerson.spawn(browser, i)),
    );
    const g = await a.createGroup("m-swap");
    await b.joinGroup(g);
    await c.joinGroup(g);
    await a.setNewQueue(["8"]); // multitrack item (2 audio + 2 subs)
    await a.unpause();
    await waitUntilInSync([a, b, c]);
    await b.swapAudio(); // B switches to the 2nd audio track
    await waitUntilInSync([a, b, c]); // B re-converges, group intact
    await b.swapSubtitle(); // B switches subtitle track
    await waitUntilInSync([a, b, c]);
    await a.close();
    await b.close();
    await c.close();
  });
});
