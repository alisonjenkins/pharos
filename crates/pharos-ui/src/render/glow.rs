//! WebGL2 through `glow`: the [`Surface`] the browser build draws with.

use std::cell::Cell;
use std::rc::Rc;

use glow::HasContext as _;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast as _;

use super::catalogue::{fragment_source, Program, VERTEX};
use super::{Fallback, Frame, Surface, DATA_ROWS, DATA_WIDTH};

/// Device pixels per CSS pixel at most: past this a fill-rate-bound headset
/// (Quest, Steam Frame) spends GPU time on detail a soft shader never shows.
const MAX_PIXEL_RATIO: f64 = 1.25;

struct Uniforms {
    time: Option<glow::UniformLocation>,
    resolution: Option<glow::UniformLocation>,
    bg: Option<glow::UniformLocation>,
    accent: Option<glow::UniformLocation>,
    fg: Option<glow::UniformLocation>,
    level: Option<glow::UniformLocation>,
}

pub struct GlowSurface {
    gl: glow::Context,
    canvas: web_sys::HtmlCanvasElement,
    program: glow::Program,
    vao: glow::VertexArray,
    texture: glow::Texture,
    uniforms: Uniforms,
    silence: Vec<u8>,
    lost: Rc<Cell<bool>>,
    on_lost: Closure<dyn FnMut(web_sys::Event)>,
    /// Kept to release the context on drop: browsers cap live contexts
    /// (~16) and free a dropped one only when it is garbage collected.
    lose: Option<web_sys::WebglLoseContext>,
}

pub fn open(canvas_id: &str, program: Program) -> Result<Box<dyn Surface>, Fallback> {
    let canvas = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(canvas_id))
        .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        .ok_or(Fallback::NoCanvas)?;
    let options = js_sys::Object::new();
    for (key, value) in [
        ("alpha", false),
        ("antialias", false),
        ("depth", false),
        ("stencil", false),
        ("premultipliedAlpha", false),
    ] {
        let _ = js_sys::Reflect::set(&options, &key.into(), &value.into());
    }
    let _ = js_sys::Reflect::set(&options, &"powerPreference".into(), &"low-power".into());
    let context = canvas
        .get_context_with_context_options("webgl2", &options)
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into::<web_sys::WebGl2RenderingContext>().ok())
        .ok_or(Fallback::NoWebgl2)?;
    let lose = context
        .get_extension("WEBGL_lose_context")
        .ok()
        .flatten()
        .map(|e| e.unchecked_into::<web_sys::WebglLoseContext>());
    let gl = glow::Context::from_webgl2_context(context);

    let lost = Rc::new(Cell::new(false));
    let on_lost = {
        let lost = Rc::clone(&lost);
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_| lost.set(true))
    };
    let _ = canvas
        .add_event_listener_with_callback("webglcontextlost", on_lost.as_ref().unchecked_ref());

    // SAFETY: every handle below comes from this context and is used only
    // with it; WebGL validates the calls and reports errors rather than
    // invoking undefined behaviour.
    unsafe {
        let program_handle = link(&gl, &fragment_source(program))?;
        let uniform = |name: &str| gl.get_uniform_location(program_handle, name);
        let uniforms = Uniforms {
            time: uniform("u_time"),
            resolution: uniform("u_resolution"),
            bg: uniform("u_bg"),
            accent: uniform("u_accent"),
            fg: uniform("u_fg"),
            level: uniform("u_level"),
        };
        let vao = gl.create_vertex_array().map_err(Fallback::Shader)?;
        let texture = gl.create_texture().map_err(Fallback::Shader)?;
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        for (param, value) in [
            (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
            (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
            (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, param, value as i32);
        }
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.use_program(Some(program_handle));
        gl.uniform_1_i32(
            gl.get_uniform_location(program_handle, "u_data").as_ref(),
            0,
        );
        Ok(Box::new(GlowSurface {
            gl,
            canvas,
            program: program_handle,
            vao,
            texture,
            uniforms,
            silence: silence(),
            lost,
            on_lost,
            lose,
        }))
    }
}

/// Bytes for a quiet frame: no spectrum, a flat waveform at mid-scale.
fn silence() -> Vec<u8> {
    let mut data = vec![0; DATA_WIDTH * DATA_ROWS];
    data[DATA_WIDTH..].fill(128);
    data
}

unsafe fn link(gl: &glow::Context, fragment: &str) -> Result<glow::Program, Fallback> {
    let program = gl.create_program().map_err(Fallback::Shader)?;
    let mut shaders = Vec::with_capacity(2);
    for (kind, source) in [
        (glow::VERTEX_SHADER, VERTEX),
        (glow::FRAGMENT_SHADER, fragment),
    ] {
        let shader = gl.create_shader(kind).map_err(Fallback::Shader)?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            return Err(Fallback::Shader(format!("compile: {log}")));
        }
        gl.attach_shader(program, shader);
        shaders.push(shader);
    }
    gl.link_program(program);
    for shader in shaders {
        gl.detach_shader(program, shader);
        gl.delete_shader(shader);
    }
    if !gl.get_program_link_status(program) {
        let log = gl.get_program_info_log(program);
        gl.delete_program(program);
        return Err(Fallback::Shader(format!("link: {log}")));
    }
    Ok(program)
}

impl GlowSurface {
    /// Match the drawing buffer to the canvas's box, capped at
    /// [`MAX_PIXEL_RATIO`]; returns the buffer size.
    fn fit(&self) -> (i32, i32) {
        let ratio = web_sys::window()
            .map_or(1.0, |w| w.device_pixel_ratio())
            .min(MAX_PIXEL_RATIO);
        let width = (f64::from(self.canvas.client_width()) * ratio)
            .round()
            .max(1.0) as u32;
        let height = (f64::from(self.canvas.client_height()) * ratio)
            .round()
            .max(1.0) as u32;
        if self.canvas.width() != width {
            self.canvas.set_width(width);
        }
        if self.canvas.height() != height {
            self.canvas.set_height(height);
        }
        (width as i32, height as i32)
    }
}

impl Surface for GlowSurface {
    fn draw(&mut self, frame: &Frame<'_>) -> Result<(), Fallback> {
        if self.lost.get() {
            return Err(Fallback::ContextLost);
        }
        let (width, height) = self.fit();
        let data = frame
            .data
            .filter(|d| d.len() == DATA_WIDTH * DATA_ROWS)
            .unwrap_or(&self.silence);
        let gl = &self.gl;
        let u = &self.uniforms;
        let [bg, accent, fg] = [frame.palette.bg, frame.palette.accent, frame.palette.fg];
        // SAFETY: as in `open`; all handles belong to `self.gl`.
        unsafe {
            gl.viewport(0, 0, width, height);
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::R8 as i32,
                DATA_WIDTH as i32,
                DATA_ROWS as i32,
                0,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(data)),
            );
            gl.uniform_1_f32(u.time.as_ref(), frame.time_s);
            gl.uniform_2_f32(u.resolution.as_ref(), width as f32, height as f32);
            gl.uniform_3_f32(u.bg.as_ref(), bg[0], bg[1], bg[2]);
            gl.uniform_3_f32(u.accent.as_ref(), accent[0], accent[1], accent[2]);
            gl.uniform_3_f32(u.fg.as_ref(), fg[0], fg[1], fg[2]);
            gl.uniform_1_f32(u.level.as_ref(), frame.level);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
        Ok(())
    }
}

impl Drop for GlowSurface {
    fn drop(&mut self) {
        let _ = self.canvas.remove_event_listener_with_callback(
            "webglcontextlost",
            self.on_lost.as_ref().unchecked_ref(),
        );
        // SAFETY: the handles belong to `self.gl` and are not used again.
        unsafe {
            self.gl.delete_texture(self.texture);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_program(self.program);
        }
        if let Some(lose) = &self.lose {
            lose.lose_context();
        }
    }
}
