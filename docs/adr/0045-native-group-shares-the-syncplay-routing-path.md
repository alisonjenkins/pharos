# ADR-0045: Native Group interface shares one routing path with the Jellyfin SyncPlay handlers

- **Status:** Accepted
- **Date:** 2026-11-04T00:00:00Z
- **Deciders:** Ali

## Context

ADR-0028 gave first-party clients a pharos-native Cap'n Proto API next to the
Jellyfin one. Spec 014 puts watch-together on that API. V20 requires a native
member and a Jellyfin member to share one SyncPlay group, so the native
`Group` cannot hold groups of its own: it has to drive the same
`GroupRegistry` actors the `/SyncPlay/*` handlers and the Jellyfin `/socket`
drive. V172 requires a behaviour exposed on both APIs to live once, in a
domain service both wire layers call.

The Jellyfin handlers already carried the routing: resolve the device to a
session, apply the V201 queue check, send the actor a `GroupMsg`, recover from
the persisted membership during the deploy reconnect gap (B24), detach a dead
actor, and count `pharos_syncplay_command_total`. That code answered with an
`HttpResponse`, and a push to a member went through the `/socket` sink only.

## Decision

- **One routing path.** `group_ops::route_command` (surface-neutral, returns a
  `CommandOutcome`) is the only way a command reaches a group. The Jellyfin
  handlers map the outcome to a status; the native `Membership` maps it to a
  `Done` result (`notFound`, `notPermitted`, `unavailable`, `invalid`). Listing
  (`listed_groups`), access (`may_join`, `may_queue`) and recovery
  (`recover_and_apply`, which says `NotFound`, `Refused` or `Applied`) are
  shared the same way, so V201 and the command metric apply to native members.
- **Member key.** A native member's hub key is `{userId}:{ctx.device}`, the key
  `/socket` registers under, so its member id (`member_id_for_device`) is
  stable across reconnects. A call without a device is `invalid{ctx.device}`.
  A device is in one group at a time: a create or join from a device that is
  in another group first removes it from that roster.
- **Push by capability.** The client passes a `GroupSink` callback capability
  to `create` and `join`. The member registers in the hub with a channel; a
  per-member pump task translates what the actor delivers into `GroupUpdate`s
  and calls the sink. The actor announces no buffering change, so the pump
  reads it from the group snapshot, after the messages that follow one and
  every 2 s.
- **Drop is socket loss.** Dropping a `Membership` (a closed tab, a lost
  connection) sends `MemberSocketLost` and removes the member after
  `SOCKET_LOSS_GRACE` unless the same device joins again; only `leave` leaves.
  A membership whose device has since joined another group answers
  `notFound{membership}`.
- **Who corrects drift.** The client owns steady-state drift correction
  against the group clock (`Group.clock`, `clockReport`). Server commands
  (`play`, `pause`, `seek`, `item`) win: a client applies them whatever its
  local state.
- **Wire surface is what is produced.** `GroupUpdate.ended` carries `left` and
  `notInGroup`, the only ends the server emits; the schema has no
  `GroupUpdate.error` or `GroupMember.isPharos`.

## Consequences

- A change to command routing is made once and reaches both surfaces; the
  Jellyfin compat suite guards it.
- A native member's seat survives a reconnect of its capability within the
  grace, with no new protocol for it.
- The native schema inherits the actor's limits: no buffering push (hence the
  2 s poll, so a stall shows up to 2 s late while the group is paused), and no
  notice when a member is pruned as a ghost, since the actor removes it
  without a message to it.
- Adding a surface (a third API) means mapping `CommandOutcome`, not copying
  the routing.

## Alternatives considered

- A separate native group registry: breaks V20; native and Jellyfin members
  could not share a group.
- Native handlers calling the actor directly: loses recovery, the metric and
  V201 on the native path, and forks the behaviour V172 forbids.
- Having the actor push buffering changes: a change to every surface's
  protocol for a native-only need; deferred.

## References

- ADR-0014, ADR-0028, ADR-0029; invariants V20, V65, V172, V201
- `crates/pharos-server/src/group_ops.rs`,
  `crates/pharos-server/src/api/pharos/rpc/group.rs`
- `specs/014-ui-together/plan.md`, `specs/017-native-api/contracts/pharos.capnp`
