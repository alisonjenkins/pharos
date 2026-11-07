# Feature specification: Pharos-styled animated subtitles, directed on the server

**Feature Branch**: `docs/anime-subtitle-styling-spec`
**Created**: 2026-11-06T00:30:00Z
**Status**: Draft
**Scope**: Phases 0 and 1 of the project design "AI-generated animated anime subtitles" (`docs/design/animated-anime-subtitles.md` on branch `claude/project-thread-62gk21`; §3 effect plan and library, §8a device tiers, §10 phases), which Alison approved. For a video with an English text subtitle track, the server works out ahead of time how each line should look, from signals it already has or can read in the same pass. It then writes that as plain ASS v4+ and serves it as an extra track, "English (Pharos styled)". No machine learning is used in these phases. Clients only draw.
**Decision record**: ADR-0067. **Backlog**: B600 (phase 0), B601-B606. **Verification**: V530, V531 (phase 0), V532-V539.
**Depends on**: spec 026 (`pharos-subs`, its ASS parser on main; its renderer will draw this output once slice 3 lands), ADR-0057 (JASSUB draws it until then), 024's `audio_analysis` job pattern, T86 media segments.

---

## Problem statement

Anime fansubs set lines in a different style for each kind of speech: a shout pops, a whisper shrinks, an inner voice is set in another face, a phone call reads like a phone. Official English tracks, and every SRT track, are flat white text. Pharos already has most of what it needs to tell those lines apart: the text, its timing and punctuation, the audio under it, the file's chapters, and the opening and ending it detects (T86). What it lacks is a step that turns those signals into styling, a format to carry the result, and a way to serve that result cheaply enough for the weakest TV.

## Hard requirement: very optimised, runs well on weak devices

Alison's standing requirement. Every bit of analysis runs on the server, ahead of time, in the background. A client only downloads an ASS file and draws it with the renderer it already has. The reference weak device is the bedroom TV, a TCL G08 Google TV (armeabi-v7a, Android WebView), as in spec 026.

| Budget | Value | How it is measured |
|---|---|---|
| Client cost of choosing the styled track | One ASS download, gzip <= 1.5x the source track's ASS. No extra requests except pack fonts the file names, each <= 100 KB | e2e, network log |
| Low tier | Static styling only: no `\t`, `\move`, `\fad`, `\fade`, `\k*`, `\blur`, `\be`, and one event per source line | Property test (V533) |
| TV tier | At most one `\t` or `\move` per event, `\fad` allowed, `\bord` but no `\blur`/`\be`, `\kf` karaoke, at most 3 animated events on screen at any instant, at most 2 events per source line | Property test (V533) |
| All tiers | At most 6 events start per second, averaged over any 10 s | Property test (V533) |
| Desktop tier | Full effect set, at most 8 animated events on screen at any instant, at most 4 events per source line | Property test (V533) |
| Server cost per episode (24 min) | One read of the file. Director plus compile <= 5 s CPU at background priority, and yields to playback (`bg_io`) | `pharos_subtitle_styling_seconds`, log |
| First styled frame after picking the track (warm) | <= 300 ms, the same as spec 026 | e2e |

## User story 1 - A shout pops and a whisper shrinks (Priority: P1)

A viewer watching an anime episode with a plain English track picks "English (Pharos styled)". Ordinary dialogue looks like the plain track. A shouted line pops in larger and bolder. A whispered line is smaller and softer. A line in parentheses or italics, an inner voice, is set in the handwritten face and fades. A phone or radio line is set in the typewriter face. Sound effects in brackets ("[explosion]") appear in the display face near the top, away from the dialogue.

**Independent test**: a fixture episode with a known SRT track and a synthetic audio track (loud and quiet tone windows under chosen lines). After the background job runs, `stream.ass?tier=desktop` gives each fixture line its expected effect, and `pharos_subtitle_effect_total{intent}` counts them.

**Acceptance scenarios**:

1. **Given** an item with an English text subtitle track that the job has styled, **When** the native detail is read, **Then** an extra subtitle track titled "English (Pharos styled)" with codec `ass` is listed after the sidecars and plays through the ASS renderer.
2. **Given** a line whose loudness is at least 6 dB above the track's median line loudness, or that is in capitals with an exclamation mark, **Then** its intent is Shout, with intensity rising with the loudness delta.
3. **Given** a line at least 8 dB below the median line loudness, or tagged `(whispering)`, **Then** its intent is Whisper.
4. **Given** a line with nothing to go on, **Then** its intent is Plain and it draws exactly as the base theme style does. The director never guesses when it has no signal.

## User story 2 - Effects suit the device and the viewer (Priority: P1)

A viewer on the TV gets smooth pops and fades with no blur. A viewer on a desktop gets the full set. A viewer who turned on reduced motion gets static styling. A viewer who wants calmer subtitles picks "Effects: Subtle" (the default) or "Off" in Settings → Appearance. If the device cannot keep up, the player drops a tier on its own.

**Independent test**: Dioxus e2e in Chromium and Firefox. Settings → Appearance shows Effects Off / Subtle / Full with Subtle selected. With `prefers-reduced-motion: reduce` emulated, the styled track is fetched at `tier=low`. With a fake render-time report over budget, the player re-fetches one tier lower and sends a `rendererMode` event with the reason.

**Acceptance scenarios**:

1. **Given** a desktop browser, **Then** the styled track is fetched at `tier=desktop`. **Given** the Android TV app, a TV user agent or a coarse pointer on a small screen, **Then** at `tier=tv`.
2. **Given** `prefers-reduced-motion: reduce`, **Then** `tier=low` whatever the device class.
3. **Given** Effects is Subtle, **Then** every intensity is capped at 1. **Given** Full, intensities are kept. **Given** Off, the styled track is not offered.
4. **Given** the ASS renderer reports it is not keeping up, or the video's dropped frames rise by more than 5% over 10 s while the styled track shows, **Then** the player switches to the next lower tier without restarting playback, at most once per 30 s, and reports the drop with its measured values.

## User story 3 - Openings and endings get karaoke (Priority: P2)

During a detected opening or ending (chapter title or T86 segment), sung lines whose source already carries syllable timing (`\k`, `\kf`, `\ko` in an ASS track) get Pharos karaoke. The fill sweeps across on TV and desktop. On desktop each syllable also lifts on the beat (Bounce) or leaves a glow trail (Ember), pulsing at the item's measured tempo. The low tier shows those lines in the song style with no karaoke. Lines without syllable timing get the song style only. Phase 1 does not invent syllable times. Timing from the forced aligner is a later source (design §4).

**Acceptance scenarios**:

1. **Given** a line inside an Intro or Outro segment whose source carries `\k` timings, **Then** its effect is Sweep: `\kf` with the source's durations on TV and desktop. On desktop it is Bounce, a single `\t` lift per syllable on its beat, or Ember at Full intensity.
2. **Given** a sung line with no syllable timing, or a line no segment and no chapter covers, **Then** no karaoke is applied.

## User story 4 - The operator can see what the director did (Priority: P2)

An operator can tell, from Prometheus and Loki alone, how many items were styled, why any were skipped, and why a given line got a given effect.

**Acceptance scenarios**:

1. **Given** the job runs over an item, **Then** `pharos_subtitle_styling_total{outcome,stage}` counts it once, whatever the outcome (styled, nothing_to_style, no_english_track, undecodable, unscanned, missing, error), with the stage the outcome was reached at.
2. **Given** a line is given an effect, **Then** `pharos_subtitle_effect_total{intent,source}` counts it. One log line per item lists the intent counts. At debug level, one log line per line decision carries its inputs (loudness delta, pitch delta, caps ratio, punctuation, tags, segment) and the rule that fired.

## Requirements

### Effect plan and compiler

- **FR-001** An effect plan type, internal to the server (`pharos-core` or a new `pharos-styling` crate; ADR-0067 decides which). It holds one `LineEffect` per source line: `intent` (Plain, Shout, Whisper, Thought, Narration, OffScreen, Phone, AttackCall, Flashback, Sarcasm, Sfx; design §3. The phase 1 director never picks Sarcasm, which needs a model, but the type and compiler carry it), `intensity` 0-3, `placement` (Default, Top, Free(x, y) in script units), `karaoke` (None, Sweep, Bounce, Ember), the source's syllable timings when it has them, and a `reason` string naming the rule and its inputs. It also holds the source track identity and the director version.
- **FR-002** A pure, deterministic compiler: `(plan, source events, theme, tier, effects level) -> ASS v4+ text`. The same inputs always give the same bytes. It does no I/O, allocates no randomness, and seeds any jitter from the line's index.
- **FR-003** The effect library is Pharos's own (design §3). Each effect is defined per tier, so the low tier still differs from the plain style in face, size and colour:
  - **Impact** (Shout): 115% scale pop to 100% over 120 ms, a slight tilt, a brief glow.
  - **Tremor** (Shout at intensity 3, a scream): a few pixels of jitter, damped over the line.
  - **Hush** (Whisper): smaller, 70% opacity, a soft blur, a slow fade.
  - **Inner voice** (Thought): italic in the handwritten face with a soft halo, and the source's brackets dropped.
  - **Echo** (AttackCall): two ghost copies that trail and fade. At Full intensity it becomes a big title-card variant.
  - **Wire** (Phone, radio): the dialogue face and size in a cool silver-blue, with a soft dark outline and a small shadow. Desktop fades in once from a paler tint; TV fades; Low is the style alone (B662).
  - **Memory** (Flashback): desaturated, a warm tint, letter-spaced.
  - **Caption** (Narration): the condensed face in a top box.
  - **Aside** (OffScreen): the dialogue face, dimmer and italic.
  - **Burst** (Sfx): the display face, placed away from dialogue.
  - Karaoke **Sweep**, **Bounce** and **Ember** (US3).

  At the low tier, blur, glow and jitter become a static border or colour. At the TV tier they become a single `\t` or `\move`. No style, effect or tag sequence is copied from any fansub group's work. Speaker colour (design §3) needs diarisation and comes in phase 2.
- **FR-004** The output uses only the tag set both renderers support (JASSUB today, `pharos-subs` per spec 026 FR-004 once slice 3 lands): `\t \move \fad \k \kf \ko \frx \fry \frz \fscx \fscy \blur \be \bord \shad \clip \p \org \pos \an \c \alpha \fn \fs \b \i`. The property test (V533) checks every compiled file twice. `pharos_subs::decode` must read it with no bad lines. A tag scanner of its own then checks the allow-list and the tier budget. `decode` cannot check the tags itself: on main it reports every tag outside the plain tier (all animation tags) as unsupported, and that stays true until spec 026 slice 3.
- **FR-005** A source ASS track's events that carry their own typesetting (`\pos`, `\move`, `\an` other than 2, `\clip`, `\p`, a non-default style with a different alignment) are passed through unchanged. Only dialogue-style events are restyled, so a fansub's signs survive.

### Device tiers and caching

- **FR-006** Three tiers, Low, TV and Desktop, with the budgets in the table above. Effects level Subtle caps intensity at 1 before compiling; Full keeps it.
- **FR-007** Compiled output is cached per (item, source track, source signature, director version, compiler version, theme, tier, effects level), in the subtitle cache (a new `SubtitleKind`). The plan is stored once per (item, source track, source signature, director version), in the store (migration 0068, sqlite and postgres). It is not kept in the LRU subtitle cache, because losing a plan would mean reading the whole file again. The source signature is the video file's `(mtime, size)`, plus the sidecar's own `(mtime, size)` when the source track is a sidecar, so an edited `.srt` is restyled. A changed file, or a bumped version, makes the old plan and compiled files unused, the way `TV_MATCH_VERSION` and `ANALYSIS_VERSION` work.
- **FR-008** The styled track is served at the existing `stream.ass` route under its synthetic index, with `?tier=low|tv|desktop&effects=subtle|full`. A missing or unknown tier is served as `tv`, and the response says which tier it carries (`X-Pharos-Subtitle-Tier`).

### Director

- **FR-009** A rule-based director, `(source events, line signals, segments, chapters, track tempo) -> plan`. Rules are ordered and the first match wins. Each rule names itself in `reason`. The rules use only:
  - Text: capitals ratio, `!`/`?` runs, ellipses, a leading dash, brackets and parentheses, italics (`<i>` or `{\i1}`), speaker and sound tags (`[on phone]`, `(whispering)`, `NARRATOR:`, `[explosion]`, `*bang*`), word count.
  - Timing: duration, gap to the neighbouring lines, overlap with another line.
  - Audio per line: loudness (EBU R128 short-term LUFS, design §4), peak, median pitch (F0 in Hz, voiced frames only), and the share of energy above 3.4 kHz, each compared with the track's median.
  - Scene cuts: video keyframe times that break the stream's regular keyframe interval, read from packet flags without decoding. Encoders put a keyframe at a cut, so this is a demux-only proxy for cuts. It misses cuts in a fixed-GOP encode. ADR-0067 weighs it against decoding with `scdet` (design §4), which costs a full video decode per episode.
  - Chapters and T86 media segments (Intro, Outro, Recap, Preview), and the `audio_analysis` tempo where the item has one (it sets the karaoke bounce rate).
- **FR-010** A line with no signal, or with conflicting signals under a rule's confidence floor, is Plain. On a typical episode at least 70% of lines are Plain (SC-002). Effects are an accent, not the default.
- **FR-011** No effect spans a scene cut. A line that crosses one keeps its intent but drops animation that would run across the cut.

### Signals pass

- **FR-012** One new libav worker op, `SubtitleSignals { input, audio_stream, windows }`, reads the file once. The audio stream is the one the subtitles are timed to: the default or original-language track (usually Japanese), never an English dub. When the layout has a front-centre channel (5.1, 7.1) only that channel is used, because dialogue sits there and music and effects mostly do not. Otherwise the channels are downmixed. It decodes that stream to mono 11 025 Hz, computes per-window loudness, peak, pitch and high-band share, and collects video keyframe timestamps from packet flags. It returns all of them inline. It is a heavy op (`is_heavy_op`), with a timeout scaled to duration like `Waveform`.
- **FR-013** Pitch is a time-domain YIN estimate over 40 ms frames, 70-500 Hz, voiced only above a clarity floor. It is computed in Rust inside the worker, and needs no new C dependency.

### Background job

- **FR-014** A `subtitle_styling` background job modelled on `audio_analysis`: it starts after a warm-up, runs on the background leader only, handles one item at a time, and takes a `BgPermit` around the signals op, so it yields to playback. It also starts no item while `pharos_transcode_pending_jobs` is above zero (design §8a). It retries a crashed or timed-out worker, or a source script it failed to extract, next pass, and stores a version-only row for an undecodable file so the file is not tried again until it changes.
- **FR-015** Which items: videos with an English (or untagged, single) text subtitle track, in a library or item whose genres include Anime or Animation. The job takes the item a viewer just opened first, through an on-demand queue that sits ahead of the backfill. [NEEDS CLARIFICATION: is the genre gate the right scope for phase 1, or should every video with an English text track be styled?]
- **FR-016** Source track choice: the full English dialogue track (not forced). An SDH track is preferred when one exists, because its sound and speaker tags feed the director. An ASS source is preferred over SRT for the same language, because its italics and positions are kept.

### Serving and the client

- **FR-017** The styled track is listed in the native detail only in phase 1, after the sidecars at `sidecar_base + sidecar_count`, titled "English (Pharos styled)", language `eng`, codec `ass`, never default. It is listed only while a current plan exists. `stream.ass` serves that index, and every other route that resolves a subtitle index (`stream.vtt`, `stream.js`, fonts, burn) answers 404 for it instead of treating it as a missing sidecar. The generated track's plain WebVTT fallback (the native `Track.url`) is its source track's `stream.vtt`, so a renderer fallback still draws text, while `stream.vtt` for the styled index itself is 404; the script URL travels in the new `Track.styledUrl` (capnp `@9`). Jellyfin `MediaStreams` comes in a later slice. That slice has to keep B71's contiguous index rules, and Jellyfin clients send no `?tier=`, so they would get `tv`.
- **FR-018** Settings → Appearance gets "Effects: Off / Subtle / Full", default Subtle, stored per device like the subtitle size (V455). The player picks the starting tier from device class and `prefers-reduced-motion` (US2). It follows a change to either while playing.
- **FR-019** The player watches the video's dropped frames (`getVideoPlaybackQuality`) and, once the renderer exposes them, the ASS renderer's frame times (`pharos-subs`' `stats()`, spec 026). JASSUB 2.5.23's public API has no per-frame render time: its frame counters are internal. While JASSUB draws, the rule therefore uses dropped frames alone. It drops one tier when either is over budget for 10 s, with at most one drop per 30 s and never below Low. It remembers the dropped tier for the device (localStorage, like V455), so the next session starts there. The check runs locally whether or not the viewer opted in to telemetry. Every drop is logged and sent as a `rendererMode` event with `renderer`, `from`, `to`, `reason` and the measured values.
- **FR-020** The styled ASS names its faces from the phase 0 font pack (V530): Bangers, Anton, Patrick Hand and Courier Prime, plus Liberation Sans for dialogue.

### Signals

- **FR-021** `pharos_subtitle_styling_total{outcome,stage}`, with stage being `select`, `read`, `signals`, `direct` or `store`, the stage an item ended in (`styled` ends in `store`). Compiling happens per request and is counted by the delivery metric below. Later phases add `asr`, `align`, `diarize`, `ocr` and `track` (design §9). Also `pharos_subtitle_styling_seconds{stage}`, `pharos_subtitle_styling_pending`, and `pharos_subtitle_effect_total{intent,source}`, where source is `rule` or `model` (always `rule` in phase 1). The signal that fired is in the log line, never a label. The design's proof query is `sum by (intent, source) (increase(pharos_subtitle_effect_total[1d]))`. Labels come from `label()` methods and are asserted distinct. The styled track's deliveries are counted by B600's `pharos_subtitle_delivery_total` with `source="generated"`.

## Key entities

- **EffectPlan**: source track (item, stream index, codec, file signature), director version, `Vec<LineEffect>`.
- **LineEffect**: source event index, intent, intensity, placement, karaoke, reason.
- **LineSignals**: loudness dBFS, peak, pitch Hz, high-band share, keyframe-cut flags for the line's span.
- **Theme**: named styling palette (faces per intent, colours, outline widths). Phase 1 ships one, "pharos".
- **Tier**: Low, TV, Desktop. **EffectsLevel**: Off, Subtle, Full.

## Success criteria

- **SC-001** (manual gate: needs the G08 between 08:00 and 23:00 local, as spec 026 FR-015) On the TCL G08, the styled track at `tier=tv` drops no more video frames than the plain track over the same 5-minute fixture (V536).
- **SC-002** (manual gate: Alison labels the samples, or names someone who will; the tooling prints the 150 lines with their intents for that) On the three reference episodes (one shonen fight, one slice-of-life, one with phone calls), at least 70% of lines are Plain, and a hand-labelled sample of 50 non-Plain lines per episode agrees with the director's intent in at least 80% of cases (V537).
- **SC-003** The server styles an episode with <= 5 s CPU and exactly one read of the file (V538).
- **SC-004** Every compiled file parses with `pharos_subs::decode` with no unsupported tag, and stays inside its tier's budget, for 512 property-test cases (V533).

## Edge cases

- A track whose lines are all capitals (some SDH tracks): the capitals rule is ignored for that track when more than 50% of lines are capitals.
- No audio stream, or an undecodable one: audio rules are skipped and text rules still apply. The outcome is still `styled`, and the log says `audio=none`.
- Several English tracks: FR-016 picks one. The others are not styled.
- The file changes after styling: the signature no longer matches, the styled track is no longer listed, and the job restyles the item next pass.
- A viewer seeks while the tier changes: the new script loads at the current time, and the old renderer is destroyed first (as the styled renderer is today on a track change).
- Overlapping lines (two speakers): each line is styled on its own. Placement keeps them apart through `Layer` and margins, never by moving dialogue to the top.

## Delivery slices

| Slice | Backlog | What ships | Gate |
|---|---|---|---|
| 0 | B600 | Phase 0: font pack (V530), delivery metric and renderer fallback report (V531). B572 already shipped sidecar ASS and embedded fonts. | V530, V531 |
| 1 | B601 | Effect plan type, theme, tiered compiler and the ten effects, with the property test. No serving yet. | V532, V533 |
| 2 | B602 | `SubtitleSignals` worker op (loudness, pitch, high band, keyframe cuts). | V534 |
| 3 | B603 | Director rules and the `subtitle_styling` job, plan storage and metrics. | V535, V537 |
| 4 | B604 | Synthetic track listed in the native detail and served at `stream.ass?tier=&effects=`, compiled-output cache. | V532 |
| 5 | B605 | UI: Effects setting, starting tier from device class and reduced motion. | V539 |
| 6 | B606 | UI: runtime tier drop on render time and dropped frames. | V536 |

Verification ids:

- V532: same inputs, same bytes.
- V533: parses with no unsupported tag and stays within the tier budget.
- V534: one file read per item and signals within tolerance on the synthetic fixture.
- V535: every job outcome counted with its stage; failure paths record the same fields.
- V536: G08 dropped frames, and the tier drop works.
- V537: Plain share and intent agreement on the reference episodes.
- V538: server CPU per episode.
- V539: Effects setting and starting tier per device class and reduced motion.

## Out of scope (later phases of the design)

Phases 2-4 of the design: ML voice and emotion models, vocal separation, speaker diarisation and speaker colour, sign detection, OCR and tracking, the learned director, generated karaoke timing, the image-track fallback for clients that cannot run an ASS renderer (spec 026 slice 5), any language other than English, user-authored themes, per-line overrides, and styling image subtitles.

## Open questions

1. FR-015: is the item scope the genre gate, or every video with an English text track?
2. Scene cuts: the keyframe proxy or `scdet` (FR-009). ADR-0067 decides.
