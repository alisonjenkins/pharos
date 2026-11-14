# ADR-0136: The Kobo app is a Cobalt app, built from a pinned and patched Cobalt

- **Status:** Accepted
- **Date:** 2026-11-14T21:30:00Z
- **Deciders:** Alison

## Context

Alison wants a Pharos client on a Kobo Libra Colour: sign in, read ebooks,
and listen to audiobooks and music (spec 047). The reader has no speaker or
headphone socket; sound goes over Bluetooth only. Its userspace is 32-bit
ARMv7 Linux on a MediaTek SoC, with 1 GB of RAM, under Kobo's own reader
("Nickel").

A survey of Rust projects for Kobo (2026-11-14) found:

- **Cobalt** (BandarLabs/Cobalt, AGPL-3.0-only): a launcher, a sandboxed
  per-app runtime, a Rust SDK of declarative screens, a browser simulator,
  and an EPUB reader and OPDS client as crates. Its runtime decodes MP3, AAC,
  Ogg, FLAC and WAV and plays them to Bluetooth headphones on the MediaTek
  readers, and its device matrix lists the Libra Colour as tested.
- `kobo-core` (MIT): a device kit that claims Bluetooth audio, one commit.
- `mezzotint` (MIT or Apache-2.0): e-ink refresh only.
- Plato and its fork Cadmus (AGPL): readers with no audio.
- KoThok: read-aloud reader under a non-commercial licence.

A spike (2026-11-14, Cobalt commit `5ac30a46`) signed in to a local pharos
with Quick Connect from the Cobalt simulator, listed music, and decoded the
stream pharos served with Cobalt's own decoder. The app built as a static
ARM binary. The simulator has no audio, so sound is unproven until a device
test.

Cobalt constraints found in the spike:

- An app reaches the network only through runtime HTTPS tasks. It cannot
  open a socket, so pharos's Cap'n Proto WebSocket API is out of reach.
- The runtime reserves the `Authorization` header and attaches named secrets
  only for app and credential pairs written into its policy code.
- An audio stream is fetched unauthenticated, whole, into memory, up to
  64 MiB, before it plays.
- Only apps compiled into the runtime's built-in list ship in its device
  package, and the runtime replaces itself with official releases, which
  would not carry our app.

## Decision

We will build the Kobo client as a Cobalt app, and build the reader's
software from a pinned Cobalt with a small patch, rather than keep a fork.

- `kobo/` in this repository holds the app (`kobo/app`, crate
  `kobo-pharos`), the patch (`kobo/cobalt.patch`) and the pinned Cobalt
  commit. The flake fetches Cobalt at that commit, applies the patch, adds
  the app as `examples/pharos`, and builds the device package. A newer Cobalt
  is a pin bump plus a patch refresh.
- The patch adds Pharos to Cobalt's built-in apps and device package, and
  refuses official platform updates, which would remove Pharos. Updates of
  our own (our feed, our signing key) are follow-up work in spec 047.
- The app talks to pharos over the Jellyfin-compatible REST API: Quick
  Connect to sign in, `Items` to list, `/Audio/{id}/universal` to stream,
  and the playing-session endpoints to save the listener's place. It sends
  its client identity and token in `X-Emby-Authorization`.
- The app holds its own access token and keeps it in its private store,
  because the runtime only attaches the secrets its policy names, and audio
  streams carry the token as `api_key` in the URL because the runtime fetches
  them without credentials. The token is a normal pharos session token with
  its own device id, revocable from the devices page.
- The app is AGPL-3.0-only, as Cobalt's SDK requires; pharos is
  AGPL-3.0-or-later, so the two combine.

## Consequences

- Bluetooth audio, e-ink refresh, EPUB pagination, OPDS parsing and
  Wi-Fi handling come from Cobalt and are not ours to maintain.
- The first install of Cobalt turns on SSH on the reader with the installing
  computer's key. Kobo firmware 5.x is not supported by Cobalt.
- The app's token is in app memory and in stream URLs, which Cobalt's design
  avoids for third-party keys. A runtime credential policy for pharos, and
  streams that carry it, would be a bigger patch.
- Long audiobook parts (over 64 MiB) cannot stream; they download to the
  app's private storage and play from there.
- Pharos-native features over the Cap'n Proto API (push events, the shared
  item cache) are unavailable to this client.
- Every Cobalt bump must reapply the patch; its `Cargo.lock` hunk is the
  likeliest to conflict.
- Building needs the Cobalt toolchain (Rust 1.85.1) and an ARM hard-float C
  compiler for the `armv7-unknown-linux-musleabihf` target.

## Alternatives considered

- **Our own app on `kobo-core`**, started from NickelMenu: we would write the
  UI, the reader and the audio glue ourselves, on a one-commit SDK. About
  twice the work.
- **A fork of Cobalt**: rejected by Alison for the upkeep of merging its
  updates; the pinned patch keeps our change in one reviewable file.
- **Offering the app to Cobalt's App Store**: cleaner on the reader, but
  waits on their review of a pharos credential policy. Can still happen
  later.
- **KOReader with OPDS** (spec 045) remains for reading; it has no audio.

## References

- Spec 047 (`specs/047-kobo-app/`).
- ADR-0128 (e-reader passwords open only the OPDS catalogue).
- Cobalt: https://github.com/BandarLabs/Cobalt (`SDK.md`, `docs/DEVICES.md`).
