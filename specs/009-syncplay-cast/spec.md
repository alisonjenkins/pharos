# Feature Specification: SyncPlay Cast Target

**Feature Branch**: `agent/snoopy-purring-cupcake` (spec dir `009-syncplay-cast`)

**Created**: 2026-10-12

**Status**: Implemented against an in-process receiver emulator (57 of 65 tasks); NOT proven on a real TV, in jellyfin-web, or on postgres (see tasks.md T006, T007, T045, T048)

**Input**: User description: "I would like to make it so that when we are watching something in Pharos in a sync play group that I can stream to a TV using the chromecast feature and it remaining in sync with the group."

## Clarifications

### Session 2026-10-12

- Q: While casting, what does the sending device do? → A: Pure remote. The TV holds the user's only seat; the sender does not play.
- Decision (analyze F1/F2, 2026-10-12, taken by the implementer, open to veto): the roster shows a casting user once, with the TV as the device (the owner's controller entry is not listed separately). A pause, seek or stop made on the TV itself (remote, voice) is treated as a command from the owner and applied to the whole group; the TV is not silently put back.
- Q (plan-phase research, 2026-10-12): Stock jellyfin-web and the stock Chromecast receiver cannot be driven by the server (the receiver has no server connection; commands travel only on the Cast channel from the sender, and the web SyncPlay client ignores group commands while casting). How does the TV follow the group? → A: The server drives the TV itself: Pharos connects to the configured TV, plays the group's item on it, applies group commands to it and corrects its drift from the receiver's own position reports. The TV is offered to stock clients as a targetable device in their normal "play on" device list.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Cast my seat in a group to a TV (Priority: P1)

I am in a SyncPlay group watching something from my laptop or phone. I hit the
Cast button and pick my living-room TV. The picture moves to the TV, and the TV
is now *my seat in the group*: when anyone in the group plays, pauses or seeks,
the TV does the same, and the TV stays aligned with everyone else's playback
position as if it were any other member.

**Why this priority**: This is the whole feature. Casting today either drops the
user out of the group or leaves the TV playing independently, so the group's
shared experience breaks for exactly the person on the big screen.

**Independent Test**: Two members in one group (one on a browser, one casting to
a receiver). Play, pause and seek from each side in turn; the receiver and the
other member land within the group's normal sync tolerance every time, with no
manual rejoin.

**Acceptance Scenarios**:

1. **Given** I am a member of a playing group, **When** I start casting to a TV,
   **Then** the TV begins playback at the group's current position (not the
   start, not where I last was) and I remain a member of the group throughout.
2. **Given** my TV is a group member via cast, **When** another member pauses,
   **Then** the TV pauses within the group's normal sync tolerance.
3. **Given** my TV is a group member via cast, **When** another member seeks,
   **Then** the TV seeks to the same position and resumes together with the
   group.
4. **Given** I started casting, **When** I look at the group's member list,
   **Then** I appear once (as me, on the TV), not twice.

---

### User Story 2 - Drive the group from the sending device (Priority: P1)

While casting, my phone or laptop becomes the remote. Pressing pause, seek,
next-episode or stop on it acts on the group exactly as if I had pressed it on a
local player — the TV is not a separate, out-of-group playback that only I can
affect.

**Why this priority**: A cast TV with no working remote is unusable; and
commands that bypass the group would desync everyone else.

**Independent Test**: With the TV casting inside a group, press pause and seek
on the sending device and observe that every other member follows, and that the
TV follows too.

**Acceptance Scenarios**:

1. **Given** I am casting inside a group, **When** I pause from the sending
   device, **Then** the whole group (including the TV) pauses.
2. **Given** I am casting inside a group, **When** I seek from the sending
   device, **Then** the whole group seeks to that position.
3. **Given** I am casting inside a group, **When** another member changes the
   queue (next item, new item), **Then** the TV moves to that item and the
   sending device's remote shows it.

---

### User Story 3 - Stop casting without leaving the group (Priority: P2)

I stop casting (from the cast control, or the TV is switched off / loses
network). The group keeps going without disruption. I can resume watching on the
sending device, rejoining the group at the current position, or simply leave.

**Why this priority**: Casting sessions end messily (TV power-off, Wi-Fi drop).
The group must survive that, and I should land somewhere sensible.

**Independent Test**: Cast into a group, kill the receiver, and confirm the
other members were never frozen beyond the group's bounded freeze, and the
sender can resume locally in sync.

**Acceptance Scenarios**:

1. **Given** I am casting inside a group, **When** I stop casting deliberately,
   **Then** my sending device resumes as a normal member at the group's current
   position, and nobody else sees a disruption.
2. **Given** I am casting inside a group, **When** the TV drops off the network,
   **Then** the group is not held waiting on it beyond the existing bounded
   wait, and the sending device is offered resuming locally.
3. **Given** the TV reappears within the group's reconvergence window, **When**
   it reconnects, **Then** it reconverges with the group without my rejoining.

---

### User Story 4 - Join the group straight from the TV side (Priority: P3)

If I begin casting *first* and join a group afterwards (or a group is created
while I'm casting), the cast TV follows the group the same way it would had I
joined before casting.

**Why this priority**: Order of operations varies; the result should not.

**Independent Test**: Cast first, then join an existing group from the sending
device; confirm the TV adopts the group's position and follows it.

**Acceptance Scenarios**:

1. **Given** I'm casting outside any group, **When** I join a group from the
   sending device, **Then** the TV becomes my seat in that group and jumps to
   the group's position.
2. **Given** I'm casting inside a group, **When** I leave the group from the
   sending device, **Then** the TV stops following the group.

---

### Edge Cases

- Receiver is slow to buffer on cast start: the group must not be frozen beyond
  the bounded freeze, and the late-joining TV must not desync existing members.
- Receiver cannot decode the item (codec/container): same transcode negotiation
  as any other member; the group is never stalled on a member that cannot ack.
- Two devices of one user both try to cast into the same group at once: one seat
  per user-device pairing; the second request is rejected with a clear message
  rather than producing duplicate members.
- Sending device's app is backgrounded or closed mid-cast: the TV keeps
  following the group; remote control is simply unavailable until it returns.
- Rolling server deploy during a cast-in-group: the cast seat survives as any
  other member does.
- Group ends (everyone leaves) while casting: cast stops cleanly; no orphaned
  playback on the TV.
- Cast to a receiver that is already a member of another group.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: A group member MUST be able to cast to a TV and have the TV take
  over that member's seat in the group, with the member remaining in the group.
- **FR-002**: The cast TV MUST begin playback at the group's current position
  and play state when casting starts or the user joins a group while casting.
- **FR-003**: The cast TV MUST follow group play, pause, seek, queue and
  buffering-gate events, and its own pause/seek/stop MUST propagate to the group,
  as for any member.
- **FR-004**: The sending device MUST act as a remote: its transport controls
  MUST act on the group, not on an out-of-group playback, and any command a
  client addresses to the TV MUST be applied to the group, never to the TV alone.
- **FR-005**: A user casting MUST appear once in the group's member list, under
  their own identity, identifying the TV as the playing device.
- **FR-006**: Ending a cast (deliberate or by receiver loss) MUST NOT disrupt the
  rest of the group beyond the existing bounded freeze, and MUST NOT leave the
  user stranded: the sending device MUST be able to resume locally at the group's
  current position.
- **FR-007**: A cast seat MUST obey the same group guarantees as any member: it
  never stalls the group on an ack it cannot produce, a late joiner does not
  desync others, and a sub-2 s blip reconverges without rejoin.
- **FR-008**: Cast-in-group MUST work with stock, unmodified clients and the
  stock receiver app: the sender picks the TV from its ordinary list of devices
  it can play on, and no client or receiver patching is required.
- **FR-009**: Casting through the client's own Cast button MUST continue to behave
  exactly as it does today. A request to play on the TV by a user who is not in a
  group MUST be refused with a user-readable reason (standalone TV playback is out
  of scope).
- **FR-012**: The server MUST offer each configured TV as a device a stock client
  can choose to play on, listed only while the TV is reachable, and visible
  only to users allowed to use it.
- **FR-013**: The TV MUST never hold up the group: the server MUST NOT wait on an
  acknowledgement the receiver cannot produce, and MUST instead align the TV from
  the position the receiver reports.
- **FR-010**: The server MUST record, for every cast-into-group decision (seat
  handed over, rejected, ended, receiver lost), the user, sending and receiving
  devices, and the reason, so a desync or failed cast can be diagnosed by query
  without reconstructing state.
- **FR-011**: A request that would create a duplicate member for one user/device
  pairing MUST be rejected with a user-readable reason.

### Key Entities

- **Group member (seat)**: A participant in a SyncPlay group, identified by user
  and device; has play state, position, buffering/ready state.
- **Cast session**: The relationship between a sending device and a receiving
  TV for one user; owns the hand-over of that user's seat from sender to
  receiver and back.
- **Sending device (remote)**: The browser/phone that initiated the cast;
  issues commands, does not itself play.
- **Receiving device (TV)**: The cast receiver; plays the media and holds the
  user's seat while the cast is active. It is driven by the server, not by the
  sender.
- **Configured TV**: An administrator-declared receiver (name and network
  address) that clients can target.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With one member casting, play/pause/seek issued by any member is
  applied on the TV within 1 s p95 of the instant the group's other members apply
  it. The 500 ms bar for browser members is unchanged and is not relaxed for them.
- **SC-002**: After starting a cast inside a group, the TV is within 2 s of the
  group's position within 15 s of the user picking the TV, and stays within 2 s
  for the rest of the session absent a network fault. The 2 s figure reflects the
  receiver reporting its position about every 1.5 s; the spike (research.md)
  confirms or replaces it before implementation.
- **SC-003**: A user who starts casting stays in the group 100 % of the time
  (never dropped, never duplicated) across the test matrix of start, stop, TV
  power-off and sender backgrounding.
- **SC-004**: A receiver dropping off the network freezes the other members for
  no longer than the existing bounded freeze, in 100 % of test runs.
- **SC-005**: The full flow works in an end-to-end run with an unmodified web
  client as sender and a cast receiver, with no manual step beyond picking the
  TV.
- **SC-006**: Every cast-into-group event is findable by a single log query
  showing user, devices and reason.

## Assumptions

- "Chromecast feature" means a Google Cast receiver running the stock Jellyfin
  receiver app (the server already advertises its app id); no new receiver
  application is introduced. The server must be able to reach the TV over the
  network; TVs are declared by an administrator rather than auto-discovered.
- The casting user's seat moves to the TV (confirmed, see Clarifications): the
  sending device is a pure remote and stops playing locally while casting.
  Local playback as a second member, or the TV mirroring a still-playing
  sender, is out of scope.
- Primary acceptance client is jellyfin-web as sender, using its normal "play
  on another device" picker; native clients that offer the same picker are
  expected to work but are not the gate.
- Only the casting user's own seat is affected; casting never changes group
  membership, ownership or permissions for anyone else.
- Receiver network quality and decode capability are subject to the existing
  capability negotiation; no special-casing of receiver models.
- AirPlay, DLNA, or other cast protocols are out of scope.
- The existing group guarantees (sync tolerance, bounded freezes, no ack
  deadlocks, deploy resilience) are the bar; this feature must not lower them.
