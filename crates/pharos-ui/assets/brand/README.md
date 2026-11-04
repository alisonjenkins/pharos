# Pharos brand assets

The **Beam P** mark: a P whose bowl holds a lit play button, with a
lighthouse beam sweeping out of it. Original artwork drawn for Pharos.
The wordmark is lowercase "pharos" set in Inter Display SemiBold
(SIL Open Font License) at -1.5% tracking, converted to outlines so no
font is needed to render it.

| File | Use |
|---|---|
| `mark.svg` | The mark alone, Pharos Night colours. |
| `wordmark.svg` | The wordmark alone, `fill="currentColor"`. |
| `logo.svg` | Mark + wordmark for dark backgrounds. |
| `logo-light.svg` | Mark + wordmark for light backgrounds. |
| `favicon.svg` | Browser tab icon; follows the OS light/dark scheme. |
| `favicon.ico` | 16/32/48 px fallback for browsers without SVG icons. |
| `apple-touch-icon.png` | 180 px, opaque, for iOS home screens. |
| `icon-192.png`, `icon-512.png` | Web manifest icons (rounded tile). |
| `icon-maskable-512.png` | Web manifest maskable icon (mark inside the 80% safe zone). |
| `manifest.webmanifest` | Installable-app manifest scoped to `/ui/`. |

## Colours

| Role | Pharos Night | Daylight |
|---|---|---|
| Body (theme accent) | `#4cb4ff` | `#0b63c5` |
| Beacon light | `#ffd27a` | `#f0a020` |
| Wordmark | `#e6e8eb` | `#15181d` |
| Tile background | `#0e1116` | — |

Inside the UI the header draws the mark inline (`native::views::BrandMark`)
in the active theme's own colours: body and beam in `--px-color-accent`,
the play button in `--px-color-accent-fg`. Every theme keeps that pair
legible, where the amber beacon would vanish on High Contrast's yellow.

## Geometry

The mark sits on a 64×64 grid: the P stem runs x 12–24, the bowl is a
20-unit-radius arc ending at x 52, the play button is the triangle
(26,16.5)–(26,35.5)–(42,26), and the beam is the wedge
(44,22)–(64,12)–(64,36)–(44,30) fading to transparent.

## Regenerating

`tools/build_brand.py` holds the drawing and writes every SVG and the
manifest; `tools/raster.js` renders the PNGs with headless Chromium.
After changing the geometry, run

    tools/regen.sh /path/to/InterDisplay-SemiBold.otf

(python3 with fontTools and Pillow, node with playwright). It reproduces
the committed files byte for byte when nothing changed. Then update the
`BRAND_*` constants in `src/native/views.rs`; a test fails until the
inline mark and the shipped SVGs agree.
