# ADR-0062: Artwork AVIF is encoded from the source, in the background

- **Status:** Accepted
- **Date:** 2026-11-05T00:00:00Z
- **Deciders:** Ali

## Context

Posters, backdrops and stills are the bulk of a library page's bytes, and the
targets include TVs and phones on slow connections. The image route served a
q:v 3 JPEG at a width capped per role. `?format=avif|webp` existed, but the
production backend (`ffmpeg-lib`) does not implement `transcode_image`, so it
silently served the JPEG. The spawn backend re-encoded that JPEG, so the copy
carried two rounds of loss and never reached the JPEG's own quality.

A benchmark on 30 library images (14 posters, 8 backdrops, 8 episode stills)
scored each encode with SSIMULACRA2 against a lossless downscale of the
original, and took the smallest file scoring at least today's JPEG:

| Encoder | Bytes vs today's JPEG |
|---|---|
| AVIF, libaom `tune=iq`, 4:4:4, crf 22 | 0.73x posters, 0.82x backdrops, 0.74x stills |
| WebP q85-88 | 0.9-1.07x; misses on some posters at q85 |
| jpegli / mozjpeg | 0.9-1.05x |
| AVIF 4:2:0 | smaller, but loses grain on dark posters |

An AVIF still costs 0.1-1 s of one core to encode, against about 50 ms for the
JPEG, on a node already busy with transcodes. AVIF decodes about 4x slower
than JPEG on a desktop. TV decode cost is not yet measured.

## Decision

- **Encode from the source.** `ImageCache::encoded_artwork` runs ffmpeg on the
  original sidecar or extracted frame, not the scaled JPEG: lanczos scale
  (never up), 4:4:4, libaom-av1 `allintra`, `cpu-used 6`, `crf 22`,
  `tune=iq`. `?format=webp` uses the same path with libwebp q85; it is kept
  correct but not tuned, since it does not beat the JPEG by enough to use.
- **Never make a request wait for an encode.** A miss queues one background
  encode and the request gets the sized JPEG with `Cache-Control: private,
  no-cache`, so the client revalidates and gets the AVIF (new ETag, a week's
  max-age) once it exists. One encode runs at a time, newest request first
  (the tiles on screen now beat the ones scrolled past); past 512 queued the
  oldest is dropped, and an encode is killed after 60 s. A failed encode
  leaves a marker, so that source is served as JPEG, cacheably, until it
  changes rather than re-encoded on every view.
- **Bucket the widths.** Encodes are made at 240/360/480/640/960/1280 px,
  rounding a requested width up, so arbitrary `fillWidth` values from
  Jellyfin clients cannot multiply the work.
- **Version the settings.** `ENCODE_VERSION` is in the cache key; changing the
  settings means bumping it.
- Requests without `?format=` (every Jellyfin client today) are unchanged.

## Consequences

- The first view of a cold image is a JPEG; later views save about a quarter
  of the bytes at equal or better quality.
- The image cache holds one more file per (image, width bucket, format).
- A client that cached an encoded image keeps it up to a week, so a settings
  change reaches clients only after their max-age, unless the URL changes.
- The UI must send `?format=avif` only where it can show AVIF, and a TV whose
  decoder is slow should keep JPEG until measured.

## Alternatives considered

- **WebP everywhere.** Universal support, but at today's quality it saves
  0-10%, and at q85 misses on some posters.
- **Encode inline on the request.** Simpler, but makes every cold tile up to a
  second slower on a busy node.
- **Pure-Rust encoder (`ravif`) or `avifenc`.** avifenc's files were about 5%
  smaller at 3-4x the encode time; ffmpeg is already in the image and on the
  CI path. The encode spawns the ffmpeg CLI rather than the resident libav
  pool, which only writes JPEG; at one encode at a time the spawn cost is
  small next to the encode.
- **`?format=` on Logo, Banner, Art and Disc.** These pass through untouched
  (alpha PNGs a 4:4:4 YUV encode would flatten), so the format hint is
  ignored for them. Production never encoded them anyway: the libav backend
  had no `transcode_image`, which this removes.
