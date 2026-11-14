# Feature specification: Pharos on a Kobo (047)

**Created**: 2026-11-14T21:30:00Z
**Status**: Draft. Slice 1 (sign in, music) built; device test pending.
**Scope**: A Pharos client on a Kobo e-reader (first target: Libra Colour) that signs in, plays music and audiobooks over Bluetooth, and reads ebooks.
**Decision record**: ADR-0136 (a Cobalt app, built from a pinned and patched Cobalt).

---

## Problem statement

- **No audio on the Kobo.** KOReader with the OPDS catalogue (spec 045) reads the owner's books, but nothing plays the music or the audiobooks. The reader has no speaker or headphone socket, only Bluetooth.
- **Typing on e-ink is slow.** Signing in with a name and password on the reader's keyboard is the worst part of any client there.
- **One place per book.** An audiobook paused on the Kobo should resume on the phone, and the other way round.

## User stories

1. As the owner, I open Pharos on the Kobo, read a six-digit code, approve it on my phone, and I am signed in. The reader stays signed in after a restart.
2. As a listener, I pick an album, tap a track, and hear it in my Bluetooth headphones.
3. As a listener, I stop a track on the Kobo and carry on from the same place on another device.
4. As a listener, I play an audiobook, including one too big to stream, with chapters, speed and a sleep timer, and my place is kept on the server.
5. As a reader, I open one of my ebooks on the Kobo.

## Requirements

- **FR-001**: The app signs in with Quick Connect: it shows the code, polls until approved, asks for a fresh code when one expires, and keeps the account in its private store.
- **FR-002**: Every request names the client (`Pharos Kobo`), device (`Kobo`) and device id (`pharos-kobo`) in `X-Emby-Authorization`, with the token once signed in.
- **FR-003**: A request rejected as unauthorised forgets the account and starts sign-in again. Offline and unreachable failures say so and offer Try again.
- **FR-004**: Music lists albums by name, seven rows to a panel page, turned with the side of the panel, and fetches the next fifty before the reader reaches the end. An album lists its tracks in disc and track order with their lengths.
- **FR-005**: A track plays from `/Audio/{id}/universal` as MP3 through Cobalt's audio player, which handles headphones and Bluetooth. Back from the player keeps it playing.
- **FR-006**: The app reports playback to `/Sessions/Playing` on start, `/Progress` when sound starts, on play/pause and every 30 seconds while playing, and `/Stopped` on sign-out.
- **FR-007**: The reader's software is built from Cobalt at a pinned commit plus `kobo/cobalt.patch`, which adds Pharos to the built-in apps and refuses official platform updates.
- **FR-008** (slice 2): Audiobooks list from the Books library; a part over 64 MiB downloads to the app's storage before it plays; chapters, speed and the sleep timer come from Cobalt's player; the place resumes from the server.
- **FR-009** (slice 3): The platform updates itself from Pharos's own signed releases.
- **FR-010** (slice 4): Ebooks open in Cobalt's reader.

## Success criteria

- **SC-001**: In the Cobalt simulator against a local pharos: the code shows, approving it lists albums, an album lists its tracks, and tapping one opens the player and logs `playback: session started` with `client="Pharos Kobo"` on the server.
- **SC-002**: On the Libra Colour: a track is heard in Bluetooth headphones.
- **SC-003**: The device package builds from the flake, and `kobo inspect` accepts it.

## Out of scope

- Typing a server address on the reader: it is fixed when the package is built (`PHAROS_KOBO_SERVER`).
- Offering the app to Cobalt's App Store (ADR-0136 Alternatives).
- Video, lyrics, karaoke.
