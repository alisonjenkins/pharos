# ADR-0112: Kept books are stored by the page, one cache per user

- **Status:** Accepted
- **Date:** 2026-11-10T07:53:27Z
- **Deciders:** Alison

## Context

ADR-0110 left a seam for spec 036 slice 3: whether the UI service worker
would serve book bytes, which are per user (spec 036 FR-005). Slice 3 adds
Keep offline and the On this device list. This ADR amends
[ADR-0110](0110-the-ui-service-worker-caches-the-shell-by-bundle-hash.md);
the text below was first written as an amendment section inside it.

## Decision

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

## Consequences

The worker stays free of per-user data (V1133), so a shell cache never holds
a book. A kept PDF needs one online open after a deploy before it opens
offline again, because the deploy replaces the shell cache that holds the
PDF module.

## References

- [ADR-0110](0110-the-ui-service-worker-caches-the-shell-by-bundle-hash.md)
- `specs/036-*/` slice 3, B1172, V1136–V1139
