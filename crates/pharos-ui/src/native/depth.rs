//! The DOM side of poster depth: pointer tilt. It writes straight to the
//! element's style instead of a signal, so moving the pointer never
//! re-renders.
//!
//! Host builds have no DOM; there the handlers are no-ops. The arithmetic
//! is a plain function, tested on the host.

use dioxus::prelude::*;

/// The element the tilt handlers sit on.
#[cfg(feature = "web")]
const ART: &str = ".pharos-poster-art";
/// The link around the art, which never moves.
#[cfg(feature = "web")]
const POSTER: &str = ".pharos-poster";

/// The element matching `selector` at or above the event's target. Dioxus
/// listens once at the app root and dispatches from there, so the event's
/// `currentTarget` is the root, not the element whose handler runs.
#[cfg(feature = "web")]
fn closest(web: &web_sys::Event, selector: &str) -> Option<web_sys::HtmlElement> {
    use wasm_bindgen::JsCast;
    web.target()?
        .dyn_into::<web_sys::Element>()
        .ok()?
        .closest(selector)
        .ok()??
        .dyn_into::<web_sys::HtmlElement>()
        .ok()
}

/// Tilt the poster art under the pointer towards it: sets `--px-tilt-x` and
/// `--px-tilt-y` in -1..=1 on the element the handler is on; CSS turns them
/// into degrees scaled by the theme's focus scale, so a theme with none never
/// tilts. Touch is ignored, since a finger moving over a poster is
/// scrolling the rail.
#[cfg(feature = "web")]
pub fn tilt(evt: &Event<PointerData>) {
    use dioxus_web::WebEventExt;
    if evt.pointer_type() == "touch" {
        return;
    }
    let Some(web) = evt.try_as_web_event() else {
        return;
    };
    let (Some(el), Some(poster)) = (closest(&web, ART), closest(&web, POSTER)) else {
        return;
    };
    // Measure the art where layout put it, not where the last tilt moved it:
    // the link is not transformed and starts where the art does, and the
    // offset size ignores transforms. Reading the tilted art's own rect would
    // feed each tilt back into the next.
    let origin = poster.get_bounding_client_rect();
    let (w, h) = (f64::from(el.offset_width()), f64::from(el.offset_height()));
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let at = evt.client_coordinates();
    let x = tilt_fraction(at.x, origin.left(), w);
    let y = tilt_fraction(at.y, origin.top(), h);
    let style = el.style();
    let _ = style.set_property("--px-tilt-x", &format!("{x:.3}"));
    let _ = style.set_property("--px-tilt-y", &format!("{y:.3}"));
}

#[cfg(not(feature = "web"))]
pub fn tilt(_evt: &Event<PointerData>) {}

/// Level the poster art again when the pointer leaves it.
#[cfg(feature = "web")]
pub fn untilt(evt: &Event<PointerData>) {
    use dioxus_web::WebEventExt;
    let Some(el) = evt.try_as_web_event().and_then(|web| closest(&web, ART)) else {
        return;
    };
    let style = el.style();
    let _ = style.remove_property("--px-tilt-x");
    let _ = style.remove_property("--px-tilt-y");
}

#[cfg(not(feature = "web"))]
pub fn untilt(_evt: &Event<PointerData>) {}

/// Where `at` falls across a box starting at `origin` of length `size`,
/// from -1 at one edge to 1 at the other, clamped.
pub fn tilt_fraction(at: f64, origin: f64, size: f64) -> f64 {
    ((at - origin) / size * 2.0 - 1.0).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilt_runs_edge_to_edge_and_clamps_outside() {
        assert_eq!(tilt_fraction(100.0, 100.0, 200.0), -1.0);
        assert_eq!(tilt_fraction(200.0, 100.0, 200.0), 0.0);
        assert_eq!(tilt_fraction(300.0, 100.0, 200.0), 1.0);
        assert_eq!(tilt_fraction(900.0, 100.0, 200.0), 1.0);
        assert_eq!(tilt_fraction(0.0, 100.0, 200.0), -1.0);
    }
}
