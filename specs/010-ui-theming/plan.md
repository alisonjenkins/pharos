# Implementation Plan: user-themeable pharos-ui

**Branch**: `feat/ui-theming-plan` | **Date**: 2026-11-02 | **Spec**: [spec.md](./spec.md)
**Input**: `specs/010-ui-theming/spec.md` (clarified 2026-11-02, 3 answers)
**Depends on**:
- 009 FR-007 (tokens). 010 owns their shape.
- 009 FR-005 (GPU layer), phase 4 only.
- 017 `Prefs` (T068), phase 3 only.

## Summary

Every colour, font, shape and motion value in pharos-ui comes from a resolved
theme:
- a TOML document (R1)
- inheritance through `extends`
- validated into a typed token set
- emitted as `--px-*` custom properties on `<html>`

Seven themes ship as data. `pharos.css` loses its literal colours and reads
only tokens. Phase 1 is fully host-testable and blocked on nothing. The
server-side pieces (themes that follow the user, admin defaults) wait for
017's `Prefs`. The GPU effects wait for 009's renderer. Until then, effects
are validated and drawn only as their CSS fallbacks.

## Technical Context

**Language/Version**: Rust stable → `wasm32-unknown-unknown` via `dx` (ADR-0012)
**Primary Dependencies**:
- existing: `dioxus` 0.7, `web-sys`, `serde`, `serde_json`
- new to `pharos-ui`: `toml` 1 (workspace dependency; parse + display), costing ≤140 KB optimised wasm by the R1 probe and re-measured by phase 1's first task
**Storage**: `localStorage` per device (R5). Server `Prefs` from phase 3.
**Testing**:
- host unit tests for model, resolve, contrast and emit
- a stylesheet contract test (contracts/css-tokens.md)
- `dioxus-ssr` tests for the Appearance page
- Playwright (`compat-playwright/tests/dioxus/`) for apply, persist, fallback and import
**Target Platform**: as 009 (desktop browsers, Meta Quest Browser, Steam Frame browser)
**Project Type**: web front-end (wasm). Server work only in phase 3.
**Performance Goals**: SC-002, one style write per theme switch. No first-load regression beyond the measured `toml` increment.
**Constraints**:
- themes are data only (spec §Theme model)
- no literal colours outside tokens (009 US5)
- the `theme` module has no `web_sys` (V12)
**Scale/Scope**: 7 shipped themes, about 20 palette roles, one settings page.

## Constitution Check

| Gate | Status |
|---|---|
| I. Wire compatibility | PASS. UI-only. jellyfin-web and every API route are untouched. The new `/settings` route joins `route::FIRST_SEGMENTS` and the collision test |
| II. Group sync | N/A |
| III. Test-first + ODD | PASS. Each phase starts with failing tests. Signals are defined before code (data-model § Signals): `applied`, `warning`, `fallback` and `rejected` lines carry the same fields, reasons come from `label()`, and tests assert the lines and go red without them |
| IV. Never panics, never lies | PASS. Parsing returns `ThemeError`, which names the path and value. Unreadable storage falls back to the default and logs it. No `unwrap`/`expect` |
| V. Types over conventions | PASS. A resolved `Theme` holds bounded newtypes and closed enums, so an out-of-range value cannot be represented |
| Tech constraints | PASS. Rust/Dioxus, devShell. `toml` is already a workspace dependency, so run `just hakari-regen` when it is added to `pharos-ui` |
| V16 / V172 (no backdoor endpoints) | PASS. Phase 3 uses `Prefs` on the native API; it adds no REST route |

No violations. Re-checked after design: unchanged.

**ADR to write when phase 1 lands**: themes are TOML documents over a
platform-neutral token model, emitted as `--px-*` properties (R1, R3). Take
the number from main at rebase time.

## Project Structure

```text
specs/010-ui-theming/
├── spec.md plan.md research.md data-model.md quickstart.md
└── contracts/{theme-file.md, css-tokens.md}

crates/pharos-ui/
├── themes/*.pharos-theme.toml       # the seven shipped themes (include_str!)
├── assets/pharos.css                # :root = emit(Pharos Night); rules use --px-* only
└── src/
    ├── theme/
    │   ├── mod.rs                   # Catalogue: shipped + custom, by name
    │   ├── doc.rs                   # ThemeDoc (serde), parse, unknown-key walk
    │   ├── resolve.rs               # extends chain → Theme, ThemeError/ThemeWarning
    │   ├── value.rs                 # Rgba, FontStack, bounded newtypes
    │   ├── contrast.rs              # WCAG ratio, ContrastPair table, check()
    │   ├── css.rs                   # emit()
    │   └── signal.rs                # log-line formatting, reason labels
    └── native/
        ├── web.rs   host.rs         # apply_tokens(), load/save DeviceThemeState, media-query listeners
        ├── app.rs                   # theme provider at Root; applies before first native paint
        └── views.rs                 # Appearance page (phase 2)
crates/pharos-ui/tests/ssr_render.rs # Appearance page renders
compat-playwright/tests/dioxus/theming.spec.ts
```

## Phases (outline for `/speckit-tasks`)

1. **Engine and the shipped themes** (unblocked):
   - Measure the `toml` increment in `pharos-ui` and record it in R1.
   - Value types and parse, with errors that name the path.
   - `extends` resolution with cycle detection, and unknown-key and
     newer-schema warnings.
   - The contrast checker and its pair table.
   - `css::emit`.
   - The seven themes as TOML, each passing contrast in a test.
   - Rewrite `pharos.css` onto `--px-*`, plus the stylesheet contract test.
   - Apply the device theme at `Root`, with `localStorage` read and write and
     the fallback path.
   - Signals and their tests.
   - The ADR.
2. **Appearance page**, with a gallery and live preview:
   - FR-003: the editor, with a contrast readout and the US6 save gate.
   - US7: `auto`.
   - FR-005: import and export, both from a file and fetched by the browser
     from a URL.
   - The Playwright spec.
3. **Themes follow the user**:
   - First 017 T068: the `Prefs` server (`get`/`set` per user, on sqlite
     and postgres, and `just test-postgres`).
   - Then US3, with the server copy and a device override.
   - Then FR-008: an admin default, a login theme and message, and an
     allow-custom flag.
4. **GPU effects**, once 009 FR-005 exists:
   - the uniform block from the same `Theme`
   - shader allow-list parameters
   - the FR-007 effects budget, with an `effects_reduced` signal

Each phase is its own PR or PRs, and each commit is green on its own.
