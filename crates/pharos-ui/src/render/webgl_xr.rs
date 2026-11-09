//! The WebGL side of the VR fallback (spec 022 US3, ADR 0088): one program,
//! one unit quad, one texture holding the video's current frame. The frame
//! is uploaded once per new video frame, not per display frame, and drawn
//! into each eye's viewport with that eye's part of the frame.

use web_sys::{
    HtmlVideoElement, WebGl2RenderingContext as Gl, WebGlBuffer, WebGlProgram, WebGlTexture,
    WebGlUniformLocation, WebGlVertexArrayObject,
};

use super::stereo_quad::{FRAGMENT, VERTEX};
use super::webgl::link;
use super::Fallback;

/// Two triangles covering the square from -1 to 1.
const CORNERS: [f32; 12] = [
    -1.0, -1.0, 1.0, -1.0, -1.0, 1.0, //
    -1.0, 1.0, 1.0, -1.0, 1.0, 1.0,
];

pub struct EyeQuads {
    gl: Gl,
    program: WebGlProgram,
    vao: WebGlVertexArrayObject,
    corners: WebGlBuffer,
    texture: WebGlTexture,
    projection: Option<WebGlUniformLocation>,
    view: Option<WebGlUniformLocation>,
    model: Option<WebGlUniformLocation>,
    eye: Option<WebGlUniformLocation>,
    /// The video time last uploaded, so a display frame between two video
    /// frames uploads nothing.
    uploaded_at: f64,
}

impl EyeQuads {
    pub fn new(gl: Gl) -> Result<EyeQuads, Fallback> {
        let program = link(&gl, VERTEX, FRAGMENT)?;
        let Ok(at) = u32::try_from(gl.get_attrib_location(&program, "a_corner")) else {
            gl.delete_program(Some(&program));
            return Err(Fallback::Shader("no a_corner".to_owned()));
        };
        let vao = gl
            .create_vertex_array()
            .ok_or_else(|| Fallback::Shader("create vertex array".to_owned()))?;
        let corners = gl
            .create_buffer()
            .ok_or_else(|| Fallback::Shader("create buffer".to_owned()))?;
        gl.bind_vertex_array(Some(&vao));
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&corners));
        let data = js_sys::Float32Array::from(CORNERS.as_slice());
        gl.buffer_data_with_array_buffer_view(Gl::ARRAY_BUFFER, &data, Gl::STATIC_DRAW);
        gl.enable_vertex_attrib_array(at);
        gl.vertex_attrib_pointer_with_i32(at, 2, Gl::FLOAT, false, 0, 0);
        gl.bind_vertex_array(None);
        let texture = gl
            .create_texture()
            .ok_or_else(|| Fallback::Shader("create texture".to_owned()))?;
        gl.bind_texture(Gl::TEXTURE_2D, Some(&texture));
        for (k, v) in [
            (Gl::TEXTURE_MIN_FILTER, Gl::LINEAR),
            (Gl::TEXTURE_MAG_FILTER, Gl::LINEAR),
            (Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE),
            (Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameteri(Gl::TEXTURE_2D, k, v as i32);
        }
        let uniform = |name: &str| gl.get_uniform_location(&program, name);
        Ok(EyeQuads {
            projection: uniform("u_projection"),
            view: uniform("u_view"),
            model: uniform("u_model"),
            eye: uniform("u_eye"),
            gl,
            program,
            vao,
            corners,
            texture,
            uploaded_at: f64::NAN,
        })
    }

    /// Uploads `video`'s frame if it moved on since the last upload.
    pub fn upload(&mut self, video: &HtmlVideoElement) -> Result<(), String> {
        let now = video.current_time();
        if video.ready_state() < 2 || now == self.uploaded_at {
            return Ok(());
        }
        let gl = &self.gl;
        gl.bind_texture(Gl::TEXTURE_2D, Some(&self.texture));
        match gl.tex_image_2d_with_u32_and_u32_and_html_video_element(
            Gl::TEXTURE_2D,
            0,
            Gl::RGBA as i32,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            video,
        ) {
            Ok(()) => {
                self.uploaded_at = now;
                Ok(())
            }
            Err(e) => Err(format!("{e:?}")),
        }
    }

    /// Clears the bound framebuffer to black.
    pub fn clear(&self) {
        self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
        self.gl.clear(Gl::COLOR_BUFFER_BIT | Gl::DEPTH_BUFFER_BIT);
    }

    /// Draws the screen into one eye's `viewport` (x, y, w, h) of the bound
    /// framebuffer, sampling `eye` of the frame.
    pub fn draw(
        &self,
        viewport: (i32, i32, i32, i32),
        projection: &[f32],
        view: &[f32],
        model: &[f32; 16],
        eye: [f32; 4],
    ) {
        let gl = &self.gl;
        gl.viewport(viewport.0, viewport.1, viewport.2, viewport.3);
        gl.use_program(Some(&self.program));
        gl.uniform_matrix4fv_with_f32_array(self.projection.as_ref(), false, projection);
        gl.uniform_matrix4fv_with_f32_array(self.view.as_ref(), false, view);
        gl.uniform_matrix4fv_with_f32_array(self.model.as_ref(), false, model);
        gl.uniform4fv_with_f32_array(self.eye.as_ref(), &eye);
        gl.active_texture(Gl::TEXTURE0);
        gl.bind_texture(Gl::TEXTURE_2D, Some(&self.texture));
        gl.bind_vertex_array(Some(&self.vao));
        gl.draw_arrays(Gl::TRIANGLES, 0, 6);
        gl.bind_vertex_array(None);
    }
}

impl Drop for EyeQuads {
    fn drop(&mut self) {
        let gl = &self.gl;
        gl.delete_texture(Some(&self.texture));
        gl.delete_buffer(Some(&self.corners));
        gl.delete_vertex_array(Some(&self.vao));
        gl.delete_program(Some(&self.program));
    }
}
