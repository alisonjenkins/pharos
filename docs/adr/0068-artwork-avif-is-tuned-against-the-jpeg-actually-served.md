# ADR-0068: Artwork AVIF is tuned against the JPEG actually served

- **Status:** Accepted
- **Date:** 2026-11-06T00:00:00Z
- **Deciders:** Ali
- **Supersedes:** the encode settings in ADR-0063 (crf 22). The rest of ADR-0063 stands.

## Context

ADR-0063 chose crf 22, the smallest AVIF that matched an ffmpeg CLI
`-q:v 3` JPEG. Production does not serve that JPEG. The libav worker sets
`global_quality` without the `QSCALE` flag, so its MJPEG encoder is
rate-controlled, and its 480 px posters score about 67 on SSIMULACRA2
(quant tables near IJG q71-86) where the CLI's score about 79. Against the
real JPEG, crf 22 AVIF was bigger in 9 of 12 live posters, for example
47.8 kB AVIF against 36.7 kB JPEG.

The benchmark was re-run on the same 30 images against an emulation of the
production JPEG (mjpeg, `global_quality 3`, no qscale). Median bytes and
SSIMULACRA2 against it, for AVIF 4:4:4 `tune=iq`:

| crf | Poster 480 | Poster 240 | Backdrop 1280 | Still 640 |
|---|---|---|---|---|
| 22 | 1.32x, +14.6 | 1.08x, +8.5 | 1.49x, +13.2 | 1.09x, +10.0 |
| 31 | 0.88x, +8.2 | 0.74x, +2.5 | 0.90x, +7.7 | 0.78x, +4.1 |
| 34 | 0.76x, +5.1 | 0.65x, +0.1 | 0.76x, +4.7 | 0.68x, +1.5 |

## Decision

- **crf 31.** Every role gets both fewer bytes and a visibly better image
  than today's JPEG; `ENCODE_VERSION` goes to 2 so crf 22 files are not
  served.
- **The smaller file wins.** When an encoded copy is bigger than the JPEG the
  route would otherwise send (a source that is already a small, heavily
  compressed JPEG), the JPEG is sent, under the normal max-age.

## Consequences

- Cold AVIF posters are about 12% smaller than the JPEG instead of about 30%
  bigger, and about 8 SSIMULACRA2 points better.
- The production JPEG's own quality bug (rate control instead of
  `-q:v 3`) is untouched: fixing it raises every Jellyfin client's image
  bytes, which is a separate decision.
- Each encoded request also looks up the JPEG at the encode's bucket width
  (a source header read and a cache check) and reads both files' sizes; a
  JPEG evicted from the cache is made again on that request.
