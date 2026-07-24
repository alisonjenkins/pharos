// Same-origin front-end for the SyncPlay E2E harness.
//
// Serves the static jellyfin-web bundle AND reverse-proxies every non-file
// request (the REST API) plus the /socket WebSocket upgrade to pharos. Because
// the browser then talks to a SINGLE origin (this server), jellyfin-web treats
// the serving origin as its Jellyfin server: its boot /System/Info/Public probe
// resolves to real pharos JSON and the app goes straight to login on ANY
// browser. That removes the fragile manual "Add Server" cross-origin connect to
// pharos:8096 — which a full chromium could not reach inside the CI container,
// even though the stripped FOSS chromium and shell curl could.
//
// Why not `http-server --proxy`: it mangles the proxied path (a trailing "?"
// turned it into a query string) AND does not forward the /socket WebSocket
// upgrade, so an origin-connected member would log in but never receive any
// SyncPlay group message. This proxy forwards both.

const http = require("http");
const fs = require("fs");
const path = require("path");
const httpProxy = require("http-proxy");

const DIR = process.env.JELLYFIN_WEB_DIR;
const PORT = parseInt(process.env.JELLYFIN_WEB_PORT || "8910", 10);
const TARGET = process.env.PHAROS_URL || "http://127.0.0.1:8096";
if (!DIR) {
  console.error("JELLYFIN_WEB_DIR not set — enter the nix devShell.");
  process.exit(1);
}
const ROOT = path.resolve(DIR);

const proxy = httpProxy.createProxyServer({
  target: TARGET,
  changeOrigin: true,
  ws: true,
});
proxy.on("error", (err, _req, res) => {
  if (res && res.writeHead && !res.headersSent) {
    res.writeHead(502, { "content-type": "text/plain" });
    res.end("jf-proxy upstream error: " + err.message);
  }
});

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".css": "text/css",
  ".json": "application/json",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".gif": "image/gif",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
  ".ttf": "font/ttf",
  ".map": "application/json",
  ".wasm": "application/wasm",
};

// Serve a static file from the bundle if one exists for this path; return false
// (→ proxy to pharos) otherwise. "/" maps to index.html.
function tryStatic(req, res) {
  const urlPath = decodeURIComponent(req.url.split("?")[0]);
  const rel = urlPath === "/" ? "/index.html" : urlPath;
  const filePath = path.join(ROOT, path.normalize(rel));
  if (filePath !== ROOT && !filePath.startsWith(ROOT + path.sep)) return false; // traversal guard
  let st;
  try {
    st = fs.statSync(filePath);
  } catch {
    return false;
  }
  if (!st.isFile()) return false;
  res.writeHead(200, {
    "content-type": MIME[path.extname(filePath)] || "application/octet-stream",
    "access-control-allow-origin": "*",
  });
  if (req.method === "HEAD") {
    res.end();
  } else {
    fs.createReadStream(filePath).pipe(res);
  }
  return true;
}

const server = http.createServer((req, res) => {
  // Only GET/HEAD can be a static asset; everything else is API → pharos.
  if ((req.method === "GET" || req.method === "HEAD") && tryStatic(req, res)) return;
  proxy.web(req, res);
});
// Forward the /socket WebSocket upgrade to pharos.
server.on("upgrade", (req, socket, head) => proxy.ws(req, socket, head));
server.listen(PORT, "127.0.0.1", () => {
  console.log(`jf-proxy listening on http://127.0.0.1:${PORT} → ${TARGET}`);
});
