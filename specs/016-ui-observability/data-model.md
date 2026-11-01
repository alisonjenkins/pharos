# Data model: 016-ui-observability

## ConsentState (client)

`Unknown | Enabled{sampling: f32} | Disabled`. Starts `Unknown`; set from the
`telemetry.enabled` pref after login. Only `Enabled` sends. Transitions are
logged locally (overlay) and, when entering `Enabled`, as one event.

## ClientEvent

`{ kind: EventKind, level: Error|Warn|Info, mono_ms, wall_ms, trace_id?,
span_id?, route, action?, ids: {item?, play_session?, group?, device},
fields: BoundedMap<Key, Value>, repeat: u32 }`. `EventKind` is a closed enum
(contracts/events.md). Unknown kinds are rejected by the server and counted.
`fields` holds ≤ 16 entries, each value ≤ 512 bytes, after redaction.

## ClientSpan

`{ trace_id, span_id, parent_span_id?, name: SpanName, start_wall_ms,
duration_ms, status: Ok|Error{message}, attrs: BoundedMap }`. `SpanName` comes
from the action registry, route enum or `rpc.<iface>.<method>`.

## Breadcrumb

`{ mono_ms, kind: Route|Action|Rpc|Media|Error, label }`. A ring of 200; only
labels, never payloads.

## PlaybackTimeline (per PlaySessionId)

An ordered list of `{ mono_ms, kind: Decision|Rendition|Buffer|Stall|Switch|Seek|Error|TrackChange, data }`.
It is sent at session end or on error (if Enabled), and is always visible in
the overlay.

## ProblemReport

`{ report_id, user_id, created_at, app_version, build_hash, browser, gpu,
renderer_mode, route, breadcrumbs, recent_events, timeline?, ws_state,
cache_stats, trace_ids, note }`, stored per R6. It is deleted at 30 days
(configurable) or by its owner or an admin.
