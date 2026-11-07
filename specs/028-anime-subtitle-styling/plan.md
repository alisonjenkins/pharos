# Implementation plan: Pharos-styled animated subtitles (028)

**Spec**: [spec.md](spec.md) · **ADR**: ADR-0067 · **Created**: 2026-11-06T00:45:00Z

## Technical context

- Language and stack: Rust (workspace toolchain), actix-web, sqlx (sqlite + postgres), the libav worker pool (`pharos-transcode`, P48), Dioxus web UI (`pharos-ui`).
- No new third-party dependency. Pitch (YIN) and EBU R128 short-term loudness (K-weighting biquads plus a 3 s window) are written in Rust inside the worker, next to `audio_features.rs`. No ML in phase 1.
- Performance goals: the budget table in spec.md. The server takes <= 5 s CPU and one file read per episode. The client pays one ASS download.

## Constitution check

- TDD: each slice starts with a failing test. The compiler and director are pure, so they are tested without I/O.
- ODD: metrics and log lines are named in spec FR-021 and asserted in tests, with a disarm check on each. The queries are under "Signals" below.
- V17: no `unwrap`/`expect` outside tests.
- ADR: ADR-0067 records the module boundary, the plan storage format and the scene-cut proxy.

## Module layout

| Where | What |
|---|---|
| `crates/pharos-styling` (new, pure, no I/O, no async) | `plan.rs` (EffectPlan, LineEffect, Intent, Placement, Karaoke), `source.rs` (reads SRT/WebVTT/ASS into `SourceLine`s and keeps each ASS event's raw text, override tags and `\k` timings), `signals.rs` (LineSignals, TrackSignals), `director.rs` (ordered rules), `theme.rs` (the `pharos` theme: faces from the V530 pack, colours, outlines), `compile.rs` (plan + source + theme + tier + level to ASS text), `effects/*.rs` (one file per effect, each with `low`, `tv` and `desktop` forms), `budget.rs` (tag scanner and tier budget checker, used by tests and by a debug assertion after compile), `version.rs` (`DIRECTOR_VERSION`, `COMPILER_VERSION`). |
| `crates/pharos-transcode` | `TinyOp::SubtitleSignals` and `WorkerEvent::SubtitleSignalsResult` (appended last, so bincode indices stay stable), `src/subtitle_signals.rs` (R128 short-term, YIN, high-band share, keyframe-cut proxy), `libav/subtitle_signals.rs` (one demux pass: decode the audio stream and read video packet flags), `LibavWorkerPool::subtitle_signals`. |
| `crates/pharos-core` | `SubtitleStyling` record and the `SubtitleStylingStore` trait (put/get plan per item, track and signature). |
| `crates/pharos-store-sqlx` | Migration `0068_subtitle_styling` (sqlite + postgres): table `subtitle_styling(item_id, source_index, source_mtime, source_size, sidecar_mtime, sidecar_size, director_version, outcome, plan_json, styled_at)`, PK `(item_id, source_index)`. |
| `crates/pharos-cache` | `SubtitleKind::Styled { tier, level }`. The disk path letter `y` is suffixed with tier, level, `COMPILER_VERSION` and `DIRECTOR_VERSION`. |
| `crates/pharos-server` | `subtitle_styling.rs` (the job, modelled on `audio_analysis.rs`, plus an on-demand queue), the synthetic index in `services/watch.rs::tracks` (native detail only), the `stream.ass` styled branch with `?tier=&effects=` and `X-Pharos-Subtitle-Tier`, 404 for that index on the other routes, and the metrics. |
| `crates/pharos-ui` | `native/playback.rs`: `EffectsLevel`, `Tier`, `starting_tier(device, reduced_motion)`, `styled_track_url(track, tier, level)`, and the tier-drop state machine (pure, unit-tested). `native/appearance.rs`: the Effects select. `native/web.rs`: device class, the dropped-frame sampler, and persisting the dropped tier. |

## Data flow

1. The job picks an item: on-demand first, then backfill. It checks the gates: genre, an English text track, a changed signature.
2. **select**: `source::choose(item.probe, sidecars)` picks the source track (FR-016) and reads its script through the existing `ass_script` / WebVTT extraction (cached).
3. **signals**: one `BgPermit`, then `pool.subtitle_signals(path, audio_stream, windows)`, where the windows are the source lines' `[start, end]`. Gives `TrackSignals { lines, keyframe_cuts }`.
4. **direct**: `director::direct(&lines, &signals, &segments, tempo) -> EffectPlan`. Pure. One debug log per line and one info log per item.
5. Store the plan (`plan_json`) with the source signature and `DIRECTOR_VERSION`.
6. **compile** is lazy, at request time. `stream.ass?tier=&effects=` for the styled index loads the plan, compiles, caches by `SubtitleKind::Styled` and serves. Compiling is pure and costs milliseconds, so there is no need to compile all six variants up front.

## Director rules (phase 1, first match wins)

| # | Rule | Intent | Source of signal |
|---|---|---|---|
| 1 | Line is all in brackets or asterisks and names a sound (`[explosion]`, `*bang*`) | Sfx | text |
| 2 | Speaker tag `NARRATOR:`, or italic and over 4 s inside a Recap segment | Narration | text, segment |
| 3 | A bracket naming a device: `[phone]`, `(radio)`, `[TV]`, `(intercom)`, `[on phone]`, `(over radio)`, `(through the intercom)`. The audio alone never decides it (B661) | Phone | text |
| 4 | `(whispering)` tag, or loudness 8 LU or more below the median | Whisper | text, audio |
| 5 | Wholly in parentheses or italic, and loudness within ±4 LU | Thought | text, audio |
| 6 | Italic, and loudness above the median | OffScreen | text, audio |
| 7 | 1-3 words, ends in `!`, loudness 9 LU or more above the median, pitch 20% or more above the median | AttackCall | text, audio |
| 8 | Loudness 6 LU or more above the median, or a capitals ratio of 0.8 or more with `!` (the capitals rule is off on an all-caps track) | Shout, intensity = clamp((ΔLU − 6) / 3, 0, 2) + 1, and 3 when ΔLU ≥ 15 | text, audio |
| 9 | Inside an Intro or Outro segment with source `\k` timings | Plain plus karaoke | segment |
| 10 | Otherwise | Plain | default |

Flashback has no phase 1 rule (it needs scene mood, a phase 4 signal), except a source ASS style named like "flashback". Sarcasm has none.

## Tier budgets (enforced by `budget.rs`, asserted by V533)

| Tier | `\t` per event | `\move` | `\fad` | `\blur`/`\be` | `\k*` | Animated on screen | Events per source line | Event starts per second |
|---|---|---|---|---|---|---|---|---|
| Low | 0 | 0 | 0 | 0 | 0 | 0 | 1 | <= 6 |
| TV | <= 1 (`\t` or `\move`, not both) | <= 1 | yes | 0 | `\kf` only | <= 3 | <= 2 | <= 6 |
| Desktop | <= 4 | yes | yes | yes | all | <= 8 | <= 4 | <= 6 |

## Client tier choice

- Device class: `Tv` when the UA has `Android TV`, `GoogleTV`, `AFT`, `BRAVIA`, `SMART-TV` or `Tizen`, or the host is the Android shell (025). `Phone` when the pointer is coarse and the screen is narrow. Otherwise `Desktop`.
- Starting tier: reduced motion gives Low. Otherwise Tv and Phone give TV, and Desktop gives Desktop. That is then capped at the remembered dropped tier for the device.
- Drop rule: sample `getVideoPlaybackQuality()` every 1 s. Over budget means the dropped-frame delta is more than 5% of the presented-frame delta across a sliding 10 s window. Drop one tier, with a 30 s cooldown and Low as the floor. Remember the result, log it, and report it as `rendererMode`.

## Signals (ODD)

- Proof of styling: `sum by (outcome, stage) (increase(pharos_subtitle_styling_total[1d]))`.
- Effect mix: `sum by (intent, source) (increase(pharos_subtitle_effect_total[1d]))`.
- Serving: `sum by (outcome) (increase(pharos_subtitle_delivery_total{source="generated"}[1d]))`.
- Why a line shook: `{namespace="pharos"} |= "subtitle effect" | intent="Shout"`, at debug level per line. The info line per item carries the counts.
- Tier drops: `{namespace="pharos"} |= "rendererMode" |= "tier"`.

## Risks

- The keyframe proxy misses cuts in fixed-GOP encodes. That is acceptable for phase 1, because FR-011 only stops animation across a cut. ADR-0067 records the trade-off. `scdet` is the fallback if the reference episodes show misses.
- The front-centre channel is absent on stereo: the downmix lets music swell into Shout. Mitigation: the median baseline is computed per Intro, Outro and other segment, and the Shout threshold rises 3 LU inside Intro and Outro.
- The renderer thread changes the ASS path (JASSUB to pharos-subs). Output is plain ASS v4+ (design §8b), so nothing on the wire changes.
