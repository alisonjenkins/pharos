//! Black bars baked into a file's picture (B1017, V976, V977, V991): a 4:3 show
//! stored pillarboxed in a 1920x1080 frame, say. The server finds where the
//! picture sits in the frame once per file and sends it with each version;
//! the watch page crops the frame to it, so the picture fills the player
//! the way a file without bars would.
//!
//! The crop is laid out like a 3D eye (V921): the `<video>` is sized to the
//! picture's own shape, the whole frame is drawn stretched into it and moved
//! and clipped to the picture, so everything that fits itself to the
//! element sees only the picture. [`parse_crop`] reads the frame's
//! `data-crop` back for the readers of the picture's shape.

use super::stereo::FrameRect;
use pharos_client::api::PictureArea;

/// The part of a `width` x `height` frame the picture covers, as fractions;
/// `None` when there is nothing to crop: no area, an area that is the whole
/// frame or does not fit inside it, or an unknown frame size.
pub fn picture_rect(area: Option<PictureArea>, width: u32, height: u32) -> Option<FrameRect> {
    let a = area?;
    if width == 0 || height == 0 || a.width == 0 || a.height == 0 {
        return None;
    }
    let fits = u64::from(a.x) + u64::from(a.width) <= u64::from(width)
        && u64::from(a.y) + u64::from(a.height) <= u64::from(height);
    let whole = a.width == width && a.height == height;
    if !fits || whole {
        return None;
    }
    let (w, h) = (f64::from(width), f64::from(height));
    Some(FrameRect {
        x: f64::from(a.x) / w,
        y: f64::from(a.y) / h,
        w: f64::from(a.width) / w,
        h: f64::from(a.height) / h,
    })
}

/// A picture to crop to: where it sits in the frame, and its shape from the
/// version's own size, for the first frames before the `<video>` knows its
/// size (the coded size: an anamorphic file is off until then).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub rect: FrameRect,
    pub aspect: f64,
}

impl Crop {
    /// The crop for a version `width` x `height` whose picture is `area`.
    pub fn of(area: Option<PictureArea>, width: u32, height: u32) -> Option<Crop> {
        let rect = picture_rect(area, width, height)?;
        let (w, h) = shown_size(Some(rect), width, height);
        (w > 0 && h > 0).then(|| Crop {
            rect,
            aspect: f64::from(w) / f64::from(h),
        })
    }

    /// The crop while the server burns a subtitle into the picture: the
    /// bars above and below stay, since a burned cue may sit in them (a
    /// disc's subtitle in a letterbox bar); the side bars still go.
    pub fn keeping_rows(self) -> Crop {
        let rect = FrameRect {
            y: 0.0,
            h: 1.0,
            ..self.rect
        };
        Crop {
            rect,
            aspect: self.aspect * self.rect.h,
        }
    }

    /// Whether it crops anything at all.
    pub fn crops(self) -> bool {
        self.rect.w < 1.0 || self.rect.h < 1.0
    }
}

/// The frame's `data-crop` value: the picture's fractions, `x y w h`.
pub fn crop_attr(r: FrameRect) -> String {
    format!("{:.6} {:.6} {:.6} {:.6}", r.x, r.y, r.w, r.h)
}

/// A `data-crop` value read back; `None` for anything malformed or empty.
pub fn parse_crop(attr: &str) -> Option<FrameRect> {
    let mut parts = attr.split_whitespace().map(str::parse::<f64>);
    let mut next = || parts.next()?.ok().filter(|v| v.is_finite());
    let r = FrameRect {
        x: next()?,
        y: next()?,
        w: next()?,
        h: next()?,
    };
    let unit = |v: f64| (0.0..=1.0).contains(&v);
    (unit(r.x) && unit(r.y) && r.w > 0.0 && r.h > 0.0 && unit(r.x + r.w) && unit(r.y + r.h))
        .then_some(r)
}

/// The shape the picture is shown at, for a `width` x `height` decoded
/// frame cropped to `crop` (`None`: the frame itself).
pub fn shown_size(crop: Option<FrameRect>, width: u32, height: u32) -> (u32, u32) {
    match crop {
        Some(r) => (scale(width, r.w), scale(height, r.h)),
        None => (width, height),
    }
}

fn scale(length: u32, by: f64) -> u32 {
    // Lossless: `by` is at most 1, so the product fits back in a u32.
    (f64::from(length) * by).round() as u32
}

/// The `<video>`'s inline style while cropped to `r`: the transform that
/// stretches the picture's part of the frame over the element and the clip
/// that hides the bars. pharos.css applies both (V977).
pub fn crop_style(r: FrameRect) -> String {
    format!(
        "--pharos-crop-transform: scale({:.6}, {:.6}) translate({:.4}%, {:.4}%); \
         --pharos-crop-clip: inset({:.4}% {:.4}% {:.4}% {:.4}%);",
        1.0 / r.w,
        1.0 / r.h,
        // `0 - v`, not `-v`: a negative zero would print as "-0".
        0.0 - r.x * 100.0,
        0.0 - r.y * 100.0,
        r.y * 100.0,
        (1.0 - r.x - r.w) * 100.0,
        (1.0 - r.y - r.h) * 100.0,
        r.x * 100.0,
    )
}

/// Where an image subtitle cue at `cue` (fractions of the whole frame)
/// lands on a picture cropped to `crop`: moved into the picture's
/// coordinates and kept inside it, so a cue the disc placed in a bar is
/// still seen.
pub fn cue_in_picture(cue: FrameRect, crop: Option<FrameRect>) -> FrameRect {
    let Some(c) = crop.filter(|c| c.w > 0.0 && c.h > 0.0) else {
        return cue;
    };
    let w = (cue.w / c.w).min(1.0);
    let h = (cue.h / c.h).min(1.0);
    FrameRect {
        x: ((cue.x - c.x) / c.w).clamp(0.0, 1.0 - w),
        y: ((cue.y - c.y) / c.h).clamp(0.0, 1.0 - h),
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    /// Bewitched: a 4:3 picture pillarboxed in 1920x1080.
    const BEWITCHED: PictureArea = PictureArea {
        x: 242,
        y: 0,
        width: 1436,
        height: 1080,
    };

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn a_pillarboxed_picture_is_the_middle_of_the_frame() {
        let r = picture_rect(Some(BEWITCHED), 1920, 1080).expect("cropped");
        assert!(close(r.x, 242.0 / 1920.0), "{r:?}");
        assert!(close(r.w, 1436.0 / 1920.0), "{r:?}");
        assert!(close(r.y, 0.0) && close(r.h, 1.0), "{r:?}");
        assert_eq!(shown_size(Some(r), 1920, 1080), (1436, 1080));
    }

    #[test]
    fn nothing_is_cropped_without_an_area_inside_a_known_frame() {
        assert_eq!(picture_rect(None, 1920, 1080), None);
        assert_eq!(picture_rect(Some(BEWITCHED), 0, 0), None, "size unknown");
        let whole = PictureArea {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(picture_rect(Some(whole), 1920, 1080), None, "no bars");
        // An area from a probe of another size must not crop this frame.
        assert_eq!(picture_rect(Some(BEWITCHED), 1280, 720), None);
        let empty = PictureArea {
            width: 0,
            ..BEWITCHED
        };
        assert_eq!(picture_rect(Some(empty), 1920, 1080), None);
        assert_eq!(shown_size(None, 1920, 1080), (1920, 1080));
    }

    #[test]
    fn a_burned_subtitle_keeps_the_bars_above_and_below() {
        // Windowboxed: bars on all four sides.
        let area = PictureArea {
            x: 240,
            y: 60,
            width: 1440,
            height: 960,
        };
        let crop = Crop::of(Some(area), 1920, 1080).expect("cropped");
        assert!(close(crop.aspect, 1.5), "{crop:?}");
        let kept = crop.keeping_rows();
        assert!(
            close(kept.rect.x, 0.125) && close(kept.rect.w, 0.75),
            "{kept:?}"
        );
        assert!(
            close(kept.rect.y, 0.0) && close(kept.rect.h, 1.0),
            "{kept:?}"
        );
        assert!(close(kept.aspect, 1440.0 / 1080.0), "{kept:?}");
        assert!(kept.crops());
        // A letterbox alone has nothing left to crop once the rows stay.
        let letterbox = PictureArea {
            x: 0,
            y: 140,
            width: 1920,
            height: 800,
        };
        let crop = Crop::of(Some(letterbox), 1920, 1080).expect("cropped");
        assert!(crop.crops());
        assert!(!crop.keeping_rows().crops());
    }

    #[test]
    fn the_crop_attribute_reads_back() {
        let r = picture_rect(Some(BEWITCHED), 1920, 1080).expect("cropped");
        let back = parse_crop(&crop_attr(r)).expect("parses");
        assert!(close(back.x, r.x) && close(back.w, r.w), "{back:?}");
        for bad in [
            "",
            "0.1 0 0.8",
            "a b c d",
            "0 0 0 1",
            "0.5 0 0.8 1",
            "NaN 0 1 1",
        ] {
            assert_eq!(parse_crop(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_style_stretches_the_picture_over_the_element_and_clips_the_bars() {
        let r = FrameRect {
            x: 0.125,
            y: 0.0,
            w: 0.75,
            h: 1.0,
        };
        let style = crop_style(r);
        assert!(
            style.contains("scale(1.333333, 1.000000) translate(-12.5000%, 0.0000%)"),
            "{style}"
        );
        assert!(
            style.contains("inset(0.0000% 12.5000% 0.0000% 12.5000%)"),
            "{style}"
        );
    }

    #[test]
    fn an_image_cue_in_a_bar_is_moved_onto_the_picture() {
        // A letterboxed film: the picture is the middle 3/4 of the height.
        let crop = Some(FrameRect {
            x: 0.0,
            y: 0.125,
            w: 1.0,
            h: 0.75,
        });
        // A cue the disc put in the bottom bar.
        let cue = FrameRect {
            x: 0.25,
            y: 0.9,
            w: 0.5,
            h: 0.05,
        };
        let r = cue_in_picture(cue, crop);
        assert!(close(r.x, 0.25) && close(r.w, 0.5), "{r:?}");
        assert!(close(r.h, 0.05 / 0.75), "{r:?}");
        assert!(close(r.y + r.h, 1.0), "kept on the picture's bottom: {r:?}");
        // On the picture, a cue only moves into its coordinates.
        let mid = FrameRect {
            x: 0.25,
            y: 0.5,
            w: 0.5,
            h: 0.075,
        };
        let r = cue_in_picture(mid, crop);
        assert!(close(r.y, 0.5), "{r:?}");
        assert_eq!(cue_in_picture(mid, None), mid);
    }
}
