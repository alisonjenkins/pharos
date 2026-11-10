#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use quire::{FixedLayout, FixedLayoutError};

use super::*;

/// A PDF built here: one page per `(width, height)`, each with a black
/// square from (20, 20) to (80, 80) in page units.
fn pdf(pages: &[(u32, u32)]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    let mut object = |out: &mut Vec<u8>, body: String| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", offsets.len()).as_bytes());
    };
    object(&mut out, "<< /Type /Catalog /Pages 2 0 R >>".to_owned());
    let kids: String = (0..pages.len())
        .map(|i| format!("{} 0 R", 3 + 2 * i))
        .collect::<Vec<_>>()
        .join(" ");
    object(
        &mut out,
        format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()),
    );
    for (i, (w, h)) in pages.iter().enumerate() {
        object(
            &mut out,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents {} 0 R >>",
                4 + 2 * i
            ),
        );
        let content = "0 0 0 rg 20 20 60 60 re f";
        object(
            &mut out,
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        );
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for at in &offsets {
        out.extend_from_slice(format!("{at:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn pixel(r: &quire::Raster, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * r.width + x) * 4) as usize;
    r.rgba[at..at + 4].try_into().unwrap()
}

#[test]
fn a_built_pdf_reports_its_pages() {
    let book = PdfBook::open(pdf(&[(100, 100), (200, 100), (100, 100)])).unwrap();
    assert_eq!(book.page_count(), 3);
    let size = book.page_size(1).unwrap();
    assert_eq!((size.width, size.height), (200.0, 100.0));
    assert_eq!(
        book.page_size(3),
        Err(FixedLayoutError::OutOfRange { index: 3, len: 3 })
    );
}

#[test]
fn a_page_is_drawn_at_the_height_asked_with_its_content() {
    let book = PdfBook::open(pdf(&[(100, 100), (200, 100)])).unwrap();
    let square = book.render(0, 200).unwrap();
    assert_eq!((square.width, square.height), (200, 200));
    assert_eq!(square.rgba.len(), 200 * 200 * 4);
    assert_eq!(
        pixel(&square, 100, 100),
        [0, 0, 0, 255],
        "inside the square"
    );
    assert_eq!(
        pixel(&square, 5, 5),
        [255, 255, 255, 255],
        "paper outside it"
    );
    let wide = book.render(1, 100).unwrap();
    assert_eq!(
        (wide.width, wide.height),
        (200, 100),
        "width follows the page"
    );
}

#[test]
fn a_page_past_the_end_is_an_error_not_a_panic() {
    let book = PdfBook::open(pdf(&[(100, 100)])).unwrap();
    assert_eq!(
        book.render(1, 50),
        Err(FixedLayoutError::OutOfRange { index: 1, len: 1 })
    );
}

#[test]
fn a_zero_height_and_an_enormous_one_stay_in_bounds() {
    let book = PdfBook::open(pdf(&[(100, 100)])).unwrap();
    let tiny = book.render(0, 0).unwrap();
    assert!(tiny.height >= 1 && tiny.width >= 1);
    let scale = scale_for(100.0, 100.0, u32::MAX);
    assert!(100.0 * scale <= MAX_SIDE_PX as f32 + 1.0, "{scale}");
    let narrow = scale_for(1.0, 1000.0, 16_000);
    assert!(narrow > 0.0 && narrow.is_finite());
    assert!(
        scale_for(0.0, 0.0, 100) > 0.0,
        "a degenerate page still scales"
    );
}

#[test]
fn bytes_that_are_not_a_pdf_are_refused() {
    for bytes in [
        Vec::new(),
        b"not a pdf at all".to_vec(),
        b"%PDF-1.4\n".to_vec(),
    ] {
        assert!(PdfBook::open(bytes).is_err());
    }
}
