# Implementation Plan: SyncPlay Cast Target

**Branch**: `009-syncplay-cast` | **Date**: 2026-10-12 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/009-syncplay-cast/spec.md`

## Summary

A group member picks a configured TV from their client's normal "play on another device" list; Pharos connects to the TV as the Cast sender, plays the group's item on it, and makes the TV a server-driven group seat. Research ([research.md](research.md)) showed the stock receiver cannot be driven by the sender-side SyncPlay client or by the server over any existing channel, so the server speaks Cast itself, applies group commands to the TV, and corrects drift from the receiver's own progress reports. The seat never gates the group (V139/V161). Unmodified clients and the unmodified receiver app are kept (V1).

## Technical Context

**Language/Version**: Rust stable (workspace `rust-version = 1.80`), edition 2021

**Primary Dependencies**: actix-web, tokio (mpsc actors), existing `pharos-sync`; new: a CASTV2 implementation behind a trait (`rust-cast` or `rustls` + `prost`, chosen in spike S1; licence must suit AGPL-3.0-or-later)

**Storage**: group snapshots in `sync_groups` (existing); `PersistMember` gains `kind`/`cast` with serde defaults. No new tables. TV tokens in the existing token store.

**Testing**: `cargo nextest` (+ doctests); in-process receiver emulator; `group_conformance.rs`, `hub_integration.rs`, `syncplay_restart_recovery.rs`, `jellyfin_sessions.rs`; Playwright `just compat-syncplay` extension; manual real-TV run

**Target Platform**: Linux server (k3s pod, Nix-built OCI image)

**Project Type**: web-service (Cargo workspace)

**Performance Goals**: TV applies a group command within 1 s p95 (SC-001); within 2 s of group position (SC-002, to be confirmed by spike S2)

**Constraints**: no `unwrap`/`expect` (V17); no blocking on the TV in the group actor; one connection per TV owned by one replica; secrets never logged (V8)

**Scale/Scope**: a handful of TVs, one seat per user per group, units not thousands

## Constitution Check

*GATE: passes before research; re-checked after design (below).*

| Principle | Verdict | Note |
|-----------|---------|------|
| I. Wire compatibility (V1, V7, V38, V39, V16) | PASS | No new routes; TV appears as a typed `SessionInfoDto` in `GET /Sessions`; `Stop` sent is an enum member. No UI change, so V16 untouched. Existing stock-Cast-button behaviour unchanged. |
| II. Group sync beats Jellyfin (V19, V21, V25, V31, V139) | PASS | Seat excluded from gating by kind; lost TV bounded (V31 style); persisted `kind` and re-created connection satisfy V25. |
| III. Test-first + prove by query | PASS | First commit is instrumentation (metrics + logs), then emulator tests red-first; disarm check in quickstart; queries named in quickstart §4. |
| IV. Never panics / leaks / lies (V8, V9, V10, V17) | PASS | TV token hashed at rest, bounded, revoked on end, never logged; virtual sessions filtered per user (V9); rejection never reported as success (V160 spirit). |
| V. Types over conventions, actors over locks (V18, V60) | PASS | `kind` is a closed enum; `CastDriver` is an actor owning the connection; `CastTransport` trait at the IO edge. |

**Post-design re-check**: unchanged. One new risk recorded: the single-owner TV connection under multi-replica (R3).

## Project Structure

### Documentation (this feature)

```text
specs/009-syncplay-cast/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/
│   └── cast-surface.md
├── checklists/requirements.md
└── tasks.md             # /speckit-tasks — not created here
```

### Source Code (repository root)

```text
crates/
├── pharos-cast/                      # NEW
│   └── src/
│       ├── lib.rs
│       ├── transport.rs              # CastTransport trait + CASTV2 impl
│       ├── driver.rs                 # CastDriver actor (state machine; drives one TV)
│       ├── drift.rs                  # pure planner: plan(target, observed, now, state) -> Action
│       ├── messages.rs               # typed connectsdk command/report DTOs
│       └── fake.rs                   # in-memory receiver emulator (test + compat harness)
├── pharos-sync/src/
│   ├── group.rs                      # MemberKind, follower_ids exclusion, persist kind/cast
│   └── delivery.rs                   # sink variant for a cast adapter
├── pharos-server/src/
│   ├── config.rs                     # [cast] block
│   ├── cast.rs                       # NEW: registry of seats, hand-off/restore, virtual sessions, token mint/revoke
│   ├── api/jellyfin/sessions.rs      # route interception for virtual TV ids
│   └── api/jellyfin/syncplay.rs      # leave ends owned seat
└── pharos-jellyfin-test-client/      # SyncPlay + socket + session-command helpers
compat-playwright/                    # cast spec using the emulator
```

**Structure Decision**: a new `pharos-cast` crate isolates the protocol and the emulator behind a trait (V12; keeps `libav`/`workspace-hack` churn out of `pharos-sync`); membership semantics live in `pharos-sync` where the gate is; orchestration and the Jellyfin surface live in `pharos-server`.

## Phase order

0. Instrumentation commit (metrics, log events, `label()` methods with distinctness test) — ships and can be read before the behaviour.
1. Spikes S1–S4; record results in `research.md`.
2. `MemberKind` + persistence + gate exclusion in `pharos-sync` (red tests first).
3. `pharos-cast` (transport trait, emulator, drift planner, `CastDriver`).
4. Server orchestration: config, virtual sessions, route interception, hand-off/restore, token mint/revoke, leave/dissolve cleanup.
5. Distributed ownership + restart hydration of the connection.
6. Compat harness + real-TV run; invariants/bugs appended.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|--------------------------------------|
| New crate `pharos-cast` | Protocol + emulator + planner are testable with no DB or network (V12) and have their own dependency set | Putting it in `pharos-server` would drag the protocol dep into every server build and mix IO into group logic |
| Server speaks Cast | Stock receiver has no other control channel (R1) | Patching jellyfin-web breaks V1 and needs the sender tab open; "degrade only" misses the goal |

## New invariants to append (proposed ids V168+; landed as V172-V175 because main took V168-V171 first)

- V172 (proposed V168): a cast seat never contributes to a readiness gate or buffering freeze; the group never waits on an ack the TV cannot produce.
- V173 (proposed V169): a command addressed to a virtual TV session is applied to the group, never to the TV alone.
- V174 (proposed V170): every cast-into-group decision (started, rejected, ended, lost, reconnected) records user, sender device, TV and the offending reason value, success and failure alike.
- V175 (proposed V171): a TV-bound token is revoked on every exit from a cast session.
