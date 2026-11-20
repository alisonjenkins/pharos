# Feature specification: Keep the normal UI offline (049)

**Created**: 2026-11-20T19:30:00Z
**Status**: Slice 1 (web, PWA, TV) built. Phone and Kobo slices follow.
**Scope**: Every app that shows the library: the web app and PWA, the TV app, the phone app, and the Kobo app.
**Decision record**: ADR-0149 (the device's library answers the screens' reads).

---

## Problem statement

- **Offline, the app becomes another app.** With no server the web app drops to separate lists of downloads and kept books; the phone shows its own offline start screen. Home, the library pages and the title pages are gone.
- **A dropped connection keeps showing what cannot play.** Mid-session, the screens keep the server's listings, so nearly everything shown fails when opened.

## User stories

1. As a viewer with no signal, I open the app and see the usual Home, in the usual order, with a line saying I am offline, listing only what I can play or read now.
2. As a viewer, I open a show from Home and see only the episodes on this device; its page offers Play and nothing that needs the server.
3. As a viewer whose connection drops while browsing, after a few seconds the page shows only what is on the device; a blip changes nothing.
4. As a viewer, when the server is back, I see "Back online" for a moment and the library fills back in where I am, without a reload, and anything playing keeps playing.
5. As a Kobo reader, I get the same: the usual screens, an offline line, only what is in storage, and the library again when the server answers.

## Requirements

- **FR-001**: While the server is out of reach the app keeps its shell and screens and reads them from the device's library, which holds only what plays or opens with no server (downloaded videos, kept books, cached songs) and the items above each (season, series, album, library).
- **FR-002**: Home keeps its order (Continue Watching, Next Up, TV Shows, Movies, Music, Books, Media). A rail or a library with nothing on the device is left out, and so is its tab.
- **FR-003**: Continue Watching lists videos part watched on the device; one watched past 95% counts as watched. Next Up lists, per show, the first episode on the device after the last one watched. Books part read show in Reading.
- **FR-004**: A title page offline offers Play, Resume, Read, Play from start and the keep control only.
- **FR-005**: The switch happens at once with no network, 5 s after a signed-in session loses its connection, and on a first start that waits or is slow (3 s). A line "You're offline. Showing what's on this device." shows throughout, with "Reconnecting…" while the app retries.
- **FR-006**: When the session is back, the screens read the server again in place; "Back online" shows for 4 s.
- **FR-007**: Each kept item is noted with its card and page text and every parent, per user: at sign-in, every 5 minutes while online, and every kept item again once a day. A note walks up only until a parent already noted. Notes nothing kept needs are dropped; signing out forgets them.
- **FR-008**: The browser keeps one poster per noted item for offline cards, answered by the service worker at any card width.
- **FR-009**: The verdict, each notes pass and each poster pass are logged with their counts and reason.
- **FR-010** (phone): the phone app uses the same screens offline, replacing its offline start screen, with its cached music and downloads.
- **FR-011** (Kobo): the Kobo app's home and library screens list only what is in storage when the server does not answer, with an offline line drawn in the same single screen draw, and list the library again once it answers.

## Out of scope (slice 1)

- Posters on the phone and TV apps' native image paths (cards show their letter).
- Mixes in the offline Music rail (they stay on the Downloads page).
- Sending positions saved offline to the server.
