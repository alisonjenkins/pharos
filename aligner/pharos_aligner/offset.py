"""Whole-song offset between synced lyric stamps and the audio.

Lyrics timed for another cut of a song (a re-recording, a radio edit) are
early or late by the same amount on every line. Each line is aligned only
inside its own stamped window, so a constant offset of a few seconds makes
every line match the singing of its neighbour. A few distinctive lines are
looked for in wide windows first; when they agree on a shift, every line is
moved by it before the per-line alignment.
"""

from __future__ import annotations

import statistics
from typing import Callable, NamedTuple

from pharos_aligner.plan import CAP_EXTRA_MS, CAP_MS_PER_WORD, Line
from pharos_aligner.text import words

# How far either side of its stamp a probe line is looked for: covers a
# re-recording a few seconds longer or shorter in its intro.
SEARCH_MS = 8_000
PROBES = 7
# Short lines ("oh", "yeah") match any vocal; three words pin a line down.
MIN_PROBE_WORDS = 3
MIN_AGREEING = 3
# Probes within this of their median count as agreeing on the shift.
AGREE_MS = 750
# Smaller shifts are within what the stamps and the per-line windows already
# absorb; moving lines for them would trade one small error for another.
NEGLIGIBLE_MS = 400

Decode = Callable[[int, int], bytes]
AlignWide = Callable[[bytes, list[str]], list[tuple[int, int]]]


class Estimate(NamedTuple):
    offset_ms: int
    # Each probe's own shift, for the log; None where it found nothing.
    probes: list[int | None]
    reason: str


def probe_indices(lines: list[Line]) -> list[int]:
    """Up to PROBES lines spread over the song, long enough and unrepeated."""
    keyed = [tuple(w.token for w in words(line.text)) for line in lines]
    counts: dict[tuple[str, ...], int] = {}
    for k in keyed:
        counts[k] = counts.get(k, 0) + 1
    candidates = [i for i, k in enumerate(keyed) if len(k) >= MIN_PROBE_WORDS and counts[k] == 1]
    if len(candidates) <= PROBES:
        return candidates
    step = len(candidates) / PROBES
    return [candidates[int(n * step)] for n in range(PROBES)]


def estimate(lines: list[Line], decode: Decode, align_wide: AlignWide) -> Estimate:
    """Find the shift that moves the stamped lines onto the singing."""
    probes: list[int | None] = []
    for i in probe_indices(lines):
        line = lines[i]
        tokens = [w.token for w in words(line.text)]
        sung_ms = min(line.end_ms - line.start_ms, len(tokens) * CAP_MS_PER_WORD + CAP_EXTRA_MS)
        start = max(0, line.start_ms - SEARCH_MS)
        end = line.start_ms + sung_ms + SEARCH_MS
        pcm = decode(start, end)
        spans = align_wide(pcm, tokens) if pcm else []
        if not spans or spans[0] == (0, 0):
            probes.append(None)
            continue
        probes.append(start + spans[0][0] - line.start_ms)

    found = [p for p in probes if p is not None]
    if len(found) < MIN_AGREEING:
        return Estimate(0, probes, f"only {len(found)} probes found their line")
    middle = statistics.median(found)
    agreeing = [p for p in found if abs(p - middle) <= AGREE_MS]
    if len(agreeing) < max(MIN_AGREEING, len(found) // 2 + 1):
        return Estimate(0, probes, f"{len(agreeing)} of {len(found)} probes agree")
    offset = int(statistics.median(agreeing))
    if abs(offset) < NEGLIGIBLE_MS:
        return Estimate(0, probes, f"shift {offset} ms is negligible")
    return Estimate(offset, probes, f"{len(agreeing)} of {len(found)} probes agree")


def shift(lines: list[Line], offset_ms: int) -> list[Line]:
    """Move every line by `offset_ms`, keeping times at or after zero."""
    if offset_ms == 0:
        return lines
    return [
        Line(max(0, line.start_ms + offset_ms), max(0, line.end_ms + offset_ms), line.text)
        for line in lines
    ]
