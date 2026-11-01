# Contract: serving pharos-ui at the root (FR-002, research R4)

| Request | Response |
|---|---|
| `GET /` (Accept: text/html) | `index.html`, `Cache-Control: no-cache` |
| `GET /assets/<hash>.<ext>` | the file, `Cache-Control: public, max-age=31536000, immutable` |
| `GET /title/123` (no server route; Accept: text/html) | `index.html` |
| `GET /title/123` (Accept: application/json) | 404 (never HTML to an API client) |
| `GET /ui/<rest>` | `308` → `/<rest>` |
| `GET /web/<rest>` | jellyfin-web, unchanged |
| any Jellyfin or pharos API route, any case | unchanged (LowercasePath still applies) |
| `GET /pharos/rpc` with `Upgrade: websocket` | 017 RPC |

pharos-ui disabled (`ui_dir` unset): `/` keeps today's behaviour: `router.rs:88`
answers `200 "pharos"`, pinned by `root_responds_200`. When `ui_dir` is set, that
route yields to the UI. Some probes may rely on the body, so the plain-text
answer moves to `/info`'s existing JSON, which liveness checks should use instead.
