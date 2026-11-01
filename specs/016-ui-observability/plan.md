# Implementation Plan: end-to-end observability for pharos-ui

**Branch**: `016-ui-observability` | **Date**: 2026-11-01 | **Spec**: [spec.md](./spec.md)
**Input**: `specs/016-ui-observability/spec.md` (clarified 2026-11-01)
**Depends on**: 017 (transport, `CallContext`, `Telemetry` interface, V173 server instrumentation), 009 (shell, routes, renderer)

## Summary

Make every UI problem diagnosable by query from either end. The client mints
the trace context per user action and carries it on every RPC and controllable
HTTP request. The server joins it, so one Tempo trace runs from click to
encoder. Client events, spans and playback timelines go to pharos only when the
user has opted in (default off). pharos re-emits them into Loki and Prometheus
and forwards spans through its existing OTLP exporter. "Report a problem"
bundles state on demand, kept 30 days. A debug overlay shows everything
locally. The facility's core lives in `pharos-client` for reuse by future apps.

## Technical Context

**Language/Version**: Rust stable; wasm32 client
**Primary Dependencies**: existing `tracing`, `metrics`, `opentelemetry`/`_sdk`/`-otlp` 0.33 (R3), `pharos-api`/`pharos-client` (017). No new third-party dependencies are planned.
**Storage**: report files under `<log_dir>/clientlog/reports/` plus an index row per report (store migration, sqlite + postgres)
**Testing**: unit (redaction property test, consent state machine, batching); server integration (`it_rpc`: submit → Loki-shaped `tracing` capture, metrics, OTLP via an in-process collector stub); Playwright (default-off zero-calls, failure classes found by named query, report flow, overlay)
**Target Platform**: as 009
**Performance Goals**: ≤ 1 % main-thread time and ≤ 20 KB/min upload when idle (SC-004)
**Constraints**: default off (US9); no secrets (V8, R5); bounded labels; failure path at least as rich as success
**Scale/Scope**: one household; ingest rate-limited per device

## Constitution Check

| Gate | Status |
|---|---|
| III. ODD | PASS. This feature is ODD for the client. SC-002 requires each instrumentation test to fail when disarmed |
| IV. No leaks (V8) | PASS. Client `Secret`, URL scrubber, `$secret` schema fields, property test (R5) |
| V. Traits at IO edges | PASS. `TelemetrySink` (client), `ClientSpanSink` and report store (server) |
| Tech: observability baked in, labels a dashboard contract | PASS. Label sets in contracts/events.md, asserted distinct |
| V16 / V172 / V173 | PASS. Rides 017's native API and its instrumented dispatch |

No violations. Re-checked after design: unchanged.

**ADR when the feature lands**: client telemetry forwarded through the server's
OTLP exporter, default-off consent model, 30-day report retention.

## Project Structure

```text
specs/016-ui-observability/
├── spec.md plan.md research.md data-model.md quickstart.md
└── contracts/events.md

crates/pharos-client/src/telemetry/     # consent, events, breadcrumbs, batching, redaction, trace ctx
crates/pharos-ui/src/telemetry/         # browser capture, overlay (gated), report UI
crates/pharos-api/schema/pharos.capnp   # Telemetry + Admin.reports methods appended
crates/pharos-server/src/services/telemetry.rs   # validate → tracing + metrics + ClientSpanSink
crates/pharos-server/src/services/client_reports.rs  # store, access, retention task
crates/pharos-store-sqlx/migrations/    # client_reports index (sqlite + postgres)
deploy/grafana/pharos-ui.json           # if the deploy layout supports it (R7)
docs/observability.md                   # UI section: named queries, catalogue, report-id → traces
```

## Phases (outline for `/speckit-tasks`)

1. **Consent + redaction first**: state machine, `Secret`, URL scrubber, property test. Default-off Playwright test (SC-006) goes red, then green.
2. **Trace context**: per-action contexts in `pharos-client`; `CallContext` on every call; Tempo trace test with an in-process collector (SC-001).
3. **Ingest**: `Telemetry.submit` validation, re-emit, metrics, rejections; the `ClientSpanSink` OTLP path.
4. **Browser capture**: each FR-010 class, each with a Playwright provocation plus its named-query assertion, and disarm verification (SC-002).
5. **Playback timeline**: subscribe to 013's `PlaybackEngine` port events (stubbed until 013); the summary metrics.
6. **Reports**: bundle, upload, index + files, access rules, retention task, admin and self views.
7. **Overlay**: gated by admin/dev, zero cost when closed.
8. **Anonymous channel**: admin toggle, `serverInfo` advertisement, errors-only enforcement.
9. **Docs + dashboard + by-query verification** (SC-005).
