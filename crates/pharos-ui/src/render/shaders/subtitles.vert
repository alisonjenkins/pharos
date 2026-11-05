#version 300 es
// One unit quad (a_corner in 0..1) placed by u_rect = x, y, w, h in canvas
// pixels, y down. Read through an enabled array at location 0 like the
// full-screen shaders (Firefox on macOS emulates attribute 0 otherwise).
layout(location = 0) in vec2 a_corner;
uniform vec4 u_rect;
uniform vec2 u_canvas;
out vec2 v_uv;
void main() {
  vec2 px = u_rect.xy + a_corner * u_rect.zw;
  vec2 clip = px / u_canvas * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
  v_uv = a_corner;
}
