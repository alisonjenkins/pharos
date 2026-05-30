# pharos — structure evaluation

Snapshot of the current crate layout, dependency graph, and internal cluster shape of `pharos-server`, plus a ranked list of split candidates aimed at faster test feedback and better reusability.

Source data captured 2026-05-29 against `main` at commit `4bf59ad` (P53). Re-run the `just diagrams` recipe and `wc -l crates/pharos-server/src/*.rs` to refresh.

## 1. Filesystem layout

```
pharos/
├── Cargo.toml                      # workspace root, 8 members
├── flake.nix                       # pinned toolchain + ffmpeg + dx
├── justfile                        # test / hakari / compat recipes
├── .config/nextest.toml            # per-crate test-thread overrides
├── SPEC.md                         # spec-driven (T1..T27 in §T)
│
├── crates/
│   ├── pharos-core/                # LEAF — domain traits, auth/secret
│   ├── pharos-scanner/             # fs walk + ffprobe parse
│   │   └── benches/                # criterion: parse, hash
│   ├── pharos-server/              # ★ monolith — bin `pharos` + lib
│   │   ├── src/                    # 19 top-level modules, ~5.4k LOC
│   │   │   ├── main.rs router.rs lib.rs
│   │   │   ├── api/jellyfin/       # HTTP handlers + DTOs
│   │   │   ├── middleware/
│   │   │   ├── sync/               # WS state machine (6 files)
│   │   │   ├── auth.rs sessions.rs quick_connect.rs transcode_sessions.rs
│   │   │   ├── hls_cache.rs image_cache.rs trickplay_cache.rs subtitle_cache.rs
│   │   │   ├── ssdp.rs dlna.rs live_tv.rs
│   │   │   ├── state.rs config.rs cli.rs obs.rs health.rs
│   │   └── tests/                  # 55 integration files (see §3)
│   ├── pharos-store-sqlx/          # feat: sqlite (default) | postgres
│   ├── pharos-transcode/           # feat: backend-spawn | backend-lib
│   ├── pharos-ui/                  # Dioxus / WASM (web feat)
│   └── pharos-jellyfin-test-client/# test-only Jellyfin wire client
│
├── workspace-hack/                 # cargo-hakari unified deps
│
├── docs/                           # architecture + protocol notes
│   ├── architecture.md
│   ├── group-sync-protocol.md
│   ├── jellyfin-mapping.md
│   ├── jellyfin-parity-audit.md
│   ├── dioxus-parity-audit.md
│   ├── observability.md
│   └── structure-eval.md           # ← this doc
│
├── scripts/dev-stack.sh            # local docker/podman stack
└── compat-playwright/              # E2E TS tests (T29 Phase 3)
```

## 2. Workspace dep graph

Source: [`structure-eval-deps.d2`](./structure-eval-deps.d2) — render via `just diagrams`.

```d2
direction: down

server: "pharos-server\n(bin + lib, ~5.4k LOC)" {
  style.fill: "#f8d7da"
}
core: "pharos-core\nLEAF — domain traits" {
  style.fill: "#d4edda"
}
scan: "pharos-scanner"
store: "pharos-store-sqlx\nfeat: sqlite | postgres"
tx: "pharos-transcode\nfeat: spawn | lib"
ui: "pharos-ui\nDioxus / WASM" { style.fill: "#fff3cd" }
tc: "pharos-jellyfin-test-client" { style.fill: "#fff3cd" }

server -> core
server -> scan
server -> store
server -> tx
server -> tc: dev { style.stroke-dash: 4 }
scan -> core
store -> core
tx -> core
```

Observations:

- **Clean DAG, no cycles.** `pharos-core` is the only true leaf in the production cone; all middle-layer crates (`scanner`, `store-sqlx`, `transcode`) depend on it and nothing else.
- **`pharos-server` is the sole root.** Carries the binary + every middle-layer crate. Anything you can lift out of it ships immediately to anyone reusing the middle layers.
- **`pharos-ui` and `pharos-jellyfin-test-client` are detached** from the production cone. They're coupled to the server by Jellyfin wire format and by convention, not by Cargo. This means changes to either compile and test without touching the server — already a strong reuse story; no action needed.
- **`workspace-hack`** is a cargo-hakari artifact (P53). Ignored for architecture.

## 3. `pharos-server` internal clusters

Source: [`structure-eval-server.d2`](./structure-eval-server.d2) — render via `just diagrams`.

Six containers group the 19 top-level modules into would-be-a-crate boundaries. Orange = top split candidate. All clusters currently funnel through `state::AppState`, which is the main coupling point a split has to address.

```d2
direction: right

http: "HTTP surface" {
  main: "main.rs (403)"
  router: "router.rs (47)"
  apij: "api/jellyfin/"
  mw: "middleware/"
  health: "health.rs (198)"
}

auth_sess: "Auth / sessions" {
  auth: "auth.rs (183)"
  sess: "sessions.rs (281)"
  qc: "quick_connect.rs (290)"
  txs: "transcode_sessions.rs (185)"
}

caches: "Caches\n~1720 LOC — unit-testable\n[split candidate]" {
  style.fill: "#ffe5b4"
  hls: "hls_cache.rs (488)"
  img: "image_cache.rs (487)"
  tp: "trickplay_cache.rs (450)"
  sub: "subtitle_cache.rs (293)"
}

discovery: "Discovery / protocols" {
  ssdp: "ssdp.rs (393)"
  dlna: "dlna.rs (470)"
  ltv: "live_tv.rs (439)"
}

sync_grp: "Sync / group playback\nWS state machine\n[split candidate]" {
  style.fill: "#ffe5b4"
  syncmod: "sync/mod.rs"
  ws: "sync/ws.rs"
  grp: "sync/group.rs"
  reg: "sync/registry.rs"
  msg: "sync/messages.rs"
  clk: "sync/clock.rs"
}

core_glue: "App glue" {
  state: "state.rs (318)"
  cfg: "config.rs (283)"
  cli: "cli.rs (67)"
  obs: "obs.rs (72)"
}

http -> core_glue.state: AppState
auth_sess -> core_glue.state
caches -> core_glue.state
discovery -> core_glue.state
sync_grp -> core_glue.state
http.apij -> auth_sess.sess
http.apij -> caches.hls
http.apij -> sync_grp.ws
```

**Test attribution** — filename prefixes in `crates/pharos-server/tests/` (55 files, 44 inline `#[cfg(test)]` modules in `src/`):

| Bucket | Count | Cluster |
| --- | --- | --- |
| `jellyfin_*.rs` | ~25 | HTTP / api/jellyfin |
| `*_audit.rs` | ~18 | cross-cutting (http + auth_sess) |
| `group_*.rs`, `socket_*.rs` | 4 | sync_grp |
| `dlna.rs` | 1 | discovery |
| boot, route smoke, redaction | ~7 | http glue |

This is the **single largest test bottleneck in the workspace**: every one of the 55 integration files links the full `pharos-server` lib. Splitting any cluster out moves its tests into a smaller compile unit, and `just test-changed` (P52) will skip the rest of the server when only the split crate is touched.

## 4. Split-candidate analysis

Ranked by test-blast-radius reduction × independence-of-blocker.

### 1. `pharos-sync` — extract `sync/`

- **Move:** `sync/{mod,ws,group,registry,messages,clock}.rs` and the 4 tests that target it (`group_conformance.rs`, `group_drift.rs`, `socket_msg_fuzz.rs`, `jellyfin_socket.rs`).
- **Why:** Self-contained WebSocket state machine with its own protocol document (`docs/group-sync-protocol.md`). Sync tests are tokio-heavy and among the slowest in the suite; isolating them shrinks `pharos-server` link + rebuild time per iteration.
- **Reuse:** The state machine is renderer-agnostic — usable by any future frontend that wants group playback, including non-Jellyfin clients.
- **Blocker:** Currently reaches into `state::AppState` for session lookup. Needs a thin `SyncHost` trait (`fn lookup_session`, `fn user_for_token`) on the server side so the new crate doesn't reverse-import. ~1 day of refactor.

### 2. `pharos-cache` — extract the four caches

- **Move:** `hls_cache.rs`, `image_cache.rs`, `trickplay_cache.rs`, `subtitle_cache.rs` (~1,720 LOC combined). Bring inline `#[cfg(test)]` modules with them.
- **Why:** Pure data-plane code. The four cache files are near-identical in shape (LRU + eviction + on-disk staging); a shared `Cache<K, V>` primitive likely falls out for free. Tests are unit-style, no HTTP needed, so they leave `pharos-server` cleanly and run faster as a sibling crate.
- **Reuse:** Any other media binary (CLI repackager, alt frontend) gets caching without pulling actix-web.
- **Blocker:** Caches take `&AppState` for paths + config. Replace with a `CacheCtx` struct (paths + retention + size caps). Low-risk because the dependency surface is small.

### 3. `pharos-jellyfin-api` — extract `api/jellyfin/` (two-phase)

- **Phase A — DTOs first:** Move the serde types + URL/query types into the new crate. ~18 `*_audit.rs` tests today boot the whole server only to assert serde behavior; once DTOs live in their own crate, those audits become unit tests there.
- **Phase B — handlers:** Move the actix-web handlers, leaving `pharos-server` as a thin wiring crate.
- **Reuse:** Pairs with `pharos-jellyfin-test-client` — both ends of the wire share types. Eliminates the current "coupled by convention, not by Cargo" gap visible in the dep graph.
- **Blocker:** Handlers need `AppState`. Two-phase split avoids the trait-extraction work until Phase A has shipped value.

### Honorable mention — discovery (not yet)

`ssdp.rs` + `dlna.rs` + `live_tv.rs` (~1,300 LOC) is also clusterable, but:
- `ssdp` and `dlna` do real network I/O in tests (SSDP multicast, DLNA XML descriptors), so the test acceleration win is smaller than for the caches.
- `live_tv` reaches deep into `state` + store. Trait extraction here is non-trivial.

Tackle after the three above land.

### Anti-recommendation

Do **not** split `pharos-ui` further. It's already a separate crate, already detached from the production cone, and its WASM build cost is independent of the server. Splitting `views/` into sub-crates would add Cargo overhead without test or reuse benefit at this size.

## 5. How splits compose with the existing test loop

- **`just test-changed`** (P52) maps changed files → owning crate → `nextest -E "rdeps(pkg) + ..."`. Every successful split shrinks the rdeps closure for changes inside the new crate.
- **`just test-fast`** (`--lib` only) already skips integration tests; splits don't change that loop directly, but they cut the unit-test compile size of `pharos-server`.
- **`workspace-hack`** (P53) absorbs added crates with no recipe change; just remember `just hakari-regen` after creating the new crate's `Cargo.toml`.
- **`.config/nextest.toml`** has a per-package override only for `pharos-store-sqlx` today; new crates inherit the default profile.

## 6. Rendering the diagrams

```sh
just diagrams                # renders every docs/*.d2 -> docs/*.svg
```

The recipe uses `nix run nixpkgs#d2` so `d2` does not need to live in the devShell. Rendered SVGs are intentionally not committed — the `.d2` source is canonical.
