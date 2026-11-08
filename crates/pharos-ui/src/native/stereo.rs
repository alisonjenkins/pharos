//! 3D titles (V722): how a packing is named.

use pharos_client::api::Stereo;

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
