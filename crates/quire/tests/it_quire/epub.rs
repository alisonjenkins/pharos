use std::io::{Cursor, Write};

use proptest::prelude::*;
use quire::{Content, El, Publication, Target, Value};

const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

fn epub(files: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mimetype", opts).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", opts).unwrap();
    zip.write_all(CONTAINER.as_bytes()).unwrap();
    for (name, body) in files {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn xhtml(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" xml:lang="en">
<head><title>t</title><link rel="stylesheet" href="../css/book.css"/></head>
<body class="b">{body}</body></html>"#
    )
}

const OPF3: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Sense &amp; Sensibility</dc:title>
    <dc:creator>Jane Austen</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="c1" href="text/c1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="text/c2.xhtml" media-type="application/xhtml+xml"/>
    <item id="notes" href="text/notes.xhtml" media-type="application/xhtml+xml"/>
    <item id="css" href="css/book.css" media-type="text/css"/>
    <item id="cover" href="img/cover.jpg" media-type="image/jpeg" properties="cover-image"/>
  </manifest>
  <spine page-progression-direction="ltr">
    <itemref idref="c1"/><itemref idref="c2"/><itemref idref="notes" linear="no"/>
  </spine>
</package>"#;

const NAV: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body>
<nav epub:type="toc"><ol>
  <li><a href="text/c1.xhtml">Chapter
       One</a></li>
  <li><span>Part Two</span><ol><li><a href="text/c2.xhtml#s2">Section 2</a></li></ol></li>
</ol></nav></body></html>"#;

fn book3(c1_body: &str) -> Publication {
    Publication::open_epub(epub(&[
        ("OEBPS/content.opf", OPF3),
        ("OEBPS/nav.xhtml", NAV),
        ("OEBPS/text/c1.xhtml", &xhtml(c1_body)),
        ("OEBPS/text/c2.xhtml", &xhtml(r#"<h2 id="s2">Two</h2>"#)),
        ("OEBPS/text/notes.xhtml", &xhtml("<p>n</p>")),
        (
            "OEBPS/css/book.css",
            "p { background: url(../img/cover.jpg) }",
        ),
        ("OEBPS/img/cover.jpg", "jpeg"),
    ]))
    .unwrap()
}

fn html(p: &Publication, i: usize) -> String {
    p.section(i)
        .unwrap()
        .to_html(&mut |r| Some(format!("blob:{r}")))
}

#[test]
fn an_epub3_opens_with_its_metadata_spine_and_nav() {
    let p = book3("<p>Hello</p>");
    let md = p.metadata();
    assert_eq!(md.title.as_deref(), Some("Sense & Sensibility"));
    assert_eq!(md.creators, vec!["Jane Austen"]);
    assert_eq!(md.cover.as_deref(), Some("OEBPS/img/cover.jpg"));
    assert!(!md.right_to_left);

    let spine: Vec<_> = p
        .spine()
        .iter()
        .map(|s| (s.path.as_str(), s.linear))
        .collect();
    assert_eq!(
        spine,
        vec![
            ("OEBPS/text/c1.xhtml", true),
            ("OEBPS/text/c2.xhtml", true),
            ("OEBPS/text/notes.xhtml", false)
        ]
    );

    let toc = p.toc();
    assert_eq!(toc[0].title, "Chapter One");
    assert_eq!(
        toc[0].target,
        Some(Target {
            spine: 0,
            fragment: None
        })
    );
    assert_eq!(toc[1].title, "Part Two");
    assert_eq!(toc[1].target, None);
    assert_eq!(
        toc[1].children[0].target,
        Some(Target {
            spine: 1,
            fragment: Some("s2".into())
        })
    );
}

#[test]
fn an_epub2_falls_back_to_its_ncx() {
    let opf = r#"<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
      <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Old</dc:title>
        <meta name="cover" content="cv"/></metadata>
      <manifest>
        <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
        <item id="c1" href="c1.html" media-type="application/xhtml+xml"/>
        <item id="cv" href="cover.png" media-type="image/png"/>
      </manifest>
      <spine toc="ncx"><itemref idref="c1"/></spine></package>"#;
    let ncx = r#"<ncx><navMap><navPoint id="p1"><navLabel><text>Start</text></navLabel>
      <content src="c1.html#top"/></navPoint></navMap></ncx>"#;
    let p = Publication::open_epub(epub(&[
        ("OEBPS/content.opf", opf),
        ("OEBPS/toc.ncx", ncx),
        (
            "OEBPS/c1.html",
            "<html><body><p id='top'>x</p></body></html>",
        ),
    ]))
    .unwrap();
    assert_eq!(p.metadata().cover.as_deref(), Some("OEBPS/cover.png"));
    assert_eq!(p.toc()[0].title, "Start");
    assert_eq!(
        p.toc()[0].target,
        Some(Target {
            spine: 0,
            fragment: Some("top".into())
        })
    );
}

#[test]
fn a_book_without_a_package_or_spine_does_not_open() {
    assert!(Publication::open_epub(b"not a zip".to_vec()).is_err());
    let empty = r#"<package><metadata/><manifest/><spine/></package>"#;
    assert!(Publication::open_epub(epub(&[("OEBPS/content.opf", empty)])).is_err());
}

#[test]
fn a_section_keeps_text_styles_and_images() {
    let p = book3(
        r#"<p epub:type="bodymatter" style="color: red">Hi <img src="../img/cover.jpg" alt="c"/></p>"#,
    );
    let s = p.section(0).unwrap();
    assert_eq!(
        html(&p, 0),
        r#"<div data-quire-body="" class="b" lang="en"><p data-epub-type="bodymatter" style="color: red">Hi <img src="blob:OEBPS/img/cover.jpg" alt="c"></p></div>"#
    );
    assert_eq!(s.styles.len(), 1);
    assert_eq!(
        s.styles[0].render(&mut |r| Some(format!("blob:{r}"))),
        r#"p { background: url("blob:OEBPS/img/cover.jpg") }"#
    );
    assert_eq!(s.resources(), vec!["OEBPS/img/cover.jpg"]);
    let r = p.resource("OEBPS/img/cover.jpg").unwrap();
    assert_eq!(r.media_type, "image/jpeg");
}

#[test]
fn links_become_jumps_inert_or_external() {
    let p = book3(
        r##"<p><a href="c2.xhtml#s2">in</a><a href="#here">same</a><a href="../img/cover.jpg">res</a><a href="https://example.org/">out</a><a href="javascript:alert(1)">js</a></p>"##,
    );
    assert_eq!(
        html(&p, 0),
        concat!(
            r#"<div data-quire-body="" class="b" lang="en"><p>"#,
            r#"<a data-quire-link="1#s2">in</a>"#,
            r#"<a data-quire-link="0#here">same</a>"#,
            r#"<a>res</a>"#,
            r#"<a href="https://example.org/" target="_blank" rel="noopener noreferrer">out</a>"#,
            r#"<a>js</a></p></div>"#
        )
    );
}

#[test]
fn nothing_that_runs_or_fetches_survives() {
    let p = book3(concat!(
        r#"<script>alert(1)</script>"#,
        r#"<p onclick="alert(1)" onmouseover="x">a</p>"#,
        r#"<img src="https://evil.example/x.png" onerror="alert(1)"/>"#,
        r#"<iframe src="https://evil.example/"></iframe>"#,
        r#"<form action="https://evil.example/"><input name="x"/></form>"#,
        r#"<object data="x.swf"></object><embed src="x"/>"#,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1" onload="alert(1)">"#,
        r#"<image xlink:href="https://evil.example/i.png"/><use href="https://evil.example/s.svg#a"/>"#,
        r#"<rect fill="url(https://evil.example/f.svg#g)" width="1"/><rect fill="url(#g)"/>"#,
        r#"<foreignObject><p>x</p></foreignObject></svg>"#,
        r#"<style>@import url(https://evil.example/a.css); p{background:url(https://evil.example/b.png)}</style>"#,
        r#"<marquee>b</marquee>"#,
    ));
    let s = p.section(0).unwrap();
    assert_eq!(
        html(&p, 0),
        concat!(
            r#"<div data-quire-body="" class="b" lang="en"><p>a</p><img>"#,
            r#"<svg viewBox="0 0 1 1"><image></image><rect width="1"></rect><rect fill="url(#g)"></rect></svg>"#,
            r#"b</div>"#
        )
    );
    let css: Vec<String> = s.styles.iter().map(|c| c.render(&mut |_| None)).collect();
    assert_eq!(css[1].trim(), "p{background:none}");
}

#[test]
fn escaped_urls_in_svg_attributes_are_dropped() {
    let p = book3(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="u\72l(https://evil.example/f.svg#g)" width="1"/></svg>"#,
    );
    assert!(html(&p, 0).contains(r#"<rect width="1"></rect>"#));
}

#[test]
fn styles_inside_dropped_content_do_not_apply() {
    let p = book3(
        r#"<template><style>body{display:none}</style></template><noscript><style>p{}</style></noscript><style>em{}</style><p>x</p>"#,
    );
    let s = p.section(0).unwrap();
    let css: Vec<String> = s.styles.iter().map(|c| c.render(&mut |_| None)).collect();
    assert_eq!(
        css.len(),
        2,
        "the linked sheet and the body's own <style>: {css:?}"
    );
    assert_eq!(css[1], "em{}");
}

#[test]
fn unclosed_void_elements_keep_their_siblings() {
    let p = book3(r#"<p>a<img src="../img/cover.jpg">b<br>c</p><p>d</p>"#);
    let s = p.section(0).unwrap();
    assert_eq!(s.text(), "abcd");
    assert_eq!(
        html(&p, 0),
        r#"<div data-quire-body="" class="b" lang="en"><p>a<img src="blob:OEBPS/img/cover.jpg">b<br>c</p><p>d</p></div>"#
    );
}

#[test]
fn a_resource_never_comes_back_as_a_document_type() {
    let opf = OPF3.replace(
        r#"<item id="css" href="css/book.css" media-type="text/css"/>"#,
        r#"<item id="css" href="css/book.css" media-type="text/css"/><item id="x" href="img/x.svg" media-type="text/html"/>"#,
    );
    let p = Publication::open_epub(epub(&[
        ("OEBPS/content.opf", &opf),
        ("OEBPS/text/c1.xhtml", &xhtml("<p>x</p>")),
        ("OEBPS/text/c2.xhtml", &xhtml("<p>x</p>")),
        ("OEBPS/text/notes.xhtml", &xhtml("<p>x</p>")),
        ("OEBPS/img/x.svg", "<svg><script>x()</script></svg>"),
        ("OEBPS/text/page.html", "<p>x</p>"),
    ]))
    .unwrap();
    assert_eq!(
        p.resource("OEBPS/img/x.svg").unwrap().media_type,
        "application/octet-stream"
    );
    assert_eq!(
        p.resource("OEBPS/text/page.html").unwrap().media_type,
        "application/octet-stream"
    );
    assert!(matches!(
        p.resource("OEBPS/nope.png"),
        Err(quire::ArchiveError::Missing { .. })
    ));
}

#[test]
fn svg_keeps_its_case_and_namespace() {
    let p = book3(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10" preserveAspectRatio="xMidYMid"><image width="10" height="10" xlink:href="../img/cover.jpg"/></svg>"#,
    );
    let s = p.section(0).unwrap();
    let Content::Element(svg) = &s.root.children[0] else {
        panic!()
    };
    assert_eq!(svg.ns, quire::Namespace::Svg);
    assert!(svg.attrs.iter().any(|a| a.name == "viewBox"));
    assert!(svg.attrs.iter().any(|a| a.name == "preserveAspectRatio"));
    let Content::Element(img) = &svg.children[0] else {
        panic!()
    };
    assert!(img
        .attrs
        .iter()
        .any(|a| a.name == "href" && a.value == Value::Resource("OEBPS/img/cover.jpg".into())));
}

#[test]
fn section_text_is_the_text_in_document_order() {
    let p = book3("<p>One &amp; <em>two</em></p>\n<p>thr😀ee</p>");
    let s = p.section(0).unwrap();
    assert_eq!(s.text(), "One & two\nthr😀ee");
    assert_eq!(s.text_len_utf16(), 17);
}

#[test]
fn progress_weighs_sections_by_size() {
    let p = book3("<p>x</p>");
    assert_eq!(p.progress(0, 0.0), 0.0);
    assert!((p.progress(3, 0.0) - 1.0).abs() < 1e-9);
    let mid = p.progress(1, 0.5);
    assert!(mid > p.progress(1, 0.0) && mid < p.progress(2, 0.0));
    assert_eq!(p.progress(0, f64::NAN), 0.0);
}

fn has_unsafe(el: &El) -> bool {
    el.name.eq_ignore_ascii_case("script") || el.attrs.iter().any(|a| {
        a.name.to_ascii_lowercase().starts_with("on")
            || matches!(&a.value, Value::Text(t) if t.to_ascii_lowercase().contains("javascript:"))
    })
        || el
            .children
            .iter()
            .any(|c| matches!(c, Content::Element(e) if has_unsafe(e)))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    /// V351: whatever mix of tags and attributes a book has, no script element
    /// and no event-handler attribute or javascript: value comes out.
    #[test]
    fn sanitised_output_never_holds_a_script_or_handler(
        parts in prop::collection::vec(
            prop_oneof![
                Just("<script>x()</script>"),
                Just("<p onclick=\"x()\">a</p>"),
                Just("<a href=\"javascript:x()\">j</a>"),
                Just("<a href=\" JaVaScRiPt:x()\" title=\"javascript:y\">j</a>"),
                Just("<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"x()\"><script>x()</script></svg>"),
                Just("<div><span onmouseover=\"x()\">s</span></div>"),
                Just("<SCRIPT>x()</SCRIPT>"),
                Just("<p>plain</p>"),
                Just("<img src=\"a.png\" onerror=\"x()\"/>"),
            ],
            0..12,
        )
    ) {
        let p = book3(&parts.concat());
        let s = p.section(0).unwrap();
        prop_assert!(!has_unsafe_ignoring_title(&s.root));
    }
}

/// `title` is plain text a reader sees, so "javascript:" in one is harmless;
/// everything else must be clean.
fn has_unsafe_ignoring_title(el: &El) -> bool {
    let mut copy = el.clone();
    strip_titles(&mut copy);
    has_unsafe(&copy)
}

fn strip_titles(el: &mut El) {
    el.attrs.retain(|a| a.name != "title");
    for c in &mut el.children {
        if let Content::Element(e) = c {
            strip_titles(e);
        }
    }
}

/// A fixed-layout book in the shape a PDF-to-EPUB converter makes: one
/// spine item per page, each a page-sized box with its artwork as an image
/// and its text placed at exact points in a font shipped in the book.
fn fixed_layout_book(item_props: &str, page_meta: &str) -> Publication {
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" prefix="rendition: http://www.idpf.org/vocab/rendition/#">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Placed Pages</dc:title>
    <meta property="rendition:layout">pre-paginated</meta>
    <meta name="viewport" content="width=500, height=700"/>
  </metadata>
  <manifest>
    <item id="p1" href="p1.xhtml" media-type="application/xhtml+xml"/>
    <item id="p2" href="p2.xhtml" media-type="application/xhtml+xml"/>
    <item id="font" href="fonts/hand.ttf" media-type="application/font-sfnt"/>
  </manifest>
  <spine><itemref idref="p1"/><itemref idref="p2" {item_props}/></spine>
</package>"#
    );
    let page = |meta: &str| {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>p</title>
<style>@font-face{{font-family:hand;src:url('fonts/hand.ttf?reload=1') format('truetype')}}
.t{{position:absolute;white-space:pre}}</style>{meta}</head>
<body><div style="width:945.5px;height:1341px"><img src="bg.jpg" style="position:absolute"/>
<div class="t" style="left:10px;top:20px;font-family:hand">Placed text</div></div></body></html>"#
        )
    };
    Publication::open_epub(epub(&[
        ("OEBPS/content.opf", &opf),
        (
            "OEBPS/p1.xhtml",
            &page(r#"<meta name="viewport" content="width=945.5, height=1341"/>"#),
        ),
        ("OEBPS/p2.xhtml", &page(page_meta)),
        ("OEBPS/fonts/hand.ttf", "ttf"),
        ("OEBPS/bg.jpg", "jpeg"),
    ]))
    .unwrap()
}

#[test]
fn a_pre_paginated_book_gives_each_section_its_page_size() {
    let book = fixed_layout_book("", "");
    assert!(book.metadata().fixed_layout);
    assert!(book.spine().iter().all(|s| s.fixed));
    let p1 = book.section(0).unwrap().page.unwrap();
    assert_eq!((p1.width, p1.height), (945.5, 1341.0));
    let p2 = book.section(1).unwrap().page.unwrap();
    assert_eq!(
        (p2.width, p2.height),
        (500.0, 700.0),
        "a page with no viewport of its own takes the package's"
    );
}

#[test]
fn a_reflowable_item_in_a_fixed_book_flows() {
    let book = fixed_layout_book(r#"properties="rendition:layout-reflowable""#, "");
    assert!(!book.spine()[1].fixed);
    assert_eq!(book.section(1).unwrap().page, None);
}

#[test]
fn a_viewport_in_a_reflowable_book_is_not_a_page() {
    let book = book3(r#"<p>x</p>"#);
    assert!(!book.metadata().fixed_layout);
    assert_eq!(book.section(0).unwrap().page, None);
}

#[test]
fn a_fixed_page_keeps_its_placed_text_and_its_font() {
    let book = fixed_layout_book("", "");
    let s = book.section(0).unwrap();
    let faces: Vec<_> = s.styles.iter().flat_map(|c| c.font_faces()).collect();
    assert_eq!(faces.len(), 1);
    assert_eq!(faces[0].family, "hand");
    assert_eq!(faces[0].sources, ["OEBPS/fonts/hand.ttf"]);
    assert!(s.resources().contains(&"OEBPS/fonts/hand.ttf".to_owned()));
    let h = s.to_html(&mut |p| Some(format!("blob:{p}")));
    assert!(h.contains("left:10px;top:20px;font-family:hand"), "{h}");
    let css = s.styles[0].render(&mut |p| Some(format!("blob:{p}")));
    assert!(!css.contains("font-face"), "{css}");
}
