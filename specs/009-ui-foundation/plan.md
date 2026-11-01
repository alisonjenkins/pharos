# Implementation Plan: pharos-ui foundation

**Branch**: `009-ui-foundation` | **Date**: 2026-11-01 | **Spec**: [spec.md](./spec.md)
**Input**: `specs/009-ui-foundation/spec.md` (clarified 2026-11-01, 5 answers)
**Depends on**: 017 phases 1–3 (schema, transport, bootstrap) for the native `PharosApi`

## Summary

Rebuild pharos-ui's skeleton so that every later spec has somewhere to stand:
- a typed history-mode router at `/` that cannot shadow API paths (R3, R4);
- a query cache and live updates from the shared `pharos-client` core (R5);
- a hybrid renderer, with a `glow` WebGL2 layer behind the DOM and a complete
  CSS fallback (R2);
- design tokens emitted to CSS and to GPU uniforms;
- ports and adapters with fakes and contract tests (R6);
- Fluent i18n with a pseudo-locale gate (R7);
- an axe-checked accessibility baseline (R8).

The proof screen is one poster rail that uses every layer. The current 2.3k-line
`app_state.rs` is split along those seams, and its existing panes keep working
on the transitional REST adapter until they migrate.

## Technical Context

**Language/Version**: Rust stable → `wasm32-unknown-unknown` via `dx` (ADR-0012)
**Primary Dependencies**: existing: `dioxus` 0.7, `dioxus-web`, `web-sys`, `wasm-bindgen(-futures)`. New: `dioxus-router` 0.7, `glow`, `fluent-bundle`, `pharos-client` (017). Test-only: `@axe-core/playwright` (npm, compat-playwright).
**Storage**: browser session storage (history state), `localStorage` for non-secret device prefs. No tokens (017 Q3).
**Testing**: `dioxus-ssr` view tests (existing 88 keep passing); port contract suites; Playwright for routing, renderer modes, axe, pseudo-locale and the route-collision test; server-side actix tests for serving.
**Target Platform**: desktop Chrome/Firefox/Safari/Edge, Meta Quest Browser, Steam Frame browser (R9)
**Project Type**: web front-end (wasm) + small server change
**Performance Goals**: NFR-002 ≤100 ms route paint from cache; NFR-003 60 fps with 500 virtualised posters; first-load gate from T001 (R1)
**Constraints**: no view imports `web_sys`/`js_sys`/`gloo`/adapters (FR-014); every screen usable in fallback mode (US4); no literal colours outside tokens (US5)
**Scale/Scope**: ~10 routes, one proof screen, framework for 010–016

## Constitution Check

| Gate | Status |
|---|---|
| I. Wire compatibility | PASS. jellyfin-web stays at `/web/`; API routes are untouched (serving contract); the collision test guards it |
| III. Test-first + ODD | PASS. Renderer-mode, fallback-reason, route-paint and long-frame signals defined (spec §Observability), emitted via 016's facility and asserted in tests |
| IV. Never panics/leaks | PASS. wasm panic hook reports (016); no tokens in storage |
| V. Types, traits at IO edges | PASS. Typed routes, typed tokens, ports at every external edge, generics via adapter aliases (R6) |
| Tech: Dioxus → WASM, Nix | PASS. New crates build under the devShell; `pharos-ui` stays out of workspace-hack |
| V16 (amended), V172 | PASS. Native-first for new views (017 Q2) |

No violations. Re-checked after design: unchanged.

**ADRs to write when the feature lands**: (1) hybrid DOM + WebGL2 renderer via
`glow` with mandatory CSS fallback; (2) pharos-ui served at `/` with the
collision rule (also supersedes the `/ui` mount in ADR-0012's "build wiring").

## Project Structure

```text
specs/009-ui-foundation/
├── spec.md plan.md research.md data-model.md quickstart.md
└── contracts/{ports.md, serving.md}

crates/pharos-ui/src/
├── app.rs                 # root: providers (adapters, tokens, messages), router outlet
├── routes.rs              # #[derive(Routable)] enum + collision-test export
├── shell/                 # side rail, top bar, toasts, sheets, mini-player slot
├── data/                  # Dioxus bindings over pharos-client cache (use_query, use_mutation)
├── render/                # Renderer port, GlowRenderer, NullRenderer, sprite hook, shader catalogue
├── tokens/                # DesignTokens → CSS vars + uniforms
├── i18n/                  # Fluent bundle, locales/en/*.ftl, pseudo locale
├── ports/  adapters/      # traits + adapter aliases (R6)
├── views/                 # per-route views; existing panes moved here unchanged
└── bin/web.rs
crates/pharos-ui/tests/{ssr_render.rs, ports/*.rs}
crates/pharos-server/src/main.rs, router.rs   # root serving + /ui redirect (contracts/serving.md)
charts/pharos/templates/pharos-ui.yaml        # angie fallback + /pharos/rpc upgrade
compat-playwright/tests/dioxus/*.spec.ts      # routing, renderer modes, axe, pseudo-locale, collisions
```

## Phases (outline for `/speckit-tasks`)

1. **Measure** (T001, T002): devShell `dx build --release` sizes for today, with `glow`, with `pharos-client`, with fluent; set the NFR-001 gate and add the CI size check. Record Steam Frame capabilities.
2. **Split + router**: move `app_state.rs` panes into `views/` unchanged (SSR tests stay green). Introduce `routes.rs` with dioxus-router. Server serving change and `/ui` redirect. Collision test.
3. **Ports**: traits, fakes, contract suites, the view-import lint. Wire `PharosApi` native (017) and transitional REST adapters.
4. **Data layer**: `use_query`/`use_mutation` over the `pharos-client` cache. Event-driven invalidation from `LiveChannel`. SC-003 test.
5. **Tokens + i18n**: token types, CSS emission, literal-colour lint, Fluent, pseudo-locale CI.
6. **Renderer**: `NullRenderer` first (the fallback is the baseline), then `GlowRenderer`, sprite hook, visible-set sync, context-loss handling, mode decision events.
7. **Shell + proof rail**: shell, proof rail in both modes, axe run, budgets (NFR-002..004) recorded.
8. **Docs**: ADRs, CLAUDE.md §Web UI build (root path, profile), the chart.
