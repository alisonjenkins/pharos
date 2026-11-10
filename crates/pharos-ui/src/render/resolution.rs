//! How much of the device's pixel ratio a visualiser draws at: a style whose
//! shader the GPU cannot fill at full size draws smaller, and the browser
//! scales the canvas up (B1218, V1178, ADR-0111). Pure, so the host tests it.

/// Frames per verdict: about a second at 60 fps.
pub const WINDOW: usize = 60;

/// A window whose median frame is slower than this (under ~50 fps) lowers
/// the scale.
pub const SLOW_FRAME_MS: f64 = 20.0;

/// Intervals longer than this are a pause, a hidden page or a style load,
/// not drawing: they are left out.
pub const GAP_MS: f64 = 250.0;

/// Each slow window multiplies the scale by this.
pub const STEP: f64 = 0.8;

/// The scale goes no lower: half the size each way still reads as the same
/// soft picture once the browser scales it up.
pub const MIN_SCALE: f64 = 0.5;

/// A change of scale, for the decision log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lowered {
    pub from: f64,
    pub to: f64,
    /// The median frame interval of the window that decided it.
    pub median_ms: f64,
}

/// The scale of one visualiser run. It only goes down: a run that was slow
/// at a size is slow there again, and the next style starts at full size.
#[derive(Debug, Clone)]
pub struct Resolution {
    scale: f64,
    last_ms: Option<f64>,
    window: Vec<f64>,
}

impl Default for Resolution {
    fn default() -> Self {
        Self {
            scale: 1.0,
            last_ms: None,
            window: Vec::with_capacity(WINDOW),
        }
    }
}

impl Resolution {
    /// The share of the device's pixel ratio to draw at, in
    /// `MIN_SCALE..=1.0`.
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Note a frame drawn at `now_ms`; `Some` when this frame lowered the
    /// scale.
    pub fn frame(&mut self, now_ms: f64) -> Option<Lowered> {
        let last = self.last_ms.replace(now_ms)?;
        let interval = now_ms - last;
        if !(0.0..=GAP_MS).contains(&interval) {
            return None;
        }
        self.window.push(interval);
        if self.window.len() < WINDOW {
            return None;
        }
        let median_ms = median(&mut self.window);
        self.window.clear();
        if median_ms <= SLOW_FRAME_MS || self.scale <= MIN_SCALE {
            return None;
        }
        let from = self.scale;
        self.scale = (from * STEP).max(MIN_SCALE);
        Some(Lowered {
            from,
            to: self.scale,
            median_ms,
        })
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `frames` frames `interval_ms` apart from `at`; the lowerings.
    fn run(r: &mut Resolution, at: &mut f64, frames: usize, interval_ms: f64) -> Vec<Lowered> {
        (0..frames)
            .filter_map(|_| {
                *at += interval_ms;
                r.frame(*at)
            })
            .collect()
    }

    #[test]
    fn a_style_at_sixty_frames_keeps_its_full_size() {
        let mut r = Resolution::default();
        let mut at = 0.0;
        assert!(run(&mut r, &mut at, 600, 16.7).is_empty());
        assert_eq!(r.scale(), 1.0);
    }

    #[test]
    fn a_slow_style_draws_smaller_a_step_a_window_down_to_the_floor() {
        // Starfield on the TV: 50 ms frames at full size.
        let mut r = Resolution::default();
        let mut at = 0.0;
        let first = run(&mut r, &mut at, WINDOW + 1, 50.0);
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].from, 1.0);
        assert!((first[0].to - STEP).abs() < 1e-9);
        assert_eq!(first[0].median_ms, 50.0);
        run(&mut r, &mut at, WINDOW * 10, 50.0);
        assert_eq!(r.scale(), MIN_SCALE);
        // At the floor nothing more is logged.
        assert!(run(&mut r, &mut at, WINDOW * 2, 50.0).is_empty());
    }

    #[test]
    fn a_few_slow_frames_do_not_lower_it() {
        // The karaoke wipe or a GC pause now and then: under half of any
        // window is slow, so the median stays at a 60 fps frame.
        let mut r = Resolution::default();
        let mut at = 0.0;
        for i in 0..WINDOW * 20 {
            at += if i % WINDOW < WINDOW / 2 - 1 {
                33.4
            } else {
                16.7
            };
            assert!(r.frame(at).is_none(), "frame {i}");
        }
        assert_eq!(r.scale(), 1.0);
    }

    #[test]
    fn a_pause_is_not_a_slow_frame() {
        let mut r = Resolution::default();
        let mut at = 0.0;
        for _ in 0..WINDOW * 3 {
            // Each frame after a paused second: never counted.
            at += 1000.0;
            assert!(r.frame(at).is_none());
        }
        assert_eq!(r.scale(), 1.0);
        // A clock that goes back is not counted either.
        assert!(r.frame(at - 5.0).is_none());
    }
}
