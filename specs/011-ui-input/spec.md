# Feature Specification: keyboard-first, mouse-efficient input model

**Feature directory**: `specs/011-ui-input/`
**Created**: 2026-11-01
**Status**: draft (`/speckit-specify` stage; next: `/speckit-clarify`)
**Input**: "Efficient for use via keyboard and mouse." (Alison, 2026-11-01)
**Depends on**: 009 (router, shell, focus ring)

## Goal

Anything in pharos-ui can be done without a mouse, usually in fewer keystrokes
than clicks, and anything can be done with a mouse alone without hidden
gestures. Today the crate handles no key events at all.

## User Stories

### US1 — spatial navigation (P1)

Arrow keys (and `h j k l` when enabled) move focus between cards, rails, menus
and buttons by screen position, Netflix-TV style; `Enter` activates, `Esc`/
`Backspace` goes back. Focus is always visible and never lost after a screen
change.

**Independent test**: from home, reach the third poster of the second rail and
open it using only arrows + Enter; Back returns focus to that poster.

### US2 — command palette (P1)

`Ctrl/Cmd+K` (and `/` for search-only) opens a palette that fuzzy-finds items,
people, libraries, routes ("Settings → Subtitles"), and actions ("Mark season
played", "Start group", "Scan library", "Switch theme to Kali Purple"). Results
are context-aware: on an item page, its actions rank first.

**Independent test**: `Ctrl+K`, type "kali", Enter → theme switches. `Ctrl+K`,
type part of a film title, Enter → its detail page.

### US3 — single-key actions (P1)

On a focused item: `p` play, `r` resume / play from start chooser, `f`
favourite, `w` toggle watched, `i` info, `q` add to queue, `a` add to
playlist/collection, `.` context menu. Globally: `g h` home, `g l` libraries,
`g s` search, `g t` together, `g ,` settings. `?` shows the sheet.

### US4 — player keys (P1)

`Space`/`k` play-pause, `←/→` ±10 s (`Shift` ±60 s, `,`/`.` frame step when
paused), `↑/↓` volume, `m` mute, `f` fullscreen, `c` cycle subtitles, `Shift+C`
subtitle menu, `a` cycle audio, `s` skip intro/outro when offered, `n`/`N`
next/previous, `0–9` seek to 0–90 %, `[`/`]` speed, `t` trickplay scrub mode.
Keys work in fullscreen and without the chrome visible.

### US5 — mouse efficiency (P1)

Hover a card for a preview (backdrop + quick actions) after a short delay;
right-click any item for its context menu; middle-click / Ctrl-click opens in a
new tab (real URLs, 009); drag to reorder playlists and queue; shift-click and
drag-rectangle multi-select in grids; mouse wheel scrolls rails horizontally on
hover; thumb buttons (Back/Forward) navigate.

### US6 — multi-select and bulk actions (P2)

`x` or Ctrl-click toggles selection; `Shift+arrows` extends; a bulk bar offers
mark played, favourite, add to playlist/collection, refresh metadata (admin),
delete (admin with confirmation).

### US7 — remap and inspect (P2)

Settings → Keyboard lists every binding with search, shows conflicts, lets the
user rebind, pick a preset (Default, Vim, Media-keys-only), and reset. Bindings
sync with the user (as themes do, 010 US3).

### US8 — media keys and gamepads (P3)

Hardware media keys via the Media Session API (also gives OS now-playing
controls). Gamepad as a spatial-navigation source `[NEEDS CLARIFICATION: in v1?]`.

## Functional Requirements

- **FR-001** Focus manager: one focus tree per route with named zones (side rail,
  top bar, rails, grid, dialog); spatial algorithm over DOM rects; remembered
  focus per zone and per history entry; focus trap in dialogs.
- **FR-002** Keymap engine: layered contexts (global → route → zone → widget →
  player), chords (`g h`), modifiers, per-platform `Cmd`/`Ctrl`, never steals
  keys while a text field has focus (except `Esc`).
- **FR-003** Action registry: every user-visible action is registered once with
  id, label, icon, context predicate, default keys; the palette, menus,
  shortcut sheet and remapping UI all read from it. No action exists only as a
  click handler.
- **FR-004** Command palette with fuzzy match over actions + server search
  (`/Search/Hints`), recent items, keyboard-only flow, ≤ 50 ms local results.
- **FR-005** Shortcut sheet (`?`) generated from the registry for the current
  context.
- **FR-006** Hover/context menu/drag/multi-select per US5–US6, all also
  reachable by keyboard.
- **FR-007** Input-mode detection: show focus glow on keyboard/gamepad use, hide
  hover-only affordances on touch; no "layout mode" setting (replaces
  jellyfin-web layout modes, gap D16).
- **FR-008** Bindings persisted per user, overridable per device.
- **FR-009** Media Session API: metadata, artwork, play/pause/seek/next/prev.

## Success criteria

- **SC-001** Playwright "no mouse" suite: log in, browse to an episode, play,
  switch subtitle, skip intro, mark the season watched, start a group — keyboard
  only.
- **SC-002** Every registered action is reachable from the palette (test
  enumerates the registry).
- **SC-003** No key binding fires while typing in any input (test types the
  whole keymap into the search box).

## Edge cases

- Non-US layouts: bindings use `KeyboardEvent.key` for characters and `code` for
  positional keys (`[`/`]`) `[NEEDS CLARIFICATION: confirm UK layout is primary]`.
- Browser-reserved shortcuts (Ctrl+W, Ctrl+T) are never bound.
- Focus target removed by a live update (009 FR-004): focus moves to nearest
  neighbour, not to `<body>`.
