# CLAUDE.md — pharos

## Dev environment

**Always work inside the Nix devShell.** It pins the rust toolchain, clippy, rustfmt, ffmpeg, and cargo tooling so behavior matches CI.

- One-shot command: prefix with `nix develop --command <cmd>`, e.g. `nix develop --command cargo test --workspace`.
- Interactive: `nix develop` (or `direnv allow` if `.envrc` is set up).
- Do not invoke `cargo`, `rustc`, `clippy`, `ffmpeg`, etc. from the host shell — versions may drift from the flake.
- Tests run via **`cargo nextest run --workspace`** (config in `.config/nextest.toml`). Faster + better output than the built-in runner. Use `cargo test --doc --workspace` separately for doctests.
- Fast-feedback recipes via `just`:
  - `just test-fast` — workspace `--lib` only, skips heavy `tests/*.rs` binaries.
  - `just test-changed [from=main]` — `cargo-guppy` enumerates packages touched vs `from`, then `nextest -E 'rdeps(pkg1) + rdeps(pkg2)'` runs only the transitively-affected tests.
  - `just test` — full workspace (strips macOS Gatekeeper quarantine attr first).
  - `just test-thorough` — full workspace with `PROPTEST_CASES=512` for nightly / pre-release.
  - **Workflow**: iterate with `test-fast` / `test-changed` (blast-radius only) for tight loops; always run the full `just test` before a commit.
- After a dep change in any crate's `Cargo.toml`, run `just hakari-regen` to
  refresh `workspace-hack` (CI's `just hakari-check` fails on a stale hack
  crate). The `nix build .#pharos` / `.#oci` jobs build via `buildRustPackage`
  straight from `Cargo.lock` (see §Transcode), so no separate Nix regen step —
  a bumped `Cargo.lock` is all Nix needs.

Rationale: reproducibility + V17 (`clippy::unwrap_used` / `expect_used` deny) requires clippy from the pinned toolchain. Host system may not have it.

## Workflow

- Spec lives in `SPEC.md`. Mutate only via `/ck:spec` (or `/ck:build` for §T status flips).
- Tasks numbered T1…T27 in §T. Pick next via `/ck:build --next` or `/ck:build T<n>`.
- Bugs append to §B with cause + invariant link (`/ck:spec bug: …`).

## Observability-driven development (ODD)

Runs **alongside TDD, not instead of it**. A change is not designed until you
can name the observable behaviour that proves it — in production, by query.

The bugs that have cost this project the most were not hard, they were
*invisible*. The browser-playback outage (2026-07-25) was diagnosed from a
reverse proxy's 499s because the segment failure path logged nothing, while the
success path beside it recorded twelve fields. Diagnosis time went entirely on
reconstructing state the server could have stated.

On every change, in this order:

1. **Name the signal first.** Before writing a fix, state the exact LogQL /
   PromQL query that shows the bug happening now. If no such query exists, the
   FIRST commit adds the instrumentation — separately, so it can ship and be
   read before the fix lands.
2. **Instrument decisions, not just errors.** Any branch choosing between
   behaviours (codec, device, delivery method, burn/no-burn, cache hit/miss)
   records its inputs, its verdict, AND the reason — carrying the offending
   value, never a bare class (see §"Expose the cause" discipline in the error
   types). A reason that doesn't name the value is another round of guessing.
3. **Test the signal.** The metric/log is part of the contract: assert it in a
   unit test, and confirm the test FAILS without the instrumentation (disarm
   it once and watch it go red — `red_metrics.rs`'s abort counter was verified
   this way).
4. **Symmetry.** Whatever the success path records, the failure path records
   too. Rich-on-success/silent-on-failure is the shape that hides outages.
5. **Verify by query, not by assertion.** After deploying, run the named query
   and report its actual output. "Should be fixed" is not a result.

Metric labels are a dashboard contract: bounded cardinality, stable strings
from a `label()` method, asserted distinct in a test. A renamed label breaks
alerts silently.

Signals already available (use them before adding more):
`pharos_playback_decision_total{decision,direct_play_block,downgrade}`,
`pharos_segment_produced_total{outcome,reason}`, `pharos_segment_cache_total`,
`pharos_segment_transcode_seconds`, `http_client_aborted_total{method,path}`,
`pharos_source_unreadable_total{reason}`, `pharos_transcode_device_{capacity,in_use,cooldown}`,
`pharos_transcode_pending_jobs`. Loki: `{namespace="pharos"}`, LB at
`192.168.1.244:3100`. Traces export to Tempo via OTLP when configured.

## Subagent worktree isolation

`.claude/settings.json` configures `WorktreeCreate` / `WorktreeRemove` hooks so the `Agent` tool with `isolation: "worktree"` works. Each isolated agent gets its own `agent/<basename>` ephemeral branch off `main`; the hook cleans up the branch on remove.

If `Agent isolation: "worktree"` errors with "not in a git repository", restart Claude Code once — settings hot-reload is best-effort and the harness's git-repo check is cached at session start. Worktrees should work in subsequent sessions.

## Web UI build

Dioxus UI lives in `crates/pharos-ui` and compiles to WASM via the
`dx` CLI shipped in the devShell.

- Dev loop: `nix develop --command dx serve --package pharos-ui` (hot reload).
- Release bundle: `nix develop --command dx build --package pharos-ui --release`.
- Output lands under `target/dx/pharos-ui/release/web/public/`.
- Point the server at it via `[server].ui_dir` in `config.toml`; pharos serves the bundle at `/ui/*`.
- WASM target is pinned in `rust-toolchain.toml`; `cargo build --target wasm32-unknown-unknown` works without extra setup inside the devShell.

## Transcode / ffmpeg backends (P48)

Two interchangeable ffmpeg backends, selected by Cargo feature:
- **`ffmpeg-lib`** (default) — runs the high-frequency "tiny ops" (probe,
  image extract, trickplay tiles, srt→webvtt, waveform) **in-process** via
  `ffmpeg-the-third` (libav), serviced by a persistent, crash-isolated
  `transcode-worker` pool (`pharos-transcode::worker::LibavWorkerPool`).
  Video-segment / live transcode **always** stays on the spawn worker
  (encode time dwarfs fork/exec; the scheduler already load-balances every
  GPU + CPU). A libav fault kills only a worker, never the server (V6). This
  is the hybrid the deployment runs.
- **`ffmpeg-spawn`** — forks the `ffmpeg`/`ffprobe` binaries. Build it with
  `--no-default-features --features backend-spawn`/`ffmpeg-spawn`.

Because libav is the default, the FFI crate (`ffmpeg-the-third`) builds by
default and needs the libav headers + bindgen — the devShell exports
`LIBCLANG_PATH` + ffmpeg dev libs, so plain `cargo`/`nextest` work. The
`.#oci` image builds the server + worker via **`buildRustPackage`** (cargo),
NOT crate2nix: the pinned nixpkgs `buildRustCrate` mishandles the crate's
modern `cargo::`-syntax version cfgs and compiles the wrong libav API.

**Pixel formats are encoder-specific — always set them explicitly:**
- mjpeg (posters / thumbs / trickplay) needs full-range `yuvj420p`; the
  scale/tile filters emit limited-range `yuv420p` which ffmpeg 8.1's mjpeg
  encoder rejects ("Non full-range YUV is non-standard").
- Software / NVENC / QSV / VideoToolbox H.264/HEVC force `-pix_fmt
  yuv420p` for broad 8-bit 4:2:0 client compat (a 10-bit/4:4:4 source
  would otherwise carry through and fail many decoders).
- VAAPI uploads via `format=nv12,hwupload` instead of a software
  `-pix_fmt` (frames live in GPU memory).

`[server].image_seek_seconds` (default 30) is the poster/thumb seek
timestamp; lower it for short test clips so the seek lands inside the file
(a seek past EOF yields no frame → 404).

## Client-compat validation (T29)

Two layers:
- Layer B (in-tree, runs in `cargo nextest`): `tests/client_compat.rs`
  spins pharos on an ephemeral port and drives `pharos-jellyfin-test-client`
  through a real-device-shape flow (Emby-Authorization header, strict
  serde DTOs). Every PR runs this via `just test`.
- Layer A (manual / nightly): `just compat-openapi` fetches the Jellyfin
  OpenAPI spec and prints the `schemathesis run` invocation. Schemathesis
  ships in the devShell.

### Playwright (jellyfin-web E2E)

`just compat-playwright-full` seeds a user + real media, starts pharos,
and drives unmodified jellyfin-web headless. Notes:
- **Browsers come from nix** (`PLAYWRIGHT_BROWSERS_PATH`, exported by the
  devShell from `pkgs.playwright-driver.browsers`) — no `npx playwright
  install`, works offline + identically everywhere. The npm
  `@playwright/test` version (`compat-playwright/package.json`) **must
  match** `playwright-driver.version`; bump both together (check via
  `nix eval --raw nixpkgs#playwright-driver.version`).
- The static jellyfin-web bundle is served with `http-server --proxy`
  forwarding all REST paths to pharos, so the browser sees one same-origin
  server (real-Jellyfin-shape; the boot `/System/Info/Public` probe
  resolves instead of 404ing).

## Stack

actix-web · clap derive · tokio · sqlx · Dioxus + dx (WASM) · tracing + metrics + Prometheus · reqwest (compat tests only).
