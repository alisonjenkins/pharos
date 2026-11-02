# Contract: theme file (schema 1)

The file a user exports and imports. Shape: data-model.md § ThemeDoc.

- **Encoding**: UTF-8 TOML 1.0. Extension `.pharos-theme.toml`. Media type
  `application/toml`.
- **Size**: at most 64 KiB. Anything larger is rejected before it is parsed.
- **Required**: `[meta].name` and `[meta].schema`. Everything else inherits
  through `extends` (FR-001).
- **Data only**: no key holds CSS, script, a URL that the server fetches, or
  shader source. `effects.shader` is a name from the built-in allow-list.
  `effects.background = "image"` refers to an image the user uploads. Images
  are out of scope until a later task, so for now `image` is a `BadValue`.
- **Unknown keys**: ignored, with one `UnknownKey` warning each.
- **Newer `schema`**: the keys schema 1 knows are loaded, plus one
  `NewerSchema` warning.
- **Round trip (SC-003)**: export writes the document as stored. It is not
  re-serialised from the resolved theme, so a third-party file round-trips
  byte for byte. Normalising (adding keys inherited from `extends`) is a
  separate, explicit "flatten" action.
- **Import from URL (FR-005)**: the browser fetches the file under the page's
  CSP. The server never fetches it. A failed fetch reports its status and the
  URL.
- **Versioning**: a breaking change to this file bumps `schema`, and the
  loader keeps reading every older schema.
