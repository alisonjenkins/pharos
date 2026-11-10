#!/usr/bin/env node
// Spec 031 T031-17: one Markdown table from the HLS bench's JSON runs
// (tests/dioxus/hls-bench.spec.ts), a row per browser and engine, newest
// run of each pair. Usage: node tools/hls-bench-compare.cjs [dir]
"use strict";
const fs = require("node:fs");
const path = require("node:path");

const dir = process.argv[2] ?? "test-results/hls-bench";
const runs = fs
  .readdirSync(dir)
  .filter((f) => f.endsWith(".json"))
  .map((f) => JSON.parse(fs.readFileSync(path.join(dir, f), "utf8")));
if (runs.length === 0) {
  console.error(`hls-bench-compare: no runs in ${dir}`);
  process.exit(1);
}

const newest = new Map();
for (const r of runs) {
  const key = `${r.browser}\u0000${r.engine}`;
  if (!newest.has(key) || newest.get(key).recorded_at < r.recorded_at)
    newest.set(key, r);
}

/** Seconds buffered ahead of the play head in one sample. */
function ahead(s) {
  const range = s.buffered.find(
    ([a, b]) => a <= s.current_s + 0.1 && s.current_s <= b,
  );
  return range ? range[1] - s.current_s : 0;
}

/** Samples where playback should have moved and did not. */
function stuck(samples) {
  let n = 0;
  for (let i = 1; i < samples.length; i++) {
    const [a, b] = [samples[i - 1], samples[i]];
    if (!a.paused && !b.paused && b.current_s - a.current_s < 0.1) n += 1;
  }
  return n;
}

const fmt = (v, unit = "") =>
  v === null || v === undefined ? "n/a" : `${v}${unit}`;
const rows = [...newest.values()].sort((a, b) =>
  a.browser === b.browser
    ? a.engine.localeCompare(b.engine)
    : a.browser.localeCompare(b.browser),
);

console.log(
  `| Browser | Engine | First frame | Seek to playing | Forward buffer (mean / min) | Stuck seconds | Long tasks (count / max) | JS heap max | Errors | Recorded |`,
);
console.log(`|---|---|---|---|---|---|---|---|---|---|`);
for (const r of rows) {
  // From the seek on, the buffer refills from the new point; count it all.
  const fwd = r.samples.map(ahead);
  const mean = fwd.reduce((a, b) => a + b, 0) / Math.max(1, fwd.length);
  const heaps = r.samples.map((s) => s.heap_bytes).filter((h) => h !== null);
  const lt = r.long_tasks;
  console.log(
    `| ${r.browser} ${r.browser_version} | ${r.engine} | ${fmt(r.first_frame_ms, " ms")} | ${fmt(r.seek?.to_playing_ms, " ms")} | ${mean.toFixed(1)} s / ${Math.min(...fwd).toFixed(1)} s | ${stuck(r.samples)} | ${lt ? `${lt.count} / ${Math.round(lt.max_ms)} ms` : "n/a"} | ${heaps.length ? `${(Math.max(...heaps) / 1048576).toFixed(1)} MiB` : "n/a"} | ${r.errors.length} | ${r.recorded_at} |`,
  );
}
