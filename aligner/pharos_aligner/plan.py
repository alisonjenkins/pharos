"""Planning, windowing, timestamp generation, and request parsing for alignment."""

from __future__ import annotations

import os
from typing import Any, NamedTuple

from pharos_aligner.text import Word

MARGIN_MS = 250


class Line(NamedTuple):
    start_ms: int
    end_ms: int
    text: str


# A line is aligned against at most this long a stretch of audio a word, plus
# CAP_EXTRA_MS, however far off the next line starts: a line followed by an
# instrumental break otherwise hands CTC a window that is mostly music.
CAP_MS_PER_WORD = 700
CAP_EXTRA_MS = 2000


def window(line: Line, n_words: int) -> tuple[int, int]:
    """Compute audio decode window with safety margin around line LRC span."""
    end_ms = min(line.end_ms, line.start_ms + n_words * CAP_MS_PER_WORD + CAP_EXTRA_MS)
    return (max(0, line.start_ms - MARGIN_MS), end_ms + MARGIN_MS)


def stamps(
    text: str,
    aligned: list[Word],
    spans: list[tuple[int, int]],
    window_start_ms: int,
) -> list[dict[str, int]]:
    """Convert relative token spans to non-decreasing word and end timestamps."""
    if not aligned or len(aligned) != len(spans):
        return []

    result: list[dict[str, int]] = []
    last_t = -1
    for i, (w, (s, _)) in enumerate(zip(aligned, spans)):
        t = window_start_ms + s
        if i > 0:
            t = max(t, last_t)
        result.append({"at": w.at, "start_ms": t})
        last_t = t

    end_t = max(window_start_ms + spans[-1][1], last_t)
    result.append({"at": len(text), "start_ms": end_t})
    return result


def parse_request(body: dict[str, Any]) -> tuple[str, list[Line]]:
    """Validate request body and return normalized path and line definitions."""
    if not isinstance(body, dict):
        raise ValueError("request body must be an object")

    if "path" not in body:
        raise ValueError("path is required")
    path = body["path"]
    if not isinstance(path, str):
        raise ValueError("path must be a string")
    if not path:
        raise ValueError("path cannot be empty")

    if "lines" not in body:
        raise ValueError("lines is required")
    lines_raw = body["lines"]
    if not isinstance(lines_raw, list):
        raise ValueError("lines must be a list")
    if len(lines_raw) > 500:
        raise ValueError("lines cannot exceed 500")

    lines: list[Line] = []
    for line in lines_raw:
        if not isinstance(line, dict):
            raise ValueError("each line must be an object")
        for field in ("start_ms", "end_ms", "text"):
            if field not in line:
                raise ValueError(f"line field {field} is required")

        start_ms = line["start_ms"]
        end_ms = line["end_ms"]
        text = line["text"]

        if not isinstance(start_ms, int) or isinstance(start_ms, bool):
            raise ValueError("start_ms must be an integer")
        if not isinstance(end_ms, int) or isinstance(end_ms, bool):
            raise ValueError("end_ms must be an integer")
        if not isinstance(text, str):
            raise ValueError("text must be a string")

        if start_ms < 0:
            raise ValueError("start_ms cannot be negative")
        if end_ms < 0:
            raise ValueError("end_ms cannot be negative")
        if end_ms < start_ms:
            raise ValueError("end_ms cannot be less than start_ms")
        if len(text) > 500:
            raise ValueError("text cannot exceed 500 characters")

        lines.append(Line(start_ms=start_ms, end_ms=end_ms, text=text))

    return path, lines


def resolve(media_root: str, path: str) -> str:
    """Resolve relative path inside media root, rejecting escapes."""
    if os.path.isabs(path):
        raise ValueError("path escapes the media root")
    real_root = os.path.realpath(media_root)
    joined = os.path.join(real_root, path)
    real_path = os.path.realpath(joined)

    try:
        common = os.path.commonpath([real_root, real_path])
    except ValueError:
        raise ValueError("path escapes the media root")

    if common != real_root or real_path == real_root:
        raise ValueError("path escapes the media root")

    return real_path
