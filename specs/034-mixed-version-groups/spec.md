# Feature specification: a group where each member picks 2D or 3D

**Feature directory**: `specs/034-mixed-version-groups/`
**Created**: 2026-11-09T18:30:00Z
**Status**: draft
**Input**: "It would be good to have a sync play that enables some of the
people to play in 3d and some in 2d. Can you spec that out and get something
working on that please?" (Alison, 2026-11-09)
**Depends on**: 014 (Together in pharos-ui), 022 (3D in pharos-ui: versions,
the picker, the 3D views), ADR-0079 and ADR-0080 (copies of a title are
versions of one item), #1055 (2D and 3D copies of a film are versions)
**Ids**: bugs B941–B959, invariants V902–V919, ADR-0089. B960+, V920+ and
ADR-0088 belong to the 3D playback work amending spec 022.

## Problem

Since #1055 the 2D and 3D copies of a film are versions of one item, so a
group already plays one logical item on one timeline. Each member's player
negotiates the version that device last picked (`pharos.version.<item>` in
`localStorage`), and a group start begins at the group's position whatever
the version (B664). A member who picked 3D on the title page before the group
started already plays 3D beside a member on 2D.

Four things stop that being something to rely on:

1. **The group's audio and subtitle picks name stream indexes** (B941). The
   group holds `GroupTracksView { audio, subtitle }` as stream indexes of the
   file the picking member played, and every following member applies the
   same index to its own file. A 3D rip's streams are seldom laid out as the
   2D file's: index 2 is Japanese in one and English commentary in the
   other. One member's pick moves another to the wrong language.
2. **There is no way to change version from the player.** The picker is on
   the title page alone. A member who joins from an invite lands in the
   player and can only watch the version their device last used.
3. **Nobody is told when two versions do not run alike.** Copies from the
   same master play frame for frame. A copy with another studio logo or a
   cut scene does not, and the group then shows two members different
   moments at "the same" position, with no signal anywhere.
4. **Nothing says who is on which version.** A member cannot see that a
   friend is watching in 3D.

## Scope

In scope: the group syncs on the logical item and one timeline; each member
picks their own version; track picks carry across versions; a member can
switch version mid-group; a version whose runtime differs is detected and
reported.

Not in scope:

- **How 3D is viewed** (one eye, both eyes, a headset). That is device-local,
  never synced, and owned by the 3D playback work under spec 022.
- **Aligning versions that differ** (US5) is specified here and built in a
  later slice.
- Jellyfin SyncPlay clients. A Jellyfin queue takes item ids, and a version's
  id is a `MediaSourceId`, never an item id, so a Jellyfin group always keys
  on the logical item. Each Jellyfin client picks its source through
  `PlaybackInfo` as it does today. Nothing changes for them.

## User stories

### US1: each member watches the version they picked (P1)

Alison and Sam watch a film together. Alison's device picked the 3D version,
Sam's the 2D. Both start at the group's position, play, pause and seek
together, and each sees their own version.

Acceptance:

1. A member whose device picked a version plays that version in the group;
   one that never picked plays the primary, which is 2D when a 2D copy
   exists (ADR-0080's ranking).
2. Play, pause, seek and buffering waits are the same as for a group on one
   version: they are positions on the item's timeline.
3. The device logs `pharos: version chosen=… stored=… of N` as today, and
   the group start logs `pharos: playback group_start item=… position_ms=…`.

### US2: the group's audio and subtitles follow by stream, not index (P1)

Sam picks Japanese audio and English subtitles from the 2D file. Alison, on
the 3D file and following the group's tracks, gets Japanese audio and English
subtitles from hers, whatever their indexes.

Acceptance:

1. A pick sent to the group names the matching stream of the item's
   **primary** file (ADR-0089). A member on another version translates its
   own index to the primary's before sending, and the primary's index to its
   own before applying.
2. Streams match by language and forced and hearing-impaired flags; among
   those, a stream whose title, codec and channels also agree pairs first,
   and the rest pair in order: a commentary ahead of the main track in one
   file still leaves the main tracks paired.
3. A stream with no match is not applied: the member stays on what it plays
   and the device logs `pharos: group tracks unmapped kind=… index=…
   version=…`. A pick with no match in the primary is applied locally only.
4. Members on the primary see no change: the mapping is the identity.

### US3: switch version from the player (P1)

Alison is in the group on 2D and wants 3D. The player's settings show a
Version control listing the item's versions. Choosing 3D stores the pick for
this device and renegotiates on the chosen file at the current position. The
member stays in the group: no leave, no rejoin, no message to other members.
The group's catch-up seek corrects any drift during the switch.

Acceptance:

1. The control shows only when the item has more than one version, labelled
   by `version_label`.
2. A switch logs `pharos: version switch item=… from=… to=… at_ms=…
   grouped=…`.
3. The new player starts at the old player's last position, not the stored
   resume point.

### US4: a version that runs differently is flagged (P1)

The 3D copy runs 40 s longer than the 2D. A member who picks it in a group
sees a notice: "This version runs 40 s longer than the main one, so it may be
out of step with the group." The device logs `pharos: version runtime_gap
item=… version=… primary_ms=… version_ms=… gap_ms=…`.

Acceptance:

1. Each version carries its own runtime (`Version.durationMs`, 0 when
   unknown).
2. The notice shows when both runtimes are known and differ by more than
   `RUNTIME_TOLERANCE_MS` (2 s: a container's rounding and a trailing black
   frame stay inside it). It never blocks playback.
3. Outside a group the notice does not show; the log line still does.

### US5: aligned versions (P2, later slice)

A version that runs differently plays at the moment the group is at, not the
same number of milliseconds. The server finds each version's offset from the
primary by matching the audio (as the aligner does for lyrics) and stores it
on the version. A member maps the group's position through it on every play,
seek and report. A version whose offset changes partway (a cut scene) is
not aligned and keeps US4's notice.

### US6: see who watches which version (P3, later slice)

The group's member list marks members on a 3D version, as it marks members
on their own tracks. The member reports its version's stereo flag to the
group on start and on a switch.

## Requirements

- FR-001: the group's state names one logical item and a position on its
  timeline; no member's version enters it.
- FR-002: group track picks are primary-file indexes (US2, ADR-0089).
- FR-003: the player offers a version switch that keeps group membership
  (US3).
- FR-004: `Version` carries `durationMs`; the API's schemaVersion goes to 31.
- FR-005: a runtime gap over 2 s is logged and, in a group, shown (US4).
- FR-006: the viewing mode of a 3D version is never sent to the group.

## Observability

- Today's mixed group, by query: `{namespace="pharos"} |= "version chosen="`
  in the client log stream, beside the group's join.
- B941 happening: a follower's `pharos: playback … audio=Some(n)` naming a
  stream whose language differs from the picker's. After the fix,
  `group tracks unmapped` counts the picks that could not carry.
- Switches: `version switch`. Runtime gaps: `version runtime_gap`.

## Invariants

- V902: a group track pick names the primary file's stream; a member maps it
  to and from its own version by language, flags and order (B941).
- V903: a version switch renegotiates at the current position and does not
  leave or rejoin the group.
- V904: a version whose runtime differs from the primary's by more than 2 s
  is logged with both runtimes, and a member in a group is told.

## Slices

1. This PR: US1 (verified), US2, US3, US4.
2. US6: member version marks (sync state, capnp, RPC, roster).
3. US5: offsets by audio alignment.
