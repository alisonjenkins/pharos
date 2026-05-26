# CLAUDE.md — pharos

## Dev environment

**Always work inside the Nix devShell.** It pins the rust toolchain, clippy, rustfmt, ffmpeg, and cargo tooling so behavior matches CI.

- One-shot command: prefix with `nix develop --command <cmd>`, e.g. `nix develop --command cargo test --workspace`.
- Interactive: `nix develop` (or `direnv allow` if `.envrc` is set up).
- Do not invoke `cargo`, `rustc`, `clippy`, `ffmpeg`, etc. from the host shell — versions may drift from the flake.

Rationale: reproducibility + V17 (`clippy::unwrap_used` / `expect_used` deny) requires clippy from the pinned toolchain. Host system may not have it.

## Workflow

- Spec lives in `SPEC.md`. Mutate only via `/ck:spec` (or `/ck:build` for §T status flips).
- Tasks numbered T1…T27 in §T. Pick next via `/ck:build --next` or `/ck:build T<n>`.
- Bugs append to §B with cause + invariant link (`/ck:spec bug: …`).

## Stack

actix-web · clap derive · tokio · sqlx (planned T2) · Dioxus (planned T24) · tracing + metrics + Prometheus.
