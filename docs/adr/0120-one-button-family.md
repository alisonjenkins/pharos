# ADR-0120: One button family, a slim pill

- **Status:** Accepted
- **Date:** 2026-11-11T07:24:49Z
- **Deciders:** Alison

## Context

The web and TV UI drew buttons six ways (B1343): 4px blocks, blocks at the
theme's corner radius, small chips, slim 2.25rem pills, and three shapes on the
video. Each page added its own rule, so the same control (the queue buttons,
B1344) changed shape between pages. The newest pieces, the library filter row
(B1311) and the music tab row (B1334), are slim pills, and the album page
redesign was being mocked up in the same shape.

## Decision

We will draw every button from one family in `pharos.css`:

- Shape: a pill, `border-radius: calc(var(--px-radius) * 999)`. A theme with
  square corners (`radius_px = 0`, Neon Grid) gets square buttons; every other
  theme gets pills.
- Kinds: primary `.pharos-button` (accent fill, one per view), secondary
  `.pharos-button-secondary` (surface and border; also toggles, filled while
  `aria-pressed="true"`), quiet `.pharos-button-quiet` (no border or fill),
  danger `.pharos-button-danger` (outline; `is-strong` fills it to confirm).
  `.pharos-chip` is the secondary kind under its older name.
- Modifiers: `is-small` (rows and tiles) and `is-icon` (square width).
- Sizes are `:root` variables: 2.25rem and 1.75rem small; 2.75rem and 2.25rem
  on a coarse pointer (44px touch target); 1.9rem and 1.5rem on a TV.
- Existing class names stay, so tests, the TV build and other threads' work keep
  working; pages move onto the family one change at a time.

Alison chose the pill (with square corners in a square theme) over following
the theme's corner radius and over pills in every theme.

## Consequences

- One rule decides how a button looks; a new button needs only a class.
- Any rule that sets a corner on a family button is a test failure (V1314).
- The player bar, the mini player and the on-video buttons (skip, Up Next,
  playback failure) still have their own rules; they move in a follow-up
  (quiet icon pills and an `is-on-video` kind).
- The listen view keeps its smaller borderless chips until Alison asks for a
  change there.

## Alternatives considered

- Follow each theme's corner radius (8px rounded blocks in Pharos Night): would
  have reshaped the newer filter row and music tabs instead of the older blocks.
- Pills in every theme: loses the square look Neon Grid is built on.

## References

B1343–B1345, V1314–V1316; the proposal page shown to Alison on 2026-11-11.
