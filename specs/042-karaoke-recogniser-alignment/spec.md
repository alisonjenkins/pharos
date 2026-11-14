# Feature Specification: karaoke timing from the singing

**Feature Branch**: `claude/project-thread-sylrb6` (spec), later slices on their own branches
**Created**: 2026-11-14T06:30:00Z
**Status**: Draft
**Decision records**: ADR-0124 (pipeline), ADR-0125 (vocal separation and recogniser models)
**Bugs**: B1402 (word times follow wrong line stamps), B1403 (spare). **Verification**: V1372, V1373 (spare)
**Input**: Karaoke word times must follow the singing, not the synced-lyrics line stamps, which are often seconds wrong around instrumental breaks.

---

## Problem statement

Karaoke word times today come from the aligner (spec 039, ADR-0116).
It times each lyric line only inside a short window around that line's synced-lyrics stamp.

- Downloaded synced lyrics are often timed by ear and can be several seconds wrong, worst around instrumental breaks.
- When a stamp is wrong, the line's window covers the wrong audio. The aligner still places every word inside it, so the line lights up early or late with believable spacing (B1402).
- A whole-song offset (the existing probe estimate) cannot fix this: the error changes from line to line.

Measured on 2026-11-13 on a song with lyrics mistimed around an instrumental break (a 4:49 track, 34 sung lines):

- One line was stamped 2.8 s before it is sung. The next line was stamped where that line is sung, about 28 s before its own singing.
- From about two thirds through, the highlight ran 4-5 s ahead of the singing.
- Per-line scores do not flag it: wrong lines scored 0.02-0.13, right lines 0.09-0.55.

Approaches tried on that song plus 8 well-timed songs (desktop prototype, 2026-11-13):

| Approach | Result |
|---|---|
| Re-search low-scoring runs between confident lines | Fixed the first break; moved a correctly stamped line in another song 8 s early (ear check said the stamp was right) |
| One forced alignment over the whole song, stamps ignored | Long gaps opened inside lines; repeated choruses slid 16-46 s |
| Whole song + gap penalty inside a line + stamp band | Gaps fixed; repeated choruses still landed anywhere in a ~30 s range |
| Same on separated vocals | More confident, no better placement |
| Separated vocals + speech recogniser anchors each line + letter-level timing inside the anchor | 97 of 110 lines anchored on 4 songs; both ear-checked points held; 2 well-timed songs unchanged |

The letter model alone cannot tell which line is sung where in a dense mix. A recogniser can.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Words light up when they are sung (Priority: P1)

As a listener, each word in the karaoke view lights up when it is sung, even where the downloaded lyrics are mistimed.

**Why this priority**: it is the bug the owner reported, and the reason karaoke exists.

**Independent Test**: play the mistimed-lyrics song; the line after the first instrumental break lights up within 0.3 s of its first sung word.

**Acceptance Scenarios**:

1. **Given** a song whose synced lyrics stamp a line 2.8 s early, **When** it is aligned, **Then** that line's first word starts within 0.3 s of the singing.
2. **Given** a line stamped about 28 s before its singing, **When** it is aligned, **Then** it is placed at its singing, not at its stamp.
3. **Given** a song whose stamps are already right, **When** it is aligned, **Then** no line moves by more than 2 s from where today's aligner puts it, unless an ear check shows today's time was wrong.

### User Story 2 - Repeated lines go to the right repeat (Priority: P1)

As a listener, a chorus that repeats lights up on the repeat being sung, not an earlier or later copy.

**Why this priority**: repeated choruses are where whole-song alignment fails without a recogniser.

**Independent Test**: on songs with repeated choruses, the ear-checked repeat lines land within 1 s of the singing.

**Acceptance Scenarios**:

1. **Given** a line repeated four times in the lyrics, **When** aligned, **Then** each copy is placed in order on a different sung occurrence.
2. **Given** a line the recogniser mishears, **When** aligned, **Then** it is placed between its anchored neighbours, never past them.

### User Story 3 - The first lines still arrive quickly (Priority: P2)

As a listener opening karaoke for a song not yet aligned, I still see words streamed line by line during the first play (ADR-0121).

**Independent Test**: on a cold song, the first timed line arrives before that line is sung in at least 9 of 10 songs on the GPU path.

**Acceptance Scenarios**:

1. **Given** a song not yet aligned, **When** karaoke opens, **Then** lines arrive one by one in order as today, with the same answer shape.

### User Story 4 - The operator can see how each line was placed (Priority: P2)

As the operator, I can tell from logs and metrics, per song and in aggregate, how many lines were anchored by the recogniser, refined, or fell back, and why.

**Independent Test**: one query returns the share of lines placed by each method over the last day.

### Edge Cases

- Instrumental tracks or songs where the recogniser hears nothing: every line falls back; the result must be no worse than today's.
- Outro vocals or ad-libs after the last lyric line: they must not pull the last lines (seen: +22 to +42 s).
- Recogniser hallucinations in gaps ("thank you", captions, other languages): ignored unless they match lyric words in order.
- Short repeated lines ("oh", a two-word hook) that the recogniser mishears.
- Lyrics in a language or script the letter model cannot spell: placement falls back to today's behaviour for those lines.
- The GPU runs out of memory or is busy with another workload: the song falls back to CPU or to today's pipeline, and says which.
- A very long track (over the 500-line or duration limits): rejected or degraded as today, not crashed.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The aligner MUST place each lyric line from what is sung in the audio. Synced-lyrics stamps may only break ties and give the fallback position.
- **FR-002**: The aligner MUST separate the vocals from the music before recognition.
- **FR-003**: The aligner MUST recognise the sung words with per-word times, and match them to the lyric words in lyric order. A line counts as anchored when at least half of its words (minimum 2) match.
- **FR-004**: Inside each anchored line, word starts MUST be refined letter by letter against the audio, within a bounded margin of the anchor (prototype: ±1.5 s).
- **FR-005**: An unanchored line MUST stay between its anchored neighbours. With no usable audio evidence it MUST fall back to its stamp moved by the nearest anchor's offset.
- **FR-006**: Lines MUST come out in lyric order with non-decreasing word starts (V1372).
- **FR-007**: Whatever the alignment output looks like today (`POST /align` contract, streamed line events per ADR-0121, `score`) MUST stay compatible; any new per-line field is additive. `aligner_version` MUST rise so the server re-aligns cached songs when the operator chooses.
- **FR-008**: Each song MUST log and count how each line was placed (anchored / refined / between anchors / fallback), the recogniser's anchor share, and the reason for every fallback, carrying the offending value (constitution III).
- **FR-009**: The pipeline MUST run without Python, on the GPU when one is usable and on the CPU otherwise, with the same answers on both within 20 ms per word.
- **FR-010**: Models MUST load in stages and release memory between stages, so GPU memory held for one song stays under 6 GiB, and MUST unload when idle as today.
- **FR-011**: When a stage fails (out of memory, model error, timeout), the song MUST fall back to today's per-line alignment rather than fail, and record why.
- **FR-012**: No song, artist, album or file details of the owner's library may appear in tests, fixtures, specs or logs pushed to the repository.

### Key Entities

- **Transcript word**: a recognised word with start and end time and a confidence.
- **Anchor**: a lyric line matched to a run of transcript words, with its matched span and match share.
- **Placement**: per line, how it was placed (anchored, refined, between anchors, fallback) and why.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On the mistimed-lyrics song, every ear-checked line starts within 0.3 s of the singing (today: up to 28 s off).
- **SC-002**: On a regression set of at least 20 songs, at least 85% of sung lines are anchored, and every ear-checked spot is within 1 s.
- **SC-003**: On songs whose stamps are already right, no line moves more than 2 s from today's times unless an ear check confirms the move.
- **SC-004**: GPU time per 5-minute song stays under 45 s and GPU memory held for one song under 6 GiB; host RAM rises by no more than 1 GiB over today.
- **SC-005**: Streaming still delivers the first line before it is sung in at least 9 of 10 cold songs on the GPU.
- **SC-006**: After deploying, one query shows the share of lines placed by each method and the fallback reasons, and its output is reported.

## Assumptions

- The recogniser is Whisper large-v3-turbo and the separator is htdemucs (ADR-0125). The prototype found large-v3 no better at twice the memory.
- Models run in f32 on the GPU. fp16 gave NaNs and word drift for the letter model before (spec 039).
- Alignment stays one song at a time and karaoke keeps priority on the GPU.
- Songs already aligned keep their stored words until the operator re-aligns them (the server checks `aligner_version`); re-aligning on the live server needs the owner's OK.
- The regression set and ear checks use the owner's library privately; only made-up text and synthetic audio go into the repository.
