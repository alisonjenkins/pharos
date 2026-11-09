//! A theme's animated backdrop: its allow-listed shader drawn on a canvas
//! behind every native screen (not the legacy `/manage` panes) through the GPU layer (010 effects, ADR-0039). The
//! CSS light and vignette in `pharos.css` sit on top of it and are what the
//! page shows when the shader cannot run.

use dioxus::prelude::*;

use crate::render::{Palette, Program};
use crate::theme::value::Background;
use crate::theme::Theme;

use super::app::Themes;
use super::platform;
use super::route::Route;

/// The canvas the backdrop draws on.
pub const CANVAS_ID: &str = "pharos-backdrop";
const REDUCED_MOTION: &str = "(prefers-reduced-motion: reduce)";

/// The shader a theme asks for behind the page, if any.
pub fn program_for(theme: &Theme) -> Option<Program> {
    match theme.effects.background {
        Background::Shader => theme.effects.shader.map(Program::backdrop),
        Background::Solid | Background::Gradient => None,
    }
}

/// Whether the backdrop runs behind `route`: not behind the watch page,
/// whose video covers the whole screen, so the shader would burn GPU time
/// nobody can see (B808). The same holds behind the full-screen reader.
pub fn runs_behind(route: &Route) -> bool {
    !matches!(route, Route::Watch(_) | Route::Read(_))
}

/// The theme colours the shader tints with.
pub fn palette_for(theme: &Theme) -> Palette {
    let p = &theme.palette;
    Palette::from_tokens(&p.bg.to_css(), &p.accent.to_css(), &p.fg.to_css())
}

/// Frames a backdrop draws per animation frame: every other one, as a slow
/// backdrop gains nothing from more and a headset keeps the GPU time.
pub const DRAW_EVERY: u32 = 2;
/// Animation frames to skip after start before judging, while the page and
/// its images settle.
const WARM_UP: u32 = 60;
/// Animation frames the budget averages over.
const WINDOW: usize = 60;
/// Mean milliseconds between animation frames past which the backdrop is
/// costing the page its smoothness (under ~30 fps), and stops (FR-007).
pub const BUDGET_MS: f64 = 34.0;
/// A gap past this is a pause, not slowness: a hidden tab, a stall while
/// the page loads, a long collection. It is left out of the mean.
const PAUSE_MS: f64 = 250.0;

/// FR-007's effects budget for one backdrop: fed the time of each animation
/// frame, it says when the page has been too slow for long enough that the
/// backdrop should stop.
#[derive(Debug, Default)]
pub struct Budget {
    frames: u32,
    last_ms: Option<f64>,
    gaps: Vec<f64>,
    next: usize,
}

impl Budget {
    /// Record a frame at `now_ms`; `Some(mean gap)` once the mean gap over
    /// the window is past [`BUDGET_MS`].
    pub fn over(&mut self, now_ms: f64) -> Option<f64> {
        let last = self.last_ms.replace(now_ms);
        self.frames = self.frames.saturating_add(1);
        let gap = now_ms - last?;
        if self.frames <= WARM_UP || gap > PAUSE_MS {
            return None;
        }
        if self.gaps.len() < WINDOW {
            self.gaps.push(gap);
        } else {
            self.gaps[self.next] = gap;
            self.next = (self.next + 1) % WINDOW;
        }
        if self.gaps.len() < WINDOW {
            return None;
        }
        let mean = self.gaps.iter().sum::<f64>() / WINDOW as f64;
        (mean > BUDGET_MS).then_some(mean)
    }
}

/// The applied theme's backdrop, or nothing when it has none or the user
/// asks for reduced motion. Remounts on a theme change, so a new program
/// or palette starts a fresh surface.
#[component]
pub fn Backdrop() -> Element {
    let themes = use_context::<Themes>();
    let mut reduced = use_signal(|| platform::prefers(REDUCED_MOTION));
    use_hook(move || {
        std::rc::Rc::new(platform::on_media_change(REDUCED_MOTION, move || {
            reduced.set(platform::prefers(REDUCED_MOTION))
        }))
    });
    let Some(theme) = themes.applied_theme() else {
        return rsx! {};
    };
    let Some(program) = program_for(&theme) else {
        return rsx! {};
    };
    if reduced() {
        platform::info(&format!(
            "pharos: backdrop mode=css reason=reduced_motion program={program:?}"
        ));
        return rsx! {};
    }
    let palette = palette_for(&theme);
    rsx! {
        BackdropCanvas { key: "{theme.name}", program, palette }
    }
}

#[component]
fn BackdropCanvas(program: Program, palette: Palette) -> Element {
    let mut running = use_signal(|| None::<std::rc::Rc<animate::Running>>);
    // Effects run once the canvas is in the page; dropping the component
    // drops the handle, which stops the animation.
    use_effect(move || running.set(animate::start(program, palette).map(std::rc::Rc::new)));
    rsx! {
        canvas { id: CANVAS_ID, class: "pharos-backdrop", aria_hidden: "true" }
    }
}

#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod animate {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast as _;

    use super::{Budget, CANVAS_ID, DRAW_EVERY};
    use crate::native::platform;
    use crate::render::{self, Fallback, Frame, Palette, Program};

    type Tick = Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>>;

    /// A running backdrop; dropping it stops the animation.
    ///
    /// The frame callback is only ever freed here, never from inside its
    /// own call: the pending frame is cancelled first, so the browser can
    /// no longer call it, and taking it out of `tick` breaks the cycle the
    /// callback holds through its own `tick` handle.
    pub struct Running {
        tick: Tick,
        pending: Rc<Cell<Option<i32>>>,
        visibility: Closure<dyn FnMut()>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let (Some(window), Some(id)) = (web_sys::window(), self.pending.take()) {
                let _ = window.cancel_animation_frame(id);
            }
            if let Some(document) = web_sys::window().and_then(|w| w.document()) {
                let _ = document.remove_event_listener_with_callback(
                    "visibilitychange",
                    self.visibility.as_ref().unchecked_ref(),
                );
            }
            drop(self.tick.borrow_mut().take());
        }
    }

    fn page_hidden() -> bool {
        web_sys::window()
            .and_then(|w| w.document())
            .is_some_and(|d| d.hidden())
    }

    pub fn start(program: Program, palette: Palette) -> Option<Running> {
        let mut surface = match render::open(CANVAS_ID, program, false) {
            Ok(surface) => surface,
            Err(e) => {
                fall_back(program, &e);
                return None;
            }
        };
        platform::info(&format!(
            "pharos: backdrop mode=webgl2 reason=none program={program:?}"
        ));
        let tick: Tick = Rc::new(RefCell::new(None));
        let pending = Rc::new(Cell::new(None));
        let mut budget = Budget::default();
        let mut start_ms = None::<f64>;
        let mut count = 0u32;
        *tick.borrow_mut() = Some(Closure::new({
            let tick = Rc::clone(&tick);
            let pending = Rc::clone(&pending);
            move |now_ms: f64| {
                pending.set(None);
                // The loop stops while the page is hidden and the
                // visibility listener starts it again (B808).
                if page_hidden() {
                    platform::info("pharos: backdrop paused reason=hidden");
                    return;
                }
                if let Some(mean) = budget.over(now_ms) {
                    platform::warn(&format!(
                        "pharos: theme warning reason=effects_reduced path=effects.shader value={program:?} mean_frame_ms={mean:.1}"
                    ));
                    hide();
                    return;
                }
                count = count.wrapping_add(1);
                if count % DRAW_EVERY == 0 {
                    let start = *start_ms.get_or_insert(now_ms);
                    let frame = Frame {
                        time_s: Frame::clock(now_ms, start),
                        palette,
                        level: 0.0,
                        data: None,
                    };
                    if let Err(e) = surface.draw(&frame) {
                        fall_back(program, &e);
                        return;
                    }
                }
                request(&tick, &pending);
            }
        }));
        request(&tick, &pending);
        let visibility = Closure::<dyn FnMut()>::new({
            let tick = Rc::clone(&tick);
            let pending = Rc::clone(&pending);
            move || {
                if !page_hidden() && pending.get().is_none() && tick.borrow().is_some() {
                    platform::info("pharos: backdrop resumed reason=visible");
                    request(&tick, &pending);
                }
            }
        });
        if let Some(document) = web_sys::window().and_then(|w| w.document()) {
            let _ = document.add_event_listener_with_callback(
                "visibilitychange",
                visibility.as_ref().unchecked_ref(),
            );
        }
        Some(Running {
            tick,
            pending,
            visibility,
        })
    }

    fn request(tick: &Tick, pending: &Cell<Option<i32>>) {
        if let (Some(window), Some(callback)) = (web_sys::window(), tick.borrow().as_ref()) {
            pending.set(
                window
                    .request_animation_frame(callback.as_ref().unchecked_ref())
                    .ok(),
            );
        }
    }

    fn fall_back(program: Program, e: &Fallback) {
        let detail = match e {
            Fallback::Shader(log) => format!(" detail={log:?}"),
            _ => String::new(),
        };
        platform::warn(&format!(
            "pharos: backdrop mode=css reason={} program={program:?}{detail}",
            e.label()
        ));
        hide();
    }

    /// Hide the canvas so the CSS backdrop shows through.
    fn hide() {
        if let Some(canvas) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(CANVAS_ID))
        {
            let _ = canvas.set_attribute("hidden", "");
        }
    }
}

#[cfg(not(all(feature = "web", target_arch = "wasm32")))]
mod animate {
    use crate::render::{Palette, Program};

    pub struct Running;

    pub fn start(_program: Program, _palette: Palette) -> Option<Running> {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::theme::value::ShaderName;
    use crate::theme::{self};

    fn shipped(name: &str) -> Theme {
        theme::shipped().resolve(name).unwrap().theme
    }

    #[test]
    fn the_backdrop_stays_off_the_watch_page_only() {
        use pharos_client::api::ItemRef;
        let item = ItemRef::Real(42);
        assert!(!runs_behind(&Route::Watch(item)));
        assert!(!runs_behind(&Route::Read(item)));
        for route in [Route::Home, Route::Together] {
            assert!(runs_behind(&route), "{route:?}");
        }
    }

    #[test]
    fn only_a_shader_background_gets_a_program() {
        let mut t = shipped("High Contrast");
        assert_eq!(program_for(&t), None);
        t.effects.background = Background::Shader;
        t.effects.shader = Some(ShaderName::Grain);
        assert_eq!(program_for(&t), Some(Program::Grain));
        t.effects.background = Background::Gradient;
        assert_eq!(program_for(&t), None);
    }

    #[test]
    fn a_smooth_page_stays_within_budget() {
        let mut b = Budget::default();
        for i in 0..600 {
            assert_eq!(b.over(f64::from(i) * 16.7), None, "frame {i}");
        }
    }

    #[test]
    fn a_slow_page_goes_over_budget_after_warm_up_and_a_full_window() {
        let mut b = Budget::default();
        let mut first = None;
        for i in 0..400 {
            if b.over(f64::from(i) * 50.0).is_some() {
                first = Some(i);
                break;
            }
        }
        // Frame 0 sets the clock, the next WARM_UP are forgiven, and the
        // window then fills one gap per frame.
        assert_eq!(first, Some(WARM_UP as i32 + WINDOW as i32 - 1));
    }

    #[test]
    fn a_warm_up_stall_is_forgiven() {
        let mut b = Budget::default();
        let mut now = 0.0;
        for i in 0..400 {
            now += if i < WARM_UP { 200.0 } else { 16.7 };
            assert_eq!(b.over(now), None, "frame {i}");
        }
    }

    #[test]
    fn a_pause_after_warm_up_is_not_counted_as_slowness() {
        let mut b = Budget::default();
        let mut now = 0.0;
        for i in 0..600 {
            // A hidden tab for ten seconds, then a three-second stall.
            now += match i {
                200 => 10_000.0,
                400 => 3_000.0,
                _ => 16.7,
            };
            assert_eq!(b.over(now), None, "frame {i}");
        }
    }
}
