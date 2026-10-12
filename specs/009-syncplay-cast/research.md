# Research: SyncPlay Cast Target

**Date**: 2026-10-12 · **Spec**: [spec.md](spec.md)

Sources: this repo (read 2026-10-12), `jellyfin/jellyfin-chromecast` master, `jellyfin/jellyfin-web` v10.11.11. Anything not read directly from source is marked UNVERIFIED and gets a spike task (T-first in the plan).

## R1. Can the server control a stock Chromecast receiver through the sender?

**Decision**: No. Design around the server being the Cast sender.

**Findings**
- The receiver opens no server websocket; its only transport is the Cast message channel (`urn:x-cast:com.connectsdk`) from whoever launched it. All server I/O is REST (`/Sessions/Capabilities/Full`, `/Sessions/Playing`, `/Sessions/Playing/Progress`, `/Sessions/Playing/Stopped`, `/Sessions/Playing/Ping`).
- Every command message carries `command`, `serverAddress`, `accessToken` (+ `userId`, `deviceId`, `serverId`, `receiverName`). The receiver authenticates with the sender's token but reports under its **own** DeviceId (`btoa(receiverName)`), client `Chromecast`.
- Handled commands: DisplayContent, Identify, InstantMix, Mute, NextTrack, Pause, PlayLast, PlayNext, PlayNow, PlayPause, PreviousTrack, Seek, SetAudioStreamIndex, SetRepeatMode, SetSubtitleStreamIndex, SetVolume, Shuffle, Stop, ToggleMute, Unmute, Unpause, VolumeDown, VolumeUp. `Seek` takes `options.position` in seconds. Unknown commands fall through to Play.
- The receiver mirrors every playback report to the Cast sender on the same channel (`playbackstart`, `playbackprogress`, `playbackstop`), server-bound progress every >5 s, sender-bound every ≥1.5 s.
- jellyfin-web while casting: SyncPlay swaps to the `NoActivePlayer` wrapper; server Pause/Unpause/Seek are dropped ("no active player"); Ready/Buffering are never sent; its UI actions become SyncPlay requests. So a casting browser is a controller that never acks.
- Pharos today: no cast-target logic at all. `POST /Sessions/{id}/Playing[/{command}]` broadcasts a `SessionCommand` to every `/socket` with no check that the target exists or is the caller's (`state.rs:1027-1042`, `socket.rs:345-366`); `cast_receiver_id` is a constant only (`dto.rs:42`).

**Rationale**: A server-side participant needs a control channel; the only one the stock receiver has is Cast. Controlling it from the server removes the dependency on any client behaviour.

**Alternatives rejected**: patch jellyfin-web (breaks V1/unmodified clients, sender tab must stay open, needs a custom bundle); degrade only (TV not synced, misses the goal).

## R2. How does a stock client choose the TV?

**Decision**: Expose each configured TV as a *virtual Jellyfin session* in `GET /Sessions` (`SupportsRemoteControl: true`, client `Chromecast`, device name = TV name). jellyfin-web's ordinary "play on another device" picker then offers it and sends `POST /Sessions/{virtualId}/Playing` (and `/Playing/{command}`) — requests pharos already receives.

**Why**: stays inside the public Jellyfin API (V16, V1); needs no pharos-only UI; the Dioxus UI is untouched.

**UNVERIFIED (spike S4)**: that jellyfin-web 10.11.x actually lists a server-supplied session as a target with only these fields set, and which fields (`Capabilities`, `PlayableMediaTypes`, `SupportedCommands`) it requires. Note the existing `/Sessions` socket broadcast hard-codes `supports_remote_control:false` (`socket.rs ~895-915`) while the REST record says `true`; a virtual session must be consistent on both.

## R3. Where does the TV sit in the group?

**Decision**: A new member kind `CastSeat` in `pharos-sync`, owned by a user, whose `MemberSink` is an adapter that turns the group's outgoing `SendCommand`s into Cast messages. It never gates: it is excluded from `follower_ids` like an `ignore_wait` member (V139, V161), and its `connected` flag mirrors the Cast connection health, reusing the existing `MemberSocketLost`/prune path.

**Findings in repo**
- Members are keyed by `MemberId = UUIDv5("{user}:{deviceId}")` (`hub.rs:25-37`), deterministic across restarts (B24). A cast seat gets `UUIDv5("{user}:cast:{tvName}")` so it is deterministic too.
- Sinks are looked up by member id; a non-websocket sink is a local addition. Group actor does no roster check on the *sender* of HTTP commands (`syncplay.rs dispatch`), so a casting user's controller commands already reach the group.
- Persisted member (`PersistMember{id,name,ignore_wait}`) needs `kind` with `#[serde(default)]` for old-snapshot compatibility (`group.rs:2168` precedent). Multi-replica: a new `GroupMsg` needs a `RemoteCommand` variant (`distributed.rs`).
- The TV connection is single-owner: only the replica that owns the group holds it, or two replicas fight over one TV (a Cast device accepts multiple senders; the receiver then answers both).

**Alternative rejected**: model the TV as the user's existing member. The user's member and the TV have different drivers and ack capabilities; conflating them is exactly the V161/V139 wedge class.

## R4. How is drift measured and corrected?

**Decision**: Treat the group as the clock. Each receiver `playbackprogress` (≥1.5 s cadence, plus on pause/play) gives `(position, is_paused, wall-clock arrival)`. Interpolate between reports. A pure function `plan(target, observed, now, state) -> Action` decides `None | Seek(to) | Pause | Resume` with a dead-band (seek only above a threshold) and hysteresis (at most one correction per cooldown) so corrections do not oscillate. Threshold and cooldown are config with defaults chosen in spike S2.

**UNVERIFIED (spike S2)**: Cast seek accuracy and settle time on a real TV; whether `Seek` followed by an immediate progress report is trustworthy. SC-002's 2 s figure rests on this.

## R5. Receiver credentials

The receiver needs a token in every command message. Use a **short-lived, device-bound token minted for the TV** (not the sender's token): bounded lifetime, hashed at rest, revoked when the cast ends or the TV is lost. Never logged (V8). Auth-hardening work already hashes tokens at rest (memory: auth hardening LANDED); the mint path is confirmed in spike S1.

## R6. Cast protocol implementation

**Decision**: a `CastTransport` trait in a new `pharos-cast` crate; a real implementation speaking CASTV2 (TLS to port 8009, length-prefixed protobuf frames, `connection`/`heartbeat`/`receiver` namespaces, launch app `F007D354`, then `urn:x-cast:com.connectsdk`), and an in-memory fake.

**Candidate**: the `rust-cast` crate vs hand-rolling over `rustls` + `prost`. UNVERIFIED which is maintained/licence-compatible (AGPL-3.0-or-later workspace) and whether it is async; decided in spike S1. Either way it sits behind the trait (V12, constitution V), so the group logic never sees it. `workspace-hack` must be regenerated (`just hakari-regen`) after the dep change.

## R7. Network reachability and discovery

Deploy is a k3s pod. Pod → TV unicast TCP/8009 works if the cluster can route to the LAN; mDNS multicast discovery from a pod does not. **Decision**: TVs are declared in config (`[cast.tvs]` name → address), no discovery in this feature. Reachability is checked by connecting (a TV is listed only while it answers).

## R8. Hand-off of the sender

On start, the sender's own player must stop or it plays twice. The server sends the sender's socket a `PlayState` `Stop` (an existing, enum-valid message type, V39). UNVERIFIED (spike S3): that jellyfin-web in SyncPlay mode acts on a server `Stop` rather than turning it into another SyncPlay request. Fallback if it does not: demote the sender to controller, tell the user in the response, and leave local playback to the user. On stop, the sender is brought back through the existing late-joiner resync (`ResyncMember`).

## Spike list (first tasks; outputs recorded here before implementation)

| Id | Question | Needs |
|----|----------|-------|
| S1 | Crate choice; token mint path; CASTV2 launch + `connectsdk` message shapes for `PlayNow` with a start position | TV or receiver emulator |
| S2 | Seek accuracy/settle time; progress cadence; real drift over a 60-min item | real TV |
| S3 | jellyfin-web response to server `PlayState Stop` while in a group | Playwright harness |
| S4 | jellyfin-web lists a virtual session in its device picker; fields required | Playwright harness |

## Risks

- Real-TV behaviour (S2) cannot be proven in CI: the receiver emulator proves the logic, a manual run on a real TV is the only evidence for SC-001/SC-002. Report that run as unwitnessed until done.
- Receiver seek accuracy may make 2 s infeasible; SC-002 is then restated from the spike, not silently loosened.
- Group owner failover (rolling deploy) must hand the TV connection to the new owner without a visible glitch beyond the existing V25 bound.

## Spike S1 outcome (2026-10-12; sources: crates.io API, rust-cast/cast-sender repos, jellyfin-chromecast master 2026-09-23, jellyfin AuthorizationContext.cs)

**Crate decision: hand-roll** `CastTransport` on `tokio-rustls` + `prost` + `tokio_util::codec::LengthDelimitedCodec` (about 400 lines). `rust_cast` 0.21 (MIT) is blocking, pins `protobuf =3.7.2` exactly, and needs `spawn_blocking` plus a reader thread; `cast-sender` 0.3 is smol/native-tls (openssl) and young; `chromecast` is stale. Fallback if hand-rolling stalls: `rust_cast` with `thread_safe` on a dedicated thread. TLS: a rustls verifier that accepts the Chromecast's self-signed certificate (optionally pinned by fingerprint).

**Protocol surface**: one protobuf message `CastMessage` (protocol_version, source_id, destination_id, namespace, payload_type, payload_utf8); 4-byte big-endian length framing. Namespaces: `tp.connection` (CONNECT to `receiver-0`, then to the app `transportId`, CLOSE), `tp.heartbeat` (PING/PONG about every 5 s, answer the device's pings), `receiver` (LAUNCH `F007D354`, GET_STATUS, STOP; parse `RECEIVER_STATUS.status.applications[].{appId,transportId,sessionId}`), and `urn:x-cast:com.connectsdk` (JSON strings both ways).

**Receiver command shapes** (only `command`, `serverAddress`, `accessToken` are mandatory; `userId`, `deviceId`, `serverId`, `serverVersion` are ignored; `receiverName` optional but should be set and stable because the receiver derives `DeviceId = btoa(receiverName)`; `serverAddress` is a base URL without trailing slash):

```json
{"command":"PlayNow","serverAddress":"http://pharos:8096","accessToken":"<tok>","receiverName":"Living Room TV","maxBitrate":20000000,
 "options":{"items":[{"Id":"<itemId>","Type":"Movie"}],"startIndex":0,"startPositionTicks":0,"mediaSourceId":"..","audioStreamIndex":1,"subtitleStreamIndex":3}}
{"command":"Pause", ...}  {"command":"Unpause", ...}  {"command":"Stop", ...}
{"command":"Seek","options":{"position":120}, ...}        // seconds
{"command":"SetAudioStreamIndex","options":{"index":2}, ...}
```

`PlayNow.items[0]` is dereferenced, so the list must be non-empty; only `Id` is functionally needed (the receiver re-fetches the item and fetches PlaybackInfo itself with its own DeviceProfile) but `Type` must be sent (an `Episode` with `smart` expands to the season, so the driver sends `Movie`/`Video` semantics and controls the queue itself). `PlayNext` only enqueues (idle behaviour UNVERIFIED). `NextTrack`/`PreviousTrack` act only on the receiver's own playlist, so item changes use `PlayNow`. Volume/mute commands are no-ops on the receiver. No `Identify` step is needed; credentials travel on every message.

**Receiver reports** (JSON objects on the same namespace, sent to the LAST sender id): `playbackstart`, `playbackprogress`, `playbackstop`, `playstatechange` (also after our commands: immediately, +100 ms, +500 ms), `volumechange`, `repeatmodechange`, `error`, `playbackerror`, `connectionerror`. Payload `data.PlayState.{PositionTicks,IsPaused,ItemId,PlayMethod,PlaySessionId,...}`. Progress cadence: bus message on every time-update between 1.5 s and 5 s after each server report, so treat it as roughly 1 Hz, not a fixed 5 s.

**Consequences for the plan**
1. Config needs `[cast] server_url`: a base URL the TV can reach, plain `http://` on a LAN works; an HTTPS URL with a private CA fails on the TV.
2. Pharos auth must not require the request's `DeviceId` header to equal the token's bound device. The receiver always sends its own `btoa(receiverName)`; real Jellyfin tolerates this. T005 follow-up: verify pharos in `auth_extractor.rs`, and decide session keying so receiver reports do not create a stray second session (key by token, or recognise the TV's derived device id).
3. The receiver posts its own `/Sessions/Capabilities/Full` on the first message; it overwrites capabilities stored for that token's session, so the virtual TV session must not rely on stored caps.
4. The user is resolved from the token alone on the receiver's `GET /Items/{id}` calls (no `userId` param); the TV token routes must resolve user from the token.
5. A second sender silently takes over the receiver's replies (`window.senderId` is the last sender) and replaces its credentials: the `tv_busy` rejection must also refuse a TV already running a foreign cast, detected from `GET_STATUS` (an app already running with another sender is "busy").
6. Heartbeat or TLS loss kills the virtual connection and the receiver app may exit without a message, which the `Lost` state treats like any other connection loss.

UNVERIFIED still: the `@jellyfin/sdk` Authorization header string the receiver sends; `PlayNext` while idle; whether `F007D354` is the id of the code read (the id is registered in the Google Cast console, not in source); idle-timeout behaviour in production receivers.
