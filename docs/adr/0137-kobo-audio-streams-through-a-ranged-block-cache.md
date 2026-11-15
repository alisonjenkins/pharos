# ADR-0137: Kobo audio streams through a ranged block cache

- **Status:** Accepted
- **Date:** 2026-11-15T14:20:00Z
- **Deciders:** Alison

## Context

ADR-0136 recorded that Cobalt fetches an audio stream whole into memory, up
to 64 MiB, before it plays. A multi-hour audiobook part is several hundred
megabytes, so it failed to load. The failure reached the listener as "This
reader cannot play this kind of audio file", because the over-cap case
returned the same `DeviceError::InvalidInput` as an unplayable format.

The reader has 1 GB of RAM. Pharos serves `/Audio/{id}/universal` with
`Range` support and sends the original file (M4B, FLAC, MP3 and so on). The
URL names no container. An M4B often keeps its `moov` index at the end of
the file, so a probe reads the end before the start.

## Decision

We will extend `kobo/cobalt.patch` so a stream from a server that sends
ranges is read through a block cache, not held whole.

- `kobo-audio` gets `Remote`: fixed 256 KiB blocks, at most 32 resident
  (8 MiB), least-recently-used eviction outside the window around the read
  position. It is `Read + Seek` and a symphonia `MediaSource`. Every open of
  the book shares one cache.
- A read-ahead thread keeps about 24 blocks (6 MiB) after the read position
  fetched. It retries a block with backoff of 0.5, 1, 2 and 4 s, about 30 s
  in total, then reports the network lost. It stops when the book is
  dropped.
- `kobo-net` gets `fetch_range`: a 64-bit offset, and whether the reply was
  a `206` plus the total length from `Content-Range`.
- The container comes from the opening bytes (`ftyp`, `fLaC`, `OggS` and
  `OpusHead`, `RIFF`/`WAVE`, `ID3` or a frame sync), then from the name.
- A server that ignores ranges is read whole in one request up to the old
  64 MiB cap. A `200` to a later range is refused, not misread.
- `DeviceError` gains `StreamTooLong` (8), `ConnectionLost` (9) and
  `Unsupported` (10), each with its own player message. A lost network
  pauses the book and keeps its place.

This replaces the stream bullets of ADR-0136's Context and Consequences;
the rest of ADR-0136 stands.

## Consequences

- Any length of book streams in flat memory: 8 MiB of blocks plus the
  decoder.
- The new error codes are wire values. A Cobalt app built against an older
  `kobo-protocol` cannot decode them. The Pharos build ships runtime and
  apps together, so this holds today; a later Cobalt bump must keep the
  numbers.
- The patch is larger (about 2,100 more lines), so a Cobalt pin bump costs more
  to rebase.
- While the network is down the audio worker thread blocks in a read for up
  to about 45 s, and commands such as Pause wait behind it.
- A server that does not send ranges still has the 64 MiB limit, now
  reported as `StreamTooLong`.

## Alternatives considered

- Raise the whole-download cap: memory grows with the book, and 1 GB is
  shared with everything else.
- Download long parts to the app's private storage first (ADR-0136's
  fallback): a long wait before the first sound, and the storage cost.
- Stream through a socket: Cobalt apps reach the network only through
  runtime HTTPS tasks.

## References

- ADR-0136; `kobo/cobalt.patch` (`crates/kobo-audio/src/remote.rs`,
  `crates/kobo-hal/src/audio.rs` `open_stream`, `kobo_net::fetch_range`).
- B1450.
