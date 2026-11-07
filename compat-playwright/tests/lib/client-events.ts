import { APIRequestContext } from "@playwright/test";

// The server's count of client events (spec 016) from its `/metrics`:
// `pharos_client_events_total{kind,level}` for every event a signed-in,
// opted-in client submitted, and `pharos_client_events_dropped_total{reason}`
// for the ones it refused. Neither is labelled by user, so a test reads them
// as deltas around the step it drives, with the suite running one test at a
// time (`workers: 1`).

const EVENTS = "pharos_client_events_total";
const DROPPED = "pharos_client_events_dropped_total";

/** One scrape: `kind/level` and `dropped/reason` keys to their counts. */
export type ClientEventCounts = Map<string, number>;

/** The key of an event kind at a level, as `ClientEventCounts` holds it. */
export const eventKey = (kind: string, level: "info" | "warn" | "error") => `${kind}/${level}`;

/** The key of a drop reason, as `ClientEventCounts` holds it. */
export const droppedKey = (reason: string) => `dropped/${reason}`;

/** Parses the client-event series out of a Prometheus text exposition. */
export function parseClientEvents(text: string): ClientEventCounts {
  const counts: ClientEventCounts = new Map();
  for (const line of text.split("\n")) {
    if (line.startsWith("#")) continue;
    const m = /^(\w+)\{([^}]*)\}\s+(\S+)/.exec(line);
    if (!m || (m[1] !== EVENTS && m[1] !== DROPPED)) continue;
    const labels = new Map(
      [...m[2].matchAll(/(\w+)="([^"]*)"/g)].map(([, k, v]) => [k, v] as [string, string]),
    );
    const key =
      m[1] === EVENTS
        ? eventKey(labels.get("kind") ?? "?", (labels.get("level") ?? "?") as "info")
        : droppedKey(labels.get("reason") ?? "?");
    counts.set(key, (counts.get(key) ?? 0) + Number(m[3]));
  }
  return counts;
}

/** Scrapes the server's client-event counts now. */
export async function clientEvents(
  request: APIRequestContext,
  baseUrl: string,
): Promise<ClientEventCounts> {
  const res = await request.get(`${baseUrl}/metrics`);
  if (!res.ok()) throw new Error(`GET /metrics answered ${res.status()}`);
  return parseClientEvents(await res.text());
}

/**
 * What changed between two scrapes, keeping only keys that moved. A series
 * absent from a scrape counts as 0: a counter shows up only once it has
 * been incremented.
 */
export function delta(before: ClientEventCounts, after: ClientEventCounts): ClientEventCounts {
  const out: ClientEventCounts = new Map();
  for (const [key, n] of after) {
    const d = n - (before.get(key) ?? 0);
    if (d !== 0) out.set(key, d);
  }
  return out;
}

/** A delta as one line, for failure messages: `groupDrift/info=+2 ...`. */
export function describe(d: ClientEventCounts): string {
  if (d.size === 0) return "(no client events counted)";
  return [...d]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([k, n]) => `${k}=${n > 0 ? "+" : ""}${n}`)
    .join(" ");
}

/** The error-level events in a delta (`panic`, `jsError`, `playbackError`, ...). */
export function errorEvents(d: ClientEventCounts): ClientEventCounts {
  return new Map([...d].filter(([key, n]) => key.endsWith("/error") && n > 0));
}
