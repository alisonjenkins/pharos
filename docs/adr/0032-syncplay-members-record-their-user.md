# ADR-0032: SyncPlay group members record their user; a queue change needs every member to see it

- **Status:** Accepted
- **Date:** 2026-11-02T06:50:00Z
- **Deciders:** Alison Jenkins

## Context

A SyncPlay group member is identified by a hash of user and device, and its
display name is chosen by the client. Nothing in the group actor, its
persisted snapshot (`sync_groups.state_json`) or the replica bus said which
user a member was. Joining is gated on the joiner seeing every queued item
(V201), but a member already in a group could queue an item another member
may not see, and every member's client then fetched it (B268).

The snapshot and the bus are JSON and are read by other replicas, including
ones running an older build during a rolling deploy, and by a replica that
restores a group written before this change.

## Decision

We will record the user behind each member:

- `GroupMsg::AddMember` and `GroupMsg::ResyncMember` carry `user:
  Option<UserId>`; every caller that admits a member passes the signed-in
  user.
- The persisted member (`PersistMember.user`) and the bus commands
  (`RemoteCommand::AddMember.user`, `RemoteCommand::Resync.user`) carry it
  with `#[serde(default)]`, so state and commands without it still decode.
- `GroupSnapshot` and `SnapshotSummary` report `member_users`.
- A member with no recorded user can see nothing. A reconnect
  (`ResyncMember`) fills in a missing user.

A queue change (`SetNewQueue`, `Queue`, and the creator's seed) is applied
only when every member's user may see every item in the resulting queue, the
rule Jellyfin's `GroupController` applies (`AllUsersHaveAccessToQueue`).
Otherwise it is refused as a whole and the queue is left as it was.

## Consequences

- One member who may not see an item blocks queueing it for the whole group,
  as in Jellyfin. That includes a member whose policy narrowed after joining.
- A group restored from an old snapshot refuses queue changes until each of
  its members has reconnected once, or left.
- An older replica forwarding a join sends no user, so that member blocks
  queue changes until it reconnects to an upgraded replica.
- `UserId` now serialises (as its UUID).

## Alternatives considered

- **Remove members who cannot see the new queue.** This avoids one member
  blocking the group, but it diverges from Jellyfin. The removal must also
  reach a member's socket on another replica before the queue broadcast does.
  Rejected for this change.
- **Check only the queuing member.** This leaves the leak it was meant to
  close: other members' clients fetch the item.
- **Add a new bus variant instead of a field.** That is needed only for a
  binary encoding. The bus is JSON, and a defaulted field decodes in both
  directions.

## References

- B266, B268; V201.
- `crates/pharos-sync/src/group.rs` (`PersistMember`, `RemoteCommand`,
  `GroupSnapshot`, `SnapshotSummary`).
