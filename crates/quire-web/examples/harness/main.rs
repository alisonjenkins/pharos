//! Mounts a generated book in `#host` and exposes the view to a Playwright
//! script (`check.mjs`), which checks that a place survives a change of font
//! size and window size.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects
)]

use std::cell::RefCell;
use std::io::{Cursor, Write};
use std::rc::Rc;

use quire::{Locator, Publication};
use quire_web::{Settings, Start, View};
use wasm_bindgen::prelude::*;

thread_local! {
    static VIEW: RefCell<Option<View>> = const { RefCell::new(None) };
}

fn view() -> View {
    VIEW.with(|v| v.borrow().clone()).expect("mounted")
}

/// A book of `sections` chapters, each of long paragraphs that cross pages,
/// with a character outside the Basic Multilingual Plane in every one.
fn book(sections: usize) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default();
    let mut put = |name: &str, body: &str| {
        zip.start_file(name, o).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    };
    put(
        "META-INF/container.xml",
        r#"<container><rootfiles><rootfile full-path="c.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
    );
    let items: String = (0..sections)
        .map(|i| {
            format!(r#"<item id="s{i}" href="s{i}.xhtml" media-type="application/xhtml+xml"/>"#)
        })
        .collect();
    let refs: String = (0..sections)
        .map(|i| format!(r#"<itemref idref="s{i}"/>"#))
        .collect();
    put(
        "c.opf",
        &format!("<package><metadata/><manifest>{items}</manifest><spine>{refs}</spine></package>"),
    );
    for i in 0..sections {
        let paras: String = (0..12)
            .map(|p| {
                let words: String = (0..180).map(|w| format!("w{i}.{p}.{w} ")).collect();
                format!("<p>Para {p} 😀 {words}</p>")
            })
            .collect();
        put(
            &format!("s{i}.xhtml"),
            &format!("<html><body><h1>Chapter {i}</h1>{paras}</body></html>"),
        );
    }
    zip.finish().unwrap().into_inner()
}

#[wasm_bindgen]
pub async fn mount(font_px: f64) -> Result<(), JsValue> {
    let doc = web_sys::window().unwrap().document().unwrap();
    let host = doc.get_element_by_id("host").unwrap();
    let book = Rc::new(Publication::open_epub(book(3)).map_err(|e| e.to_string())?);
    let settings = Settings {
        font_size_px: font_px,
        ..Settings::default()
    };
    let v = View::mount(&host, book, settings).map_err(|e| e.to_string())?;
    VIEW.with(|cell| *cell.borrow_mut() = Some(v.clone()));
    v.open(0, Start::Beginning)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[wasm_bindgen]
pub async fn next() -> Result<bool, JsValue> {
    view().next().await.map_err(|e| e.to_string().into())
}

#[wasm_bindgen]
pub fn locator() -> String {
    view().locator().to_string()
}

#[wasm_bindgen]
pub async fn go_to(at: String) -> Result<(), JsValue> {
    let at: Locator = at
        .parse()
        .map_err(|e: quire::ParseLocatorError| e.to_string())?;
    view().go_to(at).await.map_err(|e| e.to_string().into())
}

#[wasm_bindgen]
pub fn set_font(px: f64) {
    let v = view();
    let settings = Settings {
        font_size_px: px,
        ..Settings::default()
    };
    v.set_settings(settings);
}

/// `[page shown, page holding offset, pages]` for the current section.
#[wasm_bindgen]
pub fn pages_for(offset: u32) -> Vec<u32> {
    let v = view();
    vec![v.page(), v.page_of(offset), v.pages()]
}

/// Where each text node of the current section starts.
#[wasm_bindgen]
pub fn node_starts() -> Vec<u32> {
    fn walk(el: &quire::El, at: &mut u32, out: &mut Vec<u32>) {
        for c in &el.children {
            match c {
                quire::Content::Text(t) => {
                    out.push(*at);
                    *at += t.encode_utf16().count() as u32;
                }
                quire::Content::Element(e) => walk(e, at, out),
            }
        }
    }
    let v = view();
    let section = v.book().section(v.spine()).unwrap();
    let mut out = Vec::new();
    walk(&section.root, &mut 0, &mut out);
    out
}

/// The first character drawn on the page shown, measured now.
#[wasm_bindgen]
pub fn first_visible() -> u32 {
    view().first_visible()
}

/// The section text from `offset`, `n` UTF-16 units long.
#[wasm_bindgen]
pub fn text_at(offset: u32, n: u32) -> String {
    let v = view();
    let text = v.book().section(v.spine()).unwrap().text();
    let units: Vec<u16> = text
        .encode_utf16()
        .skip(offset as usize)
        .take(n as usize)
        .collect();
    String::from_utf16_lossy(&units)
}
