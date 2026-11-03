# ADR-0045: The native player draws image subtitles; the server extracts them

- **Status:** Accepted
- **Date:** 2026-11-03T19:00:00Z
- **Deciders:** Alison Jenkins

## Context

013 FR-003 left image subtitles (DVD `dvd_subtitle`, Blu-ray `hdmv_pgs_subtitle`,
DVB `dvb_subtitle`) to burn-in "as the server decides today". In the native
player that means turning one on renegotiates a direct-played film into an HLS
transcode: the stream restarts, quality falls to the transcode ceiling, an
encoder is held for the whole session, and turning the subtitle off
renegotiates again. A browser cannot decode these formats from the file it
direct-plays, and the libraries that render PGS in the browser do not cover
DVD subtitles.

## Decision

We will decode image subtitles on the server once per file and stream, and
have the native player draw them over the direct-played video (spec 019).

- A libav worker op decodes the subtitle stream (`avcodec_decode_subtitle2`
  through `ffmpeg_the_third`), applies each rect's palette and writes it as an
  RGBA PNG, with a JSON manifest of events (start, end, rect on the subtitle
  canvas, forced flag) and the canvas size. Extraction is on demand only,
  behind the background-I/O gate, deduplicated per file and stream, cached on
  disk with a version marker and a byte cap, and never run from a scan or warm
  pass (B49/B52).
- The manifest is served while extraction runs (`complete: false`,
  `through_ms`) so early cues show within seconds; the player polls it.
- A native client opts in by listing the codecs it draws in
  `Capabilities.imageSubtitles`; negotiation then treats them as External and
  the track's delivery is the new `SubtitleDelivery::image` with the manifest
  URL. Clients that do not declare it, jellyfin-web included, keep burn-in.

## Consequences

- An image subtitle on the native player costs one demux of the file and some
  small PNGs instead of a transcode for the session; switching it is instant
  and keeps the play session.
- The player owns placement: it maps the subtitle canvas onto the video's
  displayed content box, including letterboxing and anamorphic DVD aspect.
- A viewer who resumes deep into a cold file waits until extraction reaches
  that point before cues appear (about the 30 s cold read in the worst case).
- Reverting means restoring burn-in for the native player only: drop the
  capability from the client, and the server falls back without a migration.

## Alternatives considered

- Keep burn-in (013 FR-003): works today, but every subtitle toggle is a new
  transcode session.
- Burn while extraction is cold, switch to drawing when ready: a second
  renegotiation mid-film and two code paths for the same choice; the
  progressive manifest makes the cold case short enough without it.
- A browser-side PGS renderer: no DVD subtitle support, so it would not cover
  the title that raised this.
- OCR to text: misreads words, loses placement, and adds a heavy dependency.
- Extract during library scans: B49/B52 showed whole-library subtitle demuxes
  freeze live playback.
