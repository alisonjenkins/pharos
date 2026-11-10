# Spec 036: read books offline

- **Status:** accepted 2026-11-10; slices 1–3 merged (#1172, #1178, #1195), newer-place fix #1201 and slice 4 in review
- **Created:** 2026-11-10T06:00:00Z
- **Input:** "I want the option of allowing the user to read offline if they are
  reading on the train and they go through a tunnel. They would need the wasm
  reader and the book for that." (Alison, 2026-11-10)
- **Depends on:** 023 (the reader: EPUB slices 1–4, PDF slice 5 #1162),
  ADR-0047, 017 (native API)
- **Ids:** bugs B1170–B1179, invariants V1130–V1139 and V1184–V1186, ADR 0110 (service worker and cache design).

## Problem

The reader keeps a book in memory once it is open, so a tunnel does not stop
the page in front of you. Three things break without a connection:

1. **Saving your place.** `setReadPosition` goes over the RPC socket. Offline,
   the save fails, is logged and is not retried, so the reading done in the
   tunnel is lost to other devices.
2. **Reloading or reopening.** pharos-ui has no service worker. A reload, a
   phone that dropped the tab, or opening another book needs the network: the
   app shell, the wasm, the PDF module and the book bytes all come from the
   server.
3. **Starting offline.** Opening pharos on a train with no signal shows the
   browser's offline page.

## Scope

In: EPUB and PDF books in the browser app (desktop, phone, Quest browser),
reading and saving positions with no connection, and catching up when back.
Out: video and music offline; the Android TV app (a TV is not taken on a
train); editing the library offline; DRM books (005 §Scope stands).

## User stories

### US1: the tunnel (P1)

Reading with the book open, the connection drops. Pages keep turning, the
place is saved on the device, and when the connection returns the place is
sent to the server without anything from the reader. Nothing on screen
changes except a small "Offline, your place is saved on this device" note.

### US2: keep a book for the journey (P1)

On a book's title page, "Keep offline" stores the book and everything needed
to open it on this device. A kept book opens with no connection, from a
reload, a closed tab or a cold start. "Remove from device" frees the space.
A "On this device" list shows kept books and the space they use.

### US3: open pharos with no signal (P2)

Opening `/ui/` offline shows the app with the kept books, not an error.
Everything that needs the server says so plainly and is not offered.

### US4: two devices disagree (P2)

Read offline on the phone to chapter 9, then on the desktop online to chapter
7, then the phone reconnects. The newest reading wins, judged by when it was
read, not when it reached the server.

## Requirements (draft)

- **FR-001** Position saves that fail go into an on-device queue (one entry
  per book, the newest replacing older ones), retried with backoff when the
  socket returns and on `online`. Entries carry the time they were read.
- **FR-002** `setReadPosition` gains the client's read time. The server keeps
  the newer of the stored and the sent position, so a late upload never
  overwrites newer reading from another device (US4). Clock skew is bounded:
  a time ahead of the server's clock is clamped to now.
- **FR-003** A service worker scoped to `/ui/` caches the app shell (index,
  main wasm, CSS, fonts, icons) versioned by the bundle hash, so a deploy
  replaces it whole and an old shell never runs against a new server schema.
  Network first for the shell when online, so a deploy shows at once.
- **FR-004** "Keep offline" stores the book bytes, the PDF module when the
  book is a PDF, and the title page details the reader needs (title, format,
  saved position) in Cache Storage, and asks for persistent storage
  (`navigator.storage.persist()`). The 64 MiB weak-device cap applies to
  keeping as it does to opening.
- **FR-005** Book bytes in the cache are only served to the signed-in user who
  kept them; signing out removes every kept book from the device.
- **FR-006** The reader opens a kept book from the cache before trying the
  network, so a kept book opens at once even online.
- **FR-007** Observable: queued saves, flushes, flush failures and cache sizes
  are logged with item ids; the server counts late (client-timed) saves and
  how many lost to a newer position.

## Decisions

1. **Which books are on the device: all three, configurable** (Alison,
   2026-11-10T07:01Z: "a and b and c I think. It should be configurable").
   Settings, per device (local, not synced, since storage is per device):
   - **Keep offline** on the title page is always offered (a).
   - **Keep the book I'm reading** (b), default on: the open book is kept
     while it is open and for 7 days after it was last opened.
   - **Keep my last N books** (c), default off, N from 1 to 10 (default 3).
   - **Space for automatic books**, default 512 MiB. Books kept by (b) or (c)
     are removed oldest-opened first when over it. Books kept by hand (a) are
     never removed automatically and do not count against it.
   - The 64 MiB weak-device cap still applies to every kept book.
   - The "On this device" list shows why each book is kept (by hand, reading
     now, recent).
   Defaults and the 512 MiB / 7 day figures are mine, not Alison's; she can
   change them.
2. **Order:** save queue first (slice 1, #1172), then the service worker
   (slice 2), then keeping books (slice 3).

- **FR-008** Automatic keeping (b, c) runs only after a book has opened
  successfully and never re-downloads a book already fetched for reading:
  the bytes already in memory are written to the cache.
- **FR-009** Settings live under Settings, "Offline reading", and are read
  on each book open, so a change applies without a reload. Turning (b) or
  (c) off removes the books it kept on the next open of the list or reader.

## Slices

| # | PR | Lands |
|---|---|---|
| 1 | Offline save queue + client read time (FR-001, FR-002) | schema bump, UI queue, server newest-wins |
| 2 | Service worker for the shell (FR-003) | sw.js, bundle hash, CSP check |
| 3 | Keep offline + On this device (FR-004–FR-006) | Cache Storage, title page, settings list |
| 4 | Automatic keeping (FR-008, FR-009) | reading-now and last-N policies, space budget, settings |
