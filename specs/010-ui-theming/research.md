# Research: 010-ui-theming

**Date**: 2026-11-02T10:50:00Z. Measured in the Nix devShell (rustc 1.99.0)
on ali-desktop.

## R1 — theme file format: TOML (Clarifications, session 2026-11-02)

- **Decision**: TOML for theme files, parsed with the workspace's `toml` 1.x
  (`default-features = false`, features `parse`, `display`, `serde`, `std`).
  It matches `config.toml`, takes comments, and is easy to hand-edit, which is
  what makes a shared theme readable before it is imported.
- **Cost, measured**: two standalone `cdylib` probes, each with the same
  serde-derived theme struct, built for `wasm32-unknown-unknown` with
  `opt-level = "z"`, LTO, 1 CGU, strip and `panic = "abort"`, then
  `wasm-opt -Oz`:

  | Probe | raw | after wasm-opt | gzip -9 |
  |---|---|---|---|
  | `serde_json` read + write | 85 560 B | 75 416 B | 33 762 B |
  | the same plus `toml` read + write | 243 121 B | 215 007 B | 98 034 B |

  So `toml` adds about **140 KB optimised, 64 KB gzipped**: ~8 % on top of
  today's 1.66 MB UI wasm (B269/V206). The probe has nothing else in it, so
  the figure includes code (formatting, `std` pieces) the UI may already
  link. It is an upper bound. The first phase-1 task measures the real increment inside
  `pharos-ui` and records it here.
- **In pharos-ui, measured 2026-11-02T12:10:00Z**: the `nix build .#pharosUiBundle`
  wasm went from 1 684 972 B to 1 954 794 B on main bdf4b726. That is
  +269 822 B (+16 %) raw, and gzip -9 went from 611 291 B to 720 797 B
  (+109 506 B). The figure covers the whole phase-1 engine: `toml`, the
  serde derives, error formatting, the 7 embedded themes (~8 KB of text) and
  CSS emission. The probe above puts about 140 KB of it on `toml`. Whether
  that is acceptable is the owner's call. If it is not, the first cut is to
  parse JSON instead (no new dependency) and to merge `extends` on the typed
  docs rather than through `toml::Table`.
- **Alternatives**:
  - **JSON**: costs nothing extra, because `serde_json` is already a
    `pharos-ui` dependency. But it allows no comments, it is noisier to
    hand-edit, and it doesn't match `config.toml`. It's the fallback if that task's
    real increment is unacceptable. Changing it then means switching one
    parser and one ADR, not a redesign, because the model is
    format-independent (data-model.md).
  - **Parse-only `toml`, writing TOML by hand**: export becomes a small
    hand-written writer over a fixed schema. Revisit only if `display` is most
    of the cost.
- **ADR**: "Themes are TOML documents over a platform-neutral token model",
  written in phase 1's PR. Take the next ADR number from main at rebase time.

## R2 — where the theme engine lives

- **Decision**: a `theme` module in `pharos-ui` with no `web_sys` in it:
  model, parse, `extends` resolution, validation, contrast and CSS
  emission. The browser half (applying properties, reading and writing
  `localStorage`) goes in `native/web.rs` with a stand-in in `native/host.rs`,
  the same split the playback engine uses.
- **Rationale**: everything that decides something is host-testable (V12). The
  stored form is platform-neutral TOML (FR-004), so a future phone or TV
  client needs only the model, not the module.
- **Alternative**: a separate `pharos-theme` crate. Rejected for now: there
  is one consumer, and extracting it is cheap when there is a second.

## R3 — token naming: `--px-<group>-<name>`, no aliases

- **Decision**: emit 009's naming (`--px-color-bg`, `--px-color-accent`,
  `--px-radius-card`, …) and rewrite `pharos.css` to use it in the same
  change. Today the sheet has six variables (`--bg`, `--fg`, `--muted`,
  `--accent`, `--tile-bg`, `--error`) and six literal colours outside `:root`
  (`#1f2530`, `#2a3142`, `#11151c` and `#001e2e` three times). Each literal
  becomes a role: `border`, `surface`, `accent-fg`.
- **Rationale**: the sheet is 372 lines with one consumer, so a rename costs
  less than carrying two names indefinitely.
- **Fallback before the wasm runs**: `pharos.css`'s `:root` keeps Pharos
  Night's values, so the first paint is themed before any script runs. A test
  asserts that block equals `css::emit(pharos_night)` exactly, so the two
  cannot drift.

## R4 — contrast rule (US6, FR-006)

- **Decision**: WCAG 2.2 relative luminance and contrast ratio, in `f64`, on
  sRGB values. Every declared pair has a minimum:
  - text pairs: 4.5:1 (AA) for body text, 3:1 for large display text
  - focus ring against `bg` and `surface`: 3:1 (non-text, SC 1.4.11)
  - High Contrast: 7:1 for text (AAA)
- Alpha is composited over the pair's background before measuring. A pair the
  user marks as decorative is reported but not enforced, and only on non-text
  roles.

## R5 — device-local storage until `Prefs`

- **Decision**: `localStorage` holds two keys:
  - `pharos.theme.active`: the theme name, or `auto`
  - `pharos.theme.custom`: a JSON array of TOML theme documents
- **Rationale**: 009 already uses `localStorage` for non-secret device prefs,
  and `server_picker` reads and writes it the same way. A theme holds no
  secrets.
- When `Prefs` lands (017 T068), the server copy wins, and the device keeps
  only its override (US3).
- **Reads fail soft**: unreadable storage or an invalid stored theme falls
  back to the default theme and logs a `theme fallback` line (data-model §
  Signals). The page still loads.

## R6 — what waits, and on what

| Piece | Blocked on | Until then |
|---|---|---|
| US3 / FR-004 themes follow the user | `Prefs.get` / `Prefs.set` on the server (017 T068) | per-device `localStorage` |
| FR-008 admin default, login theme and message | the same, plus a server-wide prefs scope | the default theme is the shipped default |
| FR-002 GPU uniforms, FR-007 effects budget | 009 FR-005 renderer (`Renderer` port, `glow`) | effects are parsed and validated, and rendered only as their CSS fallback |
| Telemetry `themeFallback` events | `Telemetry.submit` on the server (017 T069) | browser console lines in the same key=value shape |

## R7 — fonts

- **Decision**: phase 1 accepts only named system stacks: `system-ui`,
  `sans`, `serif` and `mono`. The bundled, licence-clear set (spec §Theme
  model 3) is its own task, with a size measurement, because every font
  file adds to first load. A theme that names an unknown font falls back to
  the system stack and logs a `font_missing` warning (spec §Edge cases).
