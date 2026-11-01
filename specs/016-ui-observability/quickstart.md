# Quickstart / validation: 016-ui-observability

Inside `nix develop`, with the Tilt dev stack for Tempo/Loki (`just tilt-up`).

1. **Default off** (SC-006): `just compat-dioxus` (the "telemetry default off" spec; run it alone with `npx playwright test -c playwright.dioxus.config.ts -g "telemetry default off"` in `compat-playwright/`).
   A fresh user browses and provokes a fetch failure; zero `Telemetry.submit`
   calls are recorded by the RPC tap.
2. **Opt in** (Settings → Privacy) and repeat. Expect one `rpc_failed` event, then:
   `{namespace="pharos"} | json | source="ui" | event="rpc_failed"` returns it
   with `trace_id`, `route` and the failing method.
3. **One trace** (SC-001): click Play on a seeded item; open the trace id from
   the overlay in Grafana/Tempo. Expect `ui.action play` → `rpc.playback.negotiate`
   → `http.request` for `master.m3u8` → segment spans.
4. **Redaction** (SC-003): `cargo nextest run -p pharos-client telemetry::redaction`.
5. **Report**: press `Ctrl+Shift+D`. Expect a report id; it is listed in
   Manage → Reports and in the reporter's Settings → Privacy. Expiry:
   `cargo nextest run -p pharos-server client_reports_expire_after_retention`.
6. **Anonymous channel off by default**: `Bootstrap.serverInfo.anonymousErrors == false`,
   and `submitAnonymous` returns `Unimplemented`.
7. **After deploy**, run and record:
   `sum by (event,level) (rate(pharos_ui_events_total[1h]))`,
   `sum by (reason) (rate(pharos_ui_events_rejected_total[1h]))`.
