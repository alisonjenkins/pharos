// Sync oracle for the SyncPlay group-watch harness. Samples every member's
// <video>.currentTime and asserts convergence within tolerance. Tolerances
// live here as the single source of truth (see the plan's Global Constraints).

export const SYNC_TOL_MS = 1500; // playing convergence: pairwise spread ceiling
export const SEEK_TOL_MS = 1000; // seek accuracy vs the target position
export const SETTLE_MS = 8000; // how long to poll for a predicate to hold
export const POLL_MS = 200; // poll interval

export interface VideoProbeLike {
  itemId: string | null;
  currentTime: number; // seconds
  paused: boolean;
  errorCode: number | null;
}

export interface Probeable {
  readonly label: string;
  probe(): Promise<VideoProbeLike>;
  diagnostics?(): string;
}

/** Max pairwise spread of the given currentTime values, in milliseconds. */
export function maxPairwiseDeltaMs(timesSec: number[]): number {
  if (timesSec.length < 2) return 0;
  return (Math.max(...timesSec) - Math.min(...timesSec)) * 1000;
}

/** True when the currentTime spread is within `tolMs`. */
export function withinTol(timesSec: number[], tolMs: number): boolean {
  return maxPairwiseDeltaMs(timesSec) <= tolMs;
}

// Poll `fn` until it returns a non-null value or the timeout elapses. Date.now
// here is wall-clock for a bounded poll loop — acceptable in test code.
async function poll<T>(
  fn: () => Promise<T | null>,
  timeoutMs: number,
  pollMs: number,
): Promise<T | null> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const v = await fn();
    if (v !== null) return v;
    if (Date.now() >= deadline) return null;
    await new Promise((r) => setTimeout(r, pollMs));
  }
}

function snapshot(people: Probeable[], probes: VideoProbeLike[]): string {
  return probes
    .map(
      (pr, i) =>
        `${people[i].label}: item=${pr.itemId} t=${pr.currentTime.toFixed(2)} ` +
        `paused=${pr.paused} err=${pr.errorCode}`,
    )
    .join("\n");
}

/** Wait until all members report the same item, are all playing, and their
 *  currentTimes are within tolerance. Throws with a per-member snapshot. */
export async function waitUntilInSync(
  people: Probeable[],
  opts: { tolMs?: number } = {},
): Promise<void> {
  const tol = opts.tolMs ?? SYNC_TOL_MS;
  let last = "";
  const ok = await poll(
    async () => {
      const probes = await Promise.all(people.map((p) => p.probe()));
      last = snapshot(people, probes);
      const ids = probes.map((p) => p.itemId);
      const allSameItem = ids.every((id) => id !== null && id === ids[0]);
      const allPlaying = probes.every((p) => !p.paused && p.errorCode === null);
      const converged = withinTol(
        probes.map((p) => p.currentTime),
        tol,
      );
      return allSameItem && allPlaying && converged ? true : null;
    },
    SETTLE_MS,
    POLL_MS,
  );
  if (!ok) {
    throw new Error(
      `members never converged (≤${tol}ms, all playing, same item):\n${last}`,
    );
  }
}

/** Wait until every member's current item equals `itemId`. */
export async function assertItemAll(
  people: Probeable[],
  itemId: string,
): Promise<void> {
  let last = "";
  const ok = await poll(
    async () => {
      const probes = await Promise.all(people.map((p) => p.probe()));
      last = snapshot(people, probes);
      return probes.every((p) => p.itemId === itemId) ? true : null;
    },
    SETTLE_MS,
    POLL_MS,
  );
  if (!ok) throw new Error(`not all members reached item ${itemId}:\n${last}`);
}

/** Wait until every member is paused. */
export async function assertPausedAll(people: Probeable[]): Promise<void> {
  let last = "";
  const ok = await poll(
    async () => {
      const probes = await Promise.all(people.map((p) => p.probe()));
      last = snapshot(people, probes);
      return probes.every((p) => p.paused) ? true : null;
    },
    SETTLE_MS,
    POLL_MS,
  );
  if (!ok) throw new Error(`not all members paused:\n${last}`);
}

/** Wait until every member's position is within SEEK_TOL_MS of `targetMs`. */
export async function assertSeekConverged(
  people: Probeable[],
  targetMs: number,
): Promise<void> {
  let last = "";
  const ok = await poll(
    async () => {
      const probes = await Promise.all(people.map((p) => p.probe()));
      last = snapshot(people, probes);
      return probes.every(
        (p) => Math.abs(p.currentTime * 1000 - targetMs) <= SEEK_TOL_MS,
      )
        ? true
        : null;
    },
    SETTLE_MS,
    POLL_MS,
  );
  if (!ok) {
    throw new Error(
      `members did not converge to seek target ${targetMs}ms (±${SEEK_TOL_MS}):\n${last}`,
    );
  }
}
