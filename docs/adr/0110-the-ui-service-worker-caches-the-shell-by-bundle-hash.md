# ADR-0110: The UI service worker caches the app shell, versioned by bundle hash

- **Status:** Accepted
- **Date:** 2026-11-10T09:00:00Z
- **Deciders:** Alison

## Context

Spec 036 (read books offline) needs `/ui/` to open with no connection: a
reload in a tunnel, a phone that dropped the tab, a cold start on a train.
pharos-ui had no service worker, so every one of those needed the server for
index, wasm, JS and CSS.

Two things make a shell cache dangerous. A cache that outlives a deploy runs
an old shell against a new server schema (the native API is versioned by
`schemaVersion`, and the shell and server are deployed together). And a
worker that answers requests it should not (API, the RPC socket, media, book
bytes) serves stale or another user's data.

The bundle's build is `dx build` plus `flake.nix`'s `pharosUiBundle`
install step; the server indexes it at start (`ui.rs`, spec 009). Its CSP
already allows `worker-src 'self' blob:`.

## Decision

**One worker, `/ui/sw.js`, scope `/ui/`.** The source is
`crates/pharos-ui/assets/sw.js`, installed at the bundle root. Its scope is
its directory, so no `Service-Worker-Allowed` header is sent.

**Version is a hash of the shell, computed by the server when it loads the
bundle.** `UiBundle::load` hashes (SHA-256, first 8 bytes in hex) the path and
bytes of every shell file in path order, and fills two placeholders in the
template: the hash and the list of shell URLs. The server serves the result
at `/ui/sw.js` with `Cache-Control: no-cache`. A bundle whose `sw.js` lacks a
placeholder fails to load, so an unversioned worker cannot ship. Any change
to a shell file changes the worker's bytes, so the browser installs a new
worker; a file outside the shell (the PDF module) does not.

**The shell is everything in the bundle except `sw.js` and three lazy
directories** (`assets/pdf/`, `assets/vendor/`, `assets/fonts/`: the PDF
module, hls.js and JASSUB, the subtitle fonts). Index, the dx wasm and JS,
`pharos.css`, and the brand icons are precached at install into the cache
`pharos-shell-<hash>`. Install fetches with `cache: "reload"` and fails as a
whole if any file fails, so the previous worker keeps serving.

**Network first.** A navigation under `/ui/` and a request for a shell file
go to the network. The cache answers only when the fetch fails, when it has
not started answering within 4 s (a tunnel's dead connection, with a cached
copy to fall back on), or when it answers 5xx. Every navigation falls back to
the cached index, as the server does for a deep link. A deploy shows at
once, because the network is asked first.

**A deploy replaces the whole cache.** The new worker `skipWaiting`s, and on
activate deletes every cache named `pharos-shell-*` other than its own, then
claims its clients. Shell files are written to the cache only at install, so
one cache never holds two deploys' files.

**Lazy modules are cached when fetched.** A GET under `/ui/assets/pdf/` is
network first and stored in the shell cache when it answers 200, keyed by
path (the loader's `?n=` attempt query is ignored). It goes with the shell on
a deploy. hls.js, JASSUB and the fonts are video-only and not cached.

**Never cached:** the worker answers only same-origin, non-range GETs of a
navigation under `/ui/`, a precached URL or the PDF directory. Everything
else, the API, the RPC websocket, media, `Range` requests, book bytes and
cross-origin requests, is left to the browser (no `respondWith`).

**Seam for slice 3.** Kept books live in caches named `pharos-books-*`. This
worker never opens, lists for any purpose but deletion by its own prefix, or
deletes a cache outside `pharos-shell-`. Slice 3 adds its own fetch handling
for book URLs beside `keyFor`, and its own cache management, without touching
shell versioning.

**Registration** is `native/sw_web.rs`, called once from the web binary's
`main`, wasm only (TV and host builds never compile it). Failure, such as
plain http off localhost, is logged and the app carries on. The adapter is
the off switch: `localStorage` `pharos.service_worker` = `off` makes the next
start unregister the worker. Registration, the version (the worker answers a
`hello` message) and activation are logged to the console, as the rest of
pharos-ui logs; the crate has no `tracing` subscriber in the browser.

## Consequences

- `/ui/` opens offline after one online visit, for the same origin and a
  secure context (https, or localhost).
- A deploy is visible on the next navigation. An already-open old tab keeps
  its loaded shell until reload; the new worker serves its later lazy
  fetches, which hashed filenames keep consistent.
- On a slow network after a deploy (no answer within 4 s), the old cached
  shell can start. It is stopped by the check pages already have: on connect
  `connect` in `native/app.rs` compares the server's `schemaVersion` with the
  page's (`pharos_client::is_outdated_by`) and, when the server is newer, sets
  `outdated`, which shows the reload prompt. The worker adds no check of its
  own; a deploy that changes the shell but not `SCHEMA_VERSION` is not caught
  by it, only by the next successful network-first navigation.
- Server start reads every shell file once to hash it (tens of MB; tens of
  ms).
- The first visit after a deploy downloads the shell twice: once for the page
  and once for install, though the HTTP cache usually serves the second.
- The shell cache is not a security boundary: it holds public bundle files
  only. Book bytes, which are per user, get their own rules in slice 3
  (spec 036 FR-005).

## Amendment: kept books (spec 036 slice 3, 2026-11-10)

The "seam for slice 3" above left open whether the worker would serve book
bytes. It does not. The page reads and writes kept books itself, through
`window.caches` (`native/kept_web.rs`), so V1133 stands: the worker never
reads, writes or serves a book, and a book is only readable by the page of
the user who kept it.

- **Cache and key.** Books live in `pharos-books-<userId>`, one cache per
  user (the id `Session.me` reports), each under the URL the reader would
  fetch, `/Items/<id>/Download`. The download streams from `fetch` into
  `cache.put`, never through wasm memory; the size is read back from the
  stored blob and a book over the weak-device cap (64 MiB) is deleted again.
- **Record.** One `localStorage` value, `pharos.kept`, holds the user id and,
  per book, title, format, size, kept-at time, reason (`hand`, the only one
  in this slice), and the last place read with the time it was read. The
  offline start and the title page read only this, so nothing runs at
  start-up on a device with nothing kept. Cache Storage is opened only to
  read, keep or remove a book.
- **Offline start.** When the server cannot be reached, or `navigator.onLine`
  is false, or the first connection has not answered in 3 s (a phone in a
  tunnel reports itself online), and a book is kept, the app shows the kept
  books instead of the waiting page, and `/ui/read/<id>` opens the reader
  without an API. The 3 s timer starts only when a book is kept. With nothing
  kept nothing changes.
- **Position.** There is one queue: the reader's save outbox
  (`pharos.reader.pending`, spec 036 FR-001), flushed by the app. A kept
  book also notes each place it saves in its record, with the read time, and
  resumes from the later of that and the book's queued save. The record's
  place is seeded from the server's when the book is kept.
- **Users.** The reader does not wait for the server: it opens the book from
  the cache and the record, then asks `me` for at most 3 s. A copy kept by
  someone else is closed and cleared; no answer leaves the book open. The
  shell also compares the record's user with `me` whenever it is asked.
  Signing out (the machine reaching the sign-in phase) deletes every
  `pharos-books-*` cache and the record. A session the server revokes does
  not, so a tunnel does not cost the books; the next different user finds
  them cleared.
- **PDF module.** Keeping a PDF fetches the module's three files so this
  worker stores them in the shell cache (it controls the page). A deploy
  replaces the shell cache, so a kept PDF needs one online open after a
  deploy before it opens offline again.

## Alternatives considered

- **Hash at build time in `flake.nix`.** A second source of truth, invisible
  to `dx serve`, and the server already indexes the bundle.
- **Cache first for hashed assets.** Faster offline start, but the HTTP cache
  already makes them cheap online, and network first is the one rule to
  reason about (spec 036 FR-003).
- **Workbox.** A dependency and a build step for about 100 lines.
- **Precache the PDF module.** Several MB read by few; it is cached on first
  use.

## References

- Spec 036 (FR-003, US3), ADR-0047 (PDF module), ADR-0038 (the `/ui/` mount)
- `crates/pharos-server/src/ui.rs`, `crates/pharos-ui/assets/sw.js`,
  `crates/pharos-ui/src/native/sw_web.rs`
