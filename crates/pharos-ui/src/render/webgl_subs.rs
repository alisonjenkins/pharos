//! WebGL2 compositor for subtitle masks (spec 026 FR-007).
//!
//! One program, one unit-quad vertex array, one R8 texture per distinct mask
//! id. A frame that did not change touches nothing; a changed one uploads the
//! new bitmaps, clears and draws one quad per mask, back to front. The
//! canvas is premultiplied-alpha so the browser composites it over the video.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use pharos_subs::Frame;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast as _;
use web_sys::{
    WebGl2RenderingContext as Gl, WebGlBuffer, WebGlProgram, WebGlTexture, WebGlUniformLocation,
    WebGlVertexArrayObject,
};

use super::subtitles::{Plan, SubtitleCompositor, Textures, FRAGMENT, VERTEX};
use super::webgl::link;
use super::Fallback;

/// How long a lost context may take to come back before subtitles fall back
/// to the browser's cues.
const RESTORE_WAIT_MS: f64 = 5000.0;

type Listener = Closure<dyn FnMut(web_sys::Event)>;

/// `a_corner` for two triangles covering the unit square.
const CORNERS: [u8; 12] = [0, 0, 1, 0, 0, 1, 1, 0, 1, 1, 0, 1];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Context {
    Live,
    Lost(f64),
    Restored,
}

struct Resources {
    program: WebGlProgram,
    vao: WebGlVertexArrayObject,
    corners: WebGlBuffer,
    rect: Option<WebGlUniformLocation>,
    canvas: Option<WebGlUniformLocation>,
    tint: Option<WebGlUniformLocation>,
}

struct WebglCompositor {
    gl: Gl,
    canvas: web_sys::HtmlCanvasElement,
    resources: Resources,
    textures: Textures,
    handles: HashMap<u64, WebGlTexture>,
    /// The last changed frame, to draw again after a context restore.
    last: Option<Frame>,
    context: Rc<Cell<Context>>,
    listeners: Vec<(&'static str, Listener)>,
    lose: Option<web_sys::WebglLoseContext>,
}

pub fn open(canvas_id: &str) -> Result<Box<dyn SubtitleCompositor>, Fallback> {
    let canvas = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(canvas_id))
        .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        .ok_or(Fallback::NoCanvas)?;
    let options = js_sys::Object::new();
    for (key, value) in [
        ("alpha", true),
        ("premultipliedAlpha", true),
        ("antialias", false),
        ("depth", false),
        ("stencil", false),
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
    let resources = match build(&gl) {
        Ok(r) => r,
        Err(f) => {
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
            e.prevent_default();
            web_sys::console::warn_1(&"pharos: subtitles webgl context lost".into());
            context.set(Context::Lost(js_sys::Date::now()));
        })
    };
    let on_restored = {
        let context = Rc::clone(&context);
        Listener::new(move |_: web_sys::Event| {
            if matches!(context.get(), Context::Lost(_)) {
                context.set(Context::Restored);
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
    Ok(Box::new(WebglCompositor {
        gl,
        canvas,
        resources,
        textures: Textures::default(),
        handles: HashMap::new(),
        last: None,
        context,
        listeners,
        lose,
    }))
}

fn build(gl: &Gl) -> Result<Resources, Fallback> {
    let program = link(gl, VERTEX, FRAGMENT)?;
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
    gl.use_program(Some(&program));
    gl.uniform1i(gl.get_uniform_location(&program, "u_mask").as_ref(), 0);
    gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 1);
    gl.enable(Gl::BLEND);
    // Premultiplied source over what is there.
    gl.blend_func(Gl::ONE, Gl::ONE_MINUS_SRC_ALPHA);
    gl.clear_color(0.0, 0.0, 0.0, 0.0);
    let loc = |name: &str| gl.get_uniform_location(&program, name);
    Ok(Resources {
        rect: loc("u_rect"),
        canvas: loc("u_canvas"),
        tint: loc("u_tint"),
        program,
        vao,
        corners,
    })
}

impl WebglCompositor {
    /// Whether the context can be drawn on now.
    fn ready(&mut self) -> Result<bool, Fallback> {
        match self.context.get() {
            Context::Live => Ok(true),
            Context::Lost(at) if js_sys::Date::now() - at > RESTORE_WAIT_MS => {
                Err(Fallback::ContextLost)
            }
            Context::Lost(_) => Ok(false),
            Context::Restored => {
                self.resources = build(&self.gl)?;
                self.handles.clear();
                self.textures.clear();
                self.context.set(Context::Live);
                web_sys::console::info_1(&"pharos: subtitles webgl context restored".into());
                Ok(true)
            }
        }
    }

    fn upload(&mut self, plan: &Plan<'_>) -> Result<(), Fallback> {
        let gl = &self.gl;
        for id in &plan.evict {
            if let Some(t) = self.handles.remove(id) {
                gl.delete_texture(Some(&t));
            }
        }
        for m in &plan.upload {
            let t = gl
                .create_texture()
                .ok_or_else(|| Fallback::Shader("create texture".to_owned()))?;
            gl.bind_texture(Gl::TEXTURE_2D, Some(&t));
            for (param, value) in [
                (Gl::TEXTURE_MIN_FILTER, Gl::LINEAR),
                (Gl::TEXTURE_MAG_FILTER, Gl::LINEAR),
                (Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE),
                (Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameteri(Gl::TEXTURE_2D, param, value as i32);
            }
            let n = (m.w * m.h) as usize;
            // Fails only on bad arguments; a lost context makes it a no-op.
            let _ = gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
                Gl::TEXTURE_2D,
                0,
                Gl::R8 as i32,
                m.w as i32,
                m.h as i32,
                0,
                Gl::RED,
                Gl::UNSIGNED_BYTE,
                m.coverage.get(..n),
            );
            self.handles.insert(m.id, t);
        }
        Ok(())
    }

    fn render(&mut self, frame: &Frame) -> Result<(), Fallback> {
        if !self.ready()? {
            return Ok(());
        }
        let plan = self.textures.plan(frame);
        self.upload(&plan)?;
        let gl = &self.gl;
        let r = &self.resources;
        let (w, h) = (self.canvas.width(), self.canvas.height());
        gl.viewport(0, 0, w as i32, h as i32);
        gl.clear(Gl::COLOR_BUFFER_BIT);
        if plan.quads.is_empty() {
            return Ok(());
        }
        gl.use_program(Some(&r.program));
        gl.bind_vertex_array(Some(&r.vao));
        gl.active_texture(Gl::TEXTURE0);
        gl.uniform2f(r.canvas.as_ref(), w as f32, h as f32);
        for q in &plan.quads {
            let Some(t) = self.handles.get(&q.id) else {
                continue;
            };
            gl.bind_texture(Gl::TEXTURE_2D, Some(t));
            gl.uniform4f(r.rect.as_ref(), q.rect[0], q.rect[1], q.rect[2], q.rect[3]);
            gl.uniform4f(r.tint.as_ref(), q.tint[0], q.tint[1], q.tint[2], q.tint[3]);
            gl.draw_arrays(Gl::TRIANGLES, 0, 6);
        }
        Ok(())
    }
}

impl SubtitleCompositor for WebglCompositor {
    fn resize(&mut self, width: u32, height: u32) {
        if self.canvas.width() != width {
            self.canvas.set_width(width);
        }
        if self.canvas.height() != height {
            self.canvas.set_height(height);
        }
    }

    fn draw(&mut self, frame: &Frame) -> Result<(), Fallback> {
        if !frame.changed {
            return Ok(());
        }
        self.last = Some(frame.clone());
        self.render(frame)
    }

    fn needs_redraw(&self) -> bool {
        self.context.get() != Context::Live
    }

    fn redraw(&mut self) -> Result<(), Fallback> {
        match self.last.clone() {
            Some(frame) => self.render(&frame),
            None => self.ready().map(|_| ()),
        }
    }
}

impl Drop for WebglCompositor {
    fn drop(&mut self) {
        for (name, listener) in &self.listeners {
            let _ = self
                .canvas
                .remove_event_listener_with_callback(name, listener.as_ref().unchecked_ref());
        }
        for t in self.handles.values() {
            self.gl.delete_texture(Some(t));
        }
        let r = &self.resources;
        self.gl.delete_vertex_array(Some(&r.vao));
        self.gl.delete_buffer(Some(&r.corners));
        self.gl.delete_program(Some(&r.program));
        if let Some(lose) = &self.lose {
            lose.lose_context();
        }
    }
}
