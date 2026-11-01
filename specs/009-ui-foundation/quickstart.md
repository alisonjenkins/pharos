# Quickstart / validation: 009-ui-foundation

Inside `nix develop`.

1. **Build and size** (NFR-001):
   `dx build --package pharos-ui --release`, then `just ui-size` (new recipe, T001) prints the
   compressed first-load total against the gate.
2. **Serve at root**: set `[server].ui_dir` to the bundle and start pharos.
   - `curl -sI localhost:8096/` → `200`, `text/html`
   - `curl -s -H 'Accept: application/json' localhost:8096/title/1 -o /dev/null -w '%{http_code}'` → `404`
   - `curl -sI localhost:8096/ui/settings/display` → `308` to `/settings/display`
   - `curl -s localhost:8096/web/` → jellyfin-web, unchanged
3. **Collision guard**: `cargo nextest run -p pharos-server ui_routes_do_not_shadow_api`.
4. **Ports**: `cargo nextest run -p pharos-ui ports::` (each contract suite against each host adapter) and `cargo nextest run -p pharos-ui views_import_no_adapters`.
5. **Browser suite**: `just compat-dioxus` covers deep link + refresh + Back
   restore, a live update after a played toggle from another client (≤ 2 s),
   the proof rail in WebGL2 and fallback modes (screenshot diff of content and
   layout), axe with zero serious/critical violations, and pseudo-locale coverage.
6. **By query after deploy**: `sum by (mode,reason) (rate(pharos_ui_renderer_mode_total[1d]))`
   (only from opted-in users, 016) and `histogram_quantile(0.95, sum by (le,route) (rate(pharos_ui_route_paint_seconds_bucket[1d])))`.
