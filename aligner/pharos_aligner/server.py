"""HTTP server for lyric word alignment service."""

from __future__ import annotations

import http.server
import json
import logging
import os
import subprocess
import sys
import threading
import time
from typing import Any, Callable

from pharos_aligner.offset import estimate, shift
from pharos_aligner.plan import parse_request, resolve, stamps, window
from pharos_aligner.text import words

logger = logging.getLogger("pharos_aligner")

MAX_BODY_BYTES = 1024 * 1024  # 1 MiB

# One alignment at a time: the model is not shared across threads and the CPU
# is the bottleneck anyway.
ALIGN_LOCK = threading.Lock()

DecoderFunc = Callable[..., bytes]

# The loaded model holds about 1.3 GiB; reloading it takes 10 to 30 s.
DEFAULT_IDLE_UNLOAD_SECS = 600
IDLE_CHECK_SECS = 30

# torch otherwise runs one thread a core. On a busy node its OpenMP threads
# wait on each other at every op, and a song that takes 30 s at 8 threads
# takes 500 s at 44.
DEFAULT_THREADS = 8


def idle_unload_secs(raw: str | None) -> float | None:
    """Parse PHAROS_ALIGNER_IDLE_UNLOAD_SECS; 0 keeps the model loaded."""
    if raw is None or raw.strip() == "":
        return DEFAULT_IDLE_UNLOAD_SECS
    value = float(raw)
    if value < 0:
        raise ValueError(f"PHAROS_ALIGNER_IDLE_UNLOAD_SECS must be >= 0, got {raw!r}")
    return None if value == 0 else value


def torch_threads(raw: str | None) -> int:
    """Parse PHAROS_ALIGNER_THREADS, the CPU threads torch may use."""
    if raw is None or raw.strip() == "":
        return DEFAULT_THREADS
    value = int(raw)
    if value < 1:
        raise ValueError(f"PHAROS_ALIGNER_THREADS must be >= 1, got {raw!r}")
    return value


class AlignerHandler(http.server.BaseHTTPRequestHandler):
    """HTTP request handler for health checks and lyric alignment."""

    aligner: Any = None
    decoder: DecoderFunc | None = None
    media_root: str = ""
    # Seconds a socket read may wait, so a client that stalls mid-body frees
    # its thread.
    timeout = 30

    def get_aligner(self) -> Any:
        if self.aligner is not None:
            return self.aligner
        server_aligner = getattr(self.server, "aligner", None)
        if server_aligner is not None:
            return server_aligner
        from pharos_aligner.model import Aligner

        return Aligner()

    def get_decoder(self) -> DecoderFunc:
        if self.decoder is not None:
            return self.decoder
        server_decoder = getattr(self.server, "decoder", None)
        if server_decoder is not None:
            return server_decoder
        from pharos_aligner.audio import decode

        return decode

    def get_media_root(self) -> str:
        if self.media_root:
            return self.media_root
        server_root = getattr(self.server, "media_root", None)
        if server_root:
            return server_root
        return os.environ.get("PHAROS_ALIGNER_MEDIA_ROOT", "")

    def log_message(self, format: str, *args: Any) -> None:
        logger.debug("%s - - [%s] %s", self.address_string(), self.log_date_time_string(), format % args)

    def _send_json(self, status: int, payload: dict[str, Any]) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path == "/healthz":
            self._send_json(200, {"ok": True})
        else:
            self._send_json(404, {"error": "not found"})

    def do_POST(self) -> None:
        if self.path != "/align":
            self._send_json(404, {"error": "not found"})
            return

        length_header = self.headers.get("Content-Length")
        if length_header is None:
            self._send_json(400, {"error": "Content-Length header required"})
            return

        try:
            content_length = int(length_header)
        except ValueError:
            self._send_json(400, {"error": "invalid Content-Length"})
            return

        if content_length < 0 or content_length > MAX_BODY_BYTES:
            self._send_json(413, {"error": "payload too large"})
            return

        raw_bytes = self.rfile.read(content_length)
        try:
            body = json.loads(raw_bytes.decode("utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError) as e:
            self._send_json(400, {"error": f"invalid JSON: {e}"})
            return

        start_time = time.monotonic()
        try:
            rel_path, lines = parse_request(body)
            full_path = resolve(self.get_media_root(), rel_path)
        except ValueError as e:
            self._send_json(400, {"error": str(e)})
            return
        # Checked here, not caught: a FileNotFoundError from inside the work
        # (ffmpeg missing) is a server fault, not a missing file.
        if not os.path.isfile(full_path):
            self._send_json(404, {"error": f"file not found: {rel_path}"})
            return

        try:
            decoder = self.get_decoder()
            aligner = self.get_aligner()

            def align_wide(pcm: bytes, tokens: list[str]) -> list[tuple[int, int]]:
                try:
                    return aligner.align(pcm, tokens, lead_star=True)
                except (RuntimeError, ValueError):
                    logger.warning("path=%s offset probe failed", rel_path, exc_info=True)
                    return []

            probe_timed_out = False

            def decode_probe(start_ms: int, end_ms: int) -> bytes:
                # A probe is optional: a window ffmpeg cannot decode costs it
                # the offset, not the song its word times. After one timeout
                # the rest are skipped, so seven of them cannot outlast the
                # server's whole-song budget.
                nonlocal probe_timed_out
                if probe_timed_out:
                    return b""
                try:
                    return decoder(full_path, start_ms, end_ms)
                except subprocess.TimeoutExpired:
                    probe_timed_out = True
                    logger.warning("path=%s offset probe decode timed out; skipping the rest", rel_path)
                    return b""
                except subprocess.CalledProcessError:
                    logger.warning("path=%s offset probe decode failed", rel_path, exc_info=True)
                    return b""

            with ALIGN_LOCK:
                found = estimate(lines, decode_probe, align_wide)
                logger.info(
                    "path=%s offset_ms=%d probes=%s reason=%s",
                    rel_path,
                    found.offset_ms,
                    found.probes,
                    found.reason,
                )
                lines = shift(lines, found.offset_ms)
                out_lines: list[dict[str, Any]] = []
                words_aligned = 0
                prev_end_ms = 0
                for index, line in enumerate(lines):
                    w_list = words(line.text)
                    if not w_list:
                        out_lines.append({"words": []})
                        continue
                    w_start, w_end = window(line, len(w_list), prev_end_ms)
                    pcm = decoder(full_path, w_start, w_end)
                    tokens = [w.token for w in w_list]
                    try:
                        spans = aligner.align(pcm, tokens)
                    except (RuntimeError, ValueError):
                        # A window too short for its text makes the aligner
                        # raise; only that line goes without word times.
                        logger.warning("path=%s line=%d alignment failed", rel_path, index, exc_info=True)
                        out_lines.append({"words": []})
                        continue
                    line_stamps = stamps(line.text, w_list, spans, w_start)
                    out_lines.append({"words": line_stamps})
                    if line_stamps:
                        words_aligned += len(w_list)
                        prev_end_ms = line_stamps[-1]["start_ms"]
        except subprocess.CalledProcessError as e:
            self._send_json(422, {"error": f"ffmpeg failed: {e}"})
            return
        except Exception:
            logger.exception("internal error during alignment request")
            self._send_json(500, {"error": "internal error"})
            return

        self._send_json(200, {"lines": out_lines, "offset_ms": found.offset_ms})
        duration_ms = int((time.monotonic() - start_time) * 1000)
        logger.info(
            "path=%s lines=%d words_aligned=%d offset_ms=%d duration_ms=%d",
            rel_path,
            len(lines),
            words_aligned,
            found.offset_ms,
            duration_ms,
        )


def create_server(
    server_address: tuple[str, int],
    media_root: str,
    aligner: Any = None,
    decoder: DecoderFunc | None = None,
) -> http.server.HTTPServer:
    """Create a configured HTTP server; alignments run one at a time."""
    # Threaded so /healthz answers during an alignment; ALIGN_LOCK keeps the
    # alignments themselves one at a time.
    server = http.server.ThreadingHTTPServer(server_address, AlignerHandler)
    server.media_root = media_root  # type: ignore[attr-defined]
    server.aligner = aligner  # type: ignore[attr-defined]
    server.decoder = decoder  # type: ignore[attr-defined]
    return server


def main() -> None:
    """Run the pharos-aligner HTTP service."""
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%Y-%m-%dT%H:%M:%SZ",
    )
    logging.Formatter.converter = time.gmtime
    media_root = os.environ.get("PHAROS_ALIGNER_MEDIA_ROOT")
    if not media_root:
        sys.stderr.write("PHAROS_ALIGNER_MEDIA_ROOT environment variable is required\n")
        sys.exit(1)

    listen = os.environ.get("PHAROS_ALIGNER_LISTEN", "0.0.0.0:8790")
    if ":" in listen:
        host, port_str = listen.rsplit(":", 1)
        port = int(port_str)
    else:
        host = listen
        port = 8790

    from pharos_aligner.model import Aligner, set_threads, start_idle_reaper

    threads = torch_threads(os.environ.get("PHAROS_ALIGNER_THREADS"))
    set_threads(threads)
    idle_unload_s = idle_unload_secs(os.environ.get("PHAROS_ALIGNER_IDLE_UNLOAD_SECS"))
    aligner = Aligner(idle_unload_s=idle_unload_s)
    if idle_unload_s is not None:
        start_idle_reaper(aligner, interval_s=min(IDLE_CHECK_SECS, idle_unload_s))
    server = create_server((host, port), media_root=media_root, aligner=aligner)
    logger.info(
        "Serving pharos-aligner on %s:%d (media root: %s, idle unload: %s s, threads: %d)",
        host,
        port,
        media_root,
        "off" if idle_unload_s is None else int(idle_unload_s),
        threads,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
