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

from pharos_aligner.plan import parse_request, resolve, stamps, window
from pharos_aligner.text import words

logger = logging.getLogger("pharos_aligner")

MAX_BODY_BYTES = 1024 * 1024  # 1 MiB

# One alignment at a time: the model is not shared across threads and the CPU
# is the bottleneck anyway.
ALIGN_LOCK = threading.Lock()

DecoderFunc = Callable[..., bytes]


class AlignerHandler(http.server.BaseHTTPRequestHandler):
    """HTTP request handler for health checks and lyric alignment."""

    aligner: Any = None
    decoder: DecoderFunc | None = None
    media_root: str = ""

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
            media_root = self.get_media_root()
            full_path = resolve(media_root, rel_path)
            if not os.path.isfile(full_path):
                raise FileNotFoundError(f"file not found: {rel_path}")

            decoder = self.get_decoder()
            aligner = self.get_aligner()

            with ALIGN_LOCK:
                out_lines: list[dict[str, Any]] = []
                words_aligned = 0
                for line in lines:
                    w_list = words(line.text)
                    if not w_list:
                        out_lines.append({"words": []})
                        continue
                    w_start, w_end = window(line)
                    pcm = decoder(full_path, w_start, w_end)
                    tokens = [w.token for w in w_list]
                    spans = aligner.align(pcm, tokens)
                    line_stamps = stamps(line.text, w_list, spans, w_start)
                    out_lines.append({"words": line_stamps})
                    if line_stamps:
                        words_aligned += len(w_list)

            self._send_json(200, {"lines": out_lines})
            duration_ms = int((time.monotonic() - start_time) * 1000)
            logger.info(
                "path=%s lines=%d words_aligned=%d duration_ms=%d",
                rel_path,
                len(lines),
                words_aligned,
                duration_ms,
            )
        except ValueError as e:
            self._send_json(400, {"error": str(e)})
        except FileNotFoundError as e:
            self._send_json(404, {"error": str(e)})
        except subprocess.CalledProcessError as e:
            self._send_json(422, {"error": f"ffmpeg failed: {e}"})
        except Exception:
            logger.exception("internal error during alignment request")
            self._send_json(500, {"error": "internal error"})


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

    # One model for the life of the process: loading it takes seconds.
    from pharos_aligner.model import Aligner

    server = create_server((host, port), media_root=media_root, aligner=Aligner())
    logger.info("Serving pharos-aligner on %s:%d (media root: %s)", host, port, media_root)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
