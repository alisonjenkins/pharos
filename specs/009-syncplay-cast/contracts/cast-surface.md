# Contract: Cast Surface

No new endpoints. The feature reuses existing Jellyfin routes (V1, V16) and adds one config block.

## Config

```toml
[cast]
reconnect_grace_secs = 20      # Lost -> Ended bound
drift_threshold_ms   = 1500    # dead-band; replaced by spike S2 value
correction_cooldown_ms = 5000  # hysteresis

[[cast.tvs]]
name = "Living room"
address = "192.168.1.60:8009"
allowed_users = []             # empty = all users
```

## Jellyfin routes touched

| Route | Today | With this feature |
|-------|-------|-------------------|
| `GET /Sessions` | players only | also lists one virtual session per reachable, permitted TV (`SupportsRemoteControl: true`, `Client: "Chromecast"`). Same values on the socket `Sessions` broadcast. |
| `POST /Sessions/{id}/Playing` | broadcasts `Play` to every socket | if `{id}` is a virtual TV session: caller must be a group member, else `400` with a readable message (FR-009); otherwise start a cast seat (hand-off). Non-virtual ids behave exactly as today. |
| `POST /Sessions/{id}/Playing/{command}` | broadcasts a `PlayState` | if `{id}` is a virtual TV session: the command (Pause, Unpause, Seek, Stop, NextTrack, PreviousTrack) is applied to the **group**, never to the TV alone (FR-004). `Stop` from the seat's owner ends the cast. |
| `POST /Sessions/{id}/Command/{command}` | broadcasts `GeneralCommand` | unchanged for virtual TV ids except volume/display commands, which are ignored with a debug log. |
| `POST /SyncPlay/Leave` | removes the caller | if the caller owns a cast seat, ends it first (reason `owner_left`). |

Responses on rejection use the Jellyfin error shape (V4). Rejection reasons are stable strings used as metric labels: `not_in_group`, `tv_busy`, `tv_unreachable`, `user_has_seat`, `not_permitted`.

## Messages to clients

- To the sender on hand-off: a `PlayState` message with command `Stop` (enum-valid, V39), addressed through the existing socket path. Spike S3 decides whether this suffices.
- To the sender on cast end: the existing resync path puts it back to `Player` at the group position.
- To the group: nothing new on the wire. Members see the roster change through the existing group-update messages; the seat appears under the owner's user name with the TV's name as the device.

## Cast channel (server ↔ TV), internal

- Launch app `F007D354`; namespace `urn:x-cast:com.connectsdk`.
- Outbound command messages carry `command`, `serverAddress`, `accessToken` (TV token), `userId`, `deviceId`, `serverId`, `receiverName`, plus per-command `options` (`Seek`: `position` seconds; `PlayNow`: `ids`, `startPositionTicks`, track indexes). Exact `PlayNow` shape is fixed by spike S1.
- Inbound: `playbackstart` / `playbackprogress` / `playbackstop` mirrored by the receiver (≥1.5 s cadence); the platform heartbeat for liveness.
