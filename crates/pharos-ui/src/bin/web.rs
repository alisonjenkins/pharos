//! WASM entrypoint. Compiled only with `--features web` for the
//! `wasm32-unknown-unknown` target. The Nix devShell ships `dx`
//! (dioxus-cli) which drives the full build:
//!
//!     nix develop --command dx serve --package pharos-ui
//!
//! For a production bundle:
//!
//!     nix develop --command dx build --package pharos-ui --release
//!
//! Output lands under `target/dx/pharos-ui/release/web/public/`. Point
//! pharos-server at it via `[server].ui_dir` in `config.toml`.

fn main() {
    dioxus_web::launch::launch(pharos_ui::App, vec![], dioxus_web::Config::new());
}
