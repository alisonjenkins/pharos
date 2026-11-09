"""Whole-song offset estimate, with fake audio whose singing is shifted."""

from __future__ import annotations

import json

from pharos_aligner.offset import NEGLIGIBLE_MS, PROBES, estimate, probe_indices, shift
from pharos_aligner.plan import Line
from pharos_aligner.text import words

SONG = [
    Line(37_420, 41_000, "Breathe and I'll carry you away"),
    Line(41_000, 45_000, "Into the velvet sky"),
    Line(45_000, 49_000, "And we'll drift on the wind"),
    Line(49_000, 52_000, "Oh oh"),
    Line(52_000, 56_000, "Over the moonlit sea tonight"),
    Line(56_000, 60_000, "I am floating away"),
    Line(60_000, 64_000, "Into the velvet sky"),
    Line(64_000, 68_000, "Where the stars hang low and bright"),
]


def sung_at(lines: list[Line], late_ms: int) -> dict[tuple[str, ...], int]:
    return {tuple(w.token for w in words(line.text)): line.start_ms + late_ms for line in lines}


def fakes(sung: dict[tuple[str, ...], int]):
    def decode(start_ms: int, end_ms: int) -> bytes:
        return json.dumps([start_ms, end_ms]).encode()

    def align_wide(pcm: bytes, tokens: list[str]) -> list[tuple[int, int]]:
        start_ms, end_ms = json.loads(pcm)
        at = sung[tuple(tokens)]
        if not start_ms <= at < end_ms:
            return []
        rel = at - start_ms
        return [(rel + i * 300, rel + (i + 1) * 300) for i in range(len(tokens))]

    return decode, align_wide


def test_lyrics_timed_for_another_cut_are_moved_onto_the_singing() -> None:
    decode, align_wide = fakes(sung_at(SONG, 3_700))
    found = estimate(SONG, decode, align_wide)
    assert found.offset_ms == 3_700, found
    moved = shift(SONG, found.offset_ms)
    assert moved[0] == Line(41_120, 44_700, SONG[0].text)


def test_early_singing_moves_lines_back_but_not_below_zero() -> None:
    lines = [Line(1_000, 4_000, "Breathe and I'll carry you away"), *SONG[1:]]
    decode, align_wide = fakes(sung_at(lines, -2_500))
    found = estimate(lines, decode, align_wide)
    assert found.offset_ms == -2_500, found
    assert shift(lines, found.offset_ms)[0].start_ms == 0


def test_lyrics_that_fit_are_left_alone() -> None:
    decode, align_wide = fakes(sung_at(SONG, NEGLIGIBLE_MS - 100))
    found = estimate(SONG, decode, align_wide)
    assert found.offset_ms == 0
    assert "negligible" in found.reason
    assert shift(SONG, 0) is SONG


def test_probes_that_disagree_shift_nothing() -> None:
    sung = sung_at(SONG, 0)
    lates = [3_000, -4_000, 6_000, 0, -2_000]
    for key, late in zip(list(sung), lates):
        sung[key] += late
    decode, align_wide = fakes(sung)
    found = estimate(SONG, decode, align_wide)
    assert found.offset_ms == 0
    assert "agree" in found.reason, found


def test_too_few_probes_shift_nothing() -> None:
    lines = SONG[:2]
    decode, align_wide = fakes(sung_at(lines, 3_000))
    found = estimate(lines, decode, align_wide)
    assert found.offset_ms == 0
    assert "only" in found.reason


def test_probes_skip_short_and_repeated_lines_and_spread_over_the_song() -> None:
    picked = probe_indices(SONG)
    # "Oh oh" is short; "Into the velvet sky" is sung twice.
    assert 3 not in picked and 1 not in picked and 6 not in picked
    names = [a + b for a in "abcdefgh" for b in "xyzvw"]
    many = [Line(i * 4_000, i * 4_000 + 4_000, f"line called {n} here") for i, n in enumerate(names)]
    spread = probe_indices(many)
    assert len(spread) == PROBES
    assert spread[0] == 0 and spread[-1] > 30
