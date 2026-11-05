"""Text normalization and word extraction for alignment."""

from __future__ import annotations

import re
import unicodedata
from typing import NamedTuple

ALPHABET = set("abcdefghijklmnopqrstuvwxyz'")


class Word(NamedTuple):
    at: int
    token: str


def words(text: str) -> list[Word]:
    """Extract words and character offsets from text with accents folded and symbols stripped."""
    result: list[Word] = []
    for m in re.finditer(r"\S+", text):
        raw = m.group()
        mapped = raw.replace("\u2019", "'")
        decomposed = unicodedata.normalize("NFKD", mapped)
        no_combining = "".join(c for c in decomposed if not unicodedata.combining(c))
        lowered = no_combining.lower()
        token = "".join(c for c in lowered if c in ALPHABET)
        if token:
            result.append(Word(at=m.start(), token=token))
    return result
