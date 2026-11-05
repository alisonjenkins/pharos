"""Tests for text normalization and word tokenization."""

from __future__ import annotations

from pharos_aligner.text import Word, words


def test_offsets_multiple_spaces() -> None:
    res = words("Hello  wonderful world")
    assert res == [
        Word(at=0, token="hello"),
        Word(at=7, token="wonderful"),
        Word(at=17, token="world"),
    ]


def test_right_single_quote_folded() -> None:
    res = words("Don’t")
    assert res == [Word(at=0, token="don't")]


def test_accent_folded() -> None:
    res = words("Café")
    assert res == [Word(at=0, token="cafe")]


def test_non_alphabet_words_skipped() -> None:
    res = words("夜空 la")
    assert res == [Word(at=3, token="la")]


def test_punctuation_stripped() -> None:
    res = words("world!")
    assert res == [Word(at=0, token="world")]


def test_empty_and_whitespace_only() -> None:
    assert words("") == []
    assert words("   \t\n  ") == []
