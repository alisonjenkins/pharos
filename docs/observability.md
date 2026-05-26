# Observability

Three pillars wired from T1, deepened in T23. Behaviour is governed by SPEC §V invariants V13, V14, V15, V8.

## Logs (V15)

- Sole logging crate: `tracing`. No `println!`/`eprintln!` in non-CLI code.
- Subscriber: JSON formatter + `EnvFilter`. Init lives in `pharos_server::obs::init`. Idempotent under concurrent callers via `std::sync::Once`.
- Span context: `tracing-actix-web::TracingLogger` wraps every request, opening a `http.request` span with method, URI, request_id.
- Hot ops carry their own spans via `#[tracing::instrument]`:
  - `pharos_store_sqlx::sqlite::SqliteStore::{get,put,list}` — fields `media.id`, `media.kind`
  - `pharos_scanner::fs::FsScanner::{scan,scan_into}` — field `root`
- Tune verbosity per-deploy via `PHAROS_LOG_LEVEL` or `obs.log_level` in `config.toml`. Accepts the standard `tracing-subscriber` directive syntax (e.g. `info,pharos_store_sqlx=debug`).

## Metrics (V14)

- Exposition format: Prometheus. Endpoint: `GET /metrics`.
- Recorder: `metrics-exporter-prometheus`. Cached handle behind `OnceLock`; render path is lock-free (V18).
- Request-level RED via `pharos_server::middleware::RedMetrics`:
  - `http_requests_total{method,path,status}` — counter
  - `http_request_duration_seconds{method,path}` — histogram (seconds, default criterion buckets)
- Path label is the route match pattern (e.g. `/Items/{id}`), not the concrete URI. Keeps label cardinality bounded — guarded by an integration test.
- Subsystems may emit their own counters/histograms freely; they land in the same registry and surface at `/metrics`.

## Health (V14)

- `/healthz` — constant 200 as long as the actix worker thread is alive.
- `/readyz` — 200 only when **every** required probe is marked. Returns 503 with a JSON snapshot of pending probes otherwise.
- `/info` — JSON `{name, version}`.
- State is owned by a single tokio task (`ReadinessHandle::spawn`). Handlers query via oneshot reply. No `Mutex` on the request path (V18).

## Secrets / log redaction (V8)

Two-layer strategy:

1. **Structural** — wrap any token, password, or API key in `pharos_core::SecretString`. Its `Debug` and `Display` impls return the literal string `<redacted>`. An accidental `tracing::info!(token = %tok)` cannot leak the value; tests confirm.
2. **Don't log it** — handlers must not pull bearer tokens, cookies, or `X-Emby-Token` headers into log fields. Reviewers should reject patches that do.

`SecretString` is intentionally not `Serialize`/`Deserialize`. Callers needing the bytes off the type must call `.expose()` explicitly — the name is a flag for code review.

## Tracing → OTLP (planned)

`config.obs.otlp_endpoint` is plumbed end-to-end but the actual exporter wiring is feature-gated and lands when the first downstream collector requirement appears. Default deploys emit JSON logs to stdout only.

## Where each invariant lives

| Invariant | Implementation site |
|---|---|
| V8 (no token leak) | `pharos_core::secret::SecretString` + reviewer discipline |
| V13 (every request traced, every IO spanned) | `TracingLogger` wrap + `#[tracing::instrument]` on hot ops |
| V14 (`/healthz`/`/readyz`/`/metrics`) | `pharos_server::health` + `pharos_server::obs` |
| V15 (structured logs only) | `pharos_server::obs::init` + workspace ban via review |
| V18 (no `Mutex` on hot path) | `OnceLock`/`Once` for init; mpsc actors for runtime state |
