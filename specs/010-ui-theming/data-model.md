# Data model: 010-ui-theming

## ThemeDoc (the file, schema 1)

The TOML a user imports and exports, and the form stored (R1, R5). Every
key except `[meta].name` and `[meta].schema` is optional. Anything missing
comes from `extends`, then from the built-in base (Pharos Night).

```toml
[meta]
name = "Kali Purple"          # unique within the user's themes; 1..=64 chars
schema = 1                     # ThemeDoc schema version
author = "pharos"
version = "1.0.0"
scheme = "dark"               # dark | light
extends = "Pharos Night"      # a shipped or user theme name; no cycles

[palette]                      # #rgb, #rrggbb or #rrggbbaa
bg = "#120a1f"
accent = "#b06cff"
# … one key per PaletteRole

[palette.kind]                 # per-kind accents
movie = "#…"

[type]
ui = "system-ui"              # FontStack: system-ui | sans | serif | mono (R7)
display = "sans"
scale = 1.2                    # 1.0..=1.6, the modular type ratio
base_px = 16                   # 12..=24
mono_ui = false

[shape]
radius_px = 8                  # 0..=32
border_px = 1                  # 0..=4
density = "comfortable"       # comfortable | compact

[motion]
duration_ms = 180              # 0..=1000
easing = "standard"           # standard | emphasised | linear
focus_scale = 1.05             # 1.0..=1.2

[effects]                      # validated now; rendered only as CSS fallback until 009 FR-005
background = "gradient"       # solid | gradient | image | shader
shader = "scanlines"          # ShaderName, allow-listed; only with background = "shader"
blur_px = 24                   # 0..=64
vignette = 0.3                 # 0..=1
grain = 0.0                    # 0..=1
glow = 0.6                     # 0..=1

[layout]
hero = true
rail_poster = "landscape"     # portrait | landscape | numbered
rail_collapsed = false

[contrast]
level = "aa"                   # aa | aaa; High Contrast is aaa (7:1 text)
decorative = ["badge"]         # non-text roles the user accepts below the minimum
```

## PaletteRole (closed set)

`bg`, `surface`, `surface-raised`, `overlay`, `border`, `fg`, `fg-muted`,
`accent`, `accent-fg`, `focus`, `success`, `warning`, `danger`, `progress`,
`badge`. Per-kind accents: `movie`, `show`, `music`, `book`, `live`.

`border` is new relative to the spec's list. It is the role behind
`pharos.css`'s two border literals (R3).

## Theme (resolved, in memory)

Built by `resolve(doc, catalogue) -> Result<Resolved, ThemeError>`. Every
field holds a typed value with nothing optional:
- `Rgba` for each role
- `FontStack`
- bounded newtypes (`Ratio`, `Px`, `Unit01`, `Millis`)
- enums for the rest

An out-of-range number cannot be represented once resolved (constitution V).

`Resolved { theme: Theme, warnings: Vec<ThemeWarning> }`.

## ThemeError (rejects the document) and ThemeWarning (loads it)

Each error and warning names the TOML path and the offending value
(constitution IV: expose the cause). Errors:
- `Syntax { line, col, message }`
- `MissingKey { path }`
- `BadValue { path, value, expected }`
- `OutOfRange { path, value, min, max }`
- `UnknownBase { name }`
- `ExtendsCycle { chain }`
- `ExtendsTooDeep { chain }` (a chain of more than `MAX_EXTENDS_DEPTH` = 8 themes)
- `Contrast { pair, ratio, min }` (only when saving or importing, US6)
- `TooLarge { bytes }` (over 64 KiB, before parsing)
- `UnknownTheme { name }` (a stored name that is not in the catalogue)
- `Storage { message }` (the browser could not read `localStorage`)

Warnings:
- `UnknownKey { path }`
- `NewerSchema { found, supported }`
- `FontMissing { path, value }`
- `EffectsReduced { reason }`

## ContrastPair

`{ fg: PaletteRole, bg: PaletteRole, min: Ratio, text: bool }`. The list is
fixed in code (R4). `check(theme) -> Vec<ContrastResult { pair, ratio, pass,
decorative }>` feeds both the editor's readout (FR-003) and the save gate
(US6).

## TokenSet → CSS

`css::emit(&Theme) -> String`: a single `:root { … }` block of
`--px-<group>-<name>: <value>;` in a fixed order. Groups are `color`,
`font`, `type`, `space`, `radius`, `border`, `motion` and `effect`. It is
applied as one `style` attribute write on `<html>` (SC-002: one write, one
style recalculation). See contracts/css-tokens.md.

## DeviceThemeState (`localStorage`, R5)

- `pharos.theme.active`: a theme name or `auto`.
- `pharos.theme.custom`: a JSON array of ThemeDoc TOML strings.

State machine for the page's theme:
- `Boot`: the `pharos.css` defaults are showing.
- From `Boot`, read the active name:
  - shipped or valid custom → `Applied(name, source)`
  - else → `Applied(default, shipped)` and log a `fallback` line
- On a user pick or edit, `Applied` → `Applied`.
- `auto` resolves through `prefers-color-scheme` and `prefers-contrast` (US7)
  and re-resolves on their `change` events.

## Signals (ODD)

Browser console lines until `Telemetry.submit` exists (R6). They use the
playback logs' `pharos: key=value` shape, and success and failure carry the
same fields:

```
pharos: theme applied name=<n> source=shipped|local|import schema=<v> warnings=<count>
pharos: theme warning name=<n> source=<s> reason=unknown_key|newer_schema|font_missing|effects_reduced path=<p> value=<v>
pharos: theme fallback name=<n> source=<s> reason=syntax|missing_key|bad_value|out_of_range|unknown_base|extends_cycle|extends_too_deep|too_large|unknown_theme|storage path=<p> value=<v> to=<default>
pharos: theme rejected name=<n> source=import|editor reason=contrast pair=<fg>/<bg> ratio=<r> min=<m>
```

Reasons are stable strings from a `label()` method, asserted distinct in a
test. When telemetry lands, `fallback` maps to `EventKind.themeFallback`.
