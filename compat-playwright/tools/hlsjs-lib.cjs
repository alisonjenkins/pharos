// Shared plumbing for the hls.js probes.
//
// These exist because a byte-level diagnosis is not a diagnosis until a real
// player has agreed with it. Twice in one session a confident root cause
// ("hls.js chokes on the stray chapter track") survived code review and died
// the moment hls.js was actually handed the bytes.
'use strict';
const http = require('http');
const fs = require('fs');
const path = require('path');

const SEG_SECS = 6.006;

/** Segment indices present in `<root>/<rung>/`, from `segN.m4s`. */
function segsOf(root, rung) {
  return fs
    .readdirSync(path.join(root, rung))
    .filter((f) => /^seg\d+\.m4s$/.test(f))
    .map((f) => Number(f.match(/\d+/)[0]))
    .sort((a, b) => a - b);
}

/** A VOD media playlist over those segments, URLs pointing back at this server. */
function mediaPlaylist(root, rung, segSecs = SEG_SECS) {
  const list = segsOf(root, rung);
  let pl = '#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:7\n';
  pl += `#EXT-X-MAP:URI="/${rung}/init.mp4"\n#EXT-X-MEDIA-SEQUENCE:${list[0]}\n`;
  for (const s of list) pl += `#EXTINF:${segSecs.toFixed(3)},\n/${rung}/seg${s}.m4s\n`;
  return pl + '#EXT-X-ENDLIST\n';
}

/**
 * A master with one video rung, optionally pairing it with a demuxed audio
 * group — the shape pharos actually serves a browser, and the shape a
 * video-only probe silently fails to exercise.
 */
function masterPlaylist(videoRung, codecs, audioRung) {
  let m = '#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n';
  if (audioRung) {
    m +=
      '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="Audio",DEFAULT=YES,AUTOSELECT=YES,' +
      `URI="/${audioRung}/index.m3u8"\n`;
  }
  m += `#EXT-X-STREAM-INF:BANDWIDTH=12128000,CODECS="${codecs}"${
    audioRung ? ',AUDIO="aud"' : ''
  }\n/${videoRung}/index.m3u8\n`;
  return m;
}

/** The page that drives hls.js and reports what happened. */
function probePage(hlsSrc, sourceUrl, runSecs) {
  return `<!doctype html><meta charset="utf-8"><title>hls probe</title>
<video id="v" muted autoplay playsinline></video>
<script>${hlsSrc}</script>
<script>
window.__probe = (async () => {
  const out = { support: {}, errors: [], loads: [], pageErrors: [] };
  window.onerror = (m, s, l, c, e) => out.pageErrors.push(String((e && e.stack) || m));
  for (const t of ['video/mp4; codecs="avc1.640028"', 'audio/mp4; codecs="opus"',
                   'audio/mp4; codecs="mp4a.40.2"', 'video/mp4; codecs="vp09.00.10.08"']) {
    out.support[t] = MediaSource.isTypeSupported(t);
  }
  const v = document.getElementById('v');
  const hls = new Hls({ debug: false });
  hls.on(Hls.Events.ERROR, (_e, d) => out.errors.push(
    d.type + '/' + d.details + (d.fatal ? ' FATAL' : '') +
    (d.error && d.error.message ? ' :: ' + d.error.message : '')));
  hls.on(Hls.Events.FRAG_LOADED, (_e, d) => out.loads.push(d.frag.type + '#' + d.frag.sn));
  hls.loadSource(${JSON.stringify(sourceUrl)});
  hls.attachMedia(v);
  v.play().catch(() => {});
  await new Promise(r => setTimeout(r, ${runSecs * 1000}));
  out.currentTime = v.currentTime;
  out.readyState = v.readyState;
  out.buffered = [];
  for (let i = 0; i < v.buffered.length; i++) {
    out.buffered.push(v.buffered.start(i).toFixed(3) + '..' + v.buffered.end(i).toFixed(3));
  }
  // A refetch storm is the signature of a fragment the player cannot use:
  // production served ONE segment 17 times inside a single second.
  out.repeats = out.loads.length - new Set(out.loads).size;
  return out;
})();
</script>`;
}

/** Static server over `root`, synthesising the playlists. */
function serve(root, { audioRung, rungCodecs, segSecs, route } = {}) {
  const server = http.createServer((req, res) => {
    const rel = decodeURIComponent(req.url.split('?')[0]);
    // Caller-owned routes first. They must be part of THIS handler: attaching
    // a second 'request' listener makes both answer, and the loser dies with
    // ERR_HTTP_HEADERS_SENT.
    if (route && route(req, res, rel)) return;
    const m = rel.match(/^\/([\w.-]+)\/index\.m3u8$/);
    if (m) {
      res
        .writeHead(200, { 'content-type': 'application/vnd.apple.mpegurl' })
        .end(mediaPlaylist(root, m[1], segSecs));
      return;
    }
    const mm = rel.match(/^\/master-([\w.-]+)\.m3u8$/);
    if (mm) {
      res
        .writeHead(200, { 'content-type': 'application/vnd.apple.mpegurl' })
        .end(masterPlaylist(mm[1], (rungCodecs || {})[mm[1]] || 'avc1.640028', audioRung));
      return;
    }
    const p = path.join(root, rel);
    if (!p.startsWith(root) || !fs.existsSync(p) || fs.statSync(p).isDirectory()) {
      res.writeHead(404).end();
      return;
    }
    res.writeHead(200, { 'content-type': 'application/octet-stream' });
    fs.createReadStream(p).pipe(res);
  });
  return server;
}

function hlsSource() {
  return fs.readFileSync(require.resolve('hls.js/dist/hls.min.js'), 'utf8');
}

function report(title, r) {
  console.log(
    `\n=== ${title} ===\n` +
      `    currentTime = ${r.currentTime}   readyState = ${r.readyState}\n` +
      `    buffered    = ${JSON.stringify(r.buffered)}\n` +
      `    frags       = ${r.loads.length} (${r.repeats} repeats) ${JSON.stringify(
        r.loads.slice(0, 12),
      )}\n` +
      `    hls errors  = ${r.errors.length ? JSON.stringify(r.errors, null, 6) : 'none'}\n` +
      `    page errors = ${r.pageErrors.length ? JSON.stringify(r.pageErrors, null, 6) : 'none'}`,
  );
  return r;
}

/** Non-zero exit when a probe wedged, so a recipe can gate on it. */
function verdict(results) {
  const bad = results.filter((r) => r.repeats > 0 || !(r.currentTime > 0.2) || r.errors.some((e) => e.includes('FATAL')));
  if (bad.length) {
    console.log(`\nFAILED: ${bad.length}/${results.length} probe(s) did not play cleanly.`);
    process.exitCode = 1;
  } else {
    console.log(`\nOK: ${results.length}/${results.length} probe(s) played, no refetch storm.`);
  }
}

module.exports = { SEG_SECS, segsOf, mediaPlaylist, masterPlaylist, probePage, serve, hlsSource, report, verdict };
