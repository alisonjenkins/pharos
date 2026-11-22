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
// activate. It never writes or deletes any other name: `pharos-books-*` is
// reserved for kept books (spec 036 slice 3) and is never served (V1133).
//
// One exception, ADR 0143: it serves downloaded video. A same-origin GET
// under /videos/ (no Range) is answered from a `pharos-videos-*` cache by
// exact URL, query included, when one holds it, else it goes to the network.
// The page writes those caches; the worker only reads them.
//
// And ADR 0149: it serves the posters of what is kept on the device. A
// same-origin GET of an item's picture is answered from a `pharos-art-*`
// cache, by its URL without `fillWidth` and `format`, when one holds it,
// else it goes to the network. The page writes those caches too.
const SHELL_PREFIX = "pharos-shell-";
const SHELL_CACHE = SHELL_PREFIX + VERSION;

const MOUNT = "/ui/";
// Lazy modules, not precached because they are large: cached when fetched,
// in the shell cache, so they go with the shell on a deploy.
const LAZY_PREFIXES = ["/ui/assets/pdf/", "/ui/assets/vendor/"];
// With a cached copy to fall back on, the network gets this long to start
// answering, so a dead connection (a tunnel) does not hang the page.
const NETWORK_WAIT_MS = 4000;

// Downloaded video (ADR 0143). `videoCaches` is the names of the
// `pharos-videos-*` caches: null until read (a worker restarts often), then
// kept current on activate and when the page posts `videos-changed`. With
// none, a /videos/ request is left to the browser untouched.
const VIDEO_PREFIX = "pharos-videos-";
const VIDEO_PATH = "/videos/";
let videoCaches = null;
let videoCachesRead = null;

// Kept posters (ADR 0149), read and kept current like the video caches.
const ART_PREFIX = "pharos-art-";
const ART_PATH = /^\/Items\/[^/]+\/Images\//;
let artCaches = null;

function refreshVideoCaches() {
  videoCachesRead = caches
    .keys()
    .then((names) => {
      videoCaches = names.filter((n) => n.startsWith(VIDEO_PREFIX));
      artCaches = names.filter((n) => n.startsWith(ART_PREFIX));
    })
    .catch(() => {
      videoCaches = [];
      artCaches = [];
    });
  return videoCachesRead;
}

// A poster's URL without the width and format a card asks for, so every
// card size finds the one stored copy. pharos-ui's `art_key` makes the same.
function artKey(url) {
  const at = url.indexOf("?");
  if (at < 0) return url;
  const kept = url
    .slice(at + 1)
    .split("&")
    .filter((p) => p !== "" && !p.startsWith("fillWidth=") && !p.startsWith("format="));
  return kept.length === 0 ? url.slice(0, at) : url.slice(0, at) + "?" + kept.join("&");
}

function isArtRequest(request, url) {
  return (
    request.method === "GET" &&
    url.origin === self.location.origin &&
    ART_PATH.test(url.pathname)
  );
}

async function keptArt(request) {
  try {
    if (artCaches === null) await (videoCachesRead || refreshVideoCaches());
    const key = artKey(request.url);
    for (const name of artCaches) {
      const cache = await caches.open(name);
      const hit = await cache.match(key, { ignoreVary: true });
      if (hit) return hit;
    }
  } catch (err) {
    // A cache that cannot be read is no reason to fail a picture.
  }
  return fetch(request);
}

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
      await refreshVideoCaches();
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
  if (key === null) {
    if (isVideoRequest(event.request, url) && !(videoCaches && videoCaches.length === 0)) {
      event.respondWith(downloadedVideo(event.request));
    } else if (isArtRequest(event.request, url) && !(artCaches && artCaches.length === 0)) {
      event.respondWith(keptArt(event.request));
    }
    return;
  }
  event.respondWith(networkFirst(event.request, key, !precached.has(key) && key !== MOUNT));
});

function isVideoRequest(request, url) {
  return (
    request.method === "GET" &&
    !request.headers.has("range") &&
    url.origin === self.location.origin &&
    url.pathname.startsWith(VIDEO_PATH)
  );
}

// The stored response for exactly this URL, else the network. `ignoreVary`
// because the page stored it under its own request headers.
async function downloadedVideo(request) {
  try {
    if (videoCaches === null) await (videoCachesRead || refreshVideoCaches());
    for (const name of videoCaches) {
      const cache = await caches.open(name);
      const hit = await cache.match(request.url, { ignoreVary: true });
      if (hit) return hit;
    }
  } catch (err) {
    // A cache that cannot be read is no reason to fail playback.
  }
  return fetch(request);
}

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
  if (
    data &&
    data.kind === "pharos-sw" &&
    (data.event === "videos-changed" || data.event === "art-changed")
  ) {
    event.waitUntil(refreshVideoCaches());
  }
});
