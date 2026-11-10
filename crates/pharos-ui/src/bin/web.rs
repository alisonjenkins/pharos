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
//! Output lands under `target/dx/pharos-ui/release/web/public/`; served at `/ui/`. Point
//! pharos-server at it via `[server].ui_dir` in `config.toml`.

fn main() {
    pharos_ui::native::register_service_worker();
    dioxus_web::launch::launch_cfg(pharos_ui::App, dioxus_web::Config::new());
}
