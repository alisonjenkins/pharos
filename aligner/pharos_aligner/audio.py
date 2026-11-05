"""Audio decoding via ffmpeg subprocess."""

from __future__ import annotations

import subprocess


def decode(path: str, start_ms: int, end_ms: int, rate: int = 16000) -> bytes:
    """Decode audio window to single-channel 32-bit float PCM via ffmpeg."""
    start_s = "%.3f" % (start_ms / 1000.0)
    dur_s = "%.3f" % ((end_ms - start_ms) / 1000.0)
    cmd = [
        "ffmpeg",
        "-nostdin",
        "-v",
        "error",
        "-ss",
        start_s,
        "-t",
        dur_s,
        "-i",
        path,
        "-ac",
        "1",
        "-ar",
        str(rate),
        "-f",
        "f32le",
        "-",
    ]
    res = subprocess.run(cmd, check=True, capture_output=True, timeout=120)
    return res.stdout
