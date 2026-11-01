# Quickstart / validation: 017-native-api

All commands run inside `nix develop`.

## Prerequisites

- `capnp --version` prints 1.4.0 (devShell).
- A seeded library: the fixtures `just compat-playwright-full` seeds (a standalone
  seed recipe is added by this feature if the plan's tasks need one).

## 0. wasm size baseline (R8)

Measured 2026-11-01 (T001) inside the devShell, replacing the host-cargo
numbers in research.md §R8.

Toolchain: rustc 1.99.0 (b940084d7 2026-09-28), cargo 1.99.0 (5f94df478
2026-08-27), Cap'n Proto compiler `capnp` 1.4.0.

| Measure | Raw bytes | Gzip -9 bytes | Notes |
|---|---|---|---|
| Current pharos-ui-web wasm, workspace release profile | 3,533,954 (3.53 MB) | 725,042 (725 KB) | not stripped, no wasm-opt |
| Same, opt-level=z, LTO, 1 CGU, stripped | 1,861,309 (1.86 MB) | 526,279 (526 KB) | no wasm-opt / wasm-bindgen post-processing |
| capnp + capnp-rpc spike (standalone) | 290,744 (291 KB) | 106,983 (107 KB) | upper bound on increment; `run` polls the R1 flight so the client code is linked |

Commands, run from the repo root (`<target-dir>` is a scratch dir outside the
repo, passed as `--target-dir`; `<spike>` is the scratch spike crate):

```sh
nix develop --command cargo build -p pharos-ui --release --target wasm32-unknown-unknown --bin pharos-ui-web --features web --target-dir <target-dir>
gzip -9 -c <target-dir>/wasm32-unknown-unknown/release/pharos-ui-web.wasm | wc -c

nix develop --command env CARGO_PROFILE_RELEASE_OPT_LEVEL=z CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=true cargo build -p pharos-ui --release --target wasm32-unknown-unknown --bin pharos-ui-web --features web --target-dir <target-dir>
gzip -9 -c <target-dir>/wasm32-unknown-unknown/release/pharos-ui-web.wasm | wc -c

# spike: standalone crate outside the repo (schema copied from contracts/pharos.capnp)
nix develop --command cargo build --manifest-path <spike>/Cargo.toml --release --target wasm32-unknown-unknown --target-dir <spike-target>
gzip -9 -c <spike-target>/wasm32-unknown-unknown/release/r8_spike.wasm | wc -c
```

The spike's `[profile.release]` sets opt-level="z", lto=true, codegen-units=1,
strip=true, panic="abort". Its `run` polls the flight once. With the original
schema capnpc 0.27 could not pipeline through the `SessionResult` union, so
`resume` was awaited; `ResumeResult` (research R1) has since removed that hop.

## 1. Instrumentation cannot be bypassed (V173)

```sh
cargo nextest run -p pharos-api instrument
```

Expect a failing-handler test asserting the `rpc.library.latest` span, the
`pharos_rpc_calls_total{interface="library",method="latest",outcome="not_found"}`
increment and a WARN line carrying the missing id. Comment out the wrapper call
in `instrument.rs` and the same test must fail (ODD step 3).

## 2. Jellyfin compat unchanged (SC-003)

```sh
just test && just compat-playwright-full
```

Expect it green, and identical to `main` for every Jellyfin test.

## 3. Native round trips (SC-001)

```sh
cargo nextest run -p pharos-server --test it_rpc home_in_one_batch
```

The test pipelines `resume → session.library → library.home` over TCP and
asserts one client→server flight after the bootstrap, with every rail populated.

## 4. Browser path

```sh
just compat-dioxus   # Playwright; includes the /pharos/rpc login + resume spec
```

Expect: login sets `pharos_session` (`HttpOnly`); reload resumes without
re-login; `localStorage` holds no token.

## 5. Efficiency gate (SC-002)

```sh
just bench-native-vs-json   # new recipe added by T040
```

Prints bytes and decode time for a 500-item page on both surfaces. The first
run sets the gate in this file; later runs fail on a regression > 10 %.

| measure | Jellyfin JSON | native | gate |
|---|---|---|---|
| bytes (500 items) | _T040_ | _T040_ | _T040_ |
| wasm decode ms | _T040_ | _T040_ | _T040_ |

## 6. By query after deploy (SC-004)

```promql
sum by (interface,method,outcome) (rate(pharos_rpc_calls_total[5m]))
histogram_quantile(0.95, sum by (le,interface,method) (rate(pharos_rpc_duration_seconds_bucket[5m])))
max by (topic) (pharos_rpc_event_lag_seconds)
```

Record the real output here.
