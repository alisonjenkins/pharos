# ADR-0149: Offline, the device's library answers the screens' reads

- **Status:** Accepted
- **Date:** 2026-11-20T20:00:00Z
- **Deciders:** Alison Jenkins

## Context

With no server, the web and phone apps showed separate offline screens
(spec 036's kept books, spec 044's offline start, spec 048's downloads),
and a dropped session kept showing the server's listings. Alison asked
for the normal UI to stay, saying it is offline and showing only what can
be played or read, then filling back in when the server returns, in every
app including the Kobo one.

The screens already read the library through the `PharosApi` trait
(`home`, `views`, `query`, `index`, `search`, `detail`). What is kept on a
device records only an id and a title; posters stream from the server.

## Decision

We will make the device's library a second implementation of `PharosApi`:

- `pharos-ui` `offline_library::OfflineLibrary` answers the reads from a
  catalogue of notes and the list of what is on the device. Server-only
  calls answer `ClientError::Api { kind: "offline" }`.
- The catalogue (`localStorage`/settings key `pharos.offline.catalogue`)
  holds, per user, one note per kept item and every item above it: its
  card fields and page text. It is filled while online at sign-in, every
  5 minutes, and refreshed once a day; unused notes are pruned and
  sign-out forgets it.
- `Ctx::library()` hands screens the device's library while
  `device_verdict` says the server is out of reach (no network at once, a
  lost session after 5 s, a first start that waits or takes 3 s), else the
  server's. Screens do not branch on it, except the title page, which
  leaves out actions that need the server.
- In the browser, posters of noted items are kept in Cache Storage
  `pharos-art-<user>` under their URL without `fillWidth`/`format`. The
  service worker answers a same-origin GET under `/Items/*/Images/` from
  those caches by that key, else from the network, and leaves pictures
  alone when no such cache exists. Like ADR-0143's video exception to
  V1133, this serves per-user bytes from the worker, limited to pictures.

## Consequences

- The screens draw offline with no offline-only code paths, and a new
  screen that reads through `Ctx::library()` works offline for free.
- Items kept by an older build show by title only until the device is
  next online, when their notes are filled in.
- A note is a snapshot: a mark made elsewhere reaches the device's Next Up
  only after the daily refresh.
- One extra stored value per device (a few kilobytes per kept item) and,
  in the browser, one 360 px poster per noted item.
- The phone and Kobo apps follow on the same shape (spec 049 T8, T9).

## Alternatives considered

- **Keep separate offline screens, styled like the normal ones.** Two
  copies of every screen to keep in step; rejected.
- **Branch inside each screen on an offline flag.** Spreads the filtering
  over every screen and misses new ones; rejected for one data source.
- **Serve the server's own listings from a response cache.** The cached
  listings hold everything, not what is on the device, and need the RPC
  socket; rejected.

## References

- specs/049-offline-library/spec.md
- ADR-0110 (books in Cache Storage), ADR-0143 (offline video)
- crates/pharos-ui/src/native/offline_library.rs, offline_device.rs, source.rs
