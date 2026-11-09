# ADR-0091: The CPU tonemap chain uses BT.2408 reference white so it matches the GPU route

- **Status:** Accepted
- **Date:** 2026-11-09T00:00:00Z
- **Deciders:** Ali
- **Amends:** ADR-0070 (CPU chain parameters), ADR-0072 (curve and cache key paragraphs)

## Context

ADR-0070's CPU chain maps PQ with `zscale=t=linear:npl=100` and tonemaps with Hable. ADR-0072's GPU route uses libplacebo BT.2390, which places diffuse white near 203 nit (BT.2408). ADR-0072 measured the result on a real 4K HDR10 clip (mean/max luma of the 8-bit output): CPU hable 105.6 / 169, libplacebo BT.2390 143.1 / 224, and left the gap open as "a separate decision". The gap is visible: a segment redone on the CPU after a GPU failure (ADR-0072 Fallback) is darker than its neighbours in the same stream.

## Decision

- **Reference white.** The CPU chain uses `npl=203`, BT.2408 diffuse white, the value the GPU route already assumes. The GPU route is unchanged.
- **Curve.** `tonemap=mobius:param=0.35` replaces `tonemap=hable`. Raising `npl` alone with Hable makes the picture darker, not closer (table below), because Hable's highlight rolloff is steeper than BT.2390's. Mobius is linear below the knee and rolls highlights off gently, which is the shape BT.2390 has. The chain is otherwise ADR-0070's: `zscale=t=linear:npl=203:p=bt709:dither=none,format=gbrpf32le,tonemap=tonemap=mobius:param=0.35:desat=0,zscale=t=bt709:m=bt709:r=tv:dither=none`. The constants are `CPU_TONEMAP_REFERENCE_WHITE_NITS` and `CPU_TONEMAP_MOBIUS_KNEE` in `options.rs`.
- **Cache key.** Tonemapped segments are keyed `_tm2` instead of `_tm`. GPU and CPU outputs share one entry (ADR-0072), so one bump covers both routes; SDR names and `HLS_GEN_VERSION` are untouched, so nothing but HDR-tonemapped segments is re-encoded. Old `_tm` files stay on disk unread. The cap is enforced by a disk walk (`enforce_cap`), not by the in-memory LRU, so it counts them and evicts them oldest-first like any other segment; nothing is orphaned from the cap. Asserted by `enforce_cap_reclaims_orphaned_pre_adr_0091_tm_segments`.

## Consequences

Measured on a synthetic 3840x2160 10-bit HDR10 clip (PQ, BT.2020, MaxCLL 1000; no real HDR fixture is in the repo), mean/max Y of the 8-bit output from `signalstats`:

| Route | Grey gradient clip (natural-like) | Colour test pattern |
|---|---|---|
| CPU, ADR-0070 (hable, npl=100) | 109.9 / 165 | 130.1 / 234 |
| CPU, hable, npl=203 | 88.6 / 135 | 119.7 / 222 |
| CPU, this ADR (mobius 0.35, npl=203) | 124.0 / 190 | 128.6 / 229 |
| GPU, libplacebo BT.2390 | 121.5 / 200 | 160.4 / 235 |

Y at twelve PQ code values of a 0..75% ramp (PQ 0 to about 980 nit), in steps of 1/12 of the width: GPU `16 23 32 43 58 77 101 131 163 192 215 230`; CPU old `16 24 33 45 59 76 97 121 147 174 198 219`; CPU new `16 25 35 48 63 82 107 137 172 197 214 224`. The old chain was close to the GPU in the shadows and 10 to 18 code values under it in the highlights; the new one is 2 to 6 over in the mids and within 9 of it in the highlights.

- On neutral content the CPU mean is within 2.5 code values of the GPU. On the saturated colour pattern the gap stays 30 code values: the CPU chain clips out-of-gamut colour (`desat=0`) where libplacebo gamut-maps, a different gap that this ADR does not close.
- CPU cost is unchanged: 72 frames of 4K through `ffmpeg -benchmark`, utime 20.8 s before, 20.5 s after (hable npl=203: 19.9 s).
- Every HDR title looks brighter on the CPU route than before, matching what mpv and the GPU route show. Cached HDR segments are re-encoded once.
- Not covered: HLG sources use the same constants; no HLG measurement was made.

## Alternatives considered

- **Lower the GPU route to 100 nit.** Rejected: a darker picture than most players show, and it throws away the route ADR-0072 just built.
- **Keep Hable and raise `npl` only.** Measured above: it darkens the picture (88.6 vs 109.9).
- **Reinhard.** Tried at `npl=203`; its mids were darker than Mobius at the same knee and its highlights further from BT.2390.
- **A separate cache key per route.** Rejected by ADR-0072 and still so.
