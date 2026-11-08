//! Renders the spike's screens to PNGs on the host with Blitz's CPU
//! renderer, at the TV's size (1920x1080 physical, scale 2), so CSS gaps
//! show up before anything goes on the TV.
//!
//! `cargo run -p pharos-tv-native --example snapshot -- <out-dir>`

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../src/screens.rs"]
mod screens;

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_traits::shell::{ColorScheme, Viewport};
use dioxus::prelude::*;
use dioxus_native_dom::DioxusDocument;
use screens::{Screen, Spike};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const SCALE: f32 = 2.0;

#[derive(Props, Clone, PartialEq)]
struct Page {
    screen: Option<Screen>,
}

#[allow(non_snake_case)]
fn Root(props: Page) -> Element {
    match props.screen {
        Some(screen) => rsx! { Spike { start: screen } },
        // A control: plain text and an aspect-ratio box with no pharos CSS.
        None => rsx! {
            div { style: "color: white; background: #222; font-size: 40px; padding: 20px;",
                p { "Probe: plain text renders" }
                p { style: "font-family: 'DejaVu Sans'", "Probe: DejaVu Sans by name" }
                div { style: "width: 300px; display: flex; gap: 10px;",
                    // auto-width block child with aspect-ratio, inside a block
                    div { style: "width: 140px;", div { style: "aspect-ratio: 2 / 3; background: red;" } }
                    // same, with position: relative (as .pharos-poster-art)
                    div { style: "width: 140px;", div { style: "position: relative; aspect-ratio: 2 / 3; background: lime;" } }
                }
                div { style: "width: 200px; aspect-ratio: 2 / 3; background: teal;" }
                div { style: "display: grid; grid-template-columns: repeat(auto-fill, minmax(9rem, 1fr)); gap: 1rem;",
                    for i in 0..6 { div { style: "aspect-ratio: 2 / 3; background: orange;", "{i}" } }
                }
            }
        },
    }
}

/// The host's system fonts are not found under the Nix shell, so register
/// one font as every generic family. `SNAPSHOT_FONT` overrides the file.
fn font_ctx() -> Option<parley::FontContext> {
    use fontique::{Blob, GenericFamily};
    let path = std::env::var("SNAPSHOT_FONT")
        .unwrap_or_else(|_| "/run/current-system/sw/share/X11/fonts/DejaVuSans.ttf".into());
    let bytes = std::fs::read(&path).ok()?;
    let mut ctx = parley::FontContext::default();
    let families: Vec<_> = ctx
        .collection
        .register_fonts(Blob::new(std::sync::Arc::new(bytes)), None)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for generic in [GenericFamily::SansSerif, GenericFamily::Serif, GenericFamily::SystemUi, GenericFamily::UiSansSerif, GenericFamily::Monospace] {
        ctx.collection.set_generic_families(generic, families.iter().copied());
    }
    Some(ctx)
}

fn main() {
    struct Out;
    impl log::Log for Out {
        fn enabled(&self, _: &log::Metadata) -> bool { true }
        fn log(&self, r: &log::Record) { if r.target().contains("screens") { println!("log {}", r.args()); } }
        fn flush(&self) {}
    }
    let _ = log::set_logger(&Out).map(|()| log::set_max_level(log::LevelFilter::Info));
    let out = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    for screen in [None, Some(Screen::Home), Some(Screen::Grid)] {
        let vdom = VirtualDom::new_with_props(Root, Page { screen });
        let mut doc = DioxusDocument::new(
            vdom,
            DocumentConfig {
                viewport: Some(Viewport::new(WIDTH, HEIGHT, SCALE, ColorScheme::Dark)),
                font_ctx: font_ctx(),
                ..Default::default()
            },
        );
        doc.initial_build();
        use blitz_dom::Document as _;
        let started = std::time::Instant::now();
        doc.inner_mut().resolve(0.0);
        let resolved = started.elapsed();
        if screen.is_some() && std::env::var_os("SNAPSHOT_KEYS").is_some() {
            use blitz_traits::events::{BlitzKeyEvent, KeyState, UiEvent};
            use keyboard_types::{Code, Key, Location, Modifiers, NamedKey};
            println!("focussed node {:?}", doc.inner().get_focussed_node_id());
            let keys = if screen == Some(Screen::Grid) { vec![Key::Named(NamedKey::ArrowDown); 4] } else { vec![Key::Named(NamedKey::ArrowRight), Key::Named(NamedKey::ArrowRight), Key::Named(NamedKey::ArrowDown)] };
            for key in keys {
                let ev = |state| BlitzKeyEvent {
                    key: key.clone(), code: Code::Unidentified, modifiers: Modifiers::empty(),
                    location: Location::Standard, is_auto_repeating: false, is_composing: false,
                    state, text: None,
                };
                doc.handle_ui_event(UiEvent::KeyDown(ev(KeyState::Pressed)));
                doc.handle_ui_event(UiEvent::KeyUp(ev(KeyState::Released)));
                doc.poll(None);
            }
            doc.inner_mut().resolve(0.0);
        }
        if std::env::var_os("SNAPSHOT_DEBUG").is_some() {
            for sel in [".pharos-tv-focus", ".pharos-grid", ".pharos-grid > li", ".pharos-poster", ".pharos-poster-art", ".pharos-poster-placeholder", ".pharos-poster-name", ".pharos-rail"] {
                let inner = doc.inner();
                if let Ok(Some(id)) = inner.query_selector(sel) {
                    let node = inner.get_node(id).unwrap();
                    let l = node.final_layout();
                    println!("{sel}: id {id} size {:?} loc {:?} display {:?} attrs {:?}", l.size, l.location, (), node.element_data().map(|e| e.attrs().iter().map(|a| (a.name.local.to_string(), a.value.clone())).collect::<Vec<_>>()));
                } else {
                    println!("{sel}: none");
                }
            }
        }
        let rgba = render_to_buffer::<VelloCpuImageRenderer, _>(
            |scene| blitz_paint::paint_scene(scene, &mut doc.inner_mut(), SCALE as f64, WIDTH, HEIGHT, 0, 0),
            WIDTH,
            HEIGHT,
        );
        let painted = started.elapsed() - resolved;
        let name = screen.map_or("Probe".to_owned(), |s| format!("{s:?}"));
        let path = format!("{out}/{name}.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgba).unwrap();
        println!("{path}: resolve {resolved:?}, paint {painted:?}");
    }
}
