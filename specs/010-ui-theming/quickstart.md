# Quickstart: 010-ui-theming

All commands run in `nix develop`.

1. **Model, resolver, contrast and CSS (phase 1)**:
   `cargo nextest run -p pharos-ui theme::`
   - Expect every shipped theme to resolve with no warnings and pass
     contrast.
   - Expect each error variant to name its path and value.
2. **Stylesheet contract**:
   `cargo nextest run -p pharos-ui css_contract`
   - Expect it to fail when you add `color: #fff` to a rule in `pharos.css`.
   - Expect it to fail when you change one value in `:root` without changing
     Pharos Night.
3. **In the browser (phase 2)**: `just compat-dioxus` runs the theming spec
   in `compat-playwright/tests/dioxus/`:
   - pick each shipped theme, and check that `--px-color-bg` on `<html>`
     changes with no reload
   - reload, and check that the pick is kept
   - put a corrupt value in `pharos.theme.active`, reload, and check that the
     default theme is applied with a `pharos: theme fallback … reason=storage`
     console line
   - import a malformed file, and check the error names the field
4. **Signal check**: in the browser console, filter on `pharos: theme`. Every
   apply prints `applied`, and every rejected input prints `fallback` or
   `rejected` with its reason and value. Once Telemetry lands (017 T069), the
   same events become queryable in Loki as `themeFallback`. T069 defines the
   query, and this step gets updated to name it.
