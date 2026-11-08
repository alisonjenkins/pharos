"""Idle unload of the alignment model, with a fake loader and clock."""

from __future__ import annotations

from typing import Any

import pytest

from pharos_aligner.model import Aligner


class FakeClock:
    def __init__(self) -> None:
        self.now = 1000.0

    def __call__(self) -> float:
        return self.now


class CountingAligner(Aligner):
    """Aligner whose load is a counter and whose alignment is a stub."""

    def __init__(self, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self.loads = 0

    def _load(self) -> tuple[Any, Any, Any]:
        self.loads += 1
        return object(), object(), object()

    def _align_loaded(self, pcm_f32le: bytes, tokens: list[str]) -> list[tuple[int, int]]:
        return [(0, 0) for _ in tokens]


def test_model_is_unloaded_after_idle_timeout_and_reloaded_on_next_alignment() -> None:
    clock = FakeClock()
    aligner = CountingAligner(idle_unload_s=600, clock=clock)
    aligner.align(b"\0" * 4, ["hello"])
    assert aligner.loaded

    clock.now += 599
    assert not aligner.unload_if_idle()
    assert aligner.loaded

    clock.now += 1
    assert aligner.unload_if_idle()
    assert not aligner.loaded

    aligner.align(b"\0" * 4, ["hello"])
    assert aligner.loaded
    assert aligner.loads == 2


def test_each_alignment_restarts_the_idle_timer() -> None:
    clock = FakeClock()
    aligner = CountingAligner(idle_unload_s=600, clock=clock)
    aligner.align(b"\0" * 4, ["a"])
    clock.now += 500
    aligner.align(b"\0" * 4, ["b"])
    clock.now += 500
    assert not aligner.unload_if_idle()
    assert aligner.loads == 1


def test_without_idle_timeout_the_model_stays_loaded() -> None:
    clock = FakeClock()
    aligner = CountingAligner(idle_unload_s=None, clock=clock)
    aligner.align(b"\0" * 4, ["a"])
    clock.now += 1e9
    assert not aligner.unload_if_idle()
    assert aligner.loaded


def test_a_model_never_loaded_is_not_unloaded() -> None:
    clock = FakeClock()
    aligner = CountingAligner(idle_unload_s=1, clock=clock)
    clock.now += 10
    assert not aligner.unload_if_idle()
    assert aligner.loads == 0


@pytest.mark.parametrize(
    ("raw", "expected"),
    [(None, 600), ("", 600), ("0", None), ("120", 120)],
)
def test_idle_unload_secs_parses_the_environment(raw: str | None, expected: float | None) -> None:
    from pharos_aligner.server import idle_unload_secs

    assert idle_unload_secs(raw) == expected


def test_idle_unload_secs_rejects_negative() -> None:
    from pharos_aligner.server import idle_unload_secs

    with pytest.raises(ValueError):
        idle_unload_secs("-1")
