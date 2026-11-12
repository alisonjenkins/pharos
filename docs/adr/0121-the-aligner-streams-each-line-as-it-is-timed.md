# ADR-0121: The aligner streams each line as it is timed

- **Status:** Accepted
- **Date:** 2026-11-12T09:17:44Z
- **Deciders:** Alison

## Context

A karaoke song nobody has aligned yet plays with guessed word fill until the
aligner's answer arrives. `POST /align` answered once, with every line, so the
whole song had to be timed before any of it could be shown, and the player
asked again only every 5 s. On the GPU a song takes 2 to 12 s (2026-11-11, 16
songs), plus any queue wait or model reload, so the opening lines were always
guessed. Alison (2026-11-12T08:56Z): "Would streaming be possible? I think that
is a better UX".

The aligner already works in two stages: an offset search over a few short
probes, then each line in order. A line's words are final once it is timed; the
next line reads only where it ended. So the answer can be sent piece by piece
without changing any word time.

## Decision

We will let `/align` stream its answer as newline-delimited JSON
(`application/x-ndjson`) when the caller's `Accept` asks for it:

- the 200 goes out at once; validation errors (400, 404, 413) stay plain
  statuses before it;
- `{"event":"offset","offset_ms":…,"aligner_version":…,"lines":N}` once the
  offset is found;
- `{"event":"line","index":i,"words":[…],"score":…}` as each line is timed, in
  order, exactly as it will appear in the final answer;
- the last event is either `{"event":"done", …}` carrying the whole answer in
  the plain reply's shape, or `{"event":"error","status":422|500,"error":…}`
  with the status the plain reply would have had.

Without the header the answer is the single JSON body it always was. The server
asks for the stream for song jobs only (not sung subtitles, not the shadow). It
keeps the lines in memory per song while the job runs and serves them, with the
offset applied, in the lyrics answer still marked words-pending. It caches only
the `done` answer, read by the same parser as the plain reply. The player asks
every 2 s while words are pending and shows each timed line as it comes; the
rest keep the guess.

## Consequences

- The opening lines get real times after the offset search and the first line,
  not after the whole song.
- The offset still moves every line at once, so there is one jump when the
  first answer with lines lands, now seconds earlier.
- A stream that fails or is cut before `done` fails the job and stores nothing;
  the lines already shown stay on screen until the song changes.
- The server reads either shape, so the server and aligner can deploy in either
  order. An aligner that ignores the header answers whole, as before.
- The player asks 2.5 times as often while a song is pending; each ask is a
  cache and memory read on the server.
- The log lines "lyrics: aligned" and "lyrics: alignment failed" carry
  `lines_streamed` and `first_line_ms`, the time from queueing to the first
  timed line.

## Alternatives considered

- **Push over a live connection (SSE or WebSocket) to the player.** The quickest
  path to the screen, but a new transport between server and player for one
  banner. Polling at 2 s is already in place and costs at most 2 s per update.
- **Align the song in chunks of lines with one request each.** Repeats the
  offset search or needs it carried between requests, and splits the
  one-song-at-a-time lock.

## References

- B1326, B1327; V1297, V1298 in `specs/001-pharos-baseline/`.
- `crates/pharos-align/src/http.rs` (`Events`, `wants_events`),
  `crates/pharos-align/src/service.rs` (`Progress`).
- `crates/pharos-server/src/services/lyrics/align.rs` (`Event`, `Partial`,
  `Lookup::Partial`).
- ADR-0066 (the aligner service).
