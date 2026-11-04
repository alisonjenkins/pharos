//! WebGL2 through `web_sys`: the [`Surface`] the browser build draws with.
//!
//! Called directly rather than through `glow`, whose context constructor asks
//! for every extension up front, `WEBGL_debug_renderer_info` among them,
//! which Firefox warns about (ADR-0049).

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast as _;
use web_sys::{
    WebGl2RenderingContext as Gl, WebGlProgram, WebGlTexture, WebGlUniformLocation,
    WebGlVertexArrayObject,
};

use super::catalogue::{fragment_source, Program, VERTEX};
use super::{Fallback, Frame, Surface, DATA_ROWS, DATA_WIDTH};

/// Device pixels per CSS pixel at most: past this a fill-rate-bound headset
/// (Quest, Steam Frame) spends GPU time on detail a soft shader never shows.
const MAX_PIXEL_RATIO: f64 = 1.25;

struct Uniforms {
    time: Option<WebGlUniformLocation>,
    resolution: Option<WebGlUniformLocation>,
    bg: Option<WebGlUniformLocation>,
    accent: Option<WebGlUniformLocation>,
    fg: Option<WebGlUniformLocation>,
    level: Option<WebGlUniformLocation>,
}

/// Everything made on the context for one program.
struct Resources {
    program: WebGlProgram,
    vao: WebGlVertexArrayObject,
    texture: WebGlTexture,
    uniforms: Uniforms,
}

pub struct WebglSurface {
    gl: Gl,
    canvas: web_sys::HtmlCanvasElement,
    resources: Resources,
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
    let gl = canvas
        .get_context_with_context_options("webgl2", &options)
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into::<Gl>().ok())
        .ok_or(Fallback::NoWebgl2)?;
    let lose = gl
        .get_extension("WEBGL_lose_context")
        .ok()
        .flatten()
        .map(|e| e.unchecked_into::<web_sys::WebglLoseContext>());

    let lost = Rc::new(Cell::new(false));
    let on_lost = {
        let lost = Rc::clone(&lost);
        Closure::<dyn FnMut(web_sys::Event)>::new(move |_| lost.set(true))
    };
    let _ = canvas
        .add_event_listener_with_callback("webglcontextlost", on_lost.as_ref().unchecked_ref());

    let resources = build(&gl, program)?;
    Ok(Box::new(WebglSurface {
        gl,
        canvas,
        resources,
        silence: silence(),
        lost,
        on_lost,
        lose,
    }))
}

/// Compile `program` and make its vertex array and data texture.
fn build(gl: &Gl, program: Program) -> Result<Resources, Fallback> {
    let handle = link(gl, &fragment_source(program))?;
    let uniform = |name: &str| gl.get_uniform_location(&handle, name);
    let uniforms = Uniforms {
        time: uniform("u_time"),
        resolution: uniform("u_resolution"),
        bg: uniform("u_bg"),
        accent: uniform("u_accent"),
        fg: uniform("u_fg"),
        level: uniform("u_level"),
    };
    let vao = gl
        .create_vertex_array()
        .ok_or_else(|| Fallback::Shader("create vertex array".to_owned()))?;
    let texture = gl
        .create_texture()
        .ok_or_else(|| Fallback::Shader("create texture".to_owned()))?;
    gl.bind_texture(Gl::TEXTURE_2D, Some(&texture));
    for (param, value) in [
        (Gl::TEXTURE_MIN_FILTER, Gl::LINEAR),
        (Gl::TEXTURE_MAG_FILTER, Gl::LINEAR),
        (Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE),
        (Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE),
    ] {
        gl.tex_parameteri(Gl::TEXTURE_2D, param, value as i32);
    }
    gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 1);
    gl.use_program(Some(&handle));
    gl.uniform1i(gl.get_uniform_location(&handle, "u_data").as_ref(), 0);
    Ok(Resources {
        program: handle,
        vao,
        texture,
        uniforms,
    })
}

/// Bytes for a quiet frame: no spectrum, a flat waveform at mid-scale.
fn silence() -> Vec<u8> {
    let mut data = vec![0; DATA_WIDTH * DATA_ROWS];
    data[DATA_WIDTH..].fill(128);
    data
}

fn link(gl: &Gl, fragment: &str) -> Result<WebGlProgram, Fallback> {
    let program = gl
        .create_program()
        .ok_or_else(|| Fallback::Shader("create program".to_owned()))?;
    let mut shaders = Vec::with_capacity(2);
    for (kind, source) in [(Gl::VERTEX_SHADER, VERTEX), (Gl::FRAGMENT_SHADER, fragment)] {
        let shader = gl
            .create_shader(kind)
            .ok_or_else(|| Fallback::Shader("create shader".to_owned()))?;
        gl.shader_source(&shader, source);
        gl.compile_shader(&shader);
        let compiled = gl
            .get_shader_parameter(&shader, Gl::COMPILE_STATUS)
            .as_bool()
            .unwrap_or(false);
        if !compiled {
            let log = gl.get_shader_info_log(&shader).unwrap_or_default();
            gl.delete_shader(Some(&shader));
            return Err(Fallback::Shader(format!("compile: {log}")));
        }
        gl.attach_shader(&program, &shader);
        shaders.push(shader);
    }
    gl.link_program(&program);
    for shader in &shaders {
        gl.detach_shader(&program, shader);
        gl.delete_shader(Some(shader));
    }
    let linked = gl
        .get_program_parameter(&program, Gl::LINK_STATUS)
        .as_bool()
        .unwrap_or(false);
    if !linked {
        let log = gl.get_program_info_log(&program).unwrap_or_default();
        gl.delete_program(Some(&program));
        return Err(Fallback::Shader(format!("link: {log}")));
    }
    Ok(program)
}

impl WebglSurface {
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

impl Surface for WebglSurface {
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
        let r = &self.resources;
        let u = &r.uniforms;
        let [bg, accent, fg] = [frame.palette.bg, frame.palette.accent, frame.palette.fg];
        gl.viewport(0, 0, width, height);
        gl.use_program(Some(&r.program));
        gl.bind_vertex_array(Some(&r.vao));
        gl.active_texture(Gl::TEXTURE0);
        gl.bind_texture(Gl::TEXTURE_2D, Some(&r.texture));
        // Fails only on bad arguments, which these constants are not; a lost
        // context makes it a no-op rather than an error.
        let _ = gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            Gl::TEXTURE_2D,
            0,
            Gl::R8 as i32,
            DATA_WIDTH as i32,
            DATA_ROWS as i32,
            0,
            Gl::RED,
            Gl::UNSIGNED_BYTE,
            Some(data),
        );
        gl.uniform1f(u.time.as_ref(), frame.time_s);
        gl.uniform2f(u.resolution.as_ref(), width as f32, height as f32);
        gl.uniform3f(u.bg.as_ref(), bg[0], bg[1], bg[2]);
        gl.uniform3f(u.accent.as_ref(), accent[0], accent[1], accent[2]);
        gl.uniform3f(u.fg.as_ref(), fg[0], fg[1], fg[2]);
        gl.uniform1f(u.level.as_ref(), frame.level);
        gl.draw_arrays(Gl::TRIANGLES, 0, 3);
        Ok(())
    }
}

impl Drop for WebglSurface {
    fn drop(&mut self) {
        let _ = self.canvas.remove_event_listener_with_callback(
            "webglcontextlost",
            self.on_lost.as_ref().unchecked_ref(),
        );
        let r = &self.resources;
        self.gl.delete_texture(Some(&r.texture));
        self.gl.delete_vertex_array(Some(&r.vao));
        self.gl.delete_program(Some(&r.program));
        if let Some(lose) = &self.lose {
            lose.lose_context();
        }
    }
}
