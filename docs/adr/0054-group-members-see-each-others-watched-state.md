# ADR-0054: Group members may read each other's watched state, for items the reader may see

- **Status:** Accepted
- **Date:** 2026-11-04T00:00:00Z
- **Deciders:** Alison Jenkins

## Context

A group watching a show together wants to know who needs catching up and who
has seen a film. That is one user's watch history (`user_data.played`, the
resume position) shown to other users. Until now a user's played state was
readable only by that user. ADR-0032 made each group member record its user,
which makes the question answerable; it does not say whether it should be.

## Decision

- **Peers in the same group only.** `Membership.seen` answers for the group
  its `Membership` belongs to. A caller who has left, or whose device moved to
  another group, is refused `notFound{group}`. There is no way to ask about a
  user who is not in the caller's group, and no admin or library-wide form.
- **Only items the caller may see.** An id the caller's policy hides
  (`retain_visible`, V201) is omitted from every member's answer. A series or
  season counts only the episodes the caller may see, so a count cannot leak
  that a hidden episode exists. The members' own visibility is not applied:
  the reader's is the bound.
- **Only played state.** The answer is played, a resume percent and episode
  counts. It carries no timestamps, play counts, favourites, tracks or
  devices.
- **A member with no recorded user** is listed with no items and reads as not
  seen.
- **Bounded.** At most 200 ids per call, else `limit`.
- **Observable.** `pharos_group_seen_total{outcome}` (`answered`, `over_limit`,
  `not_in_group`, `unavailable`) and one `group seen` log line with the
  caller, group, ids asked, ids shown and member count.
- **No opt-out yet.** Joining a group is the consent: a user who does not want
  peers to see their state does not join. A per-user setting would be a new
  ADR.

## Consequences

- Joining a group shows the joiner's played state of every title the others
  can see to every member. The Together page and the invite landing say
  nothing of this yet; if that proves surprising, the opt-out above is the fix.
- The answer is read live from the store, so a change is seen on the next
  fetch; the UI refetches when the roster or the item changes, not on every
  played-state change.
- `GroupSnapshot.participant_users` pairs each participant with its user;
  `member_users` is in map order and must not be indexed with `participants`.
