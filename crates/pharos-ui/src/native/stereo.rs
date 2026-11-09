//! 3D titles (V722): how a packing is named, and how a packed frame is shown
//! (spec 022 US2). [`eye_size`] and [`eye_rect`] are the one place that knows
//! which part of a packed frame is an eye and what shape that eye is (V921);
//! every reader of the picture's geometry goes through them.

use pharos_client::api::Stereo;

/// The device setting holding the [`ViewMode`] (V922: it never leaves the
/// device).
pub const VIEW_MODE_KEY: &str = "pharos.player.stereo";

/// How a packing reads in a version's label.
pub fn packing_label(stereo: Stereo) -> &'static str {
    match stereo {
        Stereo::HalfSideBySide => "half side-by-side",
        Stereo::FullSideBySide => "full side-by-side",
        Stereo::HalfTopAndBottom => "half top-and-bottom",
        Stereo::FullTopAndBottom => "full top-and-bottom",
        Stereo::Mvc => "MVC, plays in 2D here",
    }
}

/// How a flat screen shows a packed 3D version.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// The left (or top) eye alone, at the eye's own shape.
    #[default]
    OneEye,
    /// The whole packed frame, for a 3D TV that unpacks it itself.
    BothEyes,
}

impl ViewMode {
    pub const ALL: [ViewMode; 2] = [ViewMode::OneEye, ViewMode::BothEyes];

    /// The mode a stored [`VIEW_MODE_KEY`] value holds; anything unknown is
    /// [`ViewMode::OneEye`].
    pub fn parse(stored: &str) -> ViewMode {
        match stored.trim() {
            "both-eyes" => ViewMode::BothEyes,
            _ => ViewMode::OneEye,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ViewMode::OneEye => "one-eye",
            ViewMode::BothEyes => "both-eyes",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ViewMode::OneEye => "3D: one eye",
            ViewMode::BothEyes => "3D: both eyes",
        }
    }
}

/// Which way a packing splits the frame into eyes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Across,
    Down,
}

/// How `stereo` splits: its axis, and whether each eye kept all its pixels
/// (a full packing). MVC's second view is never decoded, so it has none.
fn split(stereo: Stereo) -> Option<(Axis, bool)> {
    match stereo {
        Stereo::HalfSideBySide => Some((Axis::Across, false)),
        Stereo::FullSideBySide => Some((Axis::Across, true)),
        Stereo::HalfTopAndBottom => Some((Axis::Down, false)),
        Stereo::FullTopAndBottom => Some((Axis::Down, true)),
        Stereo::Mvc => None,
    }
}

/// The packing the watch page crops to one eye, if any: one that splits,
/// shown in [`ViewMode::OneEye`].
pub fn cropped(stereo: Option<Stereo>, mode: ViewMode) -> Option<Stereo> {
    stereo.filter(|s| mode == ViewMode::OneEye && split(*s).is_some())
}

/// The `data-packing` value the watch page's frame carries while cropped:
/// pharos.css lays the crop out from it, and [`parse_packing`] reads it back.
pub fn packing_attr(stereo: Stereo) -> &'static str {
    match stereo {
        Stereo::HalfSideBySide => "hsbs",
        Stereo::FullSideBySide => "fsbs",
        Stereo::HalfTopAndBottom => "htab",
        Stereo::FullTopAndBottom => "ftab",
        Stereo::Mvc => "mvc",
    }
}

pub fn parse_packing(attr: &str) -> Option<Stereo> {
    match attr {
        "hsbs" => Some(Stereo::HalfSideBySide),
        "fsbs" => Some(Stereo::FullSideBySide),
        "htab" => Some(Stereo::HalfTopAndBottom),
        "ftab" => Some(Stereo::FullTopAndBottom),
        "mvc" => Some(Stereo::Mvc),
        _ => None,
    }
}

/// The shape one eye is shown at, for a `width` x `height` decoded frame
/// packed as `stereo` (`None`: not cropped, the frame itself). A half packing
/// squeezed each eye into half the frame, so the eye is shown at the frame's
/// shape; a full packing kept each eye's pixels, so the eye is half of it.
pub fn eye_size(stereo: Option<Stereo>, width: u32, height: u32) -> (u32, u32) {
    match stereo.and_then(split) {
        Some((Axis::Across, true)) => (width / 2, height),
        Some((Axis::Down, true)) => (width, height / 2),
        _ => (width, height),
    }
}

/// Which eye of a packed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eye {
    Left,
    Right,
}

/// A part of the frame, in fractions of its width and height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Where `eye` sits in a frame packed as `stereo`: the left or top half is
/// the left eye. A frame that does not split is the whole frame for both.
pub fn eye_rect(stereo: Option<Stereo>, eye: Eye) -> FrameRect {
    let second = if eye == Eye::Right { 0.5 } else { 0.0 };
    match stereo.and_then(split) {
        Some((Axis::Across, _)) => FrameRect {
            x: second,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        },
        Some((Axis::Down, _)) => FrameRect {
            x: 0.0,
            y: second,
            w: 1.0,
            h: 0.5,
        },
        None => FrameRect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        },
    }
}

/// The WebXR layer layout that shows each eye its own half of a frame
/// packed as `stereo` (V923: only these get "Watch in VR").
pub fn xr_layout(stereo: Option<Stereo>) -> Option<&'static str> {
    match stereo.and_then(split) {
        Some((Axis::Across, _)) => Some("stereo-left-right"),
        Some((Axis::Down, _)) => Some("stereo-top-bottom"),
        None => None,
    }
}

/// How a VR session shows the video (ADR 0088).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XrPath {
    /// The compositor draws the `<video>` as a media quad layer.
    Layers,
    /// The page draws one quad per eye through WebGL2.
    Webgl,
}

impl XrPath {
    /// The media layer when the session granted `layers` and the browser has
    /// `XRMediaBinding`; the page's own drawing otherwise.
    pub fn choose(layers_granted: bool, media_binding: bool) -> XrPath {
        if layers_granted && media_binding {
            XrPath::Layers
        } else {
            XrPath::Webgl
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            XrPath::Layers => "layers",
            XrPath::Webgl => "webgl",
        }
    }
}

/// How far in front of the viewer the virtual screen floats, in metres.
pub const VR_SCREEN_DISTANCE_M: f64 = 3.0;
/// The virtual screen's width in metres: about a 60 degree view at
/// [`VR_SCREEN_DISTANCE_M`], a seat a few rows back in a cinema.
pub const VR_SCREEN_WIDTH_M: f64 = 3.5;

/// The virtual screen's height in metres for one eye's `width` x `height`
/// picture (from [`eye_size`]); 16:9 until the picture's size is known.
pub fn vr_screen_height(eye_width: u32, eye_height: u32) -> f64 {
    if eye_width == 0 || eye_height == 0 {
        return VR_SCREEN_WIDTH_M * 9.0 / 16.0;
    }
    VR_SCREEN_WIDTH_M * f64::from(eye_height) / f64::from(eye_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Stereo; 5] = [
        Stereo::HalfSideBySide,
        Stereo::FullSideBySide,
        Stereo::HalfTopAndBottom,
        Stereo::FullTopAndBottom,
        Stereo::Mvc,
    ];

    fn parts(r: FrameRect) -> (f64, f64, f64, f64) {
        (r.x, r.y, r.w, r.h)
    }

    #[test]
    fn each_packing_shows_one_eye_at_its_own_shape() {
        let cases = [
            (Stereo::HalfSideBySide, (1920, 1080), (1920, 1080)),
            (Stereo::FullSideBySide, (3840, 1080), (1920, 1080)),
            (Stereo::HalfTopAndBottom, (1920, 1080), (1920, 1080)),
            (Stereo::FullTopAndBottom, (1920, 2160), (1920, 1080)),
            (Stereo::Mvc, (1920, 1080), (1920, 1080)),
        ];
        for (s, (w, h), eye) in cases {
            assert_eq!(eye_size(Some(s), w, h), eye, "{s:?}");
        }
        assert_eq!(eye_size(None, 3840, 1080), (3840, 1080));
    }

    #[test]
    fn the_left_eye_is_the_first_half_and_the_right_the_second() {
        for s in [Stereo::HalfSideBySide, Stereo::FullSideBySide] {
            assert_eq!(
                parts(eye_rect(Some(s), Eye::Left)),
                (0.0, 0.0, 0.5, 1.0),
                "{s:?}"
            );
            assert_eq!(
                parts(eye_rect(Some(s), Eye::Right)),
                (0.5, 0.0, 0.5, 1.0),
                "{s:?}"
            );
        }
        for s in [Stereo::HalfTopAndBottom, Stereo::FullTopAndBottom] {
            assert_eq!(
                parts(eye_rect(Some(s), Eye::Left)),
                (0.0, 0.0, 1.0, 0.5),
                "{s:?}"
            );
            assert_eq!(
                parts(eye_rect(Some(s), Eye::Right)),
                (0.0, 0.5, 1.0, 0.5),
                "{s:?}"
            );
        }
        let whole = (0.0, 0.0, 1.0, 1.0);
        assert_eq!(parts(eye_rect(Some(Stereo::Mvc), Eye::Right)), whole);
        assert_eq!(parts(eye_rect(None, Eye::Left)), whole);
    }

    #[test]
    fn only_a_splitting_packing_in_one_eye_mode_is_cropped() {
        for s in ALL {
            let want = (s != Stereo::Mvc).then_some(s);
            assert_eq!(cropped(Some(s), ViewMode::OneEye), want, "{s:?}");
            assert_eq!(cropped(Some(s), ViewMode::BothEyes), None, "{s:?}");
        }
        assert_eq!(cropped(None, ViewMode::OneEye), None);
    }

    #[test]
    fn only_a_splitting_packing_gets_a_vr_layout() {
        assert_eq!(
            xr_layout(Some(Stereo::HalfSideBySide)),
            Some("stereo-left-right")
        );
        assert_eq!(
            xr_layout(Some(Stereo::FullSideBySide)),
            Some("stereo-left-right")
        );
        assert_eq!(
            xr_layout(Some(Stereo::HalfTopAndBottom)),
            Some("stereo-top-bottom")
        );
        assert_eq!(
            xr_layout(Some(Stereo::FullTopAndBottom)),
            Some("stereo-top-bottom")
        );
        assert_eq!(xr_layout(Some(Stereo::Mvc)), None);
        assert_eq!(xr_layout(None), None);
    }

    #[test]
    fn only_granted_layers_with_a_media_binding_take_the_layers_path() {
        assert_eq!(XrPath::choose(true, true), XrPath::Layers);
        assert_eq!(XrPath::choose(true, false), XrPath::Webgl);
        assert_eq!(XrPath::choose(false, true), XrPath::Webgl);
        assert_eq!(XrPath::choose(false, false), XrPath::Webgl);
    }

    #[test]
    fn the_vr_screen_has_the_eyes_shape() {
        let h = vr_screen_height(1920, 1080);
        assert!((VR_SCREEN_WIDTH_M / h - 16.0 / 9.0).abs() < 1e-9);
        assert!((vr_screen_height(0, 0) - vr_screen_height(1920, 1080)).abs() < 1e-9);
        assert!((VR_SCREEN_WIDTH_M / vr_screen_height(1440, 1080) - 4.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_packing_attribute_reads_back_as_its_packing() {
        for s in ALL {
            assert_eq!(parse_packing(packing_attr(s)), Some(s));
        }
        assert_eq!(parse_packing(""), None);
    }

    #[test]
    fn the_view_mode_round_trips_and_defaults_to_one_eye() {
        for m in ViewMode::ALL {
            assert_eq!(ViewMode::parse(m.as_str()), m);
        }
        assert_eq!(ViewMode::parse("nonsense"), ViewMode::OneEye);
        assert_eq!(ViewMode::default(), ViewMode::OneEye);
    }
}
