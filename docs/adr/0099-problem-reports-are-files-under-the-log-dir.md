# ADR-0099: Problem reports are JSON files under the log dir, kept 30 days

- **Status:** Proposed
- **Date:** 2026-11-09T23:40:00Z
- **Deciders:** Alison (owner), Claude (implementation)

## Context

Spec 016 US5 asks for a "Report a problem" button that bundles what the client
knows (route, breadcrumbs, recent events, player state, trace ids) and uploads
it through `Telemetry.report`. FR-008 says reports are kept in the same
`<log_dir>/clientlog/` area `POST /ClientLog/Document` uses, deleted after 30
days (configurable), and readable only by admins and the reporter. pharos is a
public server, so the call must not let anyone fill its disk, and the free text
in a bundle must not leak credentials (V8, FR-013).

The same audit found that `Telemetry.submit` received `ClientSpan`s and
dropped them (US1). The OpenTelemetry SDK 0.33 always mints a span's id, and a
root span's trace id, itself, so a browser span re-exported naively would lose
the ids the server's own spans already name as their parent.

## Decision

We will store each report as one JSON document, `<id>.json`, in
`[obs].report_dir`, defaulting to `<log_dir>/clientlog/reports`. With neither
set the store is absent and `report` answers `unavailable` (counted as
`no_store`), never silently dropping a report.

- The id is 8 Crockford base32 characters, `XXXX-XXXX` (40 random bits), easy
  to read aloud; only an id in exactly that shape names a file, so an id never
  carries a path.
- A bundle is a UTF-8 JSON object of at most 256 KiB
  (`specs/016-ui-observability/contracts/report-bundle.md`). The server
  redacts it again whatever the client did: credential-named keys,
  token-shaped strings and URLs with credential query params become
  `<redacted>`, strings are cut to 2 KiB and nesting past 16 levels is dropped.
- At most 20 stored reports per user and 500 in all; at the cap `report`
  answers `limit`.
- An hourly leader-only sweep deletes reports older than
  `[obs].report_retention_days` (default 30), logging and counting each
  (`pharos_ui_reports_expired_total`).
- `report` is not gated by the diagnostics opt-in, because the user pressed it
  after seeing what it sends (US9). `listReports`, `getReport` and
  `deleteReport` (schema 35) answer the reporter, or an admin for anyone;
  anyone else is told the report does not exist.

For client spans, the tracer provider's id generator returns ids forced by
`client_spans::forward` while it starts a re-exported span, and random ids
otherwise. The span then flows through the server's normal sampler and batch
exporter. Only opted-in users' spans are forwarded (US9).

## Consequences

- No migration, no new store trait, and nothing in Postgres: reports work on
  either backend and a backup of the log dir carries them.
- Listing reads every document (the summary skips the bundle without keeping
  it), bounded by the 500-report cap at 256 KiB each. Fine for an admin page;
  a much larger cap would want an index.
- With several replicas, the report dir must be shared for a report to be
  readable from every replica; today the deployment runs one.
- Two reports racing past the cap check can overshoot it by the number in
  flight; acceptable for a per-user cap of 20.
- Deployments must set `[obs].log_dir` or `[obs].report_dir` for reports to be
  kept. The chart already defaults `config.obs.logDir` to
  `/var/lib/pharos/cache/logs`, so a chart deploy keeps them on the cache PVC
  under `clientlog/reports`; recreating that PVC loses them.
- The forced-id generator relies on the SDK asking the generator for exactly
  one span id (and one trace id for a root) per span start; an SDK upgrade
  that changes that fails `client_spans` tests rather than mis-parenting
  silently.

## Alternatives considered

- **A database table.** Indexed listing and per-user queries, but a migration
  on both backends, a store trait, and large blobs in SQLite for a feature used
  a few times a month. FR-008 already named the clientlog area.
- **Gate reports on the diagnostics opt-in.** Rejected by US9: the opt-in
  governs automatic telemetry; a report is an explicit, previewed action.
- **Export client spans by building `SpanData` and calling an exporter
  directly.** Needs a second exporter and its own batching, and bypasses the
  sampler, so client and server spans of one trace could be kept
  inconsistently.

## References

- `specs/016-ui-observability/spec.md` US1, US5, US9, FR-005, FR-008, FR-013
- `crates/pharos-server/src/client_reports.rs`, `crates/pharos-server/src/client_spans.rs`
- `crates/pharos-server/src/api/pharos/rpc/report.rs`
- Invariants V1080–V1085
