# ADR-0036: Themes are TOML documents over a platform-neutral token model

- **Status:** Accepted
- **Date:** 2026-11-02T11:40:00Z
- **Deciders:** Alison Jenkins

## Context

Spec 010 makes every colour, font, shape and motion value in pharos-ui come
from a theme that users can pick, tweak, share and import. Themes travel
between users, so the file format is a data contract. Spec 010 FR-004 also
says that 017's future phone and TV clients render the same stored theme,
so the stored form cannot be CSS.

009 FR-007 says that design tokens are emitted as CSS custom properties.
Before this change, `pharos.css` had six ad-hoc variables (`--bg`, `--accent`,
…) and six literal colours.

## Decision

- A theme is a **TOML** document (`.pharos-theme.toml`, schema 1;
  `specs/010-ui-theming/contracts/theme-file.md`).
  - It holds data only: no CSS, no script, no shader source. Shaders are
    named from an allow-list.
  - Missing keys inherit through `extends`, rooted at the shipped "Pharos
    Night".
  - Unknown keys and newer schemas load with a warning. Everything else wrong
    rejects the file, naming the TOML path and the offending value.
- The resolved form is a typed `Theme`
  (`crates/pharos-ui/src/theme/resolve.rs`). Bounded numbers and closed enums
  mean an out-of-range value cannot be represented. This is the
  platform-neutral model. CSS is one rendering of it.
- The web UI renders it as `--px-<group>-<name>` custom properties
  (`contracts/css-tokens.md`), set on `<html>` in one style write.
  `pharos.css` reads only those properties. Its `:root` block is
  `emit(Pharos Night)`, and a test checks that byte for byte and rejects any
  colour literal elsewhere in the sheet.
- The theme module has no browser types, so every decision in it is
  host-tested.
- Parsing uses the workspace's `toml` 1.x (`parse`, `display`, `serde`,
  `std`).

## Consequences

- Hand-written and shared themes can carry comments and are readable before
  import. A file exports byte for byte as it was imported (SC-003), because
  the source is stored, not re-serialised.
- `toml` adds wasm. A standalone probe measured about 140 KB optimised, 64 KB
  gzipped, as an upper bound (`specs/010-ui-theming/research.md` R1).
  pharos-ui's own measurement goes there.
- Renaming a `--px-*` property breaks the stylesheet contract, and the
  contract test catches it.
- A schema change bumps `meta.schema`, and the loader keeps reading older
  schemas.

## Alternatives considered

- **JSON.** Costs no wasm, because `serde_json` is already linked. But it has
  no comments, it's noisier to hand-edit, and it doesn't match `config.toml`.
  It stays the fallback if the measured cost is unacceptable. Only the parser
  changes, because the model is format-independent.
- **Themes as CSS** (jellyfin-web Branding's custom CSS). Not safe to import
  from strangers, and not renderable by non-web clients.
- **Keep the old variable names and alias them.** Two names for each token
  would last indefinitely, against a 372-line sheet with one consumer.

## References

- `specs/010-ui-theming/` (spec, plan, research R1 and R3, data model,
  contracts)
- `crates/pharos-ui/src/theme/`, `crates/pharos-ui/themes/`
- 009 FR-007 (tokens), 017 FR-004 (platform-neutral prefs)
