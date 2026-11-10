"""Spans and phase timings for an alignment (ADR-0105, B1164)."""

from __future__ import annotations

import json
import logging
import threading
import time
import urllib.request
from pathlib import Path
from typing import Iterator

import pytest
from opentelemetry import trace
from opentelemetry.sdk.trace import ReadableSpan, TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter

from pharos_aligner.server import create_server
from pharos_aligner.tracing import Phases

EXPORTER = InMemorySpanExporter()
_provider = TracerProvider()
_provider.add_span_processor(SimpleSpanProcessor(EXPORTER))
trace.set_tracer_provider(_provider)

CALLER_TRACE = "0af7651916cd43dd8448eb211c80319c"


class FakeAligner:
    def align(self, pcm_f32le: bytes, tokens: list[str], lead_star: bool = False) -> list[tuple[int, int]]:
        return [(i * 100, (i + 1) * 100) for i in range(len(tokens))]


def fake_decoder(path: str, start_ms: int, end_ms: int, rate: int = 16000) -> bytes:
    return b"\0" * 64


@pytest.fixture
def base_url(tmp_path: Path) -> Iterator[str]:
    (tmp_path / "song.mp3").write_bytes(b"audio")
    server = create_server(("127.0.0.1", 0), media_root=str(tmp_path), aligner=FakeAligner(), decoder=fake_decoder)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def post_align(base_url: str, headers: dict[str, str]) -> None:
    payload = {"path": "song.mp3", "lines": [{"start_ms": 1000, "end_ms": 4000, "text": "hello there world"}]}
    req = urllib.request.Request(
        f"{base_url}/align",
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json", **headers},
        method="POST",
    )
    with urllib.request.urlopen(req) as resp:
        assert resp.status == 200


def finished_request_span() -> tuple[ReadableSpan, list[ReadableSpan]]:
    # The request span ends just after the answer is sent, on the server's
    # thread.
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        spans = list(EXPORTER.get_finished_spans())
        roots = [s for s in spans if s.name == "aligner.align"]
        if roots:
            return roots[0], spans
        time.sleep(0.01)
    raise AssertionError("no aligner.align span finished")


def test_an_alignment_joins_the_callers_trace_with_a_span_per_phase(
    base_url: str, caplog: pytest.LogCaptureFixture
) -> None:
    EXPORTER.clear()
    caplog.set_level(logging.INFO, logger="pharos_aligner")
    post_align(base_url, {"traceparent": f"00-{CALLER_TRACE}-b7ad6b7169203331-01"})

    root, spans = finished_request_span()
    assert format(root.context.trace_id, "032x") == CALLER_TRACE
    assert root.attributes is not None
    assert root.attributes["aligner.lines"] == 1
    assert root.attributes["aligner.words_aligned"] == 3
    assert "aligner.decode_ms" in root.attributes and "aligner.model_ms" in root.attributes

    names = {s.name for s in spans if s.context.trace_id == root.context.trace_id}
    assert {"aligner.wait", "aligner.decode", "aligner.model"} <= names
    line_model = [s for s in spans if s.name == "aligner.model" and (s.attributes or {}).get("stage") == "line"]
    assert line_model and line_model[0].attributes is not None and line_model[0].attributes["words"] == 3

    done = [r.getMessage() for r in caplog.records if "words_aligned=" in r.getMessage()]
    assert done and "decode_ms=" in done[0] and "model_ms=" in done[0], done


def test_an_alignment_without_a_caller_trace_starts_its_own(base_url: str) -> None:
    EXPORTER.clear()
    post_align(base_url, {})
    root, _ = finished_request_span()
    assert format(root.context.trace_id, "032x") != CALLER_TRACE
    assert root.parent is None


def test_phases_sum_their_time_and_name_it_in_a_stable_order() -> None:
    phases = Phases()
    with phases.phase("model"):
        pass
    with phases.phase("decode"):
        time.sleep(0.02)
    with phases.phase("decode"):
        time.sleep(0.02)
    assert phases.ms["decode"] >= 40
    assert phases.summary().startswith("decode_ms=")
    assert " model_ms=" in phases.summary()
