# Data Model: SyncPlay Cast Target

Names are descriptive; exact Rust names are settled in implementation.

## ConfiguredTv (config, immutable after load)

| Field | Type | Rule |
|-------|------|------|
| `name` | string | unique, 1–64 chars; the device name clients see |
| `address` | host:port | port defaults to 8009; validated at load |
| `allowed_users` | list of user ids, optional | empty = every user may target it (V9: a user not allowed never sees it) |

Source: `[cast.tvs]` in `config.toml` (+ env override). Invalid entry → startup error naming the entry and the value.

## CastSession (in-memory, owned by the `CastDriver` actor; one per active TV)

| Field | Meaning |
|-------|---------|
| `tv` | ConfiguredTv name |
| `owner` | user id who started it |
| `group_id` | group the seat belongs to |
| `seat_member_id` | `UUIDv5("{user}:cast:{tv}")`, deterministic across restarts |
| `sender_member_id` | the starting user's player member, demoted to Controller |
| `token_id` | id of the TV-bound token (never the secret) |
| `state` | see transitions |
| `last_report` | `(position_ticks, is_paused, item_id, arrived_at)` from the receiver |
| `last_correction_at` | for hysteresis |

One CastSession per TV and per group seat; a second start on the same TV or by a user who already has a seat is rejected (FR-011).

### State transitions

```
Idle ──start──▶ Connecting ──launched──▶ Loading ──first report──▶ Following
                    │                        │                         │
                    └──fail──▶ Failed        └──fail──▶ Failed         ├─ conn lost ─▶ Lost ──reconnect ok──▶ Following
                                                                       │                │
                                                                       │                └─ grace expired ─▶ Ended
                                                                       ├─ user stop ──▶ Ended
                                                                       └─ group gone ─▶ Ended
Failed / Ended: token revoked, sender restored to Player, event recorded with reason.
```

`Lost` is bounded (grace window in config, default 20 s to match the websocket reconnect grace); `Following` is the only state in which the seat counts as `connected`.

## Group member (pharos-sync, extended)

`PersistMember` gains, with `#[serde(default)]` so old snapshots load unchanged:

| Field | Type | Default |
|-------|------|---------|
| `kind` | `Player` \| `Controller` \| `CastSeat` | `Player` |
| `cast` | optional `{tv, owner}` | none |

- `Controller`: stays in the roster, receives group state, issues commands, excluded from `follower_ids` (never gates, never needs to ack).
- `CastSeat`: excluded from `follower_ids`; `connected` follows Cast connection health; its sink is a Cast adapter, not a websocket.
- Roster projection: a user whose `Controller` and `CastSeat` members belong to the same cast is shown as ONE entry (name = user, device = TV). The projection is derived, never stored (FR-005).
- `kind` is a closed enum, not a bool pair, so "controller that is also a seat" is unrepresentable (constitution V).

Not persisted: `last_report`, drift state, clock offsets (rebuilt after the first report). The CastSession is re-created from the persisted `cast` field on hydrate, then reconnects.

## VirtualTvSession (derived, never stored)

Projected into `GET /Sessions` for a user allowed to use a reachable TV: `Id` = stable id derived from the TV name, `DeviceName` = TV name, `Client` = `Chromecast`, `SupportsRemoteControl` = true, `NowPlayingItem` filled from the group's current item while a seat is active. Same values on the REST list and the socket broadcast.

## Token (existing store)

A device-bound token for the TV with bounded lifetime, hashed at rest, tied to `token_id` in the CastSession so it is revoked on end.
