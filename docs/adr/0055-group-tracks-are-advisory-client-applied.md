# ADR-0055: Group tracks are advisory group state applied by each following client

- **Status:** Accepted
- **Date:** 2026-11-04T00:00:00Z
- **Deciders:** Alison Jenkins

## Context

Spec 014 US7: a watch-together group has one audio track and one subtitle
choice. When a member changes either, everyone following the group changes with
them; a member can tick "Just me" to keep their own. The server already decides
how each member's stream is produced (playback decision, transcode, burn-in),
per client and per capability. A group track is therefore not a single thing
the server can apply: the same stream index may be direct-played for one member,
transcoded for another, and a subtitle may be a text track for one and burned in
for a third. jellyfin-web members have no track sync at all (V396).

## Decision

- **The group's tracks are state, not a stream choice.** The group actor keeps
  an audio and a subtitle `TrackChoice` (unset, a stream index of the playing
  item, or off) and the member who set them. `Membership.setTracks` changes the
  half it sets; the server announces it (`GroupUpdate.tracks`) and puts it in
  `joined` for a late joiner. It never selects a stream, starts a transcode or
  changes a playback decision because of it.
- **Each following client applies it.** A client that follows the group
  switches its own player to the announced indexes, renegotiating through its
  normal playback path if its player needs it. A member on its own tracks
  (`followTracks(false)`, shown as `ownTracks` in the roster) is announced the
  same updates and ignores them.
- **A queue change clears the choice.** Stream indexes belong to one item, so
  moving the group to another queue entry resets the choice and says so;
  adding to the queue does not.
- **Native only.** The Jellyfin translation drops the messages.
- **Not persisted.** A pod takeover starts a group with no choice; the next
  change sets it.
- **Observable.** `pharos_group_tracks_total{outcome}` (`applied`,
  `not_in_group`, `unchanged`, `stale`) and an INFO line per change naming group,
  member and both choices.
- **A pick names the item it was made on.** `setTracks` carries the
  `playlistItemId` the member was playing; the group drops one that is no longer
  its current entry (counted `stale`), the same guard as a stale `ready` (B37),
  so a pick made just before the group moved on never lands on the next item.

## Consequences

- The server stays out of per-client stream negotiation; a client with a track
  it cannot play degrades on its own path, not the group's.
- A client can ignore the group without the server noticing; "Just me" is a
  state the server records only so peers can see it.
- Two members can briefly show different tracks while each applies the update.
- A takeover loses the choice until someone changes it again.
- During a rolling deploy a replica built before this change cannot decode the
  new bus variants (`SetTracks`, `FollowTracks`, `Tracks`, `MemberTracks`). It
  drops that one envelope and nothing else: the command ingress logs
  "undecodable bus envelope dropped" and counts
  `pharos_syncplay_bus_undecodable_total`, the delivery ingress skips it, and the
  bus and the group carry on. Track sync is missed for the minutes the fleet is
  mixed; no code change is needed.

## Alternatives considered

- **The server picks the stream for every member.** Rejected: the right stream
  differs per member, and it would put per-client negotiation inside the group
  actor.
- **Persist the choice with the group snapshot.** Deferred: it is cheap to
  lose and recovers on the next change; a new ADR if that proves wrong.

## References

- `specs/014-ui-together/spec.md` US7, `plan.md` "US7 and US8".
- V396 in `specs/001-pharos-baseline/invariants.md`.
- ADR-0045 (the native group shares the SyncPlay routing path).
