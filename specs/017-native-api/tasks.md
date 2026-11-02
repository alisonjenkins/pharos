---
description: "Task list for the pharos native API (Cap'n Proto)"
---

# Tasks: pharos native API (Cap'n Proto)

**Input**: Design documents from `/specs/017-native-api/`

**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md),
[data-model.md](./data-model.md), [contracts/pharos.capnp](./contracts/pharos.capnp),
[quickstart.md](./quickstart.md)

**Tests**: INCLUDED. Constitution III is test-first, and spec §Observability
lists the tests by name. Every observability test names its disarm step: the
test must go red with the instrumentation removed, and that red output is kept
as evidence in the PR.

**Ids already cited elsewhere** (do not renumber): T001 (plan Phase 1,
research preamble), T010–T013 (plan Phase 2), T040 (spec SC-002, plan Phase 8,
quickstart §5). Gaps between phases are deliberate, so those ids stay where the
other documents put them.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: parallelisable — different files, no dependency on an incomplete task
- **[Story]**: US1…US6 from spec.md

## Path conventions

Single Rust workspace. Paths are repo-relative. Every command runs inside the
devShell: `nix develop --command …`.

## Commit discipline

Each **COMMIT** marker closes one atomic commit that compiles, passes
`just test-changed`, and reverts alone (CLAUDE.md). A test and the code that
turns it green land in the same commit; the red run is recorded in the PR,
not as a failing commit.

---

## Phase 1: Setup (toolchain and measurement)

**Purpose**: the `capnp` compiler everywhere cargo runs, and baseline numbers
taken with the pinned toolchain.

- [X] T001 Add `pkgs.capnproto` to `devShells.default.packages` and to `pharosCommonArgs.nativeBuildInputs` in `flake.nix` (crane deps, binary and `ciArgs` checks all inherit it), so `capnpc` finds `capnp` in the devShell, `nix build .#pharos`/`.#oci` and `nix flake check` — **COMMIT**. Then re-take research R8's three rows inside the devShell: rebuild the R1 spike (bootstrap → `resume()` → `library()` → `latest()` pipelined over a generic `AsyncRead`/`AsyncWrite`) in a scratch directory outside the repo, and re-measure the current `pharos-ui` wasm under the workspace release profile and under `opt-level="z"`, LTO, 1 CGU, stripped. Record raw and gzip bytes, `rustc --version`, `capnp --version` and the exact commands
- [X] T002 Record T001's numbers in a new §0 "wasm size baseline (R8)" in `specs/017-native-api/quickstart.md` (separate from §5's T040 table), and replace research.md's "host cargo" caveat with a pointer to it — **COMMIT**

**Checkpoint**: `capnp --version` prints 1.5.x in the devShell.

---

## Phase 2: Foundational — schema crate, instrumentation first

**Purpose**: `crates/pharos-api` with the schema, generated code for native and
wasm32, and the generated `Instrumented<S>` wrapper (research R4). No RPC
method may exist without it (V173), so it precedes every story.

**⚠️ CRITICAL**: no user-story work begins until this phase is complete.

### Crate and schema

- [X] T003 Add `capnp`, `capnp-rpc` and `capnpc` 0.27 to `[workspace.dependencies]` in `Cargo.toml`
- [X] T004 Create `crates/pharos-api` (`Cargo.toml`, `src/lib.rs`), add it to workspace `members`, and add `pharos-api` to `[final-excludes] workspace-members` in `.config/hakari.toml` **before** running `just hakari-regen` (otherwise hakari injects the workspace-hack dependency) — commit `Cargo.lock` and `workspace-hack` together with it — **COMMIT**
- [X] T005 Copy `specs/017-native-api/contracts/pharos.capnp` to `crates/pharos-api/schema/pharos.capnp`; `crates/pharos-api/build.rs` runs `capnpc::CompilerCommand` and `src/lib.rs` `include!`s the output under `pub mod pharos_capnp`, with any lint allow scoped to that module and justified in a comment (V17)
- [X] T006 Gate: `cargo build -p pharos-api --target wasm32-unknown-unknown` and `cargo clippy -p pharos-api --all-targets -- -D warnings` both pass (FR-001) — **COMMIT** (T005 + T006)
- [X] T007 Read the generated `Server` traits and `dispatch_call` in `OUT_DIR` and record their exact 0.27 signatures (method shape, `Params`/`Results`, return type) in a module comment in `crates/pharos-api/src/instrument.rs` before writing the generator

### Labels and registration surface

- [X] T008 [P] `crates/pharos-api/src/labels.rs`: `Outcome` enum with `label()` (`ok`, one per `Error.kind` variant: `not_found`, `not_permitted`, `invalid`, `limit`, `revoked`, `unavailable`, `other`, plus `failed` for a `capnp::Error`, `disconnected`, `unimplemented`, `cancelled`) and the metric-name constants `pharos_rpc_calls_total`, `pharos_rpc_duration_seconds`, `pharos_rpc_cancelled_total`
- [X] T009 **Done as**: the pass-through stage was not committed; instead each test was shown red by disarming the runtime (`CallScope::finish`, the drop guard, `register_metrics`, `redact::is_secret`) and the output kept for the PR. The generator also emits `SECRET_METHODS`, the methods whose params or results reach a `$secret` field. `build.rs` second stage: read the `CodeGeneratorRequest` (`capnp compile -o-`, via `capnpc` or the raw request) and emit `methods.rs`: a static table of every `(interface, method)` with snake_case labels, and the `(struct id, field)` / `(method, param)` pairs annotated `$secret`. Also emit a pass-through `Instrumented<S>` per interface that delegates to `S` without instrumentation, so T010–T013 compile and go red

### Tests first (spec §Observability) — must fail against T009's pass-through

- [X] T010 **Done as**: the in-process cases use a local capability; `tests/it_instrument/wire.rs` adds the over-the-wire cases, a two-party RpcSystem over a duplex pipe where the caller drops its call (`Finish`) or its connection, both counted `cancelled`. An `unimplemented` case covers a method with no handler. [P] Failing-handler test in `crates/pharos-api/tests/instrument.rs`: a `Library` handler whose `latest` returns `Error.kind.notFound("item-42")`, called through `Instrumented<_>` over an in-memory capnp-rpc two-party connection on a current-thread runtime. Assert: span `rpc.library.latest` (via `pharos_test_metrics::spans`), `pharos_rpc_calls_total{interface="library",method="latest",outcome="not_found"} == 1`, a `pharos_rpc_duration_seconds` sample, and a WARN line carrying `item-42`, `call_id` and the interface/method. A second case: a handler returning `capnp::Error` → `outcome="failed"`; a third: a successful call → `outcome="ok"` with the same span/log fields minus the failure ones (symmetry); a fourth: the caller drops the promise before the handler completes → `pharos_rpc_cancelled_total{interface="library",method="latest"} == 1` and `outcome="cancelled"`. Disarm: run against the pass-through, keep the red output
- [X] T011 [P] Label-distinctness test in `crates/pharos-api/tests/labels.rs`: every `Outcome::label()` distinct, every interface label and every `(interface, method)` pair in the generated table distinct, all `[a-z_]+`
- [X] T012 **Done as**: `every_secret_method_is_covered` compares the generated `SECRET_METHODS` with the calls the test makes, so a new `$secret` method fails until it is covered. [P] Redaction test in `crates/pharos-api/tests/redact.rs`: for every method with a `$secret` param or a param struct field annotated `$secret` (today `Bootstrap.resume.deviceToken`, `Bootstrap.login.password`, `Bootstrap.quickConnectPoll.secret`), call through `Instrumented<_>` with debug logging enabled and a known secret; assert the captured log and span fields never contain it but do contain a `<redacted>` marker (so the test is not vacuous). Results annotated `$secret` (`LoginOk.deviceToken`, `QuickConnectPending.secret`) likewise. Also assert the generated secret table is non-empty and matches the schema
- [X] T013 [P] Zero-registration test in `crates/pharos-api/tests/registration.rs`: after `pharos_api::register_metrics()`, every `(interface, method, outcome)` triple has a `pharos_rpc_calls_total` series at 0 and every `(interface, method)` a `pharos_rpc_cancelled_total` series at 0, before any call. Disarm: an empty `register_metrics` makes it red

### Implementation — turns T010–T013 green

- [X] T014 `crates/pharos-api/src/instrument.rs`: the runtime half the generated wrapper calls — `CallScope` opens `rpc.<iface>.<method>` with `call_id`, times the call, maps the result to an `Outcome`, records the counter and histogram, logs failures at WARN with the error variant and its `reason` value, and counts `cancelled` from a drop guard if the future is dropped before completion. The span carries `ctx.traceparent`; a process-wide parent hook (`set_trace_parent_hook`, a `fn(&Span, &str)`) lets the server set the remote parent through tracing-opentelemetry without pulling OTel into the wasm build (FR-006, V173)
- [X] T015 `build.rs`: the generated `Instrumented<S>` now routes every method through `CallScope`, reading the outcome back from the method's `result` union (`error` arm → `Error.kind`), with capability getters (no result union) mapped by `capnp::Error` only. Construction goes through `Instrumented::new`, the raw server is not exposed as a capability outside the crate (R4)
- [X] T016 `crates/pharos-api/src/redact.rs`: decoded, redacted debug rendering of params and results at `debug` level, target `pharos_api::rpc::<iface>`, driven by the generated `$secret` table
- [X] T017 `register_metrics()` in `crates/pharos-api/src/lib.rs`, generated from the method table — **COMMIT** (T007–T017: tests and the instrumentation that turns them green, with the disarm output in the PR)
- [X] T018 Gate: `cargo build -p pharos-api --target wasm32-unknown-unknown` still passes with `metrics`/`tracing` in the server half; feature-gate the server half behind `server` only if it does not
- [ ] T019 `nix build .#checks.x86_64-linux.clippy` succeeds with the new crate, proving `capnp` reaches the sandbox. Use the workspace check, not `.#pharos`: that builds only `pharos-server` and `pharos-transcode`, so it compiles this crate only once the server depends on it

**Checkpoint**: `cargo nextest run -p pharos-api` green; each of T010/T012/T013 shown red with its disarm step.

---

## Phase 3: Foundational — transport and bootstrap

**Purpose**: a client can connect, authenticate and reach `Session`. Blocks US1, US2, US4, US5.

- [x] T020 [P] `it_rpc` group: create `crates/pharos-server/tests/it_rpc/main.rs` with one `mod` per file, helpers via `#[path = "../common/mod.rs"]`
- [x] T021 Test (`it_rpc/connect.rs`): WebSocket upgrade at `/pharos/rpc` with a disallowed `Origin` is refused and logged with the origin; an allowed one connects and `Bootstrap.serverInfo` returns `schemaVersion` — Done as `it_rpc/connect.rs` + unit tests in `rpc/origin.rs`; `traceparent` → server span and the trace-parent hook move to T027
- [x] T022 `crates/pharos-server/src/api/pharos/rpc/mod.rs`: upgrade via `actix-ws`, byte-stream adapter (R3), `RpcSystem` spawned on the accepting worker with `actix_web::rt::spawn` (R2), `Origin` allow-list from `[server].allowed_origins`; installs the T014 trace-parent hook (test in `it_rpc/connect.rs`: a call with a known `traceparent` yields a server span with that trace id); `pharos_rpc_connections{transport}` gauge and connect/close log with close reason — **COMMIT** — Done as `rpc/socket.rs` behind `[server.native_api].enabled` (default off; `it_rpc/gate.rs`); the T014 trace-parent hook is not installed yet (T027)
- [x] T023 Test (`it_rpc/session.rs`): `POST /pharos/session` sets `pharos_session` (`HttpOnly; Secure; Path=/pharos; SameSite=Strict`); upgrade with the cookie + `resume("")` → `Session.me` returns the user; a bad token → `Error.kind.revoked`/`notPermitted` with the reason
- [x] T024 (resume + `Session.me` + refused sessions done in `rpc/bootstrap.rs`, `services/auth.rs`, generated `Refused` carrying a `Denial`; `POST /pharos/session` done too: `api/pharos/session_cookie.rs` over `services::auth::password_login`, plus `DELETE` to sign out; `Bootstrap.login` and `Session.logout` in `it_rpc/login.rs`) `crates/pharos-server/src/api/pharos/session_cookie.rs` and `rpc/bootstrap.rs`, `rpc/session.rs` over the ADR-0011 token store (R6, FR-003, FR-003a). `resume` fills `ResumeResult` (research R1, decided): on refusal, a session capability whose every call fails `revoked`, plus `status.error` — **COMMIT**
- [x] T025 (live drop: every revoke path announces on `services::auth::Revocations` and the user's open connections re-check at once, `it_rpc/revocation.rs` + `revocation_sites.rs`; login and `Session.logout` in `it_rpc/login.rs`; the cookie in `it_rpc/session.rs`; native Quick Connect in `it_rpc/quick_connect.rs` over `services::auth::quick_connect_login`) Test + impl: `Bootstrap.login` and Quick Connect return a `Session` and device token; logout/revoke drops live connections and clears the cookie — **COMMIT**
- [x] T025a Test + impl (first PR of phase 3b; #463 security review): an open `Session` notices its token being revoked. Re-validate the token on a bounded interval and on the device-revoke/logout path, and close live connections with close cause `revoked`; `pharos_rpc_closed_total{cause="revoked"}` + log. The whole-slice security review checks this landed — **COMMIT** — Done as the re-check timer in `rpc/socket.rs` (`revalidate_secs`, default 60) with `it_rpc/revocation.rs`; the immediate close on logout/revoke rides on T025
- [x] T026 (connection caps: unauthenticated server-wide, per user, auth deadline, decode limits; calls in flight per connection, `MAX_CALLS_IN_FLIGHT`; the subscription cap is T026a) [P] Limits (FR-010): per-connection message size, call concurrency, subscription count; `pharos_rpc_limit_total{limit}` + log naming the value; tests in `it_rpc/limits.rs` — **COMMIT**
- [ ] T026a Test + impl with `Events` (FR-010): cap subscriptions per connection; the one past the cap answers `limit` `subscriptions` and counts under `pharos_rpc_limit_total`; test in `it_rpc/limits.rs` — **COMMIT**
- [x] T027 [P] `pharos_rpc_bytes_total{direction}` and `pharos_rpc_client_schema_total{version}`, `Unimplemented` counter by method; tests — **COMMIT** — Bytes counted in `rpc::serve` (`it_rpc/traffic.rs`); client schema from the new `CallContext.schemaVersion` (schema version 2), once per connection, bucketed `unset`/version/`newer` (`it_instrument/client_schema.rs`); Unimplemented is `pharos_rpc_calls_total{outcome="unimplemented"}` by interface and method (`it_instrument/instrument.rs` `a_method_without_a_handler_is_counted_unimplemented`). Also from T021/T022: the trace-parent hook (`obs::rpc_trace_parent`) and the zeroed call series are installed at startup when the API is served
- [x] T028 ADR for Cap'n Proto RPC's threading model (R2) and the session-cookie flow (R6), `docs/adr/0029-*.md` + README row — **COMMIT** — ADR-0029

---

## Phase 4: US4 + US1 — shared services and the library (P1) 🎯 MVP

**Goal**: one implementation per behaviour (US4); home in one round trip (US1).

**Independent test**: `it_rpc home_in_one_batch` (quickstart §3); for each service, one service-level test plus the unchanged Jellyfin tests.

Each extraction is one commit: service + its unit test, Jellyfin handler rewired (its tests unchanged and green), native method + `it_rpc` test.

- [x] T029 [US4] `services/library.rs::views` ← `api/jellyfin` views handler; `Library.views` — **COMMIT**
- [x] T030 [US4] `services/library.rs::latest` ← `items.rs latest_items`; `Library.latest` — **COMMIT** — Done; `KindFilter` moved into `services::library` with it, and the native `ItemSummary` encoder (`rpc/library.rs write_item`) reads the Jellyfin projection, checked field by field against `/Users/{id}/Items/Latest` in `it_rpc/library.rs`
- [x] T031 [US4] `services/library.rs::resume` ← `items.rs resume_items` (typed `Vec<MediaKind>` instead of raw query parsing); `Library.resume` — **COMMIT** — Done; the typed filter is `KindFilter` (so naming only unknown types still matches nothing, B153), and `MediaTypes=` parsing stays in the Jellyfin layer (`items::media_types_filter`)
- [x] T032 [US4] `services/library.rs::next_up`; `Library.nextUp` — **COMMIT** — Done; the native call has no series scope (the schema takes only `limit`), the Jellyfin `SeriesId` scope goes through the same service
- [x] T033 [US4] `services/library.rs::query` (parent, kinds, sort, name prefix, filters, paging); `Library.query` — **COMMIT** — Done; `/Items` and `Library.query` share `services::library::query` over an `ItemQuery` in Jellyfin's token vocabulary (one parser, one `build_media_query`, one policy scope). A limit of 0 is a page of `QUERY_DEFAULT` (100); a named limit is capped at `QUERY_MAX` (1000). Kinds pharos only synthesises match nothing natively (B153); `/Items` collapses series/season/album/artist/box-set/playlist browses in its own paths, which T061 brings to the native call.
- [x] T034 [US4] `services/library.rs::detail`; `Library.detail` — **COMMIT** — Done for stored items: `/Items/{id}` and `Library.detail` share `services::library::detail` (access check, item, play state). A hidden item answers `notFound`, as a missing one does. Synthesised ids answer `notFound`; T062 gives them a native detail.
- [x] T035 [US1] `Library.home` aggregate over T029–T032; test `home_in_one_batch` pipelines `resume → session.library → library.home` over native TCP and asserts one flight after bootstrap — **COMMIT** — Done; `services::library::home` composes views, latest, resume and next up. `home_in_one_batch` decodes the client's frames up to the server's first reply and counts three rpc `Call`s there (a non-pipelined client counts one).
- [x] T036 [US4] `services/user_data.rs` set played / favorite; `UserData.*` — **COMMIT** — Done for stored items: the Jellyfin played/favourite routes and `UserData.setPlayed`/`setFavorite` share `services::user_data::update` (access check, item, read-modify-write, `UserDataChanged` to the user's jellyfin-web sockets). A hidden item answers `notFound` and is not written. Synthesised ids answer `notFound`; T063 gives them the cascade.
- [x] T037 [US4] `services/playback.rs` negotiate → `PlaybackGrant` (decision labels identical to `pharos_playback_decision_total`); report/stop — **COMMIT** — Done: `Playback.negotiate` calls `services::playback::grant`, which uses the same access check, negotiation, starting audio pick, transcoding URL (`delivery`), session registration and metric as `/Items/{id}/PlaybackInfo`. Capabilities become a one-profile-per-type `DeviceProfile`, and a direct play answers the `?tag=` capability URL. `PlaybackGrant.report`/`stop` go through `services::play_state` like `/Sessions/Playing`, and the stop ends the capability (B256). A hidden or missing item answers `notFound`, a book `notPermitted`. Follow-ups: T064–T067.
- [x] T038 [US1] [P] `Library.search` over the existing search path — **COMMIT** — Done; `/Search/Hints` and `Library.search` share `services::library::search_hits` (FTS match, then the policy's visible corpus), and the native call answers item matches only, with no aggregate hints. A limit of 0 is `SEARCH_DEFAULT` (25), capped at `QUERY_MAX`; blank text answers nothing. `native_search_agrees_with_the_jellyfin_search_hints_route` checks the ids and their order against `/Search/Hints`; a hidden library's match never appears (`native_search_leaves_out_a_hidden_library`)
- [ ] T039 [US4] Review checklist from US4 applied to every wire adapter: no branching business logic in `api/jellyfin/*` or `api/pharos/rpc/*` for migrated behaviours; record the result in the PR
- [ ] T040 [US1] SC-002 measurement gate: `just bench-native-vs-json` prints bytes and wasm decode time for a 500-item `Library.query` page vs the Jellyfin `/Items` JSON on the seeded library; record the first run in quickstart §5 as the gate and add a CI job failing on > 10 % regression — **COMMIT**

---

## Phase 5: US2 — live updates without polling (P1)

**Independent test**: subscribe, mark an item played from a second connection, receive one `userData` event with the next `seq`.

- [ ] T041 [US2] Test: per-user `seq`, replay after `afterSeq`, `resync` when older than the window (data-model §Events)
- [ ] T042 [US2] `services/events.rs`: services publish to the existing fan-out; per-user bounded replay window (10 000 events / 15 min) — **COMMIT**
- [ ] T043 [US2] `rpc/events.rs` `Events.subscribe` with `EventSink`; `pharos_rpc_subscriptions{topic}`, `pharos_rpc_events_total{topic,outcome}`, `pharos_rpc_event_lag_seconds{topic}`; tests with disarm — **COMMIT**
- [ ] T044 [US2] Multi-replica distribution per ADR-0016 (postgres arm; run `just test-postgres`) — **COMMIT**
- [ ] T045 [US2] Capability lifecycle counts (issued / released / revoked by type) and a leak test — **COMMIT**

---

## Phase 6: US1 — client core (`pharos-client`)

- [ ] T046 [US1] Add `pkgs.capnproto` to `pharosUiBundle`'s `nativeBuildInputs` in `flake.nix` once `pharos-ui` depends on `pharos-api`. Create `crates/pharos-client` (hakari final-exclude) with the `Transport` trait; native TCP transport used by `it_rpc` — **COMMIT**
- [ ] T047 [US1] Connection, resume, reconnect with backoff + resubscribe; typed `Disconnected` for in-flight calls — **COMMIT**
- [ ] T048 [US1] Client-side call instrumentation (V173 client half): interface, method, latency, outcome, bytes, call_id, trace id — **COMMIT**
- [ ] T049 [US1] Browser WebSocket transport (`cfg(wasm)`), arraybuffer byte stream; Playwright spec in `just compat-dioxus` for login + reload resume + no token in `localStorage` (quickstart §4); the cookie-blocked edge case surfaces a message and logs the browser's reason — **COMMIT**
- [ ] T050 [US1] Query cache primitives and event dispatch — **COMMIT**
- [ ] T051 [US1] FR-012 guard: a test fails if a view added after this spec reaches the REST adapter — **COMMIT**

---

## Phase 7: US5 + US6 — tooling and safe evolution

- [ ] T052 [US5] `pharos rpc call <iface>.<method> --json` CLI (schema-driven) — **COMMIT**
- [ ] T053 [US5] `pharos rpc tap` (admin): decoded, redacted summaries per user/device; extend T012 to assert the tap — **COMMIT**
- [ ] T054 [US6] `crates/pharos-api/schema/compat/main.bin` + `just` refresh recipe (R7) — **COMMIT**
- [ ] T055 [US6] Schema-compat test: fails on a removed, renamed or retyped field or method ordinal; prove it with a deliberately broken copy — **COMMIT**

---

## Phase 8: Polish and cross-cutting (US3 and docs)

- [ ] T056 [US3] Confirm SC-003 across the branch: `just test` and `just compat-playwright-full` green on every commit of the feature
- [ ] T057 [P] `docs/observability.md`: the four named queries from spec §Observability — **COMMIT**
- [ ] T058 [P] `docs/jellyfin-mapping.md`: native ↔ Jellyfin method mapping — **COMMIT**
- [ ] T059 Angie sidecar config for the `/pharos/rpc` upgrade and keep-alive below the ingress idle timeout — **COMMIT**
- [ ] T060 After deploy: run quickstart §6 queries and record real output (SC-004)
- [ ] T061 [US4] Native `Library.query` for synthesised kinds: move `/Items`' box-set, playlist, music (album/artist) and virtual series/season paths behind `services::library` so `kinds=[series]` and the like list what `/Items` lists; agreement test per kind — **COMMIT**
- [ ] T062 [US4] Native `Library.detail` for synthesised ids (library, series, season, album, artist, genre, box set, playlist): move `/Items/{id}`' synth branches behind `services::library::detail`; agreement test per kind — **COMMIT**
- [ ] T063 [US4] `UserData.setPlayed` / `setFavorite` on a synthesised series or season id: cascade over its visible episodes as the Jellyfin routes do (B36), instead of `notFound`; agreement test with the Jellyfin cascade — **COMMIT**
- [ ] T064 [US4] Native transcoding URLs carry no token: the HLS master, variant and segment routes and `/audio/{id}/universal` must accept the grant's PlaySessionId as the capability, as `/stream?tag=` does (B75/B256), so a native player needs no `api_key` in a URL. Until then a native player sends its session token in the `Authorization` header on those URLs; a bare transcoding URL answers 401.
- [ ] T065 [US4] `Capabilities.hdr` is read but not used: the negotiator has no HDR condition, so an SDR-only player is offered an HDR source to direct-play. Express it as a `VideoRangeType` codec condition once the negotiator evaluates one.
- [ ] T066 [US4] A native grant registers no `/Sessions` entry: `report` updates only a session that `/Sessions/Playing` started, so native playback is missing from the dashboard and remote control. Start one from the grant with the connection's device.
- [x] T067 [US4] `Decision` names no subtitle: the user's default subtitle pick (`DefaultSubtitleStreamIndex`: remembered, then `SubtitleMode` and languages) is not sent to a native player. Add the picked audio and subtitle to `Decision` (append-only) through a shared `pick_subtitle`. — **COMMIT** — Done (schema 4); `services::playback::pick_subtitle` is shared with PlaybackInfo, `Decision.audio` / `Decision.subtitle` carry the picks, and `PlaybackGrant.report` gained `audio` / `subtitle` so a switch made in the player without renegotiating is remembered. The same schema bump gives `ItemDetail` its audio and subtitle tracks, trickplay sprite sets and skip segments (013 FR-003..FR-005), through `services::watch` and `services::segments`.
- [ ] T070 [US4] Skip segments have no per-user preference (013 FR-005: ask / auto-skip / never, per segment type): the native watch page always asks with a button. Add a `playback.skip` key to the Prefs allow-list and have the player auto-skip or hide per type.
- [ ] T071 [US4] ASS/SSA subtitles reach the native player as their WebVTT conversion, losing styling and positioning; 013 FR-003 leaves the JS renderer (JASSUB / libass-wasm) open. Pick one and deliver the raw `.ass` to it.
- [x] T068 [US4] `Prefs` has no server side: `Prefs.get` / `Prefs.set` (schema `interface Prefs`) answer nothing, so 010 themes and 011 keymaps can only live per browser. Per-user key/value store behind a `pharos-core` trait (sqlite + postgres arms, `just test-postgres`), key allow-list and value size cap, `rpc/prefs.rs` on the session's user; signal `pharos_rpc_calls_total{interface="prefs"}` plus a WARN carrying the rejected key and size. When Prefs lands, store the diagnostics opt-in there and have `Telemetry.submit` refuse sessions whose user has not opted in. — **COMMIT** — Done; `UserPrefStore` (pharos-core) over a `user_prefs` table (migration 0060, sqlite and postgres, conformance on both), served by `rpc/prefs.rs` with a key allow-list (only `diagnostics.opt_in`) and a 4096-byte value cap; an unknown key answers `notPermitted`, an oversize value a `limit`, each with a WARN carrying the key and size and never the value; calls count under `pharos_rpc_calls_total{interface="prefs"}`. `NativeApi::pref` / `set_pref` wrap the calls. The Appearance toggle reads and writes the opt-in through Prefs and an old localStorage flag is removed, not honoured. `Telemetry.submit` drops batches from users who have not opted in: `notPermitted`, counted as `pharos_client_events_dropped_total{reason="not_opted_in"}`, debug log with the user id; the session caches the pref for 5 s. ADR-0037. Gap closed: the sqlite-to-postgres `migrate` tool copies `user_prefs` (after `display_preferences`), covered by `migrate_roundtrip`.
- [x] T069 [US4] `Telemetry.submit` has no server side, so client failures reach no log: the native UI's playback errors (`EventKind.playbackError`, `playbackDecision`) and theme fallbacks (`themeFallback`) print only to the browser console. Implement `Telemetry` on the server: bounded batch size and rate per session, each event logged at its `Level` with kind, route, item, play session and fields (redacted, V8), and `pharos_client_events_total{kind,level}`. Then wire the UI's `pharos: playback error` and `pharos: theme fallback` lines to submit, and name the Loki query in 010 quickstart §4 — **COMMIT** — Done; submits are opt-in and default off (the user turns on "Send diagnostics to this server" under Appearance); `rpc/telemetry.rs` serves `submit`, bounded at 50 events or spans per batch and 20 batches then 1 per second per session, logs each event at its level with `<redacted>` for credential-looking fields and an 8-character play session prefix, and counts `pharos_client_events_total` and `pharos_client_events_dropped_total`. `$secret` on `Field.value` and `PlaySessionId.value` keeps debug dumps clean. Spans are counted in a debug line (no span sink). The UI submits `playbackError` and `themeFallback` from `native/web.rs` and `native/app.rs`; the Loki query is in 010 quickstart §4. Gaps: the UI sends no item id. Update (T068): the server now enforces the opt-in per user (`diagnostics.opt_in` in Prefs, not localStorage); a batch from a user who has not opted in is dropped as `not_opted_in`, and events raised before sign-in are dropped, not held, because the choice is unknown until Prefs is read.

---

## Dependencies

- Phase 1 → Phase 2 → Phase 3 → {Phase 4, Phase 5}. Phase 5 needs T036 (user data publishes events).
- Phase 6 needs Phase 3; T049 needs T035 for the Playwright home check.
- T040 needs T033 and the wasm decode harness from T046/T049's codec path (native decode can be measured first; the wasm number fills in when T049 lands).
- Phase 7 needs Phase 2 (T054–T055) and Phase 4 (T052–T053).

## Parallel opportunities

- T008 alongside T007; T010–T013 are four independent test files.
- T026, T027 after T022.
- T038 alongside T036/T037 (different service files).
- T057, T058 any time after their subjects land.

## Implementation strategy

MVP = Phases 1–4: instrumented schema crate, authenticated connection, the
first tranche of shared services, home in one batch, and the SC-002 gate. Each
later phase ships independently behind the same instrumentation.
