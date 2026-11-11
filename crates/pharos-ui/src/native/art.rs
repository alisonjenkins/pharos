//! Artwork images sized for the screen, in AVIF where the browser takes it.

use dioxus::prelude::*;

/// The widths the server makes Primary art at; it caps Primary at 480 and
/// rounds a requested width up to one of its encode widths (ADR-0063).
const WIDTHS: [u32; 3] = [240, 360, 480];

/// The Android TV app (the only Android build) keeps JPEG until AVIF's decode
/// cost on its SoC is measured: AVIF decodes about 4x slower than JPEG, which a
/// desktop absorbs but a TV-class CPU may not.
const OFFER_AVIF: bool = !cfg!(target_os = "android");

/// The width a grid poster is shown at. Grid columns are `minmax(9rem, 1fr)`,
/// so with three or more columns a tile stays under 13rem; a phone shows two
/// columns of about half the screen. Erring wide keeps HiDPI tiles sharp.
///
/// The TV lays out at 960x540 CSS px with 5.5rem posters (pharos.css), about
/// 6rem with the focus scale; at its 2x pixel ratio that is the 240 px rung.
/// 13rem there fetched 480 px posters, 2.5x the bytes and 4x the decoded
/// pixels, a hundred at a time as a grid page lands.
pub const POSTER_SIZES: &str = if cfg!(target_os = "android") {
    "6rem"
} else {
    "(max-width: 40rem) 50vw, 13rem"
};

/// `src` asked for at `width` px, as AVIF when `avif`.
fn sized(src: &str, width: u32, avif: bool) -> String {
    let sep = if src.contains('?') { '&' } else { '?' };
    let format = if avif { "&format=avif" } else { "" };
    format!("{src}{sep}fillWidth={width}{format}")
}

/// A `srcset` offering `src` at each of [`WIDTHS`].
fn srcset(src: &str, avif: bool) -> String {
    WIDTHS
        .iter()
        .map(|&w| format!("{} {w}w", sized(src, w, avif)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// An artwork `<img>` the browser picks a size and format for. `sizes` is
/// the CSS width the image is shown at, as in the `sizes` attribute.
#[component]
pub fn Art(
    src: String,
    sizes: String,
    #[props(default)] class: Option<String>,
    #[props(default = true)] lazy: bool,
) -> Element {
    let src = super::platform::art_src(&src);
    let fallback = sized(&src, WIDTHS[WIDTHS.len() - 1], false);
    let jpeg = srcset(&src, false);
    let avif = OFFER_AVIF.then(|| srcset(&src, true));
    let loading = if lazy { "lazy" } else { "eager" };
    rsx! {
        picture { class: "pharos-art",
            if let Some(avif) = avif {
                // dioxus has no typed srcset/sizes on <source>.
                source { r#type: "image/avif", "srcset": "{avif}", "sizes": "{sizes}" }
            }
            img {
                class,
                src: "{fallback}",
                srcset: "{jpeg}",
                sizes: "{sizes}",
                alt: "",
                loading,
                decoding: "async",
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_is_added_to_the_tagged_url() {
        assert_eq!(
            sized("/Items/1/Images/Primary?tag=ab", 240, true),
            "/Items/1/Images/Primary?tag=ab&fillWidth=240&format=avif"
        );
        assert_eq!(sized("/a.jpg", 360, false), "/a.jpg?fillWidth=360");
    }

    #[test]
    fn the_srcset_offers_every_width() {
        assert_eq!(
            srcset("/p?tag=t", false),
            "/p?tag=t&fillWidth=240 240w, /p?tag=t&fillWidth=360 360w, /p?tag=t&fillWidth=480 480w"
        );
    }
}
