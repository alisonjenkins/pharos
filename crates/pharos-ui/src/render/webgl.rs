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
    WebGl2RenderingContext as Gl, WebGlBuffer, WebGlProgram, WebGlShader, WebGlTexture,
    WebGlUniformLocation, WebGlVertexArrayObject,
};

use super::catalogue::{fragment_source, Program, VERTEX};
use super::resolution::Resolution;
use super::{Fallback, Frame, Surface, DATA_ROWS, DATA_WIDTH};

/// Device pixels per CSS pixel at most: past this a fill-rate-bound headset
/// (Quest, Steam Frame) spends GPU time on detail a soft shader never shows.
const MAX_PIXEL_RATIO: f64 = 1.25;

/// How long a lost context may take to come back before the page settles
/// for its CSS look. A GPU reset or switch restores in well under this.
const RESTORE_WAIT_MS: f64 = 5000.0;

type Listener = Closure<dyn FnMut(web_sys::Event)>;

/// Where the context is, as its events report it, with when it was lost
/// (`Date.now()` ms).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Context {
    Live,
    Lost(f64),
    /// Back, but every handle made before the loss is dead.
    Restored(f64),
}

/// `a_corner` for the one triangle that covers the screen, in 0 to 2.
const CORNERS: [u8; 6] = [0, 0, 2, 0, 0, 2];

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
    corners: WebGlBuffer,
    texture: WebGlTexture,
    uniforms: Uniforms,
}

pub struct WebglSurface {
    gl: Gl,
    canvas: web_sys::HtmlCanvasElement,
    program: Program,
    resources: Resources,
    silence: Vec<u8>,
    context: Rc<Cell<Context>>,
    listeners: Vec<(&'static str, Listener)>,
    /// Kept to release the context on drop: browsers cap live contexts
    /// (~16) and free a dropped one only when it is garbage collected.
    lose: Option<web_sys::WebglLoseContext>,
    /// The share of the pixel ratio drawn at (B1218, V1178).
    resolution: Resolution,
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

    let resources = match build(&gl, program) {
        Ok(resources) => resources,
        Err(f) => {
            // Release now rather than at garbage collection, as `Drop` does.
            if let Some(lose) = &lose {
                lose.lose_context();
            }
            return Err(f);
        }
    };

    let context = Rc::new(Cell::new(Context::Live));
    let on_lost = {
        let context = Rc::clone(&context);
        Listener::new(move |e: web_sys::Event| {
            // Without this the browser never fires `webglcontextrestored`.
            e.prevent_default();
            web_sys::console::warn_1(
                &format!("pharos: webgl context lost program={program:?}, waiting for restore")
                    .into(),
            );
            context.set(Context::Lost(js_sys::Date::now()));
        })
    };
    let on_restored = {
        let context = Rc::clone(&context);
        Listener::new(move |_: web_sys::Event| {
            if let Context::Lost(at) = context.get() {
                context.set(Context::Restored(at));
            }
        })
    };
    let listeners = vec![
        ("webglcontextlost", on_lost),
        ("webglcontextrestored", on_restored),
    ];
    for (name, listener) in &listeners {
        let _ = canvas.add_event_listener_with_callback(name, listener.as_ref().unchecked_ref());
    }
    Ok(Box::new(WebglSurface {
        gl,
        canvas,
        program,
        resources,
        silence: silence(),
        context,
        listeners,
        lose,
        resolution: Resolution::default(),
    }))
}

/// Compile `program` and make its vertex array and data texture.
fn build(gl: &Gl, program: Program) -> Result<Resources, Fallback> {
    let handle = link(gl, VERTEX, &fragment_source(program))?;
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
    let corners = gl
        .create_buffer()
        .ok_or_else(|| Fallback::Shader("create buffer".to_owned()))?;
    gl.bind_vertex_array(Some(&vao));
    gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&corners));
    gl.buffer_data_with_u8_array(Gl::ARRAY_BUFFER, &CORNERS, Gl::STATIC_DRAW);
    gl.enable_vertex_attrib_array(0);
    gl.vertex_attrib_pointer_with_i32(0, 2, Gl::UNSIGNED_BYTE, false, 0, 0);
    gl.bind_vertex_array(None);
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
        corners,
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

pub(super) fn link(gl: &Gl, vertex: &str, fragment: &str) -> Result<WebGlProgram, Fallback> {
    let program = gl
        .create_program()
        .ok_or_else(|| Fallback::Shader("create program".to_owned()))?;
    let mut shaders = Vec::with_capacity(2);
    let linked = compile_and_link(gl, &program, vertex, fragment, &mut shaders);
    for shader in &shaders {
        gl.detach_shader(&program, shader);
        gl.delete_shader(Some(shader));
    }
    if linked.is_err() {
        gl.delete_program(Some(&program));
    }
    linked.map(|()| program)
}

/// Compile both stages into `program` and link it; every shader made is
/// left in `shaders` for the caller to delete, whatever the outcome.
fn compile_and_link(
    gl: &Gl,
    program: &WebGlProgram,
    vertex: &str,
    fragment: &str,
    shaders: &mut Vec<WebGlShader>,
) -> Result<(), Fallback> {
    for (kind, source) in [(Gl::VERTEX_SHADER, vertex), (Gl::FRAGMENT_SHADER, fragment)] {
        let shader = gl
            .create_shader(kind)
            .ok_or_else(|| Fallback::Shader("create shader".to_owned()))?;
        gl.shader_source(&shader, source);
        gl.compile_shader(&shader);
        gl.attach_shader(program, &shader);
        let compiled = gl
            .get_shader_parameter(&shader, Gl::COMPILE_STATUS)
            .as_bool()
            .unwrap_or(false);
        let log = (!compiled).then(|| gl.get_shader_info_log(&shader).unwrap_or_default());
        shaders.push(shader);
        if let Some(log) = log {
            return Err(Fallback::Shader(format!("compile: {log}")));
        }
    }
    gl.link_program(program);
    let linked = gl
        .get_program_parameter(program, Gl::LINK_STATUS)
        .as_bool()
        .unwrap_or(false);
    if !linked {
        let log = gl.get_program_info_log(program).unwrap_or_default();
        return Err(Fallback::Shader(format!("link: {log}")));
    }
    Ok(())
}

impl WebglSurface {
    /// Match the drawing buffer to the canvas's box at the device's pixel
    /// ratio, capped at [`MAX_PIXEL_RATIO`] and scaled down while frames run
    /// slow (B1218); returns the buffer size.
    fn fit(&mut self) -> (i32, i32) {
        let now = js_sys::Date::now();
        if let Some(l) = self.resolution.frame(now) {
            web_sys::console::info_1(
                &format!(
                    "pharos: visualiser resolution lowered program={:?} scale_from={:.2} scale_to={:.2} median_frame_ms={:.1}",
                    self.program, l.from, l.to, l.median_ms
                )
                .into(),
            );
        }
        let ratio = web_sys::window()
            .map_or(1.0, |w| w.device_pixel_ratio())
            .min(MAX_PIXEL_RATIO)
            * self.resolution.scale();
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

impl WebglSurface {
    /// Whether the context can be drawn on now: waits out a loss, rebuilds
    /// after a restore, and gives up [`RESTORE_WAIT_MS`] after the loss.
    fn ready(&mut self) -> Result<bool, Fallback> {
        match self.context.get() {
            Context::Live => Ok(true),
            Context::Lost(at) if js_sys::Date::now() - at > RESTORE_WAIT_MS => {
                Err(Fallback::ContextLost)
            }
            Context::Lost(_) => Ok(false),
            Context::Restored(at) => {
                self.resources = build(&self.gl, self.program)?;
                web_sys::console::info_1(
                    &format!(
                        "pharos: webgl context restored program={:?} waited_ms={:.0}",
                        self.program,
                        js_sys::Date::now() - at
                    )
                    .into(),
                );
                self.context.set(Context::Live);
                Ok(true)
            }
        }
    }
}

impl Surface for WebglSurface {
    fn needs_frame(&self) -> bool {
        self.context.get() != Context::Live
    }

    fn draw(&mut self, frame: &Frame<'_>) -> Result<(), Fallback> {
        if !self.ready()? {
            return Ok(());
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
        for (name, listener) in &self.listeners {
            let _ = self
                .canvas
                .remove_event_listener_with_callback(name, listener.as_ref().unchecked_ref());
        }
        let r = &self.resources;
        self.gl.delete_texture(Some(&r.texture));
        self.gl.delete_vertex_array(Some(&r.vao));
        self.gl.delete_buffer(Some(&r.corners));
        self.gl.delete_program(Some(&r.program));
        if let Some(lose) = &self.lose {
            lose.lose_context();
        }
    }
}
