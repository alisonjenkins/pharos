# ADR-0078: The workspace moves to Dioxus 0.8 (0.8.0-alpha.3)

- **Status:** Accepted
- **Date:** 2026-11-08T22:10:00Z
- **Deciders:** Alison

## Context

ADR-0077 kept the workspace on Dioxus 0.7 and left a move to a later ADR.
Spec 032's spike (the Android TV app on Dioxus Native) ran on 0.7 first and
then on 0.8.0-alpha.3 (`specs/032-blitz-tv/spike-0.md`):

- Dioxus Native 0.7.10 hard-wires GPU Vello, which drew a frame in about
  373 ms on the G08's Mali-G52. 0.8 makes the renderer a feature, and its
  default, vello-hybrid, draws one in about 84 ms (57 ms with CPU and GPU
  overlap).
- Blitz 0.3, which 0.8 uses, carries upstream the three fixes the spike had
  to patch into Blitz 0.2: taffy's aspect-ratio transfer, remote keys with no
  physical code, and a redraw on every key event.
- dx 0.8 bundles Android Gradle modules declared with `#[manganis::ffi]`.

One `pharos-ui` serves the web bundle, the WebView TV app (spec 025) and the
Blitz TV app, so the native app cannot move alone. 0.8 is still an alpha.
On 2026-11-08, Alison approved the bump, after she had chosen to follow the
latest Dioxus for the TV work.

## Decision

We will move the workspace's `dioxus`, `dioxus-web` and `dioxus-ssr` to
`=0.8.0-alpha.3`. They are pinned exactly, because an alpha can break
between pre-releases.

- `dx` comes from crates.io at the same version and is built in the flake
  with the pinned toolchain, because nixpkgs ships 0.7. The CLI and the
  crates move together. dx 0.8 minifies JS with esbuild and the flake
  builds with downloads off, so nixpkgs' esbuild goes on the PATH beside it.
- keyboard-types 0.8 moves named keys under `Key::Named(NamedKey::..)`.
  That is the only source change `pharos-ui` needs.
- The next pre-release or 0.8.0 is a version bump in the same three places
  plus the flake's `dioxusCli` hashes.

## Consequences

- The Blitz TV app can build from the workspace's own `pharos-ui`, with no
  second Dioxus version in the tree.
- The web UI and the WebView TV app run on an alpha. Each pre-release bump
  needs the web bundle, the WebView APK and the Blitz APK checked on their
  targets, not only `cargo check`.
- Open branches written against 0.7 that match on named keys need the same
  `Key::Named` rewrite when they rebase.
- `dx` is no longer a nixpkgs binary. The first build of the dev shell
  compiles it, so the cache matters.

## Alternatives considered

- **Stay on 0.7 and backport Blitz 0.3's fixes.** The renderer choice and
  the hybrid renderer are 0.8 features, and backporting them is a fork of
  dioxus-native. Rejected.
- **Put only the Blitz TV app on 0.8.** `pharos-ui` would then compile
  against two Dioxus versions, which would mean two copies of every
  component's hooks and contexts. Rejected.

## References

- ADR-0077 (the TV app on Dioxus Native), spec 032, `specs/032-blitz-tv/spike-0.md`.
