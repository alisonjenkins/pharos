# ADR-0067: Subtitle styling is a server-side effect plan, compiled to ASS per device tier

- **Status:** Proposed
- **Date:** 2026-11-06T00:50:00Z
- **Deciders:** Alison (design phases 0 and 1 approved), Claude

## Context

Spec 028 styles each subtitle line by the kind of speech it is: a shout, a whisper, an inner voice, a phone call. Alison's standing requirement is that this runs well on weak devices. The bedroom TV, a TCL G08, is the reference. All analysis has to happen on the server, ahead of time. Clients today draw ASS through JASSUB, and spec 026's own Rust renderer (`pharos-subs`) will take over. Both draw the same libass tag set. The design (`docs/design/animated-anime-subtitles.md` §3, §8a, §8b) asks for plain ASS v4+ on the wire and no custom format. Phase 1 uses no ML, only signals Pharos has or can read in one pass over the file.

## Decision

We will split styling into a semantic **effect plan** and a deterministic **compiler**, in a new pure crate, `crates/pharos-styling`. The crate has no I/O, no async and no randomness.

- **Plan.** One `LineEffect` per source line: intent (11 values, design §3), intensity 0-3, placement, karaoke, and a `reason`. The plan, not ASS, is what the director produces and what is stored. Storage is a `subtitle_styling` table, migration 0068 (sqlite and postgres), keyed by item and source index. The plan is JSON in the `plan_json` column, alongside the source signature (the video's mtime and size, plus a sidecar source's own) and `DIRECTOR_VERSION`. JSON keeps a stored plan readable by hand when someone asks why a line shook. A plan is a few tens of KB per episode.
- **Compiler.** `(plan, source lines, theme, tier, effects level) -> ASS v4+` text, byte-for-byte deterministic. It runs at request time, because it costs milliseconds. Its output is cached in the subtitle cache under `SubtitleKind::Styled`, keyed with `COMPILER_VERSION`. Three tiers, Low, TV and Desktop, each with a hard budget of tags and animated events that a tag scanner in the crate checks. `pharos_subs::decode` cannot check it yet, because on main it flags every animation tag as unsupported until spec 026 slice 3.
- **Director.** An ordered list of rules in the same crate, a pure function of the source lines, the per-line audio signals, the media segments, the chapters and the tempo. Its I/O lives in a `subtitle_styling` background job in `pharos-server`, modelled on `audio_analysis`.
- **Signals.** One new libav worker op, `SubtitleSignals`. In a single demux pass it decodes the subtitle-timed audio stream (the front-centre channel when there is one) for EBU R128 short-term loudness, YIN pitch and high-band share per line. It also reads video packet keyframe flags as a scene-cut proxy. No new third-party dependency.
- **Serving.** The styled track is a synthetic subtitle index after the sidecars. In phase 1 it is listed in the native detail only, and served at `stream.ass?tier=&effects=`.

## Consequences

- A rule or effect change is a version bump. A bumped `DIRECTOR_VERSION` restyles items in the background. A bumped `COMPILER_VERSION` only invalidates cached ASS.
- Clients stay simple. Any ASS renderer draws the output, and the tier is a query parameter. The renderer thread can swap JASSUB for `pharos-subs` without touching this work.
- The keyframe proxy for cuts costs nothing beyond the demux the audio decode already does. It misses cuts in fixed-GOP encodes. Because a cut only stops animation from running across it (FR-011), a miss makes an effect run a little long, never wrong. If the reference episodes show misses that matter, decoding with `scdet` is the fallback, at the cost of a full video decode per episode.
- Phase 2 detectors (diarisation, emotion, OCR, sign tracking) add plan fields and director inputs. The compiler and the wire format do not change.
- Jellyfin clients do not see the track in phase 1. Adding it later means keeping B71's contiguous index rules, and those clients would get the TV tier.

## Alternatives considered

- **Director writes ASS directly.** This loses the tier and effects-level variants (one analysis, six renderings), and the testable intent layer. Geometry and timing in ASS are easy to get subtly wrong (design §3).
- **Compile all six variants in the job.** This spends storage and job time on variants nobody requests. Compiling at request time and caching costs a few ms on the first request.
- **Plan in the subtitle cache, not the store.** The cache is LRU-bounded. Evicting a plan would mean reading the whole file again (gigabytes over NFS) to rebuild it.
- **`pharos-subs` as the source parser.** Its model keeps only the plain-tier runs and drops raw override tags and `\k` timings. FR-005 passthrough and karaoke need both, so `pharos-styling` reads the source itself. `pharos-subs` is a test-only dependency, used to prove the output parses.
- **`scdet` scene detection now.** It is better at finding cuts, but it decodes every video frame per episode on the background path. It is deferred until there is evidence the proxy is not enough.

## References

- Spec 028 (`specs/028-anime-subtitle-styling/`), design doc §3, §8a, §8b, §9.
- ADR-0057 (JASSUB), ADR-0064 / spec 026 (`pharos-subs`), ADR-0050 (`audio_analysis`), ADR-0018 (media segments).
