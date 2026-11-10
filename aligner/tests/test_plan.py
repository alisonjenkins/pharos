"""Tests for alignment planning, windowing, timestamps, and request validation."""

from __future__ import annotations

from pathlib import Path
import pytest

from pharos_aligner.plan import Line, parse_request, resolve, stamps, window
from pharos_aligner.text import Word


def test_window_clamps_at_zero() -> None:
    line_near_zero = Line(start_ms=100, end_ms=500, text="test")
    assert window(line_near_zero, 1) == (0, 750)

    line_far = Line(start_ms=1000, end_ms=2000, text="test")
    assert window(line_far, 1) == (250, 2250)


def test_window_stops_short_of_a_long_break() -> None:
    # Three words, then 30 s of music before the next line.
    line = Line(start_ms=10_000, end_ms=40_000, text="one two three")
    assert window(line, 3) == (9_250, 10_000 + 3 * 700 + 2_000 + 250)


def test_window_opens_early_enough_for_a_stamp_tapped_late() -> None:
    # Tapped 600 ms after the singing starts: the window must still hold it.
    sung_from = 20_000
    line = Line(start_ms=sung_from + 600, end_ms=25_000, text="one two")
    assert window(line, 2)[0] <= sung_from


def test_window_lead_stops_where_the_line_before_ended() -> None:
    line = Line(start_ms=20_000, end_ms=25_000, text="one two")
    # The line before ended 300 ms ahead of this stamp: the lead stops there.
    assert window(line, 2, prev_end_ms=19_700)[0] == 19_700
    # It never opens later than the plain margin, even if the line before ran on.
    assert window(line, 2, prev_end_ms=20_400)[0] == 19_750
    # Long gone: the full lead.
    assert window(line, 2, prev_end_ms=10_000)[0] == 19_250


def test_stamps_end_stamp_and_non_decreasing() -> None:
    words_list = [Word(at=0, token="hello"), Word(at=6, token="world")]
    spans = [(100, 150), (80, 200)]
    res = stamps("hello world", words_list, spans, window_start_ms=1000)

    # Word 0: 1000 + 100 = 1100
    # Word 1: 1000 + 80 = 1080 -> forced to 1100
    # End stamp: 1000 + 200 = 1200 at len("hello world") = 11
    assert res == [
        {"at": 0, "start_ms": 1100},
        {"at": 6, "start_ms": 1100},
        {"at": 11, "start_ms": 1200},
    ]


def test_stamps_mismatched_and_empty() -> None:
    assert stamps("hello", [], [], 0) == []
    words_list = [Word(at=0, token="hello")]
    assert stamps("hello", words_list, [], 0) == []
    assert stamps("hello", words_list, [(0, 10), (10, 20)], 0) == []


@pytest.mark.parametrize(
    "bad_body",
    [
        "not a dict",
        {},
        {"lines": []},
        {"path": 123, "lines": []},
        {"path": "", "lines": []},
        {"path": "a.mp3"},
        {"path": "a.mp3", "lines": "not a list"},
        {"path": "a.mp3", "lines": [{"start_ms": 0, "end_ms": 100, "text": "a"}] * 501},
        {"path": "a.mp3", "lines": ["not a dict"]},
        {"path": "a.mp3", "lines": [{"end_ms": 100, "text": "a"}]},
        {"path": "a.mp3", "lines": [{"start_ms": 0, "text": "a"}]},
        {"path": "a.mp3", "lines": [{"start_ms": 0, "end_ms": 100}]},
        {"path": "a.mp3", "lines": [{"start_ms": "0", "end_ms": 100, "text": "a"}]},
        {"path": "a.mp3", "lines": [{"start_ms": True, "end_ms": 100, "text": "a"}]},
        {"path": "a.mp3", "lines": [{"start_ms": 0, "end_ms": "100", "text": "a"}]},
        {"path": "a.mp3", "lines": [{"start_ms": 0, "end_ms": False, "text": "a"}]},
        {"path": "a.mp3", "lines": [{"start_ms": 0, "end_ms": 100, "text": 123}]},
        {"path": "a.mp3", "lines": [{"start_ms": -1, "end_ms": 100, "text": "a"}]},
        {"path": "a.mp3", "lines": [{"start_ms": 0, "end_ms": -1, "text": "a"}]},
        {"path": "a.mp3", "lines": [{"start_ms": 200, "end_ms": 100, "text": "a"}]},
        {"path": "a.mp3", "lines": [{"start_ms": 0, "end_ms": 100, "text": "x" * 501}]},
    ],
)
def test_parse_request_rejects_bad_cases(bad_body: object) -> None:
    with pytest.raises(ValueError):
        parse_request(bad_body)  # type: ignore[arg-type]


def test_parse_request_valid() -> None:
    body = {
        "path": "track.mp3",
        "lines": [{"start_ms": 100, "end_ms": 200, "text": "hi"}],
    }
    path, lines = parse_request(body)
    assert path == "track.mp3"
    assert lines == [Line(start_ms=100, end_ms=200, text="hi")]


def test_resolve_accepts_valid_and_rejects_escapes(tmp_path: Path) -> None:
    media_root = tmp_path / "media"
    media_root.mkdir()
    music_dir = media_root / "Music"
    music_dir.mkdir()
    valid_file = music_dir / "a.mp3"
    valid_file.touch()

    # Accepts "Music/a.mp3"
    resolved = resolve(str(media_root), "Music/a.mp3")
    assert resolved == str(valid_file.resolve())

    # Rejects "../x"
    with pytest.raises(ValueError, match="path escapes the media root"):
        resolve(str(media_root), "../x")

    # Rejects "/etc/passwd"
    with pytest.raises(ValueError, match="path escapes the media root"):
        resolve(str(media_root), "/etc/passwd")

    # Rejects symlink escaping root
    outside_dir = tmp_path / "outside"
    outside_dir.mkdir()
    outside_file = outside_dir / "secret.mp3"
    outside_file.touch()

    escape_symlink = media_root / "escape.mp3"
    escape_symlink.symlink_to(outside_file)

    with pytest.raises(ValueError, match="path escapes the media root"):
        resolve(str(media_root), "escape.mp3")
