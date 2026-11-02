//! The DOM side of rails and posters: sideways wheel scrolling on rails.
//! It writes straight to the element (a scroll offset) instead of a signal,
//! so scrolling never re-renders.
//!
//! Host builds have no DOM; there every function is a no-op.

use dioxus::prelude::*;

/// The element the wheel handler sits on.
#[cfg(feature = "web")]
const RAIL: &str = ".pharos-rail-items";

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

/// Pixels one wheel "line" moves a rail, for mice that report lines.
#[cfg(feature = "web")]
const LINE_PX: f64 = 40.0;

/// Turn a mostly-vertical wheel turn over a rail into sideways scrolling
/// (011 US: "mouse wheel scrolls rails horizontally on hover"), since the
/// rail shows no scrollbar to drag. At either end of the rail the turn is
/// left alone, so the page scrolls on.
#[cfg(feature = "web")]
pub fn rail_wheel(evt: &Event<WheelData>) {
    use dioxus::html::geometry::WheelDelta;
    use dioxus_web::WebEventExt;
    let d = evt.delta();
    let (dx, dy) = match d {
        WheelDelta::Pixels(v) => (v.x, v.y),
        WheelDelta::Lines(v) => (v.x * LINE_PX, v.y * LINE_PX),
        WheelDelta::Pages(_) => return,
    };
    if dy.abs() <= dx.abs() {
        // Trackpads and tilt wheels already scroll sideways.
        return;
    }
    let Some(web) = evt.try_as_web_event() else {
        return;
    };
    let Some(el) = closest(&web, RAIL) else {
        return;
    };
    let max = f64::from(el.scroll_width() - el.client_width());
    let left = el.scroll_left() as f64;
    let at_end = (dy < 0.0 && left <= 0.0) || (dy > 0.0 && left >= max - 1.0);
    if max <= 0.0 || at_end {
        return;
    }
    web.prevent_default();
    el.scroll_by_with_x_and_y(dy, 0.0);
}

#[cfg(not(feature = "web"))]
pub fn rail_wheel(_evt: &Event<WheelData>) {}
