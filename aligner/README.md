# pharos-aligner

Forced alignment HTTP service for Pharos karaoke word-level lyric times.

- `PHAROS_ALIGNER_LISTEN`: host and port to listen on (default `0.0.0.0:8790`).
- `PHAROS_ALIGNER_MEDIA_ROOT`: absolute path to media directory (required).
- `POST /align`: compute word-level timestamps for synced lyric lines.
- `GET /healthz`: health probe returning `{"ok": true}`.

Run tests: `pytest` inside `aligner/`.
