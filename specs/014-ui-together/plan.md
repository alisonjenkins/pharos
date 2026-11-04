# Implementation Plan: watch together in pharos-ui (MVP)

**Spec**: [spec.md](./spec.md) · **Created**: 2026-11-03

## Scope of this plan

The first cut is what Alison asked for on 2026-11-03 ("no sync play"): a
pharos-ui user can start a group from an item, others join it, and every member
plays in step, including jellyfin-web members of the same group.

| In | Deferred (own plan later) |
|----|---------------------------|
| US1: "Watch together" on an item, the Together page (`/together`), invite link (`/together/{group}`) | US2 chat and emoji reactions |
| US2 minimum: member strip with buffering state, leave | US2 group queue and vote-skip (overlaps 020/021, owned elsewhere) |
| FR-001 group client over the native `Group` interface | US3 remote control of other sessions (FR-003) |
| FR-002 clock sync and drift correction | US4 watch a URL (FR-004) |
| FR-005 drift, hard-seek and buffering-wait signals | US5 group badges on cards and the mini-player |

## Clarifications assumed (no `/speckit-clarify` round)

Recorded here so review can overturn them; each is cheap to change before the
UI lands.

1. **Native `Group`, not `/sync/v1/ws`.** ADR-0028 has pharos-ui speak only the
   native API, and 017's Session comment reserves `Group` for 014. The
   `/sync/v1/ws` path stays for other native clients (V20).
2. **The as-built command set, not the leader design.** Groups run the
   readiness-gate protocol of `docs/group-sync-protocol.md` §12, which is what
   jellyfin-web members speak. The native interface therefore exposes the same
   operations the Jellyfin SyncPlay handlers send to the actor (`Unpause`,
   `PauseShared`, `SeekTo`, `BufferingStart/End`, `MemberReady`,
   `SetNewQueue`), not `LeaderPlay`.
3. **One shared dispatch path (V172).** The guard, recover and dispatch helpers
   in `api/jellyfin/syncplay.rs` (`dispatch_checked`, `recover_and_apply`) move
   to a shared module both surfaces call, so V160 (no success without reaching
   the actor) and V201 (`SyncPlayAccess` gates list, join and create) hold on the
   native path by construction.
4. **Member identity.** A native member registers in `SessionHub` under the
   call's `ctx.device` (stable per browser, already sent on every call), so V25's
   deterministic member id covers reconnects. A call with no device is refused
   `invalid{field: "ctx.device"}`.
5. **Push is a callback capability; liveness is explicit.** `join`/`create` take a `GroupSink`; the
   server pumps the member's `ServerMsg` stream into it. Dropping the returned
   `Membership` capability is a socket loss (V65 TTL applies), not a leave;
   `leave` is explicit. The client calls `Membership.ping` every 30 s, which
   feeds the T83 ghost prune (150 s TTL), as jellyfin-web's socket `KeepAlive`
   does.
6. **Group playback starts from the item.** "Watch together" creates the group
   with a one-item queue (`SetNewQueue`) at the current position. The full group
   queue is deferred.
7. **Invite link to an ended group** shows "This group has ended" and offers to
   start a new one; it never silently creates an empty group (spec edge case,
   T90a).
8. **Drift correction has one owner at a time.** Server commands (play, pause,
   seek, item) always win, and the client makes no correction of its own for
   3 s after one. Otherwise the client owns steady-state drift: under 150 ms it
   does nothing; from 150 to 1500 ms it nudges `playbackRate` to 0.95 or 1.05
   until under 50 ms; over 1500 ms (the oracle's `SYNC_TOL_MS`) it hard-seeks
   to the expected position and reports `groupHardSeek`. The server's
   `force_reseek_one` is outage recovery, not drift control, and is untouched.
   Thresholds are constants with unit tests.
9. **V201 on every native path.** `list` filters through `may_see_queue`,
   `join` through `may_join`, `create` through `may_queue`, each counting
   `pharos_syncplay_group_hidden_total` with a native route label, as the
   Jellyfin handlers do.
10. **Stale readiness.** `buffering` and `ready` carry the playlist item id the
    member is playing, and `GroupUpdate.item` hands it out, so the B37
    stale-Ready guard covers native members.

## Contract (append-only, `schemaVersion` 6 → 7, one bump for all of 014's first cut)

```capnp
# Session
group @9 (ctx :CallContext) -> (group :Group);

struct GroupMember { id @0 :Text; name @1 :Text; buffering @2 :Bool; }
struct GroupSummary { id @0 :GroupId; name @1 :Text; item @2 :ItemId; members @3 :List(GroupMember); }
struct GroupsResult { union { groups @0 :List(GroupSummary); error @1 :Error; } }
# The ResumeResult pattern, not a union, so calls on `membership` pipeline. A
# refused create or join still returns a membership; its every call fails with
# the same Error that `status` carries.
struct MembershipResult { membership @0 :Membership; status @1 :Done; }
struct ClockSample { clientMsEcho @0 :UInt64; recvMs @1 :UInt64; sendMs @2 :UInt64; }
struct ClockResult { union { sample @0 :ClockSample; error @1 :Error; } }

interface Group {
  list   @0 (ctx :CallContext) -> (result :GroupsResult);
  create @1 (ctx :CallContext, item :ItemId, positionMs :UInt64, sink :GroupSink) -> (result :MembershipResult);
  join   @2 (ctx :CallContext, id :GroupId, sink :GroupSink) -> (result :MembershipResult);
  clock  @3 (ctx :CallContext, clientMs :UInt64) -> (result :ClockResult);   # T1 → (T1 echo, T2, T3)
}
interface Membership {
  unpause     @0 (ctx :CallContext) -> (result :Done);
  pause       @1 (ctx :CallContext) -> (result :Done);   # PauseShared: the actor holds the position
  seek        @2 (ctx :CallContext, positionMs :UInt64) -> (result :Done);
  buffering   @3 (ctx :CallContext, positionMs :UInt64, buffering :Bool, playlistItemId :Text) -> (result :Done);
  ready       @4 (ctx :CallContext, positionMs :UInt64, playing :Bool, playlistItemId :Text) -> (result :Done);
  leave       @5 (ctx :CallContext) -> (result :Done);
  ping        @6 (ctx :CallContext) -> (result :Done);   # MemberPing, every 30 s (T83 ghost TTL is 150 s)
  clockReport @7 (ctx :CallContext, t1 :UInt64, t2 :UInt64, t3 :UInt64, t4 :UInt64) -> (result :Done);   # ObserveClock: the server's RTT for its lead time
}
enum EndReason { left @0; notInGroup @1; }
struct GroupUpdate {
  union {
    joined  @0 :GroupSummary;
    play    @1 :At;   pause @2 :At;   seek @3 :At;
    member  @4 :GroupMember;           # joined, or its buffering changed
    left    @5 :Text;                  # member id
    item    @6 :ItemAt;                # the group's current entry, from PlayQueue
    ended   @7 :EndReason;
  }
  struct At { atServerMs @0 :UInt64; positionMs @1 :UInt64; }
  struct ItemAt { item @0 :ItemId; playlistItemId @1 :Text; positionMs @2 :UInt64; playing @3 :Bool; }
}
interface GroupSink { update @0 (ctx :CallContext, update :GroupUpdate) -> (result :Done); }
```

The same bump appends `groupDrift @19; groupHardSeek @20; groupWait @21;` to
`EventKind` (FR-005), with the kind→label match in `rpc/telemetry.rs` and 016's
`contracts/events.md`.

## Phases and PRs

PRs are ordered so each lands on its own and the UI work starts after the
player-shortcut PR (#607), which touches the same files.

1. **Plan** (this PR): plan.md and tasks.md.
2. **Server `Group`**: extract shared dispatch; schema; `rpc/group.rs`;
   `it_rpc` tests for create, join, a pause reaching a second member, a
   jellyfin member and a native member in one group, refusal without
   `SyncPlayAccess`, a group hidden by V201, ping, clock and an ended group.
3. **UI**: `pharos-client` wrappers; a group store with clock sync and drift
   correction; "Watch together" on the detail page; `/together` and
   `/together/{id}`; member strip on the watch page; telemetry; a two-browser
   Playwright test on `sync-oracle.ts` (needs a `data-item-id` on the player's
   `<video>`); a mixed pharos-ui and jellyfin-web group in Playwright; and
   SC-001's 10-minute drift run in `just compat-syncplay` with a pharos-ui
   member.

## US6: who has seen what (schemaVersion 11 → 12)

- **Contract.** `Membership.seen @8 (ids :List(ItemId)) -> SeenResult` with
  `MemberSeen { member, items :List(ItemSeen) }` and `ItemSeen { id, played,
  positionPct, playedCount, totalCount }`. At most 200 ids, else `limit`.
- **Server.** `services::group_seen` resolves ids (a stored item, or a
  synthesised series or season to its episodes), drops what the caller may not
  see, then reads each participant's state with `user_data_bulk` for the user
  recorded on that participant (`GroupSnapshot.participant_users`, B425).
  Counted in `pharos_group_seen_total{outcome}`. ADR-0054 records the privacy
  choice.
- **UI.** `GroupClient::seen` and `Session::seen`; the pure `seen` module words
  a mark; `use_group_seen` in app.rs fetches through the group session and
  fetches again when the roster (Member/Joined/Left) or the ids change.
  `InThisGroup`, `SeenDots` and the strip's mark are pure components.

## Observability (FR-005)

"Group feels out of sync" is answered by `groupDrift` samples (ms, per group,
1 per 10 s while playing), `groupHardSeek` and `groupWait` (ms spent waiting on
others) from the client, beside the server's existing
`pharos_syncplay_forced_reseek_total` and the V173 per-method RPC metrics for
`Group.*` and `Membership.*`.

## Risks

- capnp-rpc is `!Send`; the member's `ServerMsg` receiver is pumped on the
  connection's local task, like the session's other capabilities.
- Multi-replica: a native member on a non-owning replica gets messages through
  `BusDelivery` exactly as a Jellyfin member does; covered by keeping the hub
  path shared, not by a new test in this cut.
