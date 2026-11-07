import { test, expect } from "@playwright/test";
import { delta, describe as line, droppedKey, eventKey, parseClientEvents } from "./client-events";

// Pure parsing, no browser. Runs in any Playwright project.
test.describe("client-events scrape", () => {
  const text = [
    "# HELP pharos_client_events_total client events",
    "# TYPE pharos_client_events_total counter",
    'pharos_client_events_total{kind="groupDrift",level="info"} 3',
    'pharos_client_events_total{kind="groupHardSeek",level="warn"} 1',
    'pharos_client_events_dropped_total{reason="rate_limited"} 4',
    'pharos_client_events_totally_other{kind="groupDrift",level="info"} 9',
    'http_requests_total{path="/metrics"} 7',
  ].join("\n");

  test("reads events by kind and level, and drops by reason", () => {
    const counts = parseClientEvents(text);
    expect(counts.get(eventKey("groupDrift", "info"))).toBe(3);
    expect(counts.get(eventKey("groupHardSeek", "warn"))).toBe(1);
    expect(counts.get(droppedKey("rate_limited"))).toBe(4);
    expect(counts.size, "other series are not client events").toBe(3);
  });

  test("a delta counts a series missing from the first scrape from 0", () => {
    const before = parseClientEvents('pharos_client_events_total{kind="groupDrift",level="info"} 3');
    const d = delta(before, parseClientEvents(text));
    expect(d.get(eventKey("groupDrift", "info"))).toBeUndefined();
    expect(d.get(eventKey("groupHardSeek", "warn"))).toBe(1);
    expect(line(d)).toBe("dropped/rate_limited=+4 groupHardSeek/warn=+1");
  });
});
