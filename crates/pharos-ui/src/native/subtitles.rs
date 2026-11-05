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

use pharos_subs::{Frame, RenderStats, Renderer, Script, SubtitleStyle, Viewport};

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
