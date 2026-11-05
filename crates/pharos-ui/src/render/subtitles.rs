//! Subtitle compositing (spec 026 FR-007): textured quads over the video.
//!
//! The rest of `render/` draws one full-screen shader per canvas. Subtitle
//! masks are many small bitmaps at arbitrary positions, so they get their own
//! port and program here, sharing the module's [`Fallback`] reasons and (on
//! the web) its shader linking. The bookkeeping that decides what to upload,
//! evict and draw is plain data in [`Textures::plan`], tested on the host.

use std::collections::HashSet;

use pharos_subs::{Frame, Mask};

use super::Fallback;

/// Vertex stage of the subtitles program.
pub const VERTEX: &str = include_str!("shaders/subtitles.vert");
/// Fragment stage of the subtitles program.
pub const FRAGMENT: &str = include_str!("shaders/subtitles.frag");

/// Draws [`Frame`]s of subtitle masks on a canvas over the picture.
pub trait SubtitleCompositor {
    /// Match the drawing buffer to `width` x `height` pixels. Clears the
    /// canvas when the size changes, so the caller asks for a new frame
    /// (its viewport changed, so the renderer answers `changed`).
    fn resize(&mut self, width: u32, height: u32);

    /// Draws `frame` when it `changed`; touches nothing otherwise.
    fn draw(&mut self, frame: &Frame) -> Result<(), Fallback>;

    /// Whether the last picture must be drawn again (the context came back).
    fn needs_redraw(&self) -> bool {
        false
    }

    /// Draws the last changed frame again.
    fn redraw(&mut self) -> Result<(), Fallback> {
        Ok(())
    }
}

/// One mask to draw: where, and in what colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    /// Texture key, [`Mask::id`].
    pub id: u64,
    /// x, y, width, height in canvas pixels, y down.
    pub rect: [f32; 4],
    /// Straight RGBA, 0 to 1.
    pub tint: [f32; 4],
}

/// What to do on the GPU for one changed frame.
#[derive(Debug)]
pub struct Plan<'a> {
    /// Masks whose bitmap has no texture yet, once per id.
    pub upload: Vec<&'a Mask>,
    /// Textures no mask of this frame uses.
    pub evict: Vec<u64>,
    /// Back-to-front, in the order the renderer gave.
    pub quads: Vec<Quad>,
}

/// Which mask ids have a texture.
#[derive(Debug, Default)]
pub struct Textures {
    live: HashSet<u64>,
}

impl Textures {
    /// What a changed `frame` needs, and records it: afterwards exactly the
    /// frame's ids are live, so memory follows the cues on screen.
    pub fn plan<'a>(&mut self, frame: &'a Frame) -> Plan<'a> {
        let scale = frame.scale;
        let drawn = |m: &&Mask| m.w > 0 && m.h > 0 && m.coverage.len() >= (m.w * m.h) as usize;
        let wanted: HashSet<u64> = frame.masks.iter().filter(drawn).map(|m| m.id).collect();
        let evict: Vec<u64> = self
            .live
            .iter()
            .copied()
            .filter(|id| !wanted.contains(id))
            .collect();
        for id in &evict {
            self.live.remove(id);
        }
        let mut upload = Vec::new();
        let mut quads = Vec::with_capacity(frame.masks.len());
        for m in frame.masks.iter().filter(drawn) {
            if self.live.insert(m.id) {
                upload.push(m);
            }
            quads.push(Quad {
                id: m.id,
                rect: [
                    m.x as f32 * scale,
                    m.y as f32 * scale,
                    m.w as f32 * scale,
                    m.h as f32 * scale,
                ],
                tint: m.tint.map(|c| f32::from(c) / 255.0),
            });
        }
        Plan {
            upload,
            evict,
            quads,
        }
    }

    /// Forget every texture: the context that held them is gone.
    pub fn clear(&mut self) {
        self.live.clear();
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pharos_subs::MaskKind;

    use super::*;

    fn mask(id: u64, kind: MaskKind, x: i32, tint: [u8; 4]) -> Mask {
        Mask {
            id,
            x,
            y: 10,
            w: 4,
            h: 2,
            stride: 4,
            coverage: Arc::from(vec![255u8; 8]),
            tint,
            kind,
        }
    }

    fn frame(masks: Vec<Mask>, scale: f32) -> Frame {
        Frame {
            changed: true,
            masks,
            render_size: (100, 100),
            scale,
        }
    }

    #[test]
    fn a_border_and_its_shadow_share_one_upload_and_two_quads() {
        let f = frame(
            vec![
                mask(1, MaskKind::Backdrop, 0, [0, 0, 0, 140]),
                mask(2, MaskKind::Shadow, 1, [0, 0, 0, 255]),
                mask(2, MaskKind::Border, 0, [0, 0, 0, 255]),
                mask(3, MaskKind::Fill, 0, [255, 255, 255, 255]),
            ],
            1.0,
        );
        let mut t = Textures::default();
        let plan = t.plan(&f);
        assert_eq!(plan.upload.len(), 3);
        assert_eq!(plan.quads.len(), 4);
        // Draw order is the frame's, back to front.
        let ids: Vec<u64> = plan.quads.iter().map(|q| q.id).collect();
        assert_eq!(ids, [1, 2, 2, 3]);
        assert!(plan.evict.is_empty());
    }

    #[test]
    fn the_next_frame_reuses_cached_textures_and_evicts_the_rest() {
        let mut t = Textures::default();
        let first = frame(
            vec![
                mask(1, MaskKind::Fill, 0, [255; 4]),
                mask(2, MaskKind::Fill, 0, [255; 4]),
            ],
            1.0,
        );
        t.plan(&first);
        let second = frame(
            vec![
                mask(2, MaskKind::Fill, 0, [255; 4]),
                mask(3, MaskKind::Fill, 0, [255; 4]),
            ],
            1.0,
        );
        let plan = t.plan(&second);
        let up: Vec<u64> = plan.upload.iter().map(|m| m.id).collect();
        assert_eq!(up, [3]);
        assert_eq!(plan.evict, [1]);
        assert_eq!(t.len(), 2);
        // An empty frame empties the GPU.
        let blank = frame(Vec::new(), 1.0);
        let plan = t.plan(&blank);
        assert_eq!(plan.evict.len(), 2);
        assert!(plan.quads.is_empty());
        assert!(t.is_empty());
    }

    #[test]
    fn quads_are_scaled_to_the_canvas_and_the_tint_is_unit_range() {
        let f = frame(vec![mask(1, MaskKind::Fill, 5, [255, 0, 51, 102])], 2.0);
        let plan = Textures::default().plan(&f);
        let q = plan.quads[0];
        assert_eq!(q.rect, [10.0, 20.0, 8.0, 4.0]);
        assert_eq!(q.tint, [1.0, 0.0, 0.2, 0.4]);
    }

    #[test]
    fn empty_or_short_masks_are_skipped() {
        let mut empty = mask(1, MaskKind::Fill, 0, [255; 4]);
        empty.w = 0;
        let mut short = mask(2, MaskKind::Fill, 0, [255; 4]);
        short.coverage = Arc::from(vec![0u8; 3]);
        let f = frame(vec![empty, short], 1.0);
        let plan = Textures::default().plan(&f);
        assert!(plan.upload.is_empty() && plan.quads.is_empty());
    }

    #[test]
    fn a_lost_context_forgets_its_textures() {
        let mut t = Textures::default();
        let f = frame(vec![mask(1, MaskKind::Fill, 0, [255; 4])], 1.0);
        t.plan(&f);
        t.clear();
        assert_eq!(t.plan(&f).upload.len(), 1);
    }

    #[test]
    fn the_shaders_are_glsl_es_300_and_premultiply() {
        assert!(VERTEX.starts_with("#version 300 es"));
        assert!(FRAGMENT.starts_with("#version 300 es"));
        assert!(VERTEX.contains("layout(location = 0) in vec2 a_corner;"));
        assert_eq!(VERTEX.matches("void main()").count(), 1);
        assert_eq!(FRAGMENT.matches("void main()").count(), 1);
        assert!(FRAGMENT.contains("u_tint.rgb * a"));
    }
}
