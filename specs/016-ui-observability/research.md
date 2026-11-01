# Research: 016-ui-observability

## R1 — where the client facility lives

- **Decision**: split it in two.
  - `pharos-client::telemetry` (platform-neutral, reused by future apps):
    event API, breadcrumb ring, batching, consent gate, sampling, offline
    queue, redaction, trace-context generation, per-call RPC instrumentation
    (017 V173 client side).
  - `pharos-ui::telemetry` (browser-specific capture): wasm panic hook,
    `window.onerror` / `unhandledrejection`, WebGL context loss,
    `HTMLMediaElement`/MSE and hls.js errors (via 013's `PlaybackEngine` port
    events), CSP violations, long frames, route paint timing, and the debug
    overlay.
- **Rationale**: apps should not reimplement batching, consent or redaction.

## R2 — consent gate (US9, Clarifications)

- **Decision**: the consent flag is a per-user pref (`telemetry.enabled`,
  default `false`), read through 017 `Prefs`. Until the flag is known (before
  login, or before the first `Prefs` read) the facility is in `Unknown` and
  **buffers nothing for sending**: events go to the local overlay ring only.
  `Enabled` starts sending, and `Disabled` drops the send queue at once.
  "Report a problem" bypasses the gate only on an explicit press, after showing
  the bundle.
- The admin's anonymous pre-login channel (FR-005a) is advertised by
  `Bootstrap.serverInfo` (an `anonymousErrors: Bool` field appended to the
  schema), so a client never calls it when it is off.

## R3 — forwarding browser spans to Tempo (Clarifications Q1)

- **Finding**: `obs.rs:329` builds an `opentelemetry_otlp::SpanExporter` in a
  batch processor (opentelemetry 0.33). The SDK's `SpanExporter::export` takes
  `Vec<SpanData>`, so spans that did not originate in-process can be exported
  by building `SpanData` directly.
- **Decision**: `Telemetry.submit` accepts client spans (trace id, span id,
  parent, name, start/end, bounded attributes). The server validates them
  (trace and span id format; name from the allow-listed action/route set;
  attribute count and size caps), stamps `service.name = "pharos-ui"`, and
  hands them to a `ClientSpanSink` trait. The prod implementation pushes into a
  second batch processor on the same exporter. A null sink is used when OTLP is
  unconfigured.
- **Rejected**: the browser posting OTLP to the collector directly (Clarifications Q1).

## R4 — trace context propagation

- **Decision**: a W3C trace context is minted per user action from the 011
  action registry (and per route change and play session) in
  `pharos-client::telemetry`. It is set on 017's `CallContext.traceparent` for
  RPC, and as a `traceparent` header on the plain-HTTP media/image requests the
  UI controls (hls.js `xhrSetup`/`fetchSetup` hooks via 013's port; `<img>`
  cannot carry headers, so image requests are correlated by the action's
  trace id in the client event only). The server already continues an incoming
  `traceparent` on HTTP (`docs/observability.md`) and does so for RPC per 017
  FR-006.

## R5 — redaction on the client

- **Decision**: a `Secret<T>` newtype in `pharos-client` with no `Serialize`
  and a redacted `Debug`; a URL scrubber that strips `api_key`, `ApiKey`,
  `X-Emby-Token`, `tag`, `PlaySessionId` (when used as auth), `token` from query
  strings before a URL enters any event; fields named in the 017 schema with
  `$secret` are redacted by generated code. A property test serialises every
  event kind with seeded secrets and asserts none survive (SC-003).

## R6 — report storage and retention (Clarifications Q3)

- **Decision**: reports are JSON documents under `<log_dir>/clientlog/reports/`
  named `<report_id>.json`. `report_id` is a 10-char base32 random id, short
  enough to paste. An index row in the store holds `(report_id, user_id,
  created_at, size)` for access control. A daily background task deletes
  reports older than `[obs].client_report_retention_days` (default 30) under
  the adaptive I/O gate (ADR-0017), logging and counting each expiry.
- Access is enforced server-side: `Admin.reports*` for admins, and
  `Telemetry.myReports` / `deleteReport` scoped to the caller.

## R7 — dashboard

- **Decision**: a Grafana dashboard JSON at `deploy/grafana/pharos-ui.json`,
  provisioned alongside existing dashboards if the chart already ships some;
  otherwise documented for import. (Check during tasks; not assumed.)
