# G4 reproduction and signals (T038-400)

What each G4 problem looks like today, and the query that shows it. Read
against `main` at 3247b2f. Times are UTC.

## A denial with no trace id (FR-404)

The browser UI sends every native call with an empty `CallContext.traceparent`
(`pharos-client/src/lib.rs` `CallInfo::default`), and the server never writes
`Error.traceId` (`pharos-api/src/instrument.rs` `Denial::write`). So a failure
panel has no id to show, and the server's failure line has none to be found by.

- Shows it now: `{namespace="pharos"} |= "rpc call failed" | json | trace_id=""`
  returns every failed browser call.
- Proves the fix: the id printed on a panel (`Details: … (trace <id>)`) finds
  its line with `{namespace="pharos"} | json | trace_id="<id>"`, and the query
  above returns nothing for a client built after the fix.

## One caption for every wait (FR-401, FR-402)

`watch_view.rs` renders "Waiting for the server…" whenever `media.loading` is
set and no seek card is due: on the first frame, on a seek, on a buffer
underrun mid-play, behind a full encoder queue and while a group waits for a
member. The player cannot tell a saturated encoder from a slow network,
because `queue_wait_ms` reaches only the server's log, after the segment is
done (`pharos-transcode/src/scheduler.rs`, the `transcode job done` line).

To reproduce the saturated case: start more transcoding plays than
`sum(pharos_transcode_device_capacity)`, then seek in one of them.

- Saturation, server-side (exists):
  `sum(pharos_transcode_device_in_use) >= sum(pharos_transcode_device_capacity)`
  with `pharos_transcode_pending_by_class{class="interactive"} > 0` and
  `pharos_transcode_pending_oldest_seconds` climbing.
- What the viewer was told (missing; added by T038-402/403):
  `{namespace="pharos"} |= "playback queue hint: waiting for an encoder"`
  carries `queued`, `waited_ms`, `others_queued` and `busy` for the play
  session the caption was about, and the browser console line
  `pharos: wait caption cause=<cause>` names the caption chosen and why.

## Up Next over the ending without detected credits (FR-405, FR-406)

`playback.rs` `up_next_at` falls back to 1/20 of the runtime (20 to 90 s
before the end) when the server found no credits, and the full card covers
the picture for that whole span; in the floating window and the mini bar the
same card covers most of the small picture.

- Shows it now: the browser console has no line for where the card showed or
  why; only `pharos: up next: playing …` at the end. Client-only, so the proof
  is a unit test on the step, plus the decision line added with the fix:
  `pharos: up next: shown item=<id> mode=<card|pill|line> credits=<bool> at_ms=… of_ms=…`.

## Refused autoplay (FR-407)

`web.rs` drops the `play()` promise, so a browser that refuses autoplay
leaves a paused first frame with no word why, and in a group the silent
retry keeps calling `play()`.

- Shows it now: nothing. The fix adds the opt-in client event
  `autoplay_refused`, readable through the existing telemetry path as
  `pharos_client_events_total{kind="autoplayRefused"}` and
  `{namespace="pharos"} |= "client event" | json | kind="autoplayRefused"`.
