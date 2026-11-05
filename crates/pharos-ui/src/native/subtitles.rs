//! The subtitle renderer port (spec 026 FR-006) and its Rust adapter.
//!
//! The player talks to a [`SubtitleRenderer`], never to `pharos-subs`
//! directly, so the renderer can move into a Web Worker (needed for Android
//! and for animated tracks, later slices) without touching the view: every
//! call takes owned or plain data and every answer comes back by value.
//!
//! Slice 1 runs the renderer on the UI's own thread: a cue change costs about
//! 60 us on the host and an unchanged frame is a binary search.

use std::fmt;

use pharos_subs::{Frame, Rect, RenderStats, Renderer, Script, SubtitleStyle, Viewport};

use super::playback::SubtitleSize;

/// Why the renderer could not take a font or a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleError(pub String);

impl fmt::Display for SubtitleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SubtitleError {}

/// Draws one subtitle track as alpha masks.
pub trait SubtitleRenderer {
    /// Adds a TTF/OTF font. The first font added is the default face.
    fn add_font(&mut self, font: Vec<u8>) -> Result<(), SubtitleError>;

    /// Replaces the track with the cues in `text` (SRT, WebVTT or ASS).
    fn load(&mut self, text: String) -> Result<(), SubtitleError>;

    /// Changes the Pharos plain style; shows on the next [`frame`](Self::frame).
    fn set_style(&mut self, style: SubtitleStyle);

    /// The masks for `t_ms`. `changed == false` means nothing differs from
    /// the previous answer and `masks` is empty.
    fn frame(&mut self, t_ms: i64, viewport: &Viewport) -> Frame;

    fn stats(&self) -> RenderStats;
}

/// The `pharos-subs` renderer, in this thread.
#[derive(Default)]
pub struct RustSubs {
    inner: Renderer,
}

impl RustSubs {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SubtitleRenderer for RustSubs {
    fn add_font(&mut self, font: Vec<u8>) -> Result<(), SubtitleError> {
        self.inner
            .add_font(font)
            .map(|_| ())
            .map_err(|e| SubtitleError(e.to_string()))
    }

    fn load(&mut self, text: String) -> Result<(), SubtitleError> {
        self.inner.load(Script::parse(&text));
        Ok(())
    }

    fn set_style(&mut self, style: SubtitleStyle) {
        self.inner.set_style(style);
    }

    fn frame(&mut self, t_ms: i64, viewport: &Viewport) -> Frame {
        self.inner.frame(t_ms, viewport)
    }

    fn stats(&self) -> RenderStats {
        self.inner.stats()
    }
}

/// The Pharos look at a viewer's size choice (FR-009).
pub fn style_for(size: SubtitleSize) -> SubtitleStyle {
    SubtitleStyle {
        size_scale: size.scale(),
        ..SubtitleStyle::default()
    }
}

/// Where the picture sits inside a `width` x `height` box that shows it with
/// `object-fit: contain`; the whole box until the picture's size is known.
/// Mirrors the image subtitles' `min(100cqw, 100cqh * aw / ah)` (019).
pub fn content_box(width: u32, height: u32, video_w: u32, video_h: u32) -> Rect {
    if width == 0 || height == 0 || video_w == 0 || video_h == 0 {
        return Rect {
            x: 0,
            y: 0,
            w: width,
            h: height,
        };
    }
    let (w, h) = (u64::from(width), u64::from(height));
    let (vw, vh) = (u64::from(video_w), u64::from(video_h));
    // Compare aspect ratios without division: video wider than the box fits
    // to the width, otherwise to the height.
    let (cw, ch) = if vw * h >= vh * w {
        (w, (w * vh).div_ceil(vw).min(h))
    } else {
        ((h * vw).div_ceil(vh).min(w), h)
    };
    Rect {
        x: ((w - cw) / 2) as i32,
        y: ((h - ch) / 2) as i32,
        w: cw as u32,
        h: ch as u32,
    }
}

/// How far the control bar covers the bottom of the player: the distance
/// from the top of the bar to the bottom of the player while the controls
/// show, nothing while they are hidden.
pub fn controls_inset(controls_visible: bool, player_bottom: f64, controls_top: f64) -> u32 {
    if !controls_visible {
        return 0;
    }
    let covered = player_bottom - controls_top;
    if covered.is_finite() && covered > 0.0 {
        covered.round() as u32
    } else {
        0
    }
}

/// Why a text track fell back to the browser's own cue display. The label
/// is the `reason` of the `pharos: subtitles fallback` log line (016 FR-014
/// `pharos_subtitle_render_fallback_total{reason}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// The renderer rejected the font.
    Font,
    /// The font file could not be fetched.
    FontFetch,
    /// The track's WebVTT could not be fetched.
    TrackFetch,
    /// No WebGL2 context for the compositor.
    NoWebgl2,
    /// The compositor lost its context for good or failed to draw.
    Gpu,
    /// Fonts, track and GPU were not ready within the first-subtitle budget.
    Slow,
    /// The `<video>` was gone before the renderer could attach.
    NoVideo,
}

impl FallbackReason {
    pub const ALL: [FallbackReason; 7] = [
        FallbackReason::Font,
        FallbackReason::FontFetch,
        FallbackReason::TrackFetch,
        FallbackReason::NoWebgl2,
        FallbackReason::Gpu,
        FallbackReason::Slow,
        FallbackReason::NoVideo,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FallbackReason::Font => "font",
            FallbackReason::FontFetch => "font_fetch",
            FallbackReason::TrackFetch => "track_fetch",
            FallbackReason::NoWebgl2 => "no_webgl2",
            FallbackReason::Gpu => "gpu",
            FallbackReason::Slow => "slow",
            FallbackReason::NoVideo => "no_video",
        }
    }
}

/// The first subtitle must be ready this soon after the viewer picks the
/// track (spec 026 budget); later, the browser's cues take over.
pub const FIRST_SUBTITLE_BUDGET_MS: u32 = 300;

/// How the `<track>` of the picked subtitle should be set (spec 026 US1 #1,
/// #4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackMode {
    /// The browser draws its cues.
    Showing,
    /// Loaded, so its words survive a failure, but not drawn.
    Hidden,
}

/// The mode of the picked text track: hidden while the Rust renderer draws it
/// or is starting (no flash of `::cue` before the first frame), showing once
/// it is not an option.
pub fn track_mode(rust: RustState) -> TrackMode {
    match rust {
        RustState::Starting | RustState::Active => TrackMode::Hidden,
        RustState::Off | RustState::Failed => TrackMode::Showing,
    }
}

/// Where the Rust renderer is for the picked subtitle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RustState {
    /// The pick is not a plain text track, or subtitles are off.
    #[default]
    Off,
    Starting,
    Active,
    /// Fell back to the browser's cues.
    Failed,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use pharos_subs::{Mask, MaskKind};

    use super::*;

    const FONT: &[u8] = include_bytes!("../../assets/fonts/LiberationSans-Regular.ttf");
    const VTT: &str = "WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nHello there\n";

    /// A stand-in that rasters nothing: one fill mask per active cue. It
    /// exists so view logic can be tested without fonts, and is held to the
    /// same contract as the real adapter.
    #[derive(Default)]
    struct Fake {
        script: Option<Script>,
        style_gen: u32,
        last: Option<(Vec<usize>, u32, Viewport)>,
    }

    impl SubtitleRenderer for Fake {
        fn add_font(&mut self, font: Vec<u8>) -> Result<(), SubtitleError> {
            if font.starts_with(&[0, 1, 0, 0]) || font.starts_with(b"OTTO") {
                Ok(())
            } else {
                Err(SubtitleError("not a font".into()))
            }
        }

        fn load(&mut self, text: String) -> Result<(), SubtitleError> {
            self.script = Some(Script::parse(&text));
            self.last = None;
            Ok(())
        }

        fn set_style(&mut self, _style: SubtitleStyle) {
            self.style_gen += 1;
        }

        fn frame(&mut self, t_ms: i64, vp: &Viewport) -> Frame {
            let events = self.script.as_ref().map_or(&[][..], |s| &s.events[..]);
            let active: Vec<usize> = events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.start_ms <= t_ms && t_ms < e.end_ms)
                .map(|(i, _)| i)
                .collect();
            let key = (active.clone(), self.style_gen, *vp);
            let changed = self.last.as_ref() != Some(&key);
            self.last = Some(key);
            let masks = if changed {
                active
                    .iter()
                    .map(|i| Mask {
                        id: *i as u64 + 1,
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                        stride: 1,
                        coverage: Arc::from(vec![255u8]),
                        tint: [255; 4],
                        kind: MaskKind::Fill,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Frame {
                changed,
                masks,
                render_size: (vp.width, vp.height),
                scale: 1.0,
            }
        }

        fn stats(&self) -> RenderStats {
            RenderStats::default()
        }
    }

    fn vp() -> Viewport {
        Viewport::full(1280, 720)
    }

    /// What every adapter owes the view.
    fn contract(r: &mut dyn SubtitleRenderer) {
        r.add_font(FONT.to_vec()).unwrap();
        assert!(r.add_font(b"not a font".to_vec()).is_err());
        r.load(VTT.to_owned()).unwrap();

        // Before the cue: first frame is a (blank) change, then idle.
        let f = r.frame(0, &vp());
        assert!(f.changed && f.masks.is_empty());
        let f = r.frame(500, &vp());
        assert!(!f.changed && f.masks.is_empty());

        // Cue starts: one change carrying masks, then idle until it ends.
        let f = r.frame(1_000, &vp());
        assert!(f.changed && !f.masks.is_empty());
        assert!(f.masks.iter().any(|m| m.kind == MaskKind::Fill));
        for t in [1_500, 2_999] {
            let f = r.frame(t, &vp());
            assert!(!f.changed && f.masks.is_empty(), "t={t}");
        }

        // Cue ends: one change to nothing.
        let f = r.frame(3_000, &vp());
        assert!(f.changed && f.masks.is_empty());

        // A seek back into the cue draws it again.
        assert!(r.frame(2_000, &vp()).changed);

        // A new style, or a new viewport, redraws the same time.
        r.set_style(SubtitleStyle {
            size_scale: 1.5,
            ..SubtitleStyle::default()
        });
        assert!(r.frame(2_000, &vp()).changed);
        assert!(!r.frame(2_000, &vp()).changed);
        assert!(r.frame(2_000, &Viewport::full(1920, 1080)).changed);

        // Loading another track forgets the first.
        r.load("WEBVTT\n".to_owned()).unwrap();
        let f = r.frame(2_000, &vp());
        assert!(f.changed && f.masks.is_empty());
    }

    #[test]
    fn the_picture_is_letterboxed_or_pillarboxed_like_object_fit_contain() {
        let r = |x, y, w, h| Rect { x, y, w, h };
        // 2.39:1 film in a 16:9 box: bars above and below.
        assert_eq!(content_box(1920, 1080, 2392, 1000), r(0, 138, 1920, 803));
        // 4:3 in 16:9: bars at the sides.
        assert_eq!(content_box(1920, 1080, 640, 480), r(240, 0, 1440, 1080));
        // Same aspect fills the box.
        assert_eq!(content_box(1280, 720, 1920, 1080), r(0, 0, 1280, 720));
        // Unknown picture size: the whole box.
        assert_eq!(content_box(800, 600, 0, 0), r(0, 0, 800, 600));
        assert_eq!(content_box(0, 0, 1920, 1080), r(0, 0, 0, 0));
    }

    #[test]
    fn subtitles_clear_the_control_bar_only_while_it_shows() {
        assert_eq!(controls_inset(true, 720.0, 640.0), 80);
        assert_eq!(controls_inset(false, 720.0, 640.0), 0);
        // A bar measured off screen or not laid out never pushes them up.
        assert_eq!(controls_inset(true, 720.0, 800.0), 0);
        assert_eq!(controls_inset(true, 720.0, f64::NAN), 0);
    }

    #[test]
    fn size_choices_change_only_the_scale_of_the_pharos_style() {
        let base = style_for(SubtitleSize::Medium);
        assert_eq!(base, SubtitleStyle::default());
        let big = style_for(SubtitleSize::ExtraLarge);
        assert_eq!(big.size_scale, 1.5);
        assert_eq!(
            SubtitleStyle {
                size_scale: 1.0,
                ..big
            },
            base
        );
    }

    #[test]
    fn the_browser_cues_show_unless_the_rust_renderer_has_the_track() {
        assert_eq!(track_mode(RustState::Off), TrackMode::Showing);
        assert_eq!(track_mode(RustState::Failed), TrackMode::Showing);
        assert_eq!(track_mode(RustState::Starting), TrackMode::Hidden);
        assert_eq!(track_mode(RustState::Active), TrackMode::Hidden);
    }

    #[test]
    fn fallback_labels_are_distinct() {
        let labels: std::collections::HashSet<_> =
            FallbackReason::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(labels.len(), FallbackReason::ALL.len());
    }

    #[test]
    fn the_fake_meets_the_contract() {
        contract(&mut Fake::default());
    }

    #[test]
    fn rust_subs_meets_the_contract() {
        contract(&mut RustSubs::new());
    }

    #[test]
    fn rust_subs_counts_idle_frames() {
        let mut r = RustSubs::new();
        r.add_font(FONT.to_vec()).unwrap();
        r.load(VTT.to_owned()).unwrap();
        r.frame(1_000, &vp());
        for t in 1_001..1_100 {
            assert!(!r.frame(t, &vp()).changed);
        }
        let s = r.stats();
        assert_eq!(s.idle_frames, 99);
        assert_eq!(s.events_rastered, 1);
    }

    #[test]
    fn the_masks_are_ordered_back_to_front() {
        let mut r = RustSubs::new();
        r.add_font(FONT.to_vec()).unwrap();
        r.load(VTT.to_owned()).unwrap();
        let f = r.frame(1_500, &vp());
        let rank = |k: MaskKind| match k {
            MaskKind::Backdrop => 0,
            MaskKind::Shadow => 1,
            MaskKind::Border => 2,
            MaskKind::Fill => 3,
        };
        let ranks: Vec<_> = f.masks.iter().map(|m| rank(m.kind)).collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{ranks:?}");
        assert!(ranks.contains(&0) && ranks.contains(&3));
    }
}
