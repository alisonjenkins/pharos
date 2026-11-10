# ADR-0117: Speaker colours come from the script's speaker names

- **Status:** Proposed
- **Date:** 2026-11-10T19:50:00Z
- **Deciders:** Alison Jenkins

## Context

Every dialogue line of the styled track is drawn white, so a viewer cannot
tell speakers apart by colour. Alison asked for speaker colours as an opt-in
(spec 028 T028-31). Most fansub ASS scripts name who speaks each line in the
event's Name (actor) field. SRT carries no speaker; WebVTT can name one in a
voice span (`<v Ann>`), which the source reader does not read yet. Voice detection
from the audio would cover those too, but nothing in the project detects
voices yet.

## Decision

1. The source reader keeps the Name field as `SourceLine.speaker`.
2. The director gives palette slots to the six speakers (`MAX_SPEAKERS`)
   with the most dialogue lines, each with at least three
   (`MIN_SPEAKER_LINES`): slot 0 to the most lines, ties to whoever spoke
   first. Names are folded for case and spacing. Typeset lines, labels that
   are not a person (sign, note, OP, ED, song, lyrics, karaoke, title) and
   names of several people (`/`, `&`, `,`, `+`, `and`) get no slot. The slot
   is stored in the plan as `LineEffect.speaker`; DIRECTOR_VERSION 7.
3. The compiler draws a plain line with a slot in `SpeakerN`, the dialogue
   style in that slot's light colour, only when the theme carries speaker
   styles (`Theme::with_speakers`). Lines with an effect keep the effect.
4. The viewer turns it on with the Appearance setting "Speaker colours"
   (off by default, saved on the device). The player then asks for
   `stream.ass?speakers=1`, cached under its own name (suffix `.s`).

## Consequences

- Tracks without speaker names, every SRT source and, until the reader
  takes voice spans, every WebVTT source stay white.
- DIRECTOR_VERSION 7 re-directs every item once.
- Off is byte-for-byte the track without the feature, and its cache name
  gains no `.s` suffix. The director bump still renames every cached script
  once (`d7`), so each is compiled again on its first request after the
  re-direct.
- The thresholds are guesses. The per-item log line's `speakers` field
  counts the speakers given a slot, which is what they set. It is not how
  many colours a viewer sees: it is counted with the setting off too, and a
  slotted speaker whose lines all carry effects shows no colour.
- Changing the slot rules means another director bump and a re-direct of
  every item.

## Alternatives considered

- Voice detection (speaker diarisation) in the Rust aligner: covers sources
  without names, but needs a model and a pass over the audio of every item.
  Can come later and fill the same `LineEffect.speaker` slot.
- Colouring every named speaker: walk-ons with one or two lines would use
  up the colours the main cast needs.
