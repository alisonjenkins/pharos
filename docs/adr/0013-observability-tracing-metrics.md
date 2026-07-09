# ADR-0013: Structured tracing + Prometheus metrics + OTLP traces

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

pharos runs unattended in a home cluster next to a live transcode workload; when
a stream buffers or a scan stalls, we need to see *why* without shelling into the
pod. Three signals matter — logs (what happened), metrics (how often / how slow),
and traces (the causal path across async tasks and IO). Invariants V13–V15 in
`SPEC.md` require every request traced, every hot IO spanned, and structured logs
only. We wanted these wired from the start, machine-parseable, and cheap enough
that the read path never contends a lock on the request hot path (V18).

## Decision

All three pillars are bootstrapped by `pharos_server::obs::init(log_level,
otlp_endpoint)`, idempotent under concurrent callers via `std::sync::Once`.

- **Logs** — sole logging crate is `tracing`; no `println!`/`eprintln!` in
  non-CLI code. A JSON `fmt` layer plus an `EnvFilter` driven by
  `[obs].log_level` in `config.toml` (or `PHAROS_LOG_LEVEL`), accepting the
  standard directive syntax (e.g. `info,pharos_store_sqlx=debug`).
  `tracing-actix-web::TracingLogger` opens an `http.request` span per request;
  hot ops carry `#[tracing::instrument]` spans.
- **Metrics** — Prometheus exposition at `GET /metrics` via
  `metrics-exporter-prometheus`, the handle cached behind a `OnceLock` so
  rendering is lock-free (V18). Request-level RED counters/histograms use the
  route *match pattern* (`/Items/{id}`) as the path label to bound cardinality.
- **Traces** — when `[obs].otlp_endpoint` is set, spans are additionally
  exported over **OTLP/gRPC** (via `opentelemetry-otlp` + `tonic`, batch
  processor) to a collector — Tempo in the home cluster. The tracer provider is
  held in a `OnceLock` for the process lifetime so the batch exporter keeps
  flushing. When the endpoint is unset the OTLP layer is `None` (a no-op layer),
  and deploys emit JSON logs to stdout only.

Secrets never enter any signal: tokens/passwords are wrapped in
`pharos_core::SecretString`, whose `Debug`/`Display` render `<redacted>` (V8).

## Consequences

- Strong production visibility: JSON logs into the cluster log stack, a
  scrape-ready `/metrics` (a `ServiceMonitor` toggle ships in the chart), and
  distributed traces into Tempo when a collector is present.
- Tracing is opt-in per deploy — no collector, no OTLP overhead; the same binary
  runs bare (stdout logs) or fully instrumented by flipping one config field.
- Tempo needs adequate memory to hold the trace ingest/query heap: it
  OOM-crashlooped at a 512Mi limit and needs multiple Gi. Budget for it before
  enabling `otlp_endpoint` in the cluster.

## References

- `crates/pharos-server/src/obs.rs` (`init`, `build_otel_layer`)
- `docs/observability.md`; `SPEC.md` §V (V8, V13, V14, V15, V18)
- `charts/pharos/values.yaml` (`config.obs`, `serviceMonitor`)
