//! The WebGL fallback for 3D in VR (spec 022 US3, ADR 0088): a browser with
//! WebXR but no media layers gets the video drawn by the page, one quad per
//! eye, each sampling its eye's part of the packed frame. Pure parts here;
//! the GL calls are in `webgl_xr.rs`.

use crate::native::stereo::{FrameRect, VR_SCREEN_DISTANCE_M};

/// `a_corner` is a corner of the unit square from -1 to 1. `u_eye` is the
/// eye's part of the frame (x, y, w, h, top-left origin), so the quad samples
/// only that eye.
pub const VERTEX: &str = r"#version 300 es
precision highp float;
in vec2 a_corner;
uniform mat4 u_projection;
uniform mat4 u_view;
uniform mat4 u_model;
uniform vec4 u_eye;
out vec2 v_uv;
void main() {
    vec2 unit = a_corner * 0.5 + 0.5;
    v_uv = u_eye.xy + vec2(unit.x, 1.0 - unit.y) * u_eye.zw;
    gl_Position = u_projection * u_view * u_model * vec4(a_corner, 0.0, 1.0);
}
";

/// highp: mediump cannot address single texels of a 3840-wide frame. The
/// clamp keeps bilinear sampling at an eye's inner edge from blending in the
/// other eye.
pub const FRAGMENT: &str = r"#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D u_frame;
uniform vec4 u_eye;
out vec4 color;
void main() {
    vec2 half_texel = 0.5 / vec2(textureSize(u_frame, 0));
    vec2 uv = clamp(v_uv, u_eye.xy + half_texel, u_eye.xy + u_eye.zw - half_texel);
    color = vec4(texture(u_frame, uv).rgb, 1.0);
}
";

/// The model matrix (column-major) that puts the unit quad where the media
/// layer path puts its screen: `width` x `height` metres, centred
/// [`VR_SCREEN_DISTANCE_M`] in front of the viewer's starting pose.
pub fn screen_model(width: f64, height: f64) -> [f32; 16] {
    let (sx, sy) = ((width / 2.0) as f32, (height / 2.0) as f32);
    let z = -VR_SCREEN_DISTANCE_M as f32;
    [
        sx, 0.0, 0.0, 0.0, //
        0.0, sy, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, //
        0.0, 0.0, z, 1.0,
    ]
}

/// `u_eye` for an eye's part of the frame.
pub fn eye_uniform(rect: FrameRect) -> [f32; 4] {
    [rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32]
}

#[cfg(test)]
mod tests {
    use pharos_client::api::Stereo;

    use super::*;
    use crate::native::stereo::{eye_rect, Eye};

    fn apply(m: &[f32; 16], (x, y): (f32, f32)) -> (f32, f32, f32) {
        // Column-major: column c is m[4c..4c+4].
        let at = |row: usize| m[row] * x + m[4 + row] * y + m[12 + row];
        (at(0), at(1), at(2))
    }

    #[test]
    fn the_quad_spans_the_screen_in_front_of_the_viewer() {
        let m = screen_model(3.5, 1.96875);
        let z = -VR_SCREEN_DISTANCE_M as f32;
        assert_eq!(apply(&m, (-1.0, -1.0)), (-1.75, -0.984375, z));
        assert_eq!(apply(&m, (1.0, 1.0)), (1.75, 0.984375, z));
    }

    #[test]
    fn each_eye_samples_its_own_half() {
        let left = eye_uniform(eye_rect(Some(Stereo::HalfSideBySide), Eye::Left));
        let right = eye_uniform(eye_rect(Some(Stereo::HalfSideBySide), Eye::Right));
        assert_eq!(left, [0.0, 0.0, 0.5, 1.0]);
        assert_eq!(right, [0.5, 0.0, 0.5, 1.0]);
        let bottom = eye_uniform(eye_rect(Some(Stereo::FullTopAndBottom), Eye::Right));
        assert_eq!(bottom, [0.0, 0.5, 1.0, 0.5]);
    }
}
