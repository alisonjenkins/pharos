#version 300 es
// An 8-bit coverage mask tinted by u_tint (straight RGBA, 0..1), written
// premultiplied for the canvas's premultiplied alpha.
precision mediump float;
uniform sampler2D u_mask;
uniform vec4 u_tint;
in vec2 v_uv;
out vec4 o_color;
void main() {
  float a = texture(u_mask, v_uv).r * u_tint.a;
  o_color = vec4(u_tint.rgb * a, a);
}
