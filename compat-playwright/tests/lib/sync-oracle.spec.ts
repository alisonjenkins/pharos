import { test, expect } from "@playwright/test";
import { maxPairwiseDeltaMs, withinTol, SYNC_TOL_MS } from "./sync-oracle";

// Pure math — no browser. Runs in any Playwright project.
test.describe("sync-oracle math", () => {
  test("maxPairwiseDeltaMs is the spread in ms", () => {
    // currentTime is seconds; delta reported in ms.
    expect(maxPairwiseDeltaMs([10.0, 10.4, 10.1])).toBeCloseTo(400, 0);
    expect(maxPairwiseDeltaMs([5.0])).toBe(0);
    expect(maxPairwiseDeltaMs([])).toBe(0);
  });

  test("withinTol respects the tolerance", () => {
    expect(withinTol([10.0, 11.4], SYNC_TOL_MS)).toBe(true); // 1400ms ≤ 1500
    expect(withinTol([10.0, 11.6], SYNC_TOL_MS)).toBe(false); // 1600ms > 1500
  });
});
