# Feature Specification: user-themeable pharos-ui

**Feature directory**: `specs/010-ui-theming/`
**Created**: 2026-11-01
**Status**: draft (`/speckit-specify` stage; next: `/speckit-clarify`)
**Input**: "Take inspiration from UIs like Netflix + Kali's prettiest themes …
themeable by the user." (Alison, 2026-11-01)
**Depends on**: 009 (design tokens FR-007, GPU layer FR-005)
**Replaces**: jellyfin-web Branding custom-CSS (gap assessment D6)

## Goal

Every colour, font, shape, motion and GPU effect in pharos-ui comes from a
**theme**. pharos ships a small set of polished themes, users can pick, tweak,
create, import and export themes, and an admin can set the server default.

## Theme model

A theme is a versioned, declarative document (TOML or JSON `[NEEDS
CLARIFICATION: format — recommend TOML to match config.toml]`) with:

1. **Metadata** — name, author, version, schema version, light/dark, base theme
   it extends.
2. **Palette roles** — `bg`, `surface`, `surface-raised`, `overlay`, `fg`,
   `fg-muted`, `accent`, `accent-fg`, `focus`, `success`, `warning`, `danger`,
   `progress`, `badge`, plus per-kind accents (movie, show, music, book, live).
3. **Typography** — UI and display font families (from a bundled, licence-clear
   set plus system fonts), type scale, weights, letter-spacing, optional
   monospace UI mode.
4. **Shape** — radii, border widths, card aspect ratios, density
   (comfortable/compact).
5. **Motion** — durations, easings, focus scale, hero transition style.
6. **Effects (GPU layer)** — background (solid, gradient, image, animated
   shader from an allow-listed set), backdrop blur radius, vignette, grain,
   scanlines, glow colour/intensity on focus, poster reflection. Each effect has
   a static CSS fallback.
7. **Layout hints** — hero banner on/off, rail poster style (portrait, landscape,
   numbered top-10), side-rail collapsed by default.

Themes contain **data only**: no CSS strings, no JS, no shader source. Shaders
are chosen from a built-in list by name with bounded parameters. This keeps
user-shared themes safe to import.

## Shipped themes (v1)

| Theme | Inspiration | Character |
|---|---|---|
| **Pharos Night** (default) | Netflix | Near-black, red-free brand accent (pharos blue), large hero with blurred backdrop, landscape rails, bold display type |
| **Marquee** | Netflix | Black + crimson accent, numbered "Top 10" rails, heavy hero fade |
| **Kali Dark** | Kali Linux default dark | Deep navy/black, electric-blue accent, sharp corners, monospace accents |
| **Kali Purple** | Kali Purple edition | Violet gradient background, purple/magenta glow focus |
| **Neon Grid** | Kali "hacker" look | Black, green phosphor accent, scanlines + subtle matrix-rain background shader, monospace UI |
| **Daylight** | — | Light theme meeting AA, for bright rooms |
| **High Contrast** | — | WCAG AAA, no effects, thick focus ring |

`[NEEDS CLARIFICATION: which Kali themes do you mean by "prettiest" — default
Kali dark (Xfce/GNOME), Kali Purple, or community themes? Screenshots or names
would let 010's palette match.]` Names above are pharos's own; no Kali or
Netflix trademarks or artwork are shipped.

## User Stories

### US1 — pick a theme (P1)

A user opens Settings → Appearance, sees live previews of each theme, picks one,
and the whole app (DOM + GPU layer) switches without reload.

**Independent test**: switch theme; every token-driven surface changes; no
component keeps the old colour (screenshot diff against token map).

### US2 — tweak a theme (P1)

A user changes accent colour, density, font, and turns effects down, with live
preview, and saves it as "My theme".

**Independent test**: edit accent → focus ring, progress bars and buttons update
live; reload keeps it.

### US3 — themes follow the user (P1)

A user's chosen and custom themes are stored on the server and apply on any
browser they sign in on. A per-device override is allowed (e.g. no effects on
the old laptop).

**Independent test**: set theme on browser A; sign in on browser B; same theme.

### US4 — share themes (P2)

Export a theme to a file; import one from a file or URL; imported theme is
validated and previewed before applying.

**Independent test**: export → import on another user → identical tokens.
Malformed or out-of-range values are rejected with the field named.

### US5 — server default and login screen (P2)

An admin sets the default theme for new users and the login screen, plus a
login message (replaces Branding's disclaimer).

### US6 — accessibility guard (P1)

The theme editor refuses to save a theme whose text/background or focus pairs
fall below WCAG AA, showing which pair fails and by how much, unless the user
explicitly accepts "decorative low contrast" for a non-text role.

### US7 — follow the OS (P3)

"Auto" picks a light or dark theme from `prefers-color-scheme`, and the
high-contrast theme from `prefers-contrast: more`.

## Functional Requirements

- **FR-001** Theme schema with versioning and `extends`; unknown keys ignored with
  a warning; missing keys inherit from the base.
- **FR-002** Theme engine resolves a theme to the 009 token set and applies it as
  CSS custom properties + GPU uniforms in one frame.
- **FR-003** Appearance settings page with gallery, live preview, editor (colour
  pickers with role-pair contrast readout, font, density, effects sliders).
- **FR-004** Storage: per-user themes persisted server-side through the native
  `Prefs` interface (017), so they also apply to future pharos phone and TV apps.
  Tokens are platform-neutral (no CSS in the stored form) so those apps can
  render the same theme.
- **FR-005** Import/export as a file; import from URL fetched by the browser,
  never by the server.
- **FR-006** Contrast checker over every declared text/background pair (FR US6).
- **FR-007** Effects budget: the engine scales effects down when frame time
  exceeds budget (009 NFR-003) and records that it did.
- **FR-008** Admin: default theme, login theme, login message, allow/deny user
  custom themes.
- **FR-009** All shipped themes pass FR-006 and the 009 axe run.

## Success criteria

- **SC-001** Seven shipped themes, each passing contrast and axe checks.
- **SC-002** Switching themes repaints in ≤ 1 frame after tokens resolve.
- **SC-003** A third-party-authored theme file round-trips byte-for-byte through
  export → import.

## Edge cases

- A theme naming a font that is not bundled → falls back to system stack.
- A theme from a newer schema version → load what is understood, warn.
- GPU effects requested while in reduced-effects mode → CSS fallbacks only.

## Out of scope

Arbitrary user CSS or shader code. Per-library themes.
