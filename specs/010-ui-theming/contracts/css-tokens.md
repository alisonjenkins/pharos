# Contract: CSS custom properties

`css::emit` writes these, and `pharos.css` reads only these (009 FR-007, US5).
The names are an interface between the theme engine and every stylesheet, so
renaming one is a breaking change to the sheet.

| Property | From |
|---|---|
| `--px-color-<role>` | each PaletteRole, as `#rrggbb` or `#rrggbbaa` |
| `--px-color-kind-<kind>` | per-kind accents |
| `--px-font-ui`, `--px-font-display` | FontStack → a CSS font-family list |
| `--px-type-base`, `--px-type-scale` | `base_px` as `px`, `scale` unitless |
| `--px-type-step-{-1,0,1,2,3,4}` | `base_px × scale^n`, in `rem` |
| `--px-space-unit` | 8 px comfortable, 6 px compact |
| `--px-radius`, `--px-border` | `radius_px`, `border_px` as `px` |
| `--px-motion-duration`, `--px-motion-easing`, `--px-motion-focus-scale` | `motion` |
| `--px-effect-blur`, `--px-effect-vignette`, `--px-effect-glow` | `effects`, for the CSS fallback |

Rules:
- A test fails when `pharos.css` contains a colour literal (hex, `rgb()`,
  `hsl()` or a named colour other than `transparent` or `currentColor`)
  outside its `:root` block (009 US5).
- `pharos.css`'s `:root` equals `css::emit(Pharos Night)` byte for byte
  (research R3).
- `prefers-reduced-motion: reduce` sets `--px-motion-duration: 0ms`
  regardless of theme.
