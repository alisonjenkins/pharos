# Contract: the "Report a problem" bundle (016 US5, ADR-0099)

`Telemetry.report(ctx, note, bundle)` carries the user's optional `note` and a
`bundle`: a UTF-8 JSON **object** of at most 256 KiB. The client builds it when
the report sheet opens and shows it to the user verbatim before Send; what is
shown is what is sent.

## Keys

All keys are optional; the server stores whatever object it receives, after
redaction. Unknown keys are kept.

| key | type | what |
|---|---|---|
| `schema` | string | `"pharos.report/1"` |
| `capturedAtMs` | number | client wall time the bundle was built |
| `app` | object | `version`, `build` |
| `browser` | object | `userAgent`, `renderer` (render mode) |
| `route` | string | current route; logged on the server's report line |
| `breadcrumbs` | array | last ≤ 200 `{wallMs, text}` (route changes, actions) |
| `events` | array | last ≤ 50 client events: `kind`, `level`, `route`, `fields` |
| `player` | object | current player timeline, when playing |
| `traceIds` | array of string | W3C trace ids (32 lowercase hex) of recent calls; the server keeps up to 64 distinct valid ones and logs them |

## Server handling

- Refused: not JSON → `invalid` (`bundle`); not an object → `invalid`; over
  256 KiB → `limit` (`report_bundle_bytes`).
- Redacted before storing: the value of any key naming a credential
  (`token`, `password`, `secret`, `auth`, `cookie`, `credential`, `apikey`,
  `api_key`, case-insensitive substring), any token-shaped or `Bearer`/`Basic`
  string, and any URL whose query names a credential or `tag`.
- Strings cut to 2 KiB with their length noted; nesting past 16 levels is
  replaced by `"<too deep>"`.
- Stored document: `id`, `receivedAt` (ISO 8601 UTC), `receivedWallMs`,
  `owner`, `userName`, `device`, `note` (≤ 4 KiB), `traceIds`, `bundle`.

## Signals

- Loki: `{namespace="pharos"} |= "client problem report stored"` has
  `report_id`, `user`, `device`, `route`, `trace_ids`; refusals say
  `client problem report refused` / `not stored` with `outcome` and `error`.
- Prometheus: `pharos_ui_reports_total{outcome}` (`stored`, `invalid_bundle`,
  `too_large`, `rate_limited`, `no_store`, `store_full`, `user_full`,
  `store_failed`), `pharos_ui_reports_expired_total`.
