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
5. **Push is a callback capability.** `join`/`create` take a `GroupSink`; the
   server pumps the member's `ServerMsg` stream into it. Dropping the returned
   `Membership` capability is a socket loss (V65 TTL applies), not a leave;
   `leave` is explicit.
6. **Group playback starts from the item.** "Watch together" creates the group
   with a one-item queue (`SetNewQueue`) at the current position. The full group
   queue is deferred.
7. **Invite link to an ended group** shows "This group has ended" and offers to
   start a new one; it never silently creates an empty group (spec edge case,
   T90a).
8. **Drift correction.** Within 150 ms do nothing; 150–1500 ms nudge
   `playbackRate` to 0.95/1.05 until under 50 ms; over 1500 ms (the oracle's
   `SYNC_TOL_MS`) hard seek. Thresholds are constants with unit tests.

## Contract (append-only, `schemaVersion` 5 → 6)

```capnp
# Session
group @9 (ctx :CallContext) -> (group :Group);

struct GroupMember { id @0 :Text; name @1 :Text; buffering @2 :Bool; isPharos @3 :Bool; }
struct GroupSummary { id @0 :GroupId; name @1 :Text; item @2 :ItemId; members @3 :List(GroupMember); }
struct GroupsResult { union { groups @0 :List(GroupSummary); error @1 :Error; } }
struct MembershipResult { union { membership @0 :Membership; error @1 :Error; } }
struct ClockResult { union { serverMs @0 :UInt64; error @1 :Error; } }

interface Group {
  list   @0 (ctx :CallContext) -> (result :GroupsResult);
  create @1 (ctx :CallContext, item :ItemId, positionMs :UInt64, sink :GroupSink) -> (result :MembershipResult);
  join   @2 (ctx :CallContext, id :GroupId, sink :GroupSink) -> (result :MembershipResult);
  clock  @3 (ctx :CallContext, clientMs :UInt64) -> (result :ClockResult);
}
interface Membership {
  unpause   @0 (ctx :CallContext) -> (result :Done);
  pause     @1 (ctx :CallContext, positionMs :UInt64) -> (result :Done);
  seek      @2 (ctx :CallContext, positionMs :UInt64) -> (result :Done);
  buffering @3 (ctx :CallContext, positionMs :UInt64, buffering :Bool) -> (result :Done);
  ready     @4 (ctx :CallContext, positionMs :UInt64, playing :Bool) -> (result :Done);
  leave     @5 (ctx :CallContext) -> (result :Done);
}
struct GroupUpdate {
  union {
    joined  @0 :GroupSummary;
    play    @1 :At;   pause @2 :At;   seek @3 :At;
    member  @4 :GroupMember;           # joined, or its buffering changed
    left    @5 :Text;                  # member id
    item    @6 :ItemId;                # the group moved to another item
    ended   @7 :Text;                  # reason: left, dissolved, notInGroup
    error   @8 :Error;
  }
  struct At { atServerMs @0 :UInt64; positionMs @1 :UInt64; }
}
interface GroupSink { update @0 (ctx :CallContext, update :GroupUpdate) -> (result :Done); }
```

`EventKind` gains `groupDrift @19; groupHardSeek @20; groupWait @21;` (FR-005,
016's closed catalogue grows append-only).

## Phases and PRs

PRs are ordered so each lands on its own and the UI work starts after the
player-shortcut PR (#607), which touches the same files.

1. **Plan** (this PR): plan.md and tasks.md.
2. **Server `Group`**: extract shared dispatch; schema; `rpc/group.rs`;
   `it_rpc` tests for create, join, a pause reaching a second member, a
   jellyfin member and a native member in one group, refusal without
   `SyncPlayAccess`, and an ended group.
3. **UI**: `pharos-client` wrappers; a group store with clock sync and drift
   correction; "Watch together" on the detail page; `/together` and
   `/together/{id}`; member strip on the watch page; telemetry; a two-browser
   Playwright test on `sync-oracle.ts` (needs a `data-item-id` on the player's
   `<video>`).

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
