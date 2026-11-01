# Contract: ports (009 §Boundaries)

Each port is a trait in `crates/pharos-ui/src/ports/` (or in `pharos-client`
where platform-neutral). The adapter aliases live in `adapters/mod.rs` (research R6).
Signatures are indicative; `/speckit-tasks` fixes them test-first.

| Port | Crate | Core methods | v1 adapter | Fake |
|---|---|---|---|---|
| `PharosApi` | pharos-client | domain calls per 017 interfaces | `NativeApi` (capnp-rpc) ; `JellyfinRest` (transitional, existing panes only) | `FakeApi` (scripted responses) |
| `LiveChannel` | pharos-client | `subscribe(topics) -> Stream<Event>` | `NativeEvents` | `FakeEvents` |
| `Transport` | pharos-client | `connect(url) -> (Reader, Writer)` | `WsWeb` / `TcpNative` | in-memory duplex |
| `Renderer` | pharos-ui | `begin_frame`, `sprite`, `effect`, `end_frame`, `mode()` | `GlowRenderer` | `NullRenderer` (records calls) |
| `ImageSource` | pharos-ui | `url(item, kind, px) -> Url`, `placeholder(item)` | `ServerImages` | fixed URLs |
| `Storage` | pharos-ui | `get/set/remove(key)` for device-local, non-secret data | `LocalStorage` | `MemStorage` |
| `History` | pharos-ui | `push/replace/back`, `state(key)` | `BrowserHistory` | `MemHistory` |
| `Messages` | pharos-ui | `t(id, args)`, `locale()` | `Fluent` | `EchoMessages` (returns ids) |
| `Clock` | pharos-client | `wall_ms()`, `mono_ms()` | `PerformanceClock` / `StdClock` | `ManualClock` |

Contract tests: `tests/ports/<port>.rs` define one suite per port and run it
against every adapter that can run on the host, plus wasm adapters under
Playwright. A lint test fails if a module under `src/views/` imports `adapters`,
`web_sys`, `js_sys` or `gloo_*` (FR-014).
