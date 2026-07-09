# ADR-0009: Nix flake + devShell; buildRustPackage for the OCI image

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

pharos's build is not self-contained Rust: the default backend (ADR-0004) links
libav via `ffmpeg-the-third`, needing ffmpeg dev libs + bindgen + `LIBCLANG_PATH`;
the `unwrap`/`expect` deny lint (V17) needs a specific clippy; the WASM UI
(ADR-0002 / `pharos-ui`) needs `dx`, `wasm-bindgen-cli` at a pinned version, and
binaryen; the compat suite needs Playwright browsers. "Works on my machine" and
"matches CI" must be the same thing, offline and reproducibly.

## Decision

A **Nix flake** pins the entire toolchain; all development and CI commands run
inside the **devShell** (`nix develop --command …`). The flake exports the rust
toolchain, clippy, rustfmt, ffmpeg + dev libs, `LIBCLANG_PATH`, `dx` +
wasm-bindgen-cli + binaryen, skopeo, schemathesis, and the nix-pinned Playwright
browsers (`PLAYWRIGHT_BROWSERS_PATH`).

The container image is built by **`buildRustPackage`** (cargo, straight from
`Cargo.lock`) via `nix build .#oci` — **not** crate2nix / `buildRustCrate`. The
pinned nixpkgs `buildRustCrate` mishandles `ffmpeg-the-third`'s modern `cargo::`
version-cfg syntax and compiles the wrong libav API. Because the image builds
from `Cargo.lock`, a bumped lockfile is all Nix needs — no separate Nix codegen
step.

## Consequences

- Contributor and CI environments are byte-identical and offline-capable;
  clippy/rustfmt/ffmpeg versions cannot drift.
- Host `cargo`/`rustc`/`clippy` must **not** be used — they may lack libclang or
  differ from the pin (`CLAUDE.md` makes this a hard rule).
- Dependency changes require `just hakari-regen` to refresh `workspace-hack`
  (CI's `hakari-check` fails on a stale hack crate), but no separate Nix regen —
  the `Cargo.lock` bump suffices.
- The WASM `pharos-ui` deliberately has no `workspace-hack` (wasm target).
- Choosing `buildRustPackage` over crate2nix trades finer-grained Nix caching for
  correctness of the libav FFI build.

## References

- `flake.nix`, `CLAUDE.md` (§Dev environment, §Transcode, §Web UI build)
- memory `project_ffmpeg_lib_default`, `project_dioxus_ui_build_broken`
