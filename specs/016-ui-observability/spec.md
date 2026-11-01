# Feature Specification: end-to-end observability for pharos-ui

**Feature directory**: `specs/016-ui-observability/`
**Created**: 2026-11-01
**Status**: draft (`/speckit-specify` stage; next: `/speckit-clarify`)
**Input**: "A key thing that will help us with development, I want UI side
observability to enable us to debug from both server and client side so we
never have to guess what a problem is." (Alison, 2026-11-01)
**Depends on**: ADR-0013 (tracing + Prometheus + OTLP), `docs/observability.md`,
CLAUDE.md §ODD, `POST /ClientLog/Document` (B66), 009 (data layer, websocket,
renderer), 013 (player)
**Transport**: ingest and call context ride the pharos-native API (017,
ADR-0028): `Telemetry` interface and the per-call context of 017 FR-006.
**Cross-cuts**: 009–015. Each of those specs' "Observability" / instrumentation
FRs (009 FR-012, 013 FR-010, 014 FR-005, 015 FR-005) is implemented through the
facility defined here. This spec should land **with 009, before feature
pages**, so every later screen is born instrumented.

## Goal

Any problem a user sees in pharos-ui can be diagnosed by query, from either
end, without reproducing it and without guessing:

- from the **server** side, a slow or failed request leads to the exact client
  session, screen and user action that caused it;
- from the **client** side, a stall, error or wrong render leads to the server
  span, log lines and decision reasons behind it;
- the client states its own state (route, renderer mode, player buffer,
  websocket, cache) rather than leaving us to reconstruct it.

The CLAUDE.md principle applies to the browser as well: *the failure path
records as much as the success path, and a reason names the offending value.*

## Clarifications

### Session 2026-11-01

- Q: Telemetry default? → A: Off for every user, admins included; users opt in (recorded earlier in US9).
- Q: Trace context headers with telemetry off? → A: Still sent; they carry ids and the route only (US9).
- Q: How do browser spans reach Tempo? → A: Through pharos (`Telemetry.submit`, forwarded by the server's OTLP exporter); the collector stays private.
- Q: With telemetry off by default, what happens to errors on the login screen? → A: Nothing by default; an admin can enable an anonymous, error-only, rate-limited pre-login channel (FR-005a).
- Q: How long are problem reports kept, and who sees them? → A: 30 days (configurable); admins see all, reporters see and can delete their own.

## What exists today

- The server continues an incoming sampled W3C `traceparent` (parent-based
  sampling) and exports spans to Tempo over OTLP when `[obs].otlp_endpoint` is set.
- Logs are JSON to stdout → Loki `{namespace="pharos"}`.
- `POST /ClientLog/Document` stores a client-uploaded text blob under
  `<log_dir>/clientlog/` (built for native apps' "Submit logs").
- pharos-ui itself emits nothing: no trace headers, no error reporting, no
  timings, no client logs.

## User Stories

### US1 — one trace from click to encoder (P1)

Every request pharos-ui makes carries a `traceparent` whose trace starts at the
user action (click, key, palette command, route change) as a client span. The
server's spans join that trace, so Tempo shows one waterfall: `ui.action
"play"` → `PlaybackInfo` → negotiation → `master.m3u8` → segment produce →
transcode device.

**Independent test**: click Play in a test build; fetch the trace by id from
Tempo (or the in-test OTLP collector) and assert it contains both the client
action span and the server `http.request` spans for PlaybackInfo and the first
segment.

### US2 — client telemetry reaches Loki and Prometheus (P1)

pharos-ui sends structured client events (errors, warnings, and selected info
events) to a pharos ingest endpoint; the server re-emits them as structured
`tracing` events with `source="ui"` so they land in Loki next to server logs,
and counts them in bounded-label Prometheus series.

**Independent test**: force a fetch failure in a test build; the Loki query
`{namespace="pharos"} | json | source="ui" | event="request_failed"` returns a
line naming endpoint, status, trace id and route; `pharos_ui_events_total{event="request_failed"}`
increments.

### US3 — every failure says why, with the value (P1)

Every client error event carries: what was attempted, the input that mattered
(item id, route, track index, codec string, theme name — never a secret), the
error class AND the concrete message/status, the trace id, and the recent
breadcrumb trail. A reason that does not name the value is a bug against this
spec.

### US4 — playback is explainable after the fact (P1)

The player records a timeline per `PlaySessionId`: decision received (method +
`direct_play_block` / `downgrade` reasons), source/rendition chosen, buffer
level samples, stalls (start, duration, buffered ranges, network state),
quality switches with reason, decode errors with codec, dropped frames, seeks
(from → to, latency to first frame), subtitle/audio switches. Summary metrics
go to Prometheus; the timeline goes to Loki at session end or on any error, and
is shown live in the 013 stats overlay.

**Independent test**: throttle the network in Playwright until a stall occurs;
the Loki session timeline has a `stall` event with duration and buffered
ranges, and `pharos_ui_playback_stall_seconds` has an observation.

### US5 — "Report a problem" captures everything (P1)

A button (shell menu, player overlay, palette action "Report a problem", and a
key `Ctrl+Shift+D`) bundles: app version + build hash, browser + GPU + renderer
mode, route + recent breadcrumbs (≈200), last N client events, current player
timeline, websocket state, cache stats, active trace ids, and an optional user
note. It uploads through `Telemetry.report` (FR-008), returns a short report id the
user can paste, and logs a server line linking report id → user → session →
trace ids.

**Independent test**: trigger a report; `GET` the admin view of client reports
(015 US7) lists it; the Loki line with its id links to the trace ids inside it.

### US6 — debug overlay for development (P1)

A toggleable overlay (`Ctrl+Shift+O`, or `?debug=1` in the URL, admin or dev
build only) showing live: route and focus zone, renderer mode and frame time
graph, GPU texture memory, request waterfall with trace ids (click to open in
Grafana/Tempo when a Grafana base URL is configured), websocket messages in/out,
cache hit/miss, player buffer graph and decision, client event log, and the
current theme tokens.

### US7 — server-side view of clients (P2)

Admin dashboard (015 US4) lists connected pharos-ui sessions with app version,
renderer mode, current route, websocket state, and recent client errors; a
session row links to its traces and its Loki logs.

### US8 — performance and UX timings (P2)

Web-vitals style timings per route (time to first paint from cache, time to
fresh data, input-to-paint latency), long frames over budget with the active
renderer layer, image decode/upload times, and slow interactions (> 200 ms
input-to-paint) with the action id from the 011 registry.

### US9 — privacy and control (P1)

**Telemetry is OFF by default for every user, admins included; users opt in**
(decided by Alison, 2026-11-01). Concretely:

- **Opt-in governs** everything sent through `Telemetry.submit`: client events,
  errors, client spans, playback timelines, performance timings (US2, US4,
  US8). With it off, the client sends none of them and keeps nothing queued.
- **Always available regardless of the setting**, because it is local or
  explicitly user-initiated: the debug overlay (US6, local only) and
  "Report a problem" (US5, uploads only when the user presses it, after
  showing what it will send).
- **Trace context** (017 per-call context, `traceparent` on media requests) is sent
  regardless: they carry ids and the current route only, no client data, and
  only correlate the user's own requests in the server's existing logs.
  (Confirmed by Alison, 2026-11-01.)
- Settings → Privacy shows the toggle, a plain description of what is sent,
  and a live view of the last events (the debug overlay's event log).
- Admins may set a server-wide **sampling ratio** for opted-in users and may
  turn ingest off entirely; they cannot opt a user in.
- Telemetry contains no tokens, passwords, cookies or API keys (V8 extended to
  the client, FR-013); no third-party endpoint is ever contacted.

**Independent test**: a fresh user browses and triggers a fetch failure; zero
`Telemetry.submit` calls are made. After opting in, the same failure
produces one event.

## Functional Requirements

### Correlation

- **FR-001** Client trace context: W3C Trace Context generated in the browser;
  one root span per user action from the 011 action registry, per route
  change, and per playback session; every REST call and websocket message
  carries the current trace context (the 017 per-call context; `traceparent`
  header on plain-HTTP media/image requests).
- **FR-002** The server parents each `rpc.*` span on the call context (017
  FR-006) and attaches `ui.session_id`, `ui.route` and `ui.action` (bounded,
  validated) to the span and to metric exemplars (not labels). Plain-HTTP media
  requests keep using the existing `traceparent` continuation.
- **FR-003** Every server error response to pharos-ui includes the trace id
  (`traceresponse` header or body field), so the client error event and the
  server span share an id even when sampling dropped the trace.
- **FR-004** IDs that already exist are reused, not reinvented: `PlaySessionId`,
  `DeviceId`, group id, request id. Every client event carries those that apply.

### Ingest

- **FR-005** `Telemetry.submit` on the native API (017; batched, size-capped,
  rate-limited per device, on the `Session` capability, plus the pre-login
  error channel below) accepting typed event
  batches. Events are re-emitted as `tracing`
  events (`source="ui"`) and, where the client exported spans, forwarded as
  OTLP spans through the server's existing exporter so the browser never needs
  to reach Tempo directly. The collector is never exposed to browsers.
- **FR-005a** Pre-login error channel: an anonymous, error-level-only
  `Telemetry.submitAnonymous` on the bootstrap interface, **off by default**
  and enabled only by an admin setting. When on: errors only (no info events,
  timings or spans), no user or device identifiers beyond a random per-page id,
  strict per-IP rate limit, and the login screen states that anonymous error
  reports are enabled. When off, the call is not offered and nothing is sent;
  the login screen still has "Report a problem" (US5), sent only on press.
- **FR-006** Event schema lives in the 017 Cap'n Proto schema, append-only; unknown
  event kinds are counted and dropped, not stored.
- **FR-007** Prometheus series from client events, bounded: `pharos_ui_events_total{event,level}`,
  `pharos_ui_request_failures_total{endpoint_pattern,status_class}`,
  `pharos_ui_renderer_mode_total{mode,reason}`,
  `pharos_ui_ws_reconnects_total{reason}`,
  `pharos_ui_playback_stall_seconds` (histogram), `pharos_ui_playback_start_seconds`
  (histogram, by `method`), `pharos_ui_route_paint_seconds{route}`,
  `pharos_ui_long_frames_total{layer}`. Label values come from `label()`
  methods with a distinctness test (CLAUDE.md rule). Endpoint label is the
  route pattern, never the concrete URL.
- **FR-008** Client reports (US5) are submitted with `Telemetry.report` (017)
  and stored in the same `<log_dir>/clientlog/` area `ClientLog/Document` uses,
  as structured documents keyed by report id. **Retention**: deleted
  automatically after 30 days (configurable by an admin); expiry is logged and
  counted (`pharos_ui_reports_expired_total`). **Access**: admins list,
  download and delete all reports; a reporter sees, downloads and deletes their
  own (Settings → Privacy). Nobody else can read them.

### Client facility

- **FR-009** A `telemetry` module in pharos-ui: event API (`ui_event!` macro with
  level, kind, fields), breadcrumb ring buffer, batching with flush on
  interval / size / `visibilitychange` (`sendBeacon`), offline queue capped by
  size, sampling for info-level events, always-send for errors.
- **FR-010** Automatic capture: wasm panics (panic hook with message + location
  + recent breadcrumbs), unhandled JS errors and promise rejections, failed
  fetches (status, endpoint pattern, duration, trace id), websocket close codes,
  WebGL context loss, MSE/`HTMLMediaElement` errors with `MediaError.code` +
  message, hls.js error events with type/details/fatal, CSP violations.
- **FR-011** Decision events on the client mirror the server rule — any branch
  choosing between behaviours records inputs, verdict and reason: renderer
  mode, DeviceProfile probe results, quality/ABR switches, subtitle renderer
  choice, theme fallbacks (missing font, effect downgrade, 010 FR-007),
  focus recovery (011 edge case), cache served-stale vs fresh.
- **FR-012** Debug overlay per US6, compiled into release builds but gated by
  admin role or dev build, zero cost when closed.
- **FR-013** Redaction: a client `Secret` newtype (no `Serialize` without
  explicit `.expose()`), URL scrubbing of `api_key`, `ApiKey`, `X-Emby-Token`,
  `tag` and `PlaySessionId`-as-auth query params before any URL enters an event,
  with a test that serialises every event kind with secrets present and asserts
  none survive.

### Dashboards and runbook

- **FR-014** A Grafana dashboard JSON committed alongside the chart: client
  error rate by event, request failures by endpoint, playback start time and
  stall rate by method (pharos-ui vs jellyfin-web vs native from server
  signals), renderer fallback rate, websocket health, with links from panels to
  Loki/Tempo queries.
- **FR-015** `docs/observability.md` gains a "UI" section: the named queries for
  each common symptom (blank screen, won't play, buffering, out of sync, wrong
  theme, slow screen), the event catalogue, and how to go from a report id to
  traces.

## Success criteria

- **SC-001** Click-to-segment trace (US1) visible in Tempo on the dev stack
  (Tilt) and asserted in an integration test with an in-process OTLP collector.
- **SC-002** Each failure class in FR-010 is provoked in a Playwright test and
  found by its named Loki query; each test is verified to FAIL with the
  instrumentation disarmed (CLAUDE.md ODD step 3).
- **SC-003** Redaction test (FR-013) passes for every event kind; a manual grep of
  one day of `source="ui"` Loki lines for `api_key|Token|Password` returns nothing.
- **SC-006** Default-off verified: a Playwright run with a fresh user records zero `Telemetry.submit` calls (US9 test).
- **SC-004** Telemetry overhead: ≤ 1 % main-thread time and ≤ 20 KB/min upload
  for an idle-browsing session; batching verified under offline/online flips.
- **SC-005** After deploy, the named queries in FR-015 are run and their real
  output recorded (ODD step 5).

## Edge cases

- Server unreachable: events queue (capped), flush on reconnect, oldest dropped
  first with a count of dropped events sent when back.
- Pre-login failure (login page broken): diagnosed from server logs, a
  pressed "Report a problem", or, when an admin has enabled it, the FR-005a
  anonymous error channel.
- Clock skew between browser and server: events carry client monotonic time
  plus wall time; server stamps receive time; the timeline orders by monotonic.
- A noisy client loop (error every frame): client-side dedupe with a repeat
  count; server rate limit counts what it drops.
- Sampling dropped the trace: FR-003 still ties client error ↔ server log by
  trace id.

## Out of scope

Third-party RUM/APM services. Session replay (screen recording). Telemetry from
jellyfin-web or native apps (server-side signals already cover them).
