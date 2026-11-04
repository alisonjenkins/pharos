# ADR-0053: Songs hand over on the stage, and crossfade is opt-in

- **Status:** Accepted
- **Date:** 2026-11-04T17:00:00Z
- **Deciders:** Alison (asked for crossfade, off unless the listener turns it on), Claude

## Context

ADR-0052 keeps one player on the stage across route changes, but the stage
still mounted a fresh `WatchScreen` per item. On every song change the old
player unmounted at once and the new screen showed `Loading` until its
detail and play RPCs returned, then mounted a new `<audio>`, audio tap and
visualiser canvas: the screen flashed between songs (B466).

A crossfade needs the two songs playing at once. A media element removed
from the document pauses, so the old song cannot fade out after its player
unmounts, and every `platform::*` call and Playwright case finds the media
element by the one id `pharos-video`.

We considered keeping one player and swapping the item inside it, which
would mean re-keying every per-item hook (the report clock, grant, queue
seeding, lyrics, group wiring), and negotiating the next song ahead of time,
which would hold a second grant for a song that may never play.

## Decision

We will let a song hand the stage over to the next. A song that leaves for
another queued song (it ends, a crossfade starts, or the listener picks
another) records a `Handoff` on the `Stage`. The stage keys each player by
a play counter rather than the item, and renders the incoming player first
and the outgoing one after it, each in a `display: contents` slot:

- The outgoing song stops reporting at once (its grant is stopped, at the
  song's length when it crossfades) and its `<audio>` becomes
  `pharos-video-out`, so `pharos-video` is always the playing song's.
- The incoming slot is hidden while it negotiates, so the old song's
  screen stays up instead of `Loading`. Once negotiated, the incoming
  screen shows and the old one is hidden, then unmounted: at once on a
  cut, or when its fade ends.
- With a crossfade of N seconds, the next song starts N seconds before
  the end (never more than a third of the song), fading in from silence
  while the old one fades out, by stepping each element's volume.

Crossfade is off by default and set per device on the Account page
(`pharos.player.crossfade`, 0, 2, 4, 6, 8 or 12 s). Only queued songs hand
over: not video, not a Watch together group, not a song replayed in place.

## Consequences

- No `Loading` between songs; the next song starts after its negotiation,
  so a crossfade's overlap is shorter than N by that round trip.
- Two `.pharos-listen` screens can be in the page for a moment (one
  hidden); tests should locate the playing song by `#pharos-video`.
- A fade moves the element's volume, so the volume slider shows it; the
  listener's stored volume is untouched, and moving the slider ends a
  fade-in.
- iOS ignores `volume`, so a crossfade there is a cut with overlap.
- Episodes still remount between items; an episode handover is a separate
  change.
