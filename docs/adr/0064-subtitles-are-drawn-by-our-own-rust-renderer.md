# ADR-0064: Pharos clients draw text subtitles with their own pure-Rust renderer

- **Status:** Proposed
- **Date:** 2026-11-05T21:30:00Z
- **Deciders:** Alison Jenkins

## Context

The native web UI draws plain subtitles with the browser's `<track>` and
`::cue`, and ASS/SSA with a vendored JASSUB (libass in WASM, ADR-0057).
`::cue` takes no radius or padding, so the rounded, translucent backdrop
Alison wants exists only through a Chromium-only pseudo-element; Firefox,
the Android WebView and the Android shell each look different, and cue
placement is the user agent's. JASSUB cannot run outside a browser, costs
~2.1 MB of wasm, and is a second renderer to keep in step. Pharos is
heading to Android TV (ADR-0061) and phones, and an animated-subtitles
effort will need a renderer we control. The weakest target, the TCL G08 TV
(armv7, Android WebView), must play animated subtitles without dropping
video frames.

## Decision

We will draw every text subtitle with a new pure-Rust crate,
`pharos-subs`, behind a `SubtitleRenderer` port (spec 009 §Boundaries), as
specified in `specs/026-subtitle-renderer/spec.md`.

- Parsing, shaping (`harfrust`), font outlines (`skrifa`), bidi, line
  breaking, rasterising and blur are Rust with no C dependencies, so one
  crate builds for `wasm32-unknown-unknown`, Android and native hosts.
- Output is libass-style coverage masks plus tints; the `render/` WebGL2
  layer (ADR-0039/0049) composites them. The renderer runs in a Web Worker
  as a standalone wasm module, also inside the Android WebView, and
  rasterises only when an event appears or an animated one changes.
- Performance budgets on the TCL G08 (spec 026 table) gate every slice.
- libass stays the reference: the server's burn path renders the
  conformance goldens, and JASSUB remains the ASS fallback until the Rust
  renderer passes the corpus and the G08 budgets. A server-side libass
  pre-render (019's `image` delivery) is the fallback for scenes a device
  cannot animate in time.
- The server lists and serves a file's font attachments, subset to the
  glyphs a track uses, and labels sidecar `.ass` files correctly.

## Consequences

- Plain subtitles look the same on Chromium, Firefox, the TV and later
  clients, with Pharos's style, size picker and placement.
- We own an ASS renderer: compatibility with libass is ours to keep, tag by
  tag, and quirks outside the corpus may differ. Unsupported tags are
  counted so gaps are visible.
- The wasm download for subtitles drops from ~2.1 MB to a budget of 800 KB
  gzip, if open question 1 holds.
- More code to maintain than vendoring JASSUB: roughly a parser, layout
  engine, rasteriser and blur, plus a corpus and benches.
- The font-attachment and sidecar fixes (slice 0) help JASSUB at once,
  before any renderer work.

## Alternatives considered

- **Keep JASSUB and restyle `::cue`.** No new renderer, but `::cue` cannot
  take the requested style outside Chromium, JASSUB does not run off the
  web, and two renderers stay.
- **Our own libass build linked into the Rust wasm.** Needs a C toolchain
  and libc for wasm (emscripten or WASI) that the `wasm32-unknown-unknown`
  Dioxus build does not have; it is JASSUB rebuilt by hand. Natively on
  Android it would work, but the WebView still needs a wasm build.
- **`rassa` (MIT, pure-Rust libass rewrite).** Young (0.7.1, ~270
  downloads) and its renderer depends on FreeType and fontconfig, so it
  does not build for `wasm32-unknown-unknown`. Its parser may be reused.
- **Server burn-in or server pre-render for everything.** Lowest client
  cost, but every subtitle switch or size change re-renders or transcodes;
  kept only as the weak-device fallback.
- **WebVTT with positions.** No fonts, borders, rotation or karaoke.

## References

- `specs/026-subtitle-renderer/spec.md`; ADR-0057 (JASSUB), ADR-0060 (image
  subtitles), ADR-0039/0049 (GPU layer), ADR-0061 (Android TV)
- `crates/pharos-ui/assets/pharos.css` (`::cue` limits),
  `crates/pharos-ui/src/native/web.rs` (`StyledSubtitles`)
- https://github.com/altqx/rassa
