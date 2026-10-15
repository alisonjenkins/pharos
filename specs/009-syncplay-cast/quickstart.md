# Quickstart: validating SyncPlay Cast Target

Run everything inside the devShell (`nix develop --command …`).

## 1. Logic, no TV (CI)

```
just test-changed
cargo nextest run -p pharos-cast -p pharos-sync -p pharos-server
```

Covers: the receiver emulator (speaks the stock receiver's command set and mirrors progress every 1.5 s, with injectable latency and drift), the drift planner, `CastSeat` never gating a group (extends `group_conformance.rs`, `hub_integration.rs`), hand-off and restore, persisted `kind` round-trip and old-snapshot load, recovery after restart (template: `syncplay_restart_recovery.rs`), the virtual session in `GET /Sessions`.

Disarm check (constitution III): comment out each `pharos_cast_*` record call once and confirm its test goes red, then restore.

## 1b. Mock TV on a real socket (CI)

```
cargo nextest run -p pharos-server -E 'test(/cast_mock_tv/)'
```

`pharos_cast::mock_tv::MockTv` (cargo feature `testing`) is a CASTV2 receiver on `127.0.0.1:0`: TLS with a self-signed certificate, length-framed protobuf, heartbeat, `LAUNCH`, and the connectsdk commands, with `FakeReceiver` as its playback model. `tests/cast_mock_tv.rs` runs a real HTTP server whose `[[cast.tvs]]` address is the mock, reached by the production `CastV2Factory`/`CastV2Transport`, with three occupants holding sockets in one group. It covers: start (PlayNow at the group position, working token), pause/seek/unpause by other occupants, a late joiner and a buffering occupant never holding the TV (V172), injected drift corrected (`pharos_cast_correction_total`), a dropped connection inside the grace reconnecting, and a stop on the TV ending the cast as `user_stop` with the token revoked.

## 2. Real client, emulated TV (`just compat-syncplay` extension)

Two `jellyfin-web` contexts in one group plus the emulator as the TV. Steps: member A opens the device picker, selects the TV, the emulator receives `PlayNow` at the group position, member B pauses/seeks, the emulator applies them. Asserts S3 (stop on hand-off) and S4 (picker lists the TV).

## 3. Real TV (manual; the only evidence for SC-001/SC-002)

1. Add the TV under `[[cast.tvs]]`, start pharos, confirm `GET /Sessions` lists it for a permitted user.
2. Two people join a group and play an item; one picks the TV from the player's device list.
3. Expect: the TV starts within 15 s at the group position; the picker's owner's own player stops; pause/seek from the other person reach the TV within 1 s.
4. Switch the TV off. Expect: the other member is not frozen beyond the bounded freeze; the owner can resume locally.
5. Measurement: SC-001 on a real TV is position error against the group clock from interpolated receiver reports (command-to-apply is only observable on the emulator); SC-002 is that error sampled over a full item.
6. Run the queries below and record the output in this file. Until then the real-TV claims are unwitnessed.

## 4. Verify by query (name the signal first)

```logql
{namespace="pharos"} |= "cast seat" | logfmt | line_format "{{.event}} {{.user}} {{.sender_device}} {{.tv}} {{.reason}}"
```

```promql
sum by (event, reason) (increase(pharos_cast_seat_total[1h]))
histogram_quantile(0.95, sum by (le) (rate(pharos_cast_drift_seconds_bucket[10m])))
sum by (kind) (increase(pharos_cast_correction_total[1h]))
```

A cast that "did nothing" must show up as a `pharos_cast_seat_total{event="rejected",reason=…}` row, never as silence.
