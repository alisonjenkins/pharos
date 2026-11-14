# Implementation plan: karaoke timing from the singing (042)

**Spec**: [spec.md](spec.md). **ADRs**: ADR-0124 (pipeline), ADR-0125 (models and runtimes).
**Ids**: B1402 (word times follow wrong stamps), B1403 spare; V1372 (lines in order, non-decreasing starts), V1373 spare.

## Evidence the plan rests on

A Python prototype on the owner's desktop (RX 9070 XT), 2026-11-13/14, private regression set of 22 songs across rock, metal, electronic, dance, synth-pop, 60s pop and soul-pop:

- Lines anchored by the recogniser: 97 of 110 on the first 4 songs; 15 to 100% per song on the 22.
- 12 of 20 songs (2 crashed) moved 0-2 lines by more than 2 s against today; the ear-checked points held.
- Cost per 4-5 min song on the GPU: separation 6-16 s (+1.3 GB VRAM), recogniser 6-37 s, median 10 s (fp16, +4.1 GB VRAM), letter emissions ~2 s.

Three failure classes found, each designed out below:

| Class | Seen as | Design answer |
|---|---|---|
| Repeat stealing | a line takes the words of another repeat of the same text, spread over up to 29 s; the refine band then has no path | anchor span capped against the line's stamp slot in every matching pass; stamp distance as a tie-break; an infeasible band falls back |
| Pile-up | unanchored lines after the last anchor squeezed into a few seconds | a line not bounded by anchors on both sides keeps today's per-line time |
| Thin anchoring | very repetitive lyrics (chants, "na na") get under 40% of lines anchored | lines between anchors are placed by the joint pass between their anchors; a song under a minimum anchor share (0.4) keeps today's answer whole |

## Pipeline per song

1. **Today's per-line pass** (spec 039) runs first and is kept as the fallback answer for every line. Its lines stream as today (ADR-0121), so the first lines still arrive early.
2. **Separate** the vocals (htdemucs), whole song, in chunks.
3. **Recognise** the vocals (Whisper large-v3-turbo) with word times.
4. **Match** transcript words to lyric words in lyric order (monotonic DP over normalised words; near-miss spellings scored by letter similarity; a second letter-level pass for lines the first missed). Anchor = at least half the line's words, minimum 2, with its span inside the stamp slot cap.
5. **Refine** each anchored line letter by letter (MMS_FA CTC on the vocals' emission) within the anchor ±1.5 s, with no gap penalty inside the anchor and a small cost for leaving it. An infeasible band keeps the anchor's recogniser times.
6. **Place** the rest in one joint pass: a single banded Viterbi over the whole anchored span times anchored and between lines together, so one line's error cannot spread to the next (per-line windows clamped to the gap did, on the private set). Before it:
   - an anchor whose offset is more than 5 s from both neighbouring anchors' offsets AND that leaps a sung line (a line between it and a neighbour, moved by that neighbour's offset, lands on its far side) is dropped as an outlier;
   - a kept line just outside the span whose today's time overlaps the placed edge line is taken into the span and the pass runs again; if that makes it infeasible, the last feasible answer stands (V1373).
   A line outside any anchor pair keeps today's time.
7. **Emit** a corrected event for every line whose time changed, then the whole answer (ADR-0121 shape, additive `placement` per line).

Models load in stages and are released between them: separator, then recogniser, then the letter model already resident. Peak GPU memory for one song stays under 6 GiB (FR-010).

## Module shape (`crates/pharos-align`)

| Module | Holds | Slice |
|---|---|---|
| `obs` | `pharos_aligner_line_placement_total{method}` (anchored, refined, between, kept), `pharos_aligner_song_fallback_total{reason}`, and the per-song summary fields | 0 |
| `transcript` | the recogniser's word list type and a reader for a JSON transcript file (lets slices 1 and 2 land before the models) | 1 |
| `anchor` | the monotonic matcher, slot cap, stamp tie-break, anchor share | 1 |
| `refine` | banded CTC inside an anchor, feasibility check, between-anchor placement | 1 |
| `separate` | htdemucs: ONNX import into Burn first (half-day spike), hand port if it misses | 3 |
| `recognise` | Whisper turbo: whisper.cpp through `whisper-rs` with Vulkan first (one-day spike), Burn hand port if word times or stability miss | 2 |
| `service` | staged pipeline, corrected-line events, stage failure falls back to the per-line answer (FR-011) | 4 |

`ALIGNER_VERSION` goes 5 → 6 when slice 4 lands. The server's `MIN_ALIGNER_VERSION` stays, so cached songs keep their words until the operator re-aligns them.

## Slices

| Slice | Content | Size |
|---|---|---|
| 0 | Placement and fallback signals on today's path (all lines `kept`), with tests that fail without them | 0.5 d |
| 1 | `transcript`, `anchor`, `refine` in Rust over transcript files, unit tests on synthetic emissions and made-up lyrics; parity against the Python prototype's outputs on the private set (run on the desktop, results not committed) | 5 d |
| 2 | Recogniser spike (`whisper-rs` Vulkan) then the module; per-word time parity with the prototype within 100 ms | 1 d spike, 2-11 d |
| 3 | Separator spike (ONNX import) then the module | 0.5 d spike, 2-9 d |
| 4 | Service wiring, staging, events, version bump, chart memory limits | 2 d |
| 5 | Deploy, owner's OK to re-align, verify by query | 0.5 d |

## Verification (ODD)

- Signal first: slice 0 ships before any placement change, so the share of `kept` lines today (100%) is the baseline.
- After deploy: `sum by (method) (increase(pharos_aligner_line_placement_total[1d]))` and `sum by (reason) (increase(pharos_aligner_song_fallback_total[1d]))`, plus LogQL on `{namespace="pharos"} |= "song aligned" | logfmt | anchor_share != ""` for per-song anchor share. Report the output (SC-006).
- Accuracy: the ear-checked points from the owner, kept privately on the desktop, rerun after each slice that changes placement.

## Risks

- whisper.cpp brings C++ into the build and a second GPU runtime beside Burn. If the spike fails, the Burn port is the longest piece (~11 d).
- The recogniser hallucinates in gaps ("thank you", captions). The matcher ignores words that match no lyric in order; the second letter pass must not use them to steal repeats.
- GPU memory on the cluster node is shared with other models. Staging and idle unload keep the song peak under 6 GiB; the chart sets the limit.
- Only 22 songs and a handful of ear checks back the design. Each slice reruns the private set before it merges.
