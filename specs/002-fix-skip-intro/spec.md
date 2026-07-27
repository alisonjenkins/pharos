# Feature Specification: Skip Intro reaches the viewer

**Feature Branch**: `002-fix-skip-intro`
**Created**: 2026-07-27
**Status**: Draft
**Input**: "The skip intro button does not appear in the UI. Skip outro does appear to work in the Google TV app."

## Problem

A viewer watching a TV episode is offered **Skip Outro** but never **Skip Intro**,
on the same show, in the same session. The reported symptom is a missing button;
the measured cause is a missing *segment* — for the shows being watched, the
server holds no intro to offer.

Evidence gathered 2026-07-27 before writing this spec:

- Delivery is healthy. The client asks for both kinds
  (`GET /MediaSegments/{id}?includeSegmentTypes=Intro&…Outro&…`, Jellyfin Android
  TV 0.19.9) and gets `200`. Intro and Outro travel the same code path, in the
  same response shape, so a rendered Outro proves the path works for Intro too.
- Detection is *partially* healthy. Library-wide there are 6113 intro rows and
  6746 outro rows; 481 seasons have at least one intro, 506 at least one outro.
- The gap is per-show and one-directional: **145 seasons have outros and zero
  intros**, against 120 the other way. The episodes the viewer actually reported
  (Mushoku Tensei S03) are in that set — every episode carries an outro, none
  carries an intro.
- For that season the detector recorded its own reason: on the **same four
  episodes**, Outro emitted 3 of 4, while Intro returned `no_span` for all four —
  `0 matched / 0 agreeing / confidence 0.00`. Zero matches on every episode of a
  season whose opening is byte-identical across episodes is not the ordinary
  "this show has no intro" outcome the detector is designed to report calmly.

So this is a **recall** problem in intro detection, not a UI or wire problem, and
the recall failure looks systematic rather than show-specific.

## User Scenarios & Testing

### User Story 1 - Skip a recurring opening (Priority: P1)

A viewer starts an episode of a series whose episodes share an opening sequence.
When playback reaches that opening, the client offers Skip Intro; pressing it
jumps to the end of the opening and playback continues without a gap or a
re-buffer.

**Why this priority**: it is the entire reported defect.

**Acceptance scenarios**:

1. **Given** a season whose episodes share an opening, **When** the viewer plays
   any episode past the first, **Then** the client offers Skip Intro at the
   opening's start and stops offering it at the opening's end.
2. **Given** the same season, **When** the viewer plays it on a different client
   (Google TV app and the browser), **Then** both offer Skip Intro over the same
   time window.
3. **Given** an episode whose opening was found, **When** the viewer presses Skip
   Intro, **Then** playback resumes at the opening's end within the tolerance a
   viewer perceives as immediate, with no repeated dialogue and no missed scene.

### User Story 2 - Silence stays explainable (Priority: P1)

An operator asks why a given season offers no intro, and gets an answer that
distinguishes "this show genuinely has no shared opening" from "the opening was
there and the detector threw it away", without re-running anything by hand.

**Why this priority**: the constitution requires it (V75), and it is the only
reason the cause of this bug was identifiable at all. The fix must not be
verifiable only by eyeballing a TV.

**Acceptance scenarios**:

1. **Given** any analysed season, **When** an operator queries the recorded
   detection outcomes, **Then** each episode's verdict, its match evidence and
   the rejection reason are available for both intro and outro.
2. **Given** a season that produced no intro, **When** the operator compares it to
   the same season's outro run, **Then** the difference in inputs between the two
   is visible, not inferred.

### User Story 3 - Existing libraries recover without a full rescan (Priority: P2)

Once detection improves, seasons that were already analysed and came up empty are
re-examined, so a viewer's existing library gains the intros it should have had.

**Why this priority**: 145 seasons are already in the outro-only state. A fix that
only applies to newly added media leaves the reported symptom in place for the
shows that prompted the report.

**Acceptance scenarios**:

1. **Given** a season previously analysed with no intro found, **When** detection
   has been improved, **Then** that season is re-analysed without an operator
   deleting rows or forcing a rescan.
2. **Given** a season whose intro was already found correctly, **When**
   re-analysis runs, **Then** its existing segment is preserved or reproduced —
   the viewer never loses a working Skip Intro.

### Edge Cases

- A show with a **cold open**: the opening starts several minutes in, not at 0.
- A show whose opening **moves between episodes**, or is skipped entirely in some
  episodes (finales, specials, recap episodes).
- A **short-form** show where the opening is a large fraction of the runtime.
- A season with **very few episodes** — too few peers for cross-episode agreement.
- A show that **genuinely has no shared opening**; the correct outcome is no
  segment and no button, recorded as such.
- The opening's audio differs slightly between episodes (different mix, different
  language track, a dub with re-recorded vocals over the same music).
- An episode whose source file is unreadable or zero-length; it must not poison
  the season's result for its peers.
- An intro whose detected window would start at exactly 0, or would run past the
  point where the viewer already is.

## Requirements

### Functional Requirements

- **FR-001**: The system MUST offer a skip action for a recurring opening on any
  season where such an opening exists and is discoverable from the episodes
  themselves.
- **FR-002**: The system MUST apply the same discovery quality to openings as it
  already achieves for closings — a season whose closing is found and whose
  opening is equally repetitive MUST NOT silently yield only the closing.
- **FR-003**: The system MUST record, per season and per kind, whether an opening
  was found, and when it was not, the evidence that led to the rejection.
- **FR-004**: The system MUST distinguish "no shared opening exists" from "an
  opening was found and discarded" in that record.
- **FR-005**: The system MUST re-examine seasons whose previous analysis produced
  no opening, whenever the detection behaviour changes, without operator
  intervention.
- **FR-006**: The system MUST NOT discard or degrade an opening it has already
  found correctly when re-examining.
- **FR-007**: The system MUST report a found opening to every client that asks for
  segment data, in the form those clients already consume for closings.
- **FR-008**: A reported opening MUST bound the actual opening — pressing skip
  MUST NOT land the viewer inside the opening, nor past content that follows it.
- **FR-009**: Analysis MUST remain subordinate to live playback: discovering
  openings MUST NOT degrade anyone's viewing.
- **FR-010**: Analysis MUST tolerate individual unreadable episodes, completing
  for the rest of the season and recording the exclusion.

### Key Entities

- **Season** — the group of episodes an opening is discovered across; the unit of
  analysis and of the recorded verdict.
- **Episode** — an individual playable item; carries at most one opening and one
  closing marker.
- **Segment marker** — a labelled time range on an episode (opening, closing) with
  the confidence and the method that produced it.
- **Detection verdict** — the per-episode record of what the analysis concluded
  and why, including rejections.

## Success Criteria

### Measurable Outcomes

- **SC-001**: For seasons where a closing is discovered, an opening is discovered
  too in at least **80%** of cases where a human confirms a shared opening exists.
  Baseline today: of 987 seasons, 145 have a closing and no opening.
- **SC-002**: The reported outro-only count for the seasons named in this report
  (Mushoku Tensei S03, and spot-checked members of the 145) falls to zero, and
  each shows Skip Intro on the Google TV app and in the browser.
- **SC-003**: A skip lands within **1 second** of the opening's true end, judged
  against a human-marked timestamp on a sample of at least 10 episodes.
- **SC-004**: No season that has a working opening today loses it.
- **SC-005**: Every season that yields no opening has a queryable reason, with
  zero seasons reporting the "no evidence recorded" state.
- **SC-006**: Detection running does not increase playback start time or rebuffer
  rate for a viewer streaming at the same moment.

## Assumptions

- The reported client is the Jellyfin Android TV app (0.19.9); the browser client
  consumes the same segment data. No client-side change is in scope — if a client
  turns out to need something different for openings than for closings, that
  becomes a separate finding.
- "Shared opening" means a sequence repeated across episodes of the same season.
  A per-episode opening that repeats nowhere is out of scope.
- The existing outro behaviour is correct and is the quality bar; this feature
  does not revisit it beyond not breaking it.
- Re-examination may re-read episode audio, and is expected to cost background I/O
  on the media store; it stays behind the existing background-I/O gate.
- Movies are out of scope. This is about episodic content.
