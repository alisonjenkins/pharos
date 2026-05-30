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
- After a dep change in any crate's `Cargo.toml`, run **two** regens or CI breaks:
  - `just hakari-regen` — refresh `workspace-hack` (CI's `just hakari-check` fails on a stale hack crate).
  - `nix develop --command crate2nix generate` — regenerate `Cargo.nix` from `Cargo.lock`. The `nix build .#pharos` / `.#oci` jobs build each crate as its own derivation with explicit `--extern`s read from `Cargo.nix`; a stale `Cargo.nix` fails with `unresolved import <newdep>` even though `cargo build` in the devShell passes. Commit the regenerated `Cargo.nix`.

Rationale: reproducibility + V17 (`clippy::unwrap_used` / `expect_used` deny) requires clippy from the pinned toolchain. Host system may not have it.

## Workflow

- Spec lives in `SPEC.md`. Mutate only via `/ck:spec` (or `/ck:build` for §T status flips).
- Tasks numbered T1…T27 in §T. Pick next via `/ck:build --next` or `/ck:build T<n>`.
- Bugs append to §B with cause + invariant link (`/ck:spec bug: …`).

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
- **`ffmpeg-spawn`** (default) — forks the `ffmpeg`/`ffprobe` binaries.
- **`ffmpeg-lib`** — runs the high-frequency "tiny ops" (probe, image
  extract, trickplay tiles, srt→webvtt, waveform) **in-process** via
  `ffmpeg-the-third` (libav), serviced by a persistent, crash-isolated
  `transcode-worker` pool (`pharos-transcode::worker::LibavWorkerPool`).
  Video-segment / live transcode **always** stays on the spawn worker
  (encode time dwarfs fork/exec; the scheduler already load-balances every
  GPU + CPU). A libav fault kills only a worker, never the server (V6).
  Build/test it with `--no-default-features --features
  backend-lib`/`ffmpeg-lib`; the server feature is `ffmpeg-lib`.

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
