// pharos-ui's service worker (spec 036 FR-003, ADR-0110): the app shell,
// offline. pharos-server serves this file at /ui/sw.js with the two
// placeholders below filled in, and the browser takes a new worker whenever
// they change, so a deploy replaces the whole cache.
"use strict";

// A hash of the shell's files, set by the server when it loads the bundle.
const VERSION = "__PHAROS_BUNDLE_HASH__";
// The shell's URLs (index, main wasm and JS, CSS, icons), set by the server.
const PRECACHE = __PHAROS_PRECACHE__;

// This worker owns caches named `pharos-shell-*` and deletes the old ones on
// activate. It never opens or deletes any other name: `pharos-books-*` is
// reserved for kept books (spec 036 slice 3).
const SHELL_PREFIX = "pharos-shell-";
const SHELL_CACHE = SHELL_PREFIX + VERSION;

const MOUNT = "/ui/";
// Lazy modules, not precached because they are large: cached when fetched,
// in the shell cache, so they go with the shell on a deploy.
const LAZY_PREFIXES = ["/ui/assets/pdf/"];
// With a cached copy to fall back on, the network gets this long to start
// answering, so a dead connection (a tunnel) does not hang the page.
const NETWORK_WAIT_MS = 4000;

const precached = new Set(PRECACHE);

self.addEventListener("install", (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(SHELL_CACHE);
      // `reload` so the HTTP cache cannot hand the cache a stale file.
      await cache.addAll(PRECACHE.map((url) => new Request(url, { cache: "reload" })));
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      for (const name of await caches.keys()) {
        if (name.startsWith(SHELL_PREFIX) && name !== SHELL_CACHE) {
          await caches.delete(name);
        }
      }
      await self.clients.claim();
      const clients = await self.clients.matchAll({ type: "window" });
      for (const client of clients) {
        client.postMessage({ kind: "pharos-sw", event: "activated", version: VERSION });
      }
    })(),
  );
});

// The cache key for a request this worker handles, or null to leave it to
// the browser. Only GETs of the bundle's own files qualify, so the API, the
// RPC socket, media and book bytes are never touched.
function keyFor(request, url) {
  if (request.method !== "GET" || request.headers.has("range")) return null;
  if (url.origin !== self.location.origin) return null;
  if (request.mode === "navigate" && url.pathname.startsWith(MOUNT)) return MOUNT;
  if (precached.has(url.pathname)) return url.pathname;
  if (LAZY_PREFIXES.some((p) => url.pathname.startsWith(p))) return url.pathname;
  return null;
}

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  const key = keyFor(event.request, url);
  if (key === null) return;
  event.respondWith(networkFirst(event.request, key, !precached.has(key) && key !== MOUNT));
});

async function networkFirst(request, key, remember) {
  const cache = await caches.open(SHELL_CACHE);
  const cached = await cache.match(key);
  try {
    const response = await fetchWithin(request, cached ? NETWORK_WAIT_MS : 0);
    if (!response.ok && response.status >= 500 && cached) return cached;
    // Shell files are only written at install, so a cache never mixes two
    // deploys; a lazy module is written when fetched.
    if (remember && response.ok) await cache.put(key, response.clone());
    return response;
  } catch (err) {
    if (cached) return cached;
    throw err;
  }
}

function fetchWithin(request, ms) {
  const fetched = fetch(request);
  if (ms === 0) return fetched;
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error("network wait elapsed")), ms);
  });
  return Promise.race([fetched, timeout]).finally(() => clearTimeout(timer));
}

// The page asks which build is running; it logs the answer.
self.addEventListener("message", (event) => {
  const data = event.data;
  if (data && data.kind === "pharos-sw" && data.event === "hello" && event.source) {
    event.source.postMessage({ kind: "pharos-sw", event: "version", version: VERSION });
  }
});
