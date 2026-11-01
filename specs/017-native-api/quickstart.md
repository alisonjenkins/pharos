# Quickstart / validation: 017-native-api

All commands run inside `nix develop`.

## Prerequisites

- `capnp --version` prints 1.5.x (devShell).
- A seeded library: the fixtures `just compat-playwright-full` seeds (a standalone
  seed recipe is added by this feature if the plan's tasks need one).

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
