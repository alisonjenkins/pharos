"""Integration tests for the HTTP server using fake decoder and aligner."""

from __future__ import annotations

import json
from pathlib import Path
import threading
import urllib.error
import urllib.request
from typing import Iterator
import pytest

from pharos_aligner.server import create_server


class FakeAligner:
    """Fake aligner returning evenly spaced spans per token."""

    def align(self, pcm_f32le: bytes, tokens: list[str]) -> list[tuple[int, int]]:
        if not tokens:
            return []
        step = 100
        return [(i * step, (i + 1) * step) for i in range(len(tokens))]


def fake_decoder(path: str, start_ms: int, end_ms: int, rate: int = 16000) -> bytes:
    """Fake decoder returning dummy float32 audio bytes."""
    return b"\0" * 64


@pytest.fixture
def running_server(tmp_path: Path) -> Iterator[tuple[str, Path]]:
    """Start the aligner HTTP server on port 0 in a background thread."""
    server = create_server(
        ("127.0.0.1", 0),
        media_root=str(tmp_path),
        aligner=FakeAligner(),
        decoder=fake_decoder,
    )
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    base_url = f"http://127.0.0.1:{port}"
    try:
        yield base_url, tmp_path
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def test_healthz(running_server: tuple[str, Path]) -> None:
    base_url, _ = running_server
    with urllib.request.urlopen(f"{base_url}/healthz") as resp:
        assert resp.status == 200
        data = json.loads(resp.read().decode("utf-8"))
        assert data == {"ok": True}


def test_post_align_two_lines(running_server: tuple[str, Path]) -> None:
    base_url, media_root = running_server
    song_file = media_root / "test.mp3"
    song_file.write_bytes(b"dummy audio content")

    payload = {
        "path": "test.mp3",
        "lines": [
            {"start_ms": 1000, "end_ms": 2000, "text": "Hello world"},
            {"start_ms": 2500, "end_ms": 3000, "text": "♪♪"},
        ],
    }
    req = urllib.request.Request(
        f"{base_url}/align",
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req) as resp:
        assert resp.status == 200
        data = json.loads(resp.read().decode("utf-8"))

    assert "lines" in data
    assert len(data["lines"]) == 2

    # Line 0: window start is max(0, 1000 - 250) = 750
    # tokens: ["hello", "world"]
    # fake spans: [(0, 100), (100, 200)]
    # stamps: word 0: at=0, 750+0=750; word 1: at=6, 750+100=850; end: at=11, 750+200=950
    line0_words = data["lines"][0]["words"]
    assert line0_words == [
        {"at": 0, "start_ms": 750},
        {"at": 6, "start_ms": 850},
        {"at": 11, "start_ms": 950},
    ]

    # Line 1: no alignable words -> empty words list
    assert data["lines"][1]["words"] == []


def test_post_align_bad_input_400(running_server: tuple[str, Path]) -> None:
    base_url, _ = running_server
    payload = {"bad": "data"}
    req = urllib.request.Request(
        f"{base_url}/align",
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with pytest.raises(urllib.error.HTTPError) as exc_info:
        urllib.request.urlopen(req)
    assert exc_info.value.code == 400


def test_post_align_missing_file_404(running_server: tuple[str, Path]) -> None:
    base_url, _ = running_server
    payload = {
        "path": "missing.mp3",
        "lines": [{"start_ms": 0, "end_ms": 1000, "text": "hello"}],
    }
    req = urllib.request.Request(
        f"{base_url}/align",
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with pytest.raises(urllib.error.HTTPError) as exc_info:
        urllib.request.urlopen(req)
    assert exc_info.value.code == 404


def test_post_align_negative_length_413(running_server: tuple[str, Path]) -> None:
    base_url, _ = running_server
    host, port = base_url.removeprefix("http://").split(":")
    import http.client

    conn = http.client.HTTPConnection(host, int(port), timeout=5)
    conn.putrequest("POST", "/align")
    conn.putheader("Content-Length", "-1")
    conn.endheaders()
    assert conn.getresponse().status == 413
