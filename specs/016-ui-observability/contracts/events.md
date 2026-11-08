# Contract: client event catalogue and server signals (016)

## Event kinds (closed set; append-only like the 017 schema)

| kind | level | when | required fields |
|---|---|---|---|
| `panic` | error | wasm panic hook | message, location |
| `js_error` | error | `window.onerror` / unhandled rejection | message, source, line |
| `rpc_failed` | error/warn | 017 call returned error or disconnected | interface, method, outcome, reason |
| `http_failed` | error/warn | media/image/session HTTP failure | endpoint_pattern, status |
| `ws_closed` | warn | RPC connection closed | code, reason, duration_ms |
| `renderer_mode` | info | mode decided or changed | mode, reason |
| `webgl_context_lost` | warn | context lost or restored | phase |
| `long_frame` | warn | frame > budget | layer, ms |
| `route_paint` | info | route rendered | route, from_cache, ms |
| `slow_interaction` | warn | input-to-paint > 200 ms | action, ms |
| `playback_decision` | info | negotiate result | method, direct_play_block, downgrade |
| `playback_stall` | warn | stall ended | duration_ms, buffered_ranges, network_state (Rust HLS engine adds engine, position_ms, forward_ms, end, last_ttfb_ms, last_load_ms; V693) |
| `playback_error` | error | media/MSE/engine error | code, message, codec |
| `quality_switch` | info | ABR or user switch | from, to, reason |
| `seek` | info (warn when abandoned or slower than 3 s) | seek completed | from_ms, to_ms, latency_ms (Rust HLS engine adds engine, seeked_ms, in_buffer, superseded, paused, end, segments, bytes, first_index, first_ttfb_ms, first_load_ms, first_appended_ms; V694) |
| `theme_fallback` | warn | font/effect downgrade (010) | what, wanted, used |
| `focus_recovered` | info | focus target vanished (011) | zone |
| `cache_served` | info (sampled) | query served stale vs fresh | key_kind, stale |
| `csp_violation` | warn | CSP report | directive, blocked |
| `group_drift` | info (sampled) | playhead vs group clock while playing (014, 1 per 10 s) | group, drift_ms |
| `group_hard_seek` | warn | drift past the hard-seek threshold (014) | group, drift_ms |
| `group_wait` | info | time spent waiting on other members (014) | group, wait_ms |
| `playback_resume` | info (warn on timeout) | a paused song resumed, once it sounded or after 5 s (B563) | click_to_playing_ms, playing_to_audible_ms, ready_state, buffered_ahead_ms, context_state, base_latency_ms, output_latency_ms, context_resumed, engine |
| `playback_start` | info (warn when abandoned or slower than 5 s) | Rust HLS engine: attach to the first `playing`, or the player closing first (031, V691) | engine, startup_ms, start_ms, end, segments, first_index, first_ttfb_ms, first_load_ms, first_appended_ms |
| `playback_summary` | info (warn with stalls, quota or refused switches) | Rust HLS engine: every 5 min of segments and when the engine goes (031, V692) | engine, reason, window_ms, video_segments, audio_segments, bytes, kbps, ttfb_p50_ms, ttfb_max_ms, load_max_ms, buffer_min_ms, buffer_mean_ms, stalls, stall_ms, seeks, seek_max_ms, switches, refused_switches, quota |

## Server-side signals produced from them (labels bounded via `label()`)

`pharos_ui_events_total{event,level}`,
`pharos_ui_request_failures_total{endpoint_pattern,status_class}`,
`pharos_ui_renderer_mode_total{mode,reason}`,
`pharos_ui_ws_reconnects_total{reason}`,
`pharos_ui_playback_stall_seconds` (histogram),
`pharos_ui_playback_start_seconds{method}` (histogram),
`pharos_ui_route_paint_seconds{route}` (histogram),
`pharos_ui_long_frames_total{layer}`,
`pharos_ui_events_rejected_total{reason}` (unknown kind, oversize, rate limit, consent mismatch),
`pharos_ui_reports_total`, `pharos_ui_reports_expired_total`,
`pharos_ui_anonymous_events_total{outcome}`.

Loki: each accepted event becomes one `tracing` event with `source="ui"`,
`event=<kind>`, the ids, `trace_id`, and the fields.

## RPC methods (appended to 017's schema)

`Telemetry.submit(ctx, events, spans)`, `Telemetry.report(ctx, note, bundle) -> reportId`,
`Telemetry.myReports() / deleteReport(id)`, `Admin.reports(...) / report(id) / deleteReport(id)`,
`Bootstrap.submitAnonymous(ctx, events)` (errors only; when advertised).
