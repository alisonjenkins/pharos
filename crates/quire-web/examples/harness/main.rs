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

/// A book whose first section links into the others: a deep id, an old
/// `<a name>` anchor, a footnote in a later section and a same-page note.
fn linked_book() -> Vec<u8> {
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
    put(
        "c.opf",
        r#"<package><metadata/><manifest><item id="toc" href="toc.xhtml" media-type="application/xhtml+xml"/><item id="c" href="text/c%201.xhtml" media-type="application/xhtml+xml"/><item id="n" href="notes.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="toc"/><itemref idref="c"/><itemref idref="n" linear="no"/></spine></package>"#,
    );
    put(
        "toc.xhtml",
        r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>Contents</h1>
<p><a id="to-deep" href="text/c%201.xhtml#deep">Deep section</a></p>
<p><a id="to-old" href="text/c%201.xhtml#old">Old anchor</a></p>
<p><a id="to-start" href="text/c%201.xhtml">Chapter start</a></p>
<p><a id="to-web" href="https://example.org/">Web page</a></p>
</body></html>"#,
    );
    let paras: String = (0..14)
        .map(|p| {
            let words: String = (0..160).map(|w| format!("c.{p}.{w} ")).collect();
            let mark = match p {
                8 => r#" id="deep""#.to_owned(),
                _ => String::new(),
            };
            let old = if p == 11 { r#"<a name="old"></a>"# } else { "" };
            let note = if p == 12 {
                r#"<a id="ref" href="../notes.xhtml#n1">1</a>"#
            } else {
                ""
            };
            format!(r##"<p{mark}>{old}Para {p} <a href="#deep">back</a> {note} {words}</p>"##)
        })
        .collect();
    put(
        "text/c 1.xhtml",
        &format!(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>Chapter</h1>{paras}</body></html>"#
        ),
    );
    put(
        "notes.xhtml",
        r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>Notes</h1><p id="n1">Note one.</p></body></html>"#,
    );
    zip.finish().unwrap().into_inner()
}

thread_local! {
    static JUMPS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Mounts [`linked_book`] at its contents page and follows every link the
/// view reports, as the reader does.
#[wasm_bindgen]
pub async fn mount_links(font_px: f64) -> Result<(), JsValue> {
    let doc = web_sys::window().unwrap().document().unwrap();
    let host = doc.get_element_by_id("host").unwrap();
    let book = Rc::new(Publication::open_epub(linked_book()).map_err(|e| e.to_string())?);
    let settings = Settings {
        font_size_px: font_px,
        ..Settings::default()
    };
    let v = View::mount(&host, book, settings).map_err(|e| e.to_string())?;
    VIEW.with(|cell| *cell.borrow_mut() = Some(v.clone()));
    let weak = v.downgrade();
    v.on_action(move |a| {
        if let quire_web::Action::Jump(t) = a {
            let Some(v) = weak.upgrade() else { return };
            wasm_bindgen_futures::spawn_local(async move {
                let landed = v.go(&t).await.map_err(|e| e.to_string());
                JUMPS.with(|j| j.borrow_mut().push(format!("{} {landed:?}", t.encode())));
            });
        }
    });
    v.open(0, Start::Beginning)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Every link followed so far, `"<target> <outcome>"`.
#[wasm_bindgen]
pub fn jumps() -> Vec<String> {
    JUMPS.with(|j| j.borrow().clone())
}

#[wasm_bindgen]
pub fn focus_link(forward: bool) -> bool {
    view().focus_link(forward)
}

#[wasm_bindgen]
pub fn focused_link() -> Option<String> {
    view().focused_link().map(|t| t.encode())
}

#[wasm_bindgen]
pub fn spine() -> u32 {
    view().spine() as u32
}

#[wasm_bindgen]
pub fn page() -> u32 {
    view().page()
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

/// A two-page fixed-layout book, in the shape a PDF-to-EPUB converter makes:
/// each page a 500x700 box holding a block of colour where the artwork
/// would be and text placed at exact points in a font shipped in the book.
fn fixed_book() -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default();
    let mut put = |name: &str, body: &[u8]| {
        zip.start_file(name, o).unwrap();
        zip.write_all(body).unwrap();
    };
    put(
        "META-INF/container.xml",
        br#"<container><rootfiles><rootfile full-path="c.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
    );
    put(
        "c.opf",
        br#"<package version="3.0"><metadata><meta property="rendition:layout">pre-paginated</meta></metadata><manifest><item id="p0" href="p0.xhtml" media-type="application/xhtml+xml"/><item id="p1" href="p1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="p0"/><itemref idref="p1"/></spine></package>"#,
    );
    for i in 0..2 {
        put(
            &format!("p{i}.xhtml"),
            format!(
                r#"<html><head><meta name="viewport" content="width=500, height=700"/>
<style>@font-face{{font-family:placed-hand;src:url('hand.ttf?reload=1') format('truetype')}}
.t{{position:absolute;white-space:pre;font-family:placed-hand,serif}}</style></head>
<body><div id="page" style="width:500px;height:700px;background-color:#fff">
<div id="art" style="position:absolute;left:0;top:0;width:500px;height:350px;background-color:rgb(250,220,200)"></div>
<div id="placed" class="t" style="left:40px;top:400px;font-size:30px;color:rgb(10,20,30)">Page {i} placed text</div>
</div></body></html>"#
            )
            .as_bytes(),
        );
    }
    // Not a real font: the check is that the face reaches the document.
    put("hand.ttf", b"not a font");
    zip.finish().unwrap().into_inner()
}

/// Mounts `bytes` (any EPUB; the fixed-layout test book when empty) and
/// opens reading-order item `spine`.
#[wasm_bindgen]
pub async fn mount_book(bytes: Vec<u8>, spine: usize) -> Result<(), JsValue> {
    let doc = web_sys::window().unwrap().document().unwrap();
    let host = doc.get_element_by_id("host").unwrap();
    let bytes = if bytes.is_empty() {
        fixed_book()
    } else {
        bytes
    };
    let book = Rc::new(Publication::open_epub(bytes).map_err(|e| e.to_string())?);
    let v = View::mount(&host, book, Settings::default()).map_err(|e| e.to_string())?;
    VIEW.with(|cell| *cell.borrow_mut() = Some(v.clone()));
    v.open(spine, Start::Beginning)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// `[page shown, pages, reading-order item]`.
#[wasm_bindgen]
pub fn where_now() -> Vec<u32> {
    let v = view();
    vec![v.page(), v.pages(), v.spine() as u32]
}
