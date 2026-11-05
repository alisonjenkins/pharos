# ADR-0066: Lyric word times come from a separate aligner service, run on demand for karaoke

- **Status:** Accepted
- **Date:** 2026-11-05T01:10:00Z
- **Deciders:** Alison Jenkins

## Context

Karaoke mode (B483) fills the sung line from left to right. Most lyrics,
LRCLIB's included, time lines, not words, so the fill is an estimate from
syllables (B495). Enhanced-LRC word stamps are used when a file has them
(B496, schema v15 `LyricLine.words`), but few files do. Alison asked for a
model to work the word times out from the audio.

Forced alignment does this: given the words and the audio, a CTC acoustic
model (wav2vec2) finds where each word falls. It needs PyTorch. The pinned
nixpkgs has `torch` 2.13 and `torchaudio` 2.11 in the binary cache, and
torchaudio ships `functional.forced_align` and the `MMS_FA` multilingual
wav2vec2 bundle. Demucs, which would split the vocals out of the mix first,
is not in nixpkgs. The cluster has CPUs only, so a model like this takes
seconds to tens of seconds per song. Its image is gigabytes and its memory
use is far more than the server's.

## Decision

We will run alignment in its own service, `pharos-aligner`, a small Python
program on nixpkgs `torch`/`torchaudio` built as its own image (flake output
`alignerOci`, not part of `oci`). It has one HTTP route, `POST /align`, taking
a media-relative file path and the song's lines (start, end, text) and
answering word stamps per line in the v15 shape (char offset, ms). It reads
the media through the same mount as the server, read-only, so no audio
crosses the wire, and decodes each line's window (its LRC span plus a small
margin) with ffmpeg. Aligning a line inside its own window, rather than the
whole song, keeps a mistake in one line out of the next and costs less.

The server calls it only when `[lyrics].aligner_url` is set (off by default)
and only for a song a listener has opened in karaoke mode: the lyrics call
says so (schema v20), and synced lyrics without word times then queue one
alignment job. Jobs run one at a time from a bounded, de-duplicated queue on
the replica that took the call, like the trickplay queue; a song whose job
failed is left alone for an hour. The answer is cached on disk
beside the lyrics cache, keyed on the lines' text and times and the audio
file's signature, so new lyrics or a changed file align again. Word stamps
from the lyrics source always win over aligned ones. While the answer says the
words are pending, a client in karaoke mode asks again every 20 s, for up to
five minutes.

We will not separate vocals at first. If alignment on full mixes proves too
rough, Demucs can be packaged into the aligner image without changing the
server or the wire.

## Consequences

- The main image and the server's dependencies are unchanged; a server
  without an aligner behaves as before.
- Running it means a second Deployment, its image, and the media mount.
  Alison approved the 3-4 GB image on 2026-11-05: CI publishes `alignerOci`
  as `ghcr.io/alisonjenkins/pharos-aligner`, tagged by its store hash and
  pushed only when that tag is missing, and the chart runs it behind
  `aligner.enabled`.
- MMS_FA aligns romanised text: words with no ASCII letters (Japanese,
  Cyrillic, most digits) get no word times and keep the syllable estimate.
- An answer is cached per file and line text, empty lines included, so a
  line the aligner could not place is not retried until the file or lyrics
  change.
- The model weights (about 1.2 GB) are fetched into the image at build time
  with a fixed hash, so the service needs no network at run time.
- The first karaoke play of a song still shows the estimate; the aligned fill
  arrives on a later fetch, then on every play from the cache.
- Accuracy on full mixes without vocal separation is unproven; it is checked
  by hand on a few songs before the deployment goes live.
