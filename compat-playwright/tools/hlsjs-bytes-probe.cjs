#!/usr/bin/env node
// Replay CAPTURED segment bytes through real hls.js in a real browser.
//
// The question this answers is the one a byte-level inspection cannot: "would
// a player actually choke on what pharos served?". Point it at a directory
// captured by `capture-segments.sh` and it builds a playlist over those exact
// bytes, loads it with the same hls.js jellyfin-web ships, and reports whether
// playback advanced — and whether the player refetched a fragment, which is
// the signature of a segment it cannot use (production served one segment 17
// times in a single second).
//
// Usage:
//   hlsjs-bytes-probe.cjs <capture-dir> [--rung h264] [--audio aud]
//                         [--browser playwright-firefox|playwright-chromium|system-firefox]
//                         [--codecs 'avc1.640028,opus'] [--secs 20]
//
// `--browser system-firefox` drives the Firefox on PATH rather than
// Playwright's: Playwright's build ships NO H.264 decoder, so it cannot judge
// the h264 rung at all — the exact cell a user on desktop Firefox occupies.
'use strict';
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFile } = require('child_process');
const lib = require('./hlsjs-lib.cjs');

function arg(name, dflt) {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1] : dflt;
}

const root = path.resolve(process.argv[2] || '.');
const rung = arg('rung', 'h264');
const audioRung = arg('audio', fs.existsSync(path.join(root, 'aud')) ? 'aud' : null);
const browser = arg('browser', 'playwright-firefox');
const secs = Number(arg('secs', 20));
const codecs = arg('codecs', rung === 'vp9' ? 'vp09.00.10.08' : 'avc1.640028') + (audioRung ? ',opus' : '');

if (!fs.existsSync(path.join(root, rung, 'init.mp4'))) {
  console.error(`no ${rung}/init.mp4 under ${root} — capture it first (tools/capture-segments.sh)`);
  process.exit(2);
}

const page =
  lib.probePage(lib.hlsSource(), `/master-${rung}.m3u8`, secs) +
  // The system-firefox run cannot be driven from node, so the page posts its
  // own verdict back. Harmless under Playwright, which reads `window.__probe`.
  `<script>window.__probe.then(o => fetch('/result', { method: 'POST', body: JSON.stringify(o) }));</script>`;

let onResult = null;
const server = lib.serve(root, {
  audioRung,
  rungCodecs: { [rung]: codecs },
  route(req, res, rel) {
    if (rel === '/probe.html') {
      res.writeHead(200, { 'content-type': 'text/html' }).end(page);
      return true;
    }
    if (req.method === 'POST' && rel === '/result') {
      let body = '';
      req.on('data', (c) => (body += c));
      req.on('end', () => {
        res.writeHead(204).end();
        if (onResult) onResult(JSON.parse(body));
      });
      return true;
    }
    return false;
  },
});

/** Playwright browsers: fast, but no H.264 in the Firefox build. */
async function viaPlaywright(which) {
  const pw = require('@playwright/test');
  const bt = which === 'playwright-chromium' ? pw.chromium : pw.firefox;
  const b = await bt.launch();
  const p = await b.newPage();
  await p.goto(`http://127.0.0.1:${server.address().port}/probe.html`);
  const out = await p.evaluate(() => window.__probe);
  await b.close();
  return out;
}

/**
 * The system Firefox, which HAS an H.264 decoder. It cannot be driven by
 * Playwright, so the page posts its own verdict back to this server.
 */
async function viaSystemFirefox() {
  const done = new Promise((r) => (onResult = r));
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'hlsprobe-'));
  const url = `http://127.0.0.1:${server.address().port}/probe.html`;
  const child = execFile('firefox', ['--headless', '--no-remote', '--profile', profile, url], () => {});
  const out = await Promise.race([
    done,
    new Promise((r) => setTimeout(() => r({ timeout: true, errors: [], loads: [], pageErrors: [] }), (secs + 40) * 1000)),
  ]);
  child.kill();
  fs.rmSync(profile, { recursive: true, force: true });
  return out;
}

(async () => {
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const out =
    browser === 'system-firefox' ? await viaSystemFirefox() : await viaPlaywright(browser);
  server.close();
  lib.report(`${browser} / ${rung}${audioRung ? ' + demuxed ' + audioRung : ' (video only)'} — ${root}`, out);
  lib.verdict([out]);
})();
