# ADR-0041: The server is the Cast sender; the TV is a non-gating group seat

- **Status:** Accepted
- **Date:** 2026-11-02T20:30:00Z
- **Deciders:** Alison Jenkins

## Context

SyncPlay members wanted to watch a group's item on a TV with a stock
Chromecast. The stock jellyfin-chromecast receiver (app `F007D354`) opens no
server socket. Its only control channel is the Cast message channel
(`urn:x-cast:com.connectsdk`) from whoever launched it. While a jellyfin-web
tab is casting, its SyncPlay drops the server's Pause, Unpause and Seek ("no
active player") and never sends Ready or Buffering. A browser sender can
therefore neither keep the TV in sync nor take part in the group's readiness
gates (`specs/009-syncplay-cast/research.md` R1, R3).

TVs sit on the LAN. The server runs as a k3s pod that can reach TCP/8009 on
the LAN but cannot do mDNS discovery (R7).

## Decision

We will make pharos itself the Cast sender, in a new crate `pharos-cast`:

- **Transport.** A hand-rolled CASTV2 client behind a `CastTransport` trait:
  TLS to port 8009 (`tokio-rustls`, accepting the device's self-signed
  certificate), 4-byte length-framed `CastMessage` protobufs (`prost`),
  heartbeat, `LAUNCH` of `F007D354`, then JSON on the connectsdk namespace.
  An in-process receiver emulator (`FakeReceiver`, `MockTv`) implements the
  same trait for tests.
- **Group role.** The TV joins the SyncPlay group as a new member kind,
  `MemberKind::CastSeat`, owned by the casting user, with member id
  `UUIDv5("{user}:cast:{tv}")`. The seat never gates the group: it is left out
  of readiness gates and buffering freezes (V226). Commands addressed to the
  TV act on the group (V227). The seat is persisted with the group and resumed
  after a restart or takeover. Only the replica that runs the group holds the
  TV connection, enforced by a per-TV lease.
- **Discovery.** TVs are declared in config (`[cast]` with `[[cast.tvs]]` name
  and address), not discovered. Each reachable TV is offered to clients as a
  virtual Jellyfin session in `GET /Sessions`, so jellyfin-web's own "play on
  another device" picker lists it.
- **Drift.** The group is the clock. A pure planner compares the receiver's
  progress reports with the group's position and corrects with a dead band
  (1500 ms) and a cooldown (5000 ms).

## Consequences

- Unmodified clients can cast a SyncPlay group to a TV. The sender's tab does
  not have to stay open.
- pharos now owns a network protocol: CASTV2 framing, heartbeats, reconnects
  within a grace period, and a receiver whose behaviour is known only from
  its source. Real-TV behaviour (seek accuracy, buffering reports, drift
  defaults) is still unmeasured (spec 009 T048).
- New third-party dependencies: `prost`, `rustls`, `tokio-rustls`.
- The TV needs its own credential. That is ADR-0042.
- A TV whose address changes needs a config change: there is no discovery.

## Alternatives considered

- **Patch jellyfin-web to drive the TV.** Breaks the unmodified-client goal,
  needs the sender tab open, and needs a custom bundle.
- **`rust_cast` 0.21.** Blocking I/O, pins `protobuf =3.7.2` exactly, and
  needs a reader thread. **`cast-sender` 0.3** is smol and native-tls
  (OpenSSL) and young. Hand-rolling was about 400 lines (research.md, spike
  S1).
- **Model the TV as the user's existing member.** The user's player and the
  TV have different drivers and acknowledgement abilities. Merging them is
  the gate-wedge class V139 and V161 guard against.

## References

- `specs/009-syncplay-cast/` (spec, research R1-R8, spike S1)
- `crates/pharos-cast/`, `crates/pharos-server/src/cast_service.rs`
- Invariants V226-V229
