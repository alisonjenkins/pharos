# CLAUDE.md — pharos

## Dev environment

**Always work inside the Nix devShell.** It pins the rust toolchain, clippy, rustfmt, ffmpeg, and cargo tooling so behavior matches CI.

- One-shot command: prefix with `nix develop --command <cmd>`, e.g. `nix develop --command cargo test --workspace`.
- Interactive: `nix develop` (or `direnv allow` if `.envrc` is set up).
- Do not invoke `cargo`, `rustc`, `clippy`, `ffmpeg`, etc. from the host shell — versions may drift from the flake.
- Tests run via **`cargo nextest run --workspace`** (config in `.config/nextest.toml`). Faster + better output than the built-in runner. Use `cargo test --doc --workspace` separately for doctests.

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

## Stack

actix-web · clap derive · tokio · sqlx · Dioxus + dx (WASM) · tracing + metrics + Prometheus.
