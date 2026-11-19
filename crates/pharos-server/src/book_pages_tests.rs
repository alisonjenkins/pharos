#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use std::collections::HashSet;
use std::io::Read;

/// A PDF of `pages`, each `(width, height, colour)`: a black square, or a red
/// one on a colour page.
fn pdf(pages: &[(u32, u32, bool)]) -> Vec<u8> {
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
    for (i, (w, h, colour)) in pages.iter().enumerate() {
        object(
            &mut out,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents {} 0 R >>",
                4 + 2 * i
            ),
        );
        let ink = if *colour { "1 0 0" } else { "0 0 0" };
        let content = format!("{ink} rg 20 20 {} {} re f", w / 2, h / 2);
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

fn request(width: u32, height: u32, grey: bool) -> PagesRequest {
    PagesRequest {
        bx: SlimBox { width, height },
        grey,
    }
}

/// Draws `book` and returns each member's name, compression and bytes.
fn drawn(
    book: &[u8],
    request: PagesRequest,
) -> (DrawStats, Vec<(String, CompressionMethod, Vec<u8>)>) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("book.pdf");
    let out = dir.path().join("book.cbz");
    std::fs::write(&src, book).unwrap();
    let stats = draw(&src, &out, request).unwrap();
    let mut archive = zip::ZipArchive::new(File::open(&out).unwrap()).unwrap();
    let members = (0..archive.len())
        .map(|i| {
            let mut file = archive.by_index(i).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            (file.name().unwrap().into_owned(), file.compression(), bytes)
        })
        .collect();
    (stats, members)
}

#[test]
fn only_pages_pictures_asks_and_the_box_defaults_to_the_kobo_panel() {
    assert_eq!(PagesRequest::from_query("maxWidth=600"), None);
    assert_eq!(PagesRequest::from_query("pages=text"), None);
    assert_eq!(
        PagesRequest::from_query("Pages=Pictures"),
        Some(PagesRequest {
            bx: DEFAULT_BOX,
            grey: false
        })
    );
    assert_eq!(
        PagesRequest::from_query("pages=pictures&maxWidth=600&maxHeight=800&grey=true"),
        Some(request(600, 800, true))
    );
}

#[test]
fn every_page_becomes_a_stored_picture_in_order_fitted_to_the_box() {
    let book = pdf(&[(612, 792, false), (612, 792, true), (792, 612, false)]);
    let (stats, members) = drawn(&book, request(600, 800, false));
    let names: Vec<_> = members.iter().map(|(n, ..)| n.as_str()).collect();
    assert_eq!(names, ["page-0001.png", "page-0002.jpg", "page-0003.png"]);
    assert!(members
        .iter()
        .all(|(_, c, _)| *c == CompressionMethod::Stored));
    assert_eq!(
        stats,
        DrawStats {
            pages: 3,
            colour: 1,
            grey: 2,
            failed: 0
        }
    );
    for (name, _, bytes) in &members {
        let picture = image::load_from_memory(bytes).unwrap();
        assert!(picture.width() <= 600 && picture.height() <= 800, "{name}");
        assert!(
            picture.width() >= 599 || picture.height() >= 799,
            "{name} fills the box: {}x{}",
            picture.width(),
            picture.height()
        );
    }
    let colour = image::load_from_memory(&members[1].2).unwrap().to_rgb8();
    let red = colour
        .pixels()
        .filter(|p| p.0[0] > 200 && p.0[1] < 60 && p.0[2] < 60)
        .count();
    assert!(red > 0, "the colour page keeps its colour");
}

#[test]
fn a_grey_page_is_stored_at_the_panels_sixteen_greys() {
    let book = pdf(&[(612, 792, false)]);
    let (_, members) = drawn(&book, request(300, 400, false));
    let decoder = png::Decoder::new(std::io::Cursor::new(members[0].2.clone()));
    let reader = decoder.read_info().unwrap();
    assert_eq!(reader.info().color_type, png::ColorType::Grayscale);
    assert_eq!(reader.info().bit_depth, png::BitDepth::Four);
    let picture = image::load_from_memory(&members[0].2).unwrap().to_luma8();
    // The square's ink is black and the paper white.
    assert_eq!(
        picture
            .get_pixel(picture.width() / 4, picture.height() * 3 / 4)
            .0,
        [0]
    );
    assert_eq!(picture.get_pixel(picture.width() - 2, 2).0, [255]);
}

#[test]
fn asking_for_grey_draws_a_colour_page_in_grey() {
    let book = pdf(&[(612, 792, true)]);
    let (stats, members) = drawn(&book, request(300, 400, true));
    assert_eq!(members[0].0, "page-0001.png");
    assert_eq!(stats.colour, 0);
}

#[test]
fn bytes_that_are_not_a_pdf_are_an_error_and_leave_no_copy() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("book.pdf");
    let out = dir.path().join("book.cbz");
    std::fs::write(&src, b"not a pdf at all").unwrap();
    let e = draw(&src, &out, DEFAULT_REQUEST).unwrap_err();
    assert!(matches!(e, PagesError::Open(_)), "{e}");
    assert!(!out.exists());
}

const DEFAULT_REQUEST: PagesRequest = PagesRequest {
    bx: DEFAULT_BOX,
    grey: false,
};

#[test]
fn a_page_is_fitted_inside_the_box_and_the_edge_cap() {
    let bx = SlimBox {
        width: 1264,
        height: 1680,
    };
    assert_eq!(fitted(612.0, 792.0, bx), (1264, 1636));
    assert_eq!(fitted(792.0, 612.0, bx), (1264, 977));
    let huge = SlimBox {
        width: 16_000,
        height: 16_000,
    };
    let (w, h) = fitted(612.0, 792.0, huge);
    assert!(w <= MAX_PAGE_EDGE && h == MAX_PAGE_EDGE);
    assert_eq!(fitted(0.0, 0.0, bx), (1264, 1264));
}

#[test]
fn faint_fringes_do_not_make_a_page_colour() {
    let mut rgba = vec![255u8; 1000 * 4 * 4];
    rgba[0..4].copy_from_slice(&[255, 0, 0, 255]);
    assert!(!has_colour(&rgba));
    for p in rgba.chunks_exact_mut(4).take(10) {
        p.copy_from_slice(&[255, 0, 0, 255]);
    }
    assert!(has_colour(&rgba));
}

#[tokio::test]
async fn a_second_request_is_served_the_copy_drawn_by_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("book.pdf");
    std::fs::write(&src, pdf(&[(612, 792, false), (612, 792, false)])).unwrap();
    let cache = ConvertedBookCache::new(dir.path().join("cache"));
    let first = get_or_draw(Some(&cache), 7, None, &src, DEFAULT_REQUEST)
        .await
        .unwrap();
    assert!(first.starts_with(cache.pages_dir()));
    assert_eq!(first.extension().unwrap(), "cbz");
    let modified = std::fs::metadata(&first).unwrap().modified().unwrap();
    let second = get_or_draw(Some(&cache), 7, None, &src, DEFAULT_REQUEST)
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(
        std::fs::metadata(&second).unwrap().modified().unwrap(),
        modified
    );
    let grey = get_or_draw(Some(&cache), 7, None, &src, request(1264, 1680, true))
        .await
        .unwrap();
    assert_ne!(first, grey, "another colour choice is another copy");
}

#[tokio::test]
async fn with_no_cache_the_request_is_refused_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("book.pdf");
    std::fs::write(&src, pdf(&[(612, 792, false)])).unwrap();
    let e = get_or_draw(None, 7, None, &src, DEFAULT_REQUEST)
        .await
        .unwrap_err();
    assert!(matches!(e, PagesError::NoCache));
    assert_eq!(e.outcome(), PagesOutcome::NoCache);
}

#[test]
fn every_outcome_has_its_own_label() {
    let labels: HashSet<_> = PagesOutcome::ALL.iter().map(|o| o.label()).collect();
    assert_eq!(labels.len(), PagesOutcome::ALL.len());
}

/// Disarm check: make `record_pages` a no-op and the counter is absent.
#[test]
fn a_decision_is_counted_under_its_label() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    metrics::with_local_recorder(&recorder, || {
        record_pages(PagesOutcome::Drawn);
        record_pages(PagesOutcome::Failed);
        record_pages(PagesOutcome::Failed);
    });
    let count = |outcome: &str| -> u64 {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| {
                k.key().name() == "pharos_book_pages_total"
                    && k.key()
                        .labels()
                        .any(|l| l.key() == "outcome" && l.value() == outcome)
            })
            .map(|(.., v)| match v {
                DebugValue::Counter(n) => n,
                _ => 0,
            })
            .sum()
    };
    assert_eq!(count("drawn"), 1);
    assert_eq!(count("failed"), 2);
    assert_eq!(count("cache_hit"), 0);
}

#[test]
fn the_drawing_is_published_in_the_blocking_task_only_if_the_source_held_still() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("book.pdf");
    let (tmp, copy) = (dir.path().join("book.tmp"), dir.path().join("book.cbz"));
    std::fs::write(&src, pdf(&[(612, 792, false)])).unwrap();
    let e = draw_and_publish(&src, &tmp, &copy, DEFAULT_REQUEST, |_| false).unwrap_err();
    assert!(matches!(e, PagesError::Changed), "{e}");
    assert!(!copy.exists());
    let stats = draw_and_publish(&src, &tmp, &copy, DEFAULT_REQUEST, |_| true).unwrap();
    assert_eq!(stats.pages, 1);
    assert!(copy.exists() && !tmp.exists());
}
