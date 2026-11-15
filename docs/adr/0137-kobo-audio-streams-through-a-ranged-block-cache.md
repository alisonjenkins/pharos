# ADR-0137: Kobo audio streams through a ranged block cache and is kept on the reader

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

The reader also drops off Wi-Fi: in a pocket, on a train, in a room the
router does not reach. A stream that must be fetched as it plays stalls
there, and a book the owner has already listened to cannot be started at all.
So the blocks are kept.

- Disk store. A stream from a ranged server is written block by block to a
  file the runtime owns, under `/mnt/onboard/.adds/cobalt/downloads/<app>/`,
  named by a 128-bit FNV-1a hash of the URL without its `api_key` (a new
  sign-in keeps the download). Beside it a sidecar records the length, the
  block size, when it was last played, which blocks are present and which
  generation of the data file it describes: the file is named
  `<hash>.<generation>.data`, a replacement writes its own beside the old one,
  and renaming the sidecar is the single commit, after which files no sidecar
  names are swept. (An inode would not do: FAT renumbers them on every
  remount.) A block
  counts only once it is on disk; the sidecar is saved every 16 blocks and on
  close, after the data is synced. The RAM cache stays the hot layer and the
  file is the backing store. The folder is not the app's shelf: a shelf is
  capped at 256 MiB and counts every file under it.
- Download. Once a book from a ranged server is open, the read-ahead thread
  fetches every missing block from the read position on and then from the
  start, with the same backoff. Playback's blocks come first and end the wait
  on a far block. A block the network cannot give for 30 s pauses the download
  for 60 s and never stops playback from the blocks already on disk. The
  length is checked when the last block lands, and the download is marked
  complete.
- Offline. A complete download opens with no request, and an app that has no
  network stack at all still opens it. A partial one opens from its recorded
  length when the server cannot be reached, plays what is present and pauses
  with `ConnectionLost` at the first gap. A download whose length or block
  size no longer matches the server's is replaced.
- Budget. All downloads together take at most 4 GiB, counted by the full
  length of each book. Starting one that does not fit evicts the least
  recently played complete downloads that are not open; if that is not enough
  the book is streamed without being kept. Nothing is started or continued
  while the volume has under 2 GiB free (`statvfs`), because the card also
  holds the stock reader's database. Both limits and the free-space and clock
  readings are injectable for tests.
- Protocol, gated by `DOWNLOADS_VERSION` (18). `ListDownloads` (request 63)
  names up to 64 stream URLs and is answered by `Downloads` (result 26) with
  how many bytes of each are kept and how long it is; `RemoveDownload`
  (request 64) deletes one, answers `Done`, `NotFound`, or `Denied(Busy)`
  while that book is the loaded one. Playing a stream is what starts its
  download. Both requests need the same declaration as playing a stream
  (`bluetooth-audio`) and keep working with the network off.
- Observability. Start, resume, complete, evict, remove, low-space skips,
  over-budget skips, write failures and fetch failures are logged to standard
  error with the key, the byte counts and the cause, never the URL (it carries
  the token). `DownloadStats` counts them and the block bytes written;
  `RemoteStats` counts blocks read back from disk.

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
  reported as `StreamTooLong`, and is not kept.
- Up to 4 GiB of the reader's book storage is spent on downloads, and only
  the least recently played complete download is given back, so a full
  budget of half-played books refuses a new one rather than deleting them.
- The download runs for as long as the book is loaded, keeping the Wi-Fi
  radio busy, which costs battery. Closing the book stops it, and the next
  play resumes from the sidecar.
- A power cut loses at most the 16 blocks since the last save; it never
  leaves a block marked present that is not on disk.
- The new numbers are wire values: 63, 64 and 26 are the next free request
  and result tags, and a later Cobalt bump must keep them.
- The Pharos app lists a downloaded book when the server is out of reach from
  names it saved in its own store (a few hundred KiB at most) and queues
  progress reports there, so what it keeps is a copy that can lag the server.

## Alternatives considered

- Raise the whole-download cap: memory grows with the book, and 1 GB is
  shared with everything else.
- Download long parts to the app's private storage first (ADR-0136's
  fallback): a long wait before the first sound, and the storage cost. The
  block store plays from the first block while it downloads.
- Keep downloads in the app's shelf: capped at 256 MiB, and an audiobook
  does not fit.
- Let the app name a path, or list the folder: an app could then read or
  remove another app's files. It names a stream URL and the runtime maps it.
- Stream through a socket: Cobalt apps reach the network only through
  runtime HTTPS tasks.

## References

- ADR-0136; `kobo/cobalt.patch` (`crates/kobo-audio/src/remote.rs`,
  `crates/kobo-hal/src/audio.rs` `open_stream`, `kobo_net::fetch_range`).
- B1450, V1424, V1425.
