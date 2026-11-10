# pharos-aligner

Forced alignment HTTP service for Pharos karaoke word-level lyric times.

- `PHAROS_ALIGNER_LISTEN`: host and port to listen on (default `0.0.0.0:8790`).
- `PHAROS_ALIGNER_MEDIA_ROOT`: absolute path to media directory (required).
- `PHAROS_ALIGNER_IDLE_UNLOAD_SECS`: seconds without an alignment before the model (about 1.3 GiB) is freed (default `600`, `0` keeps it loaded). The next alignment reloads it, which takes 10 to 30 s.
- `PHAROS_ALIGNER_THREADS`: CPU threads torch may use (default `8`). torch would otherwise take one a core, which on a busy node is up to 20 times slower.
- `PHAROS_ALIGNER_OTLP_ENDPOINT`: OTLP/gRPC endpoint for traces (ADR-0105), e.g. `http://tempo:4317`; unset exports nothing. A request's `traceparent` header makes its spans part of the caller's trace. Each song logs one line, `path=… words_aligned=… duration_ms=… decode_ms=… model_ms=…`, or `failed=ffmpeg|internal|caller_gone` with the same totals.
- `POST /align`: compute word-level timestamps for synced lyric lines. Before timing the words it looks for a constant offset between the lines and the singing (lyrics timed for another cut of the song), moves every line by it, and reports it as `offset_ms`.
- `GET /healthz`: health probe returning `{"ok": true}`.

Run tests: `pytest` inside `aligner/`.
