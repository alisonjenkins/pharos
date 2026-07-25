#!/usr/bin/env node
// Ask whether an ffmpeg output-flag change is one a real player can tell apart.
//
// Builds two fMP4 ladders from ONE synthetic source — identical except for the
// flags under test — the way pharos builds them (one `-f mp4` run per segment,
// init split off the first `moof`, as `fmp4::process_segment` does), then
// plays each with real hls.js in real browsers.
//
// This is the tool that refuted a confident diagnosis: a stray MP4 chapter
// track was "obviously" what made hls.js throw, and hls.js played the ladder
// carrying it without complaint in both engines. Run it BEFORE claiming an
// output defect explains a player symptom.
//
// Usage:
//   hlsjs-flag-probe.cjs --variant with-chapters: --variant no-chapters:-map_chapters,-1
//                        [--codec vp9|h264] [--secs 12]
//
// Flags within a variant are COMMA-separated, not space-separated: `just`
// re-splits recipe arguments on whitespace, so `-map_chapters -1` would arrive
// as two unrelated argv words and ffmpeg would reject the orphaned value.
'use strict';
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');
const lib = require('./hlsjs-lib.cjs');

function arg(name, dflt) {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1] : dflt;
}
const variants = process.argv
  .map((a, i) => (a === '--variant' ? process.argv[i + 1] : null))
  .filter(Boolean)
  .map((v) => {
    const [name, ...rest] = v.split(':');
    return { name, extra: rest.join(':').split(',').map((x) => x.trim()).filter(Boolean) };
  });
if (!variants.length) {
  console.error('need at least one --variant name:<comma-separated ffmpeg args>');
  process.exit(2);
}

const codec = arg('codec', 'vp9');
const secs = Number(arg('secs', 12));
const SEG = 4;
const SEGS = 3;
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'flagprobe-'));
const ff = (args) => execFileSync('ffmpeg', ['-v', 'error', '-y', ...args], { stdio: 'pipe' });

// A source with chapters, subtitles and two tracks — the features that leak
// into an output when a flag is missing.
const meta = path.join(dir, 'ch.txt');
fs.writeFileSync(
  meta,
  ';FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=4000\ntitle=Cold Open\n' +
    '[CHAPTER]\nTIMEBASE=1/1000\nSTART=4000\nEND=12000\ntitle=Main\n',
);
const src = path.join(dir, 'src.mkv');
ff([
  '-f', 'lavfi', '-i', 'testsrc2=duration=12:size=320x180:rate=24',
  '-f', 'lavfi', '-i', 'sine=frequency=440:duration=12:sample_rate=48000',
  '-i', meta, '-map', '0:v', '-map', '1:a', '-map_chapters', '2',
  '-c:v', 'libx264', '-preset', 'ultrafast', '-pix_fmt', 'yuv420p',
  '-c:a', 'libopus', '-shortest', src,
]);

/** Offset of the first top-level `moof` — where pharos splits init from media. */
function moofOffset(buf) {
  let off = 0;
  while (off + 8 <= buf.length) {
    const size = buf.readUInt32BE(off);
    if (buf.toString('ascii', off + 4, off + 8) === 'moof') return off;
    if (size < 8) throw new Error(`bad box size ${size} at ${off}`);
    off += size;
  }
  throw new Error('no moof found');
}

const VCODEC =
  codec === 'h264'
    ? ['-c:v', 'libx264', '-preset', 'ultrafast']
    : ['-c:v', 'libvpx-vp9', '-deadline', 'realtime', '-cpu-used', '8', '-row-mt', '1'];

function build(v) {
  const out = path.join(dir, v.name);
  fs.mkdirSync(out, { recursive: true });
  for (let seg = 0; seg < SEGS; seg++) {
    const whole = path.join(out, `whole${seg}.mp4`);
    ff([
      '-ss', String(seg * SEG), '-i', src, '-t', String(SEG),
      '-an', '-sn', ...v.extra, ...VCODEC, '-pix_fmt', 'yuv420p', '-g', '48',
      '-f', 'mp4', '-movflags', '+frag_keyframe+empty_moov+default_base_moof', whole,
    ]);
    const buf = fs.readFileSync(whole);
    const cut = moofOffset(buf);
    if (seg === 0) fs.writeFileSync(path.join(out, 'init.mp4'), buf.subarray(0, cut));
    fs.writeFileSync(path.join(out, `seg${seg}.m4s`), buf.subarray(cut));
    fs.rmSync(whole);
  }
  const kinds = execFileSync(
    'ffprobe',
    ['-v', 'error', '-show_entries', 'stream=codec_type', '-of', 'csv=p=0', path.join(out, 'init.mp4')],
    { encoding: 'utf8' },
  )
    .split('\n')
    .map((l) => l.trim().replace(/,$/, ''))
    .filter(Boolean);
  console.log(`ladder ${v.name.padEnd(16)} init tracks = ${JSON.stringify(kinds)}  (${v.extra.join(' ') || 'no extra flags'})`);
  return { ...v, kinds };
}

const built = variants.map(build);
const codecs = codec === 'h264' ? 'avc1.640028' : 'vp09.00.10.08';
const server = lib.serve(dir, { rungCodecs: Object.fromEntries(built.map((b) => [b.name, codecs])) });

(async () => {
  const pw = require('@playwright/test');
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const results = [];
  for (const [engine, bt] of [['firefox', pw.firefox], ['chromium', pw.chromium]]) {
    // Playwright's Firefox has no H.264 decoder; skip rather than report a
    // codec-support failure as if it were a verdict on the flags.
    if (engine === 'firefox' && codec === 'h264') {
      console.log('\n(skipping firefox: the Playwright build has no H.264 decoder — use hlsjs-bytes-probe.cjs --browser system-firefox)');
      continue;
    }
    for (const v of built) {
      const b = await bt.launch();
      const p = await b.newPage();
      const html = lib.probePage(lib.hlsSource(), `/master-${v.name}.m3u8`, secs);
      await p.route('**/probe.html', (r) => r.fulfill({ contentType: 'text/html', body: html }));
      await p.goto(`http://127.0.0.1:${server.address().port}/probe.html`);
      const out = await p.evaluate(() => window.__probe);
      await b.close();
      results.push(lib.report(`${engine} / ${v.name} (init tracks ${JSON.stringify(v.kinds)})`, out));
    }
  }
  server.close();
  fs.rmSync(dir, { recursive: true, force: true });
  lib.verdict(results);
})();
